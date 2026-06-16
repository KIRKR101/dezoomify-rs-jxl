use std::convert::TryFrom;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use sanitize_filename_reader_friendly::sanitize;

use crate::{Vec2d, ZoomError};

const MAX_INDEXED_SUFFIXES: u32 = 10_000;

/// Suffix appended to reservation markers. The marker sits *next to* the
/// intended destination rather than *at* the destination, so a process
/// crash between reservation and encoding leaves a clearly-named `.tmp`
/// file alongside the (untouched) target rather than a 0-byte file at
/// the target path.
const RESERVATION_SUFFIX: &str = ".tmp";

pub(crate) fn reserve_output_file(path: &Path) -> Result<(), ZoomError> {
    let marker = reservation_marker(path);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)?;
    Ok(())
}

/// Return the path of the reservation marker used to claim `path`. The
/// marker is a sibling file with a `.tmp` suffix.
pub(crate) fn reservation_marker(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(RESERVATION_SUFFIX);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// Remove the reservation marker for `path`, if it exists. Best-effort: any
/// error (e.g. file already removed by a previous cleanup) is swallowed so
/// that callers can use this in `Drop` and on success/failure paths
/// without having to thread a `Result` through.
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

/// Try to reserve a candidate output path. On `AlreadyExists`, append the next
/// `_NNNN` suffix to the file stem and try again. Returns `Err` after
/// `MAX_INDEXED_SUFFIXES` collisions to bound the loop and avoid a panic on
/// pathological directories.
fn try_reserve_with_suffix(
    mut path: PathBuf,
    filename: &OsString,
    ext: &OsString,
    derived: bool,
) -> Result<PathBuf, ZoomError> {
    for i in 1..=MAX_INDEXED_SUFFIXES {
        match reserve_output_file(&path) {
            Ok(()) => return Ok(path),
            Err(ZoomError::Io { source })
                if derived && source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let mut name = OsString::from(filename);
                name.push(format!("_{i:04}."));
                name.push(ext);
                path.set_file_name(name);
            }
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

/// Compute an output path and atomically reserve it, falling back to indexed
/// suffixes when two callers race on the same derived title. Explicit outfiles
/// are reserved as-is and fail if they already exist.
pub fn reserve_unique_outname(
    outfile: &Option<PathBuf>,
    zoom_name: &Option<String>,
    base_dir: &Path,
    size: Option<Vec2d>,
) -> Result<PathBuf, ZoomError> {
    let path = get_outname(outfile, zoom_name, base_dir, size)?;

    // Only derived names get the indexed fallback; explicit outfiles keep the
    // original fail-if-exists behavior.
    let derived = outfile.is_none();
    let filename = path.file_stem().map(OsString::from).unwrap_or_default();
    let ext = path.extension().map(OsString::from).unwrap_or_default();

    try_reserve_with_suffix(path, &filename, &ext, derived)
}

pub fn get_outname(
    outfile: &Option<PathBuf>,
    zoom_name: &Option<String>,
    base_dir: &Path,
    size: Option<Vec2d>,
) -> Result<PathBuf, ZoomError> {
    // An image can be encoded as JPEG only if both its dimensions can be encoded as u16.
    // JXL has no such limitation, so it is used as the default extension.
    let fits_in_jpg = size.map(|Vec2d { x, y }| u16::try_from(x.max(y)).is_ok());
    let extension = "jxl";
    if let Some(path) = outfile {
        if let Some(forced_extension) = path.extension() {
            let ext = forced_extension.to_string_lossy().to_lowercase();
            if fits_in_jpg == Some(false) && (ext == "jpg" || ext == "jpeg") {
                return Err(ZoomError::Io {
                    source: std::io::Error::other(format!(
                        "The image ({:?} pixels) is too large to be saved as JPEG",
                        size
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
        // Collision-aware suffixing is performed by `reserve_unique_outname`
        // via `try_reserve_with_suffix` so that the file is also atomically
        // created (not merely checked for existence). Returning the bare
        // base name here keeps the suffixing logic in a single place.
        Ok(path)
    }
}

#[allow(clippy::expect_fun_call)]
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

        // First call reserves the base name. The reservation marker is a
        // sibling `.tmp` file; the destination path itself is left untouched
        // so that a crash between reservation and encoding does not leave a
        // 0-byte file at the requested output name.
        let first = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size).unwrap();
        assert_eq!(first.file_name().unwrap(), "collision.jxl");
        assert!(
            !first.exists(),
            "destination must not be created during reservation"
        );
        assert!(reservation_marker(&first).exists());

        // Second call with the same title must pick a suffix.
        let second = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size).unwrap();
        assert_eq!(second.file_name().unwrap(), "collision_0001.jxl");
        assert!(reservation_marker(&second).exists());
    }

    #[test]
    fn reserve_unique_outname_returns_error_after_max_collisions() {
        // Pre-seed the directory so that *every* candidate suffix collides, then
        // verify the function returns an error rather than looping forever.
        let base_dir = TempDirBuilder::new()
            .prefix("dezoomify-rs-test-reserve-saturation")
            .tempdir()
            .unwrap();
        let zoom_name = Some("flood".to_string());
        let size = Some(Vec2d { x: 100, y: 100 });

        // Reserve the base name first.
        let _ = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size).unwrap();
        // Then pre-create all suffixed marker variants up to the limit.
        for i in 1..=MAX_INDEXED_SUFFIXES {
            let name = format!("flood_{i:04}.jxl.tmp");
            File::create(base_dir.as_ref().join(&name)).expect("pre-create collision");
        }

        let result = reserve_unique_outname(&None, &zoom_name, base_dir.as_ref(), size);
        assert!(
            result.is_err(),
            "expected an error when all suffixes are taken"
        );
    }

    #[test]
    fn default_extension_is_jxl() {
        let base_dir = TempDirBuilder::new()
            .prefix("dezoomify-rs-test-jxl")
            .tempdir()
            .unwrap();
        let base = |s| base_dir.as_ref().join(s);
        let tests = vec![
            // outfile, zoom_name, size, expected_result
            (None, Some("hello".to_string()), None, base("hello.jxl")),
            (
                None,
                Some("hello".to_string()),
                Some(Vec2d { x: 1000, y: 1000 }),
                base("hello.jxl"),
            ),
            (None, Some(String::new()), None, base("dezoomified.jxl")),
            (None, None, None, base("dezoomified.jxl")),
            (
                None,
                None,
                Some(Vec2d { x: 1000, y: 1000 }),
                base("dezoomified.jxl"),
            ),
            (
                Some("test.tiff".into()),
                Some("hello".to_string()),
                Some(Vec2d { x: 1000, y: 1000 }),
                "test.tiff".into(),
            ),
        ];
        for (outfile, zoom_name, size, expected_result) in tests.into_iter() {
            let outname = get_outname(&outfile, &zoom_name, base_dir.as_ref(), size).unwrap();
            assert_eq!(outname, expected_result);
        }
    }
}
