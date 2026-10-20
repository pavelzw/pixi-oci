//! Assembling the image config, manifest and index in an OCI layout directory.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, SecondsFormat, Utc};
use oci_client::Reference;
use oci_spec::image::{
    Config, Descriptor, HistoryBuilder, ImageConfiguration, ImageConfigurationBuilder,
    ImageIndexBuilder, ImageManifest, ImageManifestBuilder, MediaType, Os, RootFsBuilder,
    SCHEMA_VERSION,
};
use ocidir::OciDir;
use serde::Serialize;

use crate::{
    base_image::{BaseImage, TargetPlatform},
    layers::{PlannedLayer, write_layer},
};

/// Annotation holding the tag, read by podman, skopeo and containerd.
pub const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
/// Annotation containerd (and therefore `docker load`) prefers for the image name.
pub const CONTAINERD_NAME_ANNOTATION: &str = "io.containerd.image.name";

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

/// Merge the base image config with the user's config, following Dockerfile semantics:
/// fields set in `overlay` win, setting an entrypoint resets an inherited cmd, and labels,
/// ports and volumes accumulate.
pub fn merge_config(base: Option<&Config>, overlay: &Config) -> Config {
    let base = base.cloned().unwrap_or_default();
    let mut merged = Config::default();

    let (entrypoint, cmd) = match (overlay.entrypoint(), overlay.cmd()) {
        (Some(e), cmd) => (Some(e.clone()), cmd.clone()),
        (None, Some(c)) => (base.entrypoint().clone(), Some(c.clone())),
        (None, None) => (base.entrypoint().clone(), base.cmd().clone()),
    };
    merged.set_entrypoint(entrypoint);
    merged.set_cmd(cmd);

    merged.set_env(overlay.env().clone().or_else(|| base.env().clone()));
    merged.set_working_dir(
        overlay
            .working_dir()
            .clone()
            .or_else(|| base.working_dir().clone()),
    );
    merged.set_user(overlay.user().clone().or_else(|| base.user().clone()));
    merged.set_stop_signal(
        overlay
            .stop_signal()
            .clone()
            .or_else(|| base.stop_signal().clone()),
    );

    let mut labels: HashMap<String, String> = base.labels().clone().unwrap_or_default();
    labels.extend(overlay.labels().clone().unwrap_or_default());
    merged.set_labels(Some(labels).filter(|l| !l.is_empty()));

    merged.set_exposed_ports(union(base.exposed_ports(), overlay.exposed_ports()));
    merged.set_volumes(union(base.volumes(), overlay.volumes()));

    merged
}

/// `base` followed by the entries of `extra` not already in it; `None` when empty.
fn union(base: &Option<Vec<String>>, extra: &Option<Vec<String>>) -> Option<Vec<String>> {
    let mut out = base.clone().unwrap_or_default();
    for item in extra.iter().flatten() {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    Some(out).filter(|o| !o.is_empty())
}

/// Prepend `prefix` (e.g. `[shell, wrapper-script]`) to the entrypoint of `config`.
pub fn prepend_entrypoint(config: &mut Config, prefix: Vec<String>) {
    let mut entrypoint = prefix;
    entrypoint.extend(config.entrypoint().clone().unwrap_or_default());
    config.set_entrypoint(Some(entrypoint));
}

/// Inputs for [`assemble_image`].
pub struct AssembleInputs<'a> {
    pub output: &'a OciDir,
    pub output_dir: &'a Path,
    pub base: Option<&'a BaseImage>,
    pub layers: &'a [PlannedLayer],
    pub platform: &'a TargetPlatform,
    pub config: Config,
    pub created: DateTime<Utc>,
    /// `name:tag` as the user wrote it.
    pub reference: &'a str,
}

/// Reference the base layers (already in `output`), write the new layers, then config,
/// manifest and index.
pub fn assemble_image(inputs: AssembleInputs<'_>) -> Result<AssembledImage> {
    let mut layer_descriptors: Vec<Descriptor> = Vec::new();
    let mut diff_ids: Vec<String> = Vec::new();
    let mut history = Vec::new();

    if let Some(base) = inputs.base {
        layer_descriptors.extend(base.layers.iter().cloned());
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
    use oci_spec::image::ConfigBuilder;

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
        let overlay = ConfigBuilder::default()
            .entrypoint(vec!["caddy".to_string()])
            .build()
            .unwrap();
        let merged = merge_config(Some(&base()), &overlay);
        assert_eq!(
            merged.entrypoint().as_deref(),
            Some(&["caddy".to_string()][..])
        );
        assert!(merged.cmd().is_none());
        assert_eq!(merged.working_dir().as_deref(), Some("/base"));
    }

    #[test]
    fn inherits_when_unset_and_merges_labels() {
        let overlay = ConfigBuilder::default()
            .labels(HashMap::from([("b".to_string(), "mine".to_string())]))
            .exposed_ports(vec!["80/tcp".to_string(), "8080/tcp".to_string()])
            .build()
            .unwrap();
        let merged = merge_config(Some(&base()), &overlay);
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
        let overlay = ConfigBuilder::default()
            .entrypoint(vec!["caddy".to_string(), "run".to_string()])
            .build()
            .unwrap();
        let mut merged = merge_config(None, &overlay);
        prepend_entrypoint(
            &mut merged,
            vec![
                "/bin/bash".into(),
                "/opt/env/etc/pixi-oci/entrypoint.sh".into(),
            ],
        );
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
