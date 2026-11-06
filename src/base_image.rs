//! Pulling the base image from a registry into the output OCI layout.

use std::{io::Write, str::FromStr};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use oci_client::{
    Client, Reference,
    client::ClientConfig,
    manifest::{ImageIndexEntry, OciDescriptor, OciManifest},
    secrets::RegistryAuth,
};
use oci_spec::image::{
    Arch, Descriptor, DescriptorBuilder, Digest, ImageConfiguration, MediaType, Os,
};
use ocidir::{OciDir, OciRead};

use crate::{config::SCRATCH, install::USER_AGENT};

/// A base image whose layers are available in the output layout.
#[derive(Debug)]
pub struct BaseImage {
    pub reference: Reference,
    /// Layer descriptors with OCI media types, in order.
    pub layers: Vec<Descriptor>,
    pub config: ImageConfiguration,
}

/// The target platform of the image.
#[derive(Debug, Clone)]
pub struct TargetPlatform {
    pub arch: Arch,
    pub variant: Option<String>,
}

impl TargetPlatform {
    pub fn to_oci_platform(&self) -> Result<oci_spec::image::Platform> {
        let mut builder = oci_spec::image::PlatformBuilder::default()
            .os(Os::Linux)
            .architecture(self.arch.clone());
        if let Some(variant) = &self.variant {
            builder = builder.variant(variant.clone());
        }
        Ok(builder.build()?)
    }

    fn matches(&self, entry: &ImageIndexEntry, require_variant: bool) -> bool {
        entry.platform.as_ref().is_some_and(|p| {
            p.os == Os::Linux
                && p.architecture == self.arch
                && (!require_variant || p.variant == self.variant)
        })
    }
}

/// Map registry media types (docker or OCI) to the OCI media types used in the layout.
pub fn layer_media_type(media_type: &str) -> Result<MediaType> {
    Ok(match media_type {
        "application/vnd.docker.image.rootfs.diff.tar" => MediaType::ImageLayer,
        "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => MediaType::ImageLayerGzip,
        other => match MediaType::from(other) {
            MediaType::ImageLayer
            | MediaType::ImageLayerGzip
            | MediaType::ImageLayerZstd
            | MediaType::ImageLayerNonDistributable
            | MediaType::ImageLayerNonDistributableGzip
            | MediaType::ImageLayerNonDistributableZstd => MediaType::from(other),
            _ => bail!("unsupported layer media type `{other}`"),
        },
    })
}

fn to_oci_descriptor(layer: &OciDescriptor) -> Result<Descriptor> {
    let digest = Digest::from_str(&layer.digest)
        .with_context(|| format!("invalid layer digest {}", layer.digest))?;
    let size = u64::try_from(layer.size).context("negative layer size")?;
    Ok(DescriptorBuilder::default()
        .media_type(layer_media_type(&layer.media_type)?)
        .digest(digest)
        .size(size)
        .build()?)
}

/// A registry client for pulling base images, shared by all platforms of a build so that
/// connections and tokens are reused. The platform is picked per pull by [`fetch_base_image`].
pub fn registry_client() -> Client {
    Client::new(ClientConfig {
        user_agent: USER_AGENT,
        platform_resolver: None,
        ..Default::default()
    })
}

/// The digest of the manifest for `platform` in an image index, preferring an exact variant
/// match.
fn select_manifest(entries: &[ImageIndexEntry], platform: &TargetPlatform) -> Option<String> {
    entries
        .iter()
        .find(|e| platform.matches(e, true))
        .or_else(|| entries.iter().find(|e| platform.matches(e, false)))
        .map(|e| e.digest.clone())
}

/// Pull the manifest, config and layers of `image` for `platform` into `output`.
/// Returns `None` for `scratch`.
pub async fn fetch_base_image(
    client: &Client,
    image: &str,
    platform: &TargetPlatform,
    output: &OciDir,
) -> Result<Option<BaseImage>> {
    if image == SCRATCH {
        return Ok(None);
    }
    let reference: Reference = image
        .parse()
        .with_context(|| format!("invalid base image reference `{image}`"))?;

    eprintln!(
        "⬇️  Pulling base image {} ({}/{}{})",
        reference.whole(),
        Os::Linux,
        platform.arch,
        platform
            .variant
            .as_deref()
            .map(|v| format!("/{v}"))
            .unwrap_or_default()
    );
    let context = || {
        format!(
            "could not pull base image {} for linux/{}",
            reference.whole(),
            platform.arch
        )
    };
    // A multi-platform image is an index of per-platform manifests: pick ours, then pull it by
    // digest.
    let (top, digest) = client
        .pull_manifest(&reference, &RegistryAuth::Anonymous)
        .await
        .with_context(context)?;
    let digest = match top {
        OciManifest::Image(_) => digest,
        OciManifest::ImageIndex(index) => select_manifest(&index.manifests, platform)
            .ok_or_else(|| anyhow!("{} has no image for this platform", reference.whole()))
            .with_context(context)?,
    };
    let (manifest, _digest, config_json) = client
        .pull_manifest_and_config(
            &reference.clone_with_digest(digest),
            &RegistryAuth::Anonymous,
        )
        .await
        .with_context(context)?;
    let config = ImageConfiguration::from_reader(config_json.as_bytes())
        .context("could not parse the base image config")?;
    if config.os() != &Os::Linux || config.architecture() != &platform.arch {
        bail!(
            "base image {} is {}/{} but the build targets linux/{}",
            reference.whole(),
            config.os(),
            config.architecture(),
            platform.arch
        );
    }

    let mut layers = Vec::with_capacity(manifest.layers.len());
    for layer in &manifest.layers {
        let descriptor = to_oci_descriptor(layer)?;
        if output.has_blob(&descriptor)? {
            tracing::debug!("base layer {} already present", layer.digest);
        } else {
            download_blob(client, &reference, layer, &descriptor, output).await?;
        }
        layers.push(descriptor);
    }

    Ok(Some(BaseImage {
        reference,
        layers,
        config,
    }))
}

async fn download_blob(
    client: &Client,
    reference: &Reference,
    layer: &OciDescriptor,
    descriptor: &Descriptor,
    output: &OciDir,
) -> Result<()> {
    let mut stream = client
        .pull_blob_stream(reference, layer)
        .await
        .with_context(|| format!("could not download layer {}", layer.digest))?;
    let progress = indicatif::ProgressBar::new(stream.content_length.unwrap_or(descriptor.size()));
    progress.set_style(
        indicatif::ProgressStyle::with_template(
            "  {msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({eta})",
        )
        .expect("valid template")
        .progress_chars("=> "),
    );
    progress.set_message(short_digest(&layer.digest));

    let mut writer = output.create_blob()?;
    while let Some(chunk) = stream.stream.next().await {
        let chunk = chunk.with_context(|| format!("error while downloading {}", layer.digest))?;
        writer.write_all(&chunk)?;
        progress.inc(chunk.len() as u64);
    }
    writer
        .complete_verified_as(descriptor)
        .map_err(|e| anyhow!("layer {} failed verification: {e}", layer.digest))?;
    progress.finish_and_clear();
    Ok(())
}

fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    hex.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_docker_layer_types_to_oci() {
        assert_eq!(
            layer_media_type("application/vnd.docker.image.rootfs.diff.tar.gzip").unwrap(),
            MediaType::ImageLayerGzip
        );
        assert_eq!(
            layer_media_type("application/vnd.oci.image.layer.v1.tar+zstd").unwrap(),
            MediaType::ImageLayerZstd
        );
        assert!(layer_media_type("application/vnd.oci.image.config.v1+json").is_err());
    }

    #[test]
    fn selects_manifest_for_platform() {
        let entry = |digest: &str, arch: &str, variant: Option<&str>| -> ImageIndexEntry {
            serde_json::from_value(serde_json::json!({
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": digest,
                "size": 1,
                "platform": { "os": "linux", "architecture": arch, "variant": variant },
            }))
            .unwrap()
        };
        let platform = |arch, variant: Option<&str>| TargetPlatform {
            arch,
            variant: variant.map(str::to_string),
        };
        let entries = [
            entry("sha256:amd64", "amd64", None),
            entry("sha256:arm64", "arm64", None),
            entry("sha256:arm64v8", "arm64", Some("v8")),
        ];
        assert_eq!(
            select_manifest(&entries, &platform(Arch::Amd64, None)).as_deref(),
            Some("sha256:amd64")
        );
        // An exact variant match wins over an earlier entry without variant.
        assert_eq!(
            select_manifest(&entries, &platform(Arch::ARM64, Some("v8"))).as_deref(),
            Some("sha256:arm64v8")
        );
        // Without an exact match, any variant of the architecture will do.
        assert_eq!(
            select_manifest(&entries[..2], &platform(Arch::ARM64, Some("v8"))).as_deref(),
            Some("sha256:arm64")
        );
        assert_eq!(
            select_manifest(&entries, &platform(Arch::s390x, None)),
            None
        );
    }

    #[test]
    fn normalizes_references() {
        let r: Reference = "ubuntu".parse().unwrap();
        assert_eq!(r.whole(), "docker.io/library/ubuntu:latest");
        let r: Reference = "ghcr.io/prefix-dev/pixi:0.40.0".parse().unwrap();
        assert_eq!(r.whole(), "ghcr.io/prefix-dev/pixi:0.40.0");
    }
}
