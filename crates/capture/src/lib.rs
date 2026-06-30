//! Windows screen/window capture for arc.
//!
//! Window capture uses **Windows.Graphics.Capture** (WGC) first: unlike GDI
//! `PrintWindow`, it captures the DWM-composed frame, so DirectComposition /
//! swap-chain apps (WinUI 3, Electron, Chromium) come out correct rather than
//! black, and it works in a detached/disconnected RDP session. GDI is the
//! fallback. Self-maintained on arc's own `windows` crate version — no foreign
//! windows-rs pulled in by a capture dependency.
//!
//! The API is deliberately proto-free: it deals in window handles and
//! [`image::RgbaImage`], and the runner maps `CaptureTarget` / encodes WebP.

use image::RgbaImage;

/// A capture failure, categorised so the caller can map it to a protocol error.
#[derive(Debug)]
pub enum CaptureError {
    /// The requested window/monitor does not exist.
    NotFound(String),
    /// Capture is not available on this platform/build.
    Unsupported(String),
    /// Capture was attempted but failed.
    Failed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NotFound(m) | CaptureError::Unsupported(m) | CaptureError::Failed(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for CaptureError {}

/// Result alias for capture operations.
pub type Result<T> = std::result::Result<T, CaptureError>;

/// Captures the window with handle `hwnd` (a raw `HWND` as `isize`).
///
/// Tries WGC first (correct for composed/GPU content, works detached); on
/// failure falls back to GDI `PrintWindow`.
pub fn capture_window(hwnd: isize) -> Result<RgbaImage> {
    imp::capture_window(hwnd)
}

/// Captures the primary monitor (WGC first, GDI `BitBlt` fallback).
pub fn capture_primary_monitor() -> Result<RgbaImage> {
    imp::capture_primary_monitor()
}

#[cfg(windows)]
mod gdi;
#[cfg(windows)]
mod wgc;

#[cfg(windows)]
mod imp {
    use super::{CaptureError, Result, gdi, wgc};
    use image::RgbaImage;

    /// Combines a primary (WGC) and fallback (GDI) error into one message,
    /// preserving the primary's category.
    fn combined(primary: CaptureError, fallback: CaptureError) -> CaptureError {
        let msg = format!("WGC: {primary}; GDI fallback: {fallback}");
        match primary {
            CaptureError::NotFound(_) => CaptureError::NotFound(msg),
            _ => CaptureError::Failed(msg),
        }
    }

    pub fn capture_window(hwnd: isize) -> Result<RgbaImage> {
        // Per-window WGC (composed content, occlusion-proof) → monitor-crop
        // (visible static windows that don't present per-window frames) → GDI.
        match wgc::capture_window(hwnd) {
            Ok(image) => Ok(image),
            Err(primary) => match wgc::capture_window_region(hwnd) {
                Ok(image) => Ok(image),
                Err(_) => gdi::capture_window(hwnd)
                    .map_err(|fallback| combined(primary, fallback)),
            },
        }
    }

    pub fn capture_primary_monitor() -> Result<RgbaImage> {
        match wgc::capture_primary_monitor() {
            Ok(image) => Ok(image),
            Err(primary) => {
                gdi::capture_primary_monitor().map_err(|fallback| combined(primary, fallback))
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{CaptureError, Result};
    use image::RgbaImage;

    fn unsupported() -> CaptureError {
        CaptureError::Unsupported("screen capture is only supported on Windows".to_owned())
    }

    pub fn capture_window(_hwnd: isize) -> Result<RgbaImage> {
        Err(unsupported())
    }
    pub fn capture_primary_monitor() -> Result<RgbaImage> {
        Err(unsupported())
    }
}
