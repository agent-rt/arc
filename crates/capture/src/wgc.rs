//! Windows.Graphics.Capture path: capture a window or monitor as a still RGBA
//! frame via the DWM compositor (correct for DirectComposition / swap-chain
//! apps, and works in a detached session).

use core::ffi::c_void;
use std::time::{Duration, Instant};

use image::RgbaImage;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND, POINT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{IInspectable, Interface};

use crate::{CaptureError, Result};

/// Maps a windows-rs error to a `Failed` capture error.
fn fail(e: windows::core::Error) -> CaptureError {
    CaptureError::Failed(e.to_string())
}

/// Captures a window by handle.
pub fn capture_window(hwnd: isize) -> Result<RgbaImage> {
    ensure_supported()?;
    let interop = item_interop()?;
    let hwnd = HWND(hwnd as *mut c_void);
    // SAFETY: `interop` is the live capture-item factory; CreateForWindow
    // validates the HWND and errors on a stale handle.
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }
        .map_err(|e| CaptureError::NotFound(format!("CreateForWindow: {e}")))?;
    capture_item(&item)
}

/// Captures a window by grabbing the primary monitor and cropping to the
/// window's rectangle. Works for visible windows that don't present per-window
/// frames to WGC (some static WinUI 3 surfaces), as long as they're not
/// occluded — the desktop always composes.
pub fn capture_window_region(hwnd: isize) -> Result<RgbaImage> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let handle = HWND(hwnd as *mut c_void);
    let mut rect = RECT::default();
    // SAFETY: GetWindowRect errors on a stale handle.
    unsafe { GetWindowRect(handle, &mut rect) }.map_err(fail)?;

    let monitor = capture_primary_monitor()?;
    let x = rect.left.max(0) as u32;
    let y = rect.top.max(0) as u32;
    let w = ((rect.right - rect.left).max(1) as u32).min(monitor.width().saturating_sub(x));
    let h = ((rect.bottom - rect.top).max(1) as u32).min(monitor.height().saturating_sub(y));
    if w == 0 || h == 0 {
        return Err(CaptureError::Failed("window is off-screen".to_owned()));
    }
    Ok(image::imageops::crop_imm(&monitor, x, y, w, h).to_image())
}

/// Captures the primary monitor.
pub fn capture_primary_monitor() -> Result<RgbaImage> {
    ensure_supported()?;
    let interop = item_interop()?;
    // SAFETY: MonitorFromPoint has no preconditions; the origin maps to the
    // primary monitor.
    let monitor: HMONITOR =
        unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    // SAFETY: `interop` is live; `monitor` is a valid HMONITOR.
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }.map_err(fail)?;
    capture_item(&item)
}

fn ensure_supported() -> Result<()> {
    if GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        Ok(())
    } else {
        Err(CaptureError::Unsupported(
            "Windows.Graphics.Capture is not available on this system".to_owned(),
        ))
    }
}

fn item_interop() -> Result<IGraphicsCaptureItemInterop> {
    windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(fail)
}

/// Drives one capture: spin up a D3D device + frame pool, grab the first frame,
/// copy it to the CPU, and decode BGRA → RGBA.
fn capture_item(item: &GraphicsCaptureItem) -> Result<RgbaImage> {
    // WinRT calls need an initialised apartment; MTA suits the free-threaded pool.
    // SAFETY: CoInitializeEx is safe to call repeatedly; S_FALSE is benign.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let size = item.Size().map_err(fail)?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(CaptureError::Failed(format!(
            "capture item has zero size ({}x{})",
            size.Width, size.Height
        )));
    }

    let (device, context) = create_device()?;
    let rt_device = create_rt_device(&device)?;

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &rt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(fail)?;
    let session = pool.CreateCaptureSession(item).map_err(fail)?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    // Drop the capture border where allowed (unpackaged processes may be denied).
    let _ = session.SetIsBorderRequired(false);

    let (tx, rx) = std::sync::mpsc::channel::<Direct3D11CaptureFrame>();
    let handler =
        TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
            if let Some(pool) = pool.as_ref()
                && let Ok(frame) = pool.TryGetNextFrame()
            {
                let _ = tx.send(frame);
            }
            Ok(())
        });
    let token = pool.FrameArrived(&handler).map_err(fail)?;
    session.StartCapture().map_err(fail)?;

    // The first frame often predates the app presenting its swap-chain content
    // (window chrome only, black client area). Wait for it, then keep the most
    // recent frame over a short settle window so composed content lands.
    let mut frame = rx
        .recv_timeout(Duration::from_millis(1500))
        .map_err(|_| CaptureError::Failed("timed out waiting for a capture frame".to_owned()))?;
    let settle = Instant::now() + Duration::from_millis(300);
    loop {
        let now = Instant::now();
        if now >= settle {
            break;
        }
        match rx.recv_timeout(settle - now) {
            Ok(next) => {
                let _ = frame.Close();
                frame = next;
            }
            Err(_) => break, // no fresher frame within the window
        }
    }

    let image = read_frame(
        &device,
        &context,
        &frame,
        size.Width as u32,
        size.Height as u32,
    );

    let _ = pool.RemoveFrameArrived(token);
    let _ = frame.Close();
    let _ = session.Close();
    let _ = pool.Close();
    image
}

/// Creates a D3D11 device (hardware, falling back to the WARP software renderer
/// for headless/detached hosts).
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: standard device creation; outputs are checked before use.
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if hr.is_ok()
            && let (Some(device), Some(context)) = (device, context)
        {
            return Ok((device, context));
        }
    }
    Err(CaptureError::Failed(
        "D3D11CreateDevice failed (hardware and WARP)".to_owned(),
    ))
}

/// Wraps the D3D11 device as a WinRT `IDirect3DDevice` for the frame pool.
fn create_rt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast().map_err(fail)?;
    // SAFETY: `dxgi` is a live DXGI device from our D3D11 device.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(fail)?;
    inspectable.cast().map_err(fail)
}

/// Copies the captured GPU texture to a CPU-readable staging texture and
/// decodes its BGRA rows into an RGBA image.
fn read_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
    width: u32,
    height: u32,
) -> Result<RgbaImage> {
    let surface = frame.Surface().map_err(fail)?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(fail)?;
    // SAFETY: the surface wraps a D3D11 texture; GetInterface returns it.
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(fail)?;

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `texture` is live; GetDesc fills `desc`.
    unsafe { texture.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;

    let mut staging: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` describes a valid staging texture; output is checked.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.map_err(fail)?;
    let staging =
        staging.ok_or_else(|| CaptureError::Failed("CreateTexture2D returned null".to_owned()))?;

    // SAFETY: same-format/-size copy from the captured texture to staging.
    unsafe { context.CopyResource(&staging, &texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `staging` is CPU-readable; Map yields its rows, Unmap'd below.
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.map_err(fail)?;

    let row_pitch = mapped.RowPitch as usize;
    let stride = width as usize * 4;
    let mut rgba = vec![0u8; stride * height as usize];
    // SAFETY: `mapped.pData` points at width*height BGRA pixels with `row_pitch`
    // bytes per row; we read within those bounds and write within `rgba`.
    unsafe {
        let src = mapped.pData as *const u8;
        for y in 0..height as usize {
            let row = src.add(y * row_pitch);
            let out = &mut rgba[y * stride..y * stride + stride];
            for x in 0..width as usize {
                let p = row.add(x * 4);
                out[x * 4] = *p.add(2); // R (from BGRA)
                out[x * 4 + 1] = *p.add(1); // G
                out[x * 4 + 2] = *p; // B
                out[x * 4 + 3] = 255; // opaque
            }
        }
    }
    // SAFETY: balances the Map above.
    unsafe { context.Unmap(&staging, 0) };

    RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| CaptureError::Failed("rgba buffer size mismatch".to_owned()))
}
