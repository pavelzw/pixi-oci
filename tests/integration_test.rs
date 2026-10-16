//! End-to-end tests. They need network access (conda-forge and Docker Hub) like the builds
//! themselves. Docker and podman based tests run only when the respective tool is installed.

use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use pixi_oci::{BuildOptions, OutputFormat};
use rstest::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example")
}

#[fixture]
fn tmpdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// A loaded OCI layout tarball.
struct Archive {
    entries: HashMap<String, Vec<u8>>,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize, Clone)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: HashMap<String, String>,
    platform: Option<Platform>,
}

#[derive(Deserialize, Clone)]
struct Platform {
    architecture: String,
    os: String,
    variant: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ImageConfig {
    architecture: String,
    os: String,
    variant: Option<String>,
    config: InnerConfig,
    rootfs: RootFs,
    history: Vec<History>,
}

#[derive(Deserialize)]
struct InnerConfig {
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Env")]
    env: Vec<String>,
}

#[derive(Deserialize)]
struct RootFs {
    diff_ids: Vec<String>,
}

#[derive(Deserialize)]
struct History {
    created_by: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerManifest {
    config: String,
    repo_tags: Vec<String>,
    layers: Vec<String>,
}

impl Archive {
    fn open(path: &Path) -> Self {
        let file = fs::File::open(path).expect("archive exists");
        let mut tar = tar::Archive::new(file);
        let mut entries = HashMap::new();
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            entries.insert(name, data);
        }
        Self { entries }
    }

    fn blob(&self, digest: &str) -> &[u8] {
        let hex = digest.strip_prefix("sha256:").expect("sha256 digest");
        self.entries
            .get(&format!("blobs/sha256/{hex}"))
            .unwrap_or_else(|| panic!("blob {digest} missing"))
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, name: &str) -> T {
        serde_json::from_slice(&self.entries[name]).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn index(&self) -> Index {
        self.json("index.json")
    }

    fn manifest(&self) -> Manifest {
        let index = self.index();
        assert_eq!(index.manifests.len(), 1);
        serde_json::from_slice(self.blob(&index.manifests[0].digest)).unwrap()
    }

    fn config(&self) -> ImageConfig {
        serde_json::from_slice(self.blob(&self.manifest().config.digest)).unwrap()
    }

    /// Uncompressed tar entries (name -> (header, data)) of a layer.
    fn layer_entries(&self, layer: &Descriptor) -> Vec<(String, tar::Header, Vec<u8>)> {
        let raw = self.blob(&layer.digest);
        let data = if layer.media_type.ends_with("+gzip") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw)
                .read_to_end(&mut out)
                .unwrap();
            out
        } else {
            raw.to_vec()
        };
        let mut tar = tar::Archive::new(data.as_slice());
        tar.entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let name = e.path().unwrap().to_string_lossy().into_owned();
                let header = e.header().clone();
                let mut data = Vec::new();
                e.read_to_end(&mut data).unwrap();
                (name, header, data)
            })
            .collect()
    }

    /// Check every blob digest and every diff_id.
    fn verify_digests(&self) {
        let manifest = self.manifest();
        let config = self.config();
        assert_eq!(config.rootfs.diff_ids.len(), manifest.layers.len());
        for (layer, diff_id) in manifest.layers.iter().zip(&config.rootfs.diff_ids) {
            let raw = self.blob(&layer.digest);
            assert_eq!(raw.len() as u64, layer.size);
            assert_eq!(format!("sha256:{}", sha256_hex(raw)), layer.digest);
            let uncompressed = if layer.media_type.ends_with("+gzip") {
                let mut out = Vec::new();
                flate2::read::GzDecoder::new(raw)
                    .read_to_end(&mut out)
                    .unwrap();
                out
            } else {
                raw.to_vec()
            };
            assert_eq!(
                format!("sha256:{}", sha256_hex(&uncompressed)),
                *diff_id,
                "diff_id mismatch for {}",
                layer.digest
            );
        }
        assert!(self.entries.contains_key("oci-layout"));
    }
}

async fn build_example(
    output: &Path,
    platform: &str,
    base_image: Option<&str>,
    format: OutputFormat,
) -> pixi_oci::BuildReport {
    pixi_oci::build(BuildOptions {
        manifest_path: example_dir(),
        platform: Some(platform.to_string()),
        output: Some(output.to_path_buf()),
        format,
        base_image: base_image.map(str::to_string),
        ..Default::default()
    })
    .await
    .expect("build succeeds")
}

#[rstest]
#[tokio::test]
async fn builds_example_on_ubuntu(tmpdir: tempfile::TempDir) {
    let out = tmpdir.path().join("example.tar");
    let report = build_example(&out, "linux-64", Some("ubuntu"), OutputFormat::OciTar).await;
    assert_eq!(report.reference, "example-project:latest");
    assert_eq!(report.rootfs_layers, 0);
    // More packages than `max-layers = 20`, so the last package layer is shared.
    assert!(report.packages > 20);
    assert_eq!(report.package_layers, 20);
    assert!(report.copy_layer);
    assert!(report.base_layers >= 1);

    let archive = Archive::open(&out);
    archive.verify_digests();

    let index = archive.index();
    let desc = &index.manifests[0];
    assert_eq!(
        desc.annotations["org.opencontainers.image.ref.name"],
        "example-project:latest"
    );
    assert_eq!(
        desc.annotations["io.containerd.image.name"],
        "docker.io/library/example-project:latest"
    );
    let platform = desc.platform.clone().unwrap();
    assert_eq!(
        (platform.os.as_str(), platform.architecture.as_str()),
        ("linux", "amd64")
    );

    let manifest = archive.manifest();
    assert_eq!(
        manifest.layers.len(),
        report.base_layers + report.package_layers + 1
    );
    for layer in &manifest.layers {
        assert!(
            layer
                .media_type
                .starts_with("application/vnd.oci.image.layer.v1.tar"),
            "{}",
            layer.media_type
        );
    }

    let config = archive.config();
    assert_eq!(config.os, "linux");
    assert_eq!(config.architecture, "amd64");
    assert_eq!(
        config.config.entrypoint.as_deref(),
        Some(&["caddy", "file-server", "--browse", "--root", "."].map(String::from)[..])
    );
    assert!(config.config.env[0].starts_with("PATH=/opt/env/bin:"));
    assert!(
        config
            .config
            .env
            .contains(&"CONDA_PREFIX=/opt/env".to_string())
    );
    let last_history: Vec<_> = config
        .history
        .iter()
        .rev()
        .take(2)
        .filter_map(|h| h.created_by.clone())
        .collect();
    assert_eq!(last_history[0], "pixi-oci: copy-to-image");
    assert!(
        last_history[1].contains("packages (") && last_history[1].contains("caddy"),
        "{}",
        last_history[1]
    );

    // The shared last package layer carries caddy, conda-meta and no host artefacts.
    let package_layer = &manifest.layers[manifest.layers.len() - 2];
    let entries = archive.layer_entries(package_layer);
    let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"opt/env/bin/caddy"), "{names:?}");
    assert!(
        names.contains(&"opt/env/conda-meta/caddy-2.11.7-hfc2019e_0.json"),
        "{names:?}"
    );
    assert!(names.contains(&"opt/env/conda-meta/history"), "{names:?}");
    assert!(
        !names.iter().any(|n| n.contains("CACHEDIR.TAG")),
        "{names:?}"
    );
    for (name, header, _) in &entries {
        assert_eq!(header.uid().unwrap(), 0, "{name}");
        assert_eq!(header.gid().unwrap(), 0, "{name}");
        assert_eq!(header.mtime().unwrap(), 0, "{name}");
    }
    let caddy = entries
        .iter()
        .find(|(n, _, _)| n == "opt/env/bin/caddy")
        .unwrap();
    assert_eq!(
        caddy.1.mode().unwrap() & 0o111,
        0o111,
        "caddy must be executable"
    );

    // The copy layer has both files at the expected destinations.
    let copy_layer = manifest.layers.last().unwrap();
    let copy: HashMap<String, Vec<u8>> = archive
        .layer_entries(copy_layer)
        .into_iter()
        .map(|(n, _, d)| (n, d))
        .collect();
    assert_eq!(copy["my-file"], b"hello from my-file\n");
    assert_eq!(copy["my-file-renamed"], b"hello from my-file2\n");

    // docker-archive compatibility
    let docker: Vec<DockerManifest> = archive.json("manifest.json");
    assert_eq!(docker.len(), 1);
    assert_eq!(docker[0].repo_tags, vec!["example-project:latest"]);
    assert_eq!(docker[0].layers.len(), manifest.layers.len());
    assert!(archive.entries.contains_key(&docker[0].config));
    for layer in &docker[0].layers {
        assert!(archive.entries.contains_key(layer), "{layer}");
    }
}

#[rstest]
#[tokio::test]
async fn builds_glibc_rootfs_from_scratch(tmpdir: tempfile::TempDir) {
    // The example's `base` is a scratch table with glibc and the shadow setup.
    let out = tmpdir.path().join("example.tar");
    let report = build_example(&out, "linux-64", None, OutputFormat::OciTar).await;
    assert_eq!(report.base_layers, 0);
    assert_eq!(report.rootfs_layers, 2);

    let archive = Archive::open(&out);
    archive.verify_digests();
    let manifest = archive.manifest();
    assert_eq!(manifest.layers.len(), 2 + report.package_layers + 1);
    let history: Vec<_> = archive
        .config()
        .history
        .into_iter()
        .filter_map(|h| h.created_by)
        .collect();
    assert_eq!(history[0], "pixi-oci: glibc 2.41-12+deb13u4 (debian libc6)");
    assert_eq!(history[1], "pixi-oci: shadow setup");

    let glibc: HashMap<String, tar::Header> = archive
        .layer_entries(&manifest.layers[0])
        .into_iter()
        .map(|(n, h, _)| (n, h))
        .collect();
    assert!(glibc.contains_key("usr/lib/x86_64-linux-gnu/libc.so.6"));
    assert!(glibc.contains_key("usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"));
    assert!(glibc.contains_key("usr/share/doc/libc6/copyright"));
    assert!(glibc.contains_key("etc/nsswitch.conf"));
    assert!(!glibc.keys().any(|n| n.contains("/gconv/")));
    let lib64 = &glibc["lib64"];
    assert_eq!(lib64.entry_type(), tar::EntryType::Symlink);
    assert_eq!(
        lib64.link_name().unwrap().unwrap().to_str(),
        Some("usr/lib64")
    );

    let shadow: HashMap<String, (tar::Header, Vec<u8>)> = archive
        .layer_entries(&manifest.layers[1])
        .into_iter()
        .map(|(n, h, d)| (n, (h, d)))
        .collect();
    assert!(String::from_utf8_lossy(&shadow["etc/passwd"].1).starts_with("root:x:0:0:"));
    assert!(shadow.contains_key("etc/group"));
    assert_eq!(shadow["tmp/"].0.mode().unwrap(), 0o1777);
    assert_eq!(shadow["root/"].0.mode().unwrap(), 0o700);
}

#[rstest]
#[tokio::test]
async fn builds_from_scratch_for_aarch64_as_directory(tmpdir: tempfile::TempDir) {
    let out = tmpdir.path().join("layout");
    let report = build_example(&out, "linux-aarch64", Some("scratch"), OutputFormat::OciDir).await;
    assert_eq!(report.base_layers, 0);
    assert!(out.join("oci-layout").is_file());
    assert!(!out.join("manifest.json").exists());

    let index: Index = serde_json::from_slice(&fs::read(out.join("index.json")).unwrap()).unwrap();
    let desc = &index.manifests[0];
    let platform = desc.platform.clone().unwrap();
    assert_eq!(platform.architecture, "arm64");
    assert_eq!(platform.variant.as_deref(), Some("v8"));
    let blob = |digest: &str| {
        fs::read(
            out.join("blobs/sha256")
                .join(digest.strip_prefix("sha256:").unwrap()),
        )
        .unwrap()
    };
    let manifest: Manifest = serde_json::from_slice(&blob(&desc.digest)).unwrap();
    let layers = report.package_layers + 1;
    assert_eq!(manifest.layers.len(), layers);
    let config: ImageConfig = serde_json::from_slice(&blob(&manifest.config.digest)).unwrap();
    assert_eq!(config.architecture, "arm64");
    assert_eq!(config.variant.as_deref(), Some("v8"));
    assert_eq!(config.rootfs.diff_ids.len(), layers);
    assert_eq!(config.history.len(), layers);
    // No base image: PATH falls back to the default search path.
    assert_eq!(
        config.config.env[0],
        format!("PATH=/opt/env/bin:{}", pixi_oci::activation::DEFAULT_PATH)
    );
}

#[rstest]
fn cli_build_is_reproducible(tmpdir: tempfile::TempDir) {
    let mut digests = Vec::new();
    for i in 0..2 {
        let out = tmpdir.path().join(format!("build-{i}.tar"));
        let status = Command::new(env!("CARGO_BIN_EXE_pixi-oci"))
            .args(["build", "--manifest-path"])
            .arg(example_dir())
            .args([
                "--platform",
                "linux-64",
                "--base-image",
                "scratch",
                "--output",
            ])
            .arg(&out)
            .env("SOURCE_DATE_EPOCH", "0")
            .status()
            .expect("pixi-oci runs");
        assert!(status.success());
        digests.push(sha256_hex(&fs::read(&out).unwrap()));
    }
    assert_eq!(digests[0], digests[1], "builds must be reproducible");
}

#[rstest]
fn cli_rejects_non_linux_platform() {
    let output = Command::new(env!("CARGO_BIN_EXE_pixi-oci"))
        .args(["build", "--manifest-path"])
        .arg(example_dir())
        .args(["--platform", "osx-arm64", "--output", "-"])
        .output()
        .expect("pixi-oci runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a linux platform"), "{stderr}");
}

fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[rstest]
#[tokio::test]
async fn docker_can_load_and_run(tmpdir: tempfile::TempDir) {
    if !tool_available("docker") || std::env::var_os("PIXI_OCI_SKIP_DOCKER_TESTS").is_some() {
        eprintln!("docker not available, skipping");
        return;
    }
    let host_arch = Command::new("docker")
        .args(["info", "--format", "{{.Architecture}}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let (subdir, oci_platform) = match host_arch.as_str() {
        "aarch64" | "arm64" => ("linux-aarch64", "linux/arm64"),
        _ => ("linux-64", "linux/amd64"),
    };
    let out = tmpdir.path().join("example.tar");
    build_example(&out, subdir, None, OutputFormat::OciTar).await;
    let load = Command::new("docker")
        .args(["load", "-i"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        load.status.success(),
        "docker load failed: {}",
        String::from_utf8_lossy(&load.stderr)
    );
    let run = Command::new("docker")
        // The image's entrypoint is a long-running `caddy file-server`, so override it.
        .args([
            "run",
            "--rm",
            "--platform",
            oci_platform,
            "--entrypoint",
            "caddy",
        ])
        .args(["example-project:latest", "version"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(String::from_utf8_lossy(&run.stdout).contains("2.11.7"));

    // The Debian dynamic loader is reachable through the /lib64 (/lib) symlink and runs.
    let loader = match subdir {
        "linux-aarch64" => "/lib/ld-linux-aarch64.so.1",
        _ => "/lib64/ld-linux-x86-64.so.2",
    };
    let run = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            oci_platform,
            "--entrypoint",
            loader,
        ])
        .args(["example-project:latest", "--version"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "running the loader failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(String::from_utf8_lossy(&run.stdout).contains("2.41"));
}

#[rstest]
#[tokio::test]
async fn skopeo_can_inspect(tmpdir: tempfile::TempDir) {
    if !tool_available("skopeo") {
        eprintln!("skopeo not available, skipping");
        return;
    }
    let out = tmpdir.path().join("example.tar");
    build_example(&out, "linux-64", Some("scratch"), OutputFormat::OciTar).await;
    let inspect = Command::new("skopeo")
        .arg("inspect")
        .arg(format!("oci-archive:{}", out.display()))
        .output()
        .unwrap();
    assert!(
        inspect.status.success(),
        "skopeo inspect failed: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(json["Architecture"], "amd64");
    assert_eq!(json["Os"], "linux");
}
