//! **L2 — end-to-end layer.** The semantic commands an Agent issues and the
//! results the runner returns. These travel sealed inside the Noise channel and
//! are never visible to the relay.
//!
//! A controller sends a [`Request`]; the runner answers with exactly one
//! terminal [`Response`] and, for long-running work, zero or more interim
//! [`Event`]s carrying the same [`RequestId`]. All three are unified by the
//! [`Frame`] enum, which is what [`codec`](crate::codec) serializes.

use serde::{Deserialize, Serialize};

use crate::id::{ElementId, RequestId, WindowId};

/// Recommended default wall-clock timeout (ms) a controller applies to
/// [`Command::RunCommand`] when the caller specifies none — a safety net so a
/// runaway command is eventually killed rather than running forever. Generous
/// enough for typical builds; callers pass an explicit `0` for "no limit". This
/// is a client-side policy, not enforced by the protocol itself.
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 600_000; // 10 minutes

/// Top-level L2 message. One [`Frame`] is one CBOR document, sealed and sent as
/// one (possibly chunked) relay payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// Controller → runner: perform a command.
    Request(Request),
    /// Runner → controller: terminal result of a command.
    Response(Response),
    /// Runner → controller: interim progress for an in-flight command.
    Event(Event),
}

/// A command to execute on the runner, tagged with a controller-unique id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Correlates the eventual [`Response`] and any [`Event`]s.
    pub id: RequestId,
    /// What to do.
    pub command: Command,
}

/// The terminal outcome of a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Id of the [`Request`] this answers.
    pub id: RequestId,
    /// Success payload or structured failure.
    pub result: Result<Reply, RemoteError>,
}

/// The set of operations the runner exposes to an Agent.
///
/// Operations are *semantic* first (drive the UI through UI Automation) and
/// *pixel* second (screenshot + coordinate input) so that an Agent can act on
/// named controls rather than brittle pixel offsets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Command {
    /// Run a shell command and capture its output.
    RunCommand {
        /// Which interpreter to use.
        shell: Shell,
        /// The command line to execute.
        command: String,
        /// Optional wall-clock timeout in milliseconds.
        timeout_ms: Option<u64>,
        /// If `true`, stdout/stderr are streamed as [`Event`]s before the
        /// terminal [`Response`].
        stream: bool,
    },
    /// Run a script by **content** (not a remote path). The runner writes
    /// `content` to a temp file with the interpreter's extension, runs it with
    /// `args` passed through, streams output like [`Command::RunCommand`], then
    /// deletes the temp file. The source travels as a data field — never through
    /// a shell — so there is no cross-shell quoting to escape, and PowerShell is
    /// invoked with `-ExecutionPolicy Bypass -File` (no policy prompt, args bind
    /// to the script's `param()` directly).
    RunScript {
        /// Which interpreter to run the script with.
        shell: Shell,
        /// The script source.
        content: String,
        /// Arguments passed to the script (each a separate process argument).
        #[serde(default)]
        args: Vec<String>,
        /// Optional wall-clock timeout in milliseconds.
        timeout_ms: Option<u64>,
        /// If `true`, output streams as [`Event`]s before the terminal response.
        stream: bool,
    },
    /// Launch an application by path or registered name.
    OpenApp {
        /// Executable path or app name.
        target: String,
        /// Command-line arguments.
        args: Vec<String>,
    },
    /// Capture an image of the screen, a window, an element, or a region.
    Screenshot {
        /// What to capture.
        target: CaptureTarget,
        /// Preferred encoding (`None` = WebP). PNG falls back automatically if
        /// the WebP encoder rejects the frame.
        #[serde(default)]
        format: Option<ImageFormat>,
        /// If set, the runner re-captures until two consecutive frames are
        /// stable (or this many ms elapse) before returning — a reliable
        /// replacement for a blind "wait for the app to render" sleep.
        #[serde(default)]
        settle_ms: Option<u64>,
        /// When settling, first wait for the frame to *change* from the initial
        /// capture (so a just-launched window's static backdrop isn't mistaken
        /// for "rendered") before looking for stability.
        #[serde(default)]
        settle_await_change: bool,
    },
    /// Enumerate top-level windows.
    ListWindows,
    /// Enumerate UI Automation elements within a window.
    ListElements {
        /// Window to inspect.
        window: WindowId,
    },
    /// Find elements in `window` matching `query` — so an Agent can target a
    /// control by name / automation-id / type without listing the whole tree.
    /// With `wait_ms` set, the runner re-scans until at least one element matches
    /// or the deadline elapses (returning a timeout error); otherwise it returns
    /// the current matches at once (possibly empty).
    FindElements {
        /// Window to search.
        window: WindowId,
        /// Attribute filter.
        query: ElementQuery,
        /// If set, poll until ≥1 match or this many ms elapse.
        #[serde(default)]
        wait_ms: Option<u64>,
    },
    /// Invoke/click a UI element or a raw screen coordinate.
    Click {
        /// What to click.
        target: ClickTarget,
    },
    /// Type Unicode text into the focused element.
    TypeText {
        /// Text to inject.
        text: String,
    },
    /// Set the value of a UI element directly (preferred over keystrokes).
    SetValue {
        /// Target element.
        element: ElementId,
        /// New value.
        value: String,
    },
    /// Read a (range of a) file from the runner. Reads up to `max_len` bytes
    /// starting at `offset`; `max_len == 0` reads to end of file. A read that
    /// returns fewer bytes than `max_len` has reached EOF — the basis for
    /// chunked, resumable transfer of files larger than one frame.
    ReadFile {
        /// Absolute path on the runner.
        path: String,
        /// Byte offset to start reading from.
        #[serde(default)]
        offset: u64,
        /// Maximum bytes to read (`0` = whole file from `offset`).
        #[serde(default)]
        max_len: u64,
    },
    /// Write a (chunk of a) file on the runner. `offset == 0` creates/truncates
    /// the file; `offset > 0` seeks and writes there — so a large file is sent
    /// as a sequence of chunks.
    WriteFile {
        /// Absolute path on the runner.
        path: String,
        /// File contents (this chunk).
        contents: Vec<u8>,
        /// Byte offset to write at.
        #[serde(default)]
        offset: u64,
    },
    /// Hash a set of files (relative to `root`) for incremental sync diffing.
    /// The runner returns each path's content hash, or `None` if it is absent —
    /// so the controller transfers only files that differ or are missing,
    /// without ever walking the runner's build outputs.
    HashFiles {
        /// Base directory on the runner.
        root: String,
        /// Paths relative to `root` (forward-slash separated).
        paths: Vec<String>,
    },
    /// Enumerate file paths under `root` (recursive), **skipping build/VCS
    /// directories** (`target`, `bin`, `obj`, `node_modules`, `.git`) — so a
    /// mirroring sync can prune remote files no longer present locally without
    /// ever touching build outputs.
    ListTree {
        /// Base directory on the runner.
        root: String,
    },
    /// Delete a single file on the runner (no-op if already absent).
    DeleteFile {
        /// Absolute path on the runner.
        path: String,
    },
    /// Press a key, optionally with modifiers held — for keys and combinations
    /// that [`Command::TypeText`] cannot express (Enter, Tab, arrows, F-keys,
    /// and chords like Ctrl+C or Alt+F4). The modifiers are pressed, the key is
    /// clicked, then the modifiers are released (always, even on error).
    KeyChord {
        /// Modifier keys held for the duration of the chord (may be empty).
        #[serde(default)]
        modifiers: Vec<Modifier>,
        /// The main key to press.
        key: Key,
    },
    /// Inject a coordinate-based mouse action — move, multi-click, button
    /// down/up, scroll, or drag. The superset of [`Command::Click`]'s
    /// [`ClickTarget::Point`] path.
    Mouse {
        /// What the mouse should do.
        action: MouseAction,
    },
}

/// Interpreter selection for [`Command::RunCommand`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shell {
    /// Windows PowerShell / PowerShell Core.
    PowerShell,
    /// Legacy `cmd.exe`.
    Cmd,
}

/// What [`Command::Screenshot`] should capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureTarget {
    /// The entire virtual desktop.
    FullScreen,
    /// A single window by handle.
    Window(WindowId),
    /// A single UI element by id — captured as its on-screen bounding box.
    Element(ElementId),
    /// A rectangular region in virtual-desktop coordinates.
    Region {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
    },
}

/// What [`Command::Click`] targets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClickTarget {
    /// A semantic UI Automation element (preferred).
    Element(ElementId),
    /// A raw screen coordinate (fallback for non-UIA surfaces).
    Point {
        /// X in virtual-desktop coordinates.
        x: i32,
        /// Y in virtual-desktop coordinates.
        y: i32,
        /// Which mouse button.
        button: MouseButton,
    },
}

/// Mouse button selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// Primary (left) button.
    Left,
    /// Secondary (right) button.
    Right,
    /// Middle button.
    Middle,
}

/// A modifier key held for the duration of a [`Command::KeyChord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Modifier {
    /// Control.
    Ctrl,
    /// Alt / Option.
    Alt,
    /// Shift.
    Shift,
    /// Windows / Meta / Command key.
    Win,
}

/// The main key of a [`Command::KeyChord`]. [`Key::Char`] is any printable
/// character key (e.g. `'c'` for Ctrl+C); the rest are named non-printable keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Key {
    /// A character key (letter, digit, punctuation).
    Char(char),
    /// Enter / Return.
    Enter,
    /// Tab.
    Tab,
    /// Spacebar.
    Space,
    /// Backspace.
    Backspace,
    /// Forward Delete.
    Delete,
    /// Escape.
    Escape,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Function key F1–F24.
    F(u8),
}

/// A coordinate-based mouse action for [`Command::Mouse`]. Coordinates are in
/// virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAction {
    /// Move the cursor without pressing any button.
    Move {
        /// Target X.
        x: i32,
        /// Target Y.
        y: i32,
    },
    /// Move to `(x, y)` and click `button` `count` times (2 = double-click).
    Click {
        /// Target X.
        x: i32,
        /// Target Y.
        y: i32,
        /// Which button.
        button: MouseButton,
        /// Number of clicks (≥ 1).
        count: u32,
    },
    /// Move to `(x, y)` and press (hold) `button`.
    Down {
        /// Target X.
        x: i32,
        /// Target Y.
        y: i32,
        /// Which button.
        button: MouseButton,
    },
    /// Move to `(x, y)` and release `button`.
    Up {
        /// Target X.
        x: i32,
        /// Target Y.
        y: i32,
        /// Which button.
        button: MouseButton,
    },
    /// Scroll by `dx`/`dy` notches (positive = right / down).
    Scroll {
        /// Horizontal notches.
        dx: i32,
        /// Vertical notches.
        dy: i32,
    },
    /// Press `button` at the start point, move to the end point, release.
    Drag {
        /// Start X.
        from_x: i32,
        /// Start Y.
        from_y: i32,
        /// End X.
        to_x: i32,
        /// End Y.
        to_y: i32,
        /// Which button.
        button: MouseButton,
    },
}

impl Modifier {
    /// Parses a modifier token: `ctrl`/`control`, `alt`/`option`, `shift`,
    /// `win`/`meta`/`cmd`/`super` (case-insensitive).
    pub fn parse(token: &str) -> Result<Self, String> {
        Ok(match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Modifier::Ctrl,
            "alt" | "option" => Modifier::Alt,
            "shift" => Modifier::Shift,
            "win" | "meta" | "cmd" | "super" => Modifier::Win,
            other => return Err(format!("unknown modifier: {other}")),
        })
    }
}

impl Key {
    /// Parses a key token: a named key (`enter`, `esc`, `pageup`, …), a
    /// function key (`f1`–`f24`), or a single character (`a`, `5`, `/`).
    pub fn parse(token: &str) -> Result<Self, String> {
        let lower = token.to_ascii_lowercase();
        Ok(match lower.as_str() {
            "enter" | "return" => Key::Enter,
            "tab" => Key::Tab,
            "space" => Key::Space,
            "backspace" | "bksp" => Key::Backspace,
            "delete" | "del" => Key::Delete,
            "escape" | "esc" => Key::Escape,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" | "pgup" => Key::PageUp,
            "pagedown" | "pgdn" => Key::PageDown,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            _ => {
                if let Some(n) = lower.strip_prefix('f')
                    && let Ok(num) = n.parse::<u8>()
                    && (1..=24).contains(&num)
                {
                    return Ok(Key::F(num));
                }
                let mut chars = token.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Key::Char(c),
                    _ => return Err(format!("unknown key: {token}")),
                }
            }
        })
    }
}

/// Parses a chord like `ctrl+shift+esc` into the held modifiers plus the main
/// key (the last `+`-separated token). The empty string is an error.
pub fn parse_chord(chord: &str) -> Result<(Vec<Modifier>, Key), String> {
    let parts: Vec<&str> = chord
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let (key_tok, mods) = parts.split_last().ok_or("empty key chord")?;
    let modifiers = mods
        .iter()
        .map(|m| Modifier::parse(m))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((modifiers, Key::parse(key_tok)?))
}

/// Successful results, one variant per [`Command`] family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Reply {
    /// Result of [`Command::RunCommand`].
    CommandOutput {
        /// Captured standard output.
        stdout: String,
        /// Captured standard error.
        stderr: String,
        /// Process exit code (`None` if killed by timeout).
        exit_code: Option<i32>,
    },
    /// An encoded image (from [`Command::Screenshot`]).
    Image(Image),
    /// Result of [`Command::ListWindows`].
    Windows(Vec<WindowInfo>),
    /// Result of [`Command::ListElements`].
    Elements(Vec<ElementInfo>),
    /// Result of [`Command::OpenApp`].
    AppOpened {
        /// Main window of the launched app, if it surfaced one promptly.
        window: Option<WindowId>,
        /// OS process id.
        pid: u32,
    },
    /// Contents from [`Command::ReadFile`].
    FileContents(Vec<u8>),
    /// Per-path content hashes from [`Command::HashFiles`].
    FileHashes(Vec<FileHash>),
    /// Relative file paths under a root, from [`Command::ListTree`].
    Tree(Vec<String>),
    /// Acknowledgement for commands with no return payload.
    Ack,
}

/// One file's content hash for sync diffing (from [`Command::HashFiles`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHash {
    /// Path relative to the hashed root (forward-slash separated).
    pub path: String,
    /// Lowercase-hex content hash, or `None` if the file is absent.
    pub hash: Option<String>,
}

/// An encoded screenshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// Encoding of [`Image::data`].
    pub format: ImageFormat,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Encoded image bytes.
    pub data: Vec<u8>,
}

/// Screenshot encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    /// WebP (default — best size/quality for an Agent's vision model).
    Webp,
    /// PNG (lossless fallback).
    Png,
}

/// A screen rectangle in absolute coordinates (pixels).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: i32,
    /// Height.
    pub height: i32,
}

/// Metadata for a top-level window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Native handle.
    pub id: WindowId,
    /// Window title.
    pub title: String,
    /// Owning process executable name.
    pub process: String,
    /// Whether the window is currently foreground.
    pub focused: bool,
    /// Screen rectangle of the window.
    #[serde(default)]
    pub rect: Rect,
}

/// A node in a window's UI Automation tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementInfo {
    /// Opaque handle for later [`Command::Click`] / [`Command::SetValue`].
    pub id: ElementId,
    /// UIA control type (e.g. `"Button"`, `"Edit"`).
    pub control_type: String,
    /// Accessible name, if any.
    pub name: Option<String>,
    /// UIA AutomationId — the app-assigned stable identifier, if any.
    #[serde(default)]
    pub automation_id: Option<String>,
    /// Current value (Value/RangeValue pattern), if the element exposes one.
    #[serde(default)]
    pub value: Option<String>,
    /// Bounding rectangle on screen.
    #[serde(default)]
    pub rect: Rect,
    /// Whether the element is enabled and on-screen.
    pub actionable: bool,
}

/// Attribute filter for [`Command::FindElements`]. An element matches when every
/// *provided* field matches (omitted fields are ignored); string comparisons are
/// ASCII-case-insensitive, and `name` is substring-matched when `name_contains`
/// is set, else compared whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementQuery {
    /// Accessible name to match.
    #[serde(default)]
    pub name: Option<String>,
    /// Treat `name` as a substring rather than the whole name.
    #[serde(default)]
    pub name_contains: bool,
    /// UIA AutomationId to match.
    #[serde(default)]
    pub automation_id: Option<String>,
    /// Control type to match (e.g. `"Button"`, `"Edit"`).
    #[serde(default)]
    pub control_type: Option<String>,
    /// Restrict to elements that are enabled and on-screen.
    #[serde(default)]
    pub actionable_only: bool,
}

impl ElementQuery {
    /// Whether `info` satisfies every provided criterion.
    #[must_use]
    pub fn matches(&self, info: &ElementInfo) -> bool {
        if self.actionable_only && !info.actionable {
            return false;
        }
        if let Some(ct) = &self.control_type
            && !info.control_type.eq_ignore_ascii_case(ct)
        {
            return false;
        }
        if let Some(aid) = &self.automation_id
            && info
                .automation_id
                .as_deref()
                .is_none_or(|v| !v.eq_ignore_ascii_case(aid))
        {
            return false;
        }
        if let Some(want) = &self.name {
            let Some(have) = &info.name else {
                return false;
            };
            let ok = if self.name_contains {
                have.to_ascii_lowercase()
                    .contains(&want.to_ascii_lowercase())
            } else {
                have.eq_ignore_ascii_case(want)
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

/// Interim progress emitted before a [`Response`]; correlated by [`RequestId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// A chunk of standard output from a streaming [`Command::RunCommand`].
    Stdout {
        /// Owning request.
        id: RequestId,
        /// Output fragment.
        chunk: String,
    },
    /// A chunk of standard error from a streaming [`Command::RunCommand`].
    Stderr {
        /// Owning request.
        id: RequestId,
        /// Error fragment.
        chunk: String,
    },
    /// A free-form progress note (e.g. build phase).
    Progress {
        /// Owning request.
        id: RequestId,
        /// Human-readable status.
        message: String,
    },
}

/// Structured failure returned in [`Response::result`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    /// Machine-readable category.
    pub kind: RemoteErrorKind,
    /// Human-readable detail.
    pub message: String,
}

/// Categories of runner-side failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RemoteErrorKind {
    /// The command is not permitted by the runner's allow-list policy.
    Denied,
    /// A referenced window, element, file or path was not found.
    NotFound,
    /// The operation exceeded its timeout.
    Timeout,
    /// An OS-level call failed (capture, input injection, spawn, ...).
    Os,
    /// The request was structurally invalid for the current state.
    Invalid,
}
