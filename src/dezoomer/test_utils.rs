/// Test utilities for dezoomer tests.
/// Helpers to unwrap common result types in tests.

use crate::dezoomer::{DezoomerError, DezoomerResult, ZoomLevels, ZoomableImage, ZoomableImageUrl};

pub fn expect_single_resolved(result: DezoomerResult) -> ZoomLevels {
    assert_eq!(result.len(), 1, "Expected exactly one zoomable image");
    let image = result.into_iter().next().unwrap();
    match image {
        ZoomableImage::Image(img) => img.into_zoom_levels().expect("into_zoom_levels failed"),
        ZoomableImage::ImageUrl(_) => panic!("Expected a resolved image, got URL"),
    }
}

pub fn expect_only<T>(result: Result<Vec<T>, DezoomerError>) -> T {
    let mut items = result.expect("Expected success but got error");
    assert_eq!(items.len(), 1, "Expected exactly one item");
    items.pop().unwrap()
}

pub fn expect_image_urls(result: DezoomerResult) -> Vec<ZoomableImage> {
    for img in &result {
        assert!(
            matches!(img, ZoomableImage::ImageUrl(_)),
            "Expected only ImageUrl variants"
        );
    }
    result
}

pub fn expect_single_url(result: DezoomerResult) -> ZoomableImageUrl {
    assert_eq!(result.len(), 1, "Expected exactly one image");
    let img = result.into_iter().next().unwrap();
    match img {
        ZoomableImage::ImageUrl(url) => url,
        ZoomableImage::Image(_) => panic!("Expected ImageUrl, got Image"),
    }
}

pub fn expect_needs_data(result: Result<ZoomLevels, DezoomerError>) -> String {
    match result {
        Err(DezoomerError::NeedsData { uri }) => uri,
        other => panic!("Expected NeedsData, got {:?}", other),
    }
}

pub fn expect_resolved_images(result: DezoomerResult) -> Vec<Box<dyn crate::dezoomer::ZoomableImageWithLevels>> {
    result.into_iter().map(|img| {
        match img {
            ZoomableImage::Image(i) => i,
            ZoomableImage::ImageUrl(_) => panic!("Expected resolved image, got URL"),
        }
    }).collect()
}

pub fn assert_error_contains(result: Result<DezoomerResult, DezoomerError>, msg: &str) {
    match result {
        Err(e) => assert!(e.to_string().contains(msg), "Expected error containing '{}', got '{}'", msg, e),
        Ok(_) => panic!("Expected error but got Ok"),
    }
}
