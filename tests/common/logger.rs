//! A test logger: every `log` record of the test binary, kept as `"<LEVEL> <message>"`
//! lines, for assertions on the crate's `[agent-link:<id>]` lines. One logger per process
//! and tests run in parallel, so filter by session ([`lines_for`]).

use std::sync::{Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

use agent_link::SessionId;

struct Collector {
    lines: Mutex<Vec<(Instant, String)>>,
}

static COLLECTOR: Collector = Collector {
    lines: Mutex::new(Vec::new()),
};

impl log::Log for Collector {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let line = format!("{} {}", record.level(), record.args());
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((Instant::now(), line));
    }

    fn flush(&self) {}
}

/// Installs the logger at `Trace` (once per process; later calls do nothing). Call it
/// before the session whose lines a test reads starts.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        log::set_logger(&COLLECTOR).expect("no other logger in the test binary");
        log::set_max_level(log::LevelFilter::Trace);
    });
}

/// Every line logged so far.
pub fn lines() -> Vec<String> {
    timed_lines().into_iter().map(|(_, line)| line).collect()
}

/// Every line logged so far, with the time it was logged.
pub fn timed_lines() -> Vec<(Instant, String)> {
    COLLECTOR
        .lines
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The lines tagged `[agent-link:<id>]`.
pub fn lines_for(id: SessionId) -> Vec<String> {
    let tag = format!("[agent-link:{id}]");
    lines().into_iter().filter(|l| l.contains(&tag)).collect()
}

/// Polls until a line tagged with `id` contains `needle` and returns it; panics after
/// `timeout` with that session's lines.
pub fn wait_line(id: SessionId, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let lines = lines_for(id);
        if let Some(line) = lines.iter().find(|l| l.contains(needle)) {
            return line.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no {needle:?} line for session {id} within {timeout:?}; its lines: {lines:#?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
