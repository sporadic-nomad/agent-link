//! The fake agent's scene names: one list for the binary and the test harness
//! (`tests/common/fake.rs` includes this file with `#[path]`).

/// The fake peer's diagnostic after refusing an explicit `/new`.
pub const RESET_REFUSAL: &str = "fake: cannot start a new conversation";

/// Every scene `agent-link-fake-agent` accepts; any other exits 2.
pub const SCENES: &[&str] = &[
    "trust",
    "ready",
    "composer",
    "swallow",
    "late-dialog",
    "slow-dialog",
    "question-dialog",
    "question-on-paste",
    "exit-to-shell",
    "progress",
    "silent",
];
