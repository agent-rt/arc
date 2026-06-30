//! Screen / window capture, encoded to WebP (PNG fallback).
//!
//! The actual pixel grab lives in the [`arc_capture`] crate (WGC-first, GDI
//! fallback — correct for WinUI 3 / Chromium windows and detached sessions).
//! This module just maps [`CaptureTarget`], crops regions, and encodes.

use std::io::Cursor;

use arc_proto::id::WindowId;
use arc_proto::wire::{CaptureTarget, Image, ImageFormat, RemoteError, Reply};
use image::RgbaImage;

use crate::dispatch::{RemoteResult, not_found, os_error};

/// Captures the requested target and returns the encoded [`Reply::Image`].
///
/// Prefers WebP (smaller over the relay and for the Agent's vision model);
/// falls back to PNG if the WebP encoder rejects the frame.
pub fn screenshot(target: CaptureTarget) -> RemoteResult<Reply> {
    let image = capture(target)?;
    let (width, height) = (image.width(), image.height());

    let (format, data) = match encode(&image, image::ImageFormat::WebP) {
        Ok(data) => (ImageFormat::Webp, data),
        Err(_) => (ImageFormat::Png, encode(&image, image::ImageFormat::Png)?),
    };

    Ok(Reply::Image(Image {
        format,
        width,
        height,
        data,
    }))
}

/// Maps a [`CaptureTarget`] onto the capture crate, cropping regions locally.
fn capture(target: CaptureTarget) -> RemoteResult<RgbaImage> {
    match target {
        CaptureTarget::Window(WindowId(id)) => {
            arc_capture::capture_window(id as isize).map_err(map_err)
        }
        CaptureTarget::FullScreen => arc_capture::capture_primary_monitor().map_err(map_err),
        CaptureTarget::Region {
            x,
            y,
            width,
            height,
        } => {
            let full = arc_capture::capture_primary_monitor().map_err(map_err)?;
            Ok(
                image::imageops::crop_imm(&full, x.max(0) as u32, y.max(0) as u32, width, height)
                    .to_image(),
            )
        }
    }
}

/// Maps a capture error to a protocol error, preserving the not-found category.
fn map_err(error: arc_capture::CaptureError) -> RemoteError {
    match error {
        arc_capture::CaptureError::NotFound(message) => not_found(message),
        other => os_error(other.to_string()),
    }
}

/// Encodes an RGBA frame to the given image format.
fn encode(image: &RgbaImage, format: image::ImageFormat) -> RemoteResult<Vec<u8>> {
    let mut data = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut data), format)
        .map_err(|e| os_error(format!("{format:?} encode failed: {e}")))?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::encode;
    use image::RgbaImage;

    #[test]
    fn webp_encodes_and_decodes() {
        let frame = RgbaImage::from_fn(64, 48, |x, y| image::Rgba([x as u8, y as u8, 128, 255]));
        let data = encode(&frame, image::ImageFormat::WebP).expect("webp encode");
        assert!(!data.is_empty(), "webp output is empty");
        let decoded = image::load_from_memory_with_format(&data, image::ImageFormat::WebP)
            .expect("webp decode");
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
    }
}
