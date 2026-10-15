//! Writing the final OCI layout tarball.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Serialize;
use tar::{Builder, HeaderMode};
use walkdir::WalkDir;

/// A docker-archive `manifest.json` entry, needed by Docker's classic image store which
/// cannot load a pure OCI layout tarball.
#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct DockerManifestEntry {
    pub config: String,
    pub repo_tags: Vec<String>,
    pub layers: Vec<String>,
}

/// Blob path inside the layout for a `sha256:<hex>` digest string.
pub fn blob_path(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("blobs/sha256/{hex}")
}

/// Write `manifest.json` into the layout directory.
pub fn write_docker_manifest(layout_dir: &Path, entry: &DockerManifestEntry) -> Result<()> {
    let contents = serde_json::to_vec(&[entry])?;
    std::fs::write(layout_dir.join("manifest.json"), contents)
        .context("could not write manifest.json")?;
    Ok(())
}

/// Where to write the archive.
pub enum Output {
    Stdout,
    File(PathBuf),
}

/// Tar the layout directory deterministically (sorted entries, normalized headers).
/// Returns the number of bytes written.
pub fn write_oci_archive(layout_dir: &Path, output: &Output) -> Result<u64> {
    let writer: Box<dyn Write> = match output {
        Output::Stdout => Box::new(std::io::stdout().lock()),
        Output::File(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("could not create {}", parent.display()))?;
            }
            Box::new(
                std::fs::File::create(path)
                    .with_context(|| format!("could not create {}", path.display()))?,
            )
        }
    };
    let counting = CountingWriter {
        inner: writer,
        bytes: 0,
    };
    let mut archive = Builder::new(counting);
    archive.mode(HeaderMode::Deterministic);
    archive.follow_symlinks(false);

    let entries = WalkDir::new(layout_dir)
        .sort_by_file_name()
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("could not walk layout directory")?;
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(layout_dir)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if entry.file_type().is_dir() {
            archive.append_dir(relative, path)?;
        } else {
            archive.append_path_with_name(path, relative)?;
        }
    }
    let mut counting = archive.into_inner().context("could not finish archive")?;
    counting.flush()?;
    Ok(counting.bytes)
}

struct CountingWriter {
    inner: Box<dyn Write>,
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
