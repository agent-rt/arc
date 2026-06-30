use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use arc_net::Controller;
use arc_proto::id::{ElementId, WindowId};
use arc_proto::wire::{Command, ElementInfo, ElementQuery, Reply};

use crate::{ClipCmd, ack};

pub(crate) async fn windows(
    controller: &mut Controller,
    json: bool,
    filter: Option<&str>,
) -> Result<()> {
    match controller.request(Command::ListWindows).await? {
        Reply::Windows(mut windows) => {
            if let Some(needle) = filter {
                let needle = needle.to_lowercase();
                windows.retain(|w| {
                    w.title.to_lowercase().contains(&needle)
                        || w.process.to_lowercase().contains(&needle)
                });
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&windows)?);
            } else {
                for w in &windows {
                    println!("{} | {} | {}", w.id.0, w.process, w.title);
                }
            }
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub(crate) async fn elements(controller: &mut Controller, window: u64, json: bool) -> Result<()> {
    match controller
        .request(Command::ListElements {
            window: WindowId(window),
        })
        .await?
    {
        Reply::Elements(elements) => {
            print_elements(&elements, json)?;
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Finds elements by attribute (`wait_ms = None`) or waits for one to appear
/// (`wait_ms = Some`), printing the matches. A `wait` that times out surfaces
/// as the runner's error.
pub(crate) async fn find_elements(
    controller: &mut Controller,
    window: u64,
    query: ElementQuery,
    wait_ms: Option<u64>,
    json: bool,
) -> Result<i32> {
    match controller
        .request(Command::FindElements {
            window: WindowId(window),
            query,
            wait_ms,
        })
        .await?
    {
        Reply::Elements(elements) => {
            print_elements(&elements, json)?;
            Ok(0)
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Prints elements as JSON or one pipe-delimited row each.
fn print_elements(elements: &[ElementInfo], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(elements)?);
    } else {
        for e in elements {
            print_element(e);
        }
    }
    Ok(())
}

/// Prints one element row: `id | control_type | actionable? | automation_id | name`.
fn print_element(e: &ElementInfo) {
    println!(
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
    );
}

pub(crate) async fn open(
    controller: &mut Controller,
    app: String,
    args: Vec<String>,
) -> Result<()> {
    match controller
        .request(Command::OpenApp { target: app, args })
        .await?
    {
        Reply::AppOpened { window, pid } => {
            println!("launched pid={pid} window={window:?}");
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Sends a sequence of key chords in order on one connection. All chords are
/// parsed up front (so a bad chord aborts before anything is pressed), then
/// applied with a short gap between them for reliable delivery to WinUI apps.
pub(crate) async fn keys(
    controller: &mut Controller,
    chords: Vec<String>,
    into: Option<String>,
) -> Result<()> {
    let parsed = chords
        .iter()
        .map(|c| arc_proto::wire::parse_chord(c).map_err(|e| anyhow!("{c}: {e}")))
        .collect::<Result<Vec<_>>>()?;
    // Focus the target element first (chords are sent to whatever has focus).
    if let Some(element_id) = into {
        ack(
            controller,
            Command::FocusElement {
                element: ElementId(element_id),
            },
        )
        .await?;
    }
    let last = parsed.len().saturating_sub(1);
    for (i, (modifiers, key)) in parsed.into_iter().enumerate() {
        ack(controller, Command::KeyChord { modifiers, key }).await?;
        if i < last {
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    }
    Ok(())
}

/// Reads or writes the remote clipboard.
pub(crate) async fn clip(controller: &mut Controller, action: ClipCmd) -> Result<()> {
    match action {
        ClipCmd::Get => match controller.request(Command::ClipboardGet).await? {
            Reply::Text(text) => {
                print!("{text}");
                Ok(())
            }
            other => bail!("expected text, got {other:?}"),
        },
        ClipCmd::Set { text } => {
            // `-` means read the clipboard contents from stdin.
            let text = if text == "-" {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                buf
            } else {
                text
            };
            ack(controller, Command::ClipboardSet { text }).await
        }
    }
}
