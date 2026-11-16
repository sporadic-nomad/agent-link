//! The bounded tail of an RPC child's stderr, attached to its `Exit` event.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

/// Most recent stderr lines kept.
pub(super) const STDERR_TAIL_MAX_LINES: usize = 32;
/// Byte cap over the kept lines; a single longer line is truncated to it.
pub(super) const STDERR_TAIL_MAX_BYTES: usize = 4096;

/// A shared ring of the child's latest stderr lines: the stderr thread pushes, the reader
/// reads the tail when the child exits.
#[derive(Clone, Default)]
pub(super) struct StderrRing {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrRing {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pushes one line: truncated to the byte cap on a char boundary, then older lines are
    /// evicted until both caps hold — always keeping at least this one.
    pub(super) fn push(&self, mut line: String) {
        if line.len() > STDERR_TAIL_MAX_BYTES {
            let mut end = STDERR_TAIL_MAX_BYTES;
            while end > 0 && !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
        }
        let mut inner = self.lock();
        inner.bytes += line.len() + usize::from(!inner.lines.is_empty());
        inner.lines.push_back(line);
        while inner.lines.len() > 1
            && (inner.lines.len() > STDERR_TAIL_MAX_LINES || inner.bytes > STDERR_TAIL_MAX_BYTES)
        {
            if let Some(old) = inner.lines.pop_front() {
                inner.bytes -= old.len() + 1;
            }
        }
    }

    /// The kept lines, oldest first, joined by `\n`.
    pub(super) fn tail(&self) -> String {
        let inner = self.lock();
        inner.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the newest lines stay (line cap), joined newest-last.
    #[test]
    fn stderr_ring_bounds_line_count() {
        let ring = StderrRing::default();
        for i in 0..(STDERR_TAIL_MAX_LINES + 10) {
            ring.push(format!("line {i}"));
        }
        let tail = ring.tail();
        let kept: Vec<&str> = tail.lines().collect();
        assert_eq!(kept.len(), STDERR_TAIL_MAX_LINES, "capped at the line cap");
        assert_eq!(
            *kept.last().expect("non-empty"),
            format!("line {}", STDERR_TAIL_MAX_LINES + 9),
            "the newest line is kept"
        );
        assert!(!tail.contains("line 0"), "the oldest lines are evicted");
    }

    /// A line longer than the byte cap is truncated to it and still kept.
    #[test]
    fn stderr_ring_truncates_oversized_line() {
        let ring = StderrRing::default();
        ring.push("x".repeat(STDERR_TAIL_MAX_BYTES * 2));
        assert_eq!(ring.tail().len(), STDERR_TAIL_MAX_BYTES);
    }

    #[test]
    fn stderr_tail_byte_limit_includes_joining_newlines() {
        let ring = StderrRing::default();
        ring.push("a".repeat(STDERR_TAIL_MAX_BYTES / 2));
        ring.push("b".repeat(STDERR_TAIL_MAX_BYTES / 2));
        assert!(ring.tail().len() <= STDERR_TAIL_MAX_BYTES);
    }

    /// The cut never splits a character, and the byte cap evicts older lines.
    #[test]
    fn stderr_ring_cuts_on_a_char_boundary_and_evicts_by_bytes() {
        let ring = StderrRing::default();
        ring.push("old".into());
        // 3-byte characters: the cap (4 096) is not a multiple of 3.
        ring.push("€".repeat(STDERR_TAIL_MAX_BYTES));
        let tail = ring.tail();
        assert!(tail.len() <= STDERR_TAIL_MAX_BYTES, "{}", tail.len());
        assert!(tail.len() > STDERR_TAIL_MAX_BYTES - 3, "{}", tail.len());
        assert!(
            tail.chars().all(|c| c == '€'),
            "the old line went over the byte cap"
        );
    }
}
