// download_state.rs
use crate::arguments::Arguments;
use crate::dezoomer::{TileFetchResult, TileReference, ZoomLevel, ZoomLevelIter};
use crate::encoder::tile_buffer::TileBuffer;
use crate::errors::{self, ZoomError}; // `self` imports the errors module itself
use std::sync::Arc;

use crate::max_size_in_rect;
use crate::network::{TileDownloader, client as network_client, decode_tile_bytes};
use crate::throttler::Throttler;
use crate::tile::Tile;
use crate::vec2d::Vec2d; // This is a public function from lib.rs

use futures::stream::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::debug;
use std::default::Default;
use tokio_util::sync::CancellationToken;

// --- DownloadState ---
#[derive(Debug, Default)]
pub(crate) struct DownloadState {
    pub(crate) total_tiles: u64,
    pub(crate) successful_tiles: u64,
    pub(crate) last_batch_count: u64,
    pub(crate) last_batch_successes: u64,
    tile_size: Option<Vec2d>,
}

impl DownloadState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn add_batch(&mut self, count: u64) {
        self.last_batch_count = count;
        self.total_tiles += count;
        self.last_batch_successes = 0;
    }

    pub(crate) fn record_success(&mut self) {
        self.last_batch_successes += 1;
        self.successful_tiles += 1;
    }

    pub(crate) fn create_fetch_result(&self) -> TileFetchResult {
        TileFetchResult {
            count: self.last_batch_count,
            successes: self.last_batch_successes,
            tile_size: self.tile_size,
        }
    }

    pub(crate) fn is_successful(&self) -> bool {
        self.successful_tiles > 0
    }

    pub(crate) fn has_partial_failure(&self) -> bool {
        self.successful_tiles < self.total_tiles
    }
}

// --- ProgressManager ---
#[derive(Debug)]
pub(crate) struct ProgressManager {
    progress: ProgressBar,
}

impl ProgressManager {
    pub(crate) fn new() -> Self {
        let progress = progress_bar(10); // Default initial size, will be updated
        if !log::log_enabled!(log::Level::Info) {
            progress.set_draw_target(ProgressDrawTarget::hidden());
        }
        Self { progress }
    }

    /// Create a progress bar that is part of a `MultiProgress` group. The
    /// `MultiProgress` is responsible for drawing, so concurrent bars in a
    /// bulk run stack vertically instead of overwriting each other on stderr.
    pub(crate) fn new_in_multi(mp: &MultiProgress) -> Self {
        let progress = mp.add(progress_bar(10));
        if !log::log_enabled!(log::Level::Info) {
            progress.set_draw_target(ProgressDrawTarget::hidden());
        }
        Self { progress }
    }

    pub(crate) fn set_total_tiles(&self, total: u64) {
        self.progress.set_length(total);
    }

    pub(crate) fn set_computing_urls(&self) {
        self.progress
            .set_message("Computing the URLs of the image tiles...");
    }

    pub(crate) fn set_requesting_tiles(&self) {
        self.progress.set_message("Requesting the tiles...");
    }

    pub(crate) fn set_finalizing(&self) {
        self.progress
            .set_message("Downloaded all tiles. Finalizing the image file.");
    }

    pub(crate) fn increment(&self) {
        self.progress.inc(1);
    }

    pub(crate) fn update_for_tile(&self, tile: &Option<Tile>, success: bool) {
        if success {
            if let Some(tile) = tile {
                self.progress
                    .set_message(format!("Loaded tile at {}", tile.position()));
            }
        } else {
            self.progress
                .set_message("Failed to load tile, using empty replacement");
        }
    }

    pub(crate) fn finish(&self) {
        self.progress.finish_with_message("Finished tile download");
    }
}

// Helper function, private to this module
fn progress_bar(n: usize) -> ProgressBar {
    let progress = ProgressBar::new(n as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("[ETA:{eta}] {bar:40.cyan/blue} {pos:>4}/{len:4} {msg}")
            .expect("Invalid indicatif progress bar template")
            .progress_chars("##-"),
    );
    progress
}

// --- TileDownloadCoordinator ---
// Not deriving Debug because Throttler doesn't derive Debug
pub(crate) struct TileDownloadCoordinator<'a> {
    downloader: TileDownloader,
    throttler: tokio::sync::Mutex<Throttler>,
    decode_semaphore: Arc<tokio::sync::Semaphore>,
    args: &'a Arguments,
    cancel: CancellationToken,
}

impl<'a> TileDownloadCoordinator<'a> {
    pub(crate) fn new(
        zoom_level: &ZoomLevel,
        args: &'a Arguments,
        cancel: CancellationToken,
    ) -> Result<Self, ZoomError> {
        let downloader = create_tile_downloader(zoom_level, args)?;
        let throttler = tokio::sync::Mutex::new(Throttler::new(args.min_interval));
        // Decode is CPU-bound; allow one decoder per CPU core so we don't starve the Tokio runtime.
        let decode_permits = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(args.parallelism)
            .max(1);

        Ok(Self {
            downloader,
            throttler,
            decode_semaphore: Arc::new(tokio::sync::Semaphore::new(decode_permits)),
            args,
            cancel,
        })
    }

    async fn download_one_tile(
        &self,
        tile_ref: TileReference,
    ) -> Result<Tile, errors::TileDownloadError> {
        if self.cancel.is_cancelled() {
            return Err(errors::TileDownloadError {
                tile_reference: tile_ref,
                cause: ZoomError::Cancelled,
            });
        }

        // Throttle the *start* of the request, not its completion.
        let mut throttler = self.throttler.lock().await;
        tokio::select! {
            _ = throttler.wait() => {}
            _ = self.cancel.cancelled() => {
                return Err(errors::TileDownloadError {
                    tile_reference: tile_ref,
                    cause: ZoomError::Cancelled,
                });
            }
        }
        drop(throttler);

        if self.cancel.is_cancelled() {
            return Err(errors::TileDownloadError {
                tile_reference: tile_ref,
                cause: ZoomError::Cancelled,
            });
        }

        let tile_ref_for_error = tile_ref.clone();
        let (tile_ref, bytes) = tokio::select! {
            result = self.downloader.download_tile_bytes(tile_ref) => result?,
            _ = self.cancel.cancelled() => {
                return Err(errors::TileDownloadError {
                    tile_reference: tile_ref_for_error,
                    cause: ZoomError::Cancelled,
                });
            }
        };
        let permit = self
            .decode_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("decode semaphore should never be closed");
        let decode_result = tokio::task::spawn_blocking(move || {
            let result = decode_tile_bytes(tile_ref, bytes);
            drop(permit);
            result
        })
        .await;
        match decode_result {
            Ok(Ok(tile)) => Ok(tile),
            Ok(Err(e)) => Err(errors::TileDownloadError {
                tile_reference: tile_ref_for_error,
                cause: e.into(),
            }),
            Err(e) => Err(errors::TileDownloadError {
                tile_reference: tile_ref_for_error,
                cause: ZoomError::AsyncError { source: e },
            }),
        }
    }

    pub(crate) async fn download_batch(
        &self,
        tile_refs: Vec<TileReference>,
        canvas: &mut TileBuffer,
        state: &mut DownloadState,
        progress: &ProgressManager,
        zoom_level_iter: &ZoomLevelIter<'_>,
    ) -> Result<(), ZoomError> {
        if self.cancel.is_cancelled() {
            return Err(ZoomError::Cancelled);
        }

        state.add_batch(tile_refs.len() as u64);
        progress.set_total_tiles(state.total_tiles); // Update progress bar length with cumulative total
        progress.set_requesting_tiles();

        prepare_canvas_size(canvas, zoom_level_iter).await?;

        let parallelism = self.args.parallelism;
        let mut stream = futures::stream::iter(tile_refs)
            .map(|tile_ref: TileReference| self.download_one_tile(tile_ref))
            .buffer_unordered(parallelism);

        while let Some(tile_result) = stream.next().await {
            if self.cancel.is_cancelled() {
                return Err(ZoomError::Cancelled);
            }
            debug!("Received tile result: {:?}", tile_result); // Tile and TileDownloadError need Debug
            progress.increment();

            let (tile, success) = process_tile_result(
                tile_result,
                &mut state.tile_size,
                zoom_level_iter.size_hint(),
            );

            progress.update_for_tile(&tile, success);

            if success {
                state.record_success();
            }

            if let Some(tile) = tile {
                canvas.add_tile(tile).await?;
            }
        }
        Ok(())
    }
}

// Helper function, private to this module
fn create_tile_downloader(
    zoom_level: &ZoomLevel,
    args: &Arguments,
) -> Result<TileDownloader, ZoomError> {
    let level_headers = zoom_level.http_headers();
    Ok(TileDownloader {
        http_client: network_client(level_headers.iter().chain(args.headers()), args, None)?,
        post_process_fn: zoom_level.post_process_fn(),
        retries: args.retries,
        retry_delay: args.retry_delay,
        tile_storage_folder: args.tile_storage_folder.clone(),
    })
}

// Helper function, private to this module
async fn prepare_canvas_size(
    canvas: &mut TileBuffer,
    zoom_level_iter: &ZoomLevelIter<'_>,
) -> Result<(), ZoomError> {
    if let Some(size) = zoom_level_iter.size_hint() {
        canvas.set_size(size).await?;
    }
    Ok(())
}

// Helper function, private to this module
fn process_tile_result(
    tile_result: Result<Tile, errors::TileDownloadError>,
    tile_size: &mut Option<Vec2d>,
    canvas_size: Option<Vec2d>,
) -> (Option<Tile>, bool) {
    match tile_result {
        Ok(tile) => {
            // Track the largest tile seen so far. Edge tiles clipped to the canvas boundary are
            // smaller than the nominal tile size, so we must not let them shrink empty replacements.
            let observed = tile.size();
            *tile_size = Some(tile_size.map(|s| s.max(observed)).unwrap_or(observed));
            (Some(tile), true)
        }
        Err(err) => {
            let position = err.tile_reference.position;
            // Try to create an empty tile only if we know the expected tile_size and canvas_size
            let empty_tile = match (*tile_size, canvas_size) {
                (Some(current_tile_size), Some(current_canvas_size)) => {
                    let size = max_size_in_rect(position, current_tile_size, current_canvas_size);
                    Some(Tile::empty(position, size))
                }
                _ => None, // Not enough info to create a correctly sized empty tile
            };
            (empty_tile, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadState, ProgressManager, process_tile_result}; // From the parent module 'download_state'
    use crate::dezoomer::TileReference;
    use crate::errors::{TileDownloadError, ZoomError};
    use crate::max_size_in_rect;
    use crate::tile::Tile;
    use crate::vec2d::Vec2d; // Used by process_tile_result, ensure it's in scope for understanding test logic

    #[test]
    fn test_process_tile_result() {
        let mut tile_size: Option<Vec2d> = None;
        let canvas_size = Vec2d { x: 1000, y: 1000 };

        // Test successful tile result
        let tile_to_test = Tile::empty(Vec2d { x: 0, y: 0 }, Vec2d { x: 256, y: 256 });
        let ok_result: Result<Tile, TileDownloadError> = Ok(tile_to_test.clone());
        let (result_tile_opt, success) =
            process_tile_result(ok_result, &mut tile_size, Some(canvas_size));

        assert!(success, "Tile processing should succeed for Ok result");
        assert!(
            result_tile_opt.is_some(),
            "Result tile should be Some for Ok result"
        );
        if let Some(ref result_tile) = result_tile_opt {
            assert_eq!(
                result_tile.size(),
                Vec2d { x: 256, y: 256 },
                "Result tile size mismatch"
            );
        }
        assert_eq!(
            tile_size,
            Some(Vec2d { x: 256, y: 256 }),
            "tile_size variable mismatch after success"
        );

        // Test failed tile result
        // process_tile_result will use the current value of tile_size (if Some) to determine the size of the empty tile.
        // So, we set it to what a previously successful tile might have set.
        tile_size = Some(Vec2d { x: 256, y: 256 });

        let tile_ref = TileReference {
            url: "http://example.com/tile.jpg".to_string(),
            position: Vec2d { x: 100, y: 100 },
        };
        let error = TileDownloadError {
            tile_reference: tile_ref.clone(), // Clone if tile_ref is used later, or ensure it's not.
            cause: ZoomError::NoLevels,       // Using an arbitrary ZoomError variant
        };
        let err_result: Result<Tile, TileDownloadError> = Err(error);
        let (result_tile_opt_err, success_err) =
            process_tile_result(err_result, &mut tile_size, Some(canvas_size));

        assert!(!success_err, "Tile processing should fail for Err result");
        assert!(
            result_tile_opt_err.is_some(),
            "Result tile should be Some (empty tile) for Err result"
        );
        if let Some(ref empty_tile) = result_tile_opt_err {
            // The empty tile's size is determined by max_size_in_rect.
            // Given position (100,100), tile_size (256,256), canvas_size (1000,1000),
            // max_size_in_rect should return (256,256) as it fits.
            let expected_empty_size =
                max_size_in_rect(tile_ref.position, tile_size.unwrap(), canvas_size);
            assert_eq!(
                empty_tile.size(),
                expected_empty_size,
                "Empty tile size mismatch"
            );
            assert_eq!(
                empty_tile.position(),
                tile_ref.position,
                "Empty tile position mismatch"
            );
        }
        // tile_size should remain Some(Vec2d { x: 256, y: 256 }) as per logic in process_tile_result for Err case.
        assert_eq!(
            tile_size,
            Some(Vec2d { x: 256, y: 256 }),
            "tile_size variable mismatch after failure"
        );
    }

    #[test]
    fn test_tile_size_tracks_maximum() {
        let mut tile_size: Option<Vec2d> = None;
        let canvas_size = Vec2d { x: 1000, y: 1000 };

        // First tile is a clipped edge tile smaller than the nominal size.
        let small_tile = Tile::empty(Vec2d { x: 900, y: 900 }, Vec2d { x: 100, y: 100 });
        process_tile_result(Ok(small_tile), &mut tile_size, Some(canvas_size));
        assert_eq!(tile_size, Some(Vec2d { x: 100, y: 100 }));

        // A later full-sized tile should raise the tracked size.
        let full_tile = Tile::empty(Vec2d { x: 0, y: 0 }, Vec2d { x: 256, y: 256 });
        process_tile_result(Ok(full_tile), &mut tile_size, Some(canvas_size));
        assert_eq!(tile_size, Some(Vec2d { x: 256, y: 256 }));
    }

    #[test]
    fn test_has_partial_failure_uses_cumulative_counts() {
        let mut state = DownloadState::new();
        assert!(!state.has_partial_failure());

        state.add_batch(10);
        for _ in 0..10 {
            state.record_success();
        }
        assert!(!state.has_partial_failure());

        state.add_batch(10);
        for _ in 0..5 {
            state.record_success();
        }
        // The latest batch has failures, but an earlier batch succeeded completely.
        // The old bug reported no partial failure because it only checked the last batch.
        assert!(state.has_partial_failure());
        assert_eq!(state.successful_tiles, 15);
        assert_eq!(state.total_tiles, 20);
    }

    #[test]
    fn progress_manager_in_multi_is_configurable() {
        // A bar created via `new_in_multi` must be a child of the
        // `MultiProgress` and accept length updates without panicking.
        let mp = indicatif::MultiProgress::new();
        let manager = ProgressManager::new_in_multi(&mp);
        manager.set_total_tiles(42);
        manager.finish();
        // `mp.is_hidden()` reflects the current draw target (auto-hides
        // when stderr is not a tty, which is the case in `cargo test`),
        // so we do not assert on it here.
    }
}
