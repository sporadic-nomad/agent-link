# Getting Started

This chapter walks through adding `agent-link` to your project and running three minimal end-to-end recipes: a PTY shell, an interactive agent CLI, and a headless JSON-lines child.

**Next:** [PTY & Screen](pty-and-screen.md)

---

## Prerequisites

- **Rust:** `1.85` or newer (Rust 2024 edition).
- **Verified platform:** macOS only. Linux and Windows (ConPTY + Job Objects) are implementation targets with native CI configured, but their runtime support remains unverified. See the [platform evidence](../README.md#platforms).
- **C compiler:** **not** required for `agent-link` itself. Only needed if you build the separate workspace crate `agent-link-supervisor` (which bundles SQLite).

Add `agent-link` from crates.io to `Cargo.toml`:

```toml
[dependencies]
# Default features: `screen` + `agent`
agent-link = "0.3"

# Minimal PTY-only build (no vt100, regex, or agent profiles)
# agent-link = { version = "0.3", default-features = false }

# Add headless stdio RPC, CLI probes, or Tokio adapters
# agent-link = { version = "0.3", features = ["rpc", "probe", "tokio"] }
```

For development, use `{ path = "../agent-link" }` or
`{ git = "https://github.com/sporadic-nomad/agent-link" }` instead of the version.

---

## Recipe 1: Run a Shell Command on a PTY

Use `Terminal::spawn` with `SpawnSpec::shell` to start the user's login shell (`$SHELL` with `-l` for zsh/bash on Unix; `%COMSPEC%` on Windows), write bytes, stream raw output, and inspect the headless screen model.

Complete example: [`examples/bare_shell.rs`](../examples/bare_shell.rs) (`cargo run -p agent-link --example bare_shell`).

```rust
use std::time::{Duration, Instant};
use agent_link::{Event, PtySize, SpawnSpec, Terminal};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let size = PtySize { rows: 24, cols: 80 };
    let (term, events) = Terminal::spawn(SpawnSpec::shell(cwd, size))?;

    // cmd.exe terminates lines with `\r`; POSIX shells expect `\n`.
    let line: &[u8] = if cfg!(windows) {
        b"echo hel^lo\r"
    } else {
        b"echo hel''lo\n"
    };
    term.write(line)?;

    // Drain events until the command output arrives.
    // (Quote/caret splits the word so the echoed input doesn't match prematurely.)
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    while !seen.contains("hello") {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(Event::Exit { .. }) => return Err("shell exited unexpectedly".into()),
            Ok(_) => {} // Title, Progress, Frame, ScreenReset
            Err(_) => return Err("timed out waiting for output".into()),
        }
    }

    if let Some(screen) = term.screen() {
        println!("Screen:\n{}", screen.text().trim_end());
    }

    // Kills the shell's process group / Job Object and reaps the child (~2 s bound).
    let _exit = term.kill();
    Ok(())
}
```

> **Tip:** If you spawn `Terminal` and don't intend to read `events` anymore, **drop `events`** (`drop(events)`). An undrained receiver fills up at 1,024 events and stalls output reading for the whole session.

---

## Recipe 2: Launch an Interactive Agent and Deliver a Prompt

Use `AgentTerminal` when driving an interactive TUI agent (`claude`, `codex`, `pi`, `opencode`, or `cursor`) inside a PTY. It types the launch command, steers startup dialogs (when opted in), waits for a ready composer, pastes your message, presses Enter, and verifies on screen that the composer consumed it.

The CLI must be installed and signed in. Running this example submits a real model turn on that account with unattended permissions and opts into folder-trust steering.

Complete example: [`examples/agent_send.rs`](../examples/agent_send.rs) (`cargo run -p agent-link --example agent_send -- claude "Reply with the single word: pong"`).

```rust
use std::time::{Duration, Instant};
use agent_link::agent::{
    AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, ProfileRegistry, SendOptions,
    TrustPolicy,
};
use agent_link::{Event, PtySize, SpawnSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile = ProfileRegistry::builtin()
        .get("claude")
        .ok_or("unknown profile")?;

    let mut agent = AgentSpec::new(profile);
    // Opt into pre-trust flags and automatic dialog steering for headless examples.
    // Warning: some CLIs save folder-trust decisions to their user config!
    agent.trust = TrustPolicy::unattended();

    // Note: AgentSpec::default / ::new sets `permissions = Permissions::Unattended` (YOLO).
    // Set `agent.permissions = Permissions::AgentDefault` to keep the CLI's own policy.

    let size = PtySize { rows: 40, cols: 120 };
    let (term, events) = AgentTerminal::spawn(SpawnSpec::shell(std::env::current_dir()?, size), agent)?;
    term.launch()?;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::AgentState { state: AgentState::Ready, .. }) => break,
            Ok(Event::AgentState { state: AgentState::Exited, .. }) => {
                return Err("session exited before becoming ready".into());
            }
            Ok(_) => {}
            Err(_) => return Err("timed out waiting for AgentState::Ready".into()),
        }
    }
    drop(events); // Prevent channel backpressure while awaiting ticket

    let opts = SendOptions {
        submit: true,
        ..SendOptions::default()
    };
    let ticket = term.send("Reply with the single word: pong", opts);
    match ticket.wait(Duration::from_secs(120)) {
        Some(DeliveryOutcome::Delivered) => println!("Delivered!"),
        Some(other) => println!("Not delivered: {other:?}"),
        None => println!("Timed out awaiting ticket"),
    }

    Ok(()) // Dropping `term` kills and reaps the agent and shell.
}
```

> **Note on verified delivery:** `claude`, `codex`, and `pi` include measured composer verification. `opencode` and `cursor` layouts are not sufficiently measured for verified submits and return `DeliveryOutcome::Aborted(AbortReason::SubmitUnverified)`. A host that accepts unverified submission can set `SendOptions::unreadable_composer` to `UnreadableComposer::EnterOnce`. Delivery then writes one Enter without composer evidence and returns `Aborted(AbortReason::UnverifiedEnter)`, never `Delivered`. Cursor support is **work in progress**; authenticated `--force` startup/composer behavior is also unverified.

---

## Recipe 3: Run a Headless JSON-Lines Child (`rpc`)

When you want structured stdio without a PTY, enable the `rpc` feature and use `RpcChild` (or a higher-level native adapter like `codex`, `acp`, or `claude`).

The following transport-only example uses `cat` as a peer and needs no agent account. On Windows, put `cat` on `PATH` (for example, Git for Windows's `usr\bin`). Complete example: [`examples/rpc_child.rs`](../examples/rpc_child.rs) (`cargo run -p agent-link --example rpc_child --features rpc`).

```rust
use std::sync::mpsc;
use std::time::Duration;
use agent_link::agent::RpcLaunch;
use agent_link::rpc::{forward_rpc, preflight, RpcChild, RpcEvent, RpcSpawnSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request = r#"{"id":1,"method":"ping"}"#;
    let launch = RpcLaunch::new("cat", &[]);

    let spec = RpcSpawnSpec::new(launch, std::env::current_dir()?);

    // Validate limits, cwd, and command construction before spawning or replacing a child.
    // Only Windows resolves the launcher here; Unix does so in the spawned login shell.
    // Success does not guarantee that spawning or agent startup will succeed.
    preflight(&spec)?;

    let (child, events) = RpcChild::spawn(spec)?;
    let (tx, rx) = mpsc::channel();
    forward_rpc(events, move |ev| match ev {
        RpcEvent::Line { seq, line, .. } => {
            println!("line {seq}: {line}");
            if line == request {
                let _ = tx.send(Ok(()));
            }
        }
        RpcEvent::Error { error, .. } => {
            let _ = tx.send(Err(format!("transport error: {error}")));
        }
        RpcEvent::Exit { .. } => {
            let _ = tx.send(Err("child exited before echoing the request".to_string()));
        }
        _ => {}
    })?;

    child.send_line(request)?;
    rx.recv_timeout(Duration::from_secs(60))??;
    let _code = child.kill();
    Ok(())
}
```

---

## Next Steps

- Learn about PTY lifecycle, screen parsing, and environment isolation in [PTY & Screen](pty-and-screen.md).
- Dive into startup steering, delivery tickets, and custom profiles in [Agents & Delivery](agents-and-delivery.md).
- See native adapters (`codex`, `acp`, `claude`) in [RPC & Native Adapters](rpc-and-native-adapters.md).
