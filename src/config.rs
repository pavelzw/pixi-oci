//! Parsing of `pixi.toml` (or `pyproject.toml`) and the `[tool.pixi-oci]` section.

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use oci_spec::image::Config;
use serde::Deserialize;

/// Default location of the environment inside the image.
pub const DEFAULT_INSTALL_PREFIX: &str = "/opt/env";
/// Default image tag.
pub const DEFAULT_TAG: &str = "latest";
/// Default maximum number of package layers.
pub const DEFAULT_MAX_LAYERS: NonZeroUsize = NonZeroUsize::new(20).unwrap();
/// Default interpreter used for the activation wrapper script.
pub const DEFAULT_ACTIVATION_SHELL: &str = "/bin/bash";
/// Pseudo base image meaning "start from an empty root filesystem".
pub const SCRATCH: &str = "scratch";
/// Debian `libc6` version used for `glibc = true`: the glibc of the current Debian stable.
pub const DEFAULT_GLIBC_DEBIAN_VERSION: &str = "2.41-12+deb13u4";

/// A pixi manifest (`pixi.toml` or `pyproject.toml`). Only `[tool.pixi-oci]` is read; the
/// packages come from `pixi.lock`.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct Manifest {
    #[serde(default)]
    pub tool: ToolSection,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct ToolSection {
    #[serde(default, rename = "pixi-oci")]
    pub pixi_oci: PixiOciConfig,
}

/// The `[tool.pixi-oci]` section.
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PixiOciConfig {
    /// A registry image to build on, or a root filesystem built from scratch.
    pub base: Option<BaseSpec>,
    /// The pixi environment to install.
    pub environment: Option<String>,
    /// The conda platform (subdir) to build for.
    pub platform: Option<String>,
    /// Absolute path of the environment inside the image.
    pub install_prefix: Option<String>,
    /// Image name (repository).
    pub name: Option<String>,
    /// Image tag.
    pub tag: Option<String>,
    /// Maximum number of layers used for the conda packages.
    pub max_layers: Option<NonZeroUsize>,
    /// Additional files to copy into the image.
    #[serde(default)]
    pub copy_to_image: Vec<CopyEntry>,
    /// Interpreter for the activation wrapper script.
    pub activation_shell: Option<String>,
    /// Image config fields (docker image spec `config`).
    #[serde(default)]
    pub oci_config: OciConfigSection,
}

/// `base`: either a registry reference such as `"ubuntu:latest"` (or `"scratch"` for an empty
/// root filesystem), or a table describing a minimal root filesystem built from scratch.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum BaseSpec {
    Image(String),
    Scratch(ScratchBase),
}

impl Default for BaseSpec {
    fn default() -> Self {
        BaseSpec::Scratch(ScratchBase::default())
    }
}

/// A root filesystem built from scratch.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ScratchBase {
    /// Add `/etc/passwd`, `/etc/group`, their shadow files, `/root` and `/tmp`.
    #[serde(default = "default_true")]
    pub shadow_setup: bool,
    /// Add glibc from Debian's `libc6` package.
    #[serde(default)]
    pub glibc: GlibcSpec,
}

impl Default for ScratchBase {
    fn default() -> Self {
        Self {
            shadow_setup: true,
            glibc: GlibcSpec::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// `glibc = true` (default version), `false` (none) or a Debian `libc6` version.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum GlibcSpec {
    Enabled(bool),
    DebianVersion(String),
}

impl Default for GlibcSpec {
    fn default() -> Self {
        GlibcSpec::Enabled(true)
    }
}

impl GlibcSpec {
    /// The Debian `libc6` version to install, if any.
    pub fn debian_version(&self) -> Option<&str> {
        match self {
            GlibcSpec::Enabled(true) => Some(DEFAULT_GLIBC_DEBIAN_VERSION),
            GlibcSpec::Enabled(false) => None,
            GlibcSpec::DebianVersion(version) => Some(version),
        }
    }
}

/// A file or directory to copy into the image.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum CopyEntry {
    Source(String),
    Full {
        source: String,
        #[serde(default)]
        destination: Option<String>,
    },
}

impl CopyEntry {
    pub fn source(&self) -> &str {
        match self {
            CopyEntry::Source(s) => s,
            CopyEntry::Full { source, .. } => source,
        }
    }

    /// The absolute destination inside the image. Entries without an explicit destination
    /// land in `working_dir` under their file name, like `COPY` in a Dockerfile.
    pub fn destination(&self, working_dir: &str) -> Result<String> {
        let explicit = match self {
            CopyEntry::Source(_) => None,
            CopyEntry::Full { destination, .. } => destination.as_deref(),
        };
        let dest = match explicit {
            Some(d) if d.starts_with('/') => d.to_string(),
            Some(d) => join_image_path(working_dir, d),
            None => {
                let file_name = Path::new(self.source())
                    .file_name()
                    .and_then(|f| f.to_str())
                    .ok_or_else(|| {
                        anyhow!(
                            "copy-to-image source `{}` has no file name, specify a destination",
                            self.source()
                        )
                    })?;
                join_image_path(working_dir, file_name)
            }
        };
        Ok(normalize_image_path(&dest))
    }
}

/// Join an in-image path onto a working directory.
fn join_image_path(working_dir: &str, rel: &str) -> String {
    format!("{}/{}", working_dir.trim_end_matches('/'), rel)
}

/// Normalize an absolute in-image path: collapse `//`, `.` and trailing slashes.
pub fn normalize_image_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The `[tool.pixi-oci.oci-config]` section.
///
/// Field names follow the docker image spec, in kebab-case. The spec's PascalCase names are
/// accepted as aliases.
/// We keep our own struct instead of deserializing `oci_spec::image::Config` for nicer TOML.
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OciConfigSection {
    #[serde(alias = "Entrypoint")]
    pub entrypoint: Option<StringOrVec>,
    #[serde(alias = "Cmd")]
    pub cmd: Option<StringOrVec>,
    #[serde(alias = "Env")]
    pub env: Option<EnvSpec>,
    #[serde(alias = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(alias = "User")]
    pub user: Option<String>,
    #[serde(alias = "Labels", default)]
    pub labels: BTreeMap<String, String>,
    #[serde(alias = "ExposedPorts", default)]
    pub exposed_ports: Vec<PortSpec>,
    #[serde(alias = "Volumes", default)]
    pub volumes: Vec<String>,
    #[serde(alias = "StopSignal")]
    pub stop_signal: Option<String>,
}

impl OciConfigSection {
    /// Convert into an OCI image [`Config`]. Unset fields stay `None` so they can be inherited
    /// from the base image (see [`crate::image::merge_config`]).
    pub fn to_oci_config(&self) -> Result<Config> {
        let mut config = Config::default();
        config.set_entrypoint(
            self.entrypoint
                .as_ref()
                .map(StringOrVec::to_argv)
                .transpose()?,
        );
        config.set_cmd(self.cmd.as_ref().map(StringOrVec::to_argv).transpose()?);
        config.set_env(
            self.env
                .as_ref()
                .map(|env| -> Result<Vec<String>> {
                    Ok(env
                        .to_pairs()?
                        .into_iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect())
                })
                .transpose()?,
        );
        config.set_working_dir(self.working_dir.clone());
        config.set_user(self.user.clone());
        config.set_labels(
            Some(self.labels.clone().into_iter().collect::<HashMap<_, _>>())
                .filter(|l| !l.is_empty()),
        );
        config.set_exposed_ports(
            Some(
                self.exposed_ports
                    .iter()
                    .map(PortSpec::to_port_string)
                    .collect::<Vec<_>>(),
            )
            .filter(|p| !p.is_empty()),
        );
        config.set_volumes(Some(self.volumes.clone()).filter(|v| !v.is_empty()));
        config.set_stop_signal(self.stop_signal.clone());
        Ok(config)
    }
}

/// A command given either as a shell-like string or as an argv list.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum StringOrVec {
    String(String),
    Vec(Vec<String>),
}

impl StringOrVec {
    pub fn to_argv(&self) -> Result<Vec<String>> {
        match self {
            StringOrVec::Vec(v) => Ok(v.clone()),
            StringOrVec::String(s) => {
                shlex::split(s).ok_or_else(|| anyhow!("could not parse command `{s}`"))
            }
        }
    }
}

/// Environment variables given as a table or as a list of `KEY=VALUE` strings.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EnvSpec {
    Map(BTreeMap<String, String>),
    List(Vec<String>),
}

impl EnvSpec {
    pub fn to_pairs(&self) -> Result<Vec<(String, String)>> {
        match self {
            EnvSpec::Map(m) => Ok(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            EnvSpec::List(l) => l
                .iter()
                .map(|entry| {
                    entry
                        .split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .ok_or_else(|| anyhow!("environment entry `{entry}` is not KEY=VALUE"))
                })
                .collect(),
        }
    }
}

/// A port given as `"8080/tcp"`, `"8080"` or `8080`.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum PortSpec {
    Number(u16),
    String(String),
}

impl PortSpec {
    pub fn to_port_string(&self) -> String {
        match self {
            PortSpec::Number(n) => format!("{n}/tcp"),
            PortSpec::String(s) if s.contains('/') => s.clone(),
            PortSpec::String(s) => format!("{s}/tcp"),
        }
    }
}

/// A loaded manifest together with its location.
#[derive(Debug, Clone)]
pub struct LoadedManifest {
    /// The manifest file itself.
    pub path: PathBuf,
    /// The directory containing the manifest (and the lockfile).
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl LoadedManifest {
    /// Locate and parse the manifest. `path` may be the manifest file or its directory.
    pub fn load(path: &Path) -> Result<Self> {
        let path = if path.is_dir() {
            let pixi = path.join("pixi.toml");
            let pyproject = path.join("pyproject.toml");
            if pixi.exists() {
                pixi
            } else if pyproject.exists() {
                pyproject
            } else {
                bail!("no pixi.toml or pyproject.toml found in {}", path.display());
            }
        } else {
            path.to_path_buf()
        };
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read manifest {}", path.display()))?;
        let manifest: Manifest = toml::from_str(&contents)
            .with_context(|| format!("could not parse manifest {}", path.display()))?;

        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            path,
            dir,
            manifest,
        })
    }

    pub fn config(&self) -> &PixiOciConfig {
        &self.manifest.tool.pixi_oci
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Manifest {
        toml::from_str(toml).expect("valid manifest")
    }

    #[test]
    fn parses_example_manifest() {
        let m = parse(include_str!("../example/pixi.toml"));
        let cfg = m.tool.pixi_oci;
        assert_eq!(
            cfg.base,
            Some(BaseSpec::Scratch(ScratchBase {
                shadow_setup: true,
                glibc: GlibcSpec::DebianVersion("2.41-12+deb13u4".into()),
            }))
        );
        assert_eq!(cfg.max_layers, NonZeroUsize::new(20));
        assert_eq!(cfg.copy_to_image.len(), 2);
        assert_eq!(
            cfg.oci_config.entrypoint.unwrap().to_argv().unwrap(),
            vec!["caddy", "file-server", "--browse", "--root", "."]
        );
        assert_eq!(cfg.name.as_deref(), Some("example-project"));
    }

    #[test]
    fn entrypoint_as_array_and_aliases() {
        let m = parse(
            r#"
[tool.pixi-oci.oci-config]
Entrypoint = ["a", "b c"]
WorkingDir = "/app"
env = ["A=1", "B=x=y"]
exposed-ports = [8080, "9090", "53/udp"]
"#,
        );
        let cfg = m.tool.pixi_oci.oci_config;
        assert_eq!(
            cfg.entrypoint.unwrap().to_argv().unwrap(),
            vec!["a".to_string(), "b c".to_string()]
        );
        assert_eq!(cfg.working_dir.as_deref(), Some("/app"));
        assert_eq!(
            cfg.env.unwrap().to_pairs().unwrap(),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "x=y".to_string())
            ]
        );
        let ports: Vec<_> = cfg
            .exposed_ports
            .iter()
            .map(PortSpec::to_port_string)
            .collect();
        assert_eq!(ports, vec!["8080/tcp", "9090/tcp", "53/udp"]);
    }

    #[test]
    fn converts_to_oci_config() {
        let m = parse(
            r#"
[tool.pixi-oci.oci-config]
cmd = "serve --port 80"
env = { FOO = "bar" }
exposed-ports = [8080]
"#,
        );
        let config = m.tool.pixi_oci.oci_config.to_oci_config().unwrap();
        assert!(config.entrypoint().is_none());
        assert_eq!(
            config.cmd().as_deref(),
            Some(&["serve".to_string(), "--port".to_string(), "80".to_string()][..])
        );
        assert_eq!(config.env().as_deref(), Some(&["FOO=bar".to_string()][..]));
        assert_eq!(
            config.exposed_ports().as_deref(),
            Some(&["8080/tcp".to_string()][..])
        );
        assert!(config.labels().is_none());
        assert!(config.volumes().is_none());
    }

    #[test]
    fn parses_base_variants() {
        let base = |toml: &str| parse(toml).tool.pixi_oci.base;
        assert_eq!(
            base("[tool.pixi-oci]\nbase = \"ubuntu:latest\"\n"),
            Some(BaseSpec::Image("ubuntu:latest".into()))
        );
        let scratch = base("[tool.pixi-oci]\nbase = {}\n").unwrap();
        assert_eq!(scratch, BaseSpec::default());
        let BaseSpec::Scratch(scratch) = scratch else {
            panic!("expected a scratch base");
        };
        assert_eq!(
            scratch.glibc.debian_version(),
            Some(DEFAULT_GLIBC_DEBIAN_VERSION)
        );
        assert_eq!(
            base("[tool.pixi-oci]\nbase = { shadow-setup = false, glibc = false }\n"),
            Some(BaseSpec::Scratch(ScratchBase {
                shadow_setup: false,
                glibc: GlibcSpec::Enabled(false),
            }))
        );
        assert!(
            toml::from_str::<Manifest>("[tool.pixi-oci]\nbase = { glibcc = false }\n").is_err()
        );
    }

    #[test]
    fn env_as_table() {
        let m = parse("[tool.pixi-oci.oci-config]\nenv = { FOO = \"bar\" }\n");
        assert_eq!(
            m.tool.pixi_oci.oci_config.env.unwrap().to_pairs().unwrap(),
            vec![("FOO".to_string(), "bar".to_string())]
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Manifest>("[tool.pixi-oci]\nbase_image = \"ubuntu\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("base_image"), "{err}");
    }

    #[test]
    fn copy_destination_defaults_to_working_dir() {
        let e = CopyEntry::Source("dir/my-file".into());
        assert_eq!(e.destination("/").unwrap(), "/my-file");
        assert_eq!(e.destination("/app/").unwrap(), "/app/my-file");
        let e = CopyEntry::Full {
            source: "x".into(),
            destination: Some("/renamed".into()),
        };
        assert_eq!(e.destination("/app").unwrap(), "/renamed");
        let e = CopyEntry::Full {
            source: "x".into(),
            destination: Some("sub/renamed".into()),
        };
        assert_eq!(e.destination("/app").unwrap(), "/app/sub/renamed");
    }

    #[test]
    fn normalizes_image_paths() {
        assert_eq!(normalize_image_path("//a/./b//c/"), "/a/b/c");
        assert_eq!(normalize_image_path("/a/../b"), "/b");
        assert_eq!(normalize_image_path("/"), "/");
    }
}
