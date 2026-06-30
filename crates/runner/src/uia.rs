//! UI Automation: enumerate a window's elements and act on them (invoke/click,
//! set value). This is the semantic-control path — an Agent targets named
//! controls rather than raw pixels.
//!
//! ## Element identity
//!
//! [`ElementId`](arc_proto::id::ElementId) is `"<hwnd>:<runtime-id>"`,
//! where `runtime-id` is the element's UIA **RuntimeId** — an integer array,
//! stable for the element's lifetime — encoded as `.`-joined decimals.
//! Resolution re-walks the window and matches an element *by RuntimeId
//! identity*, not by position. So an id keeps pointing at the same control even
//! if siblings are added, removed or reordered between listing and acting; if
//! the element is gone we return [`NotFound`] (re-list), and crucially we never
//! act on the *wrong* element the way a positional index could.
//!
//! [`NotFound`]: arc_proto::wire::RemoteErrorKind::NotFound

#[cfg(windows)]
pub use imp::{click_element, element_rect, find_elements, focus, list_elements, set_value};

#[cfg(not(windows))]
pub use stub::{click_element, element_rect, find_elements, focus, list_elements, set_value};

#[cfg(not(windows))]
mod stub {
    use arc_proto::id::WindowId;
    use arc_proto::wire::Reply;

    use crate::dispatch::{RemoteResult, os_error};

    fn unsupported() -> arc_proto::wire::RemoteError {
        os_error("UI Automation is only supported on Windows".to_owned())
    }

    pub fn list_elements(_window: WindowId) -> RemoteResult<Reply> {
        Err(unsupported())
    }
    pub fn find_elements(
        _window: WindowId,
        _query: &arc_proto::wire::ElementQuery,
        _wait_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        Err(unsupported())
    }
    pub fn click_element(_element_id: &str) -> RemoteResult<Reply> {
        Err(unsupported())
    }
    pub fn set_value(_element_id: &str, _value: &str) -> RemoteResult<Reply> {
        Err(unsupported())
    }
    pub fn element_rect(_element_id: &str) -> RemoteResult<arc_proto::wire::Rect> {
        Err(unsupported())
    }
    pub fn focus(_element_id: &str) -> RemoteResult<()> {
        Err(unsupported())
    }
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;
    use std::time::{Duration, Instant};

    use arc_proto::id::{ElementId, WindowId};
    use arc_proto::wire::{ElementInfo, MouseButton, Rect, Reply};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::System::Ole::{
        SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
        IUIAutomationValuePattern, TreeScope_Descendants, TreeScope_Subtree, UIA_CONTROLTYPE_ID,
        UIA_InvokePatternId, UIA_ValuePatternId,
    };
    use windows::core::BSTR;

    use crate::dispatch::{RemoteResult, not_found, os_error, timeout_error};

    /// Cap on elements returned by a single [`list_elements`]; deep windows can
    /// hold thousands of nodes and an Agent rarely needs them all at once.
    const MAX_ELEMENTS: i32 = 250;

    fn automation() -> RemoteResult<IUIAutomation> {
        // SAFETY: Called on a dedicated blocking thread. CoInitializeEx may
        // return S_FALSE/RPC_E_CHANGED_MODE if COM is already initialised here;
        // both are benign, so the HRESULT is intentionally ignored.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // SAFETY: Standard activation of the in-proc CUIAutomation server; the
        // returned Result is checked.
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| os_error(format!("UIAutomation init failed: {e}")))
    }

    fn hwnd_from(id: u64) -> HWND {
        HWND(id as *mut c_void)
    }

    /// Enumerates up to [`MAX_ELEMENTS`] descendants of `window`.
    pub fn list_elements(window: WindowId) -> RemoteResult<Reply> {
        let automation = automation()?;
        // SAFETY: `automation` is a valid IUIAutomation; `collect` only borrows it.
        Ok(Reply::Elements(unsafe { collect(&automation, window) }?))
    }

    /// Finds elements in `window` matching `query`. With `wait_ms`, re-scans on a
    /// short interval until at least one matches or the deadline passes (a
    /// timeout error); otherwise returns the current matches at once.
    pub fn find_elements(
        window: WindowId,
        query: &arc_proto::wire::ElementQuery,
        wait_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        let automation = automation()?;
        let deadline = wait_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        loop {
            // SAFETY: `automation` is valid; `collect` only borrows it.
            let hits: Vec<ElementInfo> = unsafe { collect(&automation, window) }?
                .into_iter()
                .filter(|info| query.matches(info))
                .collect();
            if !hits.is_empty() || wait_ms.is_none() {
                return Ok(Reply::Elements(hits));
            }
            match deadline {
                Some(at) if Instant::now() < at => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                _ => {
                    return Err(timeout_error(format!(
                        "no element matched within {} ms",
                        wait_ms.unwrap_or(0)
                    )));
                }
            }
        }
    }

    /// Walks up to [`MAX_ELEMENTS`] descendants of `window` into [`ElementInfo`]s.
    ///
    /// # Safety
    /// `automation` must be a live `IUIAutomation` for this thread.
    unsafe fn collect(
        automation: &IUIAutomation,
        window: WindowId,
    ) -> RemoteResult<Vec<ElementInfo>> {
        // SAFETY: the HWND is validated by ElementFromHandle, which errors on a
        // stale handle; all returned interface pointers are checked before use.
        unsafe {
            let root: IUIAutomationElement = automation
                .ElementFromHandle(hwnd_from(window.0))
                .map_err(|e| os_error(format!("element from window handle: {e}")))?;
            let condition = automation
                .CreateTrueCondition()
                .map_err(|e| os_error(format!("create condition: {e}")))?;
            let array = root
                .FindAll(TreeScope_Descendants, &condition)
                .map_err(|e| os_error(format!("enumerate elements: {e}")))?;
            let length = array
                .Length()
                .map_err(|e| os_error(format!("element count: {e}")))?;

            let mut infos = Vec::new();
            for index in 0..length.min(MAX_ELEMENTS) {
                let element = array
                    .GetElement(index)
                    .map_err(|e| os_error(format!("get element {index}: {e}")))?;
                infos.push(describe(&element, window.0));
            }
            Ok(infos)
        }
    }

    /// Invokes an element; falls back to a centre-of-bounds click if it exposes
    /// no Invoke pattern.
    pub fn click_element(element_id: &str) -> RemoteResult<Reply> {
        let element = resolve(element_id)?;
        // SAFETY: `element` is a live element resolved this call; pattern
        // queries and property reads return checked Results.
        unsafe {
            if let Ok(invoke) =
                element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            {
                invoke
                    .Invoke()
                    .map_err(|e| os_error(format!("invoke failed: {e}")))?;
                return Ok(Reply::Ack);
            }
            let rect = element
                .CurrentBoundingRectangle()
                .map_err(|e| os_error(format!("no invoke pattern and no bounds: {e}")))?;
            let x = (rect.left + rect.right) / 2;
            let y = (rect.top + rect.bottom) / 2;
            crate::input::click_point(x, y, MouseButton::Left)
        }
    }

    /// Sets an element's value via the Value pattern (preferred over keystrokes).
    pub fn set_value(element_id: &str, value: &str) -> RemoteResult<Reply> {
        let element = resolve(element_id)?;
        // SAFETY: `element` is live; the Value pattern pointer is checked.
        unsafe {
            let pattern = element
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .map_err(|e| os_error(format!("element has no Value pattern: {e}")))?;
            pattern
                .SetValue(&BSTR::from(value))
                .map_err(|e| os_error(format!("set value failed: {e}")))?;
            Ok(Reply::Ack)
        }
    }

    /// Returns an element's on-screen bounding rectangle (for element capture).
    pub fn element_rect(element_id: &str) -> RemoteResult<Rect> {
        let element = resolve(element_id)?;
        // SAFETY: `element` is live this call.
        let r = unsafe { element.CurrentBoundingRectangle() }
            .map_err(|e| os_error(format!("bounding rect: {e}")))?;
        Ok(Rect {
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
        })
    }

    /// Gives an element keyboard focus (so subsequent typed keys land in it).
    pub fn focus(element_id: &str) -> RemoteResult<()> {
        let element = resolve(element_id)?;
        // SAFETY: `element` is a live element resolved this call.
        unsafe { element.SetFocus() }.map_err(|e| os_error(format!("set focus failed: {e}")))?;
        Ok(())
    }

    /// Re-walks the window's subtree and returns the element whose RuntimeId
    /// matches the one encoded in `element_id`.
    fn resolve(element_id: &str) -> RemoteResult<IUIAutomationElement> {
        let (hwnd_str, rid_str) = element_id
            .split_once(':')
            .ok_or_else(|| not_found(format!("malformed element id '{element_id}'")))?;
        let hwnd: u64 = hwnd_str
            .parse()
            .map_err(|_| not_found("malformed window handle in element id".to_owned()))?;
        if rid_str.is_empty() {
            return Err(not_found(
                "element id carries no runtime id; re-list elements".to_owned(),
            ));
        }
        let target = rid_str
            .split('.')
            .map(str::parse::<i32>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| not_found("malformed runtime id in element id".to_owned()))?;

        let automation = automation()?;
        // SAFETY: `automation` is valid; the HWND is validated by
        // ElementFromHandle. We compare each descendant's RuntimeId (read via
        // the safe `read_runtime_id`) to the target and return the match.
        unsafe {
            let root = automation
                .ElementFromHandle(hwnd_from(hwnd))
                .map_err(|e| os_error(format!("element from window handle: {e}")))?;
            let condition = automation
                .CreateTrueCondition()
                .map_err(|e| os_error(format!("create condition: {e}")))?;
            let array = root
                .FindAll(TreeScope_Subtree, &condition)
                .map_err(|e| os_error(format!("enumerate elements: {e}")))?;
            let length = array
                .Length()
                .map_err(|e| os_error(format!("element count: {e}")))?;
            for index in 0..length {
                let element = array
                    .GetElement(index)
                    .map_err(|e| os_error(format!("get element {index}: {e}")))?;
                if read_runtime_id(&element) == target {
                    return Ok(element);
                }
            }
        }
        Err(not_found(
            "element no longer present (UI changed); re-list elements".to_owned(),
        ))
    }

    /// Reads an element's UIA RuntimeId into a plain `Vec<i32>`, freeing the
    /// returned SAFEARRAY. Returns empty on any failure (treated as "no stable
    /// id").
    fn read_runtime_id(element: &IUIAutomationElement) -> Vec<i32> {
        // SAFETY: `element` is a live interface reference; GetRuntimeId yields an
        // owned 1-D SAFEARRAY of i4 that we read element-by-element and then
        // destroy exactly once.
        unsafe {
            let Ok(psa) = element.GetRuntimeId() else {
                return Vec::new();
            };
            if psa.is_null() {
                return Vec::new();
            }
            let mut out = Vec::new();
            if let (Ok(lower), Ok(upper)) = (SafeArrayGetLBound(psa, 1), SafeArrayGetUBound(psa, 1))
            {
                for index in lower..=upper {
                    let mut value = 0i32;
                    if SafeArrayGetElement(psa, &index, (&raw mut value).cast::<c_void>()).is_ok() {
                        out.push(value);
                    }
                }
            }
            let _ = SafeArrayDestroy(psa);
            out
        }
    }

    /// Encodes `"<hwnd>:<runtime-id>"` with the RuntimeId as `.`-joined decimals.
    fn encode_element_id(hwnd: u64, runtime_id: &[i32]) -> String {
        let mut id = format!("{hwnd}:");
        for (i, value) in runtime_id.iter().enumerate() {
            if i > 0 {
                id.push('.');
            }
            id.push_str(&value.to_string());
        }
        id
    }

    /// Reads display metadata for one element. Property reads that fail degrade
    /// to sensible defaults rather than aborting the whole listing.
    ///
    /// # Safety
    /// `element` must be a live UIA element pointer.
    unsafe fn describe(element: &IUIAutomationElement, hwnd: u64) -> ElementInfo {
        // SAFETY: caller guarantees `element` is live; each accessor returns a
        // checked Result that we defensively default on error.
        let name = unsafe { element.CurrentName() }
            .map(|b| b.to_string())
            .ok()
            .filter(|s| !s.is_empty());
        let automation_id = unsafe { element.CurrentAutomationId() }
            .map(|b| b.to_string())
            .ok()
            .filter(|s| !s.is_empty());
        let control_type = unsafe { element.CurrentControlType() }
            .map(control_type_name)
            .unwrap_or_else(|_| "Unknown".to_owned());
        let enabled = unsafe { element.CurrentIsEnabled() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let offscreen = unsafe { element.CurrentIsOffscreen() }
            .map(|b| b.as_bool())
            .unwrap_or(true);
        // Current value, if the element exposes a Value pattern (Edit, etc.).
        let value =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
                .ok()
                .and_then(|p| unsafe { p.CurrentValue() }.ok())
                .map(|b| b.to_string())
                .filter(|s| !s.is_empty());
        let rect = unsafe { element.CurrentBoundingRectangle() }
            .map(|r| Rect {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            })
            .unwrap_or_default();

        let runtime_id = read_runtime_id(element);

        ElementInfo {
            id: ElementId(encode_element_id(hwnd, &runtime_id)),
            control_type,
            name,
            automation_id,
            value,
            rect,
            actionable: enabled && !offscreen,
        }
    }

    /// Maps the well-known UIA control-type ids to readable names.
    fn control_type_name(control_type: UIA_CONTROLTYPE_ID) -> String {
        let name = match control_type.0 {
            50000 => "Button",
            50002 => "CheckBox",
            50003 => "ComboBox",
            50004 => "Edit",
            50005 => "Hyperlink",
            50006 => "Image",
            50007 => "ListItem",
            50008 => "List",
            50011 => "MenuItem",
            50019 => "TabItem",
            50020 => "Text",
            50023 => "TreeItem",
            50026 => "Group",
            50032 => "Window",
            50033 => "Pane",
            other => return format!("ControlType({other})"),
        };
        name.to_owned()
    }
}
