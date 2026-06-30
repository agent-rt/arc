use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use arc_net::Controller;
use arc_proto::id::{ElementId, WindowId};
use arc_proto::wire::{CaptureTarget, Command, ImageFormat, Reply};

use crate::ack;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn screencap(
    controller: &mut Controller,
    out: &str,
    window: Option<u64>,
    element: Option<String>,
    baseline: Option<&str>,
    diff: Option<&str>,
    threshold: f64,
) -> Result<i32> {
    let target = if let Some(id) = element {
        CaptureTarget::Element(ElementId(id))
    } else if let Some(handle) = window {
        CaptureTarget::Window(WindowId(handle))
    } else {
        CaptureTarget::FullScreen
    };
    // Encode to match the output extension (.png → PNG, else WebP) so there's
    // no client-side conversion step.
    let lower = out.to_ascii_lowercase();
    let format = if lower.ends_with(".png") {
        Some(ImageFormat::Png)
    } else if lower.ends_with(".webp") {
        Some(ImageFormat::Webp)
    } else {
        None
    };
    let image = match controller
        .request(Command::Screenshot {
            target,
            format,
            settle_ms: None,
            settle_await_change: false,
        })
        .await?
    {
        Reply::Image(image) => image,
        other => bail!("unexpected reply: {other:?}"),
    };
    std::fs::write(out, &image.data).with_context(|| format!("writing {out}"))?;
    println!(
        "saved {out} ({}x{}, {:?}, {} bytes)",
        image.width,
        image.height,
        image.format,
        image.data.len()
    );

    if let Some(baseline) = baseline {
        return compare_baseline(&image.data, baseline, diff, threshold);
    }
    Ok(0)
}

/// Compares freshly-captured image bytes against a baseline file, prints a
/// verdict, optionally writes a highlighted diff, and returns a non-zero exit
/// code if more than `threshold` percent of pixels changed — so it slots into a
/// regression check. Differing dimensions count as a full (100%) change.
fn compare_baseline(
    captured: &[u8],
    baseline_path: &str,
    diff_path: Option<&str>,
    threshold: f64,
) -> Result<i32> {
    let new = image::load_from_memory(captured)
        .context("decoding the captured image")?
        .to_rgba8();
    let base = image::open(baseline_path)
        .with_context(|| format!("opening baseline {baseline_path}"))?
        .to_rgba8();

    if new.dimensions() != base.dimensions() {
        println!(
            "DIFFERS: size {}x{} vs baseline {}x{}",
            new.width(),
            new.height(),
            base.width(),
            base.height()
        );
        return Ok(2);
    }

    // A pixel "changed" if any channel differs by more than a small tolerance
    // (so lossy-codec noise doesn't read as a regression).
    const TOL: u8 = 16;
    let mut changed = 0u64;
    let (n, b) = (new.as_raw(), base.as_raw());
    let mut diff_img = diff_path.map(|_| new.clone());
    for i in (0..n.len()).step_by(4) {
        let differs = (0..3).any(|c| n[i + c].abs_diff(b[i + c]) > TOL);
        if differs {
            changed += 1;
            if let Some(img) = diff_img.as_mut() {
                // Paint changed pixels magenta in the diff overlay.
                let px =
                    img.get_pixel_mut((i as u32 / 4) % new.width(), (i as u32 / 4) / new.width());
                *px = image::Rgba([255, 0, 255, 255]);
            }
        }
    }
    let total = (new.width() as u64) * (new.height() as u64);
    let pct = if total == 0 {
        0.0
    } else {
        changed as f64 * 100.0 / total as f64
    };

    if let (Some(path), Some(img)) = (diff_path, diff_img) {
        img.save(path)
            .with_context(|| format!("writing diff {path}"))?;
        println!("diff image: {path}");
    }

    if pct > threshold {
        println!("DIFFERS: {pct:.3}% of pixels changed (> {threshold}% threshold)");
        Ok(2)
    } else {
        println!("MATCH: {pct:.3}% of pixels changed (≤ {threshold}% threshold)");
        Ok(0)
    }
}

/// Infers the encoding from a file extension (`.png` → PNG, `.webp` → WebP).
fn format_from_ext(out: &str) -> Option<ImageFormat> {
    let lower = out.to_ascii_lowercase();
    if lower.ends_with(".png") {
        Some(ImageFormat::Png)
    } else if lower.ends_with(".webp") {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

/// One-shot "verify the UI": optionally launch an app, find its window, wait for
/// it to render (until two frames are stable), and screenshot it.
pub(crate) async fn shot(
    controller: &mut Controller,
    out: &str,
    app: Option<String>,
    window: Option<u64>,
    launch: Option<String>,
    wait: u64,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);

    if let Some(exe) = &launch {
        match controller
            .request(Command::OpenApp {
                target: exe.clone(),
                args: vec![],
            })
            .await?
        {
            Reply::AppOpened { .. } => {}
            other => bail!("unexpected reply launching {exe}: {other:?}"),
        }
    }

    let hwnd = if let Some(handle) = window {
        handle
    } else {
        let needle = app
            .clone()
            .or_else(|| {
                launch.as_deref().map(|e| {
                    std::path::Path::new(e)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(e)
                        .to_owned()
                })
            })
            .ok_or_else(|| anyhow!("pass --window <handle>, --app <substr>, or --launch <exe>"))?;
        find_window(controller, &needle, deadline).await?
    };

    // Restore + foreground the window first: a minimized window captures as a
    // useless title-bar sliver, so "verify the UI" must bring it up.
    ack(
        controller,
        Command::ActivateWindow {
            window: WindowId(hwnd),
        },
    )
    .await?;

    let remaining = deadline
        .saturating_duration_since(std::time::Instant::now())
        .as_millis() as u64;
    let settle_ms = remaining.max(1500);
    match controller
        .request(Command::Screenshot {
            target: CaptureTarget::Window(WindowId(hwnd)),
            format: format_from_ext(out),
            settle_ms: Some(settle_ms),
            // A just-launched window starts on a static backdrop; wait for it to
            // actually render before settling.
            settle_await_change: launch.is_some(),
        })
        .await?
    {
        Reply::Image(image) => {
            std::fs::write(out, &image.data).with_context(|| format!("writing {out}"))?;
            println!(
                "saved {out} (window {hwnd}, {}x{}, {:?}, {} bytes)",
                image.width,
                image.height,
                image.format,
                image.data.len()
            );
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Polls the window list until one matches `needle` (title or process substring,
/// case-insensitive) or the deadline passes.
async fn find_window(
    controller: &mut Controller,
    needle: &str,
    deadline: std::time::Instant,
) -> Result<u64> {
    let needle = needle.to_lowercase();
    loop {
        if let Reply::Windows(windows) = controller.request(Command::ListWindows).await?
            && let Some(w) = windows.iter().find(|w| {
                w.title.to_lowercase().contains(&needle)
                    || w.process.to_lowercase().contains(&needle)
            })
        {
            return Ok(w.id.0);
        }
        if std::time::Instant::now() >= deadline {
            bail!("no window matching '{needle}' appeared within the wait");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}
