//! Claude Code 2.1.296 transcript binding for a host-controlled, exclusive interactive writer.
//! A second CLI can resume a live UUID and append indistinguishable records. Ordinary
//! launches therefore have no provider attachment. See the paired PTY capture in
//! tests/fixtures/transcripts/claude-2.1.296.

mod parser;
mod source;
#[cfg(test)]
mod tests;
#[cfg(feature = "turns")]
pub(in crate::agent) use source::{ClaudeSource, capture_exclusive};
mod wire;

const VERSION: &str = "2.1.296";
