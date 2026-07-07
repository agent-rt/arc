//! Host-side push harness: `arc-adb-push <host:port> <local-file> <remote-path> [mode-octal]`.
//!
//! Transfers a file to the device via our own Rust adb `sync:` service, using
//! the key paired earlier (`$ARC_ADBKEY`, default `~/.arc/adbkey`). Together with
//! `arc-adb-connect` this demonstrates the whole headless bootstrap: pair →
//! connect → push the runner → launch it — no system `adb`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use arc_adb::key::AdbKey;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let (Some(host_port), Some(local), Some(remote)) = (args.next(), args.next(), args.next())
    else {
        bail!("usage: arc-adb-push <host:port> <local-file> <remote-path> [mode-octal]");
    };
    let mode = args
        .next()
        .and_then(|m| u32::from_str_radix(&m, 8).ok())
        .unwrap_or(0o644);

    let path = std::env::var("ARC_ADBKEY").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.arc/adbkey")
    });
    let pem =
        std::fs::read_to_string(&path).with_context(|| format!("read {path} (pair first?)"))?;
    let key = AdbKey::from_pkcs8_pem(&pem).context("load key")?;

    let data = std::fs::read(&local).with_context(|| format!("read {local}"))?;
    // The mode byte adbd stores is the full st_mode: regular file | perms.
    let st_mode = 0o100_000 | mode;
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    arc_adb::connect::push_file(&host_port, &key, &data, &remote, st_mode, mtime)
        .await
        .context("push")?;
    eprintln!("pushed {} bytes → {remote} (mode {mode:o})", data.len());
    Ok(())
}
