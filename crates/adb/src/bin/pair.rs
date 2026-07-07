//! Host-side pairing harness: `arc-adb-pair <host:port> <6-digit-code>`.
//!
//! Runs on the dev machine and pairs with a device's `adbd` over the network so
//! the pairing protocol can be exercised (with full packet logging) before any
//! JNI/APK packaging exists. Get the host:port + code from the device's
//! Developer options → Wireless debugging → "Pair device with pairing code".
//!
//! The adb identity is persisted at `$ARC_ADBKEY` (default
//! `~/.arc/adbkey`) as PKCS#8 PEM, and reused across runs so the same key can
//! then be used for the `A_STLS` connect.

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
    let (Some(host_port), Some(code)) = (args.next(), args.next()) else {
        bail!("usage: arc-adb-pair <host:port> <6-digit-code>");
    };

    let key = load_or_create_key()?;
    let name = format!(
        "arc@{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "host".into())
    );
    arc_adb::pairing::pair(&host_port, &code, &key, &name)
        .await
        .context("pairing")?;
    tracing::info!("paired — key authorized on device");
    Ok(())
}

/// Loads the persisted adb key, generating and saving one on first use.
fn load_or_create_key() -> Result<AdbKey> {
    let path = std::env::var("ARC_ADBKEY").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.arc/adbkey")
    });
    if let Ok(pem) = std::fs::read_to_string(&path) {
        tracing::info!(%path, "loaded adb key");
        return AdbKey::from_pkcs8_pem(&pem).context("load key");
    }
    let key = AdbKey::generate().context("generate key")?;
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&path, key.to_pkcs8_pem()?).context("save key")?;
    tracing::info!(%path, "generated + saved adb key");
    Ok(key)
}
