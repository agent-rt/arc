use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use arc_net::Controller;
use arc_proto::wire::{Command, Reply, Shell};
use blake2::{Blake2s256, Digest};
use notify::Watcher as _;
use tokio::sync::mpsc;

use crate::ack;

/// Directories never synced (build outputs, VCS) on top of `.gitignore` rules.
const SKIP_DIRS: &[&str] = &["target", "bin", "obj", "node_modules", ".git"];

/// Bytes per file-transfer frame (under the protocol's 32 MiB frame cap).
const CHUNK: usize = 8 * 1024 * 1024;

/// Sends `local` to the runner. A single file is copied wholesale; a directory
/// transfers incrementally (skipping files whose content already matches, unless
/// `whole`) and, with `delete`, prunes runner files absent locally.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn push(
    controller: &mut Controller,
    local: &str,
    remote: &str,
    delete: bool,
    dry_run: bool,
    whole: bool,
    no_ignore: bool,
) -> Result<()> {
    let meta = std::fs::metadata(local).with_context(|| format!("stat {local}"))?;
    if !meta.is_dir() {
        let data = std::fs::read(local).with_context(|| format!("reading {local}"))?;
        if dry_run {
            println!("would push {local} ({} bytes) -> {remote}", data.len());
            return Ok(());
        }
        push_bytes(controller, remote, &data).await?;
        println!("pushed {local} ({} bytes) -> {remote}", data.len());
        return Ok(());
    }

    let Stats {
        changed,
        total,
        bytes,
        removed,
    } = push_tree(controller, local, remote, delete, dry_run, whole, no_ignore).await?;
    print_transfer_summary(
        dry_run, changed, total, bytes, delete, removed, local, remote,
    );
    Ok(())
}

/// Per-transfer counters returned by [`push_tree`].
#[derive(Default)]
struct Stats {
    changed: u64,
    total: usize,
    bytes: u64,
    removed: u64,
}

/// Incrementally pushes the directory `local` to `remote` (the body shared by
/// `push` of a directory and `watch`), printing each transferred/deleted file
/// and returning the counters for the caller to summarize.
#[allow(clippy::too_many_arguments)]
async fn push_tree(
    controller: &mut Controller,
    local: &str,
    remote: &str,
    delete: bool,
    dry_run: bool,
    whole: bool,
    no_ignore: bool,
) -> Result<Stats> {
    let files = collect_files(Path::new(local), no_ignore)?;
    if files.is_empty() {
        return Ok(Stats::default());
    }
    let mut local_hashes: Vec<(String, PathBuf, String)> = Vec::with_capacity(files.len());
    for (rel, abs) in files {
        let data = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
        local_hashes.push((rel, abs, blake2_hex(&data)));
    }

    // One round-trip: what does the runner already have? (Skipped for --whole.)
    let remote_hashes: HashMap<String, Option<String>> = if whole {
        HashMap::new()
    } else {
        let paths: Vec<String> = local_hashes.iter().map(|(rel, _, _)| rel.clone()).collect();
        match controller
            .request(Command::HashFiles {
                root: remote.to_owned(),
                paths,
            })
            .await?
        {
            Reply::FileHashes(list) => list.into_iter().map(|h| (h.path, h.hash)).collect(),
            other => bail!("unexpected reply: {other:?}"),
        }
    };

    let (mut changed, mut bytes) = (0u64, 0u64);
    for (rel, abs, local_hash) in &local_hashes {
        if !whole && remote_hashes.get(rel).and_then(|h| h.as_deref()) == Some(local_hash.as_str())
        {
            continue; // identical on the runner
        }
        changed += 1;
        if dry_run {
            println!("would push {rel}");
            continue;
        }
        let data = std::fs::read(abs)?;
        push_bytes(controller, &join_remote(remote, rel), &data).await?;
        bytes += data.len() as u64;
        println!("→ {rel} ({} bytes)", data.len());
    }

    let mut removed = 0u64;
    if delete {
        let local_set: HashSet<&str> = local_hashes
            .iter()
            .map(|(rel, _, _)| rel.as_str())
            .collect();
        let remote_tree = match controller
            .request(Command::ListTree {
                root: remote.to_owned(),
                all: false,
            })
            .await?
        {
            Reply::Tree(paths) => paths,
            other => bail!("unexpected reply: {other:?}"),
        };
        for rel in &remote_tree {
            if local_set.contains(rel.as_str()) {
                continue;
            }
            removed += 1;
            if dry_run {
                println!("would delete {rel}");
                continue;
            }
            ack(
                controller,
                Command::DeleteFile {
                    path: join_remote(remote, rel),
                },
            )
            .await?;
            println!("✗ {rel}");
        }
    }

    Ok(Stats {
        changed,
        total: local_hashes.len(),
        bytes,
        removed,
    })
}

/// Watches `local` and incrementally pushes changes to `remote` until Ctrl+C.
/// Build/VCS dirs are ignored at the watcher level (no churn during `cargo
/// build`); each burst of events is debounced, then a hash-diff sync transfers
/// only what actually changed.
pub(crate) async fn watch(
    controller: &mut Controller,
    local: &str,
    remote: &str,
    on_change: Option<&str>,
    no_ignore: bool,
) -> Result<()> {
    let initial = push_tree(controller, local, remote, false, false, false, no_ignore).await?;
    println!(
        "initial sync: {}/{} files ({} bytes) {local} -> {remote}",
        initial.changed, initial.total, initial.bytes
    );
    // Run the hook once at startup so a fresh `watch` produces a baseline build.
    if let Some(cmd) = on_change {
        run_on_change(controller, cmd).await;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && !matches!(event.kind, notify::EventKind::Access(_))
            && event.paths.iter().any(|p| is_syncable(p, no_ignore))
        {
            let _ = tx.send(());
        }
    })
    .context("initialising file watcher")?;
    watcher
        .watch(Path::new(local), notify::RecursiveMode::Recursive)
        .with_context(|| format!("watching {local}"))?;
    println!("watching {local} (Ctrl+C to stop)…");

    while rx.recv().await.is_some() {
        // Debounce: keep draining until the filesystem goes quiet.
        loop {
            match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return Ok(()),
                Err(_) => break,
            }
        }
        match push_tree(controller, local, remote, false, false, false, no_ignore).await {
            Ok(s) if s.changed > 0 => {
                println!("synced {} files ({} bytes)", s.changed, s.bytes);
                if let Some(cmd) = on_change {
                    run_on_change(controller, cmd).await;
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("arc: sync error: {e:#}"),
        }
    }
    Ok(())
}

/// Runs the `watch --on-change` hook on the runner, streaming its output. Errors
/// and non-zero exits are reported but never abort the watch loop.
async fn run_on_change(controller: &mut Controller, cmd: &str) {
    println!("→ on-change: {cmd}");
    let result = crate::exec::stream_run(
        controller,
        Command::RunCommand {
            shell: Shell::PowerShell,
            command: cmd.to_owned(),
            env: Vec::new(),
            timeout_ms: None, // a build/test may run long; the user Ctrl+Cs the watch
            stream: true,
        },
    )
    .await;
    match result {
        Ok(0) => {}
        Ok(code) => eprintln!("arc: on-change exited {code}"),
        Err(e) => eprintln!("arc: on-change error: {e:#}"),
    }
}

/// True if `path` should trigger a watch resync — never inside `.git`, and
/// (unless `no_ignore`) not inside a build directory either.
fn is_syncable(path: &Path, no_ignore: bool) -> bool {
    !path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name == ".git" || (!no_ignore && SKIP_DIRS.contains(&name.as_ref()))
    })
}

/// Prints the trailing one-line summary shared by directory `push`/`pull`.
#[allow(clippy::too_many_arguments)]
fn print_transfer_summary(
    dry_run: bool,
    changed: u64,
    total: usize,
    bytes: u64,
    delete: bool,
    removed: u64,
    src: &str,
    dst: &str,
) {
    let verb = if dry_run {
        "would transfer"
    } else {
        "transferred"
    };
    let deleted = if delete {
        format!(", {removed} deleted")
    } else {
        String::new()
    };
    println!("{verb} {changed}/{total} files ({bytes} bytes){deleted}: {src} -> {dst}");
}

/// Writes a whole file to `remote` in chunks (offset 0 truncates/creates).
async fn push_bytes(controller: &mut Controller, remote: &str, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return ack(
            controller,
            Command::WriteFile {
                path: remote.to_owned(),
                contents: Vec::new(),
                offset: 0,
            },
        )
        .await;
    }
    let mut offset = 0u64;
    for chunk in data.chunks(CHUNK) {
        ack(
            controller,
            Command::WriteFile {
                path: remote.to_owned(),
                contents: chunk.to_vec(),
                offset,
            },
        )
        .await?;
        offset += chunk.len() as u64;
    }
    Ok(())
}

/// Walks `root` respecting `.gitignore`, additionally skipping [`SKIP_DIRS`];
/// returns `(forward-slash relative path, absolute path)` per file.
fn collect_files(root: &Path, no_ignore: bool) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(false);
    if no_ignore {
        // Ship everything under `root`, including .gitignore'd files and build
        // dirs — for deploying build artifacts (see `push --no-ignore`).
        builder.standard_filters(false);
    }
    for entry in builder.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        // `.git` is never shipped, even with --no-ignore; the other build dirs
        // are skipped only in the default (ignore-respecting) mode.
        if rel.components().any(|c| {
            let name = c.as_os_str().to_string_lossy();
            name == ".git" || (!no_ignore && SKIP_DIRS.contains(&name.as_ref()))
        }) {
            continue;
        }
        files.push((
            rel.to_string_lossy().replace('\\', "/"),
            entry.path().to_path_buf(),
        ));
    }
    files.sort();
    Ok(files)
}

fn join_remote(remote: &str, rel: &str) -> String {
    format!("{}/{rel}", remote.trim_end_matches('/'))
}

fn blake2_hex(data: &[u8]) -> String {
    let mut hasher = Blake2s256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Fetches `remote` to the runner. A single file is copied wholesale; a
/// directory transfers incrementally (skipping files already matching locally,
/// unless `whole`) and, with `delete`, prunes local files absent on the runner.
///
/// A non-empty [`Command::ListTree`] means `remote` is a directory (build dirs
/// excluded); an empty one means a single file (or absent) → a file pull, which
/// also lets you fetch one artifact from inside an otherwise-skipped build dir.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pull(
    controller: &mut Controller,
    remote: &str,
    local: &str,
    delete: bool,
    dry_run: bool,
    whole: bool,
    no_ignore: bool,
    watch: bool,
    interval: u64,
) -> Result<()> {
    if !watch {
        let s = pull_sync(controller, remote, local, delete, dry_run, whole, no_ignore).await?;
        print_transfer_summary(
            dry_run, s.changed, s.total, s.bytes, delete, s.removed, remote, local,
        );
        return Ok(());
    }

    // Reverse watch: poll the runner on an interval and pull only what changed —
    // the box→controller companion to `arc watch` (build on the box, artifacts
    // flow back). No runner-side file watcher; polling keeps it simple.
    let initial = pull_sync(controller, remote, local, delete, false, whole, no_ignore).await?;
    println!(
        "initial pull: {}/{} files ({} bytes) {remote} -> {local}",
        initial.changed, initial.total, initial.bytes
    );
    println!("watching {remote} every {interval}s (Ctrl+C to stop)…");
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        match pull_sync(controller, remote, local, delete, false, whole, no_ignore).await {
            Ok(s) if s.changed > 0 || s.removed > 0 => {
                let removed = if s.removed > 0 {
                    format!(", removed {}", s.removed)
                } else {
                    String::new()
                };
                println!("pulled {} file(s) ({} bytes){removed}", s.changed, s.bytes);
            }
            Ok(_) => {}
            Err(e) => eprintln!("arc: pull error: {e:#}"),
        }
    }
}

/// One reverse-sync pass: pull `remote` → `local` incrementally, returning the
/// counts (no summary print, so `pull --watch` can loop it). `no_ignore` lists
/// build dirs too (for fetching artifacts).
#[allow(clippy::too_many_arguments)]
async fn pull_sync(
    controller: &mut Controller,
    remote: &str,
    local: &str,
    delete: bool,
    dry_run: bool,
    whole: bool,
    no_ignore: bool,
) -> Result<Stats> {
    let tree = match controller
        .request(Command::ListTree {
            root: remote.to_owned(),
            all: no_ignore,
        })
        .await?
    {
        Reply::Tree(paths) => paths,
        other => bail!("unexpected reply: {other:?}"),
    };
    if tree.is_empty() {
        // A single file (or absent root): always copied wholesale.
        let bytes = if dry_run {
            println!("would pull {remote} -> {local}");
            0
        } else {
            pull_to(controller, remote, Path::new(local)).await?
        };
        return Ok(Stats {
            changed: 1,
            total: 1,
            bytes,
            removed: 0,
        });
    }

    let remote_hashes: HashMap<String, Option<String>> = if whole {
        HashMap::new()
    } else {
        match controller
            .request(Command::HashFiles {
                root: remote.to_owned(),
                paths: tree.clone(),
            })
            .await?
        {
            Reply::FileHashes(list) => list.into_iter().map(|h| (h.path, h.hash)).collect(),
            other => bail!("unexpected reply: {other:?}"),
        }
    };

    let local_root = Path::new(local);
    let (mut changed, mut bytes) = (0u64, 0u64);
    for rel in &tree {
        if !whole {
            let remote_hash = remote_hashes.get(rel).and_then(|h| h.clone());
            let local_path = local_root.join(rel);
            let local_hash = if local_path.is_file() {
                Some(blake2_hex(&std::fs::read(&local_path)?))
            } else {
                None
            };
            if remote_hash.is_some() && local_hash == remote_hash {
                continue; // identical locally
            }
        }
        changed += 1;
        if dry_run {
            println!("would pull {rel}");
            continue;
        }
        bytes += pull_to(controller, &join_remote(remote, rel), &local_root.join(rel)).await?;
        println!("← {rel}");
    }

    let mut removed = 0u64;
    if delete && local_root.exists() {
        let remote_set: HashSet<&str> = tree.iter().map(|s| s.as_str()).collect();
        // Pull-delete keeps its default filtering: never prune local ignored /
        // build files just because the runner doesn't have them.
        for (rel, abs) in collect_files(local_root, false)? {
            if remote_set.contains(rel.as_str()) {
                continue;
            }
            removed += 1;
            if dry_run {
                println!("would delete (local) {rel}");
                continue;
            }
            std::fs::remove_file(&abs).with_context(|| format!("deleting {}", abs.display()))?;
            println!("✗ (local) {rel}");
        }
    }

    Ok(Stats {
        changed,
        total: tree.len(),
        bytes,
        removed,
    })
}

/// Reads `remote` in chunks into `local` (creating parent dirs); returns bytes.
/// Writes a minidump of remote process `pid` on the runner, then pulls it back
/// to `out` (default `procdump-<pid>.dmp`). arc only captures + transfers the
/// dump; open it locally in WinDbg/cdb with symbols to read the call stacks.
pub(crate) async fn procdump(
    controller: &mut Controller,
    pid: u32,
    out: Option<&str>,
) -> Result<i32> {
    let (remote, size) = match controller.request(Command::ProcDump { pid }).await? {
        Reply::Dumped { path, size } => (path, size),
        other => bail!("unexpected reply: {other:?}"),
    };
    let default = format!("procdump-{pid}.dmp");
    let local = Path::new(out.unwrap_or(&default));
    println!("dump: {remote} ({size} bytes on runner) → pulling…");
    let pulled = pull_to(controller, &remote, local).await?;
    println!("saved {} ({pulled} bytes)", local.display());
    println!(
        "analyze locally, e.g.: cdb -z '{}' (needs symbols)",
        local.display()
    );
    Ok(0)
}

async fn pull_to(controller: &mut Controller, remote: &str, local: &Path) -> Result<u64> {
    if let Some(parent) = local.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file =
        std::fs::File::create(local).with_context(|| format!("creating {}", local.display()))?;
    let mut offset = 0u64;
    loop {
        let reply = controller
            .request(Command::ReadFile {
                path: remote.to_owned(),
                offset,
                max_len: CHUNK as u64,
            })
            .await?;
        let bytes = match reply {
            Reply::FileContents(bytes) => bytes,
            other => bail!("unexpected reply: {other:?}"),
        };
        let read = bytes.len();
        file.write_all(&bytes)?;
        offset += read as u64;
        if read < CHUNK {
            break;
        }
    }
    Ok(offset)
}

/// Streams a remote file to stdout in chunks (UTF-8, lossy).
pub(crate) async fn cat(controller: &mut Controller, remote: &str) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let mut offset = 0u64;
    loop {
        let reply = controller
            .request(Command::ReadFile {
                path: remote.to_owned(),
                offset,
                max_len: CHUNK as u64,
            })
            .await?;
        let bytes = match reply {
            Reply::FileContents(bytes) => bytes,
            other => bail!("unexpected reply: {other:?}"),
        };
        let read = bytes.len();
        stdout.write_all(String::from_utf8_lossy(&bytes).as_bytes())?;
        offset += read as u64;
        if read < CHUNK {
            break;
        }
    }
    stdout.flush()?;
    Ok(())
}
