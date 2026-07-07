//! Android capability backend for the MVP runner: maps arc commands to the
//! device's own tools (`screencap`, `input`, `uiautomator`), which are reachable
//! at the runner's shell privilege. Everything shells out — the same shape as the
//! Windows runner calling Win32/PowerShell, just different tools.

use arc_proto::id::{ElementId, RequestId, WindowId};
use arc_proto::wire::{
    CaptureTarget, ClickTarget, ElementInfo, ElementQuery, Image, ImageFormat, Key, Modifier,
    MouseAction, ProcessInfo, Rect, RemoteError, RemoteErrorKind, Reply, WindowInfo,
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

/// Launches a script **detached**: writes `content` to a temp `.sh`, spawns it
/// with stdin from `/dev/null` and stdout+stderr to a log file, and returns
/// immediately with the pid + log path. Uses [`std::process`] (not tokio) so
/// dropping the handle doesn't reap or kill the child; the long-lived runner is
/// its parent and has no controlling terminal, so the child outlives the
/// request (and the connection). Mirrors the Windows `run_detached`.
pub async fn run_detached(
    id: RequestId,
    content: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<Reply, RemoteError> {
    let script = format!("/data/local/tmp/arc-detached-{}.sh", id.0);
    let log = format!("/data/local/tmp/arc-detached-{}.log", id.0);
    tokio::fs::write(&script, content)
        .await
        .map_err(|e| os(format!("writing detached script: {e}")))?;
    let out = std::fs::File::create(&log).map_err(|e| os(format!("creating log {log}: {e}")))?;
    let err = out
        .try_clone()
        .map_err(|e| os(format!("cloning log fd: {e}")))?;
    let child = std::process::Command::new("sh")
        .arg(&script)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| os(format!("spawning detached process: {e}")))?;
    Ok(Reply::Detached {
        pid: child.id(),
        log_path: log,
    })
}

/// A best-effort process dump. Android's shell can't take a thread-stack
/// minidump (`debuggerd` needs root), so this captures the readable
/// `/proc/<pid>` entries into a text file: `cmdline`, `status`, `wchan` (what a
/// hung process is blocked in — the key no-root hang signal), `stat`, and
/// `maps` (own processes only; cross-uid maps are denied). Returns the file
/// path + size, pulled back like the Windows `.dmp`.
pub async fn proc_dump(pid: u32) -> Result<Reply, RemoteError> {
    if tokio::fs::metadata(format!("/proc/{pid}")).await.is_err() {
        return Err(invalid(format!("no such process: {pid}")));
    }
    let mut buf = String::new();
    buf.push_str(&format!(
        "# arc proc dump of pid {pid} (Android; no thread stacks without root)\n\n"
    ));
    for f in ["cmdline", "status", "wchan", "stat", "maps"] {
        let body = tokio::fs::read_to_string(format!("/proc/{pid}/{f}"))
            .await
            .map(|s| s.replace('\0', " "))
            .unwrap_or_else(|e| format!("<unavailable: {e}>"));
        buf.push_str(&format!(
            "===== /proc/{pid}/{f} =====\n{}\n\n",
            body.trim_end()
        ));
    }
    let path = format!("/data/local/tmp/arc-procdump-{pid}.txt");
    tokio::fs::write(&path, &buf)
        .await
        .map_err(|e| os(format!("writing procdump: {e}")))?;
    Ok(Reply::Dumped {
        path,
        size: buf.len() as u64,
    })
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

/// Runs a script's `content` via `sh`, passing `args` as positional parameters
/// (`$1`, `$2`, …) and `env` as environment. `sh -c <content> sh <args…>` sets
/// `$0` to "sh" and binds the rest positionally — so `arc run x.sh a b` reaches
/// the script as `$1=a $2=b` instead of being silently dropped.
pub async fn run_script(
    content: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<Reply, RemoteError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(content)
        .arg("sh")
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
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

/// Lists processes via toybox `ps`. `filter` keeps names containing it; with
/// `with_cpu` the list is sorted by CPU% (its `%CPU` is a lifetime average, not
/// a 500ms sample — enough to spot a busy process), else by memory.
pub async fn list_processes(filter: Option<&str>, with_cpu: bool) -> Result<Reply, RemoteError> {
    let mut procs = ps_snapshot().await?;
    if let Some(f) = filter {
        let f = f.to_lowercase();
        procs.retain(|p| p.name.to_lowercase().contains(&f));
    }
    if with_cpu {
        procs.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        procs.sort_by_key(|p| std::cmp::Reverse(p.memory_kb));
    }
    Ok(Reply::Processes(procs))
}

/// One `ps -A -o PID,PCPU,RSS,NAME` snapshot → [`ProcessInfo`]s. Columns are
/// whitespace-separated with the name last (RSS is in KiB); the header is
/// skipped.
async fn ps_snapshot() -> Result<Vec<ProcessInfo>, RemoteError> {
    let out = run("ps", &["-A", "-o", "PID,PCPU,RSS,NAME"]).await?;
    let text = String::from_utf8_lossy(&out);
    let mut procs = Vec::new();
    for line in text.lines().skip(1) {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(cpu), Some(rss)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let name = it.collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            continue;
        }
        procs.push(ProcessInfo {
            pid,
            name,
            memory_kb: rss.parse::<u64>().ok(),
            cpu_percent: cpu.parse::<f32>().ok(),
        });
    }
    Ok(procs)
}

/// Kills by PID (all-digit target) or by exact process name (case-insensitive),
/// via `kill -9`. Returns the matched set (killed, or would-be-killed on
/// `dry_run`). Name matching is exact — mirroring Windows `Get-Process -Name` —
/// so a broad substring can't fan out into an accidental mass kill.
pub async fn kill_process(target: &str, dry_run: bool) -> Result<Reply, RemoteError> {
    let all = ps_snapshot().await?;
    let matches: Vec<ProcessInfo> = if target.chars().all(|c| c.is_ascii_digit()) {
        let pid: u32 = target
            .parse()
            .map_err(|_| invalid(format!("bad pid: {target}")))?;
        all.into_iter().filter(|p| p.pid == pid).collect()
    } else {
        let want = target.strip_suffix(".exe").unwrap_or(target).to_lowercase();
        all.into_iter()
            .filter(|p| p.name.to_lowercase() == want)
            .collect()
    };
    if !dry_run {
        for p in &matches {
            let _ = run("kill", &["-9", &p.pid.to_string()]).await;
        }
    }
    Ok(Reply::Processes(matches))
}

/// Reports the runner's Unix identity: user/uid, whether it's root, and (best
/// effort) its SELinux context.
pub async fn identity() -> Result<Reply, RemoteError> {
    let uid = String::from_utf8_lossy(&run("id", &["-u"]).await?)
        .trim()
        .to_string();
    let user = String::from_utf8_lossy(&run("id", &["-un"]).await?)
        .trim()
        .to_string();
    let mut lines = vec![
        ("account".to_string(), format!("{user} (uid {uid})")),
        ("elevated".to_string(), (uid == "0").to_string()),
    ];
    if let Ok(ctx) = run("id", &["-Z"]).await {
        let ctx = String::from_utf8_lossy(&ctx).trim().to_string();
        if !ctx.is_empty() {
            lines.push(("selinux".to_string(), ctx));
        }
    }
    Ok(Reply::Identity(lines))
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

/// Reads one element's text: re-dumps the tree and returns the value (text,
/// else content-desc) of the node at `id`. Android exposes no stable element
/// handle, so the bounds-encoded id is the lookup key.
pub async fn read_element(id: &ElementId) -> Result<Reply, RemoteError> {
    let want = parse_bounds_id(&id.0)?;
    let el = element_at(dump().await?, want)
        .ok_or_else(|| invalid(format!("no element at {}", id.0)))?;
    Ok(Reply::Text(el.value.unwrap_or_default()))
}

/// The dumped element at `want`'s centre: the smallest node whose bounds
/// contain that point. Matches by point, not exact bounds — a field's edges
/// reflow (e.g. a clear button appears after typing, shrinking it), so exact
/// equality is too brittle to re-find an element across a content change.
fn element_at(els: Vec<ElementInfo>, want: Rect) -> Option<ElementInfo> {
    let (cx, cy) = (want.x + want.width / 2, want.y + want.height / 2);
    els.into_iter()
        .filter(|e| {
            let r = &e.rect;
            r.x <= cx && cx <= r.x + r.width && r.y <= cy && cy <= r.y + r.height
        })
        .min_by_key(|e| i64::from(e.rect.width) * i64::from(e.rect.height))
}

/// Sets an editable field's text. Android's shell has no UIA Value-pattern
/// equivalent, so this focuses the field (tap its centre), clears the existing
/// text (move to end, then backspace its current length), and `input text`s the
/// new value. Works for ordinary editable text fields.
pub async fn set_value(id: &ElementId, value: &str) -> Result<Reply, RemoteError> {
    let want = parse_bounds_id(&id.0)?;
    // Current length (so we know how many characters to delete). Bounded below.
    let cur_len = element_at(dump().await?, want)
        .and_then(|e| e.value)
        .map_or(0, |v| v.chars().count());
    // Focus by tapping the element centre.
    let cx = (want.x + want.width / 2).to_string();
    let cy = (want.y + want.height / 2).to_string();
    run("input", &["tap", &cx, &cy]).await?;
    // Move the caret to the end, then delete the existing text.
    run("input", &["keyevent", "123"]).await?; // KEYCODE_MOVE_END
    if cur_len > 0 {
        // `input keyevent 67 67 …` — KEYCODE_DEL (backspace), one per char (capped).
        let dels: Vec<&str> = std::iter::once("keyevent")
            .chain(std::iter::repeat_n("67", cur_len.min(500)))
            .collect();
        run("input", &dels).await?;
    }
    if !value.is_empty() {
        run("input", &["text", &value.replace(' ', "%s")]).await?;
    }
    Ok(Reply::Ack)
}

/// Gives an element keyboard focus. Android has no "focus without activating"
/// shell primitive, so this taps the element's centre — which focuses a
/// focusable control (a text field takes the caret + keyboard). Pairs with
/// `key`/`type` ("focus this field, then send keys").
pub async fn focus_element(id: &ElementId) -> Result<Reply, RemoteError> {
    let r = parse_bounds_id(&id.0)?;
    let cx = (r.x + r.width / 2).to_string();
    let cy = (r.y + r.height / 2).to_string();
    run("input", &["tap", &cx, &cy]).await?;
    Ok(Reply::Ack)
}

/// Android has a single foreground surface, so there is no background window to
/// raise. The useful analogue of Windows' "bring the target forward so
/// input/capture lands on a live, visible window" is to wake the screen and
/// dismiss the keyguard (best effort — a secured lock needs the user).
pub async fn activate_window(_window: WindowId) -> Result<Reply, RemoteError> {
    run("input", &["keyevent", "224"]).await?; // KEYCODE_WAKEUP
    let _ = run("wm", &["dismiss-keyguard"]).await;
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

/// Launches an app: `am start -n <pkg>/<activity>` for a full component, else
/// `monkey` on a bare package (its launcher activity). Reports the pid via
/// `pidof` if the process is up (0 if not found).
pub async fn open_app(target: &str, _args: &[String]) -> Result<Reply, RemoteError> {
    if target.contains('/') {
        run("am", &["start", "-n", target]).await?;
    } else {
        run(
            "monkey",
            &["-p", target, "-c", "android.intent.category.LAUNCHER", "1"],
        )
        .await?;
    }
    // Give the process a moment to come up before looking it up.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let pkg = target.split('/').next().unwrap_or(target);
    let pid = run("pidof", &[pkg])
        .await
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o)
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0);
    Ok(Reply::AppOpened {
        window: None,
        pid,
        exit_code: None,
        diagnostic: None,
    })
}

/// Maps a mouse action to `input` gestures: click→tap, drag→swipe, scroll→a
/// swipe from the screen centre. Move/Down/Up have no Android analogue (there is
/// no persistent cursor) and are rejected.
pub async fn mouse(action: MouseAction) -> Result<Reply, RemoteError> {
    match action {
        MouseAction::Click { x, y, .. } => {
            run("input", &["tap", &x.to_string(), &y.to_string()]).await?;
        }
        MouseAction::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            ..
        } => {
            swipe(from_x, from_y, to_x, to_y).await?;
        }
        MouseAction::Scroll { dx, dy } => {
            // Swipe from the screen centre; ~300px per notch. Positive dy is
            // "scroll down" (content moves up), so the finger swipes up.
            let (w, h) = screen_size().await.unwrap_or((1080, 1920));
            const STEP: i32 = 300;
            swipe(w / 2, h / 2, w / 2 - dx * STEP, h / 2 - dy * STEP).await?;
        }
        MouseAction::Move { .. } | MouseAction::Down { .. } | MouseAction::Up { .. } => {
            return Err(invalid(
                "android has no persistent cursor — use click/drag/scroll (tap/swipe)",
            ));
        }
    }
    Ok(Reply::Ack)
}

async fn swipe(x1: i32, y1: i32, x2: i32, y2: i32) -> Result<(), RemoteError> {
    run(
        "input",
        &[
            "swipe",
            &x1.to_string(),
            &y1.to_string(),
            &x2.to_string(),
            &y2.to_string(),
            "300",
        ],
    )
    .await
    .map(|_| ())
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
    // The resumed activity's `pkg/activity` is the closest thing to a window title.
    let resumed = String::from_utf8_lossy(
        &run(
            "sh",
            &[
                "-c",
                "dumpsys activity activities 2>/dev/null | grep -m1 mResumedActivity",
            ],
        )
        .await
        .unwrap_or_default(),
    )
    .into_owned();
    let title = resumed
        .split_whitespace()
        .find(|t| t.contains('/'))
        .map(|t| t.trim_end_matches('}').to_owned())
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
