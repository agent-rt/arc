//! The Windows [`Backend`]: implements every capability by delegating to the
//! `exec`/`capture`/`apps`/`uia`/`input`/`clipboard` modules. Capture, UI
//! Automation and input injection are blocking, thread-affine (COM / SendInput)
//! operations, so they run on [`tokio::task::spawn_blocking`] rather than
//! occupying an async worker. File transfer is handled by the shared dispatcher.

use arc_proto::id::{ElementId, RequestId, WindowId};
use arc_proto::wire::{
    Capability, CaptureTarget, ClickTarget, ElementQuery, ImageFormat, Key, Modifier, MouseAction,
    ProcessInfo, Reply, Shell,
};
use arc_runner_core::{Backend, RemoteResult, os_error};

use crate::{apps, capture, clipboard, exec, input, uia};

pub struct WindowsBackend;

impl Backend for WindowsBackend {
    fn capabilities(&self) -> Vec<Capability> {
        // The Windows backend implements the full semantic surface (every method
        // overridden below); file transfer + tunnel are added by the dispatcher.
        vec![
            Capability::RunCommand,
            Capability::RunScript,
            Capability::RunDetached,
            Capability::Screenshot,
            Capability::OpenApp,
            Capability::ProcDump,
            Capability::ListWindows,
            Capability::ActivateWindow,
            Capability::ListElements,
            Capability::FindElements,
            Capability::Click,
            Capability::TypeText,
            Capability::KeyChord,
            Capability::Mouse,
            Capability::SetValue,
            Capability::ReadElement,
            Capability::FocusElement,
            Capability::ClipboardGet,
            Capability::ClipboardSet,
            Capability::ListProcesses,
            Capability::KillProcess,
            Capability::Identity,
        ]
    }

    async fn run_command(
        &self,
        shell: Shell,
        command: String,
        env: Vec<(String, String)>,
        timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        exec::run_command(shell, &command, &env, timeout_ms).await
    }

    async fn run_script(
        &self,
        id: RequestId,
        shell: Shell,
        content: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        exec::run_script(id, shell, &content, &args, &env, timeout_ms).await
    }

    async fn run_detached(
        &self,
        id: RequestId,
        shell: Shell,
        content: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> RemoteResult<Reply> {
        blocking(move || exec::run_detached(id, shell, &content, &args, &env)).await
    }

    async fn screenshot(
        &self,
        target: CaptureTarget,
        format: Option<ImageFormat>,
        settle_ms: Option<u64>,
        settle_await_change: bool,
    ) -> RemoteResult<Reply> {
        blocking(move || capture::screenshot(target, format, settle_ms, settle_await_change)).await
    }

    async fn open_app(
        &self,
        target: String,
        args: Vec<String>,
        watch_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        blocking(move || apps::open_app(&target, &args, watch_ms)).await
    }

    async fn proc_dump(&self, pid: u32) -> RemoteResult<Reply> {
        blocking(move || apps::proc_dump(pid)).await
    }

    async fn list_windows(&self) -> RemoteResult<Reply> {
        blocking(apps::list_windows).await
    }

    async fn activate_window(&self, window: WindowId) -> RemoteResult<Reply> {
        blocking(move || apps::activate_window(window)).await
    }

    async fn list_elements(&self, window: WindowId) -> RemoteResult<Reply> {
        blocking(move || uia::list_elements(window)).await
    }

    async fn find_elements(
        &self,
        window: WindowId,
        query: ElementQuery,
        wait_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        blocking(move || uia::find_elements(window, &query, wait_ms)).await
    }

    async fn click(&self, target: ClickTarget) -> RemoteResult<Reply> {
        blocking(move || click(target)).await
    }

    async fn type_text(
        &self,
        text: String,
        into: Option<ElementId>,
        paste: bool,
    ) -> RemoteResult<Reply> {
        blocking(move || {
            if paste {
                // Clipboard paste: set the clipboard, focus the target, Ctrl+V.
                // Far faster/more reliable than per-key injection for long text.
                clipboard::set(&text)?;
                if let Some(element) = into {
                    uia::focus(&element.0)?;
                }
                input::key_chord(&[Modifier::Ctrl], Key::Char('v'))
            } else {
                // Focus the target element first (more reliable than typing into
                // whatever happens to have focus), then send real keys.
                if let Some(element) = into {
                    uia::focus(&element.0)?;
                }
                input::type_text(&text)
            }
        })
        .await
    }

    async fn key_chord(&self, modifiers: Vec<Modifier>, key: Key) -> RemoteResult<Reply> {
        blocking(move || input::key_chord(&modifiers, key)).await
    }

    async fn mouse(&self, action: MouseAction) -> RemoteResult<Reply> {
        blocking(move || input::mouse(action)).await
    }

    async fn set_value(&self, element: ElementId, value: String) -> RemoteResult<Reply> {
        blocking(move || uia::set_value(&element.0, &value)).await
    }

    async fn read_element(&self, element: ElementId) -> RemoteResult<Reply> {
        blocking(move || uia::read_element(&element.0).map(Reply::Text)).await
    }

    async fn focus_element(&self, element: ElementId) -> RemoteResult<Reply> {
        blocking(move || uia::focus(&element.0).map(|()| Reply::Ack)).await
    }

    async fn clipboard_get(&self) -> RemoteResult<Reply> {
        blocking(|| clipboard::get().map(Reply::Text)).await
    }

    async fn clipboard_set(&self, text: String) -> RemoteResult<Reply> {
        blocking(move || clipboard::set(&text).map(|()| Reply::Ack)).await
    }

    async fn list_processes(&self, filter: Option<String>, with_cpu: bool) -> RemoteResult<Reply> {
        // Emit tab-delimited `pid<TAB>name<TAB>kb[<TAB>cpu]` lines (process names
        // can't contain a tab), parsed below. `with_cpu` samples
        // TotalProcessorTime twice ~500ms apart for an instantaneous, per-core
        // CPU% (a spinning process reads high, a blocked one ~0). Sorting is done
        // in Rust after parsing.
        let filter = ps_where(filter.as_deref());
        let command = if with_cpu {
            format!(
                "$n=[Environment]::ProcessorCount; $a=@{{}}; \
                 Get-Process{filter} | ForEach-Object {{ $a[$_.Id]=$_.TotalProcessorTime.TotalMilliseconds }}; \
                 Start-Sleep -Milliseconds 500; \
                 Get-Process{filter} | ForEach-Object {{ \
                   $p=$a[$_.Id]; \
                   $c=if ($null -ne $p -and $_.TotalProcessorTime) \
                     {{ [math]::Round((($_.TotalProcessorTime.TotalMilliseconds-$p)/500/$n)*100,1) }} else {{ 0 }}; \
                   \"$($_.Id)`t$($_.ProcessName)`t$([int]($_.WS/1KB))`t$c\" }}"
            )
        } else {
            format!(
                "Get-Process{filter} | ForEach-Object {{ \
                   \"$($_.Id)`t$($_.ProcessName)`t$([int]($_.WS/1KB))\" }}"
            )
        };
        let stdout = run_ps(command).await?;
        let mut procs = parse_processes(&stdout);
        sort_processes(&mut procs, with_cpu);
        Ok(Reply::Processes(procs))
    }

    async fn kill_process(&self, target: String, dry_run: bool) -> RemoteResult<Reply> {
        // Select by PID (all digits) or by exact name (with/without `.exe`).
        // SilentlyContinue so "no match" is an empty list, not an error.
        let selector = if target.chars().all(|c| c.is_ascii_digit()) {
            format!("Get-Process -Id {target} -ErrorAction SilentlyContinue")
        } else {
            let name = target
                .strip_suffix(".exe")
                .unwrap_or(&target)
                .replace('\'', "''");
            format!("Get-Process -Name '{name}' -ErrorAction SilentlyContinue")
        };
        let pipe = if dry_run {
            ""
        } else {
            " | Stop-Process -Force -PassThru"
        };
        let command =
            format!("{selector}{pipe} | ForEach-Object {{ \"$($_.Id)`t$($_.ProcessName)\" }}");
        let stdout = run_ps(command).await?;
        Ok(Reply::Processes(parse_processes(&stdout)))
    }

    async fn identity(&self) -> RemoteResult<Reply> {
        let command = "\
            $id=[Security.Principal.WindowsIdentity]::GetCurrent(); \
            $admin=([Security.Principal.WindowsPrincipal]$id).IsInRole(\
            [Security.Principal.WindowsBuiltinRole]::Administrator); \
            $il=(whoami /groups /fo csv | ConvertFrom-Csv | \
            Where-Object { $_.SID -like 'S-1-16-*' }).'Group Name'; \
            Write-Output \"account`t$($id.Name)\"; \
            Write-Output \"admin`t$admin\"; \
            Write-Output \"integrity`t$il\"; \
            Write-Output \"session`t$((Get-Process -Id $PID).SessionId)\""
            .to_owned();
        let stdout = run_ps(command).await?;
        let lines = stdout
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once('\t')?;
                Some((k.trim().to_owned(), v.trim().to_owned()))
            })
            .collect();
        Ok(Reply::Identity(lines))
    }
}

/// A `Get-Process` name filter (`| Where-Object …`), or empty for no filter.
fn ps_where(filter: Option<&str>) -> String {
    match filter {
        Some(p) => format!(
            " | Where-Object {{ $_.ProcessName -like '*{}*' }}",
            p.replace('\'', "''")
        ),
        None => String::new(),
    }
}

/// Runs a PowerShell one-liner and returns its stdout.
async fn run_ps(command: String) -> RemoteResult<String> {
    match exec::run_command(Shell::PowerShell, &command, &[], Some(30_000)).await? {
        Reply::CommandOutput { stdout, .. } => Ok(stdout),
        other => Err(os_error(format!("unexpected reply: {other:?}"))),
    }
}

/// Parses tab-delimited `pid<TAB>name<TAB>kb[<TAB>cpu]` lines into processes.
fn parse_processes(stdout: &str) -> Vec<ProcessInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let pid = f.next()?.trim().parse::<u32>().ok()?;
            let name = f.next()?.trim().to_owned();
            let memory_kb = f.next().and_then(|s| s.trim().parse::<u64>().ok());
            let cpu_percent = f.next().and_then(|s| s.trim().parse::<f32>().ok());
            Some(ProcessInfo {
                pid,
                name,
                memory_kb,
                cpu_percent,
            })
        })
        .collect()
}

/// Sorts processes descending by CPU% (if sampled) or memory.
fn sort_processes(procs: &mut [ProcessInfo], with_cpu: bool) {
    if with_cpu {
        procs.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        procs.sort_by_key(|p| std::cmp::Reverse(p.memory_kb));
    }
}

/// Invoke a UI element or click a raw point.
fn click(target: ClickTarget) -> RemoteResult<Reply> {
    match target {
        ClickTarget::Element(element) => uia::click_element(&element.0),
        ClickTarget::Point { x, y, button } => input::click_point(x, y, button),
    }
}

/// Runs a blocking capability handler on the blocking thread pool.
async fn blocking<F>(f: F) -> RemoteResult<Reply>
where
    F: FnOnce() -> RemoteResult<Reply> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(os_error(format!("worker task failed: {e}"))),
    }
}
