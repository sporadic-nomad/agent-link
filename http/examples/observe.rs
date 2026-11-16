//! Run with AGENT_LINK_HTTP_TOKEN set to a random 32+ character secret.
//! A local host launches the shell. HTTP clients can only observe it.
use agent_link::{PtySize, SpawnSpec, Terminal};
use agent_link_http::{Config, Server};
use std::sync::Arc;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("AGENT_LINK_HTTP_TOKEN")?;
    let (terminal, receiver) = Terminal::spawn(SpawnSpec::shell(
        std::env::current_dir()?,
        PtySize { rows: 24, cols: 80 },
    ))?;
    // Every core receiver must be drained or dropped, independently of the hub.
    drop(receiver);
    let terminal = Arc::new(terminal);
    let mut server = Server::bind(terminal.clone(), Config::new(token))?;
    println!(
        "Read-only observer at http://{}; press Enter to stop",
        server.local_addr()
    );
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    server.shutdown();
    drop(server);
    drop(terminal);
    Ok(())
}
