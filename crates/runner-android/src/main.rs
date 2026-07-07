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
//!
//! ## Persistence
//!
//! The runner must stay under the **shell uid (2000)** — that uid owns the
//! `input` / `screencap` / `uiautomator` privileges. A normal APK foreground
//! service runs under its own app uid and loses them, so it is useless here;
//! reboot-surviving auto-start needs Shizuku or root (a separate milestone).
//!
//! What works with no APK/root is *detach + self-respawn*: `setsid` detaches the
//! process from the launching `adb shell` (so it survives disconnect) and
//! `--supervise` makes it re-exec itself in server mode, restarting on crash:
//!
//! ```text
//! adb shell "setsid /data/local/tmp/arc-runner-android --supervise 0.0.0.0:8787 TEST-1234 \
//!   </dev/null >/data/local/tmp/arc-runner.log 2>&1 &"
//! ```

mod backend;
mod cap;

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use arc_net::Session;
use arc_proto::id::PairingCode;
use arc_proto::wire::{Frame, Response};
use backend::AndroidBackend;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().is_some_and(|a| a == "--supervise") {
        raw.remove(0);
        return supervise(raw).await;
    }

    let mut args = raw.into_iter();
    let addr = args.next().unwrap_or_else(|| "0.0.0.0:8787".to_owned());
    let pairing_raw = args
        .next()
        .or_else(|| std::env::var("ARC_PAIRING").ok())
        .ok_or_else(|| {
            anyhow!("usage: arc-runner-android [--supervise] <listen-addr> <pairing XXXX-XXXX>")
        })?;
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

/// Supervisor loop: re-execs this binary in server mode as a child and restarts
/// it whenever it exits, so a panic or a killed link self-heals. Backoff grows on
/// fast crashes (to avoid a spin loop) and resets after a healthy run. Launched
/// under `setsid`, the whole tree also survives the `adb shell` that started it.
async fn supervise(child_args: Vec<String>) -> Result<()> {
    let exe = std::env::current_exe().context("resolving own path for --supervise")?;
    let mut backoff = Duration::from_secs(1);
    tracing::info!(?exe, ?child_args, "supervisor started");
    loop {
        let started = Instant::now();
        match tokio::process::Command::new(&exe)
            .args(&child_args)
            .status()
            .await
        {
            Ok(status) => tracing::warn!(code = status.code(), "runner exited; restarting"),
            Err(e) => tracing::error!(%e, "failed to spawn runner"),
        }
        // A run that lasted a while was healthy — restart promptly. A fast exit
        // is a crash loop — back off, capped, so we don't spin the CPU.
        if started.elapsed() >= Duration::from_secs(30) {
            backoff = Duration::from_secs(1);
        }
        tracing::info!(?backoff, "waiting before restart");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Serves one controller connection: each request → a single response, mapped by
/// the shared dispatcher against the Android backend. (No streaming or port
/// forwarding in the MVP — those are serve-loop features to add later.)
async fn serve(session: Session) {
    let (mut writer, mut reader) = session.split();
    loop {
        match reader.recv_frame().await {
            Ok(Some(Frame::Request(request))) => {
                tracing::info!(id = %request.id, "handling request");
                let result = arc_runner_core::dispatch::dispatch(
                    &AndroidBackend,
                    request.id,
                    request.command,
                )
                .await;
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
