//! Remote clipboard read/write via the Win32 clipboard API — self-maintained on
//! arc's `windows` crate (no third-party clipboard dependency), so the runner
//! stays on a single `windows-rs` version.

#[cfg(windows)]
pub use imp::{get, set};

#[cfg(not(windows))]
pub use stub::{get, set};

#[cfg(not(windows))]
mod stub {
    use crate::dispatch::{RemoteResult, os_error};

    fn unsupported() -> arc_proto::wire::RemoteError {
        os_error("clipboard is only supported on Windows".to_owned())
    }

    pub fn get() -> RemoteResult<String> {
        Err(unsupported())
    }
    pub fn set(_text: &str) -> RemoteResult<()> {
        Err(unsupported())
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    use crate::dispatch::{RemoteResult, os_error};

    /// Opens the clipboard, closing it when this guard drops (so an early return
    /// never leaves the clipboard locked for other processes).
    struct Clipboard;

    impl Clipboard {
        fn open() -> RemoteResult<Self> {
            // SAFETY: a null owner window is valid; we close it on drop.
            unsafe { OpenClipboard(None) }.map_err(|e| os_error(format!("open clipboard: {e}")))?;
            Ok(Clipboard)
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            // SAFETY: paired with the successful OpenClipboard above.
            let _ = unsafe { CloseClipboard() };
        }
    }

    /// Reads the clipboard's Unicode text (empty string if it holds none).
    pub fn get() -> RemoteResult<String> {
        let _clip = Clipboard::open()?;
        // SAFETY: clipboard is open for the lifetime of `_clip`; the handle (if
        // any) is locked before dereferencing and unlocked before returning.
        unsafe {
            let handle = match GetClipboardData(u32::from(CF_UNICODETEXT.0)) {
                Ok(h) if !h.is_invalid() => h,
                // No CF_UNICODETEXT on the clipboard — treat as empty, not error.
                _ => return Ok(String::new()),
            };
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal) as *const u16;
            if ptr.is_null() {
                return Err(os_error("lock clipboard memory".to_owned()));
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(hglobal);
            Ok(text)
        }
    }

    /// Replaces the clipboard contents with `text` as Unicode.
    pub fn set(text: &str) -> RemoteResult<()> {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        units.push(0); // NUL terminator the clipboard requires
        let bytes = std::mem::size_of_val(units.as_slice());

        let _clip = Clipboard::open()?;
        // SAFETY: clipboard is open; the global block is sized for `units`, locked
        // before the copy, and ownership transfers to the OS on SetClipboardData.
        unsafe {
            EmptyClipboard().map_err(|e| os_error(format!("empty clipboard: {e}")))?;
            let hglobal =
                GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| os_error(format!("alloc: {e}")))?;
            let dst = GlobalLock(hglobal) as *mut u16;
            if dst.is_null() {
                return Err(os_error("lock new clipboard memory".to_owned()));
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
            let _ = GlobalUnlock(hglobal);
            // The system takes ownership of the memory on success.
            SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(hglobal.0)))
                .map_err(|e| os_error(format!("set clipboard: {e}")))?;
        }
        Ok(())
    }
}
