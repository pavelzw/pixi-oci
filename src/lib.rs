//! Build OCI images from pixi environments without a container daemon.
//!
//! The pipeline: read `pixi.toml` + `pixi.lock`, install the locked conda packages of one
//! environment into a staging prefix with rattler, split the prefix into layers, stack them
//! on a base image pulled straight from a registry and write an OCI layout (directory or tar).

pub mod activation;
pub mod archive;
pub mod base_image;
pub mod cleanup;
pub mod config;
pub mod image;
pub mod inject;
pub mod install;
pub mod layers;
pub mod lockfile;
pub mod rootfs;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use oci_spec::image::Arch;
use ocidir::{OciDir, cap_std};
use rattler_conda_types::Subdir;
use rattler_lock::DEFAULT_ENVIRONMENT_NAME;

use crate::{
    activation::{ActivationInputs, prepare_activation},
    archive::{
        DockerManifestEntry, Output, blob_path, prune_blobs, write_docker_manifest,
        write_oci_archive,
    },
    base_image::{TargetPlatform, fetch_base_image},
    cleanup::{PartialFile, TempDir},
    config::{
        BaseSpec, DEFAULT_ACTIVATION_SHELL, DEFAULT_INSTALL_PREFIX, DEFAULT_MAX_LAYERS,
        DEFAULT_TAG, LoadedManifest,
    },
    image::{AssembleInputs, assemble_image, created_timestamp, merge_config, prepend_entrypoint},
    inject::inject_packages,
    install::{install_environment, load_config, reqwest_client_from_config},
    layers::{PopularitySplitter, plan_copy_layer, plan_package_layers},
    lockfile::load_locked_environment,
    rootfs::{check_glibc, plan_rootfs_layers, upstream_glibc_version},
};

/// Output format of a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// An OCI layout tarball that also carries a docker-archive `manifest.json`.
    #[default]
    OciTar,
    /// An OCI layout directory.
    OciDir,
}

/// Options for [`build`]. CLI flags override the `[tool.pixi-oci]` section.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// `pixi.toml`, `pyproject.toml` or the directory containing it.
    pub manifest_path: PathBuf,
    pub environment: Option<String>,
    pub platform: Option<String>,
    /// Output path; `-` writes the tarball to stdout. Defaults to `<name>.tar` / `<name>/`.
    pub output: Option<PathBuf>,
    pub format: OutputFormat,
    /// `name:tag` or `name`.
    pub tag: Option<String>,
    /// A rattler/pixi config file (mirrors, authentication).
    pub rattler_config: Option<PathBuf>,
    /// Local conda packages (`.conda` or `.tar.bz2`) to add to the locked environment.
    pub inject: Vec<PathBuf>,
}

/// Summary of a finished build.
#[derive(Debug, Clone)]
pub struct BuildReport {
    pub output: Option<PathBuf>,
    pub reference: String,
    pub subdir: Subdir,
    pub manifest_digest: String,
    pub config_digest: String,
    pub base_layers: usize,
    /// Layers of a scratch base's root filesystem (glibc, shadow setup).
    pub rootfs_layers: usize,
    pub package_layers: usize,
    pub copy_layer: bool,
    pub packages: usize,
}

/// Map a conda subdir to the OCI platform.
pub fn target_platform(subdir: Subdir) -> Result<TargetPlatform> {
    let (arch, variant) = match subdir {
        Subdir::Linux64 => (Arch::Amd64, None),
        Subdir::LinuxAarch64 => (Arch::ARM64, Some("v8")),
        Subdir::LinuxArmV7l => (Arch::ARM, Some("v7")),
        Subdir::LinuxPpc64le => (Arch::PowerPC64le, None),
        Subdir::LinuxS390X => (Arch::s390x, None),
        Subdir::LinuxRiscv64 => (Arch::RISCV64, None),
        Subdir::LinuxLoongArch64 => (Arch::LoongArch64, None),
        other => bail!(
            "no OCI platform mapping for conda platform {}",
            other.as_str()
        ),
    };
    Ok(TargetPlatform {
        arch,
        variant: variant.map(str::to_string),
    })
}

/// Split `name:tag` into its parts. A `:` after the last `/` separates the tag.
fn split_reference(reference: &str) -> (String, Option<String>) {
    let last_slash = reference.rfind('/').map_or(0, |i| i + 1);
    match reference[last_slash..].rfind(':') {
        Some(i) => (
            reference[..last_slash + i].to_string(),
            Some(reference[last_slash + i + 1..].to_string()),
        ),
        None => (reference.to_string(), None),
    }
}

fn open_dir(path: &Path) -> Result<cap_std::fs::Dir> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
        .with_context(|| format!("could not open {}", path.display()))
}

/// Run a build.
pub async fn build(options: BuildOptions) -> Result<BuildReport> {
    let loaded = LoadedManifest::load(&options.manifest_path)?;
    let cfg = loaded.config().clone();

    let environment = options
        .environment
        .clone()
        .or_else(|| cfg.environment.clone())
        .unwrap_or_else(|| DEFAULT_ENVIRONMENT_NAME.to_string());
    let install_prefix = cfg
        .install_prefix
        .clone()
        .unwrap_or_else(|| DEFAULT_INSTALL_PREFIX.to_string());
    if !install_prefix.starts_with('/') || config::normalize_image_path(&install_prefix) == "/" {
        bail!("install-prefix must be an absolute path below `/`, got `{install_prefix}`");
    }
    let install_prefix = config::normalize_image_path(&install_prefix);
    let max_layers = cfg.max_layers.unwrap_or(DEFAULT_MAX_LAYERS);
    let base_spec = cfg.base.clone().unwrap_or_default();
    let activation_shell = cfg
        .activation_shell
        .clone()
        .unwrap_or_else(|| DEFAULT_ACTIVATION_SHELL.to_string());

    let (name, tag) = options
        .tag
        .as_deref()
        .or(cfg.tag.as_deref())
        .map(split_reference)
        .filter(|(name, _)| !name.is_empty())
        .ok_or_else(|| {
            anyhow!("no image name: set `tag` in [tool.pixi-oci] or `--tag name:tag`")
        })?;
    let tag = tag.unwrap_or_else(|| DEFAULT_TAG.to_string());
    let reference = format!("{name}:{tag}");
    // Fail before downloading and installing anything if the registry would reject the name.
    let normalized_reference: oci_client::Reference = reference
        .parse()
        .map_err(|e| anyhow!("invalid image reference `{reference}`: {e}"))?;

    let lockfile_path = loaded.dir.join("pixi.lock");
    let mut locked = load_locked_environment(
        &lockfile_path,
        &environment,
        options.platform.as_deref().or(cfg.platform.as_deref()),
    )?;
    let subdir = locked.subdir;
    let platform = target_platform(subdir)?;
    inject_packages(&mut locked.records, &options.inject, subdir)?;
    eprintln!(
        "🔧 Building {reference} for {} (environment `{environment}`, {} packages)",
        subdir.as_str(),
        locked.records.len()
    );

    if let BaseSpec::Scratch(scratch) = &base_spec {
        let glibc = scratch
            .glibc
            .debian_version()
            .map(upstream_glibc_version)
            .transpose()?;
        check_glibc(&locked.records, glibc.as_ref())?;
    }

    // The conda package cache, shared with pixi. Override with `RATTLER_CACHE_DIR`.
    let cache_dir = rattler_cache::default_cache_dir()
        .map_err(|e| anyhow!("could not determine the cache directory: {e}"))?;

    // Decide where the layout goes. Base image layers are downloaded straight into it.
    let default_output = || match options.format {
        OutputFormat::OciTar => PathBuf::from(format!("{}.tar", name.replace('/', "_"))),
        OutputFormat::OciDir => PathBuf::from(name.replace('/', "_")),
    };
    let output_path = options.output.clone().unwrap_or_else(default_output);
    let to_stdout = output_path.as_os_str() == "-";
    let layout_tmp;
    let layout_dir: PathBuf = match options.format {
        OutputFormat::OciDir => {
            if to_stdout {
                bail!("--format oci-dir cannot write to stdout");
            }
            output_path.clone()
        }
        OutputFormat::OciTar => {
            layout_tmp = TempDir::new("pixi-oci-layout-")?;
            layout_tmp.path().to_path_buf()
        }
    };
    let output_oci =
        OciDir::ensure(open_dir(&layout_dir)?).context("could not create the output OCI layout")?;

    let rattler_config = load_config(options.rattler_config.as_deref())?.unwrap_or_default();
    let client = reqwest_client_from_config(&rattler_config)?;

    // Fetch the base while nothing else needs the network heavily: pull the registry image, or
    // assemble the root filesystem for a scratch base.
    let rootfs_staging = TempDir::new("pixi-oci-rootfs-")?;
    let (base, rootfs_layers) = match &base_spec {
        BaseSpec::Image(image) => (
            fetch_base_image(image, &platform, &output_oci).await?,
            Vec::new(),
        ),
        BaseSpec::Scratch(scratch) => (
            None,
            plan_rootfs_layers(scratch, subdir, &client, rootfs_staging.path()).await?,
        ),
    };

    // Install the environment into a staging directory mirroring the in-image layout.
    let staging = TempDir::new("pixi-oci-")?;
    let staging_prefix = staging.path().join(install_prefix.trim_start_matches('/'));
    std::fs::create_dir_all(&staging_prefix)?;
    let packages = locked.records.len();
    eprintln!("⬇️  Installing {packages} packages into {install_prefix}");
    install_environment(
        &staging_prefix,
        &install_prefix,
        subdir,
        locked.records,
        client,
        &cache_dir,
    )
    .await?;

    // Activation: static env vars plus the wrapper script (written into the staging prefix,
    // so it has to happen before the layers are planned).
    let base_config = base.as_ref().and_then(|b| b.config.config().as_ref());
    let base_env: Vec<(String, String)> = base_config
        .and_then(|c| c.env().as_ref())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let user_env = cfg
        .oci_config
        .env
        .as_ref()
        .map(|e| e.to_pairs())
        .transpose()?
        .unwrap_or_default();
    let activation = prepare_activation(ActivationInputs {
        staging_prefix: &staging_prefix,
        install_prefix: &install_prefix,
        subdir,
        base_env: &base_env,
        user_env: &user_env,
    })?;

    let working_dir = cfg
        .oci_config
        .working_dir
        .clone()
        .or_else(|| base_config.and_then(|c| c.working_dir().clone()))
        .unwrap_or_else(|| "/".to_string());

    let mut layers = rootfs_layers;
    let rootfs_layer_count = layers.len();
    layers.extend(plan_package_layers(
        &staging_prefix,
        &install_prefix,
        &PopularitySplitter { max_layers },
    )?);
    let package_layers = layers.len() - rootfs_layer_count;
    let copy_layer = plan_copy_layer(&loaded.dir, &cfg.copy_to_image, &working_dir)?;
    let has_copy_layer = copy_layer.is_some();
    layers.extend(copy_layer);

    let mut user_config = cfg.oci_config.to_oci_config()?;
    // The activation env already contains the base image env and the user's env.
    user_config.set_env(Some(activation.env));
    let mut merged = merge_config(base_config, &user_config);
    if let Some(wrapper) = activation.wrapper {
        prepend_entrypoint(&mut merged, vec![activation_shell, wrapper]);
    }

    let assembled = assemble_image(AssembleInputs {
        output: &output_oci,
        output_dir: &layout_dir,
        base: base.as_ref(),
        layers: &layers,
        platform: &platform,
        config: merged,
        created: created_timestamp(),
        reference: &reference,
        normalized_reference: &normalized_reference,
    })?;

    let manifest_digest = assembled.manifest_descriptor.digest().to_string();
    let config_digest = assembled.config_descriptor.digest().to_string();

    let output = match options.format {
        OutputFormat::OciDir => {
            // index.json only references this build, so blobs of earlier builds are garbage.
            let keep: Vec<&str> = [&manifest_digest, &config_digest]
                .into_iter()
                .map(String::as_str)
                .chain(
                    assembled
                        .manifest
                        .layers()
                        .iter()
                        .map(|l| l.digest().as_ref()),
                )
                .collect();
            let removed = prune_blobs(&layout_dir, &keep)?;
            if removed > 0 {
                tracing::info!("removed {removed} blobs of earlier builds");
            }
            eprintln!("✅ Wrote OCI layout to {}", output_path.display());
            Some(output_path)
        }
        OutputFormat::OciTar => {
            write_docker_manifest(
                &layout_dir,
                &DockerManifestEntry {
                    config: blob_path(&config_digest),
                    repo_tags: vec![reference.clone()],
                    layers: assembled
                        .manifest
                        .layers()
                        .iter()
                        .map(|l| blob_path(l.digest().as_ref()))
                        .collect(),
                },
            )?;
            let destination = if to_stdout {
                Output::Stdout
            } else {
                Output::File(output_path.clone())
            };
            let partial = (!to_stdout).then(|| PartialFile::new(output_path.clone()));
            let bytes = write_oci_archive(&layout_dir, &destination)?;
            if let Some(partial) = partial {
                partial.keep();
            }
            if to_stdout {
                eprintln!("✅ Wrote {reference} ({bytes} bytes) to stdout");
                None
            } else {
                eprintln!(
                    "✅ Wrote {reference} ({bytes} bytes) to {}",
                    output_path.display()
                );
                Some(output_path)
            }
        }
    };

    Ok(BuildReport {
        output,
        reference,
        subdir,
        manifest_digest,
        config_digest,
        base_layers: base.as_ref().map_or(0, |b| b.layers.len()),
        rootfs_layers: rootfs_layer_count,
        package_layers,
        copy_layer: has_copy_layer,
        packages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_references() {
        assert_eq!(
            split_reference("foo:1.0"),
            ("foo".to_string(), Some("1.0".to_string()))
        );
        assert_eq!(split_reference("foo"), ("foo".to_string(), None));
        assert_eq!(
            split_reference("localhost:5000/foo"),
            ("localhost:5000/foo".to_string(), None)
        );
        assert_eq!(
            split_reference("localhost:5000/foo:dev"),
            ("localhost:5000/foo".to_string(), Some("dev".to_string()))
        );
    }

    #[test]
    fn maps_platforms() {
        assert_eq!(target_platform(Subdir::Linux64).unwrap().arch, Arch::Amd64);
        let arm = target_platform(Subdir::LinuxAarch64).unwrap();
        assert_eq!(arm.arch, Arch::ARM64);
        assert_eq!(arm.variant.as_deref(), Some("v8"));
        assert!(target_platform(Subdir::OsxArm64).is_err());
    }
}
