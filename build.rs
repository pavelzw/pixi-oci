//! With the `bundled-glibc` feature, embed Debian `libc6` packages into the binary.
//!
//! The packages of the default glibc version are downloaded from snapshot.debian.org and
//! verified. Set `GLIBC_DEBIAN_FILE` to one or more local `.deb` files (separated like `PATH`)
//! to bundle those instead and skip the download, e.g. for offline builds.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GLIBC_DEBIAN_FILE");
    #[cfg(feature = "bundled-glibc")]
    bundled_glibc::generate();
}

#[cfg(feature = "bundled-glibc")]
mod bundled_glibc {
    use std::{
        env, fs,
        io::Read,
        path::{Path, PathBuf},
    };

    use sha1::{Digest, Sha1};

    /// Must match `DEFAULT_GLIBC_DEBIAN_VERSION` in `src/config.rs`.
    const VERSION: &str = "2.41-12+deb13u4";
    /// (Debian architecture, SHA-1 the snapshot archive addresses the package by)
    const PACKAGES: &[(&str, &str)] = &[
        ("amd64", "27904cc1b58dd79f0768ba6aa6dd993edc411b97"),
        ("arm64", "6ee536e6a461ca319776fac983202946f4d3dce8"),
    ];

    pub fn generate() {
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
        let debs: Vec<PathBuf> = match env::var_os("GLIBC_DEBIAN_FILE") {
            Some(files) => env::split_paths(&files)
                .map(|path| {
                    let path = fs::canonicalize(&path).unwrap_or_else(|e| {
                        panic!("GLIBC_DEBIAN_FILE: cannot open {}: {e}", path.display())
                    });
                    println!("cargo:rerun-if-changed={}", path.display());
                    path
                })
                .collect(),
            None => PACKAGES
                .iter()
                .map(|(arch, sha1)| download(&out_dir, arch, sha1))
                .collect(),
        };

        let entries: String = debs
            .iter()
            .map(|path| {
                let control = Control::read(path)
                    .unwrap_or_else(|e| panic!("invalid Debian package {}: {e}", path.display()));
                format!(
                    "    BundledDeb {{ package: {:?}, version: {:?}, architecture: {:?}, \
                     data: include_bytes!({:?}) }},\n",
                    control.package,
                    control.version,
                    control.architecture,
                    path.display().to_string()
                )
            })
            .collect();
        fs::write(
            out_dir.join("bundled_glibc.rs"),
            format!("pub static DEBS: &[BundledDeb] = &[\n{entries}];\n"),
        )
        .expect("could not write bundled_glibc.rs");
    }

    /// The identifying fields of a `.deb`'s `control` file.
    struct Control {
        package: String,
        version: String,
        architecture: String,
    }

    impl Control {
        fn read(deb: &Path) -> Result<Self, String> {
            let data = fs::read(deb).map_err(|e| e.to_string())?;
            let control_tar = ar_member(&data, "control.tar.xz")?;
            let mut archive = tar::Archive::new(liblzma::read::XzDecoder::new(control_tar));
            let mut text = String::new();
            for entry in archive.entries().map_err(|e| e.to_string())? {
                let mut entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path().map_err(|e| e.to_string())?;
                if path.file_name().is_some_and(|f| f == "control") {
                    entry.read_to_string(&mut text).map_err(|e| e.to_string())?;
                    break;
                }
            }
            let field = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                    .map(|value| value.trim().to_string())
                    .ok_or_else(|| format!("the control file has no `{name}` field"))
            };
            Ok(Self {
                package: field("Package")?,
                version: field("Version")?,
                architecture: field("Architecture")?,
            })
        }
    }

    /// The contents of the member `name` of a Unix `ar` archive (the `.deb` container).
    fn ar_member<'a>(data: &'a [u8], name: &str) -> Result<&'a [u8], String> {
        let mut rest = data.strip_prefix(b"!<arch>\n").ok_or("not an ar archive")?;
        while rest.len() >= 60 {
            let (header, body) = rest.split_at(60);
            let member = String::from_utf8_lossy(&header[..16]);
            let size: usize = String::from_utf8_lossy(&header[48..58])
                .trim()
                .parse()
                .map_err(|_| "invalid ar member size")?;
            if body.len() < size {
                return Err("truncated ar archive".into());
            }
            if member.trim_end().trim_end_matches('/') == name {
                return Ok(&body[..size]);
            }
            rest = &body[(size + size % 2).min(body.len())..];
        }
        Err(format!("no {name} in the package"))
    }

    fn download(out_dir: &Path, arch: &str, sha1: &str) -> PathBuf {
        let path = out_dir.join(format!("libc6_{VERSION}_{arch}.deb"));
        if fs::read(&path).is_ok_and(|data| hex_sha1(&data) == sha1) {
            return path;
        }
        let url = format!("https://snapshot.debian.org/file/{sha1}");
        let data = ureq::get(&url)
            .call()
            .and_then(|mut response| {
                response
                    .body_mut()
                    .with_config()
                    .limit(64 * 1024 * 1024)
                    .read_to_vec()
            })
            .unwrap_or_else(|e| {
                panic!(
                    "could not download libc6 {VERSION} ({arch}) from {url}: {e}\n\
                     Set GLIBC_DEBIAN_FILE to a local .deb to skip the download."
                )
            });
        let actual = hex_sha1(&data);
        assert_eq!(
            actual, sha1,
            "checksum mismatch for libc6 {VERSION} ({arch})"
        );
        fs::write(&path, data).expect("could not write the downloaded package");
        path
    }

    fn hex_sha1(data: &[u8]) -> String {
        Sha1::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
