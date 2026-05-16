use image::{
    ExtendedColorType, GenericImageView, ImageBuffer, ImageEncoder, ImageError, ImageResult, Pixel,
    PixelWithColorType, Rgb, Rgba,
};
use log::debug;
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use crate::encoder::Encoder;
use crate::tile::Tile;
use crate::{Vec2d, ZoomError};

type CanvasBuffer<Pix> = ImageBuffer<Pix, Vec<<Pix as Pixel>::Subpixel>>;

pub struct Canvas<Pix: Pixel = Rgba<u8>> {
    image: CanvasBuffer<Pix>,
    destination: PathBuf,
    image_writer: ImageWriter,
    icc_profile: Option<Vec<u8>>,
}

impl<Pix: Pixel> Canvas<Pix> {
    pub fn new_generic(destination: PathBuf, size: Vec2d) -> Result<Self, ZoomError> {
        Ok(Canvas {
            image: ImageBuffer::new(size.x, size.y),
            destination,
            image_writer: ImageWriter::Generic,
            icc_profile: None,
        })
    }

    pub fn new_jpeg(
        destination: PathBuf,
        size: Vec2d,
        quality: u8,
    ) -> Result<Canvas<Rgb<u8>>, ZoomError> {
        Ok(Canvas::<Rgb<u8>> {
            image: ImageBuffer::new(size.x, size.y),
            destination,
            image_writer: ImageWriter::Jpeg { quality },
            icc_profile: None,
        })
    }

    pub fn new_jxl_rgba(
        destination: PathBuf,
        size: Vec2d,
        quality: u8,
    ) -> Result<Canvas<Rgba<u8>>, ZoomError> {
        Ok(Canvas::<Rgba<u8>> {
            image: ImageBuffer::new(size.x, size.y),
            destination,
            image_writer: ImageWriter::Jxl { quality },
            icc_profile: None,
        })
    }
}

trait FromRgba {
    fn from_rgba(rgba: Rgba<u8>) -> Self;
}

impl FromRgba for Rgba<u8> {
    fn from_rgba(rgba: Rgba<u8>) -> Self {
        rgba
    }
}

impl FromRgba for Rgb<u8> {
    fn from_rgba(rgba: Rgba<u8>) -> Self {
        rgba.to_rgb()
    }
}

#[allow(clippy::panicking_unwrap)] // https://github.com/rust-lang/rust-clippy/issues/16188
impl<Pix: Pixel<Subpixel = u8> + PixelWithColorType + Send + FromRgba + 'static> Encoder
    for Canvas<Pix>
{
    fn add_tile(&mut self, tile: Tile) -> io::Result<()> {
        debug!("Copying tile data from {tile:?}");

        // Capture ICC profile from the first tile that has one
        if self.icc_profile.is_none() && tile.icc_profile.is_some() {
            self.icc_profile = tile.icc_profile.clone();
            debug!(
                "Captured ICC profile from tile (size: {} bytes)",
                self.icc_profile.as_ref().unwrap().len()
            );
        }

        let min_pos = tile.position();
        let canvas_size = self.size();
        if !min_pos.fits_inside(canvas_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tile too large for image",
            ));
        }
        let max_pos = tile.bottom_right().min(canvas_size);
        let size = max_pos - min_pos;
        for y in 0..size.y {
            let canvas_y = y + min_pos.y;
            for x in 0..size.x {
                let canvas_x = x + min_pos.x;
                let p = tile.image.get_pixel(x, y);
                self.image.put_pixel(canvas_x, canvas_y, Pix::from_rgba(p));
            }
        }
        Ok(())
    }

    fn finalize(&mut self) -> io::Result<()> {
        self.image_writer
            .write(&self.image, &self.destination, &self.icc_profile)
            .map_err(|e| match e {
                image::ImageError::IoError(e) => e,
                other => io::Error::other(other),
            })?;
        Ok(())
    }

    fn size(&self) -> Vec2d {
        self.image.dimensions().into()
    }
}

pub enum ImageWriter {
    Generic,
    Jpeg { quality: u8 },
    Jxl { quality: u8 },
}

impl ImageWriter {
    fn write<Pix: Pixel<Subpixel = u8> + PixelWithColorType>(
        &self,
        image: &CanvasBuffer<Pix>,
        destination: &Path,
        icc_profile: &Option<Vec<u8>>,
    ) -> ImageResult<()> {
        match *self {
            ImageWriter::Jpeg { quality } => {
                let file = File::create(destination)?;
                let fout = &mut BufWriter::new(file);
                let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(fout, quality);

                if let Some(profile) = icc_profile {
                    if let Err(e) = encoder.set_icc_profile(profile.clone()) {
                        debug!("Failed to set ICC profile for JPEG: {}", e);
                    } else {
                        debug!("Applied ICC profile to JPEG output");
                    }
                }

                encoder.encode(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    ExtendedColorType::Rgb8,
                )?;
            }
            ImageWriter::Generic => {
                self.encode_with_format_dispatch(image, destination, icc_profile.as_deref())?;
            }
            ImageWriter::Jxl { quality } => {
                self.write_jxl(image, destination, icc_profile, quality)?;
            }
        };
        Ok(())
    }

    fn write_jxl<Pix: Pixel<Subpixel = u8> + PixelWithColorType>(
        &self,
        image: &CanvasBuffer<Pix>,
        destination: &Path,
        icc_profile: &Option<Vec<u8>>,
        quality: u8,
    ) -> ImageResult<()> {
        use jxl_encoder::api::{
            ImageMetadata, LosslessConfig, LossyConfig, PixelLayout,
        };

        let (width, height) = image.dimensions();
        let raw = image.as_raw();
        let pixel_layout = if Pix::COLOR_TYPE == ExtendedColorType::Rgba8 {
            PixelLayout::Rgba8
        } else {
            PixelLayout::Rgb8
        };

        let metadata = icc_profile
            .as_ref()
            .map(|profile| ImageMetadata::new().with_icc_profile(profile));

        let encoded = if quality >= 100 {
            let config = LosslessConfig::new();
            let mut request = config.encode_request(width, height, pixel_layout);
            if let Some(meta) = &metadata {
                request = request.with_metadata(meta);
            }
            request.encode(raw)
        } else {
            let distance = (100.0 - quality as f32) / 10.0;
            let config = LossyConfig::new(distance);
            let mut request = config.encode_request(width, height, pixel_layout);
            if let Some(meta) = &metadata {
                request = request.with_metadata(meta);
            }
            request.encode(raw)
        };

        let jxl_bytes = encoded
            .map_err(|e| ImageError::IoError(std::io::Error::other(e)))?;

        std::fs::write(destination, &jxl_bytes).map_err(ImageError::IoError)?;
        Ok(())
    }

    fn encode_with_format_dispatch<Pix: Pixel<Subpixel = u8> + PixelWithColorType>(
        &self,
        image: &CanvasBuffer<Pix>,
        destination: &Path,
        icc_profile: Option<&[u8]>,
    ) -> ImageResult<()> {
        let extension = destination
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        match extension.as_str() {
            "jxl" => {
                let quality = 95u8;
                self.write_jxl(
                    image,
                    destination,
                    &icc_profile.map(|p| p.to_vec()),
                    quality,
                )?;
            }
            "png" => {
                if let Some(profile) = icc_profile {
                    Self::encode_with_icc_profile::<
                        Pix,
                        image::codecs::png::PngEncoder<BufWriter<File>>,
                    >(
                        image,
                        destination,
                        profile,
                        image::codecs::png::PngEncoder::new,
                        "PNG",
                    )?;
                } else {
                    image.save(destination)?;
                }
            }
            "tiff" | "tif" => {
                if let Some(profile) = icc_profile {
                    Self::encode_with_icc_profile::<
                        Pix,
                        image::codecs::tiff::TiffEncoder<BufWriter<File>>,
                    >(
                        image,
                        destination,
                        profile,
                        image::codecs::tiff::TiffEncoder::new,
                        "TIFF",
                    )?;
                } else {
                    image.save(destination)?;
                }
            }
            "webp" => {
                if let Some(profile) = icc_profile {
                    Self::encode_with_icc_profile::<
                        Pix,
                        image::codecs::webp::WebPEncoder<BufWriter<File>>,
                    >(
                        image,
                        destination,
                        profile,
                        image::codecs::webp::WebPEncoder::new_lossless,
                        "WebP",
                    )?;
                } else {
                    image.save(destination)?;
                }
            }
            _ => {
                if icc_profile.is_some() {
                    debug!("ICC profile not supported for format: {}", extension);
                }
                image.save(destination)?;
            }
        }
        Ok(())
    }

    fn encode_with_icc_profile<Pix, E>(
        image: &CanvasBuffer<Pix>,
        destination: &Path,
        icc_profile: &[u8],
        encoder_factory: fn(BufWriter<File>) -> E,
        format_name: &str,
    ) -> ImageResult<()>
    where
        Pix: Pixel<Subpixel = u8> + PixelWithColorType,
        E: ImageEncoder,
    {
        let file = File::create(destination)?;
        let fout = BufWriter::new(file);
        let mut encoder = encoder_factory(fout);

        if let Err(e) = encoder.set_icc_profile(icc_profile.to_owned()) {
            debug!("Failed to set ICC profile for {}: {}", format_name, e);
        } else {
            debug!("Applied ICC profile to {} output", format_name);
        }

        encoder.write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            Pix::COLOR_TYPE,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageBuffer, Rgba};
    use std::env::temp_dir;

    #[test]
    fn test_canvas_captures_icc_profile() {
        let destination = temp_dir().join("test_icc_canvas.png");
        let size = Vec2d { x: 2, y: 2 };
        let mut canvas = Canvas::<Rgba<u8>>::new_generic(destination, size).unwrap();

        assert!(canvas.icc_profile.is_none());

        let tile_with_profile = Tile::builder()
            .with_image(DynamicImage::ImageRgba8(
                ImageBuffer::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap(),
            ))
            .at_position(Vec2d { x: 0, y: 0 })
            .with_icc_profile(vec![1, 2, 3, 4, 5])
            .build();

        canvas.add_tile(tile_with_profile).unwrap();

        assert!(canvas.icc_profile.is_some());
        assert_eq!(canvas.icc_profile.unwrap().len(), 5);
    }

    #[test]
    fn test_canvas_ignores_later_icc_profiles() {
        let destination = temp_dir().join("test_icc_priority.png");
        let size = Vec2d { x: 2, y: 2 };
        let mut canvas = Canvas::<Rgba<u8>>::new_generic(destination, size).unwrap();

        let first_tile = Tile::builder()
            .with_image(DynamicImage::ImageRgba8(
                ImageBuffer::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap(),
            ))
            .at_position(Vec2d { x: 0, y: 0 })
            .with_icc_profile(vec![1, 2, 3])
            .build();

        canvas.add_tile(first_tile).unwrap();
        let first_profile = canvas.icc_profile.clone();

        let second_tile = Tile::builder()
            .with_image(DynamicImage::ImageRgba8(
                ImageBuffer::from_raw(1, 1, vec![0, 255, 0, 255]).unwrap(),
            ))
            .at_position(Vec2d { x: 1, y: 0 })
            .with_icc_profile(vec![4, 5, 6, 7])
            .build();

        canvas.add_tile(second_tile).unwrap();

        assert_eq!(canvas.icc_profile, first_profile);
        assert_eq!(canvas.icc_profile.unwrap().len(), 3);
    }

    #[test]
    fn test_jxl_write_creates_valid_file() {
        let destination = temp_dir().join("dezoomify-rs-jxl-test.jxl");
        let image = ImageBuffer::<Rgba<u8>, _>::from_raw(2, 2, vec![
            255, 0, 0, 255,
            0, 255, 0, 255,
            0, 0, 255, 255,
            255, 255, 0, 255,
        ]).unwrap();

        let writer = ImageWriter::Jxl { quality: 95 };
        writer.write_jxl(&image, &destination, &None, 95).unwrap();

        assert!(destination.exists());
        assert!(destination.metadata().unwrap().len() > 0);

        let file = std::fs::File::open(&destination).unwrap();
        let jxl_image = jxl_oxide::JxlImage::builder()
            .read(std::io::BufReader::new(file))
            .unwrap();
        assert_eq!(jxl_image.width(), 2);
        assert_eq!(jxl_image.height(), 2);
    }

    #[test]
    fn test_jxl_lossless_roundtrip() {
        let destination = temp_dir().join("dezoomify-rs-jxl-lossless.jxl");
        let pixels = vec![
            128, 64, 32, 255,
            10, 20, 30, 255,
            200, 150, 100, 255,
            0, 0, 0, 255,
        ];
        let image = ImageBuffer::<Rgba<u8>, _>::from_raw(2, 2, pixels.clone()).unwrap();

        let writer = ImageWriter::Jxl { quality: 100 };
        writer.write_jxl(&image, &destination, &None, 100).unwrap();

        assert!(destination.exists());

        let file = std::fs::File::open(&destination).unwrap();
        let decoder = jxl_oxide::integration::JxlDecoder::new(file).unwrap();
        let decoded = DynamicImage::from_decoder(decoder).unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
        let decoded_rgba = decoded.to_rgba8();
        assert_eq!(decoded_rgba.as_raw(), &pixels, "lossless pixel mismatch");
    }

    #[test]
    fn test_jxl_with_icc_profile() {
        let destination = temp_dir().join("dezoomify-rs-jxl-icc.jxl");
        let image = ImageBuffer::<Rgba<u8>, _>::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap();

        let icc_profile = vec![
            0x00, 0x00, 0x02, 0x0C,
            0x61, 0x63, 0x73, 0x70,
            0x00, 0x00, 0x00, 0x00,
        ];

        let writer = ImageWriter::Jxl { quality: 90 };
        writer.write_jxl(&image, &destination, &Some(icc_profile), 90).unwrap();

        assert!(destination.exists());
        assert!(destination.metadata().unwrap().len() > 0);

        let file = std::fs::File::open(&destination).unwrap();
        let jxl_image = jxl_oxide::JxlImage::builder()
            .read(std::io::BufReader::new(file))
            .unwrap();
        assert_eq!(jxl_image.width(), 1);
        assert_eq!(jxl_image.height(), 1);
    }

    #[test]
    fn test_jxl_canvas_full_pipeline_rgba() {
        let destination = temp_dir().join("dezoomify-rs-jxl-pipeline.jxl");
        let size = Vec2d { x: 2, y: 2 };
        let mut canvas = Canvas::<Rgba<u8>>::new_jxl_rgba(destination.clone(), size, 100).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 0, y: 0 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 1, y: 0 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![0, 255, 0, 255]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 0, y: 1 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![0, 0, 255, 255]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 1, y: 1 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![255, 255, 0, 255]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.finalize().unwrap();

        assert!(destination.exists());

        let file = std::fs::File::open(&destination).unwrap();
        let decoder = jxl_oxide::integration::JxlDecoder::new(file).unwrap();
        let decoded = DynamicImage::from_decoder(decoder).unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
        let decoded_rgba = decoded.to_rgba8();
        let expected_pixels = vec![
            255, 0, 0, 255,
            0, 255, 0, 255,
            0, 0, 255, 255,
            255, 255, 0, 255,
        ];
        assert_eq!(decoded_rgba.as_raw(), &expected_pixels, "pixel mismatch");
    }

    #[test]
    fn test_jxl_rgba_alpha_preserved() {
        let destination = temp_dir().join("dezoomify-rs-jxl-alpha.jxl");
        let size = Vec2d { x: 2, y: 2 };
        let mut canvas = Canvas::<Rgba<u8>>::new_jxl_rgba(destination.clone(), size, 100).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 0, y: 0 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 1, y: 0 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![0, 255, 0, 128]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 0, y: 1 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![0, 0, 255, 64]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.add_tile(
            Tile::builder()
                .at_position(Vec2d { x: 1, y: 1 })
                .with_image(DynamicImage::ImageRgba8(
                    ImageBuffer::from_raw(1, 1, vec![255, 255, 0, 0]).unwrap(),
                ))
                .build(),
        ).unwrap();

        canvas.finalize().unwrap();

        assert!(destination.exists());

        let file = std::fs::File::open(&destination).unwrap();
        let decoder = jxl_oxide::integration::JxlDecoder::new(file).unwrap();
        let decoded = DynamicImage::from_decoder(decoder).unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
        let decoded_rgba = decoded.to_rgba8();
        let expected_pixels = vec![
            255, 0, 0, 255,
            0, 255, 0, 128,
            0, 0, 255, 64,
            255, 255, 0, 0,
        ];
        assert_eq!(decoded_rgba.as_raw(), &expected_pixels, "alpha was not preserved");
    }
}
