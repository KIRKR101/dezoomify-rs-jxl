use std::collections::HashMap;
use std::iter::once;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Mutex;

use bytes::Bytes;
use lazy_static::lazy_static;
use log::{debug, trace, warn};
use lru::LruCache;
use reqwest::{Client, header};
use sanitize_filename_reader_friendly::sanitize;
use tokio::fs;
use tokio::time::Duration;
use url::Url;

const METADATA_CACHE_CAPACITY: usize = 128;

lazy_static! {
    /// Bounded in-memory cache for small metadata files fetched during a run.
    /// This avoids re-downloading the same info.json when multiple canvases reference it.
    ///
    /// **Caveat for library users:** the cache is process-global and is **not**
    /// cleared between successive calls to `dezoomify_with_cancel` from the same
    /// process. Entries from prior inputs persist for the lifetime of the
    /// process, bounded to `METADATA_CACHE_CAPACITY` (~a few MB) by an LRU
    /// policy. For CLI use this is harmless; embedders that need strict
    /// isolation between calls should fork or run each session in a separate
    /// process. A future refactor may scope this cache to a session object.
    static ref METADATA_CACHE: Mutex<LruCache<String, Bytes>> =
        Mutex::new(LruCache::new(NonZeroUsize::new(METADATA_CACHE_CAPACITY).unwrap()));
}

fn lock_metadata_cache() -> std::sync::MutexGuard<'static, LruCache<String, Bytes>> {
    METADATA_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fetch a metadata URI, caching the result in memory for the lifetime of the process.
///
/// Note: the cache lock is released before the network call, so two concurrent
/// requests for the same URI can both miss and both download. The second caller
/// simply overwrites the first entry. This is a best-effort cache; avoiding the
/// race would require memoizing the in-flight future, which is left for a future
/// refactor if duplicate fetches become a problem in practice.
pub async fn fetch_metadata_uri(uri: &str, http: &Client) -> Result<Bytes, ZoomError> {
    {
        let mut cache = lock_metadata_cache();
        if let Some(bytes) = cache.get(uri) {
            debug!("Metadata cache hit for {uri}");
            return Ok(bytes.clone());
        }
    }
    let bytes = fetch_uri(uri, http).await?;
    lock_metadata_cache().put(uri.to_string(), bytes.clone());
    Ok(bytes)
}

use crate::arguments::Arguments;
use crate::binary_display::display_bytes;
use crate::dezoomer::{PostProcessFn, TileReference};
use crate::errors::BufferToImageError;
use crate::errors::{TileDownloadError, ZoomError};
use crate::tile::{Tile, load_image_with_metadata};

/// Fetch data, either from an URL or a path to a local file.
/// If uri doesnt start with "http(s)://", it is considered to be a path
/// to a local file
pub async fn fetch_uri(uri: &str, http: &Client) -> Result<Bytes, ZoomError> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        let req = http.get(uri).build()?;
        debug!(
            "Making http request to {uri} with headers '{:?}'",
            req.headers()
        );
        let response = http.execute(req).await?;
        debug!(
            "Got http response for {uri}: status={},  headers={:?}",
            response.status(),
            response.headers()
        );
        let response = response.error_for_status()?;
        let contents = response.bytes().await?;
        trace!(
            "Successfully finished loading url: '{}' - received {} bytes: {}",
            uri,
            contents.len(),
            display_bytes(&contents[..contents.len().min(256)])
        );
        Ok(contents)
    } else {
        debug!("Loading file: '{uri}'");
        let result = fs::read(uri).await?;
        debug!(
            "Loaded file: '{}' - {} bytes: {}",
            uri,
            result.len(),
            display_bytes(&result[..result.len().min(256)])
        );
        Ok(Bytes::from(result))
    }
}

pub struct TileDownloader {
    pub http_client: reqwest::Client,
    pub post_process_fn: PostProcessFn,
    pub retries: usize,
    pub retry_delay: Duration,
    pub tile_storage_folder: Option<PathBuf>,
}

impl TileDownloader {
    /// Download the raw bytes for a tile. Network requests and cache I/O happen here,
    /// but image decoding is left to the caller so that concurrency can be controlled
    /// independently.
    pub async fn download_tile_bytes(
        &self,
        tile_reference: TileReference,
    ) -> Result<(TileReference, Bytes), TileDownloadError> {
        // The initial delay after which a failed request is retried depends on the position of the tile
        // in order to avoid sending repeated "bursts" of requests to a server that is struggling
        let n = 100;
        let idx: f64 = ((tile_reference.position.x + tile_reference.position.y) % n).into();
        let mut wait_time = self.retry_delay
            + Duration::from_secs_f64(idx * self.retry_delay.as_secs_f64() / n as f64);
        let mut failures: usize = 0;
        loop {
            match self.download_bytes(&tile_reference).await {
                Ok(bytes) => {
                    return Ok((tile_reference, bytes));
                }
                Err(cause) => {
                    if failures >= self.retries {
                        return Err(TileDownloadError {
                            tile_reference,
                            cause,
                        });
                    }
                    failures += 1;
                    warn!("{cause}. Retrying tile download in {wait_time:?}.");
                    tokio::time::sleep(wait_time).await;
                    wait_time *= 2;
                }
            }
        }
    }

    async fn download_bytes(&self, tile_reference: &TileReference) -> Result<Bytes, ZoomError> {
        let bytes = if let Some(bytes) = self.read_from_tile_cache(&tile_reference.url).await {
            bytes
        } else {
            let bytes = fetch_uri(&tile_reference.url, &self.http_client).await?;
            self.write_to_tile_cache(&tile_reference.url, &bytes).await;
            bytes
        };

        if let PostProcessFn::Fn(post_process) = self.post_process_fn {
            let tile_reference = tile_reference.clone();
            Ok(
                tokio::task::spawn_blocking(move || -> Result<Bytes, BufferToImageError> {
                    post_process(&tile_reference, bytes.into())
                        .map(Bytes::from)
                        .map_err(|e| BufferToImageError::PostProcessing { e })
                })
                .await??,
            )
        } else {
            Ok(bytes)
        }
    }

    async fn write_to_tile_cache(&self, uri: &str, contents: &Bytes) {
        if let Some(root) = &self.tile_storage_folder {
            match tokio::fs::write(root.join(sanitize(uri)), contents).await {
                Ok(_) => debug!("Wrote {} to tile cache ({} bytes)", uri, contents.len()),
                Err(e) => warn!("Unable to write {uri} to the tile cache {root:?}: {e}"),
            }
        }
    }

    async fn read_from_tile_cache(&self, uri: &str) -> Option<Bytes> {
        if let Some(root) = &self.tile_storage_folder {
            match tokio::fs::read(root.join(sanitize(uri))).await {
                Ok(d) => {
                    debug!("{uri} read from tile cache");
                    return Some(Bytes::from(d));
                }
                Err(e) => debug!("Unable to open {uri} from tile cache {root:?}: {e}"),
            }
        }
        None
    }
}

/// Decode downloaded tile bytes into a `Tile` (image + metadata).
/// This is CPU-bound and is intended to be run inside `spawn_blocking`.
pub fn decode_tile_bytes(
    tile_reference: TileReference,
    bytes: Bytes,
) -> Result<Tile, BufferToImageError> {
    let position = tile_reference.position;
    let image_with_metadata =
        load_image_with_metadata(&bytes).map_err(|source| BufferToImageError::Image { source })?;
    Ok(Tile::builder()
        .with_image(image_with_metadata.image)
        .at_position(position)
        .with_optional_icc_profile(image_with_metadata.icc_profile)
        .with_optional_exif_metadata(image_with_metadata.exif_metadata)
        .build())
}

pub fn client<'a, I: Iterator<Item = (&'a String, &'a String)>>(
    headers: I,
    args: &Arguments,
    uri: Option<&str>,
) -> Result<reqwest::Client, ZoomError> {
    let referer = uri.or(args.request_referer()).unwrap_or("");
    let header_map = default_headers()
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain(once(("Referer", referer)))
        .chain(headers.map(|(k, v)| (&**k, &**v)))
        .map(|(name, value)| Ok((name.parse()?, value.parse()?)))
        .collect::<Result<header::HeaderMap, ZoomError>>()?;
    debug!("Creating an http client with the following headers: {header_map:?}");
    let client = reqwest::Client::builder()
        .http1_title_case_headers()
        .default_headers(header_map)
        .referer(false)
        .pool_max_idle_per_host(args.max_idle_per_host)
        .danger_accept_invalid_certs(args.accept_invalid_certs)
        .timeout(args.timeout)
        .build()?;
    Ok(client)
}

pub fn default_headers() -> HashMap<String, String> {
    serde_yml::from_str(include_str!("default_headers.yaml")).unwrap()
}

pub fn resolve_relative(base: &str, path: &str) -> String {
    if Url::parse(path).is_ok() {
        return path.to_string();
    } else if let Ok(url) = Url::parse(base)
        && let Ok(r) = url.join(path)
    {
        return r.to_string();
    }
    let mut res = PathBuf::from(base.rsplitn(2, '/').last().unwrap_or_default());
    res.push(path);
    res.to_string_lossy().to_string()
}

#[test]
fn test_resolve_relative() {
    use std::path::MAIN_SEPARATOR;
    assert_eq!(
        resolve_relative("/a/b", "c/d"),
        format!("/a{}c/d", MAIN_SEPARATOR)
    );
    assert_eq!(
        resolve_relative("C:\\\\X", "c/d"),
        format!("C:\\\\X{}c/d", MAIN_SEPARATOR)
    );
    assert_eq!(
        resolve_relative("/a/b", "http://example.com/x"),
        "http://example.com/x"
    );
    assert_eq!(
        resolve_relative("http://a.b", "http://example.com/x"),
        "http://example.com/x"
    );
    assert_eq!(resolve_relative("http://a.b", "c/d"), "http://a.b/c/d");
    assert_eq!(resolve_relative("http://a.b/x", "c/d"), "http://a.b/c/d");
    assert_eq!(resolve_relative("http://a.b/x/", "c/d"), "http://a.b/x/c/d");
}
