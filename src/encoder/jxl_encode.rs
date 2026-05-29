use std::ffi::c_void;

use jpegxl_sys::common::types::{JxlBool, JxlBoxType, JxlDataType, JxlEndianness, JxlPixelFormat};
use jpegxl_sys::encoder::encode::*;
use jpegxl_sys::metadata::codestream_header::JxlBasicInfo;
use jpegxl_sys::threads::resizable_parallel_runner::*;

pub struct JxlEncoder {
    enc: *mut jpegxl_sys::encoder::encode::JxlEncoder,
    runner: *mut c_void,
}

// SAFETY: libjxl's encoder is thread-safe per the C API docs.
// The runner is internally synchronized.
unsafe impl Send for JxlEncoder {}

fn check(status: JxlEncoderStatus) -> Result<(), String> {
    match status {
        JxlEncoderStatus::Success => Ok(()),
        JxlEncoderStatus::Error => Err("JxlEncoder error".into()),
        JxlEncoderStatus::NeedMoreOutput => Ok(()),
    }
}

impl JxlEncoder {
    pub fn create() -> Result<Self, String> {
        // SAFETY: null pointer is valid for the memory manager parameter;
        // libjxl uses the default allocator when null is passed.
        let enc = unsafe { JxlEncoderCreate(std::ptr::null()) };
        if enc.is_null() {
            return Err("JxlEncoderCreate returned null".into());
        }

        // SAFETY: null memory manager is valid; runner is created in
        // single-threaded mode until SetThreads is called.
        let runner = unsafe { JxlResizableParallelRunnerCreate(std::ptr::null()) };

        // SAFETY: enc and runner are valid non-null pointers.
        let status =
            unsafe { JxlEncoderSetParallelRunner(enc, JxlResizableParallelRunner, runner) };
        if check(status).is_err() {
            // SAFETY: runner and enc were never used for encoding, safe to destroy.
            unsafe {
                JxlResizableParallelRunnerDestroy(runner);
                JxlEncoderDestroy(enc);
            }
            return Err("failed to set parallel runner".into());
        }

        Ok(Self { enc, runner })
    }

    pub fn set_basic_info(
        &self,
        width: u32,
        height: u32,
        has_alpha: bool,
        uses_original_profile: bool,
    ) -> Result<(), String> {
        // SAFETY: runner is a valid handle, num_threads is in range.
        let num_threads =
            unsafe { JxlResizableParallelRunnerSuggestThreads(width as u64, height as u64) };
        // SAFETY: runner is valid. Setting threads after the runner was already
        // attached to the encoder is supported by libjxl.
        unsafe { JxlResizableParallelRunnerSetThreads(self.runner, num_threads as usize) };

        // SAFETY: MaybeUninit is valid for uninitialized memory; JxlEncoderInitBasicInfo
        // initializes all fields to default values.
        let mut info: std::mem::MaybeUninit<JxlBasicInfo> = std::mem::MaybeUninit::uninit();
        unsafe { JxlEncoderInitBasicInfo(info.as_mut_ptr()) };
        // SAFETY: JxlEncoderInitBasicInfo fully initialized the struct.
        let mut info = unsafe { info.assume_init() };
        info.xsize = width;
        info.ysize = height;
        info.bits_per_sample = 8;
        if has_alpha {
            info.num_color_channels = 3;
            info.num_extra_channels = 1;
            info.alpha_bits = 8;
        }
        info.uses_original_profile = JxlBool::from(uses_original_profile);
        // SAFETY: enc and info are valid; info was initialized by InitBasicInfo.
        let status = unsafe { JxlEncoderSetBasicInfo(self.enc, &info) };
        check(status)
    }

    pub fn set_icc_profile(&self, profile: &[u8]) -> Result<(), String> {
        // SAFETY: enc is valid, profile pointer and length are valid for the
        // rust slice lifetime. Must be called after SetBasicInfo.
        let status = unsafe { JxlEncoderSetICCProfile(self.enc, profile.as_ptr(), profile.len()) };
        check(status)
    }

    pub fn encode_frame(
        &mut self,
        data: &[u8],
        has_alpha: bool,
        quality: f32,
        effort: u8,
        exif: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        // SAFETY: quality is a float, the C function has no safety requirements.
        let distance = unsafe { JxlEncoderDistanceFromQuality(quality) };

        // SAFETY: enc is valid; null source means use default frame settings.
        let frame_settings = unsafe { JxlEncoderFrameSettingsCreate(self.enc, std::ptr::null()) };

        if quality >= 100.0 {
            // SAFETY: frame_settings is valid.
            unsafe { JxlEncoderSetFrameLossless(frame_settings, JxlBool::True) };
        } else {
            // SAFETY: frame_settings is valid, distance is a float.
            unsafe { JxlEncoderSetFrameDistance(frame_settings, distance) };
        }

        // Map effort (1-9) from compression parameter.
        let effort = effort.clamp(1, 9) as i64;
        // SAFETY: frame_settings is valid, effort is in range [1, 9].
        unsafe {
            JxlEncoderFrameSettingsSetOption(
                frame_settings,
                JxlEncoderFrameSettingId::Effort,
                effort,
            )
        };

        // Enable progressive encoding at lower effort levels for faster
        // preview on large images. Disable at effort >= 7 where the user
        // wants maximum compression density over progressive decode support.
        // (Progressive passes add ~5–15% bitstream overhead.)
        // SAFETY: frame_settings is valid, option values are 0/1.
        if effort < 7 {
            unsafe {
                JxlEncoderFrameSettingsSetOption(
                    frame_settings,
                    JxlEncoderFrameSettingId::ProgressiveAc,
                    1,
                );
                JxlEncoderFrameSettingsSetOption(
                    frame_settings,
                    JxlEncoderFrameSettingId::ProgressiveDc,
                    1,
                );
                JxlEncoderFrameSettingsSetOption(
                    frame_settings,
                    JxlEncoderFrameSettingId::QprogressiveAc,
                    1,
                );
            }
        }

        let num_channels: u32 = if has_alpha { 4 } else { 3 };
        let pixel_format = JxlPixelFormat {
            num_channels,
            data_type: JxlDataType::Uint8,
            endianness: JxlEndianness::Native,
            align: 0,
        };

        // Enable container format if we need to add EXIF metadata boxes.
        if exif.is_some() {
            // SAFETY: enc is valid. Must be called before adding any box.
            unsafe { JxlEncoderUseBoxes(self.enc) };
        }

        // SAFETY: frame_settings and pixel_format are valid; data pointer and
        // length cover the full pixel buffer for the frame.
        let status = unsafe {
            JxlEncoderAddImageFrame(
                frame_settings,
                &pixel_format,
                data.as_ptr() as *const c_void,
                data.len(),
            )
        };
        check(status)?;

        // Add EXIF box if present. The contents must be prepended by a 4-byte
        // TIFF header offset (all zeros when the header follows immediately).
        if let Some(exif_data) = exif {
            let mut box_contents = vec![0u8; 4];
            box_contents.extend_from_slice(exif_data);
            let box_type = JxlBoxType([
                b'E' as std::ffi::c_char,
                b'x' as std::ffi::c_char,
                b'i' as std::ffi::c_char,
                b'f' as std::ffi::c_char,
            ]);
            // SAFETY: enc is valid, box_type is a valid 4-byte type,
            // contents pointer and length are valid for the slice lifetime.
            let status = unsafe {
                JxlEncoderAddBox(
                    self.enc,
                    &box_type,
                    box_contents.as_ptr(),
                    box_contents.len(),
                    JxlBool::False,
                )
            };
            check(status)?;

            // SAFETY: enc is valid; signals that no more boxes will be added.
            unsafe { JxlEncoderCloseBoxes(self.enc) };
        }

        // SAFETY: enc is valid; signals that no more frames will be added.
        unsafe { JxlEncoderCloseInput(self.enc) };

        self.collect_output()
    }

    fn collect_output(&mut self) -> Result<Vec<u8>, String> {
        // Heuristic: pre-size output to roughly the uncompressed size / 4
        // (a typical conservative compression ratio). This avoids most
        // reallocations without over-allocating too much for lossless.
        let mut output = Vec::with_capacity(1024 * 1024); // 1 MiB initial
        let mut buf = vec![0u8; 1024 * 1024]; // 1 MiB chunk
        loop {
            let mut next_out = buf.as_mut_ptr();
            let mut avail_out = buf.len();

            // SAFETY: enc is valid, next_out points to a writable buffer of
            // avail_out bytes. avail_out >= 32 per libjxl requirements.
            let status =
                unsafe { JxlEncoderProcessOutput(self.enc, &mut next_out, &mut avail_out) };

            let written = buf.len() - avail_out;
            output.extend_from_slice(&buf[..written]);

            match status {
                JxlEncoderStatus::Success => break,
                JxlEncoderStatus::NeedMoreOutput => {}
                _ => return Err("JxlEncoderProcessOutput error".into()),
            }
        }
        Ok(output)
    }
}

impl Drop for JxlEncoder {
    fn drop(&mut self) {
        // SAFETY: runner and enc are valid; destroying them after encoding is
        // complete is required by the libjxl API.
        unsafe { JxlResizableParallelRunnerDestroy(self.runner) };
        unsafe { JxlEncoderDestroy(self.enc) };
    }
}
