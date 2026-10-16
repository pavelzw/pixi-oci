//! A minimal root filesystem for images built without a base image: glibc taken from Debian's
//! `libc6` package and the user database files a shadow setup provides.

use std::{
    borrow::Cow,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, anyhow, bail};
use rattler_conda_types::{MatchSpec, ParseStrictness, RepoDataRecord, Subdir, Version};
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use walkdir::WalkDir;

use crate::{
    config::ScratchBase,
    layers::{FileEntry, PlannedLayer},
};

/// Debian's snapshot archive. It keeps every package version ever published, so pinned
/// versions stay downloadable (unlike the regular pool, which drops superseded versions).
const SNAPSHOT_URL: &str = "https://snapshot.debian.org";

const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin
";
const SHADOW: &str = "\
root:!x:::::::
nobody:!x:::::::
";
const GROUP: &str = "\
root:x:0:
nobody:x:65534:
";
const GSHADOW: &str = "\
root:x::
nobody:x::
";
const NSSWITCH: &str = "\
passwd: files
group: files
shadow: files
hosts: files dns
";

/// The Debian architecture name of a conda subdir.
fn debian_arch(subdir: Subdir) -> Result<&'static str> {
    Ok(match subdir {
        Subdir::Linux64 => "amd64",
        Subdir::LinuxAarch64 => "arm64",
        Subdir::LinuxArmV7l => "armhf",
        Subdir::LinuxPpc64le => "ppc64el",
        Subdir::LinuxS390X => "s390x",
        Subdir::LinuxRiscv64 => "riscv64",
        Subdir::LinuxLoongArch64 => "loong64",
        other => bail!(
            "no Debian architecture for conda platform {}",
            other.as_str()
        ),
    })
}

/// The upstream glibc version of a Debian package version (`2.41-12+deb13u4` -> `2.41`).
pub fn upstream_glibc_version(debian_version: &str) -> Result<Version> {
    let without_epoch = debian_version
        .split_once(':')
        .map_or(debian_version, |(_, v)| v);
    let upstream = without_epoch
        .rsplit_once('-')
        .map_or(without_epoch, |(u, _)| u);
    Version::from_str(upstream)
        .map_err(|e| anyhow!("could not parse glibc version `{debian_version}`: {e}"))
}

/// Fail if a package needs a newer glibc than `glibc`; warn if packages need glibc but the
/// image has none.
pub fn check_glibc(records: &[RepoDataRecord], glibc: Option<&Version>) -> Result<()> {
    for record in records {
        let pkg = &record.package_record;
        for dep in &pkg.depends {
            let spec = MatchSpec::from_str(dep, ParseStrictness::Lenient).with_context(|| {
                format!(
                    "could not parse dependency `{dep}` of {}",
                    pkg.name.as_normalized()
                )
            })?;
            if spec
                .name
                .as_exact()
                .is_none_or(|n| n.as_normalized() != "__glibc")
            {
                continue;
            }
            match glibc {
                None => {
                    tracing::warn!(
                        "{} requires `{dep}` but the image has no glibc; set `glibc` in `base`",
                        pkg.name.as_normalized()
                    );
                    return Ok(());
                }
                Some(version) => {
                    if !spec.version.as_ref().is_none_or(|v| v.matches(version)) {
                        bail!(
                            "{} requires `{dep}`, but the image gets glibc {version}",
                            pkg.name.as_normalized()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Plan the layers of a from-scratch base: glibc (if enabled) and the shadow setup (if enabled).
/// Files are staged below `staging`.
pub async fn plan_rootfs_layers(
    base: &ScratchBase,
    subdir: Subdir,
    client: &ClientWithMiddleware,
    cache_dir: &Path,
    staging: &Path,
) -> Result<Vec<PlannedLayer>> {
    let mut layers = Vec::new();
    if let Some(version) = base.glibc.debian_version() {
        let arch = debian_arch(subdir)?;
        let deb: Cow<'static, [u8]> = match bundled_deb("libc6", version, arch) {
            Some(deb) => Cow::Borrowed(deb),
            None => Cow::Owned(fs::read(
                fetch_deb(client, cache_dir, "libc6", version, arch).await?,
            )?),
        };
        let root = staging.join("glibc");
        extract_deb(&deb, &root, keep_glibc_file)
            .with_context(|| format!("could not extract libc6 {version} ({arch})"))?;
        // Debian gets these from `base-files` (merged /usr). Binaries look up the dynamic
        // loader through them, e.g. /lib64/ld-linux-x86-64.so.2.
        symlink("usr/lib", root.join("lib"))?;
        if root.join("usr/lib64").is_dir() {
            symlink("usr/lib64", root.join("lib64"))?;
        }
        fs::write(root.join("etc/nsswitch.conf"), NSSWITCH)?;
        layers.push(PlannedLayer {
            description: format!("pixi-oci: glibc {version} (debian libc6)"),
            files: tree_entries(&root, &[])?,
        });
    }
    if base.shadow_setup {
        let root = staging.join("shadow");
        fs::create_dir_all(root.join("etc"))?;
        for (name, contents) in [
            ("passwd", PASSWD),
            ("shadow", SHADOW),
            ("group", GROUP),
            ("gshadow", GSHADOW),
        ] {
            fs::write(root.join("etc").join(name), contents)?;
        }
        for (dir, mode) in [("root", 0o700), ("tmp", 0o1777)] {
            fs::create_dir(root.join(dir))?;
            fs::set_permissions(root.join(dir), fs::Permissions::from_mode(mode))?;
        }
        layers.push(PlannedLayer {
            description: "pixi-oci: shadow setup".to_string(),
            files: tree_entries(&root, &["root", "tmp"])?,
        });
    }
    Ok(layers)
}

/// Everything below `root` that is not a directory, plus the directories in `dirs` (which keep
/// their permissions in the layer).
fn tree_entries(root: &Path, dirs: &[&str]) -> Result<Vec<FileEntry>> {
    let mut files: Vec<FileEntry> = dirs
        .iter()
        .map(|dir| FileEntry {
            host_path: root.join(dir),
            image_path: format!("/{dir}"),
        })
        .collect();
    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        let rel = entry.path().strip_prefix(root)?;
        files.push(FileEntry {
            host_path: entry.path().to_path_buf(),
            image_path: format!("/{}", rel.to_string_lossy()),
        });
    }
    Ok(files)
}

/// Drop documentation (except the copyright file), lintian overrides and the iconv `gconv`
/// modules (8 of the 13 MB; UTF-8, ASCII and Latin-1 are built into glibc).
fn keep_glibc_file(path: &Path) -> bool {
    let path = path.to_string_lossy();
    if path.starts_with("usr/share/doc/") {
        return path.ends_with("/copyright");
    }
    !path.starts_with("usr/share/lintian/") && !path.contains("/gconv/")
}

/// Debian packages embedded with the `bundled-glibc` feature (see `build.rs`).
#[cfg(feature = "bundled-glibc")]
mod bundled {
    /// A `.deb` together with the fields of its `control` file, read by `build.rs`.
    pub struct BundledDeb {
        pub package: &'static str,
        pub version: &'static str,
        pub architecture: &'static str,
        pub data: &'static [u8],
    }

    include!(concat!(env!("OUT_DIR"), "/bundled_glibc.rs"));
}

/// The bundled `.deb` for `package` `version` on `arch`, if there is one.
#[cfg(feature = "bundled-glibc")]
fn bundled_deb(package: &str, version: &str, arch: &str) -> Option<&'static [u8]> {
    bundled::DEBS
        .iter()
        .find(|deb| deb.package == package && deb.version == version && deb.architecture == arch)
        .map(|deb| deb.data)
}

#[cfg(not(feature = "bundled-glibc"))]
fn bundled_deb(_package: &str, _version: &str, _arch: &str) -> Option<&'static [u8]> {
    None
}

#[derive(Deserialize)]
struct BinFiles {
    result: Vec<BinFile>,
}

#[derive(Deserialize)]
struct BinFile {
    architecture: String,
    hash: String,
}

/// Download `<package>_<version>_<arch>.deb` from snapshot.debian.org into the cache, verified
/// against the SHA-1 the archive addresses it by. Returns the cached path.
async fn fetch_deb(
    client: &ClientWithMiddleware,
    cache_dir: &Path,
    package: &str,
    version: &str,
    arch: &str,
) -> Result<PathBuf> {
    let dir = cache_dir.join("pixi-oci").join("debian");
    let path = dir.join(format!(
        "{package}_{}_{arch}.deb",
        version.replace(':', "%3a")
    ));
    if path.is_file() {
        return Ok(path);
    }
    eprintln!("⬇️  Downloading {package} {version} ({arch}) from snapshot.debian.org");

    let url = format!("{SNAPSHOT_URL}/mr/binary/{package}/{version}/binfiles");
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("could not query {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("Debian has no {package} version `{version}` (see {SNAPSHOT_URL}/binary/{package}/)");
    }
    let files: BinFiles = response
        .error_for_status()?
        .json()
        .await
        .with_context(|| format!("could not parse {url}"))?;
    let hash = files
        .result
        .into_iter()
        .find(|f| f.architecture == arch)
        .map(|f| f.hash)
        .ok_or_else(|| anyhow!("{package} {version} is not available for {arch}"))?;

    let data = client
        .get(format!("{SNAPSHOT_URL}/file/{hash}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await
        .with_context(|| format!("could not download {package} {version}"))?;
    let actual: String = Sha1::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != hash {
        bail!("checksum mismatch for {package} {version}: expected {hash}, got {actual}");
    }

    fs::create_dir_all(&dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    std::io::Write::write_all(&mut tmp, &data)?;
    tmp.persist(&path)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// Unpack the `data.tar.xz` of a `.deb` into `dest`, keeping only the paths `keep` accepts.
/// Directories are created as needed rather than taken from the archive.
fn extract_deb(deb: &[u8], dest: &Path, keep: impl Fn(&Path) -> bool) -> Result<()> {
    let data = ar_member(deb, "data.tar.xz")?;
    fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(liblzma::read::XzDecoder::new(data));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let path: PathBuf = entry
            .path()?
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        if keep(&path) {
            // `unpack_in` refuses paths that would escape `dest`.
            entry.unpack_in(dest)?;
        }
    }
    Ok(())
}

/// The contents of the member `name` of a Unix `ar` archive, the container format of `.deb`
/// files.
fn ar_member<'a>(data: &'a [u8], name: &str) -> Result<&'a [u8]> {
    let mut rest = data
        .strip_prefix(b"!<arch>\n")
        .ok_or_else(|| anyhow!("not an ar archive"))?;
    while !rest.is_empty() {
        if rest.len() < 60 {
            bail!("truncated ar header");
        }
        let (header, body) = rest.split_at(60);
        let member = std::str::from_utf8(&header[..16])?
            .trim_end()
            .trim_end_matches('/');
        let size: usize = std::str::from_utf8(&header[48..58])?
            .trim()
            .parse()
            .context("invalid ar member size")?;
        if body.len() < size {
            bail!("truncated ar member `{member}`");
        }
        if member == name {
            return Ok(&body[..size]);
        }
        // Members are padded to an even size.
        rest = &body[(size + size % 2).min(body.len())..];
    }
    bail!("no {name} in the package")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "bundled-glibc")]
    #[test]
    fn bundles_the_default_glibc() {
        assert!(!bundled::DEBS.is_empty());
        // Without an override, build.rs bundles the default version.
        if option_env!("GLIBC_DEBIAN_FILE").is_none() {
            let version = crate::config::DEFAULT_GLIBC_DEBIAN_VERSION;
            for arch in ["amd64", "arm64"] {
                assert!(
                    bundled_deb("libc6", version, arch).is_some(),
                    "libc6 {version} ({arch}) is not bundled"
                );
            }
        }
    }

    #[test]
    fn upstream_versions() {
        assert_eq!(
            upstream_glibc_version("2.41-12+deb13u4").unwrap(),
            Version::from_str("2.41").unwrap()
        );
        assert_eq!(
            upstream_glibc_version("1:2.28-10").unwrap(),
            Version::from_str("2.28").unwrap()
        );
    }

    #[test]
    fn filters_glibc_files() {
        assert!(keep_glibc_file(Path::new(
            "usr/lib/x86_64-linux-gnu/libc.so.6"
        )));
        assert!(keep_glibc_file(Path::new("usr/share/doc/libc6/copyright")));
        assert!(!keep_glibc_file(Path::new("usr/share/doc/libc6/NEWS.gz")));
        assert!(!keep_glibc_file(Path::new(
            "usr/lib/x86_64-linux-gnu/gconv/UTF-16.so"
        )));
    }

    #[test]
    fn parses_ar_archives() {
        let mut ar = b"!<arch>\n".to_vec();
        for (name, body) in [("debian-binary", &b"2.0\n"[..]), ("odd", &b"abc"[..])] {
            ar.extend(
                format!(
                    "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    name,
                    0,
                    0,
                    0,
                    100644,
                    body.len()
                )
                .bytes(),
            );
            ar.extend(body);
            if body.len() % 2 == 1 {
                ar.push(b'\n');
            }
        }
        assert_eq!(ar_member(&ar, "debian-binary").unwrap(), b"2.0\n");
        assert_eq!(ar_member(&ar, "odd").unwrap(), b"abc");
        assert!(ar_member(&ar, "missing").is_err());
    }
}
