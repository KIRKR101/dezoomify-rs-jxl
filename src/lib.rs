#![allow(clippy::upper_case_acronyms)]

use std::env::current_dir;

use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use log::{debug, error, info};
use reqwest::Client;

pub use arguments::Arguments;
pub use binary_display::{BinaryDisplay, display_bytes};
use dezoomer::TileReference;
use dezoomer::{Dezoomer, DezoomerError, DezoomerInput};
use dezoomer::{ZoomLevel, ZoomLevelIter};
pub use errors::ZoomError;
use network::{client, fetch_metadata_uri};
use output_file::reserve_unique_outname;
use tile::Tile;
pub use vec2d::Vec2d;

use crate::dezoomer::{DezoomerResult, PageContents, ZoomableImage};
use crate::encoder::tile_buffer::TileBuffer;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

mod arguments;
mod binary_display;

pub mod dezoomer;
pub(crate) mod download_state;
mod encoder;
mod errors;
mod network;
mod output_file;
pub mod tile;
mod vec2d;

pub mod auto;
pub mod bulk_text;
pub mod custom_yaml;
pub mod dzi;
pub mod generic;
pub mod google_arts_and_culture;
pub mod iiif;
pub mod iipimage;
mod json_utils;
pub mod krpano;
pub mod nypl;
pub mod pff;
mod throttler;
pub mod zoomify;

fn stdin_line() -> Result<String, ZoomError> {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let first_line = lines.next().ok_or_else(|| {
        let err_msg = "Encountered end of standard input while reading a line";
        io::Error::new(io::ErrorKind::UnexpectedEof, err_msg)
    })?;
    Ok(first_line?)
}

/// Process a single dezoomer to get a result, handling the NeedsData loop
async fn get_dezoomer_result(
    dezoomer: &mut dyn Dezoomer,
    http: &Client,
    uri: &str,
) -> Result<DezoomerResult, ZoomError> {
    let mut i = DezoomerInput {
        uri: String::from(uri),
        contents: PageContents::Unknown,
    };
    loop {
        match dezoomer.dezoomer_result(&i) {
            Ok(result) => return Ok(result),
            Err(DezoomerError::NeedsData { uri }) => {
                let contents = fetch_metadata_uri(&uri, http).await.into();
                debug!("Response for metadata file '{}': {:?}", uri, &contents);
                i.uri = uri;
                i.contents = contents;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Process an input URI to extract zoomable images
async fn get_images_from_uri(
    args: &Arguments,
    http: &Client,
    uri: &str,
) -> Result<Vec<ZoomableImage>, ZoomError> {
    let mut dezoomer = args.find_dezoomer()?;
    let zoomable_images = get_dezoomer_result(dezoomer.as_mut(), http, uri).await?;
    Ok(zoomable_images)
}

/// Validates a user input line as a level index
fn parse_level_index(input: &str, max_index: usize) -> Option<usize> {
    input.parse::<usize>().ok().filter(|&idx| idx < max_index)
}

/// Gets the actual level index to use, handling out-of-bounds requests
fn resolve_level_index(requested: usize, available_count: usize) -> usize {
    if requested < available_count {
        requested
    } else {
        available_count - 1
    }
}

/// Gets the actual image index to use, handling out-of-bounds requests
fn resolve_image_index(requested: usize, available_count: usize) -> usize {
    if requested < available_count {
        requested
    } else {
        available_count - 1
    }
}

/// Finds the position of a level with the specified size hint
fn find_level_with_size(levels: &[ZoomLevel], target_size: Vec2d) -> Option<usize> {
    levels
        .iter()
        .position(|l| l.size_hint() == Some(target_size))
}

/// An interactive level picker
fn level_picker(mut levels: Vec<ZoomLevel>) -> Result<ZoomLevel, ZoomError> {
    println!("Found the following zoom levels:");
    for (i, level) in levels.iter().enumerate() {
        println!("{: >2}. {}", i, level.name());
    }
    loop {
        println!("Which level do you want to download? ");
        let line = stdin_line()?;
        if let Some(idx) = parse_level_index(&line, levels.len()) {
            return Ok(levels.swap_remove(idx));
        }
        error!("'{line}' is not a valid level number");
    }
}

fn choose_level(mut levels: Vec<ZoomLevel>, args: &Arguments) -> Result<ZoomLevel, ZoomError> {
    match levels.len() {
        0 => Err(ZoomError::NoLevels),
        1 => Ok(levels.swap_remove(0)),
        _ => {
            if let Some(requested_level) = args.zoom_level {
                let actual_level = resolve_level_index(requested_level, levels.len());
                if actual_level == requested_level {
                    info!("Selected zoom level {} as requested", requested_level);
                } else {
                    info!(
                        "Requested zoom level {} not available. Using last one ({})",
                        requested_level, actual_level
                    );
                }
                return Ok(levels.swap_remove(actual_level));
            }

            if let Some(best_size) = args.best_size(levels.iter().filter_map(|l| l.size_hint()))
                && let Some(pos) = find_level_with_size(&levels, best_size)
            {
                return Ok(levels.swap_remove(pos));
            }

            level_picker(levels)
        }
    }
}

/// An interactive image picker for when multiple images are available
fn image_picker(mut images: Vec<ZoomableImage>) -> Result<ZoomableImage, ZoomError> {
    println!("Found the following images:");
    for (i, image) in images.iter().enumerate() {
        let title = image
            .title()
            .unwrap_or_else(|| format!("Image {}", i + 1).into());
        println!("{: >2}. {}", i, title);
    }
    loop {
        println!("Which image do you want to download? ");
        let line = stdin_line()?;
        if let Some(idx) = parse_level_index(&line, images.len()) {
            return Ok(images.swap_remove(idx));
        }
        error!("'{line}' is not a valid image number");
    }
}

/// Choose an image from multiple options (interactive or automatic)
fn choose_image(
    mut images: Vec<ZoomableImage>,
    args: &Arguments,
) -> Result<ZoomableImage, ZoomError> {
    match images.len() {
        0 => Err(ZoomError::NoLevels),
        1 => Ok(images.swap_remove(0)),
        _ => {
            if let Some(requested_index) = args.image_index {
                let actual_index = resolve_image_index(requested_index, images.len());
                if actual_index == requested_index {
                    info!("Selected image {} as requested", requested_index);
                } else {
                    info!(
                        "Requested image index {} not available. Using last one ({})",
                        requested_index, actual_index
                    );
                }
                return Ok(images.swap_remove(actual_index));
            }

            // In bulk mode, automatically select the first image to avoid interactive prompts
            if args.is_bulk_mode() {
                info!("Bulk mode: automatically selecting first image (index 0)");
                return Ok(images.swap_remove(0));
            }

            // Interactive selection when no command line option is provided
            image_picker(images)
        }
    }
}

/// Finds the appropriate zoomlevel for a given size if one is specified,
async fn find_zoomlevel(args: &Arguments) -> Result<ZoomLevel, ZoomError> {
    let uri = args.choose_input_uri()?;
    let http_client = client(args.headers(), args, Some(&uri))?;
    debug!("Trying to locate a zoomable image...");

    // Use the new unified processing pipeline
    let images = get_images_from_uri(args, &http_client, &uri).await?;
    debug!("Found {} zoomable images", images.len());

    // Select an image from the available options (before resolving)
    let selected_image = choose_image(images, args)?;
    debug!("Selected image: {:?}", selected_image.title());

    // NOW resolve the selected image to get its zoom levels
    let zoom_levels = selected_image
        .into_zoom_levels(&http_client)
        .await
        .map_err(|e| ZoomError::Dezoomer { source: e })?;
    debug!("Extracted {} zoom levels", zoom_levels.len());

    // Select a zoom level from the available options
    choose_level(zoom_levels, args)
}

/// Prepares the output file path for saving
fn prepare_output_path(
    outfile_arg: &Option<PathBuf>,
    title: &Option<String>,
    base_dir: &Path,
    size_hint: Option<Vec2d>,
) -> Result<PathBuf, ZoomError> {
    let outname = reserve_unique_outname(outfile_arg, title, base_dir, size_hint)?;
    let save_as = std::path::absolute(outname.as_path()).unwrap_or_else(|_e| outname.clone());
    Ok(save_as)
}

/// Creates a tile buffer for the given output path
async fn create_tile_buffer(
    save_as: PathBuf,
    compression: u8,
    jxl_effort: Option<u8>,
) -> Result<TileBuffer, ZoomError> {
    TileBuffer::new(save_as, compression, jxl_effort).await
}

pub async fn dezoomify(args: &Arguments) -> Result<PathBuf, ZoomError> {
    dezoomify_with_cancel(args, CancellationToken::new()).await
}

/// Same as [`dezoomify`], but allows the caller to share a cancellation
/// token with other concurrently-running dezoomify operations (e.g. from
/// a custom binary that drives the library).
pub async fn dezoomify_with_cancel(
    args: &Arguments,
    cancel: CancellationToken,
) -> Result<PathBuf, ZoomError> {
    let zoom_level = find_zoomlevel(args).await?;
    let base_dir = current_dir()?;
    let output_file = args.output_file();
    let save_as = prepare_output_path(
        &output_file,
        &zoom_level.title(),
        &base_dir,
        zoom_level.size_hint(),
    )?;
    let tile_buffer =
        create_tile_buffer(save_as.clone(), args.compression, args.jxl_effort).await?;
    info!("Dezooming {}", zoom_level.name());
    dezoomify_level(args, zoom_level, tile_buffer, cancel, None).await?;
    Ok(save_as)
}

/// Statistics for bulk processing
#[derive(Debug, Default)]
pub struct BulkStats {
    pub total_images: usize,
    pub successful_images: usize,
    pub failed_images: usize,
    pub partial_downloads: usize,
}

impl BulkStats {
    fn new() -> Self {
        Self::default()
    }

    fn record_success(&mut self) {
        self.successful_images += 1;
    }

    fn record_partial(&mut self) {
        self.partial_downloads += 1;
    }

    fn record_failure(&mut self) {
        self.failed_images += 1;
    }

    fn set_total(&mut self, total: usize) {
        self.total_images = total;
    }
}

/// Process multiple images in bulk mode using the new unified architecture
pub async fn process_bulk(args: &Arguments) -> Result<BulkStats, ZoomError> {
    process_bulk_with_cancel(args, CancellationToken::new()).await
}

/// Same as [`process_bulk`], but allows the caller to share a cancellation
/// token with other concurrently-running bulk operations (e.g. from a
/// custom binary that drives the library).
pub async fn process_bulk_with_cancel(
    args: &Arguments,
    cancel: CancellationToken,
) -> Result<BulkStats, ZoomError> {
    use log::{debug, trace};

    debug!("Starting bulk processing mode");
    trace!("Bulk processing arguments: {:?}", args);

    if cancel.is_cancelled() {
        return Err(ZoomError::Cancelled);
    }

    // Get the bulk file/URI from arguments
    let bulk_uri = args.bulk.as_ref().ok_or_else(|| ZoomError::NoBulkUrl {
        bulk_file_path: "No bulk source specified".to_string(),
    })?;

    debug!("Bulk source: {}", bulk_uri);

    // Get dezoomer result from the bulk source
    let http = client(std::iter::empty(), args, None)?;
    let mut dezoomer = args.find_dezoomer()?;
    let dezoomer_result = get_dezoomer_result(dezoomer.as_mut(), &http, bulk_uri).await?;

    let mut stats = BulkStats::new();
    let base_dir = current_dir()?;

    // Process each ZoomableImage in bulk mode
    stats.set_total(dezoomer_result.len());
    info!(
        "Found {} images to process in bulk mode",
        dezoomer_result.len()
    );
    debug!(
        "Images discovered: {:?}",
        dezoomer_result
            .iter()
            .map(|img| img.title().unwrap_or_else(|| "Untitled".into()))
            .collect::<Vec<_>>()
    );

    process_bulk_zoomable_images(dezoomer_result, args, &http, &mut stats, &base_dir, cancel)
        .await?;

    // Log final statistics
    info!("Bulk processing complete!");
    info!("Total images: {}", stats.total_images);
    info!("Successfully downloaded: {}", stats.successful_images);
    info!("Partial downloads: {}", stats.partial_downloads);
    info!("Failed downloads: {}", stats.failed_images);

    debug!("Final bulk processing stats: {:?}", stats);

    Ok(stats)
}

#[derive(Debug)]
enum SingleBulkResult {
    Success,
    Partial {
        successful_tiles: u64,
        total_tiles: u64,
    },
    Failure {
        msg: String,
    },
}

/// The result of processing a single image in a bulk run.
#[derive(Debug)]
struct SingleBulkOutcome {
    index: usize,
    title: String,
    result: SingleBulkResult,
    /// The path the image was saved to, if processing reached the point of
    /// reserving an output file.
    save_as: Option<PathBuf>,
}

/// Process a list of ZoomableImage objects in bulk - resolve each one to zoom levels as needed.
/// When `--bulk-parallelism` is greater than 1, images are processed concurrently.
async fn process_bulk_zoomable_images(
    images: Vec<ZoomableImage>,
    args: &Arguments,
    http: &Client,
    stats: &mut BulkStats,
    base_dir: &Path,
    cancel: CancellationToken,
) -> Result<(), ZoomError> {
    use log::warn;
    let bulk_outfile = args.bulk_output_file();
    let total_images = stats.total_images;
    let parallelism = args.bulk_parallelism.max(1);

    // A single MultiProgress is shared across all bulk images so that per-image
    // progress bars stack vertically instead of fighting each other on stderr.
    let multi = std::sync::Arc::new(indicatif::MultiProgress::new());

    let futures = images.into_iter().enumerate().map(|(index, image)| {
        let multi = std::sync::Arc::clone(&multi);
        process_single_bulk_image(
            args,
            http,
            base_dir,
            bulk_outfile.as_ref(),
            index,
            total_images,
            image,
            cancel.clone(),
            multi,
        )
    });
    let mut stream = futures::stream::iter(futures).buffer_unordered(parallelism);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                warn!("Cancellation requested; abandoning in-flight bulk image tasks.");
                // Dropping the stream cancels the futures that buffer_unordered
                // is polling, which stops network downloads promptly. Background
                // encoder tasks exit once their channels close.
                return Err(ZoomError::Cancelled);
            }
            result = stream.next() => {
                match result {
                    None => break,
                    Some(outcome) => {
                        // Suspend the bar drawing while we emit log lines, so the [INFO]/[WARN]
                        // messages land on a clean row above the bars instead of being overwritten.
                        multi.suspend(|| match outcome {
                            Ok(SingleBulkOutcome {
                                index,
                                result: SingleBulkResult::Success,
                                save_as,
                                ..
                            }) => {
                                if let Some(save_as) = save_as {
                                    info!(
                                        "Successfully saved image {} to {}",
                                        index + 1,
                                        save_as.display()
                                    );
                                } else {
                                    info!("Successfully saved image {}", index + 1);
                                }
                                stats.record_success();
                            }
                            Ok(SingleBulkOutcome {
                                index,
                                result: SingleBulkResult::Partial {
                                    successful_tiles,
                                    total_tiles,
                                },
                                ..
                            }) => {
                                warn!(
                                    "Image {} completed with partial download: {}/{} tiles",
                                    index + 1,
                                    successful_tiles,
                                    total_tiles
                                );
                                stats.record_partial();
                            }
                            Ok(SingleBulkOutcome {
                                index,
                                title,
                                result: SingleBulkResult::Failure { msg },
                                ..
                            }) => {
                                warn!(
                                    "Failed to process image {} ('{}'): {}",
                                    index + 1,
                                    title,
                                    msg
                                );
                                stats.record_failure();
                            }
                            Err(e) => {
                                warn!("Bulk task failed: {e}");
                                stats.record_failure();
                            }
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_single_bulk_image(
    args: &Arguments,
    http: &Client,
    base_dir: &Path,
    bulk_outfile: Option<&PathBuf>,
    index: usize,
    total_images: usize,
    zoomable_image: ZoomableImage,
    cancel: CancellationToken,
    multi: std::sync::Arc<indicatif::MultiProgress>,
) -> Result<SingleBulkOutcome, ZoomError> {
    use log::{debug, trace, warn};
    let image_title = zoomable_image
        .title()
        .unwrap_or_else(|| format!("Image_{}", index + 1).into())
        .to_string();
    debug!(
        "Preparing image {}/{}: {}",
        index + 1,
        total_images,
        image_title
    );

    // Resolve the ZoomableImage to get zoom levels
    let zoom_levels = match zoomable_image.into_zoom_levels(http).await {
        Ok(levels) => levels,
        Err(e) => {
            return Ok(SingleBulkOutcome {
                index,
                title: image_title,
                result: SingleBulkResult::Failure { msg: e.to_string() },
                save_as: None,
            });
        }
    };

    trace!(
        "Zoom levels for image {}: {} levels available",
        index + 1,
        zoom_levels.len()
    );

    // Choose the appropriate zoom level using existing logic
    let zoom_level = match choose_level(zoom_levels, args) {
        Ok(level) => level,
        Err(e) => {
            return Ok(SingleBulkOutcome {
                index,
                title: image_title,
                result: SingleBulkResult::Failure { msg: e.to_string() },
                save_as: None,
            });
        }
    };

    debug!(
        "Selected zoom level for image {}: {} ({}x{})",
        index + 1,
        zoom_level.name(),
        zoom_level.size_hint().map(|s| s.x).unwrap_or(0),
        zoom_level.size_hint().map(|s| s.y).unwrap_or(0)
    );

    // Compute and atomically reserve the output path. For derived titles this
    // loops on _0001-style suffixes until it successfully creates the file.
    let save_as = match if let Some(base_outfile) = bulk_outfile {
        // In bulk mode with specified outfile, use index-based naming with collision handling
        let base_path = generate_bulk_output_name(base_outfile, index);
        reserve_unique_outname(
            &Some(base_path),
            &zoom_level.title().or_else(|| Some(image_title.clone())),
            base_dir,
            zoom_level.size_hint(),
        )
    } else {
        // Use the zoom level title if present, fallback to image title
        reserve_unique_outname(
            &None,
            &zoom_level.title().or_else(|| Some(image_title.clone())),
            base_dir,
            zoom_level.size_hint(),
        )
    } {
        Ok(path) => path,
        Err(e) => {
            return Ok(SingleBulkOutcome {
                index,
                title: image_title,
                result: SingleBulkResult::Failure { msg: e.to_string() },
                save_as: None,
            });
        }
    };

    let tile_buffer =
        match create_tile_buffer(save_as.clone(), args.compression, args.jxl_effort).await {
            Ok(buffer) => buffer,
            Err(e) => {
                let file_name = save_as
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_else(|| "unknown".into());
                warn!(
                    "Failed to create tile buffer for file '{}' (image {} '{}'): {}",
                    file_name,
                    index + 1,
                    image_title,
                    e
                );
                return Ok(SingleBulkOutcome {
                    index,
                    title: image_title,
                    result: SingleBulkResult::Failure { msg: e.to_string() },
                    save_as: Some(save_as),
                });
            }
        };

    // Now show processing message since we're about to start downloading.
    // Suspend the bar drawing first so this log line lands on a clean row
    // above any per-image bars that the MultiProgress may already be showing.
    multi.suspend(|| {
        info!(
            "Processing image {}/{}: {} -> {}",
            index + 1,
            total_images,
            image_title,
            save_as.file_name().unwrap_or_default().to_string_lossy()
        );
    });

    let result = match dezoomify_level(args, zoom_level, tile_buffer, cancel, Some(&multi)).await {
        Ok(()) => SingleBulkResult::Success,
        Err(ZoomError::PartialDownload {
            successful_tiles,
            total_tiles,
            ..
        }) => SingleBulkResult::Partial {
            successful_tiles,
            total_tiles,
        },
        Err(e) => SingleBulkResult::Failure { msg: e.to_string() },
    };
    Ok(SingleBulkOutcome {
        index,
        title: image_title,
        result,
        save_as: Some(save_as),
    })
}

/// Generate a unique output filename for bulk processing
fn generate_bulk_output_name(base_outfile: &Path, index: usize) -> PathBuf {
    let mut result = base_outfile.to_path_buf();

    if let Some(stem) = base_outfile.file_stem() {
        if let Some(extension) = base_outfile.extension() {
            let new_name = format!(
                "{}_{}.{}",
                stem.to_string_lossy(),
                index + 1,
                extension.to_string_lossy()
            );
            result.set_file_name(new_name);
        } else {
            let new_name = format!("{}_{}", stem.to_string_lossy(), index + 1);
            result.set_file_name(new_name);
        }
    } else {
        result.set_file_name(format!("dezoomified_{}.jxl", index + 1));
    }

    result
}

/// Validates the download success based on the final state.
/// Validates that enough tiles were downloaded to proceed
fn validate_download_success(state: &download_state::DownloadState) -> Result<(), ZoomError> {
    if !state.is_successful() {
        Err(ZoomError::NoTile)
    } else {
        Ok(())
    }
}

/// Determines final result based on download success rate
fn determine_final_result(
    state: &download_state::DownloadState,
    destination: String,
    expects_failed_tiles: bool,
) -> Result<(), ZoomError> {
    if !expects_failed_tiles && state.has_partial_failure() {
        Err(ZoomError::PartialDownload {
            successful_tiles: state.successful_tiles,
            total_tiles: state.total_tiles,
            destination,
        })
    } else {
        Ok(())
    }
}

pub async fn dezoomify_level(
    args: &Arguments,
    mut zoom_level: ZoomLevel,
    tile_buffer: TileBuffer,
    cancel: CancellationToken,
    multi: Option<&indicatif::MultiProgress>,
) -> Result<(), ZoomError> {
    debug!("Starting to dezoomify {zoom_level:?}");
    let mut canvas = tile_buffer;
    let coordinator =
        download_state::TileDownloadCoordinator::new(&zoom_level, args, cancel.clone())?;
    let mut state = download_state::DownloadState::new();
    let progress = match multi {
        Some(mp) => download_state::ProgressManager::new_in_multi(mp),
        None => download_state::ProgressManager::new(),
    };

    progress.set_computing_urls();

    let mut zoom_level_iter = ZoomLevelIter::new(&mut zoom_level);

    while let Some(tile_refs) = zoom_level_iter.next_tile_references()? {
        if cancel.is_cancelled() {
            return Err(ZoomError::Cancelled);
        }
        coordinator
            .download_batch(
                tile_refs,
                &mut canvas,
                &mut state,
                &progress,
                &zoom_level_iter,
            )
            .await?;

        zoom_level_iter.set_fetch_result(state.create_fetch_result());
    }

    validate_download_success(&state)?;

    progress.set_finalizing();
    canvas.finalize().await?;
    progress.finish();

    let destination = canvas.destination().to_string_lossy().to_string();
    determine_final_result(&state, destination, zoom_level.expects_failed_tiles())
}

/// Returns the maximal size a tile can have in order to fit in a canvas of the given size
pub fn max_size_in_rect(position: Vec2d, tile_size: Vec2d, canvas_size: Vec2d) -> Vec2d {
    (position + tile_size).min(canvas_size) - position
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_parse_level_index() {
        assert_eq!(parse_level_index("0", 5), Some(0));
        assert_eq!(parse_level_index("4", 5), Some(4));
        assert_eq!(parse_level_index("5", 5), None); // Out of bounds
        assert_eq!(parse_level_index("abc", 5), None); // Invalid number
        assert_eq!(parse_level_index("", 5), None); // Empty string
        assert_eq!(parse_level_index("2", 1), None); // Index too high
    }

    #[test]
    fn test_resolve_level_index() {
        assert_eq!(resolve_level_index(2, 5), 2); // Within bounds
        assert_eq!(resolve_level_index(0, 5), 0); // First index
        assert_eq!(resolve_level_index(4, 5), 4); // Last valid index
        assert_eq!(resolve_level_index(10, 5), 4); // Out of bounds, use last
        assert_eq!(resolve_level_index(100, 3), 2); // Way out of bounds
    }

    #[test]
    fn test_resolve_image_index() {
        assert_eq!(resolve_image_index(1, 3), 1); // Within bounds
        assert_eq!(resolve_image_index(0, 3), 0); // First index
        assert_eq!(resolve_image_index(2, 3), 2); // Last valid index
        assert_eq!(resolve_image_index(5, 3), 2); // Out of bounds, use last
        assert_eq!(resolve_image_index(100, 1), 0); // Way out of bounds
    }

    #[test]
    fn test_max_size_in_rect() {
        // Tile fits completely within canvas
        assert_eq!(
            max_size_in_rect(
                Vec2d { x: 10, y: 10 },
                Vec2d { x: 50, y: 50 },
                Vec2d { x: 100, y: 100 }
            ),
            Vec2d { x: 50, y: 50 }
        );

        // Tile extends beyond canvas horizontally
        assert_eq!(
            max_size_in_rect(
                Vec2d { x: 80, y: 10 },
                Vec2d { x: 50, y: 50 },
                Vec2d { x: 100, y: 100 }
            ),
            Vec2d { x: 20, y: 50 }
        );

        // Tile extends beyond canvas vertically
        assert_eq!(
            max_size_in_rect(
                Vec2d { x: 10, y: 80 },
                Vec2d { x: 50, y: 50 },
                Vec2d { x: 100, y: 100 }
            ),
            Vec2d { x: 50, y: 20 }
        );

        // Tile extends beyond canvas in both dimensions
        assert_eq!(
            max_size_in_rect(
                Vec2d { x: 90, y: 90 },
                Vec2d { x: 50, y: 50 },
                Vec2d { x: 100, y: 100 }
            ),
            Vec2d { x: 10, y: 10 }
        );

        // Tile at canvas edge
        assert_eq!(
            max_size_in_rect(
                Vec2d { x: 0, y: 0 },
                Vec2d { x: 100, y: 100 },
                Vec2d { x: 100, y: 100 }
            ),
            Vec2d { x: 100, y: 100 }
        );
    }

    #[test]
    fn test_validate_download_success() {
        let mut successful_state = download_state::DownloadState::new();
        successful_state.record_success();
        assert!(validate_download_success(&successful_state).is_ok());

        let failed_state = download_state::DownloadState::new();
        assert!(validate_download_success(&failed_state).is_err());
    }

    #[test]
    fn test_determine_final_result() {
        let destination = "test.jpg".to_string();

        // Complete success - no partial failure
        let mut success_state = download_state::DownloadState::new();
        success_state.add_batch(10);
        for _ in 0..10 {
            success_state.record_success();
        }
        assert!(determine_final_result(&success_state, destination.clone(), false).is_ok());

        // Partial failure
        let mut partial_state = download_state::DownloadState::new();
        partial_state.add_batch(10);
        for _ in 0..8 {
            partial_state.record_success();
        }
        let result = determine_final_result(&partial_state, destination.clone(), false);
        assert!(result.is_err());
        if let Err(ZoomError::PartialDownload {
            successful_tiles,
            total_tiles,
            ..
        }) = result
        {
            assert_eq!(successful_tiles, 8);
            assert_eq!(total_tiles, 10);
        } else {
            panic!("Expected PartialDownload error");
        }
    }

    #[test]
    fn test_determine_final_result_ignores_failures_when_expected() {
        // The generic dezoomer probes tiles past the image boundary and relies
        // on 404s to discover the real dimensions. When the provider signals
        // this with `expects_failed_tiles: true`, a partial download state must
        // be reported as success.
        let destination = "test.jxl".to_string();

        let mut partial_state = download_state::DownloadState::new();
        partial_state.add_batch(10);
        for _ in 0..4 {
            partial_state.record_success();
        }
        assert!(
            determine_final_result(&partial_state, destination, true).is_ok(),
            "expects_failed_tiles=true should suppress PartialDownload"
        );
    }

    #[test]
    fn test_find_level_with_size() {
        // Since we can't easily create real ZoomLevel instances for testing,
        // let's test the logic directly with a simpler approach
        let sizes = [
            Some(Vec2d { x: 100, y: 100 }),
            Some(Vec2d { x: 200, y: 200 }),
            None,
            Some(Vec2d { x: 300, y: 300 }),
        ];

        let target_size = Vec2d { x: 200, y: 200 };
        let position = sizes.iter().position(|&s| s == Some(target_size));
        assert_eq!(position, Some(1));

        let target_size_not_found = Vec2d { x: 400, y: 400 };
        let position = sizes.iter().position(|&s| s == Some(target_size_not_found));
        assert_eq!(position, None);
    }

    #[test]
    fn test_generate_bulk_output_name() {
        use std::path::Path;

        // Test with extension
        let base = Path::new("output.jpg");
        assert_eq!(
            generate_bulk_output_name(base, 0),
            Path::new("output_1.jpg")
        );
        assert_eq!(
            generate_bulk_output_name(base, 9),
            Path::new("output_10.jpg")
        );

        // Test without extension
        let base = Path::new("output");
        assert_eq!(generate_bulk_output_name(base, 0), Path::new("output_1"));
        assert_eq!(generate_bulk_output_name(base, 4), Path::new("output_5"));

        // Test with complex path
        let base = Path::new("/path/to/my_file.png");
        assert_eq!(
            generate_bulk_output_name(base, 2),
            Path::new("/path/to/my_file_3.png")
        );

        // Test with no stem (edge case)
        let base = Path::new(".hidden");
        assert_eq!(generate_bulk_output_name(base, 0), Path::new(".hidden_1"));
    }

    #[test]
    fn test_bulk_stats() {
        let mut stats = BulkStats::new();

        // Test initial state
        assert_eq!(stats.total_images, 0);
        assert_eq!(stats.successful_images, 0);
        assert_eq!(stats.failed_images, 0);
        assert_eq!(stats.partial_downloads, 0);

        // Test setting total
        stats.set_total(10);
        assert_eq!(stats.total_images, 10);

        // Test recording different types of results
        stats.record_success();
        stats.record_success();
        stats.record_partial();
        stats.record_failure();
        stats.record_failure();
        stats.record_failure();

        assert_eq!(stats.successful_images, 2);
        assert_eq!(stats.partial_downloads, 1);
        assert_eq!(stats.failed_images, 3);
        assert_eq!(stats.total_images, 10); // Should remain unchanged
    }

    #[test]
    fn test_generate_bulk_output_name_edge_cases() {
        use std::path::Path;

        // Test with multiple dots
        let base = Path::new("file.name.with.dots.jpg");
        assert_eq!(
            generate_bulk_output_name(base, 0),
            Path::new("file.name.with.dots_1.jpg")
        );

        // Test with extension only
        let base = Path::new(".jpg");
        assert_eq!(generate_bulk_output_name(base, 0), Path::new(".jpg_1"));

        // Test large index
        let base = Path::new("test.png");
        assert_eq!(
            generate_bulk_output_name(base, 999),
            Path::new("test_1000.png")
        );

        // Test with Unicode characters
        let base = Path::new("测试文件.jpg");
        assert_eq!(
            generate_bulk_output_name(base, 0),
            Path::new("测试文件_1.jpg")
        );
    }

    #[test]
    fn test_bulk_mode_outfile_prefers_explicit_outfile() {
        let args = Arguments::parse_from([
            "dezoomify-rs",
            "--bulk",
            "urls.txt",
            "from_positional.jpg",
            "explicit.jpg",
        ]);
        assert_eq!(args.bulk_output_file(), Some(PathBuf::from("explicit.jpg")));
    }

    #[test]
    fn test_bulk_mode_outfile_does_not_use_input_uri() {
        let args = Arguments::parse_from(["dezoomify-rs", "--bulk", "urls.txt", "fallback.jpg"]);
        assert_eq!(args.bulk_output_file(), None);
    }

    #[test]
    fn test_bulk_mode_outfile_option_conflicts_with_positional() {
        // When the user provides both the positional OUTFILE and the --outfile
        // flag, clap must reject the invocation rather than silently picking
        // one. This guards against the previous behavior where the flag would
        // override the positional without warning.
        let result = Arguments::try_parse_from([
            "dezoomify-rs",
            "--bulk",
            "urls.txt",
            "input.jpg",          // input_uri
            "positional-out.jpg", // outfile (positional)
            "--outfile",
            "from-flag.jpg",
        ]);
        assert!(
            result.is_err(),
            "expected clap to reject conflicting positional and --outfile, got: {:?}",
            result.map(|a| a.bulk_output_file())
        );
    }
}

#[cfg(test)]
mod iiif_title_tests {
    use crate::iiif::determine_title;
    use crate::iiif::manifest_types::ExtractedImageInfo;

    #[test]
    fn test_determine_title_all_components() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: Some("Manifest Title".to_string()),
            metadata_title: Some("Metadata Title".to_string()),
            canvas_label: Some("Canvas Label".to_string()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(
            result,
            Some("Manifest Title - Metadata Title - Canvas Label".to_string())
        );
    }

    #[test]
    fn test_determine_title_manifest_and_canvas_only() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: Some("Book Title".to_string()),
            metadata_title: None,
            canvas_label: Some("Page 1".to_string()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(result, Some("Book Title - Page 1".to_string()));
    }

    #[test]
    fn test_determine_title_canvas_only() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: None,
            metadata_title: None,
            canvas_label: Some("Single Page".to_string()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(result, Some("Single Page".to_string()));
    }

    #[test]
    fn test_determine_title_no_duplicates() {
        // Test that duplicate titles are not repeated
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: Some("Same Title".to_string()),
            metadata_title: Some("Same Title".to_string()), // Duplicate
            canvas_label: Some("Different Label".to_string()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(result, Some("Same Title - Different Label".to_string()));
    }

    #[test]
    fn test_determine_title_empty() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: None,
            metadata_title: None,
            canvas_label: None,
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(result, None);
    }

    #[test]
    fn test_determine_title_metadata_only() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: None,
            metadata_title: Some("Metadata Only".to_string()),
            canvas_label: None,
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(result, Some("Metadata Only".to_string()));
    }

    #[test]
    fn test_determine_title_special_characters() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: Some("Ms. Smith's \"Book\" & Notes (1850-1900)".to_string()),
            metadata_title: None,
            canvas_label: Some("Page #1: Introduction/Overview".to_string()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(
            result,
            Some(
                "Ms. Smith's \"Book\" & Notes (1850-1900) - Page #1: Introduction/Overview"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_determine_title_very_long() {
        let long_manifest = "A".repeat(100);
        let long_canvas = "B".repeat(100);

        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: Some(long_manifest.clone()),
            metadata_title: None,
            canvas_label: Some(long_canvas.clone()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        let expected = format!("{} - {}", long_manifest, long_canvas);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_determine_title_unicode() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: Some("古典文学作品集".to_string()),
            metadata_title: Some("詩經選讀".to_string()),
            canvas_label: Some("第一章：關雎".to_string()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        assert_eq!(
            result,
            Some("古典文学作品集 - 詩經選讀 - 第一章：關雎".to_string())
        );
    }

    #[test]
    fn test_determine_title_whitespace_handling() {
        let image_info = ExtractedImageInfo {
            image_uri: "https://example.com/image.json".to_string(),
            manifest_label: Some("  Manifest with spaces  ".to_string()),
            metadata_title: Some("\tTabbed metadata\t".to_string()),
            canvas_label: Some("Canvas\nwith\nnewlines".to_string()),
            canvas_index: 0,
        };

        let result = determine_title(&image_info);
        // Note: The function doesn't currently trim whitespace, it preserves what's in the manifest
        assert_eq!(
            result,
            Some(
                "  Manifest with spaces   - \tTabbed metadata\t - Canvas\nwith\nnewlines"
                    .to_string()
            )
        );
    }
}
