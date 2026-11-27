//! Building the source packages of the lockfile (pixi-build) with `pixi build`, like pixi-pack.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use rattler_conda_types::{RepoDataRecord, Subdir, package::DistArchiveIdentifier};
use rattler_lock::UrlOrPath;

use crate::{inject::injected_record, lockfile::SourcePackage};

/// The pixi executable: `PIXI_EXE` (set by `pixi run`), else `pixi` from `PATH`.
fn pixi_executable() -> OsString {
    std::env::var_os("PIXI_EXE")
        .filter(|exe| !exe.is_empty())
        .unwrap_or_else(|| "pixi".into())
}

/// Builds the source packages of all platforms of an image.
pub struct SourceBuilder {
    /// The directory of the lockfile, which relative source locations are relative to.
    workspace_dir: PathBuf,
    /// Where `pixi build` puts the packages; they have to stay until they are installed.
    output_dir: PathBuf,
    builds: usize,
    /// Locations whose packages were all built as noarch, so other platforms reuse them.
    noarch: Vec<(UrlOrPath, Vec<RepoDataRecord>)>,
}

impl SourceBuilder {
    pub fn new(workspace_dir: &Path, output_dir: &Path) -> Self {
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
            builds: 0,
            noarch: Vec::new(),
        }
    }

    /// Build the `sources` for `subdir` and return a record for each of them.
    ///
    /// Every location is built once, even if it provides several of the packages. Noarch
    /// packages are only built for the first platform.
    pub fn build(
        &mut self,
        sources: &[SourcePackage],
        subdir: Subdir,
    ) -> Result<Vec<RepoDataRecord>> {
        let mut locations: Vec<(&UrlOrPath, Vec<&SourcePackage>)> = Vec::new();
        for source in sources {
            match locations.iter_mut().find(|(l, _)| *l == &source.location) {
                Some((_, packages)) => packages.push(source),
                None => locations.push((&source.location, vec![source])),
            }
        }

        let mut records = Vec::with_capacity(sources.len());
        for (location, packages) in locations {
            if let Some((_, built)) = self.noarch.iter().find(|(l, _)| l == location) {
                records.extend(select_packages(built, &packages, location, subdir)?);
                continue;
            }
            let built = self.build_location(location, &packages, subdir)?;
            records.extend(select_packages(&built, &packages, location, subdir)?);
            if built
                .iter()
                .all(|r| r.package_record.subdir == Subdir::NoArch.as_str())
            {
                self.noarch.push((location.clone(), built));
            }
        }
        Ok(records)
    }

    /// Run `pixi build` for the package at `location` and return all packages it produced.
    fn build_location(
        &mut self,
        location: &UrlOrPath,
        packages: &[&SourcePackage],
        subdir: Subdir,
    ) -> Result<Vec<RepoDataRecord>> {
        let names = names(packages);
        let manifest = match location {
            UrlOrPath::Path(path) => self.workspace_dir.join(path.as_str()),
            UrlOrPath::Url(url) => bail!(
                "cannot build source package {names} from {url}: only source packages in a \
                 local directory can be built. Build it with `pixi build` and use `--skip {}` \
                 with `--inject`",
                packages[0].name.as_source()
            ),
        };
        let output = self.output_dir.join(self.builds.to_string());
        self.builds += 1;
        std::fs::create_dir_all(&output)
            .with_context(|| format!("could not create {}", output.display()))?;

        eprintln!(
            "🏗️  Building source package {names} for {} with pixi build",
            subdir.as_str()
        );
        run_pixi_build(&manifest, subdir, &output)
            .with_context(|| format!("could not build source package {names}"))?;
        let built = built_records(&output)?;
        for record in &built {
            eprintln!("📦 Built {}", record.identifier);
        }
        Ok(built)
    }
}

fn names(packages: &[&SourcePackage]) -> String {
    packages
        .iter()
        .map(|p| p.name.as_source())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pick the package of each of `packages` from the packages `built` at `location`.
fn select_packages(
    built: &[RepoDataRecord],
    packages: &[&SourcePackage],
    location: &UrlOrPath,
    subdir: Subdir,
) -> Result<Vec<RepoDataRecord>> {
    packages
        .iter()
        .map(|package| {
            let mut matching = built
                .iter()
                .filter(|r| r.package_record.name == package.name);
            let record = match (matching.next(), matching.next()) {
                (Some(record), None) => record,
                (None, _) => bail!(
                    "`pixi build` of {location} did not produce source package {} (built: {})",
                    package.name.as_source(),
                    built
                        .iter()
                        .map(|r| r.identifier.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                (Some(_), Some(_)) => bail!(
                    "`pixi build` of {location} produced several variants of source package {}",
                    package.name.as_source()
                ),
            };
            let package_subdir = &record.package_record.subdir;
            if package_subdir != subdir.as_str() && package_subdir != Subdir::NoArch.as_str() {
                bail!(
                    "source package {} was built for {package_subdir}, not {} or noarch",
                    record.identifier,
                    subdir.as_str()
                );
            }
            Ok(record.clone())
        })
        .collect()
}

/// Run `pixi build` for the package at `manifest` (a directory or manifest file).
fn run_pixi_build(manifest: &Path, subdir: Subdir, output: &Path) -> Result<()> {
    if !manifest.exists() {
        bail!("{} does not exist", manifest.display());
    }
    let pixi = pixi_executable();
    let mut command = Command::new(&pixi);
    command
        .arg("build")
        .arg("--target-platform")
        .arg(subdir.as_str())
        .arg("--output-dir")
        .arg(output)
        .arg("--path")
        .arg(manifest)
        // The image may be written to stdout, so pixi's output goes to stderr.
        .stdout(Stdio::from(std::io::stderr()))
        .stdin(Stdio::null());
    tracing::info!("running {command:?}");
    let status = command.status().map_err(|e| {
        anyhow!(
            "could not run `{}`: {e}. pixi is needed to build source packages; install it \
             or set PIXI_EXE",
            pixi.to_string_lossy()
        )
    })?;
    if !status.success() {
        bail!("`{}` failed ({status})", command_line(&command));
    }
    Ok(())
}

fn command_line(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|arg| arg.to_string_lossy())
        .map(|arg| shlex::try_quote(&arg).map_or_else(|_| arg.to_string(), |a| a.into_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Records of the conda packages in `dir`.
fn built_records(dir: &Path) -> Result<Vec<RepoDataRecord>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("could not read {}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("could not read {}", dir.display()))?;
    paths.retain(|path| {
        path.is_file()
            && path
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(DistArchiveIdentifier::try_from_filename)
                .is_some()
    });
    paths.sort();
    paths.iter().map(|path| injected_record(path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_url_sources() {
        let dir = tempfile::tempdir().unwrap();
        let sources = [SourcePackage {
            name: "myapp".parse().unwrap(),
            location: UrlOrPath::Url("git+https://example.com/myapp.git".parse().unwrap()),
        }];
        let err = SourceBuilder::new(dir.path(), dir.path())
            .build(&sources, Subdir::Linux64)
            .unwrap_err();
        assert!(err.to_string().contains("--skip myapp"), "{err}");
    }

    #[test]
    fn rejects_missing_source_directories() {
        let dir = tempfile::tempdir().unwrap();
        let sources = [SourcePackage {
            name: "myapp".parse().unwrap(),
            location: UrlOrPath::Path("does-not-exist".into()),
        }];
        let err = SourceBuilder::new(dir.path(), dir.path())
            .build(&sources, Subdir::Linux64)
            .unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
    }

    #[test]
    fn quotes_command_lines() {
        let mut command = Command::new("pixi");
        command.args(["build", "--path", "my dir"]);
        assert_eq!(command_line(&command), "pixi build --path 'my dir'");
    }
}
