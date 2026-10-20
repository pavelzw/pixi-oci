//! Selecting an environment and platform from `pixi.lock`.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rattler_conda_types::{RepoDataRecord, Subdir};
use rattler_lock::{CondaPackageData, LockFile, LockedPackage};

/// The conda packages locked for one environment on one platform.
#[derive(Debug)]
pub struct LockedEnvironment {
    pub environment: String,
    pub subdir: Subdir,
    pub records: Vec<RepoDataRecord>,
}

/// Load the lockfile and resolve the requested environment and platform.
///
/// Platform resolution order: the explicit request (a lockfile platform name or a conda
/// subdir), then the current platform if it is linux, then the single linux platform of the
/// environment. Non-linux platforms are rejected.
pub fn load_locked_environment(
    lockfile_path: &Path,
    environment: &str,
    requested_platform: Option<&str>,
) -> Result<LockedEnvironment> {
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

    let platform = match requested_platform {
        Some(requested) => find_platform(&env, requested)?,
        None => default_platform(&env)?,
    };
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

    let records = packages
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

    Ok(LockedEnvironment {
        environment: environment.to_string(),
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
