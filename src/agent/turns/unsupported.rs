//! Final reply observation requires a controlled native macOS arm64 launch.
//! Other platforms expose the same API and report unsupported launch or missing attachment.
//! The best-effort [`ambient`] tier runs here as on every platform.

#[path = "ambient/mod.rs"]
pub mod ambient;
#[path = "api.rs"]
mod api;
#[path = "providers.rs"]
mod providers;
/// The ambient receiver's waits.
#[path = "wake.rs"]
mod wake;
pub use api::{
    AttachError, ExclusiveClaudeRoot, TurnGap, TurnOptions, TurnRecvError, TurnReply, TurnStatus,
};
/// The strict JSON helpers' error and text types (`profiles::turns::json`), which the ambient
/// tier's question records use here too.
#[cfg(feature = "questions")]
pub(super) use api::{TurnGap as Gap, TurnReply as Reply};
pub use providers::{IsolatedTranscriptRoot, TranscriptReceiver, TranscriptSource, TranscriptTurn};

use std::time::Duration;

/// Pull receiver for one corroborated transcript. Created only on a supported platform,
/// so no value exists here and no call can block.
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
    /// Wait up to `timeout` for the next reply or the end of observation.
    pub fn recv_timeout(&self, _timeout: Duration) -> Result<TurnReply, TurnRecvError> {
        match self.0 {}
    }
    /// Wait up to `timeout` for a status other than `seen`.
    pub fn wait_status_change(&self, _seen: TurnStatus, _timeout: Duration) -> Option<TurnStatus> {
        match self.0 {}
    }
    /// Await the next reply or the end of observation (feature `tokio`).
    #[cfg(feature = "tokio")]
    pub async fn recv(&self) -> Result<TurnReply, TurnRecvError> {
        match self.0 {}
    }
    /// Await a status other than `seen` (feature `tokio`).
    #[cfg(feature = "tokio")]
    pub async fn status_change(&self, _seen: TurnStatus) -> TurnStatus {
        match self.0 {}
    }
    /// Close this attachment and discard pending replies.
    pub fn stop(&self) {
        match self.0 {}
    }
}
