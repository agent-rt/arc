//! Android capability backend for the MVP runner: maps arc commands to the
//! device's own tools (`screencap`, `input`, `uiautomator`), which are reachable
//! at the runner's shell privilege. Everything shells out — the same shape as the
//! Windows runner calling Win32/PowerShell, just different tools.

use arc_proto::id::{ElementId, WindowId};
use arc_proto::wire::{
    CaptureTarget, ClickTarget, ElementInfo, ElementQuery, Image, ImageFormat, Key, Modifier, Rect,
    RemoteError, RemoteErrorKind, Reply, WindowInfo,
};
use tokio::process::Command;

/// Cap on elements returned by one `uiautomator` dump.
const MAX_ELEMENTS: usize = 250;

fn os(message: impl Into<String>) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::Os,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError {
        kind: RemoteErrorKind::Invalid,
        message: message.into(),
    }
}

/// Runs `program args…` (no shell) and returns raw stdout on success.
async fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, RemoteError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| os(format!("spawn {program} failed: {e}")))?;
    if !output.status.success() {
        return Err(os(format!(
            "{program} exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Runs a shell command via `sh -c` and captures its output.
pub async fn run_command(script: &str) -> Result<Reply, RemoteError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| os(format!("spawn sh failed: {e}")))?;
    Ok(Reply::CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
    })
}

/// `screencap -p` → a PNG. `FullScreen`/`Window` return the whole screen
/// (Android has one screen); `Element` crops to the element's bounds (its id
/// encodes `l,t,r,b`).
pub async fn screenshot(target: CaptureTarget) -> Result<Reply, RemoteError> {
    let data = run("screencap", &["-p"]).await?;
    let (width, height) =
        png_dimensions(&data).ok_or_else(|| os("screencap did not return a PNG".to_owned()))?;
    match target {
        CaptureTarget::FullScreen | CaptureTarget::Window(_) => Ok(Reply::Image(Image {
            format: ImageFormat::Png,
            width,
            height,
            data,
        })),
        CaptureTarget::Element(id) => crop_png(&data, parse_bounds_id(&id.0)?),
        CaptureTarget::Region {
            x,
            y,
            width,
            height,
        } => crop_png(
            &data,
            Rect {
                x,
                y,
                width: width as i32,
                height: height as i32,
            },
        ),
    }
}

/// Crops the full-screen PNG to `rect` (clamped to the image) and re-encodes it.
fn crop_png(data: &[u8], rect: Rect) -> Result<Reply, RemoteError> {
    let img = image::load_from_memory(data).map_err(|e| os(format!("decode screenshot: {e}")))?;
    let x = rect.x.max(0) as u32;
    let y = rect.y.max(0) as u32;
    let w = (rect.width.max(0) as u32).min(img.width().saturating_sub(x));
    let h = (rect.height.max(0) as u32).min(img.height().saturating_sub(y));
    let cropped = img.crop_imm(x, y, w, h);
    let mut out = Vec::new();
    cropped
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| os(format!("encode crop: {e}")))?;
    Ok(Reply::Image(Image {
        format: ImageFormat::Png,
        width: w,
        height: h,
        data: out,
    }))
}

/// Reads width/height from a PNG's IHDR (8-byte signature, then a chunk whose
/// data begins at offset 16: width, then height, big-endian u32).
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

/// `input tap` a point, or the centre of an element's bounds.
pub async fn click(target: ClickTarget) -> Result<Reply, RemoteError> {
    let (x, y) = match target {
        ClickTarget::Point { x, y, .. } => (x, y),
        ClickTarget::Element(id) => {
            let r = parse_bounds_id(&id.0)?;
            (r.x + r.width / 2, r.y + r.height / 2)
        }
    };
    run("input", &["tap", &x.to_string(), &y.to_string()]).await?;
    Ok(Reply::Ack)
}

/// Android element ids encode the element's bounds as `"<l>,<t>,<r>,<b>"` (no
/// stable runtime id exists in a `uiautomator` dump) — enough to both tap its
/// centre and crop a screenshot to it.
fn parse_bounds_id(id: &str) -> Result<Rect, RemoteError> {
    let n: Vec<i32> = id
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    match n[..] {
        [l, t, r, b] => Ok(Rect {
            x: l,
            y: t,
            width: r - l,
            height: b - t,
        }),
        _ => Err(invalid(format!(
            "malformed android element id '{id}' (want l,t,r,b)"
        ))),
    }
}

/// `input text` — spaces become `%s` (what `input` expects); passed as one argv
/// so no shell quoting is involved.
pub async fn type_text(text: &str) -> Result<Reply, RemoteError> {
    let encoded = text.replace(' ', "%s");
    run("input", &["text", &encoded]).await?;
    Ok(Reply::Ack)
}

/// `input keyevent` for named keys; a `Char` is typed as text. Modifiers are not
/// applied in the MVP (Android `input` can't easily hold them).
pub async fn key_chord(_modifiers: &[Modifier], key: Key) -> Result<Reply, RemoteError> {
    if let Key::Char(c) = key {
        return type_text(&c.to_string()).await;
    }
    let code = android_keycode(&key)
        .ok_or_else(|| invalid(format!("android MVP has no keycode mapping for {key:?}")))?;
    run("input", &["keyevent", code]).await?;
    Ok(Reply::Ack)
}

/// Maps arc's named keys to Android `KEYCODE_*` numbers.
fn android_keycode(key: &Key) -> Option<&'static str> {
    Some(match key {
        Key::Enter => "66",
        Key::Tab => "61",
        Key::Space => "62",
        Key::Backspace => "67",
        Key::Delete => "112",
        Key::Escape => "111",
        Key::Home => "122",
        Key::End => "123",
        Key::PageUp => "92",
        Key::PageDown => "93",
        Key::Up => "19",
        Key::Down => "20",
        Key::Left => "21",
        Key::Right => "22",
        Key::Char(_) | Key::F(_) => return None,
    })
}

/// Android has a single foreground UI; report it as one pseudo-window so the
/// controller's "pick a window, then list its elements" flow has a handle. The
/// element listing ignores the handle and dumps the current screen.
pub async fn list_windows() -> Result<Reply, RemoteError> {
    let focus = String::from_utf8_lossy(
        &run(
            "sh",
            &["-c", "dumpsys window 2>/dev/null | grep -m1 mCurrentFocus"],
        )
        .await
        .unwrap_or_default(),
    )
    .trim()
    .to_owned();
    let title = focus
        .split_once('/')
        .map(|(_, a)| a.trim_end_matches('}').to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "foreground".to_owned());
    let (w, h) = screen_size().await.unwrap_or((0, 0));
    Ok(Reply::Windows(vec![WindowInfo {
        id: WindowId(0),
        title,
        process: "android".to_owned(),
        focused: true,
        rect: Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        },
    }]))
}

async fn screen_size() -> Option<(i32, i32)> {
    // `wm size` → "Physical size: 1080x2340"
    let out = run("wm", &["size"]).await.ok()?;
    let s = String::from_utf8_lossy(&out);
    let dims = s.rsplit(':').next()?.trim();
    let (w, h) = dims.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// `uiautomator dump` the current screen and parse the nodes into elements.
pub async fn list_elements() -> Result<Reply, RemoteError> {
    Ok(Reply::Elements(dump().await?))
}

/// Like [`list_elements`] but filtered by `query` (arc's platform-agnostic matcher).
pub async fn find_elements(query: &ElementQuery) -> Result<Reply, RemoteError> {
    let hits = dump()
        .await?
        .into_iter()
        .filter(|e| query.matches(e))
        .collect();
    Ok(Reply::Elements(hits))
}

/// Runs `uiautomator dump` to a temp file, reads it, and parses the node tree.
async fn dump() -> Result<Vec<ElementInfo>, RemoteError> {
    let path = "/data/local/tmp/arc-uidump.xml";
    run(
        "sh",
        &["-c", &format!("uiautomator dump {path} >/dev/null 2>&1")],
    )
    .await?;
    let xml = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| os(format!("reading uiautomator dump: {e}")))?;
    parse_nodes(&xml)
}

/// Parses a `uiautomator` dump XML into [`ElementInfo`]s (capped).
fn parse_nodes(xml: &str) -> Result<Vec<ElementInfo>, RemoteError> {
    let doc =
        roxmltree::Document::parse(xml).map_err(|e| os(format!("parsing uiautomator XML: {e}")))?;
    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("node")) {
        if out.len() >= MAX_ELEMENTS {
            break;
        }
        let attr = |k: &str| node.attribute(k).unwrap_or("");
        let Some(rect) = parse_bounds(attr("bounds")) else {
            continue;
        };
        let clickable = attr("clickable") == "true";
        let enabled = attr("enabled") == "true";
        let class = attr("class");
        let control_type = class.rsplit('.').next().unwrap_or(class).to_owned();
        let text = non_empty(attr("text"));
        let desc = non_empty(attr("content-desc"));
        // Encode bounds `l,t,r,b` in the id so it can be both tapped (centre) and
        // captured (crop).
        let id = format!(
            "{},{},{},{}",
            rect.x,
            rect.y,
            rect.x + rect.width,
            rect.y + rect.height
        );
        out.push(ElementInfo {
            id: ElementId(id),
            control_type,
            name: desc.clone(),
            automation_id: non_empty(attr("resource-id")),
            value: text.or(desc),
            rect,
            actionable: clickable && enabled,
        });
    }
    Ok(out)
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

/// Parses `uiautomator` bounds `"[l,t][r,b]"` into a [`Rect`].
fn parse_bounds(s: &str) -> Option<Rect> {
    let s = s.strip_prefix('[')?.strip_suffix(']')?;
    let (tl, br) = s.split_once("][")?;
    let (l, t) = tl.split_once(',')?;
    let (r, b) = br.split_once(',')?;
    let (l, t, r, b) = (
        l.trim().parse::<i32>().ok()?,
        t.trim().parse::<i32>().ok()?,
        r.trim().parse::<i32>().ok()?,
        b.trim().parse::<i32>().ok()?,
    );
    Some(Rect {
        x: l,
        y: t,
        width: r - l,
        height: b - t,
    })
}
