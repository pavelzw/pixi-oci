//! Splitting the installed prefix into layers and writing them as tar blobs.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ocidir::{Layer, OciDir};
use rattler_conda_types::{MatchSpec, ParseStrictness, PrefixRecord};
use tar::{EntryType, Header};
use walkdir::WalkDir;

use crate::config::{CopyEntry, normalize_image_path};

/// One file (or symlink) to put into the image.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Where the content lives on the build host.
    pub host_path: PathBuf,
    /// Absolute path inside the image.
    pub image_path: String,
}

/// A layer that still has to be written.
#[derive(Debug, Clone)]
pub struct PlannedLayer {
    /// Human readable description, ends up in the image history.
    pub description: String,
    pub files: Vec<FileEntry>,
}

/// Assign the packages installed in `prefix` to at most `max_layers` layers.
///
/// Packages are ordered by how many other packages (transitively) depend on them, so that the
/// lowest layers hold the packages that change least often. The first `max_layers - 1` packages
/// each get their own layer, the remaining ones share the last layer together with every file in
/// the prefix that is not owned by a package.
pub fn plan_package_layers(
    prefix: &Path,
    install_prefix: &str,
    max_layers: usize,
) -> Result<Vec<PlannedLayer>> {
    if max_layers == 0 {
        bail!("max-layers must be at least 1");
    }
    let records = PrefixRecord::collect_from_prefix::<PrefixRecord>(prefix)
        .context("could not read conda-meta of the installed prefix")?;
    let ordered = order_by_popularity(&records);

    // The first `max_layers - 1` packages get their own layer, everything else shares the last.
    let mut groups: Vec<Vec<&PrefixRecord>> = Vec::new();
    for record in &ordered {
        if groups.len() == max_layers {
            groups
                .last_mut()
                .expect("max_layers >= 1 so a group exists")
                .push(record);
        } else {
            groups.push(vec![record]);
        }
    }

    let mut owned: HashSet<PathBuf> = HashSet::new();
    let mut layers: Vec<PlannedLayer> = groups
        .iter()
        .map(|group| {
            let mut files = Vec::new();
            for record in group {
                for rel in record.files.iter().cloned().chain(std::iter::once(
                    PathBuf::from("conda-meta").join(record.file_name()),
                )) {
                    owned.insert(rel.clone());
                    files.push(file_entry(prefix, install_prefix, &rel));
                }
            }
            let description = if let [record] = group.as_slice() {
                let pkg = &record.repodata_record.package_record;
                format!(
                    "pixi-oci: {}-{}-{}",
                    pkg.name.as_normalized(),
                    pkg.version,
                    pkg.build
                )
            } else {
                let names: Vec<_> = group
                    .iter()
                    .map(|r| r.repodata_record.package_record.name.as_normalized())
                    .collect();
                format!("pixi-oci: {} packages ({})", group.len(), names.join(", "))
            };
            PlannedLayer { description, files }
        })
        .collect();

    // Everything in the prefix that no package claims (conda-meta/history, generated scripts,
    // clobber backups, ...) goes into the last layer.
    let mut leftovers = Vec::new();
    for entry in WalkDir::new(prefix).follow_links(false).sort_by_file_name() {
        let entry = entry.context("could not walk the installed prefix")?;
        if entry.file_type().is_dir() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(prefix)
            .expect("walkdir entries are below the prefix")
            .to_path_buf();
        // rattler marks the prefix with a CACHEDIR.TAG for backup tools; it has no place in
        // the image.
        if rel == Path::new("CACHEDIR.TAG") {
            continue;
        }
        if !owned.contains(&rel) {
            leftovers.push(file_entry(prefix, install_prefix, &rel));
        }
    }
    if !leftovers.is_empty() {
        match layers.last_mut() {
            Some(last) => last.files.extend(leftovers),
            None => layers.push(PlannedLayer {
                description: "pixi-oci: environment".to_string(),
                files: leftovers,
            }),
        }
    }
    Ok(layers)
}

fn file_entry(prefix: &Path, install_prefix: &str, rel: &Path) -> FileEntry {
    let rel_str = rel.to_string_lossy();
    FileEntry {
        host_path: prefix.join(rel),
        image_path: normalize_image_path(&format!("{install_prefix}/{rel_str}")),
    }
}

/// Order packages by the number of packages that transitively depend on them (descending),
/// then by name for determinism.
fn order_by_popularity(records: &[PrefixRecord]) -> Vec<&PrefixRecord> {
    let index: HashMap<&str, usize> = records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.repodata_record.package_record.name.as_normalized(), i))
        .collect();

    // dependents[i] = packages that directly depend on i
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
    for (i, record) in records.iter().enumerate() {
        for dep in &record.repodata_record.package_record.depends {
            let Some(name) = dependency_name(dep) else {
                continue;
            };
            if let Some(&j) = index.get(name.as_str())
                && i != j
            {
                dependents[j].push(i);
            }
        }
    }

    let popularity: Vec<usize> = (0..records.len())
        .map(|start| {
            let mut seen = HashSet::new();
            let mut stack = vec![start];
            while let Some(node) = stack.pop() {
                for &d in &dependents[node] {
                    if seen.insert(d) {
                        stack.push(d);
                    }
                }
            }
            seen.remove(&start);
            seen.len()
        })
        .collect();

    let mut ordered: Vec<usize> = (0..records.len()).collect();
    ordered.sort_by(|&a, &b| {
        popularity[b].cmp(&popularity[a]).then_with(|| {
            records[a]
                .repodata_record
                .package_record
                .name
                .as_normalized()
                .cmp(
                    records[b]
                        .repodata_record
                        .package_record
                        .name
                        .as_normalized(),
                )
        })
    });
    ordered.into_iter().map(|i| &records[i]).collect()
}

/// The package name of a dependency spec like `libgcc >=13` or `python 3.12.* *_cpython`.
fn dependency_name(spec: &str) -> Option<String> {
    MatchSpec::from_str(spec, ParseStrictness::Lenient)
        .ok()
        .and_then(|m| m.name.into_exact())
        .map(|n| n.as_normalized().to_string())
        .or_else(|| spec.split_whitespace().next().map(|s| s.to_lowercase()))
}

/// Build the layer holding the `copy-to-image` entries. Returns `None` when there is nothing to copy.
pub fn plan_copy_layer(
    manifest_dir: &Path,
    entries: &[CopyEntry],
    working_dir: &str,
) -> Result<Option<PlannedLayer>> {
    let mut files = Vec::new();
    for entry in entries {
        let source = manifest_dir.join(entry.source());
        let destination = entry.destination(working_dir)?;
        let metadata = fs::symlink_metadata(&source).with_context(|| {
            format!(
                "copy-to-image source `{}` not found at {}",
                entry.source(),
                source.display()
            )
        })?;
        if metadata.is_dir() {
            for walked in WalkDir::new(&source)
                .follow_links(false)
                .sort_by_file_name()
            {
                let walked = walked?;
                if walked.file_type().is_dir() {
                    continue;
                }
                let rel = walked.path().strip_prefix(&source)?;
                files.push(FileEntry {
                    host_path: walked.path().to_path_buf(),
                    image_path: normalize_image_path(&format!(
                        "{destination}/{}",
                        rel.to_string_lossy()
                    )),
                });
            }
        } else {
            files.push(FileEntry {
                host_path: source,
                image_path: destination,
            });
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlannedLayer {
        description: "pixi-oci: copy-to-image".to_string(),
        files,
    }))
}

/// Modification time used for every tar entry. `SOURCE_DATE_EPOCH` when set, else 0.
pub fn entry_mtime() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

enum TarEntry {
    Dir,
    File(PathBuf),
}

/// Write a planned layer as a gzip-compressed tar blob into the OCI directory.
///
/// Entries are sorted, parent directories are emitted explicitly, ownership is root, mtimes are
/// fixed and hard links are deduplicated so the result is reproducible.
pub fn write_layer(oci: &OciDir, layer: &PlannedLayer) -> Result<Layer> {
    let mtime = entry_mtime();
    let mut entries: BTreeMap<String, TarEntry> = BTreeMap::new();
    for file in &layer.files {
        let rel = file.image_path.trim_start_matches('/');
        if rel.is_empty() {
            bail!("cannot copy a file to the image root `/`");
        }
        let mut parent = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
            entries.entry(parent.clone()).or_insert(TarEntry::Dir);
        }
        match entries.insert(rel.to_string(), TarEntry::File(file.host_path.clone())) {
            None | Some(TarEntry::File(_)) => {}
            Some(TarEntry::Dir) => bail!(
                "{} is both a directory and a file in layer `{}`",
                file.image_path,
                layer.description
            ),
        }
    }

    let mut builder = oci
        .create_layer(Some(flate2::Compression::default()))
        .context("could not create layer blob")?;
    builder.follow_symlinks(false);

    let mut hardlinks: HashMap<(u64, u64), String> = HashMap::new();
    let mut emitted_dirs: BTreeSet<String> = BTreeSet::new();
    for (path, entry) in &entries {
        match entry {
            TarEntry::Dir => {
                let mut header = Header::new_gnu();
                header.set_entry_type(EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                set_common(&mut header, mtime);
                builder
                    .append_data(&mut header, format!("{path}/"), std::io::empty())
                    .with_context(|| format!("could not add directory {path}"))?;
                emitted_dirs.insert(path.clone());
            }
            TarEntry::File(host_path) => {
                let metadata = fs::symlink_metadata(host_path)
                    .with_context(|| format!("could not stat {}", host_path.display()))?;
                let mut header = Header::new_gnu();
                set_common(&mut header, mtime);
                if metadata.file_type().is_symlink() {
                    let target = fs::read_link(host_path)
                        .with_context(|| format!("could not read link {}", host_path.display()))?;
                    header.set_entry_type(EntryType::Symlink);
                    header.set_mode(0o777);
                    header.set_size(0);
                    builder
                        .append_link(&mut header, path, &target)
                        .with_context(|| format!("could not add symlink {path}"))?;
                } else if metadata.is_file() {
                    header.set_mode(normalized_mode(metadata.permissions().mode()));
                    let key = (metadata.dev(), metadata.ino());
                    if metadata.nlink() > 1
                        && let Some(original) = hardlinks.get(&key)
                    {
                        header.set_entry_type(EntryType::Link);
                        header.set_size(0);
                        builder
                            .append_link(&mut header, path, original)
                            .with_context(|| format!("could not add hard link {path}"))?;
                        continue;
                    }
                    if metadata.nlink() > 1 {
                        hardlinks.insert(key, path.clone());
                    }
                    header.set_entry_type(EntryType::Regular);
                    header.set_size(metadata.len());
                    let file = fs::File::open(host_path)
                        .with_context(|| format!("could not open {}", host_path.display()))?;
                    builder
                        .append_data(&mut header, path, file)
                        .with_context(|| format!("could not add file {path}"))?;
                } else {
                    bail!(
                        "{} is neither a regular file nor a symlink",
                        host_path.display()
                    );
                }
            }
        }
    }

    let writer = builder.into_inner().context("could not finish layer tar")?;
    writer.complete().context("could not complete layer blob")
}

fn set_common(header: &mut Header, mtime: u64) {
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime);
    let _ = header.set_username("root");
    let _ = header.set_groupname("root");
}

/// Keep only the executable bit information, like `tar::HeaderMode::Deterministic`.
fn normalized_mode(mode: u32) -> u32 {
    if mode & 0o111 != 0 { 0o755 } else { 0o644 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_names_are_extracted() {
        assert_eq!(dependency_name("libgcc >=13").as_deref(), Some("libgcc"));
        assert_eq!(
            dependency_name("python 3.12.* *_cpython").as_deref(),
            Some("python")
        );
        assert_eq!(
            dependency_name("__glibc >=2.17,<3.0.a0").as_deref(),
            Some("__glibc")
        );
        assert_eq!(dependency_name("Foo").as_deref(), Some("foo"));
    }

    #[test]
    fn normalizes_modes() {
        assert_eq!(normalized_mode(0o100755), 0o755);
        assert_eq!(normalized_mode(0o100644), 0o644);
        assert_eq!(normalized_mode(0o100700), 0o755);
    }
}
