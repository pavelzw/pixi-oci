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
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
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

/// A copy of the example project in `dir` whose `base` is the registry image `base_image`
/// (or `scratch`). Returns the project directory.
fn example_with_base(dir: &Path, base_image: &str) -> PathBuf {
    let project = dir.join("project");
    fs::create_dir_all(&project).unwrap();
    for file in ["pixi.lock", "my-file", "my-file2"] {
        fs::copy(example_dir().join(file), project.join(file)).unwrap();
    }
    let manifest = fs::read_to_string(example_dir().join("pixi.toml")).unwrap();
    let base_line = manifest
        .lines()
        .find(|l| l.starts_with("base = "))
        .expect("example sets `base`");
    fs::write(
        project.join("pixi.toml"),
        manifest.replace(base_line, &format!("base = \"{base_image}\"")),
    )
    .unwrap();
    project
}

/// Build the example. `base_image` replaces its `base`; the copy goes next to `output`.
async fn build_example(
    output: &Path,
    platform: &str,
    base_image: Option<&str>,
    format: OutputFormat,
) -> pixi_oci::BuildReport {
    let manifest_path = match base_image {
        Some(base) => example_with_base(output.parent().unwrap(), base),
        None => example_dir(),
    };
    pixi_oci::build(BuildOptions {
        manifest_path,
        platforms: vec![platform.to_string()],
        output: Some(output.to_path_buf()),
        format,
        ..Default::default()
    })
    .await
    .expect("build succeeds")
    .remove(0)
}

#[rstest]
#[tokio::test]
async fn builds_example_on_ubuntu(tmpdir: tempfile::TempDir) {
    let out = tmpdir.path().join("example.tar");
    let report = build_example(&out, "linux-64", Some("ubuntu"), OutputFormat::OciTar).await;
    assert_eq!(report.reference, "example-project:latest");
    assert_eq!(report.images[0].rootfs_layers, 0);
    // More packages than `max-layers = 20`, so the last package layer is shared.
    assert!(report.images[0].packages > 20);
    assert_eq!(report.images[0].package_layers, 20);
    assert!(report.images[0].copy_layer);
    assert!(report.images[0].base_layers >= 1);

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
        report.images[0].base_layers + report.images[0].package_layers + 1
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
        last_history[1].starts_with("pixi-oci: ") && last_history[1].ends_with(" packages"),
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
    assert_eq!(report.images[0].base_layers, 0);
    assert_eq!(report.images[0].rootfs_layers, 2);

    let archive = Archive::open(&out);
    archive.verify_digests();
    let manifest = archive.manifest();
    assert_eq!(
        manifest.layers.len(),
        2 + report.images[0].package_layers + 1
    );
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
    assert_eq!(report.images[0].base_layers, 0);
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
    let layers = report.images[0].package_layers + 1;
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

    // Rebuilding into the same directory drops the blobs of the earlier build.
    let report = build_example(&out, "linux-64", Some("scratch"), OutputFormat::OciDir).await;
    let index: Index = serde_json::from_slice(&fs::read(out.join("index.json")).unwrap()).unwrap();
    let manifest: Manifest = serde_json::from_slice(&blob(&index.manifests[0].digest)).unwrap();
    assert_eq!(manifest.layers.len(), report.images[0].package_layers + 1);
    let blobs = fs::read_dir(out.join("blobs/sha256")).unwrap().count();
    assert_eq!(blobs, manifest.layers.len() + 2);
}

/// Write a noarch conda package `<name>-1.0-0.tar.bz2` with the file `share/<name>.txt`.
fn write_conda_package(dir: &Path, name: &str, depends: &[&str]) -> PathBuf {
    let contents = dir.join(format!("{name}-contents"));
    fs::create_dir_all(contents.join("info")).unwrap();
    fs::create_dir_all(contents.join("share")).unwrap();
    let file = format!("share/{name}.txt");
    fs::write(contents.join(&file), format!("hello from {name}\n")).unwrap();
    let index = serde_json::json!({
        "name": name, "version": "1.0", "build": "0", "build_number": 0,
        "subdir": "noarch", "noarch": "generic", "depends": depends,
    });
    fs::write(contents.join("info/index.json"), index.to_string()).unwrap();
    let paths = serde_json::json!({
        "paths_version": 1,
        "paths": [{ "_path": file, "path_type": "hardlink" }],
    });
    fs::write(contents.join("info/paths.json"), paths.to_string()).unwrap();
    let archive = dir.join(format!("{name}-1.0-0.tar.bz2"));
    rattler_package_streaming::write::write_tar_bz2_package(
        fs::File::create(&archive).unwrap(),
        &contents,
        &["info/index.json", "info/paths.json", &file].map(|p| contents.join(p)),
        rattler_conda_types::compression_level::CompressionLevel::Default,
        None,
        None,
    )
    .unwrap();
    archive
}

#[rstest]
#[tokio::test]
async fn injects_local_packages(tmpdir: tempfile::TempDir) {
    let package = write_conda_package(tmpdir.path(), "injected", &["caddy"]);
    let out = tmpdir.path().join("example.tar");
    let report = pixi_oci::build(BuildOptions {
        manifest_path: example_with_base(tmpdir.path(), "scratch"),
        platforms: vec!["linux-64".to_string()],
        output: Some(out.clone()),
        inject: vec![package.into()],
        ..Default::default()
    })
    .await
    .expect("build succeeds")
    .remove(0);

    let archive = Archive::open(&out);
    archive.verify_digests();
    let manifest = archive.manifest();
    let files: HashMap<String, Vec<u8>> = manifest.layers[report.images[0].rootfs_layers..]
        [..report.images[0].package_layers]
        .iter()
        .flat_map(|layer| archive.layer_entries(layer))
        .map(|(name, _, data)| (name, data))
        .collect();
    assert_eq!(
        files["opt/env/share/injected.txt"],
        b"hello from injected\n"
    );
    assert!(files.contains_key("opt/env/conda-meta/injected-1.0-0.json"));
    assert!(files.contains_key("opt/env/bin/caddy"));

    // Dependencies of injected packages must be part of the environment.
    let incompatible = write_conda_package(tmpdir.path(), "needs-new-caddy", &["caddy >=99"]);
    let err = pixi_oci::build(BuildOptions {
        manifest_path: example_with_base(tmpdir.path(), "scratch"),
        platforms: vec!["linux-64".to_string()],
        output: Some(tmpdir.path().join("other.tar")),
        inject: vec![incompatible.into()],
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("caddy >=99"), "{err:#}");
}

#[rstest]
#[tokio::test]
async fn builds_multi_platform_index(tmpdir: tempfile::TempDir) {
    let everywhere = write_conda_package(tmpdir.path(), "everywhere", &[]);
    let arm_only = write_conda_package(tmpdir.path(), "arm-only", &[]);
    let out = tmpdir.path().join("example.tar");
    let report = pixi_oci::build(BuildOptions {
        manifest_path: example_with_base(tmpdir.path(), "scratch"),
        platforms: vec!["linux-64".to_string(), "linux-aarch64".to_string()],
        output: Some(out.clone()),
        inject: vec![
            everywhere.into(),
            format!("linux-aarch64:{}", arm_only.display())
                .parse()
                .unwrap(),
        ],
        ..Default::default()
    })
    .await
    .expect("build succeeds")
    .remove(0);
    assert_eq!(report.images.len(), 2);

    let archive = Archive::open(&out);
    // docker-archive can't describe multi-platform images.
    assert!(!archive.entries.contains_key("manifest.json"));

    // index.json -> tagged image index -> one manifest per platform.
    let index = archive.index();
    assert_eq!(index.manifests.len(), 1);
    let entry = &index.manifests[0];
    assert_eq!(entry.media_type, "application/vnd.oci.image.index.v1+json");
    assert_eq!(Some(&entry.digest), report.index_digest.as_ref());
    assert_eq!(
        entry.annotations["org.opencontainers.image.ref.name"],
        "example-project:latest"
    );
    let nested: Index = serde_json::from_slice(archive.blob(&entry.digest)).unwrap();
    let platforms: Vec<_> = nested
        .manifests
        .iter()
        .map(|m| {
            let p = m.platform.clone().unwrap();
            (p.os, p.architecture, p.variant)
        })
        .collect();
    assert_eq!(
        platforms,
        [
            ("linux".to_string(), "amd64".to_string(), None),
            (
                "linux".to_string(),
                "arm64".to_string(),
                Some("v8".to_string())
            ),
        ]
    );

    let manifests: Vec<Manifest> = nested
        .manifests
        .iter()
        .map(|m| serde_json::from_slice(archive.blob(&m.digest)).unwrap())
        .collect();
    let mut files = Vec::new();
    for ((manifest, image), arch) in manifests.iter().zip(&report.images).zip(["amd64", "arm64"]) {
        let config: ImageConfig =
            serde_json::from_slice(archive.blob(&manifest.config.digest)).unwrap();
        assert_eq!(config.architecture, arch);
        assert_eq!(config.rootfs.diff_ids.len(), manifest.layers.len());
        files.push(
            manifest.layers[..image.package_layers]
                .iter()
                .flat_map(|layer| archive.layer_entries(layer))
                .map(|(name, _, _)| name)
                .collect::<Vec<_>>(),
        );
    }
    let has = |files: &[String], name: &str| files.iter().any(|f| f == name);
    assert!(has(&files[0], "opt/env/share/everywhere.txt"));
    assert!(has(&files[1], "opt/env/share/everywhere.txt"));
    assert!(!has(&files[0], "opt/env/share/arm-only.txt"));
    assert!(has(&files[1], "opt/env/share/arm-only.txt"));
    // The copy layer is the same on every platform and stored once.
    assert_eq!(
        manifests[0].layers.last().unwrap().digest,
        manifests[1].layers.last().unwrap().digest
    );

    // A platform-specific injection must name a platform of the build.
    let err = pixi_oci::build(BuildOptions {
        manifest_path: example_with_base(tmpdir.path(), "scratch"),
        platforms: vec!["linux-64".to_string()],
        output: Some(tmpdir.path().join("other.tar")),
        inject: vec![
            format!("linux-aarch64:{}", arm_only.display())
                .parse()
                .unwrap(),
        ],
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("does not match any platform"),
        "{err}"
    );
}

#[rstest]
#[tokio::test]
async fn skips_locked_packages(tmpdir: tempfile::TempDir) {
    // A skipped package can be replaced by an injected one of the same name.
    let caddy = write_conda_package(tmpdir.path(), "caddy", &[]);
    let out = tmpdir.path().join("example.tar");
    let report = pixi_oci::build(BuildOptions {
        manifest_path: example_with_base(tmpdir.path(), "scratch"),
        platforms: vec!["linux-64".to_string()],
        output: Some(out.clone()),
        inject: vec![caddy.into()],
        skip: vec!["caddy".parse().unwrap()],
        ..Default::default()
    })
    .await
    .expect("build succeeds")
    .remove(0);
    let archive = Archive::open(&out);
    let files: HashMap<String, Vec<u8>> = archive.manifest().layers
        [report.images[0].rootfs_layers..][..report.images[0].package_layers]
        .iter()
        .flat_map(|layer| archive.layer_entries(layer))
        .map(|(name, _, data)| (name, data))
        .collect();
    assert_eq!(files["opt/env/share/caddy.txt"], b"hello from caddy\n");
    assert!(!files.contains_key("opt/env/bin/caddy"));

    // Skipping a dependency of another package leaves the environment broken.
    let err = pixi_oci::build(BuildOptions {
        manifest_path: example_with_base(tmpdir.path(), "scratch"),
        platforms: vec!["linux-64".to_string()],
        output: Some(tmpdir.path().join("other.tar")),
        skip: vec!["libexpat".parse().unwrap()],
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("libexpat"), "{err:#}");

    // Unless validation is skipped.
    pixi_oci::build(BuildOptions {
        manifest_path: example_with_base(tmpdir.path(), "scratch"),
        platforms: vec!["linux-64".to_string()],
        output: Some(tmpdir.path().join("unvalidated.tar")),
        skip: vec!["libexpat".parse().unwrap()],
        skip_validation: true,
        ..Default::default()
    })
    .await
    .expect("build succeeds without validation");
}

/// A copy of the workspace with a pixi-build source package in `dir`.
fn source_build_workspace(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/source-build");
    let workspace = dir.join("source-build");
    fs::create_dir_all(workspace.join("greeter")).unwrap();
    for file in [
        "pixi.toml",
        "pixi.lock",
        "greeter/pixi.toml",
        "greeter/recipe.yaml",
    ] {
        fs::copy(source.join(file), workspace.join(file)).unwrap();
    }
    workspace
}

/// The files in the package layers of each image of a multi-platform tarball.
fn platform_files(out: &Path, report: &pixi_oci::BuildReport) -> Vec<HashMap<String, Vec<u8>>> {
    let archive = Archive::open(out);
    let index = archive.index();
    let nested: Index = serde_json::from_slice(archive.blob(&index.manifests[0].digest)).unwrap();
    nested
        .manifests
        .iter()
        .zip(&report.images)
        .map(|(entry, image)| {
            let manifest: Manifest = serde_json::from_slice(archive.blob(&entry.digest)).unwrap();
            manifest.layers[..image.package_layers]
                .iter()
                .flat_map(|layer| archive.layer_entries(layer))
                .map(|(name, _, data)| (name, data))
                .collect()
        })
        .collect()
}

#[rstest]
#[tokio::test]
async fn builds_source_packages(tmpdir: tempfile::TempDir) {
    if !tool_available(&std::env::var("PIXI_EXE").unwrap_or_else(|_| "pixi".to_string())) {
        eprintln!("pixi not available, skipping");
        return;
    }
    let workspace = source_build_workspace(tmpdir.path());
    let platforms = vec!["linux-64".to_string(), "linux-aarch64".to_string()];
    let out = tmpdir.path().join("source-build.tar");
    let report = pixi_oci::build(BuildOptions {
        manifest_path: workspace.clone(),
        platforms: platforms.clone(),
        output: Some(out.clone()),
        ..Default::default()
    })
    .await
    .expect("build succeeds")
    .remove(0);
    let files = platform_files(&out, &report);
    for files in &files {
        assert_eq!(
            files["opt/env/share/greeter/greeting.txt"],
            b"hello from greeter\n"
        );
        assert!(
            files
                .keys()
                .any(|f| f.starts_with("opt/env/share/zoneinfo/"))
        );
    }
    // The noarch package is built once and installed on both platforms.
    let greeter_meta = |files: &HashMap<String, Vec<u8>>| {
        files
            .iter()
            .find(|(name, _)| name.starts_with("opt/env/conda-meta/greeter-0.1.0-"))
            .map(|(_, data)| data.clone())
            .expect("greeter is installed")
    };
    assert_eq!(greeter_meta(&files[0]), greeter_meta(&files[1]));

    // An injected package of the same name replaces the source package, which isn't built.
    let greeter = write_conda_package(tmpdir.path(), "greeter", &[]);
    let out = tmpdir.path().join("injected.tar");
    let report = pixi_oci::build(BuildOptions {
        manifest_path: workspace.clone(),
        platforms: platforms.clone(),
        output: Some(out.clone()),
        inject: vec![greeter.into()],
        ..Default::default()
    })
    .await
    .expect("build succeeds")
    .remove(0);
    for files in platform_files(&out, &report) {
        assert_eq!(files["opt/env/share/greeter.txt"], b"hello from greeter\n");
        assert!(!files.contains_key("opt/env/share/greeter/greeting.txt"));
    }

    // A skipped source package isn't built either.
    let out = tmpdir.path().join("skipped.tar");
    let report = pixi_oci::build(BuildOptions {
        manifest_path: workspace.clone(),
        platforms,
        output: Some(out.clone()),
        skip: vec!["greeter".parse().unwrap()],
        ..Default::default()
    })
    .await
    .expect("build succeeds")
    .remove(0);
    for files in platform_files(&out, &report) {
        assert!(!files.keys().any(|f| f.contains("greeter")));
    }

    // Images that need the same source package share one build.
    let manifest = fs::read_to_string(workspace.join("pixi.toml")).unwrap();
    fs::write(
        workspace.join("pixi.toml"),
        manifest.replace("tag = \"source-build\"\n", "")
            + "\n[tool.pixi-oci.images.one]\n[tool.pixi-oci.images.two]\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pixi-oci"))
        .args(["build", "-p", "linux-64", "--manifest-path"])
        .arg(&workspace)
        .current_dir(tmpdir.path())
        .output()
        .expect("pixi-oci runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr.matches("Building greeter").count(), 1, "{stderr}");
    let metas: Vec<_> = ["one.tar", "two.tar"]
        .iter()
        .map(|file| {
            let archive = Archive::open(&tmpdir.path().join(file));
            let files: HashMap<String, Vec<u8>> = archive
                .manifest()
                .layers
                .iter()
                .flat_map(|layer| archive.layer_entries(layer))
                .map(|(name, _, data)| (name, data))
                .collect();
            greeter_meta(&files)
        })
        .collect();
    assert_eq!(metas.len(), 2);
    assert_eq!(metas[0], metas[1]);

    // A source package that fails to build fails the image build.
    fs::write(workspace.join("greeter/recipe.yaml"), "not: a recipe\n").unwrap();
    let err = pixi_oci::build(BuildOptions {
        manifest_path: workspace,
        images: vec!["one".to_string()],
        platforms: vec!["linux-64".to_string()],
        output: Some(tmpdir.path().join("broken.tar")),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("could not build source packages greeter"),
        "{err:#}"
    );
}

#[rstest]
fn cli_build_is_reproducible(tmpdir: tempfile::TempDir) {
    let project = example_with_base(tmpdir.path(), "scratch");
    let mut digests = Vec::new();
    for i in 0..2 {
        let out = tmpdir.path().join(format!("build-{i}.tar"));
        let status = Command::new(env!("CARGO_BIN_EXE_pixi-oci"))
            .args(["build", "--manifest-path"])
            .arg(&project)
            .args(["--platform", "linux-64", "--output"])
            .arg(&out)
            .env("SOURCE_DATE_EPOCH", "0")
            .status()
            .expect("pixi-oci runs");
        assert!(status.success());
        digests.push(sha256_hex(&fs::read(&out).unwrap()));
    }
    assert_eq!(digests[0], digests[1], "builds must be reproducible");
}

/// A copy of the example project on a scratch base whose `[tool.pixi-oci]` table is the default
/// for the given `images` tables instead of an image of its own.
fn example_with_images(dir: &Path, images: &str) -> PathBuf {
    let project = example_with_base(dir, "scratch");
    let manifest = fs::read_to_string(project.join("pixi.toml")).unwrap();
    let manifest = manifest.replace("tag = \"example-project\"\n", "");
    fs::write(project.join("pixi.toml"), format!("{manifest}\n{images}")).unwrap();
    project
}

#[rstest]
fn cli_builds_all_images(tmpdir: tempfile::TempDir) {
    let project = example_with_images(
        tmpdir.path(),
        r#"
[tool.pixi-oci.images.web]
platform = "linux-64"
oci-config.cmd = ["--listen", ":80"]

[tool.pixi-oci.images.web-arm]
tag = "web-arm:1.0"
platform = "linux-aarch64"
copy-to-image = []
"#,
    );
    let pixi_oci = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_pixi-oci"))
            .args(["build", "--manifest-path"])
            .arg(&project)
            .args(args)
            .current_dir(tmpdir.path())
            .output()
            .expect("pixi-oci runs")
    };

    let output = pixi_oci(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let web = Archive::open(&tmpdir.path().join("web.tar"));
    let web_arm = Archive::open(&tmpdir.path().join("web-arm.tar"));
    let index = web.index();
    assert_eq!(
        index.manifests[0].annotations["org.opencontainers.image.ref.name"],
        "web:latest"
    );
    let config = web.config();
    assert_eq!(config.architecture, "amd64");
    assert_eq!(
        config.config.cmd,
        Some(vec!["--listen".to_string(), ":80".to_string()])
    );
    // Both inherit the entrypoint; only `web` has the copy layer.
    let config_arm = web_arm.config();
    assert_eq!(config_arm.architecture, "arm64");
    assert_eq!(config_arm.config.entrypoint, config.config.entrypoint);
    assert_eq!(
        web_arm.index().manifests[0].annotations["org.opencontainers.image.ref.name"],
        "web-arm:1.0"
    );
    assert_eq!(
        web.manifest().layers.len(),
        web_arm.manifest().layers.len() + 1
    );

    // Selecting one image builds only that one, and allows --output.
    let out = tmpdir.path().join("selected.tar");
    let output = pixi_oci(&["web-arm", "--output", out.to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(Archive::open(&out).config().architecture, "arm64");

    // --output with several images is ambiguous.
    let output = pixi_oci(&["--output", "x.tar"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--output needs a single image"), "{stderr}");

    let output = pixi_oci(&["nope"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Available images: web, web-arm"),
        "{stderr}"
    );
}

#[rstest]
#[tokio::test]
async fn rejects_images_with_the_same_output(tmpdir: tempfile::TempDir) {
    let project = example_with_images(
        tmpdir.path(),
        "[tool.pixi-oci.images.a]\ntag = \"app:a\"\n[tool.pixi-oci.images.b]\ntag = \"app:b\"\n",
    );
    let err = pixi_oci::build(BuildOptions {
        manifest_path: project,
        platforms: vec!["linux-64".to_string()],
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("images `a` and `b` would both be written to app.tar"),
        "{err}"
    );
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
    // The example is only locked for these linux platforms.
    let (subdir, oci_platform) = match host_arch.as_str() {
        "aarch64" | "arm64" => ("linux-aarch64", "linux/arm64"),
        "x86_64" | "amd64" => ("linux-64", "linux/amd64"),
        "ppc64le" => ("linux-ppc64le", "linux/ppc64le"),
        "riscv64" => ("linux-riscv64", "linux/riscv64"),
        other => {
            eprintln!("the example has no packages for docker's architecture {other}, skipping");
            return;
        }
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
        "linux-ppc64le" => "/lib64/ld64.so.2",
        "linux-riscv64" => "/lib/ld-linux-riscv64-lp64d.so.1",
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

/// A password-protected `registry:2` container, removed on drop.
struct LocalRegistry {
    container: String,
    host: String,
}

impl LocalRegistry {
    const USERNAME: &str = "testuser";
    const PASSWORD: &str = "testpassword";
    /// `htpasswd -nbB testuser testpassword`
    const HTPASSWD: &str = "testuser:$2y$05$9pHzQhR.ulyGSFroL43lNeCV3rnEpXDhuOUWppOV4uAS64Mfn2fe2";

    async fn start(dir: &Path) -> Self {
        let auth_dir = dir.join("registry-auth");
        fs::create_dir_all(&auth_dir).unwrap();
        fs::write(auth_dir.join("htpasswd"), format!("{}\n", Self::HTPASSWD)).unwrap();
        let run = Command::new("docker")
            .args(["run", "-d", "--rm", "-p", "127.0.0.1::5000", "-v"])
            .arg(format!("{}:/auth:ro", auth_dir.display()))
            .args([
                "-e",
                "REGISTRY_AUTH=htpasswd",
                "-e",
                "REGISTRY_AUTH_HTPASSWD_REALM=test",
                "-e",
                "REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd",
                "registry:2",
            ])
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "starting the registry failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let container = String::from_utf8_lossy(&run.stdout).trim().to_string();
        let port = Command::new("docker")
            .args(["port", &container, "5000/tcp"])
            .output()
            .unwrap();
        let port = String::from_utf8_lossy(&port.stdout);
        let port = port
            .lines()
            .next()
            .and_then(|l| l.rsplit_once(':'))
            .map(|(_, p)| p.trim().to_string())
            .expect("registry port");
        let registry = Self {
            container,
            host: format!("localhost:{port}"),
        };
        // Wait until the registry answers (with 401, as it requires credentials).
        let client = reqwest::Client::new();
        for _ in 0..60 {
            if client
                .get(format!("http://{}/v2/", registry.host))
                .send()
                .await
                .is_ok_and(|r| r.status() == reqwest::StatusCode::UNAUTHORIZED)
            {
                return registry;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        panic!("the registry did not come up");
    }

    /// Push a tiny linux/amd64 image with a single file and return its layer digest.
    async fn push_base_image(&self, reference: &str) -> String {
        let mut layer_tar = tar::Builder::new(Vec::new());
        let content = b"private base\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        layer_tar
            .append_data(&mut header, "private-base.txt", &content[..])
            .unwrap();
        let layer_tar = layer_tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &layer_tar).unwrap();
        let layer_gz = gz.finish().unwrap();
        let config = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {"type": "layers", "diff_ids": [format!("sha256:{}", sha256_hex(&layer_tar))]},
        });

        let client = oci_client::Client::new(oci_client::client::ClientConfig {
            protocol: oci_client::client::ClientProtocol::Http,
            ..Default::default()
        });
        client
            .push(
                &reference.parse().unwrap(),
                &[oci_client::client::ImageLayer::new(
                    layer_gz.clone(),
                    oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE.to_string(),
                    None,
                )],
                oci_client::client::Config::new(
                    serde_json::to_vec(&config).unwrap(),
                    oci_client::manifest::IMAGE_CONFIG_MEDIA_TYPE.to_string(),
                    None,
                ),
                &oci_client::secrets::RegistryAuth::Basic(
                    Self::USERNAME.to_string(),
                    Self::PASSWORD.to_string(),
                ),
                None,
            )
            .await
            .expect("pushing the base image");
        format!("sha256:{}", sha256_hex(&layer_gz))
    }
}

impl Drop for LocalRegistry {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .output();
    }
}

#[rstest]
#[tokio::test]
async fn pulls_private_base_image(tmpdir: tempfile::TempDir) {
    let docker_running = Command::new("docker")
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success());
    if !docker_running || std::env::var_os("PIXI_OCI_SKIP_DOCKER_TESTS").is_some() {
        eprintln!("docker not available, skipping");
        return;
    }
    let registry = LocalRegistry::start(tmpdir.path()).await;
    let base = format!("{}/private/base:latest", registry.host);
    let base_layer = registry.push_base_image(&base).await;
    let project = example_with_base(tmpdir.path(), &base);

    let build = |docker_config: &Path, out: &Path| {
        Command::new(env!("CARGO_BIN_EXE_pixi-oci"))
            .args(["build", "--manifest-path"])
            .arg(&project)
            .args(["--platform", "linux-64", "--output"])
            .arg(out)
            .env("DOCKER_CONFIG", docker_config)
            .env("XDG_CONFIG_HOME", tmpdir.path())
            .env_remove("REGISTRY_AUTH_FILE")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("PIXI_OCI_REGISTRY_USERNAME")
            .env_remove("PIXI_OCI_REGISTRY_PASSWORD")
            .output()
            .expect("pixi-oci runs")
    };

    // Without credentials, the pull is rejected and the error explains how to log in.
    let anonymous_config = tmpdir.path().join("docker-anonymous");
    fs::create_dir_all(&anonymous_config).unwrap();
    let output = build(&anonymous_config, &tmpdir.path().join("anonymous.tar"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("docker login"), "{stderr}");

    // With credentials from the docker config, the base layer ends up in the image.
    let docker_config = tmpdir.path().join("docker");
    fs::create_dir_all(&docker_config).unwrap();
    let auth = format!("{}:{}", LocalRegistry::USERNAME, LocalRegistry::PASSWORD);
    let auth = base64_encode(auth.as_bytes());
    fs::write(
        docker_config.join("config.json"),
        serde_json::json!({"auths": {registry.host.clone(): {"auth": auth}}}).to_string(),
    )
    .unwrap();
    let out = tmpdir.path().join("private.tar");
    let output = build(&docker_config, &out);
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest = Archive::open(&out).manifest();
    assert_eq!(manifest.layers[0].digest, base_layer);
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
