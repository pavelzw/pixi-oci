//! Splitting the environment into layers and writing them as tar blobs.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::Read,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rattler::install::from_archive::{PlannedPackage, PrefixSink};
use rattler_conda_types::{MatchSpec, ParseStrictness, RepoDataRecord};
use tar::{EntryType, Header};
use walkdir::WalkDir;

use crate::{
    config::{CopyEntry, normalize_image_path, relative_image_path},
    oci_layout::{Layer, OciLayout},
};

/// One file (or symlink) on the build host to put into the image.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Where the content lives on the build host.
    pub host_path: PathBuf,
    /// Absolute path inside the image.
    pub image_path: String,
}

/// An entry of a layer that does not come from a package.
#[derive(Debug, Clone)]
pub enum LayerEntry {
    /// A file, symlink or directory on the build host.
    Host(FileEntry),
    /// A file with the given contents.
    File {
        image_path: String,
        mode: u32,
        contents: Vec<u8>,
    },
    /// A symbolic link.
    Symlink { image_path: String, target: String },
    /// A directory with explicit permission bits (e.g. the sticky `/tmp`).
    Dir { image_path: String, mode: u32 },
}

impl LayerEntry {
    /// The absolute path of the entry inside the image.
    pub fn image_path(&self) -> &str {
        match self {
            LayerEntry::Host(file) => &file.image_path,
            LayerEntry::File { image_path, .. }
            | LayerEntry::Symlink { image_path, .. }
            | LayerEntry::Dir { image_path, .. } => image_path,
        }
    }
}

/// A layer that still has to be written.
#[derive(Debug, Clone, Default)]
pub struct PlannedLayer {
    /// Human readable description, ends up in the image history.
    pub description: String,
    /// Conda packages whose files are streamed from their archives into the layer, together
    /// with their `conda-meta` record.
    pub packages: Vec<PlannedPackage>,
    /// Absolute path of the environment inside the image, where `packages` are installed.
    pub install_prefix: String,
    /// Everything else in the layer.
    pub entries: Vec<LayerEntry>,
}

impl PlannedLayer {
    /// A layer with only `entries`.
    pub fn with_entries(description: impl Into<String>, entries: Vec<LayerEntry>) -> Self {
        Self {
            description: description.into(),
            entries,
            ..Self::default()
        }
    }
}

/// Decides which packages share a layer.
///
/// Implementations return the layers bottom to top, each as the group of packages it holds.
/// Every package should appear in exactly one group; packages left out end up in the last layer.
pub trait LayerSplitter {
    fn split<'a>(&self, packages: &'a [RepoDataRecord]) -> Result<Vec<Vec<&'a RepoDataRecord>>>;
}

/// Orders packages by how many other packages (transitively) depend on them, so that the
/// lowest layers hold the packages that change least often. The first `max_layers - 1`
/// packages each get their own layer, the remaining ones share the last layer.
#[derive(Debug, Clone, Copy)]
pub struct PopularitySplitter {
    pub max_layers: NonZeroUsize,
}

impl LayerSplitter for PopularitySplitter {
    fn split<'a>(&self, packages: &'a [RepoDataRecord]) -> Result<Vec<Vec<&'a RepoDataRecord>>> {
        let mut groups: Vec<Vec<&RepoDataRecord>> = Vec::new();
        for record in order_by_popularity(packages) {
            if groups.len() == self.max_layers.get() {
                groups
                    .last_mut()
                    .expect("max_layers is non-zero so a group exists")
                    .push(record);
            } else {
                groups.push(vec![record]);
            }
        }
        Ok(groups)
    }
}

/// Split the planned packages into layers using `splitter`.
pub fn plan_package_layers(
    packages: Vec<PlannedPackage>,
    install_prefix: &str,
    splitter: &dyn LayerSplitter,
) -> Result<Vec<PlannedLayer>> {
    let records: Vec<RepoDataRecord> = packages.iter().map(|p| p.record().clone()).collect();
    let groups: Vec<Vec<usize>> = {
        let index: HashMap<&str, usize> = records
            .iter()
            .enumerate()
            .map(|(i, r)| (r.package_record.name.as_normalized(), i))
            .collect();
        splitter
            .split(&records)?
            .into_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|r| index[r.package_record.name.as_normalized()])
                    .collect()
            })
            .collect()
    };

    let mut packages: Vec<Option<PlannedPackage>> = packages.into_iter().map(Some).collect();
    let mut layers: Vec<PlannedLayer> = Vec::new();
    for group in groups {
        let group: Vec<PlannedPackage> = group
            .into_iter()
            .filter_map(|i| packages[i].take())
            .collect();
        if group.is_empty() {
            continue;
        }
        let description = if let [package] = group.as_slice() {
            let pkg = &package.record().package_record;
            format!(
                "pixi-oci: {}-{}-{}",
                pkg.name.as_normalized(),
                pkg.version,
                pkg.build
            )
        } else {
            format!("pixi-oci: {} packages", group.len())
        };
        layers.push(PlannedLayer {
            description,
            packages: group,
            install_prefix: install_prefix.to_string(),
            entries: Vec::new(),
        });
    }

    // Packages the splitter left out go into the last layer.
    let leftovers: Vec<PlannedPackage> = packages.into_iter().flatten().collect();
    if !leftovers.is_empty() {
        match layers.last_mut() {
            Some(last) => last.packages.extend(leftovers),
            None => layers.push(PlannedLayer {
                description: format!("pixi-oci: {} packages", leftovers.len()),
                packages: leftovers,
                install_prefix: install_prefix.to_string(),
                entries: Vec::new(),
            }),
        }
    }
    Ok(layers)
}

/// Order packages by the number of packages that transitively depend on them (descending),
/// then by name for determinism.
fn order_by_popularity(records: &[RepoDataRecord]) -> Vec<&RepoDataRecord> {
    let index: HashMap<&str, usize> = records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.package_record.name.as_normalized(), i))
        .collect();

    // dependents[i] = packages that directly depend on i
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
    for (i, record) in records.iter().enumerate() {
        for dep in &record.package_record.depends {
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
                .package_record
                .name
                .as_normalized()
                .cmp(records[b].package_record.name.as_normalized())
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
                files.push(LayerEntry::Host(FileEntry {
                    host_path: walked.path().to_path_buf(),
                    image_path: normalize_image_path(&format!(
                        "{destination}/{}",
                        relative_image_path(rel)
                    )),
                }));
            }
        } else {
            files.push(LayerEntry::Host(FileEntry {
                host_path: source,
                image_path: destination,
            }));
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlannedLayer::with_entries(
        "pixi-oci: copy-to-image",
        files,
    )))
}

/// Modification time used for every tar entry. `SOURCE_DATE_EPOCH` when set, else 0.
pub fn entry_mtime() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

enum TarEntry<'a> {
    /// A directory with its permission bits.
    Dir(u32),
    Entry(&'a LayerEntry),
}

/// Write a planned layer as a gzip-compressed tar blob into the OCI directory.
///
/// The files of the packages are written in the order of their archives, followed by their
/// `conda-meta` record. The other entries follow sorted by path. Parent directories are emitted
/// explicitly, ownership is root and mtimes are fixed so the result is reproducible.
/// Directories listed explicitly keep their permission bits (e.g. the sticky `/tmp`), implicit
/// ones get `0755`.
pub fn write_layer(oci: &OciLayout, layer: &PlannedLayer) -> Result<Layer> {
    let builder = oci
        .create_layer(flate2::Compression::default())
        .context("could not create layer blob")?;
    let mut writer = LayerWriter {
        builder,
        mtime: entry_mtime(),
        emitted: HashSet::new(),
        dirs: BTreeSet::new(),
    };

    let prefix = layer.install_prefix.trim_matches('/');
    for package in &layer.packages {
        let mut sink = PackageSink {
            writer: &mut writer,
            prefix,
        };
        let record = package.link(&mut sink).with_context(|| {
            format!(
                "could not install {}",
                package.record().package_record.name.as_normalized()
            )
        })?;
        let mut json = Vec::new();
        record.write_to(&mut json, true)?;
        writer.add_file(
            &format!("{prefix}/conda-meta/{}", record.file_name()),
            0o644,
            json.len() as u64,
            &mut json.as_slice(),
        )?;
    }

    let mut entries: BTreeMap<String, TarEntry<'_>> = BTreeMap::new();
    for entry in &layer.entries {
        let rel = entry.image_path().trim_start_matches('/');
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
            entries
                .entry(parent.clone())
                .or_insert(TarEntry::Dir(0o755));
        }
        let new = match entry {
            LayerEntry::Dir { mode, .. } => TarEntry::Dir(*mode),
            LayerEntry::Host(file) => {
                let metadata = fs::symlink_metadata(&file.host_path)
                    .with_context(|| format!("could not stat {}", file.host_path.display()))?;
                if metadata.is_dir() {
                    TarEntry::Dir(host_mode(&metadata) & 0o7777)
                } else {
                    TarEntry::Entry(entry)
                }
            }
            _ => TarEntry::Entry(entry),
        };
        if matches!(
            (&new, entries.get(rel)),
            (TarEntry::Entry(_), Some(TarEntry::Dir(_)))
                | (TarEntry::Dir(_), Some(TarEntry::Entry(_)))
        ) {
            bail!(
                "{} is both a directory and a file in layer `{}`",
                entry.image_path(),
                layer.description
            );
        }
        entries.insert(rel.to_string(), new);
    }

    let mut hardlinks: HashMap<(u64, u64), String> = HashMap::new();
    for (path, entry) in &entries {
        match entry {
            TarEntry::Dir(mode) => writer.add_dir(path, *mode)?,
            TarEntry::Entry(LayerEntry::Dir { .. }) => {
                unreachable!("directories are TarEntry::Dir")
            }
            TarEntry::Entry(LayerEntry::File { mode, contents, .. }) => writer.add_file(
                path,
                normalized_mode(*mode),
                contents.len() as u64,
                &mut contents.as_slice(),
            )?,
            TarEntry::Entry(LayerEntry::Symlink { target, .. }) => {
                writer.add_symlink(path, Path::new(target))?;
            }
            TarEntry::Entry(LayerEntry::Host(file)) => {
                writer.add_host_file(path, &file.host_path, &mut hardlinks)?;
            }
        }
    }

    let builder = writer
        .builder
        .into_inner()
        .context("could not finish layer tar")?;
    builder.complete().context("could not complete layer blob")
}

/// Writes the entries of a layer and keeps track of the directories that already exist.
struct LayerWriter<W: std::io::Write> {
    builder: tar::Builder<W>,
    mtime: u64,
    /// All paths written so far.
    emitted: HashSet<String>,
    /// The directories written so far.
    dirs: BTreeSet<String>,
}

impl<W: std::io::Write> LayerWriter<W> {
    /// Emit the parent directories of `path` that were not written yet.
    fn add_parents(&mut self, path: &str) -> Result<()> {
        let mut end = 0;
        while let Some(i) = path[end..].find('/') {
            end += i;
            let parent = &path[..end];
            if !self.dirs.contains(parent) {
                self.add_dir(parent, 0o755)?;
            }
            end += 1;
        }
        Ok(())
    }

    fn claim(&mut self, path: &str) -> Result<()> {
        if !self.emitted.insert(path.to_string()) {
            bail!("{path} is added to the layer twice");
        }
        Ok(())
    }

    fn header(&self, entry_type: EntryType, mode: u32, size: u64) -> Header {
        let mut header = Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_mode(mode);
        header.set_size(size);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(self.mtime);
        let _ = header.set_username("root");
        let _ = header.set_groupname("root");
        header
    }

    fn add_dir(&mut self, path: &str, mode: u32) -> Result<()> {
        if self.dirs.contains(path) {
            return Ok(());
        }
        self.add_parents(path)?;
        self.claim(path)?;
        let mut header = self.header(EntryType::Directory, mode, 0);
        self.builder
            .append_data(&mut header, format!("{path}/"), std::io::empty())
            .with_context(|| format!("could not add directory {path}"))?;
        self.dirs.insert(path.to_string());
        Ok(())
    }

    fn add_file(
        &mut self,
        path: &str,
        mode: u32,
        size: u64,
        contents: &mut dyn Read,
    ) -> Result<()> {
        self.add_parents(path)?;
        self.claim(path)?;
        let mut header = self.header(EntryType::Regular, mode, size);
        self.builder
            .append_data(&mut header, path, contents.take(size))
            .with_context(|| format!("could not add file {path}"))
    }

    fn add_symlink(&mut self, path: &str, target: &Path) -> Result<()> {
        self.add_parents(path)?;
        self.claim(path)?;
        let mut header = self.header(EntryType::Symlink, 0o777, 0);
        self.builder
            .append_link(&mut header, path, target)
            .with_context(|| format!("could not add symlink {path}"))
    }

    fn add_host_file(
        &mut self,
        path: &str,
        host_path: &Path,
        hardlinks: &mut HashMap<(u64, u64), String>,
    ) -> Result<()> {
        let metadata = fs::symlink_metadata(host_path)
            .with_context(|| format!("could not stat {}", host_path.display()))?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(host_path)
                .with_context(|| format!("could not read link {}", host_path.display()))?;
            return self.add_symlink(path, &target);
        }
        if !metadata.is_file() {
            bail!(
                "{} is neither a regular file nor a symlink",
                host_path.display()
            );
        }
        if let Some(key) = hardlink_key(&metadata) {
            if let Some(original) = hardlinks.get(&key) {
                self.add_parents(path)?;
                self.claim(path)?;
                let mut header =
                    self.header(EntryType::Link, normalized_mode(host_mode(&metadata)), 0);
                return self
                    .builder
                    .append_link(&mut header, path, original)
                    .with_context(|| format!("could not add hard link {path}"));
            }
            hardlinks.insert(key, path.to_string());
        }
        let mut file = fs::File::open(host_path)
            .with_context(|| format!("could not open {}", host_path.display()))?;
        self.add_file(
            path,
            normalized_mode(host_mode(&metadata)),
            metadata.len(),
            &mut file,
        )
    }
}

/// Receives the files of a package and writes them below the install prefix.
struct PackageSink<'a, W: std::io::Write> {
    writer: &'a mut LayerWriter<W>,
    /// The install prefix without leading and trailing `/`.
    prefix: &'a str,
}

impl<W: std::io::Write> PackageSink<'_, W> {
    fn image_path(&self, path: &Path) -> String {
        format!("{}/{}", self.prefix, relative_image_path(path))
    }
}

fn to_io_error(error: anyhow::Error) -> std::io::Error {
    std::io::Error::other(format!("{error:#}"))
}

impl<W: std::io::Write> PrefixSink for PackageSink<'_, W> {
    fn add_file(
        &mut self,
        path: &Path,
        mode: u32,
        size: u64,
        contents: &mut dyn Read,
    ) -> std::io::Result<()> {
        let path = self.image_path(path);
        self.writer
            .add_file(&path, normalized_mode(mode), size, contents)
            .map_err(to_io_error)
    }

    fn add_symlink(&mut self, path: &Path, target: &Path) -> std::io::Result<()> {
        let path = self.image_path(path);
        self.writer.add_symlink(&path, target).map_err(to_io_error)
    }

    fn add_directory(&mut self, path: &Path, mode: u32) -> std::io::Result<()> {
        let path = self.image_path(path);
        self.writer
            .add_dir(&path, mode & 0o7777)
            .map_err(to_io_error)
    }
}

/// The permission bits of a file on the build host. Hosts without unix permissions only know
/// whether a file is read-only, so their files are not executable in the image.
fn host_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        if metadata.is_dir() { 0o755 } else { 0o644 }
    }
}

/// Identifies the file behind a hard link on hosts that have them.
fn hardlink_key(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.nlink() > 1).then(|| (metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
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
