//! Drive a PTY shell asynchronously with `EventStream` and `kill_async` (`tokio` feature).
//!
//! ```text
//! cargo run -p agent-link --example tokio_shell --no-default-features --features tokio
//! ```

use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

use agent_link::{Event, EventStream, PtySize, SpawnSpec, Terminal};
use tokio::time::timeout;

const OUTPUT_WAIT: Duration = Duration::from_secs(30);

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tokio_shell: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let spec = SpawnSpec::shell(cwd, PtySize { rows: 24, cols: 80 });
    let (term, events) = Terminal::spawn(spec)?;
    let id = term.id();

    let result = async {
        let mut events = EventStream::new(events)?;
        let line: &[u8] = if cfg!(windows) {
            b"echo hel^lo\r"
        } else {
            b"echo hel''lo\n"
        };
        term.write(line)?;
        wait_for_hello(&mut events, OUTPUT_WAIT).await
    }
    .await;

    // Bridge/write errors, EOF, and timeout also take the async teardown path.
    let exit = term.kill_async().await;
    result?;
    println!("received `hello` from session {id}");
    println!("torn down asynchronously: {exit:?}");
    Ok(())
}

pub(crate) async fn wait_for_hello(
    events: &mut EventStream,
    limit: Duration,
) -> Result<(), Box<dyn Error>> {
    timeout(limit, async {
        let mut seen = Vec::new();
        while let Some(event) = events.next().await {
            match event {
                Event::Output { bytes, .. } => {
                    seen.extend_from_slice(&bytes);
                    if seen.windows(5).any(|word| word == b"hello") {
                        return Ok::<(), Box<dyn Error>>(());
                    }
                    // Retain only the suffix needed to match across chunk boundaries.
                    seen.drain(..seen.len().saturating_sub(4));
                }
                Event::Exit { .. } => return Err("shell exited before output".into()),
                _ => {}
            }
        }
        Err("event stream ended before output".into())
    })
    .await?
}
