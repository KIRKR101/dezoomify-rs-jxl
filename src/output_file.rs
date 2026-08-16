use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use sanitize_filename_reader_friendly::sanitize;

use crate::{Vec2d, ZoomError};

const MAX_INDEXED_SUFFIXES: u32 = 10_000;

const RESERVATION_SUFFIX: &str = ".tmp";

pub(crate) fn reserve_output_file(path: &Path) -> Result<(), ZoomError> {
    let marker = reservation_marker(path);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&marker)?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn reservation_marker(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(RESERVATION_SUFFIX);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

pub(crate) fn release_reservation(path: &Path) {
    let marker = reservation_marker(path);
    if let Err(e) = std::fs::remove_file(&marker)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::debug!(
            "Failed to remove reservation marker {}: {e}",
            marker.display()
        );
    }
}

fn try_reserve_with_suffix(
    mut path: PathBuf,
    filename: &OsString,
    ext: &OsString,
    derived: bool,
) -> Result<PathBuf, ZoomError> {
    for i in 1..=MAX_INDEXED_SUFFIXES {
        if derived && path.exists() {
            let mut name = OsString::from(filename);
            name.push(format!("_{i:04}."));
            name.push(ext);
            path.set_file_name(name);
            continue;
        }
        match reserve_output_file(&path) {
            Ok(()) => return Ok(path),
            Err(e) => return Err(e),
        }
    }
    Err(ZoomError::Io {
        source: std::io::Error::other(format!(
            "Could not find a free output filename after {MAX_INDEXED_SUFFIXES} \
             suffixed attempts. Please clean up the target directory."
        )),
    })
}

#[allow(clippy::ref_option)]
pub fn reserve_unique_outname(
    outfile: &Option<PathBuf>,
    zoom_name: &Option<String>,
    base_dir: &Path,
    size: Option<Vec2d>,
) -> Result<PathBuf, ZoomError> {
    let path = get_outname(outfile, zoom_name, base_dir, size)?;

    let derived = outfile.is_none();
    let filename = path.file_stem().map(OsString::from).unwrap_or_default();
    let ext = path.extension().map(OsString::from).unwrap_or_default();

    try_reserve_with_suffix(path, &filename, &ext, derived)
}

#[allow(clippy::ref_option)]
pub fn get_outname(
    outfile: &Option<PathBuf>,
    zoom_name: &Option<String>,
    base_dir: &Path,
    size: Option<Vec2d>,
) -> Result<PathBuf, ZoomError> {
    let fits_in_jpg = size.map(|Vec2d { x, y }| u16::try_from(x.max(y)).is_ok());
    let extension = "jxl";
    if let Some(path) = outfile {
        if let Some(forced_extension) = path.extension() {
            let ext = forced_extension.to_string_lossy().to_lowercase();
            if fits_in_jpg == Some(false) && (ext == "jpg" || ext == "jpeg") {
                return Err(ZoomError::Io {
                    source: std::io::Error::other(format!(
                        "The image ({size:?} pixels) is too large to be saved as JPEG",
                    )),
                });
            }
            Ok(path.into())
        } else {
            Ok(path.with_extension(extension))
        }
    } else {
        let base = zoom_name
            .as_ref()
            .map(|s| sanitize(s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "dezoomified".into());
        let mut path = base_dir.to_path_buf();
        let mut base_with_ext = OsString::from(&base);
        base_with_ext.push(".");
        base_with_ext.push(extension);
        path = path.join(base_with_ext);
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use tempfile::Builder as TempDirBuilder;

    use super::*;

    #[test]
    fn reserve_unique_outname_falls_back_on_collision() {
        let base_dir = TempDirBuilder::new()
            .prefix("dezoomify-rs-test-reserve-unique")
            .tempdir()
            .unwrap();
        let zoom_name = Some("collision".to_string());
        let size = Some(Vec2d { x: 100, y: 100 });

        let first = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size).unwrap();
        assert_eq!(first.file_name().unwrap(), "collision.jxl");
        assert!(
            !first.exists(),
            "destination must not be created during reservation"
        );
        assert!(reservation_marker(&first).exists());

        File::create(&first).unwrap();

        let second = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size).unwrap();
        assert_eq!(second.file_name().unwrap(), "collision_0001.jxl");
        assert!(reservation_marker(&second).exists());
    }

    #[test]
    fn reserve_unique_outname_returns_error_after_max_collisions() {
        let base_dir = TempDirBuilder::new()
            .prefix("dezoomify-rs-test-reserve-saturation")
            .tempdir()
            .unwrap();
        let zoom_name = Some("flood".to_string());
        let size = Some(Vec2d { x: 100, y: 100 });

        let first = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size).unwrap();
        File::create(&first).expect("pre-create base collision");
        for i in 1..=MAX_INDEXED_SUFFIXES {
            let name = format!("flood_{i:04}.jxl");
            File::create(base_dir.as_ref().join(&name)).expect("pre-create collision");
        }

        let result = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size);
        assert!(
            result.is_err(),
            "expected an error when all suffixes are taken"
        );
    }

    #[test]
    fn reservation_release_cleans_up() {
        let base_dir = TempDirBuilder::new()
            .prefix("dezoomify-rs-test-release")
            .tempdir()
            .unwrap();
        let name = base_dir.as_ref().join("test-release.jxl");

        let marker = reservation_marker(&name);
        reserve_output_file(&name).unwrap();
        assert!(marker.exists());

        release_reservation(&name);
        assert!(!marker.exists(), "marker should be removed by release");
    }

    #[test]
    fn get_outname_basics() {
        let base_dir = TempDirBuilder::new()
            .prefix("dezoomify-rs-test-outname")
            .tempdir()
            .unwrap();

        let tests = vec![
            (
                None,
                Some("hello".to_string()),
                Some(Vec2d { x: 100, y: 100 }),
                base_dir.as_ref().join("hello.jxl"),
            ),
            (
                None,
                None,
                Some(Vec2d { x: 100, y: 100 }),
                base_dir.as_ref().join("dezoomified.jxl"),
            ),
            (
                Some("test.tiff".into()),
                Some("hello".to_string()),
                Some(Vec2d { x: 1000, y: 1000 }),
                "test.tiff".into(),
            ),
        ];
        for (outfile, zoom_name, size, expected_result) in tests {
            let outname = get_outname(&outfile, &zoom_name, base_dir.as_ref(), size).unwrap();
            assert_eq!(outname, expected_result);
        }
    }

    #[test]
    fn get_outname_rejects_too_large_jpeg() {
        let base_dir = TempDirBuilder::new()
            .prefix("dezoomify-rs-test-jpeg-size")
            .tempdir()
            .unwrap();
        let result = get_outname(
            &Some("out.jpg".into()),
            &None,
            base_dir.as_ref(),
            Some(Vec2d {
                x: 100_000,
                y: 100_000,
            }),
        );
        assert!(result.is_err());
    }
}
