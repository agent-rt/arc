//! Raw input injection (mouse + keyboard) via the cross-platform `enigo`
//! crate, which wraps `SendInput` on Windows. Used as the fallback path when a
//! UI element exposes no actionable pattern, and for coordinate/key-level input
//! that UI Automation cannot express.

use arc_proto::wire::{Key, Modifier, MouseAction, MouseButton, Reply};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key as EKey, Keyboard, Mouse, Settings};

use crate::dispatch::{RemoteResult, os_error};

fn enigo() -> RemoteResult<Enigo> {
    Enigo::new(&Settings::default()).map_err(|e| os_error(format!("input backend init: {e}")))
}

fn ebutton(button: MouseButton) -> Button {
    match button {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

/// Moves the cursor to an absolute screen coordinate and clicks `button`.
pub fn click_point(x: i32, y: i32, button: MouseButton) -> RemoteResult<Reply> {
    let mut enigo = enigo()?;
    enigo
        .move_mouse(x, y, Coordinate::Abs)
        .map_err(|e| os_error(format!("move cursor: {e}")))?;
    enigo
        .button(ebutton(button), Direction::Click)
        .map_err(|e| os_error(format!("click: {e}")))?;
    Ok(Reply::Ack)
}

/// Types Unicode text into the focused element.
pub fn type_text(text: &str) -> RemoteResult<Reply> {
    let mut enigo = enigo()?;
    enigo
        .text(text)
        .map_err(|e| os_error(format!("type text: {e}")))?;
    Ok(Reply::Ack)
}

/// Presses `key` with `modifiers` held: each modifier is pressed, the key is
/// clicked, then the modifiers are released — always, even if the key press
/// fails, so a held modifier can never leak past this call.
pub fn key_chord(modifiers: &[Modifier], key: Key) -> RemoteResult<Reply> {
    let ekey = map_key(key)?;
    let mut enigo = enigo()?;

    for m in modifiers {
        enigo
            .key(modifier_key(*m), Direction::Press)
            .map_err(|e| os_error(format!("hold {m:?}: {e}")))?;
    }
    let pressed = enigo.key(ekey, Direction::Click);
    // Release in reverse order regardless of the key result.
    for m in modifiers.iter().rev() {
        let _ = enigo.key(modifier_key(*m), Direction::Release);
    }
    pressed.map_err(|e| os_error(format!("press key: {e}")))?;
    Ok(Reply::Ack)
}

/// Injects a coordinate-based mouse action.
pub fn mouse(action: MouseAction) -> RemoteResult<Reply> {
    let mut enigo = enigo()?;
    let move_to = |enigo: &mut Enigo, x: i32, y: i32| {
        enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| os_error(format!("move cursor: {e}")))
    };
    match action {
        MouseAction::Move { x, y } => move_to(&mut enigo, x, y)?,
        MouseAction::Click {
            x,
            y,
            button,
            count,
        } => {
            move_to(&mut enigo, x, y)?;
            for _ in 0..count.max(1) {
                enigo
                    .button(ebutton(button), Direction::Click)
                    .map_err(|e| os_error(format!("click: {e}")))?;
            }
        }
        MouseAction::Down { x, y, button } => {
            move_to(&mut enigo, x, y)?;
            enigo
                .button(ebutton(button), Direction::Press)
                .map_err(|e| os_error(format!("button down: {e}")))?;
        }
        MouseAction::Up { x, y, button } => {
            move_to(&mut enigo, x, y)?;
            enigo
                .button(ebutton(button), Direction::Release)
                .map_err(|e| os_error(format!("button up: {e}")))?;
        }
        MouseAction::Scroll { dx, dy } => {
            if dx != 0 {
                enigo
                    .scroll(dx, Axis::Horizontal)
                    .map_err(|e| os_error(format!("scroll x: {e}")))?;
            }
            if dy != 0 {
                enigo
                    .scroll(dy, Axis::Vertical)
                    .map_err(|e| os_error(format!("scroll y: {e}")))?;
            }
        }
        MouseAction::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            button,
        } => {
            move_to(&mut enigo, from_x, from_y)?;
            enigo
                .button(ebutton(button), Direction::Press)
                .map_err(|e| os_error(format!("drag press: {e}")))?;
            let moved = move_to(&mut enigo, to_x, to_y);
            // Release even if the move failed, so the button never sticks down.
            let released = enigo.button(ebutton(button), Direction::Release);
            moved?;
            released.map_err(|e| os_error(format!("drag release: {e}")))?;
        }
    }
    Ok(Reply::Ack)
}

fn modifier_key(modifier: Modifier) -> EKey {
    match modifier {
        Modifier::Ctrl => EKey::Control,
        Modifier::Alt => EKey::Alt,
        Modifier::Shift => EKey::Shift,
        Modifier::Win => EKey::Meta,
    }
}

fn map_key(key: Key) -> RemoteResult<EKey> {
    Ok(match key {
        Key::Char(c) => EKey::Unicode(c),
        Key::Enter => EKey::Return,
        Key::Tab => EKey::Tab,
        Key::Space => EKey::Space,
        Key::Backspace => EKey::Backspace,
        Key::Delete => EKey::Delete,
        Key::Escape => EKey::Escape,
        Key::Home => EKey::Home,
        Key::End => EKey::End,
        Key::PageUp => EKey::PageUp,
        Key::PageDown => EKey::PageDown,
        Key::Up => EKey::UpArrow,
        Key::Down => EKey::DownArrow,
        Key::Left => EKey::LeftArrow,
        Key::Right => EKey::RightArrow,
        Key::F(n) => match n {
            1 => EKey::F1,
            2 => EKey::F2,
            3 => EKey::F3,
            4 => EKey::F4,
            5 => EKey::F5,
            6 => EKey::F6,
            7 => EKey::F7,
            8 => EKey::F8,
            9 => EKey::F9,
            10 => EKey::F10,
            11 => EKey::F11,
            12 => EKey::F12,
            _ => return Err(os_error(format!("unsupported function key F{n}"))),
        },
    })
}
