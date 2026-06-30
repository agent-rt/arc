//! GDI fallback: `PrintWindow` for a window, `BitBlt` for the screen. Works
//! without a GPU/active duplication surface, but cannot read DirectComposition
//! content (hence WGC is tried first).

use core::ffi::c_void;

use image::RgbaImage;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowRect, SM_CXSCREEN, SM_CYSCREEN,
};

use crate::{CaptureError, Result};

fn fail(msg: impl Into<String>) -> CaptureError {
    CaptureError::Failed(msg.into())
}

/// `PrintWindow` flag that renders DirectComposition / layered content too.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);

pub fn capture_window(hwnd: isize) -> Result<RgbaImage> {
    let hwnd = HWND(hwnd as *mut c_void);
    let mut rect = RECT::default();
    // SAFETY: GetWindowRect errors on a stale handle.
    unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(|e| fail(format!("GetWindowRect: {e}")))?;
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);

    // SAFETY: window DC is released below regardless of outcome.
    let window_dc = unsafe { GetDC(Some(hwnd)) };
    if window_dc.is_invalid() {
        return Err(fail("GetDC(window) failed"));
    }
    let result = blit(window_dc, width, height, |mem_dc| {
        // SAFETY: mem_dc is a valid compatible DC for this window.
        unsafe { PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT) }.as_bool()
    });
    // SAFETY: balances the GetDC above.
    unsafe { ReleaseDC(Some(hwnd), window_dc) };
    result
}

pub fn capture_primary_monitor() -> Result<RgbaImage> {
    // SAFETY: GetSystemMetrics has no preconditions.
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1);
    // SAFETY: a null HWND yields the whole-screen DC, released below.
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.is_invalid() {
        return Err(fail("GetDC(screen) failed"));
    }
    let result = blit(screen_dc, width, height, |mem_dc| {
        // SAFETY: copies the screen into the compatible mem DC.
        unsafe { BitBlt(mem_dc, 0, 0, width, height, Some(screen_dc), 0, 0, SRCCOPY) }.is_ok()
    });
    // SAFETY: balances the GetDC above.
    unsafe { ReleaseDC(None, screen_dc) };
    result
}

/// Creates a memory DC + bitmap, lets `render` draw into it, reads the pixels,
/// and frees every GDI object on all paths.
fn blit(
    reference_dc: HDC,
    width: i32,
    height: i32,
    render: impl FnOnce(HDC) -> bool,
) -> Result<RgbaImage> {
    // SAFETY: all handles created here are released before returning.
    unsafe {
        let mem_dc = CreateCompatibleDC(Some(reference_dc));
        if mem_dc.is_invalid() {
            return Err(fail("CreateCompatibleDC failed"));
        }
        let bitmap = CreateCompatibleBitmap(reference_dc, width, height);
        if bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            return Err(fail("CreateCompatibleBitmap failed"));
        }
        let previous = SelectObject(mem_dc, HGDIOBJ(bitmap.0));
        let outcome = if render(mem_dc) {
            read_pixels(mem_dc, bitmap, width, height)
        } else {
            Err(fail("render into DC failed"))
        };
        SelectObject(mem_dc, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(mem_dc);
        outcome
    }
}

/// Reads a 32-bpp top-down DIB out of `bitmap` and converts BGRX → RGBA.
fn read_pixels(mem_dc: HDC, bitmap: HBITMAP, width: i32, height: i32) -> Result<RgbaImage> {
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
    // SAFETY: buffer holds exactly width*height*4 bytes for the 32-bpp request;
    // `bitmap` is selected into `mem_dc`.
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
        return Err(fail("GetDIBits failed"));
    }
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2); // BGR(X) → RGB
        pixel[3] = 255; // 4th GDI byte is undefined
    }
    RgbaImage::from_raw(width as u32, height as u32, buffer)
        .ok_or_else(|| fail("rgba buffer size mismatch"))
}
