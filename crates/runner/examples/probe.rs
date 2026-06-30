//! Minimal **controller** probe used to exercise a real runner end-to-end.
//!
//! Now that the controller transport lives in `arc-net`, this is just a
//! thin client: connect a [`Session`] as the initiator and round-trip a couple
//! of commands.
//!
//! ```text
//! cargo run -p arc-runner --example probe -- <relay-url> <session> <pairing>
//! ```

use arc_net::{Session, SessionConfig, Transport};
use arc_proto::id::{PairingCode, RequestId, Role, SessionId};
use arc_proto::wire::{CaptureTarget, Command, Frame, Reply, Request, Shell};

type Error = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    let relay_url = args
        .next()
        .ok_or("usage: probe <relay-url> <session> <pairing>")?;
    let session: SessionId = args.next().ok_or("missing session")?.parse()?;
    let pairing = PairingCode::parse(&args.next().ok_or("missing pairing")?)?;

    let config = SessionConfig {
        transport: Transport::Relay { url: relay_url },
        session,
        pairing,
    };
    let mut session = Session::connect(&config, Role::Controller).await?;
    println!("[probe] secure channel established");

    let reply = round_trip(
        &mut session,
        1,
        Command::RunCommand {
            shell: Shell::Cmd,
            command: "echo hello from windows && ver".into(),
            timeout_ms: Some(10_000),
            stream: false,
        },
    )
    .await?;
    match reply {
        Reply::CommandOutput {
            stdout,
            stderr,
            exit_code,
        } => println!(
            "[probe] RunCommand exit={exit_code:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        ),
        other => println!("[probe] unexpected reply: {other:?}"),
    }

    let reply = round_trip(
        &mut session,
        2,
        Command::Screenshot {
            target: CaptureTarget::FullScreen,
            format: None,
            settle_ms: None,
            settle_await_change: false,
        },
    )
    .await?;
    match reply {
        Reply::Image(img) => println!(
            "[probe] Screenshot {:?} {}x{} ({} bytes)",
            img.format,
            img.width,
            img.height,
            img.data.len()
        ),
        other => println!("[probe] unexpected reply: {other:?}"),
    }

    println!("[probe] done");
    Ok(())
}

async fn round_trip(session: &mut Session, id: u64, command: Command) -> Result<Reply, Error> {
    let id = RequestId(id);
    session
        .send_frame(&Frame::Request(Request { id, command }))
        .await?;
    loop {
        match session.recv_frame().await? {
            Some(Frame::Response(response)) if response.id == id => {
                return response
                    .result
                    .map_err(|e| format!("remote error: {e:?}").into());
            }
            Some(_) => continue,
            None => return Err("link closed".into()),
        }
    }
}
