//! The fake agent's scene names: one list for the binary and the test harness
//! (`tests/common/fake.rs` includes this file with `#[path]`).

/// Every scene `agent-link-fake-agent` accepts; any other exits 2.
pub const SCENES: &[&str] = &[
    "trust",
    "ready",
    "composer",
    "swallow",
    "late-dialog",
    "slow-dialog",
    "exit-to-shell",
    "progress",
    "silent",
];
