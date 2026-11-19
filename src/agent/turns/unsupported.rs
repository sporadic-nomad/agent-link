//! Final reply observation requires a native macOS arm64 Claude Code launch.
//! Other platforms expose the same API and report unsupported launch or missing attachment.

#[path = "api.rs"]
mod api;
pub use api::{AttachError, ExclusiveClaudeRoot, TurnGap, TurnOptions, TurnReply, TurnStatus};

/// Pull receiver for one corroborated transcript. Created only on a supported platform.
pub struct TurnReceiver(std::convert::Infallible);

impl TurnReceiver {
    /// Return the current observation status without performing filesystem I/O.
    pub fn status(&self) -> TurnStatus {
        match self.0 {}
    }
    /// Pull one observed final reply after bounded local evidence checks.
    pub fn try_recv(&self) -> Option<TurnReply> {
        match self.0 {}
    }
    /// Close this attachment and discard pending replies.
    pub fn stop(&self) {
        match self.0 {}
    }
}
