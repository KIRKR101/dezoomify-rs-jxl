use crate::dezoomer::{
    Dezoomer, DezoomerError, DezoomerInput, DezoomerInputWithContents, DezoomerResult,
    ZoomLevels, ZoomableImageUrl,
    dezoomer_result_from_urls,
};
use custom_error::custom_error;

custom_error! {pub BulkTextError
    InvalidUrlOrPath{line_number: usize, input: String} = "On line {line_number}: '{input}' is not a valid URL or file path"
}

impl From<BulkTextError> for DezoomerError {
    fn from(err: BulkTextError) -> Self {
        DezoomerError::Other { source: err.into() }
    }
}

/// A dezoomer for text files containing lists of URLs
/// Parses text files where each line is a deferred image URL.
#[derive(Default)]
pub struct BulkTextDezoomer;

impl Dezoomer for BulkTextDezoomer {
    fn name(&self) -> &'static str {
        "bulk_text"
    }

    fn dezoomer_result(&mut self, data: &DezoomerInput) -> Result<DezoomerResult, DezoomerError> {
        // Only process files that are actual bulk URL lists
        // Must have appropriate file extension or "bulk"/"list" in name
        // Exclude files with template variables like {{X}} or {{Y}} which are for generic dezoomer
        let is_bulk_file = (std::path::Path::new(&data.uri)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("txt") || ext.eq_ignore_ascii_case("urls"))
            || data.uri.contains("bulk")
            || data.uri.contains("list"))
            && !data.uri.contains("{{")
            && !data.uri.contains("}}");
        self.assert(is_bulk_file)?;

        let DezoomerInputWithContents { uri: _, contents } = data.with_contents()?;

        // Parse the text content to extract URLs
        let content = std::str::from_utf8(contents).map_err(|e| DezoomerError::DownloadError {
            msg: format!("Failed to parse text file as UTF-8: {e}"),
        })?;

        let urls = parse_text_urls(content)?;

        if urls.is_empty() {
            return Err(DezoomerError::Other {
                source: Box::new(std::io::Error::other("No valid URLs found in text file")),
            });
        }

        Ok(dezoomer_result_from_urls(urls))
    }

    fn zoom_levels(&mut self, _data: &DezoomerInput) -> Result<ZoomLevels, DezoomerError> {
        Err(self.wrong_dezoomer())
    }
}

/// Validate that a string is either a valid URL or an existing file path
fn validate_url_or_path(input: &str, line_number: usize) -> Result<(), BulkTextError> {
    // Try parsing as URL first
    if url::Url::parse(input).is_ok() {
        return Ok(());
    }

    // If not a valid URL, check if it's an existing file path
    if std::path::Path::new(input).exists() {
        return Ok(());
    }

    // If it is an URL template, check if it is valid
    if input.contains("{{X}}") || input.contains("{{Y}}") {
        return Ok(());
    }

    Err(BulkTextError::InvalidUrlOrPath {
        line_number,
        input: input.to_string(),
    })
}

/// Parse a text file content and extract URLs
/// Each non-empty, non-comment line should start with a valid URL.
/// An optional custom title can follow the URL. Titles may be quoted with " or '.
/// Inline comments are supported after the title using ` #` (a space followed
/// by a hash). A hash that is not preceded by a space is part of the title.
/// Formats:
///   URL
///   URL My title
///   URL "My title"
///   URL 'My title' #inline comment
fn parse_text_urls(content: &str) -> Result<Vec<ZoomableImageUrl>, BulkTextError> {
    let mut urls = Vec::new();

    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        // Skip empty lines and full-line comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (url_part, custom_title) = split_url_and_title(trimmed);

        // Validate that the first part is a valid URL or file path
        validate_url_or_path(url_part, line_num + 1)?;

        // Use custom title if provided, otherwise extract from URL
        let title = match custom_title {
            Some(t) => t.to_string(),
            None => extract_title_from_url(url_part, line_num + 1),
        };

        urls.push(ZoomableImageUrl {
            url: url_part.to_string(),
            title: Some(title),
        });
    }

    Ok(urls)
}

/// Split a bulk-text line into a URL and an optional title.
/// Supports quoted titles and ` #` inline comments.
fn split_url_and_title(line: &str) -> (&str, Option<&str>) {
    let mut chars = line.char_indices();
    let (url_end, rest) = loop {
        match chars.next() {
            Some((idx, c)) if c.is_whitespace() => break (idx, &line[idx..]),
            None => return (line, None),
            Some(_) => {},
        }
    };
    let url = &line[..url_end];
    let rest = rest.trim_start();
    if rest.is_empty() {
        return (url, None);
    }

    let title = if let Some(first) = rest.chars().next() {
        if first == '"' || first == '\'' {
            if let Some(close) = rest[1..].find(first) {
                &rest[1..=close]
            } else {
                &rest[1..]
            }
        } else if let Some(comment_pos) = rest.find(" #") {
            rest[..comment_pos].trim_end()
        } else {
            rest
        }
    } else {
        rest
    };

    let title = title.trim();
    if title.is_empty() {
        (url, None)
    } else {
        (url, Some(title))
    }
}

/// Extract a title from a URL for better identification
fn extract_title_from_url(url: &str, line_number: usize) -> String {
    if let Ok(parsed_url) = url::Url::parse(url)
        && let Some(segments) = parsed_url.path_segments()
    {
        let segments: Vec<&str> = segments.collect();
        if let Some(last_segment) = segments.iter().rev().find(|s| !s.is_empty()) {
            let title = if let Some(dot_pos) = last_segment.rfind('.') {
                &last_segment[..dot_pos]
            } else {
                last_segment
            };

            if !title.is_empty() {
                return title.to_string();
            }
        }
    }

    format!("URL_{line_number}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dezoomer::ZoomableImage;
    use crate::dezoomer::{
        PageContents,
        test_utils::{assert_error_contains, expect_image_urls},
    };

    #[test]
    fn test_parse_empty_content() {
        let urls = parse_text_urls("");
        assert!(urls.unwrap().is_empty());
    }

    #[test]
    fn test_parse_comments_and_empty_lines() {
        let content = "# This is a comment\n\n   \n# Another comment";
        let urls = parse_text_urls(content);
        assert!(urls.unwrap().is_empty());
    }

    #[test]
    fn test_parse_valid_urls() {
        let content = "http://example.com/image1.jpg\nhttps://example.org/manifest.json";
        let urls = parse_text_urls(content).unwrap();

        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].url, "http://example.com/image1.jpg");
        assert_eq!(urls[0].title, Some("image1".to_string()));
        assert_eq!(urls[1].url, "https://example.org/manifest.json");
        assert_eq!(urls[1].title, Some("manifest".to_string()));
    }

    #[test]
    fn test_parse_mixed_content() {
        let content = "# IIIF manifests\nhttp://example.com/manifest1.json\n\n# Images\nhttps://example.org/info.json\n# End";
        let urls = parse_text_urls(content).unwrap();

        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].url, "http://example.com/manifest1.json");
        assert_eq!(urls[0].title, Some("manifest1".to_string()));
        assert_eq!(urls[1].url, "https://example.org/info.json");
        assert_eq!(urls[1].title, Some("info".to_string()));
    }

    #[test]
    fn test_parse_urls_with_custom_titles() {
        let content = "http://example.com/image1.jpg My Custom Title\nhttps://example.org/manifest.json Another Title";
        let urls = parse_text_urls(content).unwrap();

        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].url, "http://example.com/image1.jpg");
        assert_eq!(urls[0].title, Some("My Custom Title".to_string()));
        assert_eq!(urls[1].url, "https://example.org/manifest.json");
        assert_eq!(urls[1].title, Some("Another Title".to_string()));
    }

    #[test]
    fn test_parse_invalid_url() {
        let content = "not_a_valid_url";
        let result: Result<Vec<ZoomableImageUrl>, _> = parse_text_urls(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not_a_valid_url"));
    }

    #[test]
    fn test_extract_title_from_url() {
        assert_eq!(
            extract_title_from_url("http://example.com/image.jpg", 1),
            "image".to_string()
        );
        assert_eq!(
            extract_title_from_url("https://example.org/path/manifest.json", 2),
            "manifest".to_string()
        );
        assert_eq!(
            extract_title_from_url("http://example.com/", 3),
            "URL_3".to_string()
        );
        assert_eq!(
            extract_title_from_url("not_a_url", 4),
            "URL_4".to_string()
        );
    }

    #[test]
    fn test_images() {
        let mut dezoomer = BulkTextDezoomer;
        let content = "http://example.com/image1.jpg\nhttps://example.org/manifest.json".as_bytes();

        let input = DezoomerInput {
            uri: "file://test.txt".to_string(),
            contents: PageContents::Success(content.to_vec()),
        };

        let urls = expect_image_urls(dezoomer.dezoomer_result(&input).unwrap());
        assert_eq!(
            urls.iter().map(|img| match img {
            ZoomableImage::ImageUrl(url) => url.url.as_str(),
            ZoomableImage::Image(_) => panic!("Expected ImageUrl"),
        }).collect::<Vec<_>>(),
            [
                "http://example.com/image1.jpg",
                "https://example.org/manifest.json"
            ]
        );
    }

    #[test]
    fn test_images_empty_file() {
        let mut dezoomer = BulkTextDezoomer;
        let content = "# Only comments\n\n# Nothing else".as_bytes();

        let input = DezoomerInput {
            uri: "file://empty.txt".to_string(),
            contents: PageContents::Success(content.to_vec()),
        };

        let result = dezoomer.dezoomer_result(&input);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No valid URLs found"));
    }

    #[test]
    fn test_images_invalid_url() {
        let mut dezoomer = BulkTextDezoomer;
        let content = "not_a_valid_url".as_bytes();

        let input = DezoomerInput {
            uri: "file://invalid.txt".to_string(),
            contents: PageContents::Success(content.to_vec()),
        };

        assert_error_contains(dezoomer.dezoomer_result(&input), "line 1");
    }

    #[test]
    fn test_parse_quoted_titles_and_inline_comments() {
        let content = r#"
http://example.com/1.jpg "My Cool Title" # ignored comment
http://example.com/2.jpg 'Another Title'
http://example.com/3.jpg Plain title # comment
"#;
        let urls = parse_text_urls(content).unwrap();
        assert_eq!(urls.len(), 3);
        assert_eq!(urls[0].title, Some("My Cool Title".to_string()));
        assert_eq!(urls[1].title, Some("Another Title".to_string()));
        assert_eq!(urls[2].title, Some("Plain title".to_string()));
    }

    #[test]
    fn test_split_url_and_title_simple() {
        let (url, title) = split_url_and_title("http://x.jpg A title");
        assert_eq!(url, "http://x.jpg");
        assert_eq!(title, Some("A title"));
    }

    #[test]
    fn test_split_url_and_title_quoted() {
        let (url, title) = split_url_and_title("http://x.jpg \"Quoted Title\" # comment");
        assert_eq!(url, "http://x.jpg");
        assert_eq!(title, Some("Quoted Title"));
    }

    #[test]
    fn test_split_url_and_title_no_title() {
        let (url, title) = split_url_and_title("http://x.jpg");
        assert_eq!(url, "http://x.jpg");
        assert_eq!(title, None);
    }

    #[test]
    fn test_split_url_and_title_comment_requires_space() {
        let (url, title) = split_url_and_title("http://x.jpg My title#comment");
        assert_eq!(url, "http://x.jpg");
        assert_eq!(title, Some("My title#comment"));
    }

    #[test]
    fn test_split_url_and_title_comment_no_trailing_space() {
        let (url, title) = split_url_and_title("http://x.jpg My title #1");
        assert_eq!(url, "http://x.jpg");
        assert_eq!(title, Some("My title"));
    }

    #[test]
    fn test_split_url_and_title_trims_quoted_whitespace() {
        let (url, title) = split_url_and_title("http://x.jpg \"  Padded  \"");
        assert_eq!(url, "http://x.jpg");
        assert_eq!(title, Some("Padded"));
    }
}
