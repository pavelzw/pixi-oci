//! Injecting local conda packages that are not part of `pixi.lock`, like `pixi-pack --inject`.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rattler_conda_types::{
    PackageRecord, RepoDataRecord, Subdir, package::DistArchiveIdentifier, package::IndexJson,
};
use rattler_digest::{Md5, Sha256, compute_file_digest};
use rattler_package_streaming::seek::read_package_file;
use url::Url;

/// Build a record for a local `.conda` or `.tar.bz2` file from the `index.json` inside it.
pub fn injected_record(path: &Path) -> Result<RepoDataRecord> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("could not find injected package {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| anyhow!("invalid package file name {}", path.display()))?;
    let identifier = DistArchiveIdentifier::try_from_filename(file_name).ok_or_else(|| {
        anyhow!("injected package {file_name} is not a `.conda` or `.tar.bz2` file")
    })?;

    let index_json: IndexJson = read_package_file(&path)
        .with_context(|| format!("could not read index.json from {}", path.display()))?;
    let size = std::fs::metadata(&path)?.len();
    let sha256 = compute_file_digest::<Sha256>(&path)?;
    let md5 = compute_file_digest::<Md5>(&path)?;
    let package_record =
        PackageRecord::from_index_json(index_json, Some(size), Some(sha256), Some(md5))
            .with_context(|| format!("could not determine the subdir of {file_name}"))?;

    let url = Url::from_file_path(&path)
        .map_err(|()| anyhow!("could not convert {} to a URL", path.display()))?;
    Ok(RepoDataRecord {
        package_record,
        identifier,
        url,
        channel: None,
    })
}

/// Add the packages at `paths` to the locked `records`. An injected package replaces a locked
/// package of the same name. Fails when a package is built for another platform, or when the
/// dependencies and constraints of the resulting environment are not satisfied.
pub fn inject_packages(
    records: &mut Vec<RepoDataRecord>,
    paths: &[impl AsRef<Path>],
    subdir: Subdir,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut injected: Vec<RepoDataRecord> = Vec::with_capacity(paths.len());
    for path in paths {
        let record = injected_record(path.as_ref())?;
        let package = &record.package_record;
        if package.subdir != subdir.as_str() && package.subdir != Subdir::NoArch.as_str() {
            bail!(
                "injected package {} is built for {}, not {} or noarch",
                record.identifier,
                package.subdir,
                subdir.as_str()
            );
        }
        if let Some(other) = injected
            .iter()
            .find(|r| r.package_record.name == package.name)
        {
            bail!(
                "injected packages {} and {} have the same name",
                other.identifier,
                record.identifier
            );
        }
        injected.push(record);
    }

    for record in injected {
        let name = &record.package_record.name;
        if let Some(locked) = records.iter().position(|r| &r.package_record.name == name) {
            let locked = records.remove(locked);
            eprintln!(
                "💉 Injecting {} (replaces {})",
                record.identifier, locked.identifier
            );
        } else {
            eprintln!("💉 Injecting {}", record.identifier);
        }
        records.push(record);
    }

    PackageRecord::validate(records.iter().map(|r| &r.package_record).collect())
        .map_err(|e| anyhow!(e))
        .context("the injected packages are not compatible with the environment")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rattler_conda_types::{
        PackageName, VersionWithSource, compression_level::CompressionLevel,
    };
    use rattler_package_streaming::write::write_tar_bz2_package;

    use super::*;

    /// Write `<name>-1.0-0.tar.bz2` with a single file `share/<name>.txt` into `dir`.
    fn write_package(dir: &Path, name: &str, subdir: &str, depends: &[&str]) -> PathBuf {
        let contents = dir.join(format!("{name}-contents"));
        std::fs::create_dir_all(contents.join("info")).unwrap();
        std::fs::create_dir_all(contents.join("share")).unwrap();
        let file = format!("share/{name}.txt");
        std::fs::write(contents.join(&file), format!("hello from {name}\n")).unwrap();
        let index = serde_json::json!({
            "name": name,
            "version": "1.0",
            "build": "0",
            "build_number": 0,
            "subdir": subdir,
            "depends": depends,
            "noarch": (subdir == "noarch").then_some("generic"),
        });
        std::fs::write(contents.join("info/index.json"), index.to_string()).unwrap();
        let paths = serde_json::json!({
            "paths_version": 1,
            "paths": [{ "_path": file, "path_type": "hardlink" }],
        });
        std::fs::write(contents.join("info/paths.json"), paths.to_string()).unwrap();

        let archive = dir.join(format!("{name}-1.0-0.tar.bz2"));
        write_tar_bz2_package(
            std::fs::File::create(&archive).unwrap(),
            &contents,
            &["info/index.json", "info/paths.json", &file].map(|p| contents.join(p)),
            CompressionLevel::Default,
            None,
            None,
        )
        .unwrap();
        archive
    }

    fn locked(name: &str) -> RepoDataRecord {
        let mut record = PackageRecord::new(
            PackageName::new_unchecked(name),
            "2.0".parse::<VersionWithSource>().unwrap(),
            "0".parse().unwrap(),
        );
        record.subdir = "linux-64".to_string();
        RepoDataRecord {
            package_record: record,
            identifier: format!("{name}-2.0-0.conda").parse().unwrap(),
            url: format!("https://example.com/linux-64/{name}-2.0-0.conda")
                .parse()
                .unwrap(),
            channel: None,
        }
    }

    #[test]
    fn reads_record_from_package() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_package(dir.path(), "foo", "noarch", &["bar >=2"]);
        let record = injected_record(&path).unwrap();
        assert_eq!(record.package_record.name.as_normalized(), "foo");
        assert_eq!(record.package_record.depends, vec!["bar >=2"]);
        assert_eq!(record.identifier.to_string(), "foo-1.0-0.tar.bz2");
        assert_eq!(record.url.scheme(), "file");
        assert!(record.package_record.sha256.is_some());
    }

    #[test]
    fn injects_and_replaces_packages() {
        let dir = tempfile::tempdir().unwrap();
        let foo = write_package(dir.path(), "foo", "noarch", &["bar >=2"]);
        let baz = write_package(dir.path(), "baz", "linux-64", &[]);
        let mut records = vec![locked("bar"), locked("baz")];
        inject_packages(&mut records, &[foo, baz], Subdir::Linux64).unwrap();
        let names: Vec<_> = records.iter().map(|r| r.identifier.to_string()).collect();
        assert_eq!(
            names,
            ["bar-2.0-0.conda", "foo-1.0-0.tar.bz2", "baz-1.0-0.tar.bz2"]
        );
    }

    #[test]
    fn rejects_unsatisfied_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let foo = write_package(dir.path(), "foo", "noarch", &["bar >=3"]);
        let err = inject_packages(&mut vec![locked("bar")], &[foo], Subdir::Linux64).unwrap_err();
        assert!(format!("{err:#}").contains("bar >=3"), "{err:#}");
    }

    #[test]
    fn rejects_other_platforms() {
        let dir = tempfile::tempdir().unwrap();
        let foo = write_package(dir.path(), "foo", "linux-aarch64", &[]);
        let err = inject_packages(&mut Vec::new(), &[foo], Subdir::Linux64).unwrap_err();
        assert!(err.to_string().contains("built for linux-aarch64"), "{err}");
    }

    #[test]
    fn rejects_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_package(dir.path(), "foo", "noarch", &[]);
        let b = dir.path().join("foo-1.0-1.tar.bz2");
        std::fs::copy(&a, &b).unwrap();
        let err = inject_packages(&mut Vec::new(), &[a, b], Subdir::Linux64).unwrap_err();
        assert!(err.to_string().contains("same name"), "{err}");
    }
}
