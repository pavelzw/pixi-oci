//! Downloading the locked packages and planning where their files go in the image.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use rattler::install::from_archive::{ArchiveLinkOptions, ArchivePackage, LinkPlan};
use rattler_cache::{ARCHIVE_CACHE_DIR, archive_cache::ArchiveCache};
use rattler_conda_types::{RepoDataRecord, Subdir};
use rattler_networking::{
    AuthenticationMiddleware, AuthenticationStorage, MirrorMiddleware,
    authentication_storage::backends::file::FileStorage, proxy::proxies_from_config,
};
use reqwest_middleware::ClientWithMiddleware;

/// The rattler/pixi configuration file type (mirrors, authentication, ...).
pub type RattlerConfig = rattler_config::config::ConfigBase<rattler_config::config::NoExtension>;

/// Load an optional rattler config file.
pub fn load_config(path: Option<&Path>) -> Result<Option<RattlerConfig>> {
    path.map(|path| {
        RattlerConfig::load_from_files(vec![path])
            .map_err(|e| anyhow!("could not parse config file {}: {e}", path.display()))
    })
    .transpose()
}

/// Create a download client like pixi does: mirrors, proxies, TLS verification and the
/// authentication file are taken from `config`.
// TODO: use rattler's client construction from a config once it provides one.
pub fn reqwest_client_from_config(config: &RattlerConfig) -> Result<ClientWithMiddleware> {
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

/// How many archives are downloaded at the same time.
const CONCURRENT_DOWNLOADS: usize = 16;

/// Download the archives of `records` (or take them from the archive cache) and plan where
/// their files end up below `install_prefix`.
///
/// Nothing is installed on the build host: the files are later streamed from the archives
/// straight into the layers, so symlinks, permissions and file names survive no matter which
/// filesystem the build host has.
pub async fn plan_environment(
    install_prefix: &str,
    subdir: Subdir,
    records: Vec<RepoDataRecord>,
    client: ClientWithMiddleware,
    cache_dir: &Path,
) -> Result<LinkPlan> {
    let cache = ArchiveCache::new(cache_dir.join(ARCHIVE_CACHE_DIR));
    let progress = ProgressBar::new(records.len() as u64).with_style(
        ProgressStyle::with_template("{spinner} {msg} [{bar:30}] {pos}/{len}")
            .expect("valid template")
            .progress_chars("=> "),
    );
    progress.set_message("Fetching packages");

    let packages: Vec<(RepoDataRecord, ArchivePackage)> = futures::stream::iter(records)
        .map(|record| {
            let (cache, client, progress) = (&cache, &client, &progress);
            async move {
                let archive = cache
                    .get_or_fetch(&record, client, None)
                    .await
                    .with_context(|| format!("could not download {}", record.url))?;
                // Reading the metadata of a `.tar.bz2` decompresses the whole archive.
                let package =
                    tokio::task::spawn_blocking(move || ArchivePackage::from_path(archive))
                        .await?
                        .with_context(|| format!("could not read {}", record.url))?;
                progress.inc(1);
                anyhow::Ok((record, package))
            }
        })
        .buffered(CONCURRENT_DOWNLOADS)
        .try_collect()
        .await?;
    progress.finish_and_clear();

    LinkPlan::new(packages, ArchiveLinkOptions::new(install_prefix, subdir))
        .context("could not plan the installation of the environment")
}
