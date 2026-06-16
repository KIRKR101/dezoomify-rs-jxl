use std::collections::HashMap;
use std::convert::TryInto;
use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use fixedbitset::FixedBitSet;
use image::GenericImage;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, SubImage};
use log::{debug, warn};
use lru::LruCache;

use crate::Vec2d;
use crate::errors::image_error_to_io_error;
use crate::{Tile, max_size_in_rect};

pub trait TileSaver {
    fn save_tile(&self, size: Vec2d, tile: Tile) -> io::Result<()>;
}

/**
A Retiler represents an image at a certain zoom level.
It works in the following way :
It has a child that represents the image at the next zoom level (where the image is smaller),
the child itself has a child, and so on until the smallest zoom level.

The retiler receives tiles from the original image.
The received tiles can have any size, so they can cover partially or entirely any number of tiles
in the target image.
It computes the list of tiles covered by the current image,
and pastes the correct resized and cropped source tile into temporary target tiles in the user's temporary folder.

When a target tile has been entirely covered by source tiles,
it is encoded to jpeg and saved to the target folder.

Every level passes the source tile to it's child when it is done with it.
**/
/// Number of target-tile images to keep in memory per retiler level. The rest are
/// spilled to temporary BMP files on disk.
const TILE_IMAGE_CACHE_SIZE: NonZeroUsize = match NonZeroUsize::new(64) {
    Some(n) => n,
    None => unreachable!(),
};

pub struct Retiler<T: TileSaver> {
    original_size: Vec2d,
    pub tile_size: Vec2d,
    scale_factor: u32,
    next_level: Option<Box<Retiler<T>>>,
    /// This hash map contains target tiles that are being written.
    /// When a target tile has been entirely covered by source tiles, its entry is set to None
    tiles: HashMap<Vec2d, Option<TmpTile>>,
    tile_saver: Arc<T>,
    /// In-memory cache for partial target-tile images. Evicted entries are written to disk.
    tile_image_cache: LruCache<(Vec2d, u32), DynamicImage>,
}

struct TmpTile {
    done_pixels: FixedBitSet,
}

impl<T: TileSaver> Retiler<T> {
    pub fn new(size: Vec2d, tile_size: Vec2d, tile_saver: Arc<T>, scale_factor: u32) -> Retiler<T> {
        let next_level = if size.ceil_div(scale_factor).fits_inside(tile_size) {
            None
        } else {
            let tile_saver = Arc::clone(&tile_saver);
            let level = Retiler::new(size, tile_size, tile_saver, scale_factor * 2);
            Some(Box::new(level))
        };
        Retiler {
            original_size: size,
            tile_size: tile_size * scale_factor,
            next_level,
            tiles: HashMap::new(),
            tile_saver,
            scale_factor,
            tile_image_cache: LruCache::new(TILE_IMAGE_CACHE_SIZE),
        }
    }

    pub fn size(&self) -> Vec2d {
        self.original_size.ceil_div(self.scale_factor)
    }

    fn tile_positions(&self, position: Vec2d, size: Vec2d) -> impl Iterator<Item = Vec2d> + use<T> {
        let top_left = (position / self.tile_size) * self.tile_size;
        let bottom_right = ((position + size).ceil_div(self.tile_size)) * self.tile_size;
        let dy = self.tile_size.y as usize;
        let dx = self.tile_size.x as usize;
        (top_left.y..bottom_right.y).step_by(dy).flat_map(move |y| {
            (top_left.x..bottom_right.x)
                .step_by(dx)
                .map(move |x| Vec2d { x, y })
        })
    }

    pub fn add_tile(&mut self, tile: &Tile) -> io::Result<()> {
        let tile_size = self.tile_size;
        let scale_factor = self.scale_factor;
        let scaled_top_left = tile.position() / scale_factor;
        let scaled_bottom_right = tile.bottom_right().ceil_div(scale_factor);
        let scaled_size = scaled_bottom_right - scaled_top_left;
        let covered_tiles_positions = self.tile_positions(tile.position, tile.size());
        let scaled_tile = if scale_factor == 1 {
            None
        } else {
            Some(
                Tile::builder()
                    .at_position(scaled_top_left)
                    .with_image(tile.image.resize_exact(
                        scaled_size.x,
                        scaled_size.y,
                        FilterType::Gaussian,
                    ))
                    .with_icc_profile(tile.icc_profile.clone().unwrap_or_default())
                    .build(),
            )
        };
        let scaled_tile = scaled_tile.as_ref().unwrap_or(tile);
        for cur_pos in covered_tiles_positions {
            let cur_tile_size = max_size_in_rect(cur_pos, tile_size, self.original_size);
            let scaled_tile_size = cur_tile_size.ceil_div(scale_factor);

            let is_pending = match self.tiles.get(&cur_pos) {
                Some(None) => false,
                Some(Some(_)) => true,
                None => {
                    debug!(
                        "Creating a new partial tile at scale factor {scale_factor} position {cur_pos} size {cur_tile_size}"
                    );
                    self.tiles
                        .insert(cur_pos, Some(TmpTile::new(scaled_tile_size)));
                    true
                }
            };
            if !is_pending {
                debug!(
                    "Source tiles overlap:\
                        Received pixels for tile at {} on level {}, but this tile has already been written.\
                        Ignoring them (source tiles overlap).",
                    cur_pos, self.scale_factor
                );
                continue;
            }

            let key = (cur_pos, scale_factor);
            let tmp_tile_path = TmpTile::path(cur_pos, scale_factor);
            let scaled_self_position = cur_pos / scale_factor;
            let scaled_level_size = self.original_size.ceil_div(scale_factor);
            let self_bottom_right = (cur_pos + cur_tile_size)
                .ceil_div(scale_factor)
                .min(scaled_level_size);
            let top_left = scaled_tile.position() - scaled_self_position;
            let bottom_right =
                scaled_tile.bottom_right().min(self_bottom_right) - scaled_self_position;
            let sub_tile_img =
                crop_image_for_tile(scaled_tile, scaled_self_position, scaled_tile_size);

            let finished_image = {
                let tmp_tile = self.tiles.get_mut(&cur_pos).unwrap().as_mut().unwrap();
                let mut tile_img =
                    take_or_create_image(&mut self.tile_image_cache, key, scaled_tile_size);

                tile_img
                    .copy_from(&*sub_tile_img, top_left.x, top_left.y)
                    .map_err(|_err| {
                        io::Error::new(io::ErrorKind::InvalidData, "tile too large for image")
                    })?;

                tmp_tile.set_done_pixels(scaled_tile_size, top_left, bottom_right);

                if tmp_tile.is_complete() {
                    debug!(
                        "Removing completed tile of level {} at position {}: {:?}",
                        self.original_size, cur_pos, tmp_tile_path
                    );
                    self.tile_image_cache.pop(&key);
                    let _ = std::fs::remove_file(&tmp_tile_path);
                    Some(tile_img)
                } else {
                    if let Some((evicted_key, evicted_img)) =
                        self.tile_image_cache.push(key, tile_img)
                    {
                        // Cache is full: spill the least-recently used tile to disk.
                        let (pos, scale) = evicted_key;
                        let spill_path = TmpTile::path(pos, scale);
                        if let Err(e) = evicted_img
                            .save(spill_path.as_path())
                            .map_err(image_error_to_io_error)
                        {
                            warn!("Unable to spill retiler tile to {spill_path:?}: {e}");
                        }
                    }
                    None
                }
            };

            if let Some(tile_img) = finished_image {
                self.tile_save(cur_pos, cur_tile_size, tile_img)?;
                self.tiles.insert(cur_pos, None);
            }
        }

        if let Some(next_level) = &mut self.next_level {
            next_level.add_tile(tile)?;
        }
        Ok(())
    }

    /// Add all partially downloaded tiles to the final image
    pub fn finalize(&mut self) {
        for (position, tile_opt) in std::mem::take(&mut self.tiles).into_iter() {
            if let Some(tile) = tile_opt {
                let cur_tile_size = max_size_in_rect(position, self.tile_size, self.original_size);
                warn!(
                    "The target tile of size {} at zoom level {} and position {} \
            was not fully covered by source tiles. It misses {} pixels.",
                    cur_tile_size,
                    self.scale_factor,
                    position,
                    tile.missing_pixels()
                );
                let key = (position, self.scale_factor);
                let tile_img = take_or_create_image(
                    &mut self.tile_image_cache,
                    key,
                    cur_tile_size.ceil_div(self.scale_factor),
                );
                let tmp_tile_path = TmpTile::path(position, self.scale_factor);
                let result = self
                    .tile_save(position, cur_tile_size, tile_img)
                    .and_then(|()| std::fs::remove_file(&tmp_tile_path));
                if let Err(e) = result {
                    warn!(
                        "Additionally, the following error occurred \
                when trying to add the partial tile to the final image: {e}"
                    )
                }
                // Ensure the cache entry for this tile is removed so it is not
                // written back to disk when the retiler is dropped.
                self.tile_image_cache.pop(&key);
            }
        }
        if let Some(next_level) = &mut self.next_level {
            next_level.finalize()
        }
    }

    pub fn tile_save(&self, position: Vec2d, size: Vec2d, image: DynamicImage) -> io::Result<()> {
        self.tile_saver.save_tile(
            size,
            Tile::builder()
                .with_image(image)
                .at_position(position)
                .build(),
        )
    }

    pub fn level_count(&self) -> u32 {
        1 + self
            .next_level
            .as_ref()
            .map(|l| l.level_count())
            .unwrap_or(0)
    }
}

impl TmpTile {
    fn new(size: Vec2d) -> TmpTile {
        let bits = size.area().try_into().expect("Tile size too large");
        TmpTile {
            done_pixels: FixedBitSet::with_capacity(bits),
        }
    }

    fn is_complete(&self) -> bool {
        self.done_pixels.count_ones(..) == self.done_pixels.len()
    }

    fn missing_pixels(&self) -> usize {
        self.done_pixels.len() - self.done_pixels.count_ones(..)
    }

    fn set_done_pixels(&mut self, self_size: Vec2d, top_left: Vec2d, bottom_right: Vec2d) {
        for y in top_left.y..bottom_right.y {
            let start = (y * self_size.x + top_left.x) as usize;
            let end = (y * self_size.x + bottom_right.x) as usize;
            self.done_pixels.insert_range(start..end);
        }
    }

    fn path(position: Vec2d, scale_factor: u32) -> PathBuf {
        let pid = std::process::id();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "dezoomify_{}_level_{}_position_{}x{}.bmp",
            pid, scale_factor, position.x, position.y
        ));
        path
    }
}

fn crop_image_for_tile(
    source_tile: &Tile,
    scaled_tile_pos: Vec2d,
    scaled_tile_size: Vec2d,
) -> SubImage<&DynamicImage> {
    let top_left = scaled_tile_pos.max(source_tile.position());
    let bottom_right = source_tile
        .bottom_right()
        .min(scaled_tile_pos + scaled_tile_size);
    let crop_position = top_left - source_tile.position();
    let crop_size = bottom_right - top_left;
    source_tile
        .image
        .view(crop_position.x, crop_position.y, crop_size.x, crop_size.y)
}

/// Retrieve a partial tile image from the in-memory cache, fall back to its
/// temporary file, or create a new blank RGB image if neither exists.
///
/// Pulled out as a free function so both `Retiler::add_tile` and
/// `Retiler::finalize` can share the same logic without tripping the borrow
/// checker when they also need to mutate `self.tiles` or `self.tile_image_cache`.
fn take_or_create_image(
    tile_image_cache: &mut LruCache<(Vec2d, u32), DynamicImage>,
    key: (Vec2d, u32),
    size: Vec2d,
) -> DynamicImage {
    if let Some(img) = tile_image_cache.pop(&key) {
        return img;
    }
    let path = TmpTile::path(key.0, key.1);
    if let Ok(img) = image::open(&path) {
        return img;
    }
    DynamicImage::new_rgb8(size.x, size.y)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use image::ImageBuffer;

    use super::*;

    fn init() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    fn plain_image(size: Vec2d, color: u8) -> DynamicImage {
        let pixels = (0..size.area()).map(|_| color).collect();
        DynamicImage::ImageLuma8(ImageBuffer::from_raw(size.x, size.y, pixels).unwrap())
    }

    #[derive(Default)]
    struct TestTileSaver {
        added: Mutex<Vec<(Vec2d, Tile)>>,
    }

    impl TileSaver for TestTileSaver {
        fn save_tile(&self, size: Vec2d, tile: Tile) -> io::Result<()> {
            self.added.lock().unwrap().push((size, tile));
            Ok(())
        }
    }

    impl TestTileSaver {
        fn get_added(&self) -> Vec<(Vec2d, Tile)> {
            self.added.lock().unwrap().clone()
        }
    }

    #[test]
    fn test_retiler() {
        init();
        let image_size = Vec2d { x: 2, y: 3 };
        let tile_size = Vec2d { x: 2, y: 2 };

        let tile_saver = Arc::new(TestTileSaver::default());
        let mut retiler = Retiler::new(image_size, tile_size, Arc::clone(&tile_saver), 1);
        retiler
            .add_tile(
                &Tile::builder()
                    .with_image(plain_image(Vec2d { x: 2, y: 1 }, 64))
                    .at_position(Vec2d { x: 0, y: 0 })
                    .build(),
            )
            .unwrap();
        retiler
            .add_tile(
                &Tile::builder()
                    .with_image(plain_image(Vec2d { x: 2, y: 2 }, 16))
                    .at_position(Vec2d { x: 0, y: 1 })
                    .build(),
            )
            .unwrap();
        retiler.finalize();
        /* We created the following image :
           |----+----|  +---------+
           | 64 | 64 |  |         |
           |----+----|  + tile 1  |
           | 16 | 16 |  |         |
           |----+----|  +---------+
           | 16 | 16 |  | tile 2  |
           +----+----+  +---------+
        */
        let expected_first_tile =
            DynamicImage::ImageLuma8(ImageBuffer::from_raw(2, 2, vec![64, 64, 16, 16]).unwrap());
        let expected_zoomed_out_tile = DynamicImage::ImageLuma8(
            ImageBuffer::from_raw(
                1,
                2,
                vec![
                    16, 16, // A scaled down version of the whole image
                ],
            )
            .unwrap(),
        );
        assert_eq!(
            tile_saver.get_added(),
            vec![
                //   ( covered size , Tile {position in target, size in target, pixels })
                (
                    Vec2d { x: 2, y: 2 },
                    Tile::builder()
                        .at_position(Vec2d { x: 0, y: 0 })
                        .with_image(expected_first_tile)
                        .build()
                ),
                (
                    Vec2d { x: 2, y: 1 },
                    Tile::builder()
                        .at_position(Vec2d { x: 0, y: 2 })
                        .with_image(plain_image(Vec2d { x: 2, y: 1 }, 16))
                        .build()
                ),
                (
                    image_size,
                    Tile::builder()
                        .at_position(Vec2d { x: 0, y: 0 })
                        .with_image(expected_zoomed_out_tile)
                        .build()
                ),
            ]
        );
    }
}
