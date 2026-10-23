//! Environment activation inside the image: static environment variables in the image config
//! plus a small wrapper that sources the `activate.d` scripts before running the command.

use std::{fs, io::Read, path::Path};

use anyhow::{Context, Result};
use rattler::install::from_archive::{PlannedPackage, PrefixSink};
use rattler_conda_types::Subdir;
use rattler_shell::{
    activation::{Activator, prefix_path_entries},
    shell::Bash,
};

use crate::config::relative_image_path;

/// Default `PATH` when neither the base image nor the user define one.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Location of the generated wrapper, relative to the install prefix.
pub const WRAPPER_RELATIVE_PATH: &str = "etc/pixi-oci/entrypoint.sh";

/// What the image needs to activate the environment.
#[derive(Debug)]
pub struct Activation {
    /// `KEY=VALUE` entries for the image config.
    pub env: Vec<String>,
    /// The wrapper script, if any activation scripts exist.
    pub wrapper: Option<Wrapper>,
}

/// The generated script that sources the activation scripts.
#[derive(Debug)]
pub struct Wrapper {
    /// Absolute path inside the image.
    pub image_path: String,
    pub contents: String,
}

/// The directories of the prefix that activation reads.
const ACTIVATION_DIRS: [&str; 2] = ["etc/conda/activate.d", "etc/conda/env_vars.d"];

fn is_activation_file(path: &Path) -> bool {
    ACTIVATION_DIRS
        .iter()
        .any(|dir| path.parent() == Some(Path::new(dir)))
}

/// Writes the files the activation reads into a directory on the build host and drops the rest.
struct ActivationSink<'a> {
    staging_prefix: &'a Path,
}

impl PrefixSink for ActivationSink<'_> {
    fn add_file(
        &mut self,
        path: &Path,
        _mode: u32,
        _size: u64,
        contents: &mut dyn Read,
    ) -> std::io::Result<()> {
        if is_activation_file(path) {
            let destination = self.staging_prefix.join(path);
            fs::create_dir_all(
                destination
                    .parent()
                    .expect("activation files have a parent"),
            )?;
            std::io::copy(contents, &mut fs::File::create(destination)?)?;
        }
        Ok(())
    }

    fn add_symlink(&mut self, path: &Path, target: &Path) -> std::io::Result<()> {
        if is_activation_file(path) {
            tracing::warn!(
                "ignoring the activation script {} because it is a symlink to {}",
                path.display(),
                target.display()
            );
        }
        Ok(())
    }

    fn add_directory(&mut self, _path: &Path, _mode: u32) -> std::io::Result<()> {
        Ok(())
    }
}

/// Write the activation scripts and environment variable files of `packages` into
/// `staging_prefix`, so that [`prepare_activation`] can inspect them. Only the packages that
/// ship such files are read.
pub fn stage_activation_files(packages: &[PlannedPackage], staging_prefix: &Path) -> Result<()> {
    for package in packages {
        if !package.target_paths().any(is_activation_file) {
            continue;
        }
        package
            .link(ActivationSink { staging_prefix })
            .with_context(|| {
                format!(
                    "could not read the activation scripts of {}",
                    package.record().package_record.name.as_normalized()
                )
            })?;
    }
    Ok(())
}

/// Inputs for computing the activation.
pub struct ActivationInputs<'a> {
    /// A directory on the host with the activation files, see [`stage_activation_files`].
    pub staging_prefix: &'a Path,
    /// The prefix path inside the image.
    pub install_prefix: &'a str,
    pub subdir: Subdir,
    /// `Env` of the base image.
    pub base_env: &'a [(String, String)],
    /// `env` from `[tool.pixi-oci.oci-config]`.
    pub user_env: &'a [(String, String)],
}

/// Ordered environment with "last write wins" semantics that keeps the original position.
#[derive(Default)]
struct OrderedEnv(Vec<(String, String)>);

impl OrderedEnv {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key, value)),
        }
    }

    fn into_strings(self) -> Vec<String> {
        self.0
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect()
    }
}

/// Compute the environment variables and, when needed, the wrapper script.
pub fn prepare_activation(inputs: ActivationInputs<'_>) -> Result<Activation> {
    let activator = Activator::from_path(inputs.staging_prefix, Bash::default(), inputs.subdir)
        .context("could not inspect the environment for activation scripts")?;

    let mut env = OrderedEnv::default();
    for (k, v) in inputs.base_env {
        env.set(k.clone(), v.clone());
    }

    let prefix_paths: Vec<String> =
        prefix_path_entries(Path::new(inputs.install_prefix), &inputs.subdir)
            .iter()
            .map(|p| image_path_string(p))
            .collect();
    let base_path = env.get("PATH").unwrap_or(DEFAULT_PATH).to_string();
    env.set("PATH", format!("{}:{}", prefix_paths.join(":"), base_path));
    env.set("CONDA_PREFIX", inputs.install_prefix);

    for (k, v) in &activator.env_vars {
        env.set(k.clone(), v.clone());
    }
    for (k, v) in inputs.user_env {
        warn_if_unexpanded(k, v);
        env.set(k.clone(), v.clone());
    }

    let wrapper = if activator.activation_scripts.is_empty() {
        None
    } else {
        let scripts: Vec<String> = activator
            .activation_scripts
            .iter()
            .map(|script| rebase(script, inputs.staging_prefix, inputs.install_prefix))
            .collect::<Result<_>>()?;
        Some(Wrapper {
            image_path: format!("{}/{WRAPPER_RELATIVE_PATH}", inputs.install_prefix),
            contents: render_wrapper(&scripts),
        })
    };

    Ok(Activation {
        env: env.into_strings(),
        wrapper,
    })
}

/// The image config holds literal values; unlike `pixi shell-hook`, no shell expands
/// references such as `$CONDA_PREFIX` or `$PATH`.
fn warn_if_unexpanded(key: &str, value: &str) {
    if value.contains('$') {
        tracing::warn!(
            "environment variable {key}={value} contains `$`, which is not expanded in the image"
        );
    }
}

fn rebase(script: &Path, staging_prefix: &Path, install_prefix: &str) -> Result<String> {
    let rel = script.strip_prefix(staging_prefix).with_context(|| {
        format!(
            "activation script {} is outside the prefix",
            script.display()
        )
    })?;
    Ok(format!("{install_prefix}/{}", relative_image_path(rel)))
}

/// An absolute image path built with the path functions of the host, with `/` separators.
fn image_path_string(path: &Path) -> String {
    format!("/{}", relative_image_path(path))
}

/// Render the wrapper script contents.
pub fn render_wrapper(scripts: &[String]) -> String {
    let mut out = String::from(
        "#!/bin/sh\n# Generated by pixi-oci. Sources the activation scripts of the environment,\n# then runs the command passed to the container.\n",
    );
    for script in scripts {
        out.push_str(&format!(
            ". {}\n",
            shlex::try_quote(script).unwrap_or(script.into())
        ));
    }
    out.push_str("exec \"$@\"\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_env_overrides_in_place() {
        let mut env = OrderedEnv::default();
        env.set("A", "1");
        env.set("B", "2");
        env.set("A", "3");
        assert_eq!(env.into_strings(), vec!["A=3", "B=2"]);
    }

    #[test]
    fn wrapper_sources_scripts_then_execs() {
        let script = render_wrapper(&["/opt/env/etc/conda/activate.d/a.sh".to_string()]);
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains(". /opt/env/etc/conda/activate.d/a.sh\n"));
        assert!(script.ends_with("exec \"$@\"\n"));
    }

    #[test]
    fn computes_env_for_prefix_without_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("bin")).unwrap();
        let activation = prepare_activation(ActivationInputs {
            staging_prefix: tmp.path(),
            install_prefix: "/opt/env",
            subdir: Subdir::Linux64,
            base_env: &[
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("LANG".to_string(), "C".to_string()),
            ],
            user_env: &[("LANG".to_string(), "C.UTF-8".to_string())],
        })
        .unwrap();
        assert_eq!(
            activation.env,
            vec![
                "PATH=/opt/env/bin:/usr/bin",
                "LANG=C.UTF-8",
                "CONDA_PREFIX=/opt/env"
            ]
        );
        assert!(activation.wrapper.is_none());
    }

    #[test]
    fn renders_wrapper_when_scripts_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let activate_d = tmp.path().join("etc/conda/activate.d");
        fs::create_dir_all(&activate_d).unwrap();
        fs::write(activate_d.join("b.sh"), "export B=1\n").unwrap();
        fs::write(activate_d.join("a.sh"), "export A=1\n").unwrap();
        let activation = prepare_activation(ActivationInputs {
            staging_prefix: tmp.path(),
            install_prefix: "/opt/env",
            subdir: Subdir::Linux64,
            base_env: &[],
            user_env: &[],
        })
        .unwrap();
        let wrapper = activation.wrapper.unwrap();
        assert_eq!(wrapper.image_path, "/opt/env/etc/pixi-oci/entrypoint.sh");
        let contents = wrapper.contents;
        let a = contents.find("activate.d/a.sh").unwrap();
        let b = contents.find("activate.d/b.sh").unwrap();
        assert!(a < b, "scripts must be sourced in sorted order");
        assert_eq!(
            activation.env[0],
            format!("PATH=/opt/env/bin:{DEFAULT_PATH}")
        );
    }
}
