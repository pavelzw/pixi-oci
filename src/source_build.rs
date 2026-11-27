//! Building the source packages of the lockfile (pixi-build) with `pixi publish`, like
//! pixi-pack.

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

/// Builds the source packages of all images and platforms of a build.
pub struct SourceBuilder {
    /// The directory of the lockfile, which relative source locations are relative to.
    workspace_dir: PathBuf,
    /// Where `pixi publish` puts the packages; they have to stay until they are installed.
    output_dir: PathBuf,
    builds: usize,
    /// Every package built so far, with the platform it was built for. A noarch package
    /// serves every platform.
    built: Vec<(Subdir, RepoDataRecord)>,
}

impl SourceBuilder {
    pub fn new(workspace_dir: &Path, output_dir: &Path) -> Self {
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
            builds: 0,
            built: Vec::new(),
        }
    }

    /// A package built earlier that serves `subdir`.
    fn earlier(&self, source: &SourcePackage, subdir: Subdir) -> Option<&RepoDataRecord> {
        self.built
            .iter()
            .find(|(s, r)| {
                r.package_record.name == source.name
                    && (*s == subdir || r.package_record.subdir == Subdir::NoArch.as_str())
            })
            .map(|(_, r)| r)
    }

    /// Build the `needed` source packages for `subdir` and return a record for each of them.
    ///
    /// `all` are the source packages of the environment, including those replaced by an
    /// injected package. They are built together in one `pixi publish`, which refuses packages
    /// whose source run dependencies it doesn't build as well. Nothing is built when earlier
    /// builds already provide every needed package, e.g. for another image, and a noarch
    /// package built for an earlier platform is reused, so all platforms install the same file.
    pub fn build(
        &mut self,
        all: &[SourcePackage],
        needed: &[&SourcePackage],
        subdir: Subdir,
    ) -> Result<Vec<RepoDataRecord>> {
        let earlier: Vec<Option<RepoDataRecord>> = needed
            .iter()
            .map(|source| self.earlier(source, subdir).cloned())
            .collect();
        if let Some(records) = earlier.iter().cloned().collect::<Option<Vec<_>>>() {
            return Ok(records);
        }

        let built = self.publish(all, subdir)?;
        let records = needed
            .iter()
            .zip(earlier)
            .map(|(source, earlier)| match earlier {
                Some(record) => Ok(record),
                None => select_package(&built, source, subdir),
            })
            .collect::<Result<Vec<_>>>()?;
        self.built
            .extend(built.into_iter().map(|record| (subdir, record)));
        Ok(records)
    }

    /// Run `pixi publish` for the manifests of `sources` and return all packages it built.
    fn publish(
        &mut self,
        sources: &[SourcePackage],
        subdir: Subdir,
    ) -> Result<Vec<RepoDataRecord>> {
        let mut manifests: Vec<PathBuf> = Vec::new();
        for source in sources {
            let manifest = match &source.location {
                UrlOrPath::Path(path) => self.workspace_dir.join(path.as_str()),
                UrlOrPath::Url(url) => bail!(
                    "cannot build source package {} from {url}: only source packages in a \
                     local directory can be built. Build it with `pixi publish` and use \
                     `--skip {}` with `--inject`",
                    source.name.as_source(),
                    source.name.as_source()
                ),
            };
            if !manifests.contains(&manifest) {
                manifests.push(manifest);
            }
        }
        let output = self.output_dir.join(self.builds.to_string());
        self.builds += 1;
        std::fs::create_dir_all(&output)
            .with_context(|| format!("could not create {}", output.display()))?;

        let names = sources
            .iter()
            .map(|p| p.name.as_source())
            .collect::<Vec<_>>()
            .join(", ");
        // Noarch packages are the same for every platform, so build them for the host: their
        // build and host environments then run here, without cross-compiling.
        let build_subdir = match Subdir::current() {
            Some(current) if sources.iter().all(|s| s.noarch) => {
                eprintln!("🏗️  Building {names} (noarch) with pixi publish");
                current
            }
            _ => {
                eprintln!(
                    "🏗️  Building {names} for {} with pixi publish",
                    subdir.as_str()
                );
                subdir
            }
        };
        run_pixi_publish(&manifests, build_subdir, &output)
            .with_context(|| format!("could not build source packages {names}"))?;
        let built = built_records(&output)?;
        for record in &built {
            eprintln!("📦 Built {}", record.identifier);
        }
        Ok(built)
    }
}

/// Pick the package of `source` from the packages `built` for `subdir`.
fn select_package(
    built: &[RepoDataRecord],
    source: &SourcePackage,
    subdir: Subdir,
) -> Result<RepoDataRecord> {
    let mut matching = built
        .iter()
        .filter(|r| r.package_record.name == source.name);
    let record = match (matching.next(), matching.next()) {
        (Some(record), None) => record,
        (None, _) => bail!(
            "`pixi publish` did not produce source package {} from {} (built: {})",
            source.name.as_source(),
            source.location,
            built
                .iter()
                .map(|r| r.identifier.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        (Some(_), Some(_)) => bail!(
            "`pixi publish` produced several variants of source package {}",
            source.name.as_source()
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
}

/// Run `pixi publish` for the packages at `manifests` (directories or manifest files),
/// copying the built packages to `output`.
fn run_pixi_publish(manifests: &[PathBuf], subdir: Subdir, output: &Path) -> Result<()> {
    if let Some(missing) = manifests.iter().find(|m| !m.exists()) {
        bail!("{} does not exist", missing.display());
    }
    let pixi = pixi_executable();
    let mut command = Command::new(&pixi);
    command
        .arg("publish")
        .arg("--target-platform")
        .arg(subdir.as_str())
        .arg("--target-dir")
        .arg(output);
    for manifest in manifests {
        command.arg("--path").arg(manifest);
    }
    command
        // Set by `pixi run` for the workspace pixi-oci runs in, not the one built here.
        .env_remove("PIXI_PROJECT_MANIFEST")
        .env_remove("PIXI_WORKSPACE_MANIFEST")
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
            noarch: true,
        }];
        let err = SourceBuilder::new(dir.path(), dir.path())
            .build(&sources, &[&sources[0]], Subdir::Linux64)
            .unwrap_err();
        assert!(err.to_string().contains("--skip myapp"), "{err}");
    }

    #[test]
    fn rejects_missing_source_directories() {
        let dir = tempfile::tempdir().unwrap();
        let sources = [SourcePackage {
            name: "myapp".parse().unwrap(),
            location: UrlOrPath::Path("does-not-exist".into()),
            noarch: true,
        }];
        let err = SourceBuilder::new(dir.path(), dir.path())
            .build(&sources, &[&sources[0]], Subdir::Linux64)
            .unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
    }

    #[test]
    fn quotes_command_lines() {
        let mut command = Command::new("pixi");
        command.args(["publish", "--path", "my dir"]);
        assert_eq!(command_line(&command), "pixi publish --path 'my dir'");
    }
}
