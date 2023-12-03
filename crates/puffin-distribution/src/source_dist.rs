//! Fetch and build source distributions from remote sources.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::bail;
use fs_err::tokio as fs;
use futures::TryStreamExt;
use fxhash::FxHashMap;
use reqwest::Response;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use thiserror::Error;
use tokio::task::JoinError;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{debug, info_span};
use url::Url;
use zip::ZipArchive;

use distribution_filename::{WheelFilename, WheelFilenameError};
use distribution_types::direct_url::{DirectArchiveUrl, DirectGitUrl};
use distribution_types::{Dist, GitSourceDist, Identifier, RemoteSource, SourceDist};
use install_wheel_rs::find_dist_info;
use platform_tags::Tags;
use puffin_cache::{digest, CacheBucket, CacheEntry, CanonicalUrl, WheelCache};
use puffin_client::{CachedClient, CachedClientError, DataWithCachePolicy};
use puffin_git::{Fetch, GitSource};
use puffin_normalize::PackageName;
use puffin_traits::BuildContext;
use pypi_types::Metadata21;

use crate::download::BuiltWheel;
use crate::locks::LockedFile;
use crate::{Download, Reporter, SourceDistDownload};

/// The caller is responsible for adding the source dist information to the error chain
#[derive(Debug, Error)]
pub enum SourceDistError {
    // Network error
    #[error("Failed to parse URL: `{0}`")]
    UrlParse(String, #[source] url::ParseError),
    #[error("Git operation failed")]
    Git(#[source] anyhow::Error),
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Client(#[from] puffin_client::Error),

    // Cache writing error
    #[error("Failed to write to source dist cache")]
    Io(#[from] std::io::Error),
    #[error("Cache (de)serialization failed")]
    Serde(#[from] serde_json::Error),

    // Build error
    #[error("Failed to build {0}")]
    Build(Box<SourceDist>, #[source] anyhow::Error),
    #[error("Built wheel has an invalid filename")]
    WheelFilename(#[from] WheelFilenameError),
    #[error("Package metadata name `{metadata}` does not match given name `{given}`")]
    NameMismatch {
        given: PackageName,
        metadata: PackageName,
    },
    #[error("Failed to parse metadata from built wheel")]
    Metadata(#[from] crate::error::Error),

    /// Should not occur, i've only seen it when another task panicked
    #[error("The task executor is broken, did some other task panic?")]
    Join(#[from] JoinError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskFilenameAndMetadata {
    /// Relative, un-normalized wheel filename in the cache, which can be different than
    /// `WheelFilename::to_string`.
    disk_filename: String,
    metadata: Metadata21,
}

type Metadata21s = FxHashMap<WheelFilename, DiskFilenameAndMetadata>;

/// The information about the wheel we either just built or got from the cache
#[derive(Debug, Clone)]
pub struct BuiltWheelMetadata {
    pub path: PathBuf,
    pub filename: WheelFilename,
    pub metadata: Metadata21,
}

impl BuiltWheelMetadata {
    fn from_cached(
        filename: &WheelFilename,
        cached_data: &DiskFilenameAndMetadata,
        cache_entry: &CacheEntry,
    ) -> Self {
        Self {
            path: cache_entry.dir.join(&cached_data.disk_filename),
            filename: filename.clone(),
            metadata: cached_data.metadata.clone(),
        }
    }
}

/// Fetch and build a source distribution from a remote source, or from a local cache.
pub struct SourceDistCachedBuilder<'a, T: BuildContext> {
    build_context: &'a T,
    cached_client: &'a CachedClient,
    reporter: Option<Arc<dyn Reporter>>,
    tags: &'a Tags,
}

const METADATA_JSON: &str = "metadata.json";

impl<'a, T: BuildContext> SourceDistCachedBuilder<'a, T> {
    /// Initialize a [`SourceDistCachedBuilder`] from a [`BuildContext`].
    pub fn new(build_context: &'a T, cached_client: &'a CachedClient, tags: &'a Tags) -> Self {
        Self {
            build_context,
            reporter: None,
            cached_client,
            tags,
        }
    }

    /// Set the [`Reporter`] to use for this source distribution fetcher.
    #[must_use]
    pub fn with_reporter(self, reporter: Arc<dyn Reporter>) -> Self {
        Self {
            reporter: Some(reporter),
            ..self
        }
    }

    pub async fn download_and_build(
        &self,
        source_dist: &SourceDist,
    ) -> Result<BuiltWheelMetadata, SourceDistError> {
        let built_wheel_metadata = match &source_dist {
            SourceDist::DirectUrl(direct_url_source_dist) => {
                let filename = direct_url_source_dist
                    .filename()
                    .unwrap_or(direct_url_source_dist.url.path());
                let DirectArchiveUrl { url, subdirectory } =
                    DirectArchiveUrl::from(&direct_url_source_dist.url);

                self.url(
                    source_dist,
                    filename,
                    &url,
                    WheelCache::Url(&url),
                    subdirectory.as_deref(),
                )
                .await?
            }
            SourceDist::Registry(registry_source_dist) => {
                let url = Url::parse(&registry_source_dist.file.url).map_err(|err| {
                    SourceDistError::UrlParse(registry_source_dist.file.url.clone(), err)
                })?;
                self.url(
                    source_dist,
                    &registry_source_dist.file.filename,
                    &url,
                    WheelCache::Index(&registry_source_dist.index),
                    None,
                )
                .await?
            }
            SourceDist::Git(git_source_dist) => self.git(source_dist, git_source_dist).await?,
            SourceDist::Path(path_source_dist) => {
                // TODO(konstin): Change this when the built wheel naming scheme is fixed
                // See: https://github.com/astral-sh/puffin/issues/478
                let wheel_dir = self
                    .build_context
                    .cache()
                    .bucket(CacheBucket::BuiltWheels)
                    .join(source_dist.distribution_id());
                fs::create_dir_all(&wheel_dir).await?;

                // Build the wheel.
                let disk_filename = self
                    .build_context
                    .build_source(
                        &path_source_dist.path,
                        None,
                        &wheel_dir,
                        &path_source_dist.to_string(),
                    )
                    .await
                    .map_err(|err| SourceDistError::Build(Box::new(source_dist.clone()), err))?;

                // Read the metadata from the wheel.
                let filename = WheelFilename::from_str(&disk_filename)?;

                // TODO(konstin): Remove duplicated `.dist-info` read.
                // See: https://github.com/astral-sh/puffin/issues/484
                let metadata = BuiltWheel {
                    dist: Dist::Source(source_dist.clone()),
                    filename: filename.clone(),
                    path: wheel_dir.join(&disk_filename),
                }
                .read_dist_info()?;

                BuiltWheelMetadata {
                    path: wheel_dir.join(disk_filename),
                    filename,
                    metadata,
                }
            }
        };

        Ok(built_wheel_metadata)
    }

    #[allow(clippy::too_many_arguments)]
    async fn url<'data>(
        &self,
        source_dist: &'data SourceDist,
        filename: &'data str,
        url: &'data Url,
        cache_shard: WheelCache<'data>,
        subdirectory: Option<&'data Path>,
    ) -> Result<BuiltWheelMetadata, SourceDistError> {
        let cache_entry = self.build_context.cache().entry(
            CacheBucket::BuiltWheels,
            cache_shard.built_wheel_dir(filename),
            METADATA_JSON.to_string(),
        );

        let response_callback = |response| async {
            // New or changed source distribution, delete all built wheels
            if cache_entry.dir.exists() {
                debug!("Clearing built wheels and metadata for {source_dist}");
                fs::remove_dir_all(&cache_entry.dir).await?;
            }
            debug!("Downloading and building source distribution: {source_dist}");

            let task = self
                .reporter
                .as_ref()
                .map(|reporter| reporter.on_build_start(source_dist));
            let span =
                info_span!("download_source_dist", filename = filename, source_dist = %source_dist);
            let (temp_dir, sdist_file) = self.download_source_dist_url(response, filename).await?;
            drop(span);

            let download = SourceDistDownload {
                dist: source_dist.clone(),
                sdist_file: sdist_file.clone(),
                subdirectory: subdirectory.map(Path::to_path_buf),
            };

            if let Some(reporter) = self.reporter.as_ref() {
                reporter.on_download_progress(&Download::SourceDist(download.clone()));
            }

            let (disk_filename, wheel_filename, metadata) = self
                .build_source_dist(
                    &download.dist,
                    temp_dir,
                    &download.sdist_file,
                    download.subdirectory.as_deref(),
                    &cache_entry,
                )
                .await
                .map_err(|err| SourceDistError::Build(Box::new(source_dist.clone()), err))?;

            if let Some(task) = task {
                if let Some(reporter) = self.reporter.as_ref() {
                    reporter.on_build_complete(source_dist, task);
                }
            }

            let mut metadatas = Metadata21s::default();
            metadatas.insert(
                wheel_filename,
                DiskFilenameAndMetadata {
                    disk_filename,
                    metadata,
                },
            );
            Ok(metadatas)
        };
        let req = self.cached_client.uncached().get(url.clone()).build()?;
        let metadatas = self
            .cached_client
            .get_cached_with_callback(req, &cache_entry, response_callback)
            .await
            .map_err(|err| match err {
                CachedClientError::Callback(err) => err,
                CachedClientError::Client(err) => SourceDistError::Client(err),
            })?;

        if let Some((filename, cached_data)) = metadatas
            .iter()
            .find(|(filename, _metadata)| filename.is_compatible(self.tags))
        {
            return Ok(BuiltWheelMetadata::from_cached(
                filename,
                cached_data,
                &cache_entry,
            ));
        }

        // At this point, we're seeing cached metadata (fresh source dist) but the
        // wheel(s) we built previously are incompatible
        let task = self
            .reporter
            .as_ref()
            .map(|reporter| reporter.on_build_start(source_dist));
        let response = self
            .cached_client
            .uncached()
            .get(url.clone())
            .send()
            .await
            .map_err(puffin_client::Error::RequestMiddlewareError)?;
        let span =
            info_span!("download_source_dist", filename = filename, source_dist = %source_dist);
        let (temp_dir, sdist_file) = self.download_source_dist_url(response, filename).await?;
        drop(span);
        let (disk_filename, wheel_filename, metadata) = self
            .build_source_dist(
                source_dist,
                temp_dir,
                &sdist_file,
                subdirectory,
                &cache_entry,
            )
            .await
            .map_err(|err| SourceDistError::Build(Box::new(source_dist.clone()), err))?;
        if let Some(task) = task {
            if let Some(reporter) = self.reporter.as_ref() {
                reporter.on_build_complete(source_dist, task);
            }
        }

        let cached_data = DiskFilenameAndMetadata {
            disk_filename: disk_filename.clone(),
            metadata: metadata.clone(),
        };

        // Not elegant that we have to read again here, but also not too relevant given that we
        // have to build a source dist next.
        // Just return if the response wasn't cacheable or there was another errors that
        // `CachedClient` already complained about
        if let Ok(cached) = fs::read(cache_entry.path()).await {
            // If the file exists and it was just read or written by `CachedClient`, we assume it must
            // be correct.
            let mut cached = serde_json::from_slice::<DataWithCachePolicy<Metadata21s>>(&cached)?;

            cached
                .data
                .insert(wheel_filename.clone(), cached_data.clone());
            fs::write(cache_entry.path(), serde_json::to_vec(&cached)?).await?;
        };

        Ok(BuiltWheelMetadata::from_cached(
            &wheel_filename,
            &cached_data,
            &cache_entry,
        ))
    }

    async fn git(
        &self,
        source_dist: &SourceDist,
        git_source_dist: &GitSourceDist,
    ) -> Result<BuiltWheelMetadata, SourceDistError> {
        let (fetch, subdirectory) = self.download_source_dist_git(&git_source_dist.url).await?;

        // TODO(konstin): Do we want to delete old built wheels when the git sha changed?
        let git_sha = fetch
            .git()
            .precise()
            .expect("Exact commit after checkout")
            .to_string();
        let cache_shard = WheelCache::Git(&git_source_dist.url);
        let cache_entry = self.build_context.cache().entry(
            CacheBucket::BuiltWheels,
            cache_shard.built_wheel_dir(&git_sha),
            METADATA_JSON.to_string(),
        );

        let mut metadatas = if cache_entry.path().is_file() {
            let cached = fs::read(&cache_entry.path()).await?;
            let metadatas = serde_json::from_slice::<Metadata21s>(&cached)?;
            // Do we have previous compatible build of this source dist?
            if let Some((filename, cached_data)) = metadatas
                .iter()
                .find(|(filename, _metadata)| filename.is_compatible(self.tags))
            {
                return Ok(BuiltWheelMetadata::from_cached(
                    filename,
                    cached_data,
                    &cache_entry,
                ));
            }
            metadatas
        } else {
            Metadata21s::default()
        };

        let task = self
            .reporter
            .as_ref()
            .map(|reporter| reporter.on_build_start(source_dist));

        let (disk_filename, filename, metadata) = self
            .build_source_dist(
                source_dist,
                None,
                fetch.path(),
                subdirectory.as_deref(),
                &cache_entry,
            )
            .await
            .map_err(|err| SourceDistError::Build(Box::new(source_dist.clone()), err))?;

        if metadata.name != git_source_dist.name {
            return Err(SourceDistError::NameMismatch {
                metadata: metadata.name,
                given: git_source_dist.name.clone(),
            });
        }

        // Store the metadata for this build along with all the other builds
        metadatas.insert(
            filename.clone(),
            DiskFilenameAndMetadata {
                disk_filename: disk_filename.clone(),
                metadata: metadata.clone(),
            },
        );
        let cached = serde_json::to_vec(&metadatas)?;
        fs::create_dir_all(&cache_entry.dir).await?;
        fs::write(cache_entry.path(), cached).await?;

        if let Some(task) = task {
            if let Some(reporter) = self.reporter.as_ref() {
                reporter.on_build_complete(source_dist, task);
            }
        }

        Ok(BuiltWheelMetadata {
            path: cache_entry.dir.join(&disk_filename),
            filename,
            metadata,
        })
    }

    async fn download_source_dist_url(
        &self,
        response: Response,
        source_dist_filename: &str,
    ) -> Result<(Option<TempDir>, PathBuf), puffin_client::Error> {
        let reader = response
            .bytes_stream()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
            .into_async_read();
        let mut reader = tokio::io::BufReader::new(reader.compat());

        // Download the source distribution.
        let cache_dir = self.build_context.cache().bucket(CacheBucket::BuiltWheels);
        fs::create_dir_all(&cache_dir).await?;
        let temp_dir = tempfile::tempdir_in(cache_dir)?;
        let sdist_file = temp_dir.path().join(source_dist_filename);
        let mut writer = tokio::fs::File::create(&sdist_file).await?;
        tokio::io::copy(&mut reader, &mut writer).await?;
        Ok((Some(temp_dir), sdist_file))
    }

    async fn download_source_dist_git(
        &self,
        url: &Url,
    ) -> Result<(Fetch, Option<PathBuf>), SourceDistError> {
        debug!("Fetching source distribution from Git: {url}");
        let git_dir = self.build_context.cache().bucket(CacheBucket::Git);

        // Avoid races between different processes, too.
        let locks_dir = git_dir.join("locks");
        fs::create_dir_all(&locks_dir).await?;
        let _lockfile = LockedFile::new(locks_dir.join(digest(&CanonicalUrl::new(url))))?;

        let DirectGitUrl { url, subdirectory } =
            DirectGitUrl::try_from(url).map_err(SourceDistError::Git)?;

        let source = if let Some(reporter) = &self.reporter {
            GitSource::new(url, git_dir).with_reporter(Facade::from(reporter.clone()))
        } else {
            GitSource::new(url, git_dir)
        };
        let fetch = tokio::task::spawn_blocking(move || source.fetch())
            .await?
            .map_err(SourceDistError::Git)?;
        Ok((fetch, subdirectory))
    }

    /// Build a source distribution, storing the built wheel in the cache.
    ///
    /// Returns the un-normalized disk filename, the parsed, normalized filename and the metadata
    async fn build_source_dist(
        &self,
        dist: &SourceDist,
        temp_dir: Option<TempDir>,
        source_dist: &Path,
        subdirectory: Option<&Path>,
        cache_entry: &CacheEntry,
    ) -> anyhow::Result<(String, WheelFilename, Metadata21)> {
        debug!("Building: {dist}");

        if self.build_context.no_build() {
            bail!("Building source distributions is disabled");
        }

        // Build the wheel.
        fs::create_dir_all(&cache_entry.dir).await?;
        let disk_filename = self
            .build_context
            .build_source(
                source_dist,
                subdirectory,
                &cache_entry.dir,
                &dist.to_string(),
            )
            .await?;

        if let Some(temp_dir) = temp_dir {
            temp_dir.close()?;
        }

        // Read the metadata from the wheel.
        let filename = WheelFilename::from_str(&disk_filename)?;

        let mut archive =
            ZipArchive::new(fs_err::File::open(cache_entry.dir.join(&disk_filename))?)?;
        let dist_info_dir =
            find_dist_info(&filename, archive.file_names().map(|name| (name, name)))?.1;
        let dist_info =
            std::io::read_to_string(archive.by_name(&format!("{dist_info_dir}/METADATA"))?)?;
        let metadata = Metadata21::parse(dist_info.as_bytes())?;

        debug!("Finished building: {dist}");
        Ok((disk_filename, filename, metadata))
    }
}

trait SourceDistReporter: Send + Sync {
    /// Callback to invoke when a repository checkout begins.
    fn on_checkout_start(&self, url: &Url, rev: &str) -> usize;

    /// Callback to invoke when a repository checkout completes.
    fn on_checkout_complete(&self, url: &Url, rev: &str, index: usize);
}

/// A facade for converting from [`Reporter`] to [`puffin_git::Reporter`].
struct Facade {
    reporter: Arc<dyn Reporter>,
}

impl From<Arc<dyn Reporter>> for Facade {
    fn from(reporter: Arc<dyn Reporter>) -> Self {
        Self { reporter }
    }
}

impl puffin_git::Reporter for Facade {
    fn on_checkout_start(&self, url: &Url, rev: &str) -> usize {
        self.reporter.on_checkout_start(url, rev)
    }

    fn on_checkout_complete(&self, url: &Url, rev: &str, index: usize) {
        self.reporter.on_checkout_complete(url, rev, index);
    }
}