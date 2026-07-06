//! The Android [`Backend`]: overrides the capabilities this runner implements
//! (shell, screenshot, input, UI tree) and delegates them to [`crate::cap`].
//! Everything not overridden (proc_dump, clipboard, set_value, …) inherits the
//! trait's "unsupported" default. File transfer is handled by the shared
//! dispatcher, so it works with no code here.

use arc_proto::id::{ElementId, WindowId};
use arc_proto::wire::Reply;
use arc_proto::wire::{
    CaptureTarget, ClickTarget, ElementQuery, ImageFormat, Key, Modifier, Shell,
};
use arc_runner_core::{Backend, RemoteResult};

use crate::cap;

pub struct AndroidBackend;

impl Backend for AndroidBackend {
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
}
