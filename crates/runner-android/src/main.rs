//! `arc-runner-android` — MVP Android runner.
//!
//! Reuses `arc-proto` + `arc-net` unchanged (the encrypted transport cross-compiles
//! to Android as-is) and serves shell commands via `sh -c`. Screenshot / UI
//! automation / input are not implemented yet and return an error — the
//! controller degrades gracefully (see the 0.8.0 capability negotiation).
//!
//! Runs in **direct** mode: listens for a controller, does the PAKE + Noise
//! handshake, then serves. Launch it with shell privilege (`adb shell`), e.g.:
//!
//! ```text
//! adb shell /data/local/tmp/arc-runner-android 0.0.0.0:8787 TEST-1234
//! adb forward tcp:8787 tcp:8787
//! arc --direct 127.0.0.1:8787 --pairing TEST-1234 shell 'uname -a'
//! ```

mod cap;

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use arc_net::Session;
use arc_proto::id::PairingCode;
use arc_proto::wire::{Command, Frame, RemoteError, RemoteErrorKind, Reply, Response};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "0.0.0.0:8787".to_owned());
    let pairing_raw = args
        .next()
        .or_else(|| std::env::var("ARC_PAIRING").ok())
        .ok_or_else(|| anyhow!("usage: arc-runner-android <listen-addr> <pairing XXXX-XXXX>"))?;
    let pairing =
        PairingCode::parse(&pairing_raw).map_err(|_| anyhow!("pairing must be XXXX-XXXX"))?;

    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "arc-runner-android listening (direct)");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(%e, "accept failed");
                continue;
            }
        };
        let pairing = pairing.clone();
        tokio::spawn(async move {
            tracing::info!(%peer, "controller connecting");
            match Session::accept_direct(stream, &pairing).await {
                Ok(session) => {
                    tracing::info!("link established");
                    serve(session).await;
                    tracing::info!("link closed");
                }
                Err(e) => tracing::warn!(%e, "handshake failed"),
            }
        });
    }
}

/// Serves one controller connection: each request → a single response.
async fn serve(session: Session) {
    let (mut writer, mut reader) = session.split();
    loop {
        match reader.recv_frame().await {
            Ok(Some(Frame::Request(request))) => {
                tracing::info!(id = %request.id, "handling request");
                let result = handle(request.command).await;
                if writer
                    .send_frame(&Frame::Response(Response {
                        id: request.id,
                        result,
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(Some(_)) => {} // stray frame; ignore
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(%e, "receive error");
                break;
            }
        }
    }
}

/// Maps a command to an Android capability. MVP: shell only; the rest return a
/// structured error so the controller sees a clear "not implemented" instead of
/// a dropped link.
async fn handle(command: Command) -> Result<Reply, RemoteError> {
    match command {
        Command::RunCommand { command, .. } => run_sh(&command).await,
        Command::RunScript { content, .. } => run_sh(&content).await,
        Command::Screenshot { target, .. } => cap::screenshot(target).await,
        Command::Click { target } => cap::click(target).await,
        Command::TypeText { text, .. } => cap::type_text(&text).await,
        Command::KeyChord { modifiers, key } => cap::key_chord(&modifiers, key).await,
        Command::ListWindows => cap::list_windows().await,
        Command::ListElements { .. } => cap::list_elements().await,
        Command::FindElements { query, .. } => cap::find_elements(&query).await,
        other => Err(RemoteError {
            kind: RemoteErrorKind::Invalid,
            message: format!(
                "arc-runner-android (MVP) does not implement this command yet: {other:?}"
            ),
        }),
    }
}

/// Runs `script` via `sh -c` and captures its output.
async fn run_sh(script: &str) -> Result<Reply, RemoteError> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| RemoteError {
            kind: RemoteErrorKind::Os,
            message: format!("spawn sh failed: {e}"),
        })?;
    Ok(Reply::CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
    })
}
