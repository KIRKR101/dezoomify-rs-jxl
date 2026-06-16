use std::path::PathBuf;

/**
Used to receive tiles asynchronously and provide them to the encoder
*/
use log::{debug, warn};
use tokio::sync::mpsc;

use crate::encoder::{Encoder, encoder_for_name};
use crate::tile::Tile;
use crate::{Vec2d, ZoomError};

/// Data structure used to store tiles until the final image size is known
pub enum TileBuffer {
    Buffering {
        destination: PathBuf,
        buffer: Vec<Tile>,
        compression: u8,
        jxl_effort: Option<u8>,
    },
    Writing {
        destination: PathBuf,
        tile_sender: mpsc::Sender<TileBufferMsg>,
        error_receiver: mpsc::Receiver<std::io::Error>,
    },
}

impl TileBuffer {
    /// Create an encoder for an image of the given size at the path
    /// Errors out if the encoder cannot create files with the given extension
    /// or at the given size
    pub async fn new(
        destination: PathBuf,
        compression: u8,
        jxl_effort: Option<u8>,
    ) -> Result<Self, ZoomError> {
        Ok(TileBuffer::Buffering {
            destination,
            buffer: vec![],
            compression,
            jxl_effort,
        })
    }

    pub async fn set_size(&mut self, size: Vec2d) -> Result<(), ZoomError> {
        let next_state = match self {
            TileBuffer::Buffering {
                buffer,
                destination,
                compression,
                jxl_effort,
            } => {
                let destination = std::mem::take(destination);
                let jxl_effort = *jxl_effort;
                debug!("Creating a tile writer for an image of size {size}");
                // Try the requested encoder first; if it is JXL and the
                // encoder refuses to initialize (e.g. unusual image
                // dimensions, codec init failure), fall back to PNG once
                // before propagating the error. Runtime encoding errors
                // mid-stream are not retried because re-encoding would
                // require re-downloading all tiles.
                let (mut encoder, final_destination) =
                    match encoder_for_name(destination.clone(), size, *compression, jxl_effort) {
                        Ok(enc) => (enc, destination),
                        Err(primary_err) => match png_fallback_destination(&destination) {
                            Some(png_dest) => {
                                warn!(
                                    "Primary encoder ({:?}) failed to initialize: {}. \
                                     Falling back to PNG at {:?}.",
                                    destination.extension(),
                                    primary_err,
                                    png_dest,
                                );
                                match encoder_for_name(
                                    png_dest.clone(),
                                    size,
                                    *compression,
                                    jxl_effort,
                                ) {
                                    Ok(enc) => (enc, png_dest),
                                    Err(fallback_err) => {
                                        return Err(fallback_err);
                                    }
                                }
                            }
                            None => return Err(primary_err),
                        },
                    };
                debug!("Adding buffered tiles: {buffer:?}");
                for tile in buffer.drain(..) {
                    encoder.add_tile(tile)?;
                }
                buffer_tiles(encoder, final_destination).await
            }
            TileBuffer::Writing { .. } => {
                unreachable!("The size of the image can be set only once")
            }
        };
        *self = next_state;
        Ok(())
    }

    /// Add a tile to the image
    pub async fn add_tile(&mut self, tile: Tile) -> Result<(), ZoomError> {
        match self {
            TileBuffer::Buffering { buffer, .. } => {
                buffer.push(tile);
                Ok(())
            }
            TileBuffer::Writing { tile_sender, .. } => tile_sender
                .send(TileBufferMsg::AddTile(tile))
                .await
                .map_err(|_| ZoomError::Io {
                    source: std::io::Error::other("The tile writer ended unexpectedly"),
                }),
        }
    }

    /// To be called when no more tile will be added
    pub async fn finalize(&mut self) -> Result<(), ZoomError> {
        if let TileBuffer::Buffering { buffer, .. } = self {
            let size = buffer
                .iter()
                .map(|t| t.position + t.size())
                .fold(Vec2d { x: 0, y: 0 }, Vec2d::max);
            self.set_size(size).await?;
        }
        let (tile_sender, error_receiver) = match self {
            TileBuffer::Buffering { .. } => unreachable!("Just set the size"),
            TileBuffer::Writing {
                tile_sender,
                error_receiver,
                ..
            } => (tile_sender, error_receiver),
        };
        tile_sender.send(TileBufferMsg::Close).await?;
        debug!("Waiting for the image encoding task to finish");
        let mut result = Ok(());
        // Wait for the encoder to terminate even if some tiles raised errors
        while let Some(err) = error_receiver.recv().await {
            result = Err(err.into())
        }
        result
    }

    pub fn destination(&self) -> &PathBuf {
        match self {
            TileBuffer::Buffering { destination, .. } => destination,
            TileBuffer::Writing { destination, .. } => destination,
        }
    }
}

#[derive(Debug)]
pub enum TileBufferMsg {
    AddTile(Tile),
    Close,
}

async fn buffer_tiles(mut encoder: Box<dyn Encoder>, destination: PathBuf) -> TileBuffer {
    let (tile_sender, mut tile_receiver) = mpsc::channel(1024);
    let (error_sender, error_receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(msg) = tile_receiver.recv().await {
            match msg {
                TileBufferMsg::AddTile(tile) => {
                    debug!("Sending tile to encoder: {tile:?}");
                    let result = tokio::task::block_in_place(|| encoder.add_tile(tile));
                    if let Err(err) = result {
                        warn!("Error when adding tile: {err}");
                        if error_sender.send(err).await.is_err() {
                            // The main task is no longer listening; stop wasting CPU.
                            break;
                        }
                    }
                }
                TileBufferMsg::Close => {
                    break;
                }
            }
        }
        debug!("Finalizing the encoder");
        if let Err(err) = encoder.finalize() {
            warn!("Error when finalizing image: {err}");
            let _ = error_sender.send(err).await;
        }
    });
    TileBuffer::Writing {
        tile_sender,
        error_receiver,
        destination,
    }
}

/// If the destination is a JXL file, return a sibling path with the `.png`
/// extension. Returns `None` for non-JXL extensions (we never override the
/// user's explicit format choice) or for paths that have no extension to
/// rewrite.
fn png_fallback_destination(destination: &std::path::Path) -> Option<PathBuf> {
    let ext = destination.extension()?.to_string_lossy().to_lowercase();
    if ext != "jxl" {
        return None;
    }
    Some(destination.with_extension("png"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn png_fallback_rewrites_jxl_extension() {
        let p = Path::new("foo.jxl");
        assert_eq!(
            png_fallback_destination(p).unwrap(),
            PathBuf::from("foo.png")
        );
    }

    #[test]
    fn png_fallback_handles_uppercase_jxl() {
        let p = Path::new("FOO.JXL");
        assert_eq!(
            png_fallback_destination(p).unwrap(),
            PathBuf::from("FOO.png")
        );
    }

    #[test]
    fn png_fallback_skips_non_jxl_extensions() {
        assert!(png_fallback_destination(Path::new("foo.png")).is_none());
        assert!(png_fallback_destination(Path::new("foo.jpg")).is_none());
        assert!(png_fallback_destination(Path::new("foo.tiff")).is_none());
    }

    #[test]
    fn png_fallback_handles_paths_without_extension() {
        assert!(png_fallback_destination(Path::new("foo")).is_none());
    }
}
