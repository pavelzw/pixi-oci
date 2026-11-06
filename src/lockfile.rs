//! Selecting an environment and platform from `pixi.lock`.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rattler_conda_types::{PackageName, RepoDataRecord, Subdir};
use rattler_lock::{CondaPackageData, LockFile, LockedPackage};

/// The conda packages locked for one environment on one platform.
#[derive(Debug)]
pub struct LockedEnvironment {
    pub environment: String,
    /// The platform name in the lockfile, usually the same as `subdir`.
    pub platform: String,
    pub subdir: Subdir,
    pub records: Vec<RepoDataRecord>,
}

/// Load the lockfile and resolve the requested environment and platforms, one
/// [`LockedEnvironment`] per platform in the requested order.
///
/// Each requested platform is a lockfile platform name or a conda subdir. Without a request,
/// the current platform is used if it is linux, else the single linux platform of the
/// environment. Non-linux platforms are rejected, and so are two platforms with the same
/// subdir, as they would be the same OCI platform.
///
/// Conda packages named in `skip` are left out, including source packages, which can't be
/// installed otherwise. Every name in `skip` must match a locked conda package on every
/// platform.
pub fn load_locked_environments(
    lockfile_path: &Path,
    environment: &str,
    requested_platforms: &[String],
    skip: &[PackageName],
) -> Result<Vec<LockedEnvironment>> {
    let lockfile = LockFile::from_path(lockfile_path)
        .with_context(|| format!("could not read lockfile {}", lockfile_path.display()))?;

    let env = lockfile.environment(environment).ok_or_else(|| {
        let mut names: Vec<_> = lockfile.environments().map(|(n, _)| n).collect();
        names.sort_unstable();
        anyhow!(
            "environment `{environment}` not found in {}. Available environments: {}",
            lockfile_path.display(),
            names.join(", ")
        )
    })?;

    let platforms = if requested_platforms.is_empty() {
        vec![default_platform(&env)?]
    } else {
        requested_platforms
            .iter()
            .map(|requested| find_platform(&env, requested))
            .collect::<Result<Vec<_>>>()?
    };
    for (i, platform) in platforms.iter().enumerate() {
        if let Some(other) = platforms[..i]
            .iter()
            .find(|p| p.subdir() == platform.subdir())
        {
            bail!(
                "platforms `{}` and `{}` both have the conda platform {}; an image index can \
                 only hold one image per platform",
                other.name().as_str(),
                platform.name().as_str(),
                platform.subdir().as_str()
            );
        }
    }

    platforms
        .into_iter()
        .map(|platform| locked_environment(&env, environment, platform, skip))
        .collect()
}

fn locked_environment(
    env: &rattler_lock::Environment<'_>,
    environment: &str,
    platform: rattler_lock::Platform<'_>,
    skip: &[PackageName],
) -> Result<LockedEnvironment> {
    let subdir = platform.subdir();
    if !subdir.is_linux() {
        bail!(
            "platform {} is not a linux platform; OCI images can only be built for linux platforms",
            subdir.as_str()
        );
    }

    let packages = env.packages(platform).ok_or_else(|| {
        anyhow!(
            "environment `{environment}` has no packages for platform {}",
            subdir.as_str()
        )
    })?;

    let mut skipped = vec![false; skip.len()];
    let records = packages
        .filter(|package| {
            let Some(name) = package.as_conda().map(CondaPackageData::name) else {
                return true;
            };
            match skip.iter().position(|s| s == name) {
                Some(i) => {
                    skipped[i] = true;
                    eprintln!("⏭️  Skipping {}", name.as_source());
                    false
                }
                None => true,
            }
        })
        .map(|package| match package {
            LockedPackage::Conda(CondaPackageData::Binary(binary)) => {
                RepoDataRecord::try_from(&**binary).with_context(|| {
                    format!("could not convert locked package {}", binary.file_name)
                })
            }
            LockedPackage::Conda(CondaPackageData::Source(source)) => Err(anyhow!(
                "source package {} is not supported",
                source.location
            )),
            LockedPackage::Pypi(_) => Err(anyhow!(
                "PyPI package {} is not supported, use conda packages instead",
                package.location()
            )),
        })
        .collect::<Result<Vec<_>>>()?;

    let unmatched: Vec<_> = skip
        .iter()
        .zip(&skipped)
        .filter(|(_, skipped)| !**skipped)
        .map(|(name, _)| name.as_source())
        .collect();
    if !unmatched.is_empty() {
        bail!(
            "cannot skip {}: not a conda package of environment `{environment}` on {}",
            unmatched.join(", "),
            subdir.as_str()
        );
    }

    Ok(LockedEnvironment {
        environment: environment.to_string(),
        platform: platform.name().as_str().to_string(),
        subdir,
        records,
    })
}

/// Find the requested platform in the locked environment: a platform name from the lockfile
/// wins over a conda subdir, a plain subdir only works when a single named platform uses it.
fn find_platform<'lock>(
    environment: &rattler_lock::Environment<'lock>,
    requested: &str,
) -> Result<rattler_lock::Platform<'lock>> {
    if let Some(platform) = environment
        .platforms()
        .find(|p| p.name().as_str() == requested)
    {
        return Ok(platform);
    }
    let matches: Vec<_> = environment
        .platforms()
        .filter(|p| p.subdir().as_str() == requested)
        .collect();
    match matches.as_slice() {
        [platform] => Ok(*platform),
        [] => Err(anyhow!(
            "platform `{requested}` not found in lockfile. Available platforms: {}",
            platform_choices(environment)
        )),
        multiple => {
            let mut names: Vec<_> = multiple.iter().map(|p| p.name().as_str()).collect();
            names.sort_unstable();
            Err(anyhow!(
                "platform `{requested}` is ambiguous, use one of the platform names instead: {}",
                names.join(", ")
            ))
        }
    }
}

fn default_platform<'lock>(
    environment: &rattler_lock::Environment<'lock>,
) -> Result<rattler_lock::Platform<'lock>> {
    let linux: Vec<_> = environment
        .platforms()
        .filter(|p| p.subdir().is_linux())
        .collect();
    if let Some(current) = Subdir::current()
        && current.is_linux()
        && let Some(platform) = linux.iter().find(|p| p.subdir() == current)
    {
        return Ok(*platform);
    }
    match linux.as_slice() {
        [platform] => Ok(*platform),
        [] => Err(anyhow!(
            "the environment has no linux platforms (found: {}); add one to `platforms` in the manifest",
            platform_choices(environment)
        )),
        _ => Err(anyhow!(
            "multiple linux platforms available, choose one with --platform: {}",
            platform_choices(environment)
        )),
    }
}

fn platform_choices(environment: &rattler_lock::Environment<'_>) -> String {
    let mut choices: Vec<_> = environment
        .platforms()
        .map(|p| {
            let name = p.name().as_str();
            let subdir = p.subdir().as_str();
            if name == subdir {
                name.to_string()
            } else {
                format!("{name} ({subdir})")
            }
        })
        .collect();
    choices.sort_unstable();
    choices.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCKFILE: &str = r#"version: 7
platforms:
- name: linux-64
environments:
  default:
    channels:
    - url: https://conda.anaconda.org/conda-forge/
    packages:
      linux-64:
      - conda: https://conda.anaconda.org/conda-forge/linux-64/caddy-2.11.7-hfc2019e_0.conda
      - conda_source: myapp[0123abcd] @ .
packages:
- conda: https://conda.anaconda.org/conda-forge/linux-64/caddy-2.11.7-hfc2019e_0.conda
  sha256: 0000000000000000000000000000000000000000000000000000000000000000
  md5: 00000000000000000000000000000000
  license: Apache-2.0
  size: 1
  timestamp: 1
- conda_source: myapp[0123abcd] @ .
  variants:
    target_platform: noarch
  depends:
  - caddy
"#;

    fn load_platforms(platforms: &[&str], skip: &[&str]) -> Result<Vec<LockedEnvironment>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pixi.lock");
        std::fs::write(&path, LOCKFILE).unwrap();
        let platforms: Vec<_> = platforms.iter().map(|p| p.to_string()).collect();
        let skip: Vec<_> = skip.iter().map(|s| s.parse().unwrap()).collect();
        load_locked_environments(&path, "default", &platforms, &skip)
    }

    fn load(skip: &[&str]) -> Result<LockedEnvironment> {
        let mut envs = load_platforms(&[], skip)?;
        assert_eq!(envs.len(), 1);
        Ok(envs.remove(0))
    }

    fn names(env: &LockedEnvironment) -> Vec<&str> {
        env.records
            .iter()
            .map(|r| r.package_record.name.as_normalized())
            .collect()
    }

    #[test]
    fn rejects_source_packages() {
        let err = load(&[]).unwrap_err();
        assert!(err.to_string().contains("source package"), "{err}");
    }

    #[test]
    fn skips_source_packages() {
        let env = load(&["myapp"]).unwrap();
        assert_eq!(names(&env), ["caddy"]);
    }

    #[test]
    fn skips_binary_packages() {
        let env = load(&["myapp", "caddy"]).unwrap();
        assert!(names(&env).is_empty());
    }

    #[test]
    fn rejects_unknown_skipped_packages() {
        let err = load(&["myapp", "nope"]).unwrap_err();
        assert!(err.to_string().contains("cannot skip nope"), "{err}");
    }

    #[test]
    fn resolves_requested_platforms() {
        let envs = load_platforms(&["linux-64"], &["myapp"]).unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].platform, "linux-64");
        assert_eq!(envs[0].subdir, Subdir::Linux64);
    }

    #[test]
    fn rejects_duplicate_platforms() {
        let err = load_platforms(&["linux-64", "linux-64"], &["myapp"]).unwrap_err();
        assert!(err.to_string().contains("one image per platform"), "{err}");
    }

    #[test]
    fn rejects_unknown_platforms() {
        let err = load_platforms(&["linux-64", "linux-aarch64"], &["myapp"]).unwrap_err();
        assert!(err.to_string().contains("not found in lockfile"), "{err}");
    }
}
