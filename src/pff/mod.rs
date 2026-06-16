use std::sync::Arc;

/// Dezoomer for the zoomify PFF servlet API format
/// See: https://github.com/lovasoa/pff-extract/wiki/Zoomify-PFF-file-format-documentation
use serde_urlencoded as urlencoded;

use image_properties::PffHeader;
use image_properties::Reply;

use crate::ZoomError;
use crate::dezoomer::*;
use crate::pff::image_properties::{
    HeaderInfo, ImageInfo, InitialServletRequestParams, PffError, RequestType, TileIndices,
};

mod image_properties;

/// Dezoomer for Zoomify PFF.
/// Takes an URL to a pff file
#[derive(Default)]
pub enum PFF {
    #[default]
    Init,
    WithHeader(HeaderInfo),
}

impl From<PffError> for DezoomerError {
    fn from(err: PffError) -> Self {
        DezoomerError::wrap(err)
    }
}

impl From<PffError> for ZoomError {
    fn from(err: PffError) -> Self {
        ZoomError::TileUrl {
            source: crate::errors::TileUrlError::from(err),
        }
    }
}

impl Dezoomer for PFF {
    fn name(&self) -> &'static str {
        "pff"
    }

    fn zoom_levels(&mut self, data: &DezoomerInput) -> Result<ZoomLevels, DezoomerError> {
        let mut parts = data.uri.splitn(2, '?');
        let base_url = parts
            .next()
            .ok_or_else(|| self.wrong_dezoomer())?
            .to_string();
        let params_str = parts.next().ok_or_else(|| self.wrong_dezoomer())?;
        match self {
            PFF::Init => {
                let init_params: InitialServletRequestParams =
                    urlencoded::from_str(params_str).map_err(PffError::from)?;
                let file = init_params.file;
                if init_params.request_type != RequestType::Metadata as u8 {
                    let uri = format!(
                        "{}?file={}&requestType={}",
                        base_url,
                        file,
                        RequestType::Metadata as u8
                    );
                    return Err(DezoomerError::NeedsData { uri });
                }
                let DezoomerInputWithContents { contents, .. } = data.with_contents()?;
                let reply: Reply<PffHeader> =
                    serde_urlencoded::from_bytes(contents).map_err(PffError::from)?;
                let header_info = HeaderInfo {
                    base_url,
                    file,
                    header: reply.reply_data,
                };
                let uri = header_info.tiles_index_url();
                *self = PFF::WithHeader(header_info);
                Err(DezoomerError::NeedsData { uri })
            }
            PFF::WithHeader(header_info) => {
                let DezoomerInputWithContents { contents, .. } = data.with_contents()?;
                let reply: Reply<TileIndices> =
                    urlencoded::from_bytes(contents).map_err(PffError::from)?;
                Ok(zoom_levels(ImageInfo {
                    header_info: header_info.clone(),
                    tiles: reply.reply_data,
                }))
            }
        }
    }
}

fn zoom_levels(info: ImageInfo) -> ZoomLevels {
    let info = Arc::new(info);
    let header = &info.header_info.header;
    let mut size = Vec2d {
        x: header.width,
        y: header.height,
    };
    let mut tiles_before = 0;
    let mut levels = vec![];
    while size.x >= header.tile_size && size.y >= header.tile_size {
        let level = PffZoomLevel {
            image_info: Arc::clone(&info),
            tiles_before,
            size,
        };
        tiles_before += level.tile_count();
        size = size.ceil_div(Vec2d { x: 2, y: 2 });
        levels.push(Box::new(level) as ZoomLevel);
    }
    levels
}

struct PffZoomLevel {
    image_info: Arc<ImageInfo>,
    tiles_before: u32,
    size: Vec2d,
}

impl TilesRect for PffZoomLevel {
    fn size(&self) -> Vec2d {
        self.size
    }

    fn tile_size(&self) -> Vec2d {
        let size = self.image_info.header_info.header.tile_size;
        Vec2d { x: size, y: size }
    }

    fn tile_url(&self, pos: Vec2d) -> Result<String, ZoomError> {
        let num_tiles_x = (self.size().ceil_div(self.tile_size())).x;
        // Check the largest product first so a u32 wrap on `pos.y * num_tiles_x`
        // is caught before any `checked_add` ever sees it.
        let i = pos
            .y
            .checked_mul(num_tiles_x)
            .and_then(|p| p.checked_add(self.tiles_before))
            .and_then(|s| s.checked_add(pos.x))
            .ok_or_else(|| ZoomError::Dezoomer {
                source: DezoomerError::Other {
                    source: format!(
                        "PFF tile index overflow at position {:?} \
                         (tiles_before={}, pos.x={}, pos.y={}, num_tiles_x={})",
                        pos, self.tiles_before, pos.x, pos.y, num_tiles_x
                    )
                    .into(),
                },
            })?;
        self.image_info
            .tile_url(i as usize)
            .map_err(ZoomError::from)
    }
}

impl std::fmt::Debug for PffZoomLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Zoomify PFF")
    }
}
