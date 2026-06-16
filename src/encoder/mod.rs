use std::path::PathBuf;

use image::{DynamicImage, GenericImageView, Rgb, Rgba, SubImage};
use log::debug;

use crate::tile::Tile;
use crate::{Vec2d, ZoomError, max_size_in_rect};

pub mod canvas;
pub mod iiif_encoder;
pub mod jxl_encode;
pub mod pixel_streamer;
pub mod png_encoder;
mod retiler;
pub mod tile_buffer;

pub trait Encoder: Send + 'static {
    /// Add a tile to the image
    fn add_tile(&mut self, tile: Tile) -> std::io::Result<()>;
    /// To be called when no more tile will be added
    fn finalize(&mut self) -> std::io::Result<()>;
    /// Size of the image being encoded
    fn size(&self) -> Vec2d;
}

/// Compute JXL quality and effort from the user-facing `--compression` and optional `--jxl-effort`.
/// The curve is tuned so that the default settings (compression=5) produce a JXL file
/// roughly the same size as a JPEG saved at the same default compression, while still
/// allowing quality to drop at high compression values.
pub fn jxl_params(compression: u8, jxl_effort: Option<u8>) -> (u8, u8) {
    // A steeper quality slope than JPEG keeps JXL size comparable at the same compression.
    // The intermediate `u16` is clamped to 100 *before* the `u8` cast so the curve stays
    // monotonic: previously, compression values above ~39 would wrap around the `u8`
    // representation and produce a *higher* quality than a smaller compression.
    let quality = 100u8.saturating_sub((compression as u16 * 26 / 10).min(100) as u8);
    // Increase effort with compression; default effort at compression=5 is 6.
    let effort =
        jxl_effort.unwrap_or_else(|| (2 + (compression as u16 * 8 / 10)).clamp(1, 9) as u8);
    (quality, effort.clamp(1, 9))
}

fn extension_str(destination: &std::path::Path) -> Option<String> {
    destination
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
}

fn encoder_for_name(
    destination: PathBuf,
    size: Vec2d,
    compression: u8,
    jxl_effort: Option<u8>,
) -> Result<Box<dyn Encoder>, ZoomError> {
    let extension = extension_str(&destination).unwrap_or_default();
    let quality = 100u8.saturating_sub(compression);

    match extension.as_str() {
        "png" => {
            debug!("Using the streaming png encoder");
            Ok(Box::new(png_encoder::PngEncoder::new(
                destination,
                size,
                compression,
            )?))
        }
        "iiif" => {
            debug!("Using the iiif tiling encoder");
            Ok(Box::new(iiif_encoder::IiifEncoder::new(
                destination,
                size,
                quality,
            )?))
        }
        "jpeg" | "jpg" => {
            debug!("Using the jpeg encoder with a quality of {quality}");
            Ok(Box::new(canvas::Canvas::<Rgb<u8>>::new_jpeg(
                destination,
                size,
                quality,
            )?))
        }
        "jxl" => {
            let (jxl_quality, effort) = jxl_params(compression, jxl_effort);
            debug!("Using the jxl encoder with quality={jxl_quality} effort={effort}");
            Ok(Box::new(canvas::DynamicCanvas::new_jxl(
                destination,
                size,
                jxl_quality,
                effort,
            )?))
        }
        _ => {
            debug!(
                "Using the generic canvas implementation {}",
                &destination.to_string_lossy()
            );
            Ok(Box::new(canvas::Canvas::<Rgba<u8>>::new_generic(
                destination,
                size,
            )?))
        }
    }
}

/// If a tile is larger than the advertised image size, then crop it to fit in the canvas
pub fn crop_tile(tile: &Tile, canvas_size: Vec2d) -> SubImage<&DynamicImage> {
    let Vec2d { x: xmax, y: ymax } = max_size_in_rect(tile.position, tile.size(), canvas_size);
    tile.image.view(0, 0, xmax, ymax)
}

#[cfg(test)]
mod tests {
    use super::jxl_params;

    #[test]
    fn test_jxl_params_defaults() {
        // Default compression=5 should give quality/effort close to default JPEG size.
        let (quality, effort) = jxl_params(5, None);
        assert!(
            (85..=90).contains(&quality),
            "default quality should be around 87, got {quality}"
        );
        assert_eq!(effort, 6);
    }

    #[test]
    fn test_jxl_params_extremes() {
        // Maximum compression should drop quality and clamp effort to 9.
        let (quality, effort) = jxl_params(255, None);
        assert_eq!(
            quality, 0,
            "max compression quality should be 0, got {quality}"
        );
        assert_eq!(effort, 9);

        // Explicit effort is respected and clamped.
        let (_, effort) = jxl_params(5, Some(0));
        assert_eq!(effort, 1);
        let (_, effort) = jxl_params(5, Some(15));
        assert_eq!(effort, 9);
    }

    #[test]
    fn test_jxl_params_quality_is_monotonic() {
        // Quality must be non-increasing as compression increases. The previous
        // formula truncated the intermediate `u16` to `u8` after multiplying, which
        // wrapped around for compression values above ~39 and broke monotonicity.
        let mut prev = 100u8;
        for compression in 0u8..=100 {
            let (quality, _) = jxl_params(compression, None);
            assert!(
                quality <= prev,
                "quality must not increase: compression={compression} \
                 produced quality {quality}, but compression={} produced {prev}",
                compression.saturating_sub(1)
            );
            prev = quality;
        }
    }
}
