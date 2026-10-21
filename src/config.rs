//! Parsing of `pixi.toml` (or `pyproject.toml`) and the `[tool.pixi-oci]` section.

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use oci_spec::image::Config;
use serde::{Deserialize, Serialize};

/// Default location of the environment inside the image.
pub const DEFAULT_INSTALL_PREFIX: &str = "/opt/env";
/// Default image tag when the reference has none.
pub const DEFAULT_TAG: &str = "latest";
/// Default maximum number of package layers.
pub const DEFAULT_MAX_LAYERS: NonZeroUsize = NonZeroUsize::new(20).unwrap();
/// Default interpreter used for the activation wrapper script.
pub const DEFAULT_ACTIVATION_SHELL: &str = "/bin/bash";
/// Pseudo base image meaning "start from an empty root filesystem".
pub const SCRATCH: &str = "scratch";
include!("default_glibc.rs");

/// A pixi manifest (`pixi.toml` or `pyproject.toml`). Only `[tool.pixi-oci]` is read; the
/// packages come from `pixi.lock`.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct Manifest {
    #[serde(default)]
    pub tool: ToolSection,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct ToolSection {
    #[serde(default, rename = "pixi-oci")]
    pub pixi_oci: PixiOciConfig,
}

/// The `[tool.pixi-oci]` section.
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PixiOciConfig {
    /// A registry image to build on, or a root filesystem built from scratch.
    pub base: Option<BaseSpec>,
    /// The pixi environment to install.
    pub environment: Option<String>,
    /// The conda platform (subdir) to build for.
    pub platform: Option<String>,
    /// Absolute path of the environment inside the image.
    pub install_prefix: Option<String>,
    /// Image reference as `name:tag` or `name` (tag `latest`).
    pub tag: Option<String>,
    /// Maximum number of layers used for the conda packages.
    pub max_layers: Option<NonZeroUsize>,
    /// Additional files to copy into the image.
    #[serde(default)]
    pub copy_to_image: Vec<CopyEntry>,
    /// Interpreter for the activation wrapper script.
    pub activation_shell: Option<String>,
    /// Image config fields (docker image spec `config`).
    #[serde(default)]
    pub oci_config: OciConfigSection,
}

/// `base`: either a registry reference such as `"ubuntu:latest"` (or `"scratch"` for an empty
/// root filesystem), or a table describing a minimal root filesystem built from scratch.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum BaseSpec {
    Image(String),
    Scratch(ScratchBase),
}

impl Default for BaseSpec {
    fn default() -> Self {
        BaseSpec::Scratch(ScratchBase::default())
    }
}

/// A root filesystem built from scratch.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ScratchBase {
    /// Add `/etc/passwd`, `/etc/group`, their shadow files, `/root` and `/tmp`.
    #[serde(default = "default_true")]
    pub shadow_setup: bool,
    /// Add glibc from Debian's `libc6` package.
    #[serde(default)]
    pub glibc: GlibcSpec,
}

impl Default for ScratchBase {
    fn default() -> Self {
        Self {
            shadow_setup: true,
            glibc: GlibcSpec::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// `glibc = true` (default version), `false` (none) or a Debian `libc6` version.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum GlibcSpec {
    Enabled(bool),
    DebianVersion(String),
}

impl Default for GlibcSpec {
    fn default() -> Self {
        GlibcSpec::Enabled(true)
    }
}

impl GlibcSpec {
    /// The Debian `libc6` version to install, if any.
    pub fn debian_version(&self) -> Option<&str> {
        match self {
            GlibcSpec::Enabled(true) => Some(DEFAULT_GLIBC_DEBIAN_VERSION),
            GlibcSpec::Enabled(false) => None,
            GlibcSpec::DebianVersion(version) => Some(version),
        }
    }
}

/// A file or directory to copy into the image.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum CopyEntry {
    Source(String),
    Full {
        source: String,
        #[serde(default)]
        destination: Option<String>,
    },
}

impl CopyEntry {
    pub fn source(&self) -> &str {
        match self {
            CopyEntry::Source(s) => s,
            CopyEntry::Full { source, .. } => source,
        }
    }

    /// The absolute destination inside the image. Entries without an explicit destination
    /// land in `working_dir` under their file name, like `COPY` in a Dockerfile.
    pub fn destination(&self, working_dir: &str) -> Result<String> {
        let explicit = match self {
            CopyEntry::Source(_) => None,
            CopyEntry::Full { destination, .. } => destination.as_deref(),
        };
        let dest = match explicit {
            Some(d) if d.starts_with('/') => d.to_string(),
            Some(d) => join_image_path(working_dir, d),
            None => {
                let file_name = Path::new(self.source())
                    .file_name()
                    .and_then(|f| f.to_str())
                    .ok_or_else(|| {
                        anyhow!(
                            "copy-to-image source `{}` has no file name, specify a destination",
                            self.source()
                        )
                    })?;
                join_image_path(working_dir, file_name)
            }
        };
        Ok(normalize_image_path(&dest))
    }
}

/// Join an in-image path onto a working directory.
fn join_image_path(working_dir: &str, rel: &str) -> String {
    format!("{}/{}", working_dir.trim_end_matches('/'), rel)
}

/// Normalize an absolute in-image path: collapse `//`, `.` and trailing slashes.
pub fn normalize_image_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The `[tool.pixi-oci.oci-config]` section.
///
/// Field names follow the docker image spec, in kebab-case.
/// We keep our own struct instead of deserializing `oci_spec::image::Config` for nicer TOML.
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OciConfigSection {
    pub entrypoint: Option<StringOrVec>,
    pub cmd: Option<StringOrVec>,
    pub env: Option<EnvSpec>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub exposed_ports: Vec<PortSpec>,
    #[serde(default)]
    pub volumes: Vec<String>,
    pub stop_signal: Option<String>,
    pub healthcheck: Option<HealthcheckSpec>,
    pub shell: Option<StringOrVec>,
}

impl OciConfigSection {
    /// Convert into an OCI image [`Config`]. Unset fields stay `None` so they can be inherited
    /// from the base image (see [`crate::image::merge_config`]).
    pub fn to_oci_config(&self) -> Result<Config> {
        let mut config = Config::default();
        config.set_entrypoint(
            self.entrypoint
                .as_ref()
                .map(StringOrVec::to_argv)
                .transpose()?,
        );
        config.set_cmd(self.cmd.as_ref().map(StringOrVec::to_argv).transpose()?);
        config.set_env(
            self.env
                .as_ref()
                .map(|env| -> Result<Vec<String>> {
                    Ok(env
                        .to_pairs()?
                        .into_iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect())
                })
                .transpose()?,
        );
        config.set_working_dir(self.working_dir.clone());
        config.set_user(self.user.clone());
        config.set_labels(
            Some(self.labels.clone().into_iter().collect::<HashMap<_, _>>())
                .filter(|l| !l.is_empty()),
        );
        config.set_exposed_ports(
            Some(
                self.exposed_ports
                    .iter()
                    .map(PortSpec::to_port_string)
                    .collect::<Vec<_>>(),
            )
            .filter(|p| !p.is_empty()),
        );
        config.set_volumes(Some(self.volumes.clone()).filter(|v| !v.is_empty()));
        config.set_stop_signal(self.stop_signal.clone());
        Ok(config)
    }

    /// The docker-specific config fields that [`Config`] has no room for.
    pub fn to_docker_extensions(&self) -> Result<DockerConfigExtensions> {
        let shell = self
            .shell
            .as_ref()
            .map(|shell| {
                let argv = shell.to_argv()?;
                if argv.is_empty() {
                    bail!("oci-config.shell must not be empty");
                }
                Ok(argv)
            })
            .transpose()?;
        Ok(DockerConfigExtensions {
            healthcheck: self
                .healthcheck
                .as_ref()
                .map(HealthcheckSpec::to_health_config)
                .transpose()
                .context("invalid oci-config.healthcheck")?,
            shell,
        })
    }
}

/// `[tool.pixi-oci.oci-config.healthcheck]`, like `HEALTHCHECK` in a Dockerfile.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct HealthcheckSpec {
    /// A string runs through the image's shell (`CMD-SHELL`), a list is executed directly
    /// (`CMD`). Lists starting with `CMD`, `CMD-SHELL` or `NONE` are taken as they are, so
    /// `["NONE"]` disables a healthcheck inherited from the base image.
    pub test: StringOrVec,
    pub interval: Option<DurationSpec>,
    pub timeout: Option<DurationSpec>,
    pub start_period: Option<DurationSpec>,
    pub start_interval: Option<DurationSpec>,
    pub retries: Option<u32>,
}

impl HealthcheckSpec {
    pub fn to_health_config(&self) -> Result<HealthConfig> {
        let test = match &self.test {
            StringOrVec::String(cmd) => vec!["CMD-SHELL".to_string(), cmd.clone()],
            StringOrVec::Vec(argv) => match argv.first().map(String::as_str) {
                None => bail!("`test` must not be empty"),
                Some("CMD" | "CMD-SHELL" | "NONE") => argv.clone(),
                Some(_) => std::iter::once("CMD".to_string())
                    .chain(argv.iter().cloned())
                    .collect(),
            },
        };
        let nanos = |name: &str, d: &Option<DurationSpec>| -> Result<Option<i64>> {
            d.as_ref()
                .map(|d| d.to_nanos().with_context(|| format!("invalid `{name}`")))
                .transpose()
        };
        Ok(HealthConfig {
            test: Some(test),
            interval: nanos("interval", &self.interval)?,
            timeout: nanos("timeout", &self.timeout)?,
            start_period: nanos("start-period", &self.start_period)?,
            start_interval: nanos("start-interval", &self.start_interval)?,
            retries: self.retries,
        })
    }
}

/// A duration given as a Go-style string (`"30s"`, `"1m30s"`, `"500ms"`) or in seconds.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum DurationSpec {
    Seconds(u32),
    String(String),
}

impl DurationSpec {
    /// The duration in nanoseconds; docker requires at least one millisecond.
    pub fn to_nanos(&self) -> Result<i64> {
        let nanos = match self {
            DurationSpec::Seconds(s) => i64::from(*s) * 1_000_000_000,
            DurationSpec::String(s) => parse_duration_nanos(s)?,
        };
        if nanos < 1_000_000 {
            bail!("duration must be at least 1ms");
        }
        Ok(nanos)
    }
}

/// Parse a Go duration such as `"1h2m3.5s"` into nanoseconds.
fn parse_duration_nanos(s: &str) -> Result<i64> {
    let invalid = || anyhow!("invalid duration `{s}`, expected e.g. `30s`, `1m30s` or `500ms`");
    let mut rest = s.trim();
    if rest.is_empty() {
        return Err(invalid());
    }
    let mut total = 0f64;
    while !rest.is_empty() {
        let number_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(invalid)?;
        let number: f64 = rest[..number_len].parse().map_err(|_| invalid())?;
        rest = &rest[number_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_len] {
            "ns" => 1.0,
            "us" | "µs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return Err(invalid()),
        };
        rest = &rest[unit_len..];
        total += number * factor;
    }
    if total > i64::MAX as f64 {
        return Err(invalid());
    }
    Ok(total.round() as i64)
}

/// Docker image spec config fields that are not part of the OCI image spec. They are added to
/// the image config next to the fields of [`Config`].
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct DockerConfigExtensions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<Vec<String>>,
}

/// The docker `Healthcheck` object. Durations are in nanoseconds.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct HealthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_period: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_interval: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
}

/// A command given either as a shell-like string or as an argv list.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum StringOrVec {
    String(String),
    Vec(Vec<String>),
}

impl StringOrVec {
    pub fn to_argv(&self) -> Result<Vec<String>> {
        match self {
            StringOrVec::Vec(v) => Ok(v.clone()),
            StringOrVec::String(s) => {
                shlex::split(s).ok_or_else(|| anyhow!("could not parse command `{s}`"))
            }
        }
    }
}

/// Environment variables given as a table or as a list of `KEY=VALUE` strings.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum EnvSpec {
    Map(BTreeMap<String, String>),
    List(Vec<String>),
}

impl EnvSpec {
    pub fn to_pairs(&self) -> Result<Vec<(String, String)>> {
        match self {
            EnvSpec::Map(m) => Ok(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            EnvSpec::List(l) => l
                .iter()
                .map(|entry| {
                    entry
                        .split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .ok_or_else(|| anyhow!("environment entry `{entry}` is not KEY=VALUE"))
                })
                .collect(),
        }
    }
}

/// A port given as `"8080/tcp"`, `"8080"` or `8080`.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum PortSpec {
    Number(u16),
    String(String),
}

impl PortSpec {
    pub fn to_port_string(&self) -> String {
        match self {
            PortSpec::Number(n) => format!("{n}/tcp"),
            PortSpec::String(s) if s.contains('/') => s.clone(),
            PortSpec::String(s) => format!("{s}/tcp"),
        }
    }
}

/// A loaded manifest together with its location.
#[derive(Debug, Clone)]
pub struct LoadedManifest {
    /// The manifest file itself.
    pub path: PathBuf,
    /// The directory containing the manifest (and the lockfile).
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl LoadedManifest {
    /// Locate and parse the manifest. `path` may be the manifest file or its directory.
    pub fn load(path: &Path) -> Result<Self> {
        let path = if path.is_dir() {
            let pixi = path.join("pixi.toml");
            let pyproject = path.join("pyproject.toml");
            if pixi.exists() {
                pixi
            } else if pyproject.exists() {
                pyproject
            } else {
                bail!("no pixi.toml or pyproject.toml found in {}", path.display());
            }
        } else {
            path.to_path_buf()
        };
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read manifest {}", path.display()))?;
        let manifest: Manifest = toml::from_str(&contents)
            .with_context(|| format!("could not parse manifest {}", path.display()))?;

        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            path,
            dir,
            manifest,
        })
    }

    pub fn config(&self) -> &PixiOciConfig {
        &self.manifest.tool.pixi_oci
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Manifest {
        toml::from_str(toml).expect("valid manifest")
    }

    #[test]
    fn parses_example_manifest() {
        let m = parse(include_str!("../example/pixi.toml"));
        let cfg = m.tool.pixi_oci;
        assert_eq!(
            cfg.base,
            Some(BaseSpec::Scratch(ScratchBase {
                shadow_setup: true,
                glibc: GlibcSpec::DebianVersion("2.41-12+deb13u4".into()),
            }))
        );
        assert_eq!(cfg.max_layers, NonZeroUsize::new(20));
        assert_eq!(cfg.copy_to_image.len(), 2);
        assert_eq!(
            cfg.oci_config.entrypoint.unwrap().to_argv().unwrap(),
            vec!["caddy", "file-server", "--browse", "--root", "."]
        );
        assert_eq!(cfg.tag.as_deref(), Some("example-project"));
    }

    #[test]
    fn entrypoint_as_array() {
        let m = parse(
            r#"
[tool.pixi-oci.oci-config]
entrypoint = ["a", "b c"]
working-dir = "/app"
env = ["A=1", "B=x=y"]
exposed-ports = [8080, "9090", "53/udp"]
"#,
        );
        let cfg = m.tool.pixi_oci.oci_config;
        assert_eq!(
            cfg.entrypoint.unwrap().to_argv().unwrap(),
            vec!["a".to_string(), "b c".to_string()]
        );
        assert_eq!(cfg.working_dir.as_deref(), Some("/app"));
        assert_eq!(
            cfg.env.unwrap().to_pairs().unwrap(),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "x=y".to_string())
            ]
        );
        let ports: Vec<_> = cfg
            .exposed_ports
            .iter()
            .map(PortSpec::to_port_string)
            .collect();
        assert_eq!(ports, vec!["8080/tcp", "9090/tcp", "53/udp"]);
    }

    #[test]
    fn pascal_case_spec_names_are_rejected() {
        let err = toml::from_str::<Manifest>("[tool.pixi-oci.oci-config]\nWorkingDir = \"/app\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("WorkingDir"), "{err}");
    }

    #[test]
    fn converts_to_oci_config() {
        let m = parse(
            r#"
[tool.pixi-oci.oci-config]
cmd = "serve --port 80"
env = { FOO = "bar" }
exposed-ports = [8080]
"#,
        );
        let config = m.tool.pixi_oci.oci_config.to_oci_config().unwrap();
        assert!(config.entrypoint().is_none());
        assert_eq!(
            config.cmd().as_deref(),
            Some(&["serve".to_string(), "--port".to_string(), "80".to_string()][..])
        );
        assert_eq!(config.env().as_deref(), Some(&["FOO=bar".to_string()][..]));
        assert_eq!(
            config.exposed_ports().as_deref(),
            Some(&["8080/tcp".to_string()][..])
        );
        assert!(config.labels().is_none());
        assert!(config.volumes().is_none());
    }

    #[test]
    fn parses_base_variants() {
        let base = |toml: &str| parse(toml).tool.pixi_oci.base;
        assert_eq!(
            base("[tool.pixi-oci]\nbase = \"ubuntu:latest\"\n"),
            Some(BaseSpec::Image("ubuntu:latest".into()))
        );
        let scratch = base("[tool.pixi-oci]\nbase = {}\n").unwrap();
        assert_eq!(scratch, BaseSpec::default());
        let BaseSpec::Scratch(scratch) = scratch else {
            panic!("expected a scratch base");
        };
        assert_eq!(
            scratch.glibc.debian_version(),
            Some(DEFAULT_GLIBC_DEBIAN_VERSION)
        );
        assert_eq!(
            base("[tool.pixi-oci]\nbase = { shadow-setup = false, glibc = false }\n"),
            Some(BaseSpec::Scratch(ScratchBase {
                shadow_setup: false,
                glibc: GlibcSpec::Enabled(false),
            }))
        );
        assert!(
            toml::from_str::<Manifest>("[tool.pixi-oci]\nbase = { glibcc = false }\n").is_err()
        );
    }

    #[test]
    fn env_as_table() {
        let m = parse("[tool.pixi-oci.oci-config]\nenv = { FOO = \"bar\" }\n");
        assert_eq!(
            m.tool.pixi_oci.oci_config.env.unwrap().to_pairs().unwrap(),
            vec![("FOO".to_string(), "bar".to_string())]
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Manifest>("[tool.pixi-oci]\nbase_image = \"ubuntu\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("base_image"), "{err}");
    }

    #[test]
    fn copy_destination_defaults_to_working_dir() {
        let e = CopyEntry::Source("dir/my-file".into());
        assert_eq!(e.destination("/").unwrap(), "/my-file");
        assert_eq!(e.destination("/app/").unwrap(), "/app/my-file");
        let e = CopyEntry::Full {
            source: "x".into(),
            destination: Some("/renamed".into()),
        };
        assert_eq!(e.destination("/app").unwrap(), "/renamed");
        let e = CopyEntry::Full {
            source: "x".into(),
            destination: Some("sub/renamed".into()),
        };
        assert_eq!(e.destination("/app").unwrap(), "/app/sub/renamed");
    }

    #[test]
    fn converts_healthcheck_and_shell() {
        let m = parse(
            r#"
[tool.pixi-oci.oci-config]
shell = ["/opt/env/bin/bash", "-c"]
[tool.pixi-oci.oci-config.healthcheck]
test = "curl -f http://localhost:8080 || exit 1"
interval = "1m30s"
timeout = 5
start-period = "500ms"
retries = 3
"#,
        );
        let ext = m.tool.pixi_oci.oci_config.to_docker_extensions().unwrap();
        assert_eq!(
            ext.shell.as_deref(),
            Some(&["/opt/env/bin/bash".to_string(), "-c".to_string()][..])
        );
        assert_eq!(
            ext.healthcheck,
            Some(HealthConfig {
                test: Some(vec![
                    "CMD-SHELL".into(),
                    "curl -f http://localhost:8080 || exit 1".into()
                ]),
                interval: Some(90_000_000_000),
                timeout: Some(5_000_000_000),
                start_period: Some(500_000_000),
                start_interval: None,
                retries: Some(3),
            })
        );
        assert_eq!(
            serde_json::to_value(&ext.healthcheck).unwrap()["StartPeriod"],
            500_000_000
        );
    }

    #[test]
    fn healthcheck_test_variants() {
        let test = |toml: &str| {
            let m = parse(&format!(
                "[tool.pixi-oci.oci-config.healthcheck]\ntest = {toml}\n"
            ));
            m.tool
                .pixi_oci
                .oci_config
                .to_docker_extensions()
                .map(|e| e.healthcheck.unwrap().test.unwrap())
        };
        assert_eq!(
            test(r#"["curl", "-f", "localhost"]"#).unwrap(),
            vec!["CMD", "curl", "-f", "localhost"]
        );
        assert_eq!(
            test(r#"["CMD-SHELL", "true"]"#).unwrap(),
            vec!["CMD-SHELL", "true"]
        );
        assert_eq!(test(r#"["NONE"]"#).unwrap(), vec!["NONE"]);
        assert!(test("[]").is_err());
        assert!(
            toml::from_str::<Manifest>(
                "[tool.pixi-oci.oci-config.healthcheck]\ninterval = \"5s\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_nanos("30s").unwrap(), 30_000_000_000);
        assert_eq!(parse_duration_nanos("1h2m3.5s").unwrap(), 3_723_500_000_000);
        assert_eq!(parse_duration_nanos("250ms").unwrap(), 250_000_000);
        assert_eq!(parse_duration_nanos("10us").unwrap(), 10_000);
        for invalid in ["", "30", "s", "5 minutes", "1.2.3s"] {
            assert!(parse_duration_nanos(invalid).is_err(), "{invalid}");
        }
        assert!(DurationSpec::String("100us".into()).to_nanos().is_err());
        assert!(DurationSpec::Seconds(0).to_nanos().is_err());
    }

    #[test]
    fn normalizes_image_paths() {
        assert_eq!(normalize_image_path("//a/./b//c/"), "/a/b/c");
        assert_eq!(normalize_image_path("/a/../b"), "/b");
        assert_eq!(normalize_image_path("/"), "/");
    }
}
