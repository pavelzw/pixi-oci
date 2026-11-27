//! Pulling the base image from a registry into the output OCI layout.

use std::{io::Write, str::FromStr};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use oci_client::{
    Client, Reference,
    client::{ClientConfig, ClientProtocol},
    manifest::{ImageIndexEntry, OciDescriptor},
};
use oci_spec::image::{
    Arch, Descriptor, DescriptorBuilder, Digest, ImageConfiguration, MediaType, Os,
};
use ocidir::{OciDir, OciRead};

use crate::{
    config::SCRATCH,
    registry_auth::{AuthSource, CredentialSources, ResolvedAuth},
};

/// A registry base image and the credentials to pull it with.
#[derive(Debug)]
pub struct BaseImageSource {
    pub reference: Reference,
    pub auth: ResolvedAuth,
}

impl BaseImageSource {
    /// Parse `image` and look up credentials for its registry. Returns `None` for `scratch`.
    pub fn resolve(image: &str) -> Result<Option<Self>> {
        if image == SCRATCH {
            return Ok(None);
        }
        let reference: Reference = image
            .parse()
            .with_context(|| format!("invalid base image reference `{image}`"))?;
        let auth = CredentialSources::from_env().resolve(&reference)?;
        tracing::info!(
            "pulling {} from {} with {}",
            reference.repository(),
            reference.registry(),
            auth.source
        );
        Ok(Some(Self { reference, auth }))
    }
}

/// Like docker, talk plain HTTP to registries on the local machine (e.g. `localhost:5000`).
fn protocol_for(registry: &str) -> ClientProtocol {
    let host = registry
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(registry, |(host, _)| host);
    if matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        ClientProtocol::HttpsExcept(vec![registry.to_string()])
    } else {
        ClientProtocol::Https
    }
}

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

/// Pull the manifest, config and layers of `source` for `platform` into `output`.
pub async fn fetch_base_image(
    source: &BaseImageSource,
    platform: &TargetPlatform,
    output: &OciDir,
) -> Result<BaseImage> {
    let reference = &source.reference;
    let resolver_platform = platform.clone();
    let client = Client::new(ClientConfig {
        protocol: protocol_for(reference.resolve_registry()),
        platform_resolver: Some(Box::new(move |entries: &[ImageIndexEntry]| {
            entries
                .iter()
                .find(|e| resolver_platform.matches(e, true))
                .or_else(|| entries.iter().find(|e| resolver_platform.matches(e, false)))
                .map(|e| e.digest.clone())
        })),
        ..Default::default()
    });

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
    let (manifest, _digest, config_json) = client
        .pull_manifest_and_config(reference, &source.auth.auth)
        .await
        .with_context(|| {
            let hint = match &source.auth.source {
                AuthSource::Anonymous => format!(
                    "; for a private image, log in with `docker login` or `podman login`, \
                     or set {} and {}",
                    crate::registry_auth::USERNAME_ENV,
                    crate::registry_auth::PASSWORD_ENV
                ),
                other => format!(" with {other}"),
            };
            format!(
                "could not pull base image {} for linux/{}{hint}",
                reference.whole(),
                platform.arch
            )
        })?;
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
            download_blob(&client, reference, layer, &descriptor, output).await?;
        }
        layers.push(descriptor);
    }

    Ok(BaseImage {
        reference: reference.clone(),
        layers,
        config,
    })
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
    fn uses_http_only_for_local_registries() {
        for local in [
            "localhost",
            "localhost:5000",
            "127.0.0.1:5000",
            "[::1]:5000",
        ] {
            assert_eq!(
                protocol_for(local),
                ClientProtocol::HttpsExcept(vec![local.to_string()])
            );
        }
        for remote in ["ghcr.io", "index.docker.io", "localhost.example.com:5000"] {
            assert_eq!(protocol_for(remote), ClientProtocol::Https);
        }
    }

    #[test]
    fn normalizes_references() {
        let r: Reference = "ubuntu".parse().unwrap();
        assert_eq!(r.whole(), "docker.io/library/ubuntu:latest");
        let r: Reference = "ghcr.io/prefix-dev/pixi:0.40.0".parse().unwrap();
        assert_eq!(r.whole(), "ghcr.io/prefix-dev/pixi:0.40.0");
    }
}
