//! The crate's error type: [`Error`] for every fallible call, and [`Result`].
//!
//! - `Display` is meant to be shown as is: it names the step that failed (`openpty: …`,
//!   `spawn shell: …`, `invalid input: cwd <path> is not a directory`, `session closed`).
//!   Hosts map it into their own error type; branch on the variant, never on the text.
//! - Refusals ([`Error::InvalidInput`], [`Error::NotFound`]) come before anything starts or
//!   is written. A [`Error::Spawn`] after a child already started is returned only once
//!   that child was killed and reaped.
//! - [`Error::Closed`] means the session or child is gone (torn down, or its writer
//!   failed): drop the handle, nothing more can be written.

/// Everything the crate can fail with. Hosts map it into their own error type; the
/// `Display` text is written to be shown to a user as is.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Spawning a child or one of its IO threads failed. The message starts with the step,
    /// e.g. "openpty: …", "spawn shell: …" (hosts surface it verbatim).
    #[error("{0}")]
    Spawn(String),
    /// The session is torn down or its writer is gone.
    #[error("session closed")]
    Closed,
    /// The caller passed a value the crate refuses (e.g. a cwd that is not a directory).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Input exceeds a configured per-message byte limit. Nothing was queued.
    #[error("input is too large: {size} bytes exceeds the {limit}-byte limit")]
    InputTooLarge {
        /// Requested input size in bytes.
        size: usize,
        /// Maximum permitted input size in bytes.
        limit: usize,
    },
    /// The bounded input queue has no capacity. Nothing was queued; retry after progress.
    #[error("input queue is full")]
    QueueFull,
    /// A launcher was not found on the child's PATH.
    #[error("{0}")]
    NotFound(String),
    /// The operation does not exist on this platform or for this kind of session.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// An OS-level IO call failed. `Display` already ends with the source's message, so show
    /// `Display` alone; a reporter that also walks `source()` prints it twice.
    #[error("{context}: {source}")]
    Io {
        /// The step that failed, e.g. "write pty".
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// `Result` with [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::Error;
    use std::error::Error as _;

    #[test]
    fn spawn_and_not_found_messages_are_verbatim() {
        assert_eq!(
            Error::Spawn("openpty: no ptys".into()).to_string(),
            "openpty: no ptys"
        );
        assert_eq!(
            Error::NotFound("claude not found on PATH".into()).to_string(),
            "claude not found on PATH"
        );
    }

    #[test]
    fn io_error_names_the_step_and_keeps_its_source() {
        let err = Error::Io {
            context: "write pty".into(),
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe closed"),
        };
        assert_eq!(err.to_string(), "write pty: pipe closed");
        assert!(err.source().is_some());
    }
}
