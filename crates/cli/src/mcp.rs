//! The MCP server: exposes the runner's capabilities as MCP tools and manages
//! the (lazily established, auto-reconnecting) controller link underneath.

use std::sync::Arc;
use std::time::Duration;

use arc_proto::id::{ElementId, WindowId};
use arc_proto::wire::{
    CaptureTarget, ClickTarget, Command, ElementInfo, ElementQuery, Event, ImageFormat,
    MouseAction, MouseButton, Reply, Shell, parse_chord,
};
use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, NumberOrString, ProgressNotificationParam,
    ProgressToken, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router,
};
use tokio::sync::{Mutex, mpsc};

use arc_net::{Controller, ControllerError, SessionConfig};

/// How long to wait for the runner to appear and the handshake to complete.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// MCP server handle. `Clone` is cheap (everything is shared) as required by
/// the rmcp service model.
#[derive(Clone)]
pub struct AgentRc {
    /// The live link, or `None` until first use / after a fatal error.
    link: Arc<Mutex<Option<Controller>>>,
    config: Arc<SessionConfig>,
    // Read at runtime by the `#[tool_handler]`-generated `call_tool`/`list_tools`,
    // which dead-code analysis cannot see through the macro expansion.
    #[allow(dead_code)]
    tool_router: ToolRouter<AgentRc>,
}

/// Arguments for [`AgentRc::run_command`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunCommandArgs {
    /// Command line to execute on the remote Windows machine.
    pub command: String,
    /// Shell to use: `"powershell"` (default) or `"cmd"`.
    #[serde(default)]
    pub shell: Option<String>,
    /// Wall-clock timeout in milliseconds. Omitted = a default safety limit
    /// (10 min); `0` = no limit (for long builds / long-running processes).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Arguments for [`AgentRc::run_script`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunScriptArgs {
    /// Script source to run on the remote Windows machine. Sent as data (not
    /// through a shell), so there is no quoting to escape.
    pub content: String,
    /// Interpreter: `"powershell"` (default, runs with ExecutionPolicy Bypass)
    /// or `"cmd"`.
    #[serde(default)]
    pub shell: Option<String>,
    /// Arguments passed through to the script.
    #[serde(default)]
    pub args: Vec<String>,
    /// Wall-clock timeout in milliseconds. Omitted = a default safety limit
    /// (10 min); `0` = no limit.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Arguments for [`AgentRc::screenshot`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ScreenshotArgs {
    /// Optional window handle (from `list_windows`) to capture just that window;
    /// omit for the full screen. A single window captures correctly (via
    /// Windows.Graphics.Capture) even in a detached/disconnected-RDP session,
    /// where a full-screen capture may not.
    #[serde(default)]
    pub window: Option<u64>,
    /// Optional element id (from `list_elements`/`find_elements`) to capture just
    /// that control's bounding box. Takes precedence over `window`.
    #[serde(default)]
    pub element: Option<String>,
}

/// Arguments for [`AgentRc::list_elements`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListElementsArgs {
    /// Native window handle (the `id` from `list_windows`).
    pub window: u64,
}

/// Arguments for [`AgentRc::find_elements`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindElementsArgs {
    /// Native window handle (the `id` from `list_windows`).
    pub window: u64,
    /// Match the accessible name.
    #[serde(default)]
    pub name: Option<String>,
    /// Match `name` as a substring rather than the whole name.
    #[serde(default)]
    pub name_contains: bool,
    /// Match the UIA AutomationId.
    #[serde(default)]
    pub automation_id: Option<String>,
    /// Match the control type (e.g. `"Button"`, `"Edit"`).
    #[serde(default)]
    pub control_type: Option<String>,
    /// Only enabled, on-screen elements.
    #[serde(default)]
    pub actionable_only: bool,
    /// If set, poll until ≥1 element matches or this many ms elapse (errors on
    /// timeout). Omitted = return current matches at once (possibly none).
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// Arguments for [`AgentRc::click_element`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ClickElementArgs {
    /// Element id from `list_elements` (`"<hwnd>:<index>"`).
    pub element_id: String,
}

/// Arguments for [`AgentRc::click_point`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ClickPointArgs {
    /// X in absolute screen coordinates.
    pub x: i32,
    /// Y in absolute screen coordinates.
    pub y: i32,
    /// `"left"` (default), `"right"`, or `"middle"`.
    #[serde(default)]
    pub button: Option<String>,
}

/// Arguments for [`AgentRc::type_text`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TypeTextArgs {
    /// Unicode text to type into the focused element.
    pub text: String,
}

/// Arguments for [`AgentRc::press_key`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PressKeyArgs {
    /// One or more chords, pressed in order — e.g. `["ctrl+a","delete"]`, or a
    /// single `["enter"]`. Each chord: `"enter"`, `"esc"`, `"f5"`, `"ctrl+c"`,
    /// `"ctrl+shift+esc"`, `"alt+f4"`. Modifiers (`ctrl`/`alt`/`shift`/`win`)
    /// join the key with `+`. For ordinary text, prefer `type_text`.
    pub keys: Vec<String>,
}

/// Arguments for [`AgentRc::mouse`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MouseArgs {
    /// One of `"move"`, `"click"`, `"down"`, `"up"`, `"scroll"`, `"drag"`.
    pub action: String,
    /// Primary coordinate / scroll dx (for `scroll`) / drag start x.
    #[serde(default)]
    pub x: i32,
    /// Primary coordinate / scroll dy (for `scroll`) / drag start y.
    #[serde(default)]
    pub y: i32,
    /// Drag end x (for `drag`).
    #[serde(default)]
    pub to_x: i32,
    /// Drag end y (for `drag`).
    #[serde(default)]
    pub to_y: i32,
    /// `"left"` (default), `"right"`, or `"middle"`.
    #[serde(default)]
    pub button: Option<String>,
    /// Click count for `"click"` (2 = double-click); defaults to 1.
    #[serde(default)]
    pub count: Option<u32>,
}

/// Arguments for [`AgentRc::set_value`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SetValueArgs {
    /// Element id from `list_elements`.
    pub element_id: String,
    /// New value to set.
    pub value: String,
}

/// Arguments for [`AgentRc::open_app`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct OpenAppArgs {
    /// Executable path or registered name (e.g. `"notepad"`).
    pub target: String,
    /// Command-line arguments.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Arguments for [`AgentRc::read_file`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileArgs {
    /// Absolute path on the remote machine.
    pub path: String,
    /// Return standard-base64 instead of UTF-8 text (for binary files).
    #[serde(default)]
    pub base64: Option<bool>,
    /// Byte offset to start reading from (for chunked transfer of large files).
    #[serde(default)]
    pub offset: Option<u64>,
    /// Maximum bytes to read; omit/`0` reads to end of file. A short read
    /// (fewer bytes than requested) signals EOF.
    #[serde(default)]
    pub length: Option<u64>,
}

/// Arguments for [`AgentRc::write_file`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileArgs {
    /// Absolute path on the remote machine; parent directories are created.
    pub path: String,
    /// UTF-8 text content (use for source code and text files).
    #[serde(default)]
    pub content: Option<String>,
    /// Standard-base64 content (use for binary files). Takes precedence over
    /// `content`.
    #[serde(default)]
    pub content_base64: Option<String>,
    /// Byte offset to write at; `0` (default) creates/truncates, `> 0` writes
    /// at that offset — send a large file as successive chunks.
    #[serde(default)]
    pub offset: Option<u64>,
}

#[tool_router]
impl AgentRc {
    /// Builds the server from session configuration.
    #[must_use]
    pub fn new(config: SessionConfig) -> Self {
        Self {
            link: Arc::new(Mutex::new(None)),
            config: Arc::new(config),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Run a shell command on the remote Windows machine. Output streams back live — as MCP progress notifications when the client supplies a progressToken — and the final result carries the full stdout/stderr and exit code. Use this to build, run and inspect Windows apps."
    )]
    async fn run_command(
        &self,
        Parameters(args): Parameters<RunCommandArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let shell = parse_shell(args.shell.as_deref());
        let command = Command::RunCommand {
            shell,
            command: args.command,
            timeout_ms: clamp_timeout(args.timeout_ms),
            stream: true,
        };
        self.stream_to_result(command, context).await
    }

    #[tool(
        description = "Run a script by its source on the remote Windows machine. The script is sent as data (no shell quoting to escape, no pre-upload) and run with the chosen interpreter — PowerShell (default, ExecutionPolicy Bypass) or cmd — with `args` passed through. Output streams back live like run_command. Prefer this over run_command for any multi-line or quote-heavy script."
    )]
    async fn run_script(
        &self,
        Parameters(args): Parameters<RunScriptArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let command = Command::RunScript {
            shell: parse_shell(args.shell.as_deref()),
            content: args.content,
            args: args.args,
            timeout_ms: clamp_timeout(args.timeout_ms),
            stream: true,
        };
        self.stream_to_result(command, context).await
    }

    /// Runs a streaming `command`, relaying chunks as MCP progress (when the
    /// client supplied a `progressToken`) and returning the full output. Shared
    /// by [`run_command`](Self::run_command) and [`run_script`](Self::run_script).
    async fn stream_to_result(
        &self,
        command: Command,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let token = context
            .meta
            .get_key_value("progressToken")
            .and_then(|(_, value)| serde_json::from_value::<NumberOrString>(value.clone()).ok());
        let peer = context.peer.clone();

        // The collector reassembles full output from streamed events and, when a
        // progress token is present, relays each chunk as an MCP progress note.
        let (tx, mut rx) = mpsc::channel::<Event>(128);
        let collector = tokio::spawn(async move {
            let (mut stdout, mut stderr) = (String::new(), String::new());
            let mut step = 0.0_f64;
            while let Some(event) = rx.recv().await {
                let chunk = match &event {
                    Event::Stdout { chunk, .. } => {
                        stdout.push_str(chunk);
                        chunk.clone()
                    }
                    Event::Stderr { chunk, .. } => {
                        stderr.push_str(chunk);
                        chunk.clone()
                    }
                    Event::Progress { message, .. } => message.clone(),
                };
                if let Some(token) = &token {
                    step += 1.0;
                    let param = ProgressNotificationParam::new(ProgressToken(token.clone()), step)
                        .with_message(chunk);
                    let _ = peer.notify_progress(param).await;
                }
            }
            (stdout, stderr)
        });

        let reply = self.dispatch_streaming(command, tx).await;

        // Always join the collector (its channel closed when `tx` dropped above).
        let (streamed_out, streamed_err) = collector.await.unwrap_or_default();

        match reply? {
            Reply::CommandOutput {
                stdout,
                stderr,
                exit_code,
            } => {
                let out = if stdout.is_empty() {
                    streamed_out
                } else {
                    stdout
                };
                let err = if stderr.is_empty() {
                    streamed_err
                } else {
                    stderr
                };
                let code = exit_code.map_or_else(|| "killed".to_owned(), |c| c.to_string());
                let text =
                    format!("exit_code: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Capture a screenshot of the remote Windows desktop — or of a single window if `window` is given — returned as an image the Agent can view. Capturing a specific window also works in a detached/disconnected session."
    )]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = if let Some(id) = args.element {
            CaptureTarget::Element(ElementId(id))
        } else if let Some(handle) = args.window {
            CaptureTarget::Window(WindowId(handle))
        } else {
            CaptureTarget::FullScreen
        };
        let reply = self
            .dispatch(Command::Screenshot {
                target,
                format: None,
            })
            .await?;
        match reply {
            Reply::Image(img) => {
                let mime = match img.format {
                    ImageFormat::Png => "image/png",
                    ImageFormat::Webp => "image/webp",
                };
                let encoded = base64::engine::general_purpose::STANDARD.encode(&img.data);
                Ok(CallToolResult::success(vec![Content::image(encoded, mime)]))
            }
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "List top-level windows on the remote desktop. Each line is `handle | process | title`; use the handle with list_elements."
    )]
    async fn list_windows(&self) -> Result<CallToolResult, McpError> {
        match self.dispatch(Command::ListWindows).await? {
            Reply::Windows(windows) => {
                let text = windows
                    .iter()
                    .map(|w| format!("{} | {} | {}", w.id.0, w.process, w.title))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(CallToolResult::success(vec![Content::text(blank_as(
                    text,
                    "<no windows>",
                ))]))
            }
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "List UI Automation elements inside a window (handle from list_windows). Each line is `element_id | control_type | actionable? | name`."
    )]
    async fn list_elements(
        &self,
        Parameters(args): Parameters<ListElementsArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .dispatch(Command::ListElements {
                window: WindowId(args.window),
            })
            .await?
        {
            Reply::Elements(elements) => Ok(CallToolResult::success(vec![Content::text(
                blank_as(format_elements(&elements), "<no elements>"),
            )])),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Find UI elements in a window by attribute — name, automation_id, control_type, or actionable — without dumping the whole tree. With wait_ms set, polls until at least one matches or the timeout elapses (errors on timeout), so you can wait for a control to appear. Returns matching rows `element_id | control_type | actionable? | automation_id | name`."
    )]
    async fn find_elements(
        &self,
        Parameters(args): Parameters<FindElementsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let query = ElementQuery {
            name: args.name,
            name_contains: args.name_contains,
            automation_id: args.automation_id,
            control_type: args.control_type,
            actionable_only: args.actionable_only,
        };
        match self
            .dispatch(Command::FindElements {
                window: WindowId(args.window),
                query,
                wait_ms: args.wait_ms,
            })
            .await?
        {
            Reply::Elements(elements) => Ok(CallToolResult::success(vec![Content::text(
                blank_as(format_elements(&elements), "<no match>"),
            )])),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(description = "Click a UI element by its element_id (from list_elements).")]
    async fn click_element(
        &self,
        Parameters(args): Parameters<ClickElementArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.ack(Command::Click {
            target: ClickTarget::Element(ElementId(args.element_id)),
        })
        .await
    }

    #[tool(description = "Click at an absolute screen coordinate (fallback when no element fits).")]
    async fn click_point(
        &self,
        Parameters(args): Parameters<ClickPointArgs>,
    ) -> Result<CallToolResult, McpError> {
        let button = match args.button.as_deref() {
            Some("right") => MouseButton::Right,
            Some("middle") => MouseButton::Middle,
            _ => MouseButton::Left,
        };
        self.ack(Command::Click {
            target: ClickTarget::Point {
                x: args.x,
                y: args.y,
                button,
            },
        })
        .await
    }

    #[tool(description = "Type Unicode text into the focused element on the remote machine.")]
    async fn type_text(
        &self,
        Parameters(args): Parameters<TypeTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.ack(Command::TypeText { text: args.text }).await
    }

    #[tool(
        description = "Press a key/chord — or a sequence of them — that type_text cannot express: Enter, Tab, Esc, arrows, F-keys, and combinations like ctrl+c, ctrl+s, alt+f4 (modifiers ctrl/alt/shift/win join the key with +). Pass keys as an ordered array, e.g. [\"ctrl+a\",\"delete\"]. For ordinary text, use type_text."
    )]
    async fn press_key(
        &self,
        Parameters(args): Parameters<PressKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.keys.is_empty() {
            return Err(McpError::invalid_params(
                "keys must not be empty".to_owned(),
                None,
            ));
        }
        let last = args.keys.len() - 1;
        for (i, chord) in args.keys.iter().enumerate() {
            let (modifiers, key) = parse_chord(chord).map_err(|e| {
                McpError::invalid_params(format!("invalid key chord '{chord}': {e}"), None)
            })?;
            match self.dispatch(Command::KeyChord { modifiers, key }).await? {
                Reply::Ack => {}
                other => return Err(unexpected(&other)),
            }
            if i < last {
                tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            }
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(
        description = "Inject a mouse action at absolute screen coordinates. action: move | click | down | up | scroll | drag. For click, count=2 double-clicks. For scroll, x/y are dx/dy notches (positive = right/down). For drag, x/y are the start and to_x/to_y the end. button: left (default)/right/middle."
    )]
    async fn mouse(
        &self,
        Parameters(args): Parameters<MouseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let button = match args.button.as_deref() {
            Some("right") => MouseButton::Right,
            Some("middle") => MouseButton::Middle,
            _ => MouseButton::Left,
        };
        let action = match args.action.as_str() {
            "move" => MouseAction::Move {
                x: args.x,
                y: args.y,
            },
            "click" => MouseAction::Click {
                x: args.x,
                y: args.y,
                button,
                count: args.count.unwrap_or(1),
            },
            "down" => MouseAction::Down {
                x: args.x,
                y: args.y,
                button,
            },
            "up" => MouseAction::Up {
                x: args.x,
                y: args.y,
                button,
            },
            "scroll" => MouseAction::Scroll {
                dx: args.x,
                dy: args.y,
            },
            "drag" => MouseAction::Drag {
                from_x: args.x,
                from_y: args.y,
                to_x: args.to_x,
                to_y: args.to_y,
                button,
            },
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown mouse action: {other}"),
                    None,
                ));
            }
        };
        self.ack(Command::Mouse { action }).await
    }

    #[tool(
        description = "Set a UI element's value directly via its element_id (preferred over typing into it)."
    )]
    async fn set_value(
        &self,
        Parameters(args): Parameters<SetValueArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.ack(Command::SetValue {
            element: ElementId(args.element_id),
            value: args.value,
        })
        .await
    }

    #[tool(description = "Launch an application on the remote machine by path or name.")]
    async fn open_app(
        &self,
        Parameters(args): Parameters<OpenAppArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .dispatch(Command::OpenApp {
                target: args.target,
                args: args.args,
            })
            .await?
        {
            Reply::AppOpened { window, pid } => Ok(CallToolResult::success(vec![Content::text(
                format!("launched pid={pid} window={window:?}"),
            )])),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Read a file from the remote machine. Returns UTF-8 text (lossy); set base64=true for binary files."
    )]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .dispatch(Command::ReadFile {
                path: args.path,
                offset: args.offset.unwrap_or(0),
                max_len: args.length.unwrap_or(0),
            })
            .await?
        {
            Reply::FileContents(bytes) => {
                let body = if args.base64.unwrap_or(false) {
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                } else {
                    String::from_utf8_lossy(&bytes).into_owned()
                };
                Ok(CallToolResult::success(vec![Content::text(body)]))
            }
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Write a file to the remote machine, creating parent directories. Provide `content` (UTF-8 text) or `content_base64` (binary). Use to push source code before building."
    )]
    async fn write_file(
        &self,
        Parameters(args): Parameters<WriteFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let contents = match (args.content, args.content_base64) {
            (_, Some(encoded)) => base64::engine::general_purpose::STANDARD
                .decode(encoded.as_bytes())
                .map_err(|e| McpError::invalid_params(format!("invalid base64: {e}"), None))?,
            (Some(text), None) => text.into_bytes(),
            (None, None) => {
                return Err(McpError::invalid_params(
                    "provide `content` or `content_base64`",
                    None,
                ));
            }
        };
        self.ack(Command::WriteFile {
            path: args.path,
            contents,
            offset: args.offset.unwrap_or(0),
        })
        .await
    }
}

impl AgentRc {
    /// Dispatches a command expected to return [`Reply::Ack`], surfacing a
    /// plain `"ok"` to the Agent.
    async fn ack(&self, command: Command) -> Result<CallToolResult, McpError> {
        match self.dispatch(command).await? {
            Reply::Ack => Ok(CallToolResult::success(vec![Content::text("ok")])),
            other => Err(unexpected(&other)),
        }
    }

    /// Ensures a live link, sends the command, and reconnects next time on a
    /// fatal transport error.
    async fn dispatch(&self, command: Command) -> Result<Reply, McpError> {
        let mut guard = self.link.lock().await;
        connect_if_needed(&self.config, &mut guard).await?;
        let result = {
            let Some(controller) = guard.as_mut() else {
                return Err(McpError::internal_error("link unexpectedly absent", None));
            };
            controller.request(command).await
        };
        finish(&mut guard, result)
    }

    /// Like [`dispatch`](Self::dispatch) but forwards interim events to
    /// `events` (consumed by the caller's collector) for live streaming.
    async fn dispatch_streaming(
        &self,
        command: Command,
        events: mpsc::Sender<Event>,
    ) -> Result<Reply, McpError> {
        let mut guard = self.link.lock().await;
        connect_if_needed(&self.config, &mut guard).await?;
        let result = {
            let Some(controller) = guard.as_mut() else {
                return Err(McpError::internal_error("link unexpectedly absent", None));
            };
            controller.request_streaming(command, &events).await
        };
        finish(&mut guard, result)
    }
}

/// Establishes the link if absent (bounded by [`CONNECT_TIMEOUT`]).
async fn connect_if_needed(
    config: &SessionConfig,
    guard: &mut Option<Controller>,
) -> Result<(), McpError> {
    if guard.is_none() {
        let controller = tokio::time::timeout(CONNECT_TIMEOUT, Controller::connect(config))
            .await
            .map_err(|_| {
                McpError::internal_error("timed out connecting to runner via relay", None)
            })?
            .map_err(map_err)?;
        *guard = Some(controller);
    }
    Ok(())
}

/// Maps a controller result to MCP, dropping the link on a fatal error so the
/// next call reconnects.
fn finish(
    guard: &mut Option<Controller>,
    result: Result<Reply, ControllerError>,
) -> Result<Reply, McpError> {
    match result {
        Ok(reply) => Ok(reply),
        Err(error) => {
            if error.is_fatal() {
                *guard = None;
            }
            Err(map_err(error))
        }
    }
}

#[tool_handler]
impl ServerHandler for AgentRc {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Remote-control a Windows machine for an Agent. Tools: run_command (shell), \
                 run_script (run a script by source), find_elements (locate/await a \
                 control by attribute), screenshot (view the desktop). \
                 Requires a paired arc-runner connected to the same relay session.",
            )
    }
}

fn map_err(error: ControllerError) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

/// Renders elements as one `id | control_type | actionable? | automation_id | name`
/// row each. Shared by `list_elements` and `find_elements`.
fn format_elements(elements: &[ElementInfo]) -> String {
    elements
        .iter()
        .map(|e| {
            format!(
                "{} | {} | {} | {} | {}",
                e.id.0,
                e.control_type,
                if e.actionable {
                    "actionable"
                } else {
                    "inactive"
                },
                e.automation_id.as_deref().unwrap_or(""),
                e.name.as_deref().unwrap_or(""),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `"cmd"` → [`Shell::Cmd`]; anything else (incl. `None`) → [`Shell::PowerShell`].
fn parse_shell(shell: Option<&str>) -> Shell {
    match shell {
        Some("cmd") => Shell::Cmd,
        _ => Shell::PowerShell,
    }
}

/// Omitted → default safety limit (10 min); explicit `0` → no limit; else as-is.
fn clamp_timeout(timeout_ms: Option<u64>) -> Option<u64> {
    match timeout_ms {
        None => Some(arc_proto::wire::DEFAULT_COMMAND_TIMEOUT_MS),
        Some(0) => None,
        Some(ms) => Some(ms),
    }
}

fn unexpected(reply: &Reply) -> McpError {
    McpError::internal_error(format!("unexpected reply from runner: {reply:?}"), None)
}

/// Returns `placeholder` when `text` is empty, else `text`.
fn blank_as(text: String, placeholder: &str) -> String {
    if text.is_empty() {
        placeholder.to_owned()
    } else {
        text
    }
}
