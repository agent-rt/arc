//! Screen, window and region capture, encoded to WebP (PNG fallback).
//!
//! Primary path is `xcap` (DPI-aware, multi-monitor). On Windows, when `xcap`'s
//! DXGI desktop duplication fails — e.g. `0x80070006` in a detached / SSH /
//! disconnected-RDP session — we fall back to GDI: `PrintWindow` for a specific
//! window (which asks the window to render itself, and works without an active
//! display surface) and `BitBlt` for full-screen/region.

use std::io::Cursor;

use arc_proto::id::WindowId;
use arc_proto::wire::{CaptureTarget, Image, ImageFormat, Reply};
use image::RgbaImage;

use arc_proto::wire::RemoteError;

use crate::dispatch::{RemoteResult, not_found, os_error};

/// Captures the requested target and returns the encoded [`Reply::Image`].
///
/// Prefers WebP (smaller payload over the relay and for the Agent's vision
/// model); falls back to PNG if the WebP encoder rejects the frame.
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

/// Encodes an RGBA frame to the given image format.
fn encode(image: &RgbaImage, format: image::ImageFormat) -> RemoteResult<Vec<u8>> {
    let mut data = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut data), format)
        .map_err(|e| os_error(format!("{format:?} encode failed: {e}")))?;
    Ok(data)
}

/// `xcap` first; on Windows fall back to GDI when it fails.
fn capture(target: CaptureTarget) -> RemoteResult<RgbaImage> {
    match capture_xcap(target) {
        Ok(image) => Ok(image),
        Err(primary) => fallback(target, primary),
    }
}

#[cfg(windows)]
fn fallback(target: CaptureTarget, primary: RemoteError) -> RemoteResult<RgbaImage> {
    gdi::capture(target).map_err(|secondary| {
        os_error(format!(
            "capture failed (xcap: {}; gdi fallback: {})",
            primary.message, secondary.message
        ))
    })
}

#[cfg(not(windows))]
fn fallback(_target: CaptureTarget, primary: RemoteError) -> RemoteResult<RgbaImage> {
    Err(primary)
}

fn capture_xcap(target: CaptureTarget) -> RemoteResult<RgbaImage> {
    match target {
        CaptureTarget::FullScreen => primary_monitor()?
            .capture_image()
            .map_err(|e| os_error(format!("capture failed: {e}"))),
        CaptureTarget::Window(WindowId(id)) => {
            let windows =
                xcap::Window::all().map_err(|e| os_error(format!("enumerate windows: {e}")))?;
            let window = windows
                .into_iter()
                .find(|w| u64::from(w.id()) == id)
                .ok_or_else(|| not_found(format!("window {id} not found")))?;
            window
                .capture_image()
                .map_err(|e| os_error(format!("capture failed: {e}")))
        }
        CaptureTarget::Region {
            x,
            y,
            width,
            height,
        } => {
            let full = primary_monitor()?
                .capture_image()
                .map_err(|e| os_error(format!("capture failed: {e}")))?;
            let cropped =
                image::imageops::crop_imm(&full, x.max(0) as u32, y.max(0) as u32, width, height)
                    .to_image();
            Ok(cropped)
        }
    }
}

fn primary_monitor() -> RemoteResult<xcap::Monitor> {
    let monitors =
        xcap::Monitor::all().map_err(|e| os_error(format!("enumerate monitors: {e}")))?;
    monitors
        .into_iter()
        .next()
        .ok_or_else(|| not_found("no monitor available".to_owned()))
}

/// GDI fallback capture for detached sessions where DXGI duplication is
/// unavailable.
#[cfg(windows)]
mod gdi {
    use arc_proto::id::WindowId;
    use arc_proto::wire::CaptureTarget;
    use image::RgbaImage;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{BITMAPINFO, BITMAPINFOHEADER};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject,
        GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
    };
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, GetWindowRect, SM_CXSCREEN, SM_CYSCREEN,
    };

    use crate::dispatch::{RemoteResult, os_error};

    /// `PrintWindow` flag that renders DirectComposition / layered content too.
    const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);

    pub fn capture(target: CaptureTarget) -> RemoteResult<RgbaImage> {
        match target {
            CaptureTarget::Window(WindowId(id)) => {
                capture_window(HWND(id as *mut core::ffi::c_void))
            }
            CaptureTarget::FullScreen => {
                // SAFETY: GetSystemMetrics has no preconditions.
                let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
                let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
                capture_rect(0, 0, width, height)
            }
            CaptureTarget::Region {
                x,
                y,
                width,
                height,
            } => capture_rect(x, y, width as i32, height as i32),
        }
    }

    /// Captures a window by asking it to render itself via `PrintWindow`.
    fn capture_window(hwnd: HWND) -> RemoteResult<RgbaImage> {
        let mut rect = RECT::default();
        // SAFETY: hwnd originates from list_windows; GetWindowRect errors on a
        // stale handle.
        unsafe { GetWindowRect(hwnd, &mut rect) }
            .map_err(|e| os_error(format!("GetWindowRect: {e}")))?;
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);

        // SAFETY: window DC is released below regardless of outcome.
        let window_dc = unsafe { GetDC(hwnd) };
        if window_dc.is_invalid() {
            return Err(os_error("GetDC(window) failed".to_owned()));
        }
        let result = blit_capture(window_dc, width, height, |mem_dc| {
            // SAFETY: mem_dc is a valid compatible DC for this window.
            unsafe { PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT) }.as_bool()
        });
        // SAFETY: balances the GetDC above.
        unsafe { ReleaseDC(hwnd, window_dc) };
        result
    }

    /// Captures a screen rectangle via `BitBlt`.
    fn capture_rect(x: i32, y: i32, width: i32, height: i32) -> RemoteResult<RgbaImage> {
        let width = width.max(1);
        let height = height.max(1);
        // SAFETY: a null HWND yields the whole-screen DC, released below.
        let screen_dc = unsafe { GetDC(HWND::default()) };
        if screen_dc.is_invalid() {
            return Err(os_error("GetDC(screen) failed".to_owned()));
        }
        let result = blit_capture(screen_dc, width, height, |mem_dc| {
            // SAFETY: copies the screen region into the compatible mem DC.
            unsafe { BitBlt(mem_dc, 0, 0, width, height, screen_dc, x, y, SRCCOPY) }.is_ok()
        });
        // SAFETY: balances the GetDC above.
        unsafe { ReleaseDC(HWND::default(), screen_dc) };
        result
    }

    /// Creates a memory DC + bitmap, lets `render` draw into it, reads the
    /// pixels, and frees every GDI object on all paths.
    fn blit_capture(
        reference_dc: HDC,
        width: i32,
        height: i32,
        render: impl FnOnce(HDC) -> bool,
    ) -> RemoteResult<RgbaImage> {
        // SAFETY: all handles created here are released before returning.
        unsafe {
            let mem_dc = CreateCompatibleDC(reference_dc);
            if mem_dc.is_invalid() {
                return Err(os_error("CreateCompatibleDC failed".to_owned()));
            }
            let bitmap = CreateCompatibleBitmap(reference_dc, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(mem_dc);
                return Err(os_error("CreateCompatibleBitmap failed".to_owned()));
            }
            let previous = SelectObject(mem_dc, HGDIOBJ(bitmap.0));
            let outcome = if render(mem_dc) {
                read_pixels(mem_dc, bitmap, width, height)
            } else {
                Err(os_error("render into DC failed".to_owned()))
            };
            SelectObject(mem_dc, previous);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(mem_dc);
            outcome
        }
    }

    /// Reads a 32-bpp top-down DIB out of `bitmap` and converts BGRX → RGBA.
    fn read_pixels(
        mem_dc: HDC,
        bitmap: HBITMAP,
        width: i32,
        height: i32,
    ) -> RemoteResult<RgbaImage> {
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative = top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buffer = vec![0u8; width as usize * height as usize * 4];
        // SAFETY: buffer holds exactly width*height*4 bytes, matching the 32-bpp
        // request in `info`; `bitmap` is selected into `mem_dc`.
        let scanlines = unsafe {
            GetDIBits(
                mem_dc,
                bitmap,
                0,
                height as u32,
                Some(buffer.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if scanlines == 0 {
            return Err(os_error("GetDIBits failed".to_owned()));
        }
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2); // BGR(X) → RGB
            pixel[3] = 255; // force opaque (the 4th GDI byte is undefined)
        }
        RgbaImage::from_raw(width as u32, height as u32, buffer)
            .ok_or_else(|| os_error("rgba buffer size mismatch".to_owned()))
    }
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
