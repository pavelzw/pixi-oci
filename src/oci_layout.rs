//! Writing blobs into an OCI image layout directory.
//!
//! Blobs are written to a temporary file next to their destination and moved into place once
//! their digest is known, so a layout never contains partial blobs.

use std::{
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use flate2::write::GzEncoder;
use oci_spec::image::{Descriptor, DescriptorBuilder, ImageConfiguration, MediaType, Sha256Digest};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Where the blobs live inside the layout.
pub const BLOB_DIR: &str = "blobs/sha256";

/// An OCI image layout directory.
#[derive(Debug)]
pub struct OciLayout {
    root: PathBuf,
}

impl OciLayout {
    /// Open the layout at `root`, creating the directories and `oci-layout` file if needed.
    pub fn ensure(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join(BLOB_DIR))
            .with_context(|| format!("could not create {}", root.display()))?;
        let marker = root.join("oci-layout");
        if !marker.exists() {
            fs::write(&marker, r#"{"imageLayoutVersion":"1.0.0"}"#)
                .with_context(|| format!("could not write {}", marker.display()))?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn blob_path(&self, descriptor: &Descriptor) -> Result<PathBuf> {
        let Some(sha256) = descriptor.as_digest_sha256() else {
            bail!("unsupported digest {}", descriptor.digest());
        };
        Ok(self.root.join(BLOB_DIR).join(sha256))
    }

    /// Whether the blob of `descriptor` is already in the layout.
    pub fn has_blob(&self, descriptor: &Descriptor) -> Result<bool> {
        Ok(self.blob_path(descriptor)?.is_file())
    }

    /// Start writing a blob.
    pub fn create_blob(&self) -> Result<BlobWriter> {
        let dir = self.root.join(BLOB_DIR);
        let file = tempfile::NamedTempFile::with_prefix_in(".tmp-", &dir)
            .with_context(|| format!("could not create a blob in {}", dir.display()))?;
        Ok(BlobWriter {
            dir,
            file: BufWriter::with_capacity(1 << 20, file),
            hash: Sha256::new(),
            size: 0,
        })
    }

    /// Start writing a gzip-compressed tar layer.
    pub fn create_layer(
        &self,
        compression: flate2::Compression,
    ) -> Result<tar::Builder<LayerWriter>> {
        Ok(tar::Builder::new(LayerWriter {
            encoder: GzEncoder::new(self.create_blob()?, compression),
            uncompressed: Sha256::new(),
        }))
    }

    /// Write `value` as canonical JSON, which keeps the digest stable.
    pub fn write_json_blob(
        &self,
        value: &impl Serialize,
        media_type: MediaType,
    ) -> Result<DescriptorBuilder> {
        let mut writer = self.create_blob()?;
        value.serialize(&mut serde_json::Serializer::with_formatter(
            &mut writer,
            canon_json::CanonicalFormatter::new(),
        ))?;
        Ok(writer.complete()?.descriptor().media_type(media_type))
    }

    /// Write the image configuration.
    pub fn write_config(&self, config: ImageConfiguration) -> Result<Descriptor> {
        Ok(self
            .write_json_blob(&config, MediaType::ImageConfig)?
            .build()?)
    }
}

/// A blob that was written to the layout.
#[derive(Debug, Clone)]
pub struct Blob {
    sha256: Sha256Digest,
    size: u64,
}

impl Blob {
    /// A descriptor for the blob, without a media type.
    pub fn descriptor(&self) -> DescriptorBuilder {
        DescriptorBuilder::default()
            .digest(self.sha256.clone())
            .size(self.size)
    }
}

/// Writes a blob, see [`OciLayout::create_blob`].
pub struct BlobWriter {
    dir: PathBuf,
    file: BufWriter<tempfile::NamedTempFile>,
    hash: Sha256,
    size: u64,
}

impl BlobWriter {
    fn finish(self) -> Result<(String, u64, tempfile::NamedTempFile, PathBuf)> {
        let file = self
            .file
            .into_inner()
            .map_err(|e| e.into_error())
            .context("could not write blob")?;
        Ok((hex(&self.hash.finalize()), self.size, file, self.dir))
    }

    fn persist(
        sha256: String,
        size: u64,
        file: tempfile::NamedTempFile,
        dir: &Path,
    ) -> Result<Blob> {
        file.persist(dir.join(&sha256))
            .map_err(|e| e.error)
            .context("could not move blob into place")?;
        Ok(Blob {
            sha256: Sha256Digest::from_str(&sha256)?,
            size,
        })
    }

    /// Finish the blob.
    pub fn complete(self) -> Result<Blob> {
        let (sha256, size, file, dir) = self.finish()?;
        Self::persist(sha256, size, file, &dir)
    }

    /// Finish the blob, failing if it does not match `descriptor`.
    pub fn complete_verified_as(self, descriptor: &Descriptor) -> Result<Blob> {
        let (sha256, size, file, dir) = self.finish()?;
        if descriptor.as_digest_sha256() != Some(sha256.as_str()) {
            bail!(
                "digest mismatch: expected {}, got sha256:{sha256}",
                descriptor.digest()
            );
        }
        if descriptor.size() != size {
            bail!("size mismatch: expected {}, got {size}", descriptor.size());
        }
        Self::persist(sha256, size, file, &dir)
    }
}

impl Write for BlobWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buf)?;
        self.hash.update(&buf[..written]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// A layer that was written to the layout.
#[derive(Debug, Clone)]
pub struct Layer {
    blob: Blob,
    uncompressed_sha256: String,
}

impl Layer {
    /// The descriptor of the (compressed) layer blob.
    pub fn descriptor(&self) -> DescriptorBuilder {
        self.blob.descriptor().media_type(MediaType::ImageLayerGzip)
    }

    /// The digest of the uncompressed tar, the "diff id" of the layer.
    pub fn diff_id(&self) -> String {
        format!("sha256:{}", self.uncompressed_sha256)
    }
}

/// Compresses a layer into a blob, see [`OciLayout::create_layer`].
pub struct LayerWriter {
    encoder: GzEncoder<BlobWriter>,
    uncompressed: Sha256,
}

impl LayerWriter {
    /// Finish the layer.
    pub fn complete(self) -> Result<Layer> {
        let blob = self
            .encoder
            .finish()
            .context("could not compress layer")?
            .complete()?;
        Ok(Layer {
            blob,
            uncompressed_sha256: hex(&self.uncompressed.finalize()),
        })
    }
}

impl Write for LayerWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.encoder.write(buf)?;
        self.uncompressed.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.encoder.flush()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_verifies_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = OciLayout::ensure(tmp.path()).unwrap();
        assert!(tmp.path().join("oci-layout").is_file());

        let mut writer = layout.create_blob().unwrap();
        writer.write_all(b"hello").unwrap();
        let descriptor = writer
            .complete()
            .unwrap()
            .descriptor()
            .media_type(MediaType::ImageConfig)
            .build()
            .unwrap();
        assert_eq!(
            descriptor.digest().to_string(),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(layout.has_blob(&descriptor).unwrap());

        let mut writer = layout.create_blob().unwrap();
        writer.write_all(b"other").unwrap();
        assert!(writer.complete_verified_as(&descriptor).is_err());
        // Only the verified blob and nothing temporary is left.
        assert_eq!(fs::read_dir(tmp.path().join(BLOB_DIR)).unwrap().count(), 1);
    }

    #[test]
    fn layers_know_their_diff_id() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = OciLayout::ensure(tmp.path()).unwrap();
        let mut builder = layout.create_layer(flate2::Compression::default()).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        builder
            .append_data(&mut header, "file", &b"hello"[..])
            .unwrap();
        let layer = builder.into_inner().unwrap().complete().unwrap();

        let descriptor = layer.descriptor().build().unwrap();
        let blob = fs::read(
            tmp.path()
                .join(BLOB_DIR)
                .join(descriptor.as_digest_sha256().unwrap()),
        )
        .unwrap();
        let mut tar = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&blob[..]), &mut tar).unwrap();
        assert_eq!(
            layer.diff_id(),
            format!("sha256:{}", hex(&Sha256::digest(&tar)))
        );
    }
}
