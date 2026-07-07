//! The Android [`Backend`]: overrides the capabilities this runner implements
//! (shell, screenshot, input, UI tree) and delegates them to [`crate::cap`].
//! Everything not overridden (proc_dump, clipboard, set_value, …) inherits the
//! trait's "unsupported" default. File transfer is handled by the shared
//! dispatcher, so it works with no code here.

use arc_proto::id::{ElementId, WindowId};
use arc_proto::wire::Reply;
use arc_proto::wire::{
    Capability, CaptureTarget, ClickTarget, ElementQuery, ImageFormat, Key, Modifier, MouseAction,
    Shell,
};
use arc_runner_core::{Backend, RemoteResult};

use crate::cap;

pub struct AndroidBackend;

impl Backend for AndroidBackend {
    fn capabilities(&self) -> Vec<Capability> {
        // Exactly the methods overridden below — shell/UI-tree/input/capture via
        // screencap/input/uiautomator. No proc_dump, clipboard, activate_window,
        // set_value, read_element, focus_element, or detached jobs yet.
        vec![
            Capability::RunCommand,
            Capability::RunScript,
            Capability::Screenshot,
            Capability::OpenApp,
            Capability::ListWindows,
            Capability::ListElements,
            Capability::FindElements,
            Capability::Click,
            Capability::TypeText,
            Capability::KeyChord,
            Capability::Mouse,
            Capability::ListProcesses,
            Capability::KillProcess,
            Capability::Identity,
            Capability::ActivateWindow,
            Capability::ReadElement,
            Capability::SetValue,
            Capability::FocusElement,
        ]
    }

    async fn run_command(
        &self,
        _shell: Shell,
        command: String,
        _env: Vec<(String, String)>,
        _timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        cap::run_command(&command).await
    }

    async fn run_script(
        &self,
        _id: arc_proto::id::RequestId,
        _shell: Shell,
        content: String,
        _args: Vec<String>,
        _env: Vec<(String, String)>,
        _timeout_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        cap::run_command(&content).await
    }

    async fn screenshot(
        &self,
        target: CaptureTarget,
        _format: Option<ImageFormat>,
        _settle_ms: Option<u64>,
        _settle_await_change: bool,
    ) -> RemoteResult<Reply> {
        cap::screenshot(target).await
    }

    async fn open_app(
        &self,
        target: String,
        args: Vec<String>,
        _watch_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        cap::open_app(&target, &args).await
    }

    async fn mouse(&self, action: MouseAction) -> RemoteResult<Reply> {
        cap::mouse(action).await
    }

    async fn list_windows(&self) -> RemoteResult<Reply> {
        cap::list_windows().await
    }

    async fn list_elements(&self, _window: WindowId) -> RemoteResult<Reply> {
        cap::list_elements().await
    }

    async fn find_elements(
        &self,
        _window: WindowId,
        query: ElementQuery,
        _wait_ms: Option<u64>,
    ) -> RemoteResult<Reply> {
        cap::find_elements(&query).await
    }

    async fn click(&self, target: ClickTarget) -> RemoteResult<Reply> {
        cap::click(target).await
    }

    async fn type_text(
        &self,
        text: String,
        _into: Option<ElementId>,
        _paste: bool,
    ) -> RemoteResult<Reply> {
        cap::type_text(&text).await
    }

    async fn key_chord(&self, modifiers: Vec<Modifier>, key: Key) -> RemoteResult<Reply> {
        cap::key_chord(&modifiers, key).await
    }

    async fn list_processes(&self, filter: Option<String>, with_cpu: bool) -> RemoteResult<Reply> {
        cap::list_processes(filter.as_deref(), with_cpu).await
    }

    async fn kill_process(&self, target: String, dry_run: bool) -> RemoteResult<Reply> {
        cap::kill_process(&target, dry_run).await
    }

    async fn identity(&self) -> RemoteResult<Reply> {
        cap::identity().await
    }

    async fn activate_window(&self, window: WindowId) -> RemoteResult<Reply> {
        cap::activate_window(window).await
    }

    async fn read_element(&self, element: ElementId) -> RemoteResult<Reply> {
        cap::read_element(&element).await
    }

    async fn set_value(&self, element: ElementId, value: String) -> RemoteResult<Reply> {
        cap::set_value(&element, &value).await
    }

    async fn focus_element(&self, element: ElementId) -> RemoteResult<Reply> {
        cap::focus_element(&element).await
    }
}
