//! Assembling the image config, manifest and index in an OCI layout directory.

use std::{collections::HashMap, io::Write, path::Path};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, SecondsFormat, Utc};
use oci_client::Reference;
use oci_spec::image::{
    Config, ConfigBuilder, Descriptor, HistoryBuilder, ImageConfiguration,
    ImageConfigurationBuilder, ImageIndexBuilder, ImageManifest, ImageManifestBuilder, MediaType,
    Os, RootFsBuilder, SCHEMA_VERSION,
};
use ocidir::{OciDir, OciRead};
use serde::Serialize;

use crate::{
    base_image::{BaseImage, TargetPlatform},
    layers::{PlannedLayer, write_layer},
};

/// Annotation holding the tag, read by podman, skopeo and containerd.
pub const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
/// Annotation containerd (and therefore `docker load`) prefers for the image name.
pub const CONTAINERD_NAME_ANNOTATION: &str = "io.containerd.image.name";

/// User-facing image settings that end up in the config.
#[derive(Debug, Default, Clone)]
pub struct ImageSettings {
    pub env: Vec<String>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub labels: HashMap<String, String>,
    pub exposed_ports: Vec<String>,
    pub volumes: Vec<String>,
    pub stop_signal: Option<String>,
    /// `[shell, wrapper-script]` prepended to the entrypoint when activation scripts exist.
    pub wrapper: Option<Vec<String>>,
}

/// Where the image ends up in the layout.
#[derive(Debug, Clone)]
pub struct AssembledImage {
    pub manifest: ImageManifest,
    pub manifest_descriptor: Descriptor,
    pub config_descriptor: Descriptor,
}

/// The image creation timestamp: `SOURCE_DATE_EPOCH` when set, otherwise now.
pub fn created_timestamp() -> DateTime<Utc> {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
        .unwrap_or_else(Utc::now)
}

/// Merge the base image config with the user settings, following Dockerfile semantics:
/// setting an entrypoint resets an inherited cmd; labels, ports and volumes accumulate.
pub fn merge_config(base: Option<&Config>, settings: &ImageSettings) -> Result<Config> {
    let mut builder = ConfigBuilder::default();

    let (entrypoint, cmd) = match (&settings.entrypoint, &settings.cmd) {
        (Some(e), cmd) => (Some(e.clone()), cmd.clone()),
        (None, Some(c)) => (base.and_then(|b| b.entrypoint().clone()), Some(c.clone())),
        (None, None) => (
            base.and_then(|b| b.entrypoint().clone()),
            base.and_then(|b| b.cmd().clone()),
        ),
    };
    let entrypoint = match &settings.wrapper {
        Some(wrapper) => {
            let mut full = wrapper.clone();
            full.extend(entrypoint.unwrap_or_default());
            Some(full)
        }
        None => entrypoint,
    };
    if let Some(entrypoint) = entrypoint {
        builder = builder.entrypoint(entrypoint);
    }
    if let Some(cmd) = cmd {
        builder = builder.cmd(cmd);
    }

    builder = builder.env(settings.env.clone());

    if let Some(working_dir) = settings
        .working_dir
        .clone()
        .or_else(|| base.and_then(|b| b.working_dir().clone()))
    {
        builder = builder.working_dir(working_dir);
    }
    if let Some(user) = settings
        .user
        .clone()
        .or_else(|| base.and_then(|b| b.user().clone()))
    {
        builder = builder.user(user);
    }
    if let Some(stop_signal) = settings
        .stop_signal
        .clone()
        .or_else(|| base.and_then(|b| b.stop_signal().clone()))
    {
        builder = builder.stop_signal(stop_signal);
    }

    let mut labels: HashMap<String, String> =
        base.and_then(|b| b.labels().clone()).unwrap_or_default();
    labels.extend(settings.labels.clone());
    if !labels.is_empty() {
        builder = builder.labels(labels);
    }

    let mut ports: Vec<String> = base
        .and_then(|b| b.exposed_ports().clone())
        .unwrap_or_default();
    for port in &settings.exposed_ports {
        if !ports.contains(port) {
            ports.push(port.clone());
        }
    }
    if !ports.is_empty() {
        builder = builder.exposed_ports(ports);
    }

    let mut volumes: Vec<String> = base.and_then(|b| b.volumes().clone()).unwrap_or_default();
    for volume in &settings.volumes {
        if !volumes.contains(volume) {
            volumes.push(volume.clone());
        }
    }
    if !volumes.is_empty() {
        builder = builder.volumes(volumes);
    }

    Ok(builder.build()?)
}

/// Inputs for [`assemble_image`].
pub struct AssembleInputs<'a> {
    pub output: &'a OciDir,
    pub output_dir: &'a Path,
    pub base: Option<&'a BaseImage>,
    pub base_cache: &'a OciDir,
    pub layers: &'a [PlannedLayer],
    pub platform: &'a TargetPlatform,
    pub config: Config,
    pub created: DateTime<Utc>,
    /// `name:tag` as the user wrote it.
    pub reference: &'a str,
}

/// Copy the base layers, write the new layers, then config, manifest and index.
pub fn assemble_image(inputs: AssembleInputs<'_>) -> Result<AssembledImage> {
    let mut layer_descriptors: Vec<Descriptor> = Vec::new();
    let mut diff_ids: Vec<String> = Vec::new();
    let mut history = Vec::new();

    if let Some(base) = inputs.base {
        for descriptor in &base.layers {
            if !inputs.output.has_blob(descriptor)? {
                let mut reader = inputs
                    .base_cache
                    .read_blob(descriptor)
                    .context("could not read cached base layer")?;
                let mut writer = inputs.output.create_blob()?;
                std::io::copy(&mut reader, &mut writer)?;
                writer.flush()?;
                writer.complete_verified_as(descriptor)?;
            }
            layer_descriptors.push(descriptor.clone());
        }
        diff_ids.extend(base.config.rootfs().diff_ids().iter().cloned());
        history.extend(base.config.history().clone().unwrap_or_default());
    }

    let created = inputs.created.to_rfc3339_opts(SecondsFormat::Secs, true);
    for planned in inputs.layers {
        eprintln!(
            "📦 Writing layer: {} ({} files)",
            planned.description,
            planned.files.len()
        );
        let layer = write_layer(inputs.output, planned)
            .with_context(|| format!("could not write layer `{}`", planned.description))?;
        diff_ids.push(layer.uncompressed_sha256_as_digest().to_string());
        layer_descriptors.push(layer.descriptor().build()?);
        history.push(
            HistoryBuilder::default()
                .created(created.clone())
                .created_by(planned.description.clone())
                .build()?,
        );
    }

    let mut config_builder = ImageConfigurationBuilder::default()
        .created(created)
        .architecture(inputs.platform.arch.clone())
        .os(Os::Linux)
        .config(inputs.config)
        .rootfs(
            RootFsBuilder::default()
                .typ("layers")
                .diff_ids(diff_ids)
                .build()?,
        )
        .history(history);
    if let Some(variant) = &inputs.platform.variant {
        config_builder = config_builder.variant(variant.clone());
    }
    let image_config: ImageConfiguration = config_builder.build()?;
    let config_descriptor = inputs.output.write_config(image_config)?;

    let manifest = ImageManifestBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageManifest)
        .config(config_descriptor.clone())
        .layers(layer_descriptors)
        .build()?;

    let normalized: Reference = inputs
        .reference
        .parse()
        .map_err(|e| anyhow!("invalid image reference `{}`: {e}", inputs.reference))?;
    let annotations = HashMap::from([
        (
            REF_NAME_ANNOTATION.to_string(),
            inputs.reference.to_string(),
        ),
        (CONTAINERD_NAME_ANNOTATION.to_string(), normalized.whole()),
    ]);
    let manifest_descriptor = inputs
        .output
        .write_json_blob(&manifest, MediaType::ImageManifest)?
        .platform(inputs.platform.to_oci_platform()?)
        .annotations(annotations)
        .build()?;

    let index = ImageIndexBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageIndex)
        .manifests(vec![manifest_descriptor.clone()])
        .build()?;
    // Canonical JSON keeps the key order stable, which keeps the tarball reproducible.
    let mut index_json = Vec::new();
    index.serialize(&mut serde_json::Serializer::with_formatter(
        &mut index_json,
        canon_json::CanonicalFormatter::new(),
    ))?;
    std::fs::write(inputs.output_dir.join("index.json"), index_json)
        .context("could not write index.json")?;

    Ok(AssembledImage {
        manifest,
        manifest_descriptor,
        config_descriptor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        ConfigBuilder::default()
            .entrypoint(vec!["/base-entry".to_string()])
            .cmd(vec!["base-cmd".to_string()])
            .working_dir("/base")
            .labels(HashMap::from([("a".to_string(), "base".to_string())]))
            .exposed_ports(vec!["80/tcp".to_string()])
            .build()
            .unwrap()
    }

    #[test]
    fn entrypoint_resets_inherited_cmd() {
        let settings = ImageSettings {
            entrypoint: Some(vec!["caddy".into()]),
            ..Default::default()
        };
        let merged = merge_config(Some(&base()), &settings).unwrap();
        assert_eq!(
            merged.entrypoint().as_deref(),
            Some(&["caddy".to_string()][..])
        );
        assert!(merged.cmd().is_none());
        assert_eq!(merged.working_dir().as_deref(), Some("/base"));
    }

    #[test]
    fn inherits_when_unset_and_merges_labels() {
        let settings = ImageSettings {
            labels: HashMap::from([("b".to_string(), "mine".to_string())]),
            exposed_ports: vec!["80/tcp".into(), "8080/tcp".into()],
            ..Default::default()
        };
        let merged = merge_config(Some(&base()), &settings).unwrap();
        assert_eq!(
            merged.entrypoint().as_deref(),
            Some(&["/base-entry".to_string()][..])
        );
        assert_eq!(merged.cmd().as_deref(), Some(&["base-cmd".to_string()][..]));
        let labels = merged.labels().as_ref().unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(
            merged.exposed_ports().as_deref(),
            Some(&["80/tcp".to_string(), "8080/tcp".to_string()][..])
        );
    }

    #[test]
    fn wrapper_is_prepended() {
        let settings = ImageSettings {
            entrypoint: Some(vec!["caddy".into(), "run".into()]),
            wrapper: Some(vec![
                "/bin/bash".into(),
                "/opt/env/etc/pixi-oci/entrypoint.sh".into(),
            ]),
            ..Default::default()
        };
        let merged = merge_config(None, &settings).unwrap();
        assert_eq!(
            merged.entrypoint().as_deref(),
            Some(
                &[
                    "/bin/bash".to_string(),
                    "/opt/env/etc/pixi-oci/entrypoint.sh".to_string(),
                    "caddy".to_string(),
                    "run".to_string()
                ][..]
            )
        );
    }
}
