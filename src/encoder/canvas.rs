use image::{
    DynamicImage, ExtendedColorType, GenericImageView, ImageBuffer, ImageEncoder, ImageError,
    ImageResult, Pixel, PixelWithColorType, Rgb, Rgba,
};
use log::debug;
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use crate::encoder::Encoder;
use crate::tile::Tile;
use crate::Vec2d;

type CanvasBuffer<Pix> = ImageBuffer<Pix, Vec<<Pix as Pixel>::Subpixel>>;

pub struct Canvas<Pix: Pixel = Rgba<u8>> {
    image: CanvasBuffer<Pix>,
    destination: PathBuf,
    image_writer: ImageWriter,
    icc_profile: Option<Vec<u8>>,
    exif_metadata: Option<Vec<u8>>,
}

impl<Pix: Pixel> Canvas<Pix> {
    pub fn new_generic(destination: PathBuf, size: Vec2d) -> Self {
        Canvas {
            image: ImageBuffer::new(size.x, size.y),
            destination,
            image_writer: ImageWriter::Generic,
            icc_profile: None,
            exif_metadata: None,
        }
    }

    pub fn new_jpeg(destination: PathBuf, size: Vec2d, quality: u8) -> Canvas<Rgb<u8>> {
        Canvas::<Rgb<u8>> {
            image: ImageBuffer::new(size.x, size.y),
            destination,
            image_writer: ImageWriter::Jpeg { quality },
            icc_profile: None,
            exif_metadata: None,
        }
    }

    pub fn new_jxl_rgba(
        destination: PathBuf,
        size: Vec2d,
        quality: u8,
        effort: u8,
    ) -> Canvas<Rgba<u8>> {
        Canvas::<Rgba<u8>> {
            image: ImageBuffer::new(size.x, size.y),
            destination,
            image_writer: ImageWriter::Jxl { quality, effort },
            icc_profile: None,
            exif_metadata: None,
        }
    }
}

/// Shared logic for capturing metadata from the first tile that has it.
fn capture_metadata(canvas: &mut Canvas<impl Pixel>, tile: &Tile) {
    if canvas.icc_profile.is_none() && tile.icc_profile.is_some() {
        canvas.icc_profile.clone_from(&tile.icc_profile);
        debug!(
            "Captured ICC profile from tile (size: {} bytes)",
            canvas.icc_profile.as_ref().unwrap().len()
        );
    }
    if canvas.exif_metadata.is_none() && tile.exif_metadata.is_some() {
        canvas.exif_metadata.clone_from(&tile.exif_metadata);
        debug!(
            "Captured EXIF metadata from tile (size: {} bytes)",
            canvas.exif_metadata.as_ref().unwrap().len()
        );
    }
}

fn validate_tile_fit(tile: &Tile, canvas_size: Vec2d) -> io::Result<(Vec2d, Vec2d)> {
    let min_pos = tile.position();
    if !min_pos.fits_inside(canvas_size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tile too large for image",
        ));
    }
    let max_pos = tile.bottom_right().min(canvas_size);
    let size = max_pos - min_pos;
    Ok((min_pos, size))
}

impl Encoder for Canvas<Rgba<u8>> {
    fn add_tile(&mut self, tile: Tile) -> io::Result<()> {
        debug!("Copying tile data from {tile:?}");
        capture_metadata(self, &tile);

        let (min_pos, size) = validate_tile_fit(&tile, self.size())?;

        match &tile.image {
            DynamicImage::ImageRgba8(src) => {
                let tile_width = src.width() as usize;
                let dst_width = self.image.width() as usize;
                let row_bytes = size.x as usize * 4;
                let dst_ptr = self.image.as_mut_ptr();
                let src_ptr = src.as_ptr();
                for y in 0..size.y {
                    let src_offset = (y as usize * tile_width) * 4;
                    let dst_offset =
                        ((min_pos.y + y) as usize * dst_width + min_pos.x as usize) * 4;
                    // SAFETY: offsets and row_bytes are within validated bounds.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src_ptr.add(src_offset),
                            dst_ptr.add(dst_offset),
                            row_bytes,
                        );
                    }
                }
            }
            other => {
                for y in 0..size.y {
                    for x in 0..size.x {
                        let p = other.get_pixel(x, y);
                        self.image.put_pixel(x + min_pos.x, y + min_pos.y, p);
                    }
                }
            }
        }
        Ok(())
    }

    fn finalize(&mut self) -> io::Result<()> {
        self.image_writer
            .write(
                &self.image,
                &self.destination,
                self.icc_profile.as_deref(),
                self.exif_metadata.as_deref(),
            )
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

impl Encoder for Canvas<Rgb<u8>> {
    fn add_tile(&mut self, tile: Tile) -> io::Result<()> {
        debug!("Copying tile data from {tile:?}");
        capture_metadata(self, &tile);

        let (min_pos, size) = validate_tile_fit(&tile, self.size())?;

        match &tile.image {
            DynamicImage::ImageRgb8(src) => {
                let tile_width = src.width() as usize;
                let dst_width = self.image.width() as usize;
                let row_bytes = size.x as usize * 3;
                let dst_ptr = self.image.as_mut_ptr();
                let src_ptr = src.as_ptr();
                for y in 0..size.y {
                    let src_offset = (y as usize * tile_width) * 3;
                    let dst_offset =
                        ((min_pos.y + y) as usize * dst_width + min_pos.x as usize) * 3;
                    // SAFETY: offsets and row_bytes are within validated bounds.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src_ptr.add(src_offset),
                            dst_ptr.add(dst_offset),
                            row_bytes,
                        );
                    }
                }
            }
            other => {
                for y in 0..size.y {
                    for x in 0..size.x {
                        let p = other.get_pixel(x, y);
                        self.image
                            .put_pixel(x + min_pos.x, y + min_pos.y, p.to_rgb());
                    }
                }
            }
        }
        Ok(())
    }

    fn finalize(&mut self) -> io::Result<()> {
        self.image_writer
            .write(
                &self.image,
                &self.destination,
                self.icc_profile.as_deref(),
                self.exif_metadata.as_deref(),
            )
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
    Jxl { quality: u8, effort: u8 },
}

impl ImageWriter {
    fn write<Pix: Pixel<Subpixel = u8> + PixelWithColorType>(
        &self,
        image: &CanvasBuffer<Pix>,
        destination: &Path,
        icc_profile: Option<&[u8]>,
        exif_metadata: Option<&[u8]>,
    ) -> ImageResult<()> {
        match *self {
            ImageWriter::Jpeg { quality } => {
                let file = File::create(destination)?;
                let fout = &mut BufWriter::new(file);
                let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(fout, quality);

                if let Some(profile) = icc_profile {
                    if let Err(e) = encoder.set_icc_profile(profile.to_vec()) {
                        debug!("Failed to set ICC profile for JPEG: {e}");
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
                Self::encode_with_format_dispatch(
                    image,
                    destination,
                    icc_profile,
                    exif_metadata,
                )?;
            }
            ImageWriter::Jxl { quality, effort } => {
                Self::write_jxl(
                    image,
                    destination,
                    icc_profile,
                    exif_metadata,
                    quality,
                    effort,
                )?;
            }
        }
        Ok(())
    }

    fn write_jxl<Pix: Pixel<Subpixel = u8> + PixelWithColorType>(
        image: &CanvasBuffer<Pix>,
        destination: &Path,
        icc_profile: Option<&[u8]>,
        exif_metadata: Option<&[u8]>,
        quality: u8,
        effort: u8,
    ) -> ImageResult<()> {
        use crate::encoder::jxl_encode::JxlEncoder;

        let (width, height) = image.dimensions();
        let raw = image.as_raw();
        let has_alpha = Pix::COLOR_TYPE == ExtendedColorType::Rgba8;
        let uses_original_profile = icc_profile.is_some() || quality >= 100;

        let mut encoder =
            JxlEncoder::create().map_err(|e| ImageError::IoError(std::io::Error::other(e)))?;

        encoder
            .set_basic_info(width, height, has_alpha, uses_original_profile)
            .map_err(|e| ImageError::IoError(std::io::Error::other(e)))?;

        if let Some(profile) = icc_profile {
            encoder
                .set_icc_profile(profile)
                .map_err(|e| ImageError::IoError(std::io::Error::other(e)))?;
        }

        let output_data = encoder
            .encode_frame(raw, has_alpha, f32::from(quality), effort, exif_metadata)
            .map_err(|e| ImageError::IoError(std::io::Error::other(e)))?;

        std::fs::write(destination, &output_data).map_err(ImageError::IoError)?;
        Ok(())
    }

    fn encode_with_format_dispatch<Pix: Pixel<Subpixel = u8> + PixelWithColorType>(
        image: &CanvasBuffer<Pix>,
        destination: &Path,
        icc_profile: Option<&[u8]>,
        exif_metadata: Option<&[u8]>,
    ) -> ImageResult<()> {
        let extension = destination
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        match extension.as_str() {
            "jxl" => {
                let quality = 80u8;
                let effort = 7u8;
                Self::write_jxl(
                    image,
                    destination,
                    icc_profile,
                    exif_metadata,
                    quality,
                    effort,
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
                    debug!("ICC profile not supported for format: {extension}");
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
            debug!("Failed to set ICC profile for {format_name}: {e}");
        } else {
            debug!("Applied ICC profile to {format_name} output");
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
    use jpegxl_rs::image::ToDynamic;
    use std::env::temp_dir;

    #[test]
    fn test_canvas_captures_icc_profile() {
        let destination = temp_dir().join("test_icc_canvas.png");
        let size = Vec2d { x: 2, y: 2 };
        let mut canvas = Canvas::<Rgba<u8>>::new_generic(destination, size);

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
        let mut canvas = Canvas::<Rgba<u8>>::new_generic(destination, size);

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
        let image = ImageBuffer::<Rgba<u8>, _>::from_raw(
            2,
            2,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ],
        )
        .unwrap();

        ImageWriter::write_jxl(&image, &destination, None, None, 95, 7)
            .unwrap();

        assert!(destination.exists());
        assert!(destination.metadata().unwrap().len() > 0);

        let bytes = std::fs::read(&destination).unwrap();
        let decoder = jpegxl_rs::decoder_builder().build().unwrap();
        let (metadata, _) = decoder.decode(&bytes).unwrap();
        assert_eq!(metadata.width, 2);
        assert_eq!(metadata.height, 2);
    }

    #[test]
    fn test_jxl_lossless_roundtrip() {
        let destination = temp_dir().join("dezoomify-rs-jxl-lossless.jxl");
        let pixels = vec![
            128, 64, 32, 255, 10, 20, 30, 255, 200, 150, 100, 255, 0, 0, 0, 255,
        ];
        let image = ImageBuffer::<Rgba<u8>, _>::from_raw(2, 2, pixels.clone()).unwrap();

        ImageWriter::write_jxl(&image, &destination, None, None, 100, 7)
            .unwrap();

        assert!(destination.exists());

        let bytes = std::fs::read(&destination).unwrap();
        let decoded = jpegxl_rs::decoder_builder()
            .build()
            .unwrap()
            .decode_to_image(&bytes)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
        let decoded_rgba = decoded.to_rgba8();
        assert_eq!(decoded_rgba.as_raw(), &pixels, "lossless pixel mismatch");
    }

    #[test]
    fn test_jxl_with_icc_profile() {
        let destination = temp_dir().join("dezoomify-rs-jxl-icc.jxl");
        let image = ImageBuffer::<Rgba<u8>, _>::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap();

        let icc_profile = minimal_srgb_icc_profile();

        ImageWriter::write_jxl(&image, &destination, Some(&icc_profile), None, 90, 7)
            .unwrap();

        assert!(destination.exists());
        assert!(destination.metadata().unwrap().len() > 0);

        let bytes = std::fs::read(&destination).unwrap();
        let decoder = jpegxl_rs::decoder_builder().build().unwrap();
        let (metadata, _) = decoder.decode(&bytes).unwrap();
        assert_eq!(metadata.width, 1);
        assert_eq!(metadata.height, 1);
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn minimal_srgb_icc_profile() -> Vec<u8> {
        let mut b = Vec::new();

        let w32 = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_be_bytes());
        let s15f16 = |b: &mut Vec<u8>, v: f32| w32(b, (v * 65536.0 + 0.5) as u32);

        let rxyz_off: u32 = 216;
        let gxyz_off: u32 = 236;
        let bxyz_off: u32 = 256;
        let rtrc_off: u32 = 276;
        let gtrc_off: u32 = 292;
        let btrc_off: u32 = 308;
        let wtpt_off: u32 = 324;
        let total_size: u32 = wtpt_off + 20;

        w32(&mut b, total_size); // [0]    profile size
        w32(&mut b, 0x6163_7370); // [4]    'acsp'
        w32(&mut b, 0x0210_0000); // [8]    version 2.1.0
        w32(&mut b, 0x6D6E_7472); // [12]   'mntr' display class
        w32(&mut b, 0x5247_4220); // [16]   'RGB '
        w32(&mut b, 0x5859_5A20); // [20]   'XYZ ' PCS
        b.resize(b.len() + 12, 0); // [24]   datetime
        w32(&mut b, 0x6163_7370); // [36]   'acsp' magic
        w32(&mut b, 0); // [40]   platform
        w32(&mut b, 0); // [44]   flags
        w32(&mut b, 0); // [48]   manufacturer
        w32(&mut b, 0); // [52]   model
        b.resize(b.len() + 8, 0); // [56]   attributes
        w32(&mut b, 0); // [64]   intent
        s15f16(&mut b, 0.9642); // [68]   PCS illuminant X (D50)
        s15f16(&mut b, 1.0); // [72]   Y
        s15f16(&mut b, 0.8249); // [76]   Z
        w32(&mut b, 0); // [80]   creator
        b.resize(b.len() + 16, 0); // [84]   profile ID
        b.resize(b.len() + 28, 0); // [100]  reserved (28 bytes)
        assert_eq!(b.len(), 128);

        w32(&mut b, 7); // [128]  tag count

        w32(&mut b, 0x7258_595A);
        w32(&mut b, rxyz_off);
        w32(&mut b, 20);
        w32(&mut b, 0x6758_595A);
        w32(&mut b, gxyz_off);
        w32(&mut b, 20);
        w32(&mut b, 0x6258_595A);
        w32(&mut b, bxyz_off);
        w32(&mut b, 20);
        w32(&mut b, 0x7254_5243);
        w32(&mut b, rtrc_off);
        w32(&mut b, 16);
        w32(&mut b, 0x6754_5243);
        w32(&mut b, gtrc_off);
        w32(&mut b, 16);
        w32(&mut b, 0x6254_5243);
        w32(&mut b, btrc_off);
        w32(&mut b, 16);
        w32(&mut b, 0x7774_7074);
        w32(&mut b, wtpt_off);
        w32(&mut b, 20);
        assert_eq!(b.len(), rxyz_off as usize);

        let write_xyz_tag = |b: &mut Vec<u8>, x: f32, y: f32, z: f32| {
            w32(b, 0x5859_5A20); // 'XYZ '
            w32(b, 0); // reserved
            s15f16(b, x);
            s15f16(b, y);
            s15f16(b, z);
        };

        let write_trc_tag = |b: &mut Vec<u8>, gamma: f32| {
            w32(b, 0x7061_7261); // 'para'
            w32(b, 0); // reserved
            b.extend_from_slice(&0u16.to_be_bytes()); // curve type 0
            b.extend_from_slice(&0u16.to_be_bytes()); // reserved
            s15f16(b, gamma); // gamma value
        };

        // sRGB primaries D50-adapted: r=0.4361,0.2225,0.0139
        write_xyz_tag(&mut b, 0.4361, 0.2225, 0.0139);
        assert_eq!(b.len(), gxyz_off as usize);

        // g=0.3851,0.7169,0.0971
        write_xyz_tag(&mut b, 0.3851, 0.7169, 0.0971);
        assert_eq!(b.len(), bxyz_off as usize);

        // b=0.1431,0.0606,0.7141
        write_xyz_tag(&mut b, 0.1431, 0.0606, 0.7141);
        assert_eq!(b.len(), rtrc_off as usize);

        write_trc_tag(&mut b, 2.2);
        assert_eq!(b.len(), gtrc_off as usize);

        write_trc_tag(&mut b, 2.2);
        assert_eq!(b.len(), btrc_off as usize);

        write_trc_tag(&mut b, 2.2);
        assert_eq!(b.len(), wtpt_off as usize);

        write_xyz_tag(&mut b, 0.9642, 1.0, 0.8249); // D50 white point
        assert_eq!(b.len(), total_size as usize);

        b
    }

    #[test]
    fn test_jxl_canvas_full_pipeline_rgba() {
        let destination = temp_dir().join("dezoomify-rs-jxl-pipeline.jxl");
        let size = Vec2d { x: 2, y: 2 };
        let mut canvas = Canvas::<Rgba<u8>>::new_jxl_rgba(destination.clone(), size, 100, 7);

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 0, y: 0 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 1, y: 0 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![0, 255, 0, 255]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 0, y: 1 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![0, 0, 255, 255]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 1, y: 1 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![255, 255, 0, 255]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas.finalize().unwrap();

        assert!(destination.exists());

        let bytes = std::fs::read(&destination).unwrap();
        let decoded = jpegxl_rs::decoder_builder()
            .build()
            .unwrap()
            .decode_to_image(&bytes)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
        let decoded_rgba = decoded.to_rgba8();
        let expected_pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
        ];
        assert_eq!(decoded_rgba.as_raw(), &expected_pixels, "pixel mismatch");
    }

    #[test]
    fn test_jxl_rgba_alpha_preserved() {
        let destination = temp_dir().join("dezoomify-rs-jxl-alpha.jxl");
        let size = Vec2d { x: 2, y: 2 };
        let mut canvas = Canvas::<Rgba<u8>>::new_jxl_rgba(destination.clone(), size, 100, 7);

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 0, y: 0 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 1, y: 0 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![0, 255, 0, 128]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 0, y: 1 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![0, 0, 255, 64]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas
            .add_tile(
                Tile::builder()
                    .at_position(Vec2d { x: 1, y: 1 })
                    .with_image(DynamicImage::ImageRgba8(
                        ImageBuffer::from_raw(1, 1, vec![255, 255, 0, 0]).unwrap(),
                    ))
                    .build(),
            )
            .unwrap();

        canvas.finalize().unwrap();

        assert!(destination.exists());

        let bytes = std::fs::read(&destination).unwrap();
        let decoded = jpegxl_rs::decoder_builder()
            .build()
            .unwrap()
            .decode_to_image(&bytes)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
        let decoded_rgba = decoded.to_rgba8();
        let expected_pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 255, 255, 0, 0,
        ];
        assert_eq!(
            decoded_rgba.as_raw(),
            &expected_pixels,
            "alpha was not preserved"
        );
    }
}
