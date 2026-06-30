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
pub fn screenshot(
    target: CaptureTarget,
    format: Option<ImageFormat>,
    settle_ms: Option<u64>,
    await_change: bool,
) -> RemoteResult<Reply> {
    // Wake DWM first: on an idle session it throttles compositing, so a
    // just-launched window's first frame can come back black until something
    // moves. A net-zero cursor jiggle reliably kicks it (so an Agent never has
    // to remember to nudge the mouse before a screenshot).
    crate::input::nudge();
    let image = match settle_ms {
        // The settle loop re-captures, so it naturally catches the post-wake frame.
        Some(ms) => capture_settled(target, ms, await_change)?,
        // Single shot: give DWM a moment to repaint after the nudge.
        None => {
            std::thread::sleep(std::time::Duration::from_millis(150));
            capture(target)?
        }
    };
    let (width, height) = (image.width(), image.height());

    let (format, data) = match format {
        Some(ImageFormat::Png) => (ImageFormat::Png, encode(&image, image::ImageFormat::Png)?),
        _ => match encode(&image, image::ImageFormat::WebP) {
            Ok(data) => (ImageFormat::Webp, data),
            Err(_) => (ImageFormat::Png, encode(&image, image::ImageFormat::Png)?),
        },
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
        CaptureTarget::Element(element) => {
            let r = crate::uia::element_rect(&element.0)?;
            crop_monitor(r.x, r.y, r.width.max(1) as u32, r.height.max(1) as u32)
        }
        CaptureTarget::FullScreen => arc_capture::capture_primary_monitor().map_err(map_err),
        CaptureTarget::Region {
            x,
            y,
            width,
            height,
        } => crop_monitor(x, y, width, height),
    }
}

/// Captures repeatedly until two consecutive frames are stable (or `timeout_ms`
/// elapses), returning the latest — a reliable replacement for a blind
/// "wait for the app to render" sleep after launching/opening a window.
fn capture_settled(
    target: CaptureTarget,
    timeout_ms: u64,
    await_change: bool,
) -> RemoteResult<RgbaImage> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let first = capture(target.clone())?;
    let mut prev = first.clone();
    // When awaiting change (e.g. a just-launched window), don't accept stability
    // until the frame has differed from the initial backdrop at least once.
    let mut changed = !await_change;
    loop {
        if Instant::now() >= deadline {
            return Ok(prev);
        }
        std::thread::sleep(Duration::from_millis(300));
        let next = capture(target.clone())?;
        if !changed && !stable(&first, &next) {
            changed = true;
        }
        if changed && stable(&prev, &next) {
            return Ok(next);
        }
        prev = next;
    }
}

/// Whether two frames are close enough to call the UI settled: same dimensions
/// and fewer than ~0.3% of sampled bytes differ meaningfully.
fn stable(a: &RgbaImage, b: &RgbaImage) -> bool {
    if a.dimensions() != b.dimensions() {
        return false;
    }
    let (a, b) = (a.as_raw(), b.as_raw());
    let (mut differing, mut sampled) = (0usize, 0usize);
    for i in (0..a.len()).step_by(16) {
        sampled += 1;
        if a[i].abs_diff(b[i]) > 8 {
            differing += 1;
        }
    }
    sampled == 0 || differing * 1000 / sampled < 3
}

/// Captures the primary monitor and crops to a screen rectangle.
fn crop_monitor(x: i32, y: i32, width: u32, height: u32) -> RemoteResult<RgbaImage> {
    let full = arc_capture::capture_primary_monitor().map_err(map_err)?;
    Ok(
        image::imageops::crop_imm(&full, x.max(0) as u32, y.max(0) as u32, width, height)
            .to_image(),
    )
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
