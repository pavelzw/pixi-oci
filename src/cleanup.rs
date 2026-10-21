//! Removing temporary directories and partial outputs when a build is interrupted.
//!
//! Ctrl-C terminates the process without running destructors, so everything that should not
//! outlive an interrupted build is registered here and removed by [`clean_up`].

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use anyhow::{Context, Result};

static PATHS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn paths() -> MutexGuard<'static, Vec<PathBuf>> {
    PATHS.lock().unwrap_or_else(|e| e.into_inner())
}

fn unregister(path: &Path) {
    paths().retain(|p| p != path);
}

/// Remove everything still registered. Meant to be called right before exiting the process
/// after an interrupt: the lock is held until the process exits so that no new paths are
/// registered and the build cannot exit with its own error while the removal is in progress.
pub fn clean_up() -> MutexGuard<'static, Vec<PathBuf>> {
    let mut paths = paths();
    for path in paths.drain(..) {
        // Other threads may still be writing into a directory while it is removed.
        for _ in 0..5 {
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => tracing::debug!("could not remove {}: {e}", path.display()),
            }
        }
    }
    paths
}

/// A temporary directory that is also removed by [`clean_up`].
pub struct TempDir {
    inner: tempfile::TempDir,
}

impl TempDir {
    pub fn new(prefix: &str) -> Result<Self> {
        let mut paths = paths();
        let inner = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .context("could not create a temporary directory")?;
        paths.push(inner.path().to_path_buf());
        Ok(Self { inner })
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Runs before `inner` removes the directory.
        unregister(self.inner.path());
    }
}

/// A file being written that is removed again unless [`PartialFile::keep`] is called, both on
/// errors and by [`clean_up`].
pub struct PartialFile {
    path: Option<PathBuf>,
}

impl PartialFile {
    pub fn new(path: PathBuf) -> Self {
        paths().push(path.clone());
        Self { path: Some(path) }
    }

    pub fn keep(mut self) {
        if let Some(path) = self.path.take() {
            unregister(&path);
        }
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let mut paths = paths();
            paths.retain(|p| p != &path);
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test: `clean_up` drains the process-wide registry, which would race with others.
    #[test]
    fn removes_registered_paths() {
        let scratch = tempfile::tempdir().unwrap();
        let kept = scratch.path().join("kept");
        let dropped = scratch.path().join("dropped");
        std::fs::write(&kept, b"").unwrap();
        std::fs::write(&dropped, b"").unwrap();
        PartialFile::new(kept.clone()).keep();
        drop(PartialFile::new(dropped.clone()));
        assert!(kept.exists());
        assert!(!dropped.exists());

        let dir = TempDir::new("pixi-oci-test-").unwrap();
        let file = dir.path().join("partial");
        std::fs::write(&file, b"partial").unwrap();
        let partial = PartialFile::new(file.clone());
        drop(clean_up());
        assert!(!file.exists());
        assert!(!dir.path().exists());
        partial.keep();
    }
}
