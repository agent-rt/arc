//! Host-side connect harness: `arc-adb-connect <host:port> <shell-command>`.
//!
//! Uses the adb key paired earlier (persisted at `$ARC_ADBKEY`, default
//! `~/.arc/adbkey`) to connect to a device's `_adb-tls-connect` endpoint over
//! TLS and run one shell command — proving the full pair→connect→shell chain
//! runs on our own Rust adb, no system `adb` involved.

use anyhow::{Context, Result, bail};
use arc_adb::key::AdbKey;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let (Some(host_port), Some(command)) = (args.next(), args.next()) else {
        bail!("usage: arc-adb-connect <host:port> <shell-command>");
    };

    let path = std::env::var("ARC_ADBKEY").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.arc/adbkey")
    });
    let pem =
        std::fs::read_to_string(&path).with_context(|| format!("read {path} (pair first?)"))?;
    let key = AdbKey::from_pkcs8_pem(&pem).context("load key")?;

    let out = arc_adb::connect::run_shell(&host_port, &key, &command)
        .await
        .context("connect")?;
    print!("{}", String::from_utf8_lossy(&out));
    Ok(())
}
