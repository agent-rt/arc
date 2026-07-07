//! The [`Backend`] capability trait: what a runner can do on its OS, one method
//! per semantic operation. Every method has a default that returns an
//! "unsupported" error, so a backend only overrides what it actually implements
//! (a locked-down Android backend can skip `proc_dump`/`clipboard`; the "not
//! implemented" reply is the default, not a per-backend match arm). File
//! transfer is not here — it is OS-agnostic and lives in [`crate::files`].

use arc_proto::id::{ElementId, RequestId, WindowId};
use arc_proto::wire::{
    Capability, CaptureTarget, ClickTarget, ElementQuery, ImageFormat, Key, Modifier, MouseAction,
    Reply, Shell,
};

use crate::{RemoteResult, invalid};

fn unsupported(op: &str) -> arc_proto::wire::RemoteError {
    invalid(format!("`{op}` is not supported by this runner"))
}

/// A runner's OS capability surface. Implemented per platform (Windows via
/// `windows-rs`, Android by shelling out to `screencap`/`input`/`uiautomator`).
/// All methods are non-streaming request→reply; streaming and port forwarding
/// are handled by the platform's serve loop, not here.
///
/// `async fn` in a trait is fine here: the trait is consumed by a generic
/// `dispatch<B: Backend>` against a concrete backend (static dispatch), so the
/// auto-trait-bound limitation the lint warns about does not apply.
#[allow(clippy::too_many_arguments, unused_variables, async_fn_in_trait)]
pub trait Backend {
    /// The OS-semantic commands this backend implements — i.e. every method it
    /// overrides below. Required (no default) so each platform declares its
    /// surface explicitly and honestly rather than silently inheriting a guess;
    /// the shared dispatcher merges in the always-available file-transfer and
    /// tunnel capabilities. Surfaced to the controller via
    /// [`arc_proto::wire::Command::Capabilities`].
    fn capabilities(&self) -> Vec<Capability>;

    /// Run a command, buffered (streaming is a serve-loop concern).
    async fn run_command(
        &self,
        shell: Shell,
        command: String,
        env: Vec<(String, String)>,
        timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        Err(unsupported("run_command"))
    }

    /// Run a script by content, buffered.
    async fn run_script(
        &self,
        id: RequestId,
        shell: Shell,
        content: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        Err(unsupported("run_script"))
    }

    /// Launch a script detached, returning immediately with a pid + log path.
    async fn run_detached(
        &self,
        id: RequestId,
        shell: Shell,
        content: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> RemoteResult<Reply> {
        Err(unsupported("run_detached"))
    }

    async fn screenshot(
        &self,
        target: CaptureTarget,
        format: Option<ImageFormat>,
        settle_ms: Option<u64>,
        settle_await_change: bool,
    ) -> RemoteResult<Reply> {
        Err(unsupported("screenshot"))
    }

    async fn open_app(
        &self,
        target: String,
        args: Vec<String>,
        watch_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        Err(unsupported("open_app"))
    }

    async fn proc_dump(&self, pid: u32) -> RemoteResult<Reply> {
        Err(unsupported("procdump"))
    }

    async fn list_windows(&self) -> RemoteResult<Reply> {
        Err(unsupported("list_windows"))
    }

    async fn activate_window(&self, window: WindowId) -> RemoteResult<Reply> {
        Err(unsupported("activate_window"))
    }

    async fn list_elements(&self, window: WindowId) -> RemoteResult<Reply> {
        Err(unsupported("list_elements"))
    }

    async fn find_elements(
        &self,
        window: WindowId,
        query: ElementQuery,
        wait_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        Err(unsupported("find_elements"))
    }

    async fn click(&self, target: ClickTarget) -> RemoteResult<Reply> {
        Err(unsupported("click"))
    }

    async fn type_text(
        &self,
        text: String,
        into: Option<ElementId>,
        paste: bool,
    ) -> RemoteResult<Reply> {
        Err(unsupported("type_text"))
    }

    async fn key_chord(&self, modifiers: Vec<Modifier>, key: Key) -> RemoteResult<Reply> {
        Err(unsupported("key_chord"))
    }

    async fn mouse(&self, action: MouseAction) -> RemoteResult<Reply> {
        Err(unsupported("mouse"))
    }

    async fn set_value(&self, element: ElementId, value: String) -> RemoteResult<Reply> {
        Err(unsupported("set_value"))
    }

    async fn read_element(&self, element: ElementId) -> RemoteResult<Reply> {
        Err(unsupported("read_element"))
    }

    async fn focus_element(&self, element: ElementId) -> RemoteResult<Reply> {
        Err(unsupported("focus_element"))
    }

    /// List processes (semantic; the backend uses its OS's enumeration).
    async fn list_processes(&self, filter: Option<String>, with_cpu: bool) -> RemoteResult<Reply> {
        Err(unsupported("list_processes"))
    }

    /// Kill process(es) by pid or name (or, with `dry_run`, just list matches).
    async fn kill_process(&self, target: String, dry_run: bool) -> RemoteResult<Reply> {
        Err(unsupported("kill_process"))
    }

    /// Report the runner's identity as ordered `(label, value)` lines.
    async fn identity(&self) -> RemoteResult<Reply> {
        Err(unsupported("identity"))
    }

    async fn clipboard_get(&self) -> RemoteResult<Reply> {
        Err(unsupported("clipboard_get"))
    }

    async fn clipboard_set(&self, text: String) -> RemoteResult<Reply> {
        Err(unsupported("clipboard_set"))
    }
}
