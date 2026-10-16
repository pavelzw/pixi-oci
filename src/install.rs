//! Downloading and installing the locked packages into a staging prefix.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use rattler::install::{IndicatifReporter, Installer};
use rattler_cache::{PACKAGE_CACHE_DIR, package_cache::PackageCache};
use rattler_conda_types::{RepoDataRecord, Subdir};
use rattler_networking::{
    AuthenticationMiddleware, AuthenticationStorage, MirrorMiddleware,
    authentication_storage::backends::file::FileStorage, proxy::proxies_from_config,
};
use reqwest_middleware::ClientWithMiddleware;

/// The rattler/pixi configuration file type (mirrors, authentication, ...).
pub type Config = rattler_config::config::ConfigBase<rattler_config::config::NoExtension>;

/// Load an optional rattler config file.
pub fn load_config(path: Option<&Path>) -> Result<Option<Config>> {
    path.map(|path| {
        Config::load_from_files(vec![path])
            .map_err(|e| anyhow!("could not parse config file {}: {e}", path.display()))
    })
    .transpose()
}

/// Create a download client like pixi does: mirrors, proxies, TLS verification and the
/// authentication file are taken from `config`.
pub fn reqwest_client_from_config(config: &Config) -> Result<ClientWithMiddleware> {
    let auth_storage = match &config.authentication_override_file {
        Some(path) => {
            let mut storage = AuthenticationStorage::empty();
            storage.add_backend(Arc::new(
                FileStorage::from_path(path.clone()).with_context(|| {
                    format!("could not read authentication file {}", path.display())
                })?,
            ));
            storage
        }
        None => AuthenticationStorage::from_env_and_defaults()?,
    };

    let mut builder = reqwest::Client::builder()
        .no_gzip()
        .pool_max_idle_per_host(20)
        .user_agent(format!("pixi-oci/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5 * 60))
        .danger_accept_invalid_certs(config.tls_no_verify.unwrap_or(false));
    for proxy in proxies_from_config(&config.proxy_config).context("invalid proxy configuration")? {
        builder = builder.proxy(proxy);
    }

    let client = reqwest_middleware::ClientBuilder::new(
        builder
            .build()
            .context("could not create download client")?,
    )
    .with(MirrorMiddleware::from_config(config))
    .with_arc(Arc::new(AuthenticationMiddleware::from_auth_storage(
        auth_storage,
    )))
    .build();
    Ok(client)
}

/// Install `records` into `staging_prefix` on the host. Hardcoded prefix paths inside the files
/// are patched to `install_prefix`, the location the environment will have inside the image.
pub async fn install_environment(
    staging_prefix: &Path,
    install_prefix: &str,
    subdir: Subdir,
    records: Vec<RepoDataRecord>,
    client: ClientWithMiddleware,
    cache_dir: &Path,
) -> Result<usize> {
    let package_cache = PackageCache::new(cache_dir.join(PACKAGE_CACHE_DIR));
    let result = Installer::new()
        .with_download_client(client)
        .with_package_cache(package_cache)
        .with_target_platform(subdir)
        .with_alternative_target_prefix(install_prefix)
        // Link scripts would run against the staging path on the build host, not the
        // final prefix, so they stay disabled.
        .with_execute_link_scripts(false)
        .with_reporter(IndicatifReporter::builder().finish())
        .install(staging_prefix, records)
        .await
        .context("could not install the environment")?;

    // conda expects a history file in every prefix.
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(staging_prefix.join("conda-meta/history"))
        .context("could not create conda-meta/history")?;

    Ok(result.transaction.operations.len())
}
