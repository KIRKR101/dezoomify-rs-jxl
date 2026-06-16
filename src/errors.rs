use std::error::Error;
use std::fmt;

use reqwest::{self, header};
use thiserror::Error;
use tokio::sync::mpsc::error::SendError;

use crate::dezoomer::TileReference;
use crate::encoder::tile_buffer::TileBufferMsg;

#[derive(Error, Debug)]
pub enum ZoomError {
    #[error("network error: {source}")]
    Networking {
        #[from]
        source: reqwest::Error,
    },

    #[error("Dezoomer error: {source}")]
    Dezoomer {
        #[from]
        source: DezoomerError,
    },

    #[error("A zoomable image was found, but it did not contain any zoom level")]
    NoLevels,

    #[error("No url found in bulk file {bulk_file_path}")]
    NoBulkUrl { bulk_file_path: String },

    #[error(
        "Could not get any tile for the image. See https://dezoomify-rs.ophir.dev/no-tile-error"
    )]
    NoTile,

    #[error(
        "Only {successful_tiles} tiles out of {total_tiles} could be downloaded. \
             The resulting image was still created in '{destination}'."
    )]
    PartialDownload {
        successful_tiles: u64,
        total_tiles: u64,
        destination: String,
    },

    #[error("invalid image error: {source}")]
    Image {
        #[from]
        source: image::ImageError,
    },

    #[error("unable to process the downloaded tile: {source}")]
    PostProcessing {
        source: Box<dyn Error + Send + Sync>,
    },

    #[error("Input/Output error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    #[error("Invalid YAML configuration file: {source}")]
    Yaml {
        #[from]
        source: serde_yml::Error,
    },

    #[error(
        "Unable to copy a {twidth}x{theight} tile at position {x},{y} on a canvas of size {width}x{height}"
    )]
    TileCopyError {
        x: u32,
        y: u32,
        twidth: u32,
        theight: u32,
        width: u32,
        height: u32,
    },

    #[error("Malformed tile string: '{tile_str}' expected 'x y url'")]
    MalformedTileStr { tile_str: String },

    #[error("No such dezoomer: {name}")]
    NoSuchDezoomer { name: String },

    #[error("Invalid header name: {source}")]
    InvalidHeaderName {
        #[from]
        source: header::InvalidHeaderName,
    },

    #[error("Invalid header value: {source}")]
    InvalidHeaderValue {
        #[from]
        source: header::InvalidHeaderValue,
    },

    #[error("Unable get the result from a thread: {source}")]
    AsyncError {
        #[from]
        source: tokio::task::JoinError,
    },

    #[error("{source}")]
    BufferToImage {
        #[from]
        source: BufferToImageError,
    },

    #[error("Unable to write tile {source:?}")]
    WriteError {
        #[from]
        source: SendError<TileBufferMsg>,
    },

    #[error("PNG encoding error: {source}")]
    PngError {
        #[from]
        source: png::EncodingError,
    },

    #[error("Operation cancelled by user")]
    Cancelled,

    #[error("Could not generate the URL for a tile: {source}")]
    TileUrl {
        #[from]
        source: TileUrlError,
    },
}

#[derive(Error, Debug)]
pub enum TileUrlError {
    #[error("Invalid tile number {tile_number}, only {num_tiles} tiles are available")]
    InvalidTileNumber {
        tile_number: usize,
        num_tiles: usize,
    },
    #[error("{0}")]
    Other(String),
}

#[derive(Error, Debug)]
pub enum BufferToImageError {
    #[error("invalid image error: {source}")]
    Image {
        #[from]
        source: image::ImageError,
    },

    #[error("unable to process the downloaded tile: {e}")]
    PostProcessing { e: Box<dyn Error + Send + Sync> },
}

#[derive(Error, Debug)]
pub enum DezoomerError {
    #[error("Need to download data from {uri}")]
    NeedsData { uri: String },

    #[error("The '{name}' dezoomer cannot handle this URI")]
    WrongDezoomer { name: &'static str },

    #[error("Unable to download required data: {msg}")]
    DownloadError { msg: String },

    #[error("Unable to create the dezoomer: {source}")]
    Other {
        source: Box<dyn Error + Send + Sync>,
    },
}

impl DezoomerError {
    pub fn wrap<E: Error + Send + Sync + 'static>(err: E) -> DezoomerError {
        DezoomerError::Other {
            source: Box::new(err),
        }
    }
}

pub fn image_error_to_io_error(err: image::ImageError) -> std::io::Error {
    match err {
        image::ImageError::IoError(e) => e,
        e => make_io_err(e),
    }
}

pub fn make_io_err<E>(e: E) -> std::io::Error
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    std::io::Error::other(e)
}

#[derive(Debug)]
pub struct TileDownloadError {
    pub tile_reference: TileReference,
    pub cause: ZoomError,
}

impl fmt::Display for TileDownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Unable to download tile '{}'. Cause: {}",
            self.tile_reference.url, self.cause
        )
    }
}

impl Error for TileDownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}
