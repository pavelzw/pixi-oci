//! Credentials for pulling a private base image.
//!
//! Sources, in order (the first one with credentials for the registry wins):
//! 1. `PIXI_OCI_REGISTRY_USERNAME` and `PIXI_OCI_REGISTRY_PASSWORD`,
//! 2. the containers auth files used by podman, skopeo and buildah: `$REGISTRY_AUTH_FILE`,
//!    `$XDG_RUNTIME_DIR/containers/auth.json` and `$XDG_CONFIG_HOME/containers/auth.json`,
//! 3. the docker config: `$DOCKER_CONFIG/config.json` or `~/.docker/config.json`, including
//!    `credsStore` and `credHelpers` (e.g. `docker-credential-osxkeychain` or `ecr-login`).
//!
//! Without credentials the registry is accessed anonymously.

use std::{
    ffi::OsString,
    fmt,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use docker_credential::{CredentialRetrievalError, DockerCredential};
use oci_client::{Reference, secrets::RegistryAuth};

pub const USERNAME_ENV: &str = "PIXI_OCI_REGISTRY_USERNAME";
pub const PASSWORD_ENV: &str = "PIXI_OCI_REGISTRY_PASSWORD";

/// The key docker uses for Docker Hub in `config.json` and with credential helpers.
const DOCKER_HUB_KEY: &str = "https://index.docker.io/v1/";

/// Where credentials were found, for log and error messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSource {
    Anonymous,
    Environment,
    File(PathBuf),
}

impl fmt::Display for AuthSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthSource::Anonymous => write!(f, "anonymous access"),
            AuthSource::Environment => write!(f, "credentials from ${USERNAME_ENV}"),
            AuthSource::File(path) => write!(f, "credentials from {}", path.display()),
        }
    }
}

/// Credentials for a registry and where they came from.
#[derive(Debug)]
pub struct ResolvedAuth {
    pub auth: RegistryAuth,
    pub source: AuthSource,
}

/// The places credentials are looked up in.
#[derive(Debug, Default)]
pub struct CredentialSources {
    pub username: Option<String>,
    pub password: Option<String>,
    /// Auth files in the docker `config.json` format, in order of precedence.
    pub files: Vec<PathBuf>,
}

impl CredentialSources {
    /// The sources from the process environment.
    pub fn from_env() -> Self {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        let string = |name| var(name).and_then(|v: OsString| v.into_string().ok());
        let home = var("HOME").map(PathBuf::from);

        let mut files = Vec::new();
        files.extend(var("REGISTRY_AUTH_FILE").map(PathBuf::from));
        files.extend(var("XDG_RUNTIME_DIR").map(|d| Path::new(&d).join("containers/auth.json")));
        files.extend(
            var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(".config")))
                .map(|d| d.join("containers/auth.json")),
        );
        files.extend(
            var("DOCKER_CONFIG")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(".docker")))
                .map(|d| d.join("config.json")),
        );

        Self {
            username: string(USERNAME_ENV),
            password: string(PASSWORD_ENV),
            files,
        }
    }

    /// Find the credentials for the registry of `reference`.
    pub fn resolve(&self, reference: &Reference) -> Result<ResolvedAuth> {
        match (&self.username, &self.password) {
            (Some(username), Some(password)) => {
                return Ok(ResolvedAuth {
                    auth: RegistryAuth::Basic(username.clone(), password.clone()),
                    source: AuthSource::Environment,
                });
            }
            (None, None) => {}
            _ => bail!("set both {USERNAME_ENV} and {PASSWORD_ENV}, or neither"),
        }

        let server = server_key(reference);
        for path in &self.files {
            if let Some(auth) = credential_from_file(path, &server) {
                return Ok(ResolvedAuth {
                    auth,
                    source: AuthSource::File(path.clone()),
                });
            }
        }
        Ok(ResolvedAuth {
            auth: RegistryAuth::Anonymous,
            source: AuthSource::Anonymous,
        })
    }
}

/// The key of the registry in auth files and for credential helpers.
fn server_key(reference: &Reference) -> String {
    match reference.registry() {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => DOCKER_HUB_KEY.to_string(),
        registry => registry.to_string(),
    }
}

/// The credentials for `server` in the auth file at `path`. Problems are logged, not returned:
/// a broken config or credential helper shouldn't fail builds that don't need credentials.
fn credential_from_file(path: &Path, server: &str) -> Option<RegistryAuth> {
    let file = File::open(path).ok()?;
    match docker_credential::get_credential_from_reader(BufReader::new(file), server) {
        Ok(DockerCredential::UsernamePassword(username, password)) => {
            Some(RegistryAuth::Basic(username, password))
        }
        Ok(DockerCredential::IdentityToken(_)) => {
            tracing::warn!(
                "ignoring the identity token for {server} in {}: identity tokens are not \
                 supported, use a username and password (or {USERNAME_ENV} and {PASSWORD_ENV})",
                path.display()
            );
            None
        }
        Err(CredentialRetrievalError::NoCredentialConfigured) => None,
        // A credential store without an entry for this registry: the common case for public
        // images on machines with Docker Desktop.
        Err(CredentialRetrievalError::HelperFailure { stdout, .. })
            if stdout.contains("credentials not found") =>
        {
            None
        }
        Err(e) => {
            tracing::warn!(
                "could not read credentials for {server} from {}: {e}",
                path.display()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn reference(image: &str) -> Reference {
        image.parse().unwrap()
    }

    #[test]
    fn docker_hub_uses_the_index_key() {
        assert_eq!(server_key(&reference("ubuntu")), DOCKER_HUB_KEY);
        assert_eq!(
            server_key(&reference("docker.io/library/ubuntu")),
            DOCKER_HUB_KEY
        );
        assert_eq!(server_key(&reference("ghcr.io/a/b")), "ghcr.io");
        assert_eq!(
            server_key(&reference("localhost:5000/a/b")),
            "localhost:5000"
        );
    }

    #[test]
    fn anonymous_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CredentialSources {
            files: vec![
                dir.path().join("missing.json"),
                auth_file(dir.path(), "a.json", r#"{"auths": {}}"#),
                auth_file(dir.path(), "broken.json", "not json"),
            ],
            ..Default::default()
        };
        let resolved = sources.resolve(&reference("ghcr.io/a/b")).unwrap();
        assert!(matches!(resolved.auth, RegistryAuth::Anonymous));
        assert_eq!(resolved.source, AuthSource::Anonymous);
    }

    #[test]
    fn reads_auths_in_order() {
        let dir = tempfile::tempdir().unwrap();
        // base64("first:one"), base64("second:two")
        let first = auth_file(
            dir.path(),
            "first.json",
            r#"{"auths": {"ghcr.io": {"auth": "Zmlyc3Q6b25l"}}}"#,
        );
        let second = auth_file(
            dir.path(),
            "second.json",
            r#"{"auths": {"ghcr.io": {"auth": "c2Vjb25kOnR3bw=="},
                          "https://index.docker.io/v1/": {"auth": "c2Vjb25kOnR3bw=="}}}"#,
        );
        let sources = CredentialSources {
            files: vec![first.clone(), second.clone()],
            ..Default::default()
        };

        let resolved = sources.resolve(&reference("ghcr.io/a/b")).unwrap();
        assert_eq!(
            resolved.auth,
            RegistryAuth::Basic("first".into(), "one".into())
        );
        assert_eq!(resolved.source, AuthSource::File(first));

        let resolved = sources.resolve(&reference("ubuntu")).unwrap();
        assert_eq!(
            resolved.auth,
            RegistryAuth::Basic("second".into(), "two".into())
        );
        assert_eq!(resolved.source, AuthSource::File(second));
    }

    #[test]
    fn skips_identity_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CredentialSources {
            files: vec![auth_file(
                dir.path(),
                "a.json",
                r#"{"auths": {"x.azurecr.io": {"identitytoken": "t"}}}"#,
            )],
            ..Default::default()
        };
        let resolved = sources.resolve(&reference("x.azurecr.io/a")).unwrap();
        assert_eq!(resolved.source, AuthSource::Anonymous);
    }

    #[test]
    fn environment_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let sources = CredentialSources {
            username: Some("env".into()),
            password: Some("secret".into()),
            files: vec![auth_file(
                dir.path(),
                "a.json",
                r#"{"auths": {"ghcr.io": {"auth": "Zmlyc3Q6b25l"}}}"#,
            )],
        };
        let resolved = sources.resolve(&reference("ghcr.io/a/b")).unwrap();
        assert_eq!(
            resolved.auth,
            RegistryAuth::Basic("env".into(), "secret".into())
        );
        assert_eq!(resolved.source, AuthSource::Environment);
    }

    #[test]
    fn rejects_half_set_environment() {
        let sources = CredentialSources {
            username: Some("env".into()),
            ..Default::default()
        };
        assert!(sources.resolve(&reference("ghcr.io/a/b")).is_err());
    }
}
