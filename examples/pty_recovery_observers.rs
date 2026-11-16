//! Stream raw PTY output through `ObserverHub`, inspect cursor-based `attach_recovery`,
//! and project fenced screen output into `transcript::Projector`.
//!
//! ```text
//! cargo run -p agent-link --example pty_recovery_observers --features "screen,observers,transcript"
//! ```

use std::error::Error;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agent_link::observers::{self, Item, ObserverHub};
use agent_link::transcript::{self, Projector, RawSpan};
use agent_link::{PtySize, RecoverySnapshot, SpawnSpec, Terminal};

const OUTPUT_WAIT: Duration = Duration::from_secs(30);

pub(crate) fn project_recovery(
    recovery: &RecoverySnapshot,
) -> Result<transcript::Snapshot, transcript::Error> {
    let mut projector = Projector::new(transcript::Limits::default())?;
    if !recovery.gaps.is_empty() {
        // A display can still be shown after raw replay loss, but it cannot repair
        // the missing input or establish a complete transcript.
        projector.record_gap()?;
    }
    if let (Some(screen), Some(last_seq)) =
        (&recovery.display, recovery.next_output_seq.checked_sub(1))
    {
        // The whole display can contain text from before the observer attached.
        // Conservatively describe its inputs from spawn through this exact fence.
        let span = RawSpan {
            generation: recovery.boundary.session,
            start: 0,
            end: recovery.boundary.offset,
            first_seq: 0,
            last_seq,
        };
        projector.screen(screen.text().trim_end(), span)?;
    }
    Ok(projector.snapshot())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pty_recovery_observers: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let size = PtySize { rows: 24, cols: 80 };
    let (term, events) = Terminal::spawn(SpawnSpec::shell(cwd, size))?;
    // ObserverHub continuously drains its own live recovery receiver; drop unused receivers
    // so they never exert backpressure on the session.
    drop(events);

    let mut hub = ObserverHub::new(&term, observers::Limits::default())?;
    let viewer = hub.attach(None)?;

    let line: &[u8] = if cfg!(windows) {
        b"echo recov^er\r"
    } else {
        b"echo recov''er\n"
    };
    term.write(line)?;

    let deadline = Instant::now() + OUTPUT_WAIT;
    let mut seen = String::new();
    let mut cursor = viewer.boundary;

    while !seen.contains("recover") {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(format!("timed out after {OUTPUT_WAIT:?}").into());
        };
        match viewer.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Some(Item::Output { end, bytes, .. }) => {
                cursor = end;
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
            Some(Item::Exit { .. }) => return Err("shell exited unexpectedly".into()),
            None if viewer.reason().is_some() => {
                return Err(format!("viewer disconnected: {:?}", viewer.reason()).into());
            }
            None => {}
        }
    }

    // Reconnect directly to Terminal from the observed byte cursor:
    let (recovery, late_rx) = term.attach_recovery(Some(cursor))?;
    drop(late_rx);
    println!(
        "recovery snapshot: requested={} boundary={} retained_start={} next_seq={} gaps={:?}",
        recovery.requested.offset,
        recovery.boundary.offset,
        recovery.retained_start.offset,
        recovery.next_output_seq,
        recovery.gaps
    );

    // Project the recovery display and its matching fence, never a later term.screen().
    let snap = project_recovery(&recovery)?;
    println!(
        "transcript snapshot: revision={}, messages={}, retained_bytes={}, incomplete={}",
        snap.revision,
        snap.messages.len(),
        snap.retained_bytes,
        snap.incomplete
    );

    drop(viewer);
    hub.shutdown();
    let exit = term.kill();
    println!("torn down: {exit:?}");
    Ok(())
}
