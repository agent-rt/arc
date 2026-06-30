//! Raw input injection (mouse + keyboard) via Win32 `SendInput` — self-
//! maintained on arc's `windows` crate (no third-party input dependency). Used
//! as the fallback when a UI element exposes no actionable pattern, and for
//! coordinate / key-level input that UI Automation cannot express.

use arc_proto::wire::{Key, Modifier, MouseAction, MouseButton, Reply};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    SendInput, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1,
    VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE,
    VK_TAB, VK_UP, VkKeyScanW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, WHEEL_DELTA,
};

use crate::dispatch::{RemoteResult, os_error};

/// Sends a batch of synthesized input events atomically.
fn send(inputs: &[INPUT]) -> RemoteResult<()> {
    // SAFETY: `inputs` is a valid slice; cbSize is the element size.
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(os_error(format!(
            "SendInput injected {sent}/{} events",
            inputs.len()
        )))
    }
}

/// A keyboard event for a virtual key.
fn vk_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A keyboard event injecting a UTF-16 code unit as text.
fn unicode_event(unit: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A mouse event with the given flags and data (position fields zero).
fn mouse_event(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                // mouseData is a DWORD; wheel deltas are its bits reinterpreted.
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// An absolute cursor move, normalised to the primary screen's 0..65535 space.
fn move_event(x: i32, y: i32) -> INPUT {
    // SAFETY: GetSystemMetrics has no preconditions.
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1) as i64;
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1) as i64;
    let nx = (x as i64 * 65535 / w) as i32;
    let ny = (y as i64 * 65535 / h) as i32;
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: nx,
                dy: ny,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn button_event(button: MouseButton, up: bool) -> INPUT {
    let flags = match (button, up) {
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEUP,
    };
    mouse_event(flags, 0)
}

/// Moves the cursor to an absolute screen coordinate and clicks `button`.
pub fn click_point(x: i32, y: i32, button: MouseButton) -> RemoteResult<Reply> {
    send(&[
        move_event(x, y),
        button_event(button, false),
        button_event(button, true),
    ])?;
    Ok(Reply::Ack)
}

/// Types Unicode text into the focused element.
pub fn type_text(text: &str) -> RemoteResult<Reply> {
    let mut inputs = Vec::new();
    for unit in text.encode_utf16() {
        inputs.push(unicode_event(unit, false));
        inputs.push(unicode_event(unit, true));
    }
    if !inputs.is_empty() {
        send(&inputs)?;
    }
    Ok(Reply::Ack)
}

/// Presses `key` with `modifiers` held, then releases everything in reverse —
/// all in one atomic batch, so a held modifier can never leak.
pub fn key_chord(modifiers: &[Modifier], key: Key) -> RemoteResult<Reply> {
    let vk = key_vk(key)?;
    let mut inputs = Vec::new();
    for m in modifiers {
        inputs.push(vk_event(modifier_vk(*m), false));
    }
    inputs.push(vk_event(vk, false));
    inputs.push(vk_event(vk, true));
    for m in modifiers.iter().rev() {
        inputs.push(vk_event(modifier_vk(*m), true));
    }
    send(&inputs)?;
    Ok(Reply::Ack)
}

/// Injects a coordinate-based mouse action.
pub fn mouse(action: MouseAction) -> RemoteResult<Reply> {
    let mut inputs: Vec<INPUT> = Vec::new();
    match action {
        MouseAction::Move { x, y } => inputs.push(move_event(x, y)),
        MouseAction::Click {
            x,
            y,
            button,
            count,
        } => {
            inputs.push(move_event(x, y));
            for _ in 0..count.max(1) {
                inputs.push(button_event(button, false));
                inputs.push(button_event(button, true));
            }
        }
        MouseAction::Down { x, y, button } => {
            inputs.push(move_event(x, y));
            inputs.push(button_event(button, false));
        }
        MouseAction::Up { x, y, button } => {
            inputs.push(move_event(x, y));
            inputs.push(button_event(button, true));
        }
        MouseAction::Scroll { dx, dy } => {
            // One wheel notch per unit. Windows convention: +wheel = up/right.
            if dx != 0 {
                inputs.push(mouse_event(MOUSEEVENTF_HWHEEL, dx * WHEEL_DELTA as i32));
            }
            if dy != 0 {
                inputs.push(mouse_event(MOUSEEVENTF_WHEEL, dy * WHEEL_DELTA as i32));
            }
        }
        MouseAction::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            button,
        } => {
            inputs.push(move_event(from_x, from_y));
            inputs.push(button_event(button, false));
            inputs.push(move_event(to_x, to_y));
            inputs.push(button_event(button, true));
        }
    }
    if !inputs.is_empty() {
        send(&inputs)?;
    }
    Ok(Reply::Ack)
}

fn modifier_vk(modifier: Modifier) -> VIRTUAL_KEY {
    match modifier {
        Modifier::Ctrl => VK_CONTROL,
        Modifier::Alt => VK_MENU,
        Modifier::Shift => VK_SHIFT,
        Modifier::Win => VK_LWIN,
    }
}

fn key_vk(key: Key) -> RemoteResult<VIRTUAL_KEY> {
    Ok(match key {
        Key::Char(c) => {
            // SAFETY: VkKeyScanW has no preconditions; -1 means "no mapping".
            let scan = unsafe { VkKeyScanW(c as u16) };
            if scan == -1 {
                return Err(os_error(format!("no virtual key for '{c}'")));
            }
            VIRTUAL_KEY((scan as u16) & 0x00ff)
        }
        Key::Enter => VK_RETURN,
        Key::Tab => VK_TAB,
        Key::Space => VK_SPACE,
        Key::Backspace => VK_BACK,
        Key::Delete => VK_DELETE,
        Key::Escape => VK_ESCAPE,
        Key::Home => VK_HOME,
        Key::End => VK_END,
        Key::PageUp => VK_PRIOR,
        Key::PageDown => VK_NEXT,
        Key::Up => VK_UP,
        Key::Down => VK_DOWN,
        Key::Left => VK_LEFT,
        Key::Right => VK_RIGHT,
        Key::F(n) if (1..=24).contains(&n) => VIRTUAL_KEY(VK_F1.0 + u16::from(n) - 1),
        Key::F(n) => return Err(os_error(format!("unsupported function key F{n}"))),
    })
}
