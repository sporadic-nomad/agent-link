//! A login shell on a PTY: spawn it, run `echo hello`, print the output until the prompt is
//! back, show the screen the crate's headless terminal model drew, then tear it down.
//!
//! ```text
//! cargo run -p agent-link --example bare_shell
//! ```
//!
//! Runs your login shell (`$SHELL`, with `-l` for zsh and bash; Windows: `%COMSPEC%`) in the
//! current directory with your environment. Output chunks are printed escaped: they hold
//! your prompt's colour and title sequences. Exit status 0 once the command's output
//! arrived; 1 when it did not within 30 s, or the shell ended first.

use std::error::Error;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agent_link::{Event, PtySize, SpawnSpec, Terminal};

/// Until the command's output: a login shell reads your rc files first.
const OUTPUT_WAIT: Duration = Duration::from_secs(30);
/// Output quiet that counts as "the prompt is back" (the crate knows no prompt format).
const PROMPT_QUIET: Duration = Duration::from_millis(500);
/// A prompt that keeps redrawing (a clock) holds the example up no longer than this.
const PROMPT_MAX: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bare_shell: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let size = PtySize { rows: 24, cols: 80 };
    let (term, events) = Terminal::spawn(SpawnSpec::shell(cwd, size))?;
    let pid = term.pid().unwrap_or_default();
    println!("session {}: pid {pid}", term.id());
    // The quote (cmd.exe: the caret) splits the word, so the shell's echo of the typed line
    // never reads `hello`, only the command's output does. cmd.exe ends a line with `\r`.
    let line: &[u8] = if cfg!(windows) {
        b"echo hel^lo\r"
    } else {
        b"echo hel''lo\n"
    };
    term.write(line)?; // queued; the session's writer thread types it

    // Print the output until the command's own output arrived…
    let deadline = Instant::now() + OUTPUT_WAIT;
    let mut seen = String::new();
    while !seen.contains("hello") {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => {
                let text = String::from_utf8_lossy(&bytes);
                println!("output: {text:?}");
                seen.push_str(&text);
            }
            Ok(Event::Exit { .. }) => return Err("the shell exited first".into()),
            Ok(_) => {} // title and frame events from the screen model
            Err(_) => return Err(format!("no `hello` within {OUTPUT_WAIT:?}").into()),
        }
    }
    // …then until it pauses: the prompt is back.
    let stop = Instant::now() + PROMPT_MAX;
    while Instant::now() < stop {
        match events.recv_timeout(PROMPT_QUIET) {
            Ok(Event::Output { bytes, .. }) => {
                println!("output: {:?}", String::from_utf8_lossy(&bytes));
            }
            Ok(Event::Exit { .. }) | Err(_) => break,
            Ok(_) => {}
        }
    }

    // The headless screen model was fed every chunk before any receiver got it.
    if let Some(screen) = term.screen() {
        println!("screen:\n{}", screen.text().trim_end());
    }
    // Kills the shell's process group and the terminal's foreground job, then reaps the
    // shell (bounded). `status: None`: the kill's signal ended it.
    let exit = term.kill();
    println!("torn down: {exit:?}");
    Ok(())
}
