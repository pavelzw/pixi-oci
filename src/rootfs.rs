//! A minimal root filesystem for images built without a base image: glibc taken from Debian's
//! `libc6` package and the user database files a shadow setup provides.

use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read,
    path::{Component, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, anyhow, bail};
use rattler_conda_types::{MatchSpec, ParseStrictness, RepoDataRecord, Subdir, Version};
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use sha1::{Digest, Sha1};

use crate::{
    config::{ScratchBase, relative_image_path},
    layers::{LayerEntry, PlannedLayer},
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
/// The files are kept in memory, nothing is written to the build host.
pub async fn plan_rootfs_layers(
    base: &ScratchBase,
    subdir: Subdir,
    client: &ClientWithMiddleware,
) -> Result<Vec<PlannedLayer>> {
    let mut layers = Vec::new();
    if let Some(version) = base.glibc.debian_version() {
        let arch = debian_arch(subdir)?;
        let deb: Cow<'static, [u8]> = match bundled_deb("libc6", version, arch) {
            Some(deb) => Cow::Borrowed(deb),
            None => Cow::Owned(fetch_deb(client, "libc6", version, arch).await?),
        };
        let mut entries = deb_entries(&deb, keep_glibc_file)
            .with_context(|| format!("could not extract libc6 {version} ({arch})"))?;
        // Debian gets these from `base-files` (merged /usr). Binaries look up the dynamic
        // loader through them, e.g. /lib64/ld-linux-x86-64.so.2.
        let has_lib64 = entries
            .iter()
            .any(|entry| entry.image_path().starts_with("/usr/lib64/"));
        entries.push(LayerEntry::Symlink {
            image_path: "/lib".to_string(),
            target: "usr/lib".to_string(),
        });
        if has_lib64 {
            entries.push(LayerEntry::Symlink {
                image_path: "/lib64".to_string(),
                target: "usr/lib64".to_string(),
            });
        }
        entries.retain(|entry| entry.image_path() != "/etc/nsswitch.conf");
        entries.push(LayerEntry::File {
            image_path: "/etc/nsswitch.conf".to_string(),
            mode: 0o644,
            contents: NSSWITCH.as_bytes().to_vec(),
        });
        layers.push(PlannedLayer::with_entries(
            format!("pixi-oci: glibc {version} (debian libc6)"),
            entries,
        ));
    }
    if base.shadow_setup {
        let mut entries: Vec<LayerEntry> = [
            ("passwd", PASSWD),
            ("shadow", SHADOW),
            ("group", GROUP),
            ("gshadow", GSHADOW),
        ]
        .into_iter()
        .map(|(name, contents)| LayerEntry::File {
            image_path: format!("/etc/{name}"),
            mode: 0o644,
            contents: contents.as_bytes().to_vec(),
        })
        .collect();
        for (dir, mode) in [("root", 0o700), ("tmp", 0o1777)] {
            entries.push(LayerEntry::Dir {
                image_path: format!("/{dir}"),
                mode,
            });
        }
        layers.push(PlannedLayer::with_entries(
            "pixi-oci: shadow setup",
            entries,
        ));
    }
    Ok(layers)
}

/// Drop documentation (except the copyright file), lintian overrides and the iconv `gconv`
/// modules (8 of the 13 MB; UTF-8, ASCII and Latin-1 are built into glibc).
fn keep_glibc_file(path: &str) -> bool {
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

/// Download `<package>_<version>_<arch>.deb` from snapshot.debian.org, verified against the
/// SHA-1 the archive addresses it by.
async fn fetch_deb(
    client: &ClientWithMiddleware,
    package: &str,
    version: &str,
    arch: &str,
) -> Result<Vec<u8>> {
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
    Ok(data.to_vec())
}

/// The files and symlinks in the `data.tar.xz` of a `.deb` that `keep` accepts. Directories
/// are created as needed rather than taken from the archive, hard links become copies.
fn deb_entries(deb: &[u8], keep: impl Fn(&str) -> bool) -> Result<Vec<LayerEntry>> {
    let data = ar_member(deb, "data.tar.xz")?;
    let mut archive = tar::Archive::new(liblzma::read::XzDecoder::new(data));
    let mut entries = Vec::new();
    // Index into `entries` by the path in the archive, to resolve hard links.
    let mut files: HashMap<PathBuf, usize> = HashMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            continue;
        }
        // Only normal components, so the path cannot leave the image root.
        let path: PathBuf = entry
            .path()?
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        let relative = relative_image_path(&path);
        if !keep(&relative) {
            continue;
        }
        let image_path = format!("/{relative}");
        let mode = entry.header().mode()?;
        match entry_type {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mut contents = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut contents)?;
                files.insert(path, entries.len());
                entries.push(LayerEntry::File {
                    image_path,
                    mode,
                    contents,
                });
            }
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()?
                    .ok_or_else(|| anyhow!("symlink {} has no target", path.display()))?;
                entries.push(LayerEntry::Symlink {
                    image_path,
                    target: target.to_string_lossy().into_owned(),
                });
            }
            tar::EntryType::Link => {
                let target: PathBuf = entry
                    .link_name()?
                    .ok_or_else(|| anyhow!("hard link {} has no target", path.display()))?
                    .components()
                    .filter(|c| matches!(c, Component::Normal(_)))
                    .collect();
                let Some(LayerEntry::File { mode, contents, .. }) =
                    files.get(&target).map(|&i| &entries[i])
                else {
                    bail!(
                        "hard link {} points to {}, which is not a file before it",
                        path.display(),
                        target.display()
                    );
                };
                let entry = LayerEntry::File {
                    image_path,
                    mode: *mode,
                    contents: contents.clone(),
                };
                files.insert(path, entries.len());
                entries.push(entry);
            }
            other => bail!("unsupported entry type {other:?} for {}", path.display()),
        }
    }
    Ok(entries)
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
        assert!(keep_glibc_file("usr/lib/x86_64-linux-gnu/libc.so.6"));
        assert!(keep_glibc_file("usr/share/doc/libc6/copyright"));
        assert!(!keep_glibc_file("usr/share/doc/libc6/NEWS.gz"));
        assert!(!keep_glibc_file("usr/lib/x86_64-linux-gnu/gconv/UTF-16.so"));
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
