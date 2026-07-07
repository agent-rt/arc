//! Host-side pairing harness: `arc-adb-pair <host:port> <6-digit-code>`.
//!
//! Runs on the dev machine and pairs with a device's `adbd` over the network so
//! the pairing protocol can be nailed down (with full packet logging) before any
//! JNI/APK packaging exists. Get the host:port + code from the device's
//! Developer options → Wireless debugging → "Pair device with pairing code".

use anyhow::{Context, Result, bail};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let host_port = args.next();
    let code = args.next();
    let (Some(host_port), Some(code)) = (host_port, code) else {
        bail!("usage: arc-adb-pair <host:port> <6-digit-code>");
    };

    arc_adb::pairing::pair(&host_port, &code)
        .await
        .context("pairing")?;
    tracing::info!("paired");
    Ok(())
}
