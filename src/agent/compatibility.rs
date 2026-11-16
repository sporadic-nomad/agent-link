//! Operation-scoped compatibility declarations. Evidence describes a particular check,
//! never certification of a provider, every platform, or future CLI versions.

/// Whether an implementation offers an operation. Unknown is not unsupported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Support {
    /// Implemented for the stated scope; consult its evidence before relying on it.
    Supported,
    /// Explicitly outside the implementation's scope.
    Unsupported,
    /// No declaration is available.
    #[default]
    Unknown,
}

/// Source of an observation, not an ordinal quality score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvidenceKind {
    /// An implementation declaration, without a runtime measurement.
    Declaration,
    /// A feature explicitly negotiated with a running peer.
    Negotiated,
    /// A manually authored fixture or scripted peer.
    SyntheticFixture,
    /// Recorded terminal bytes; only the recorded scenes were observed.
    CapturedTerminal,
    /// A check against a real CLI, scoped to the operation and context recorded.
    LiveCheck,
}

/// Provenance of one operation. `None` means unrecorded, not all versions/platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Evidence {
    /// What was measured (for example, composer recognition, not turn completion).
    pub operation: &'static str,
    /// How the evidence was obtained.
    pub kind: EvidenceKind,
    /// CLI version, if known; descriptive, never a version admission range.
    pub cli_version: Option<&'static str>,
    /// Platform of the measurement, if recorded.
    pub platform: Option<&'static str>,
    /// Repository-relative artifact or other stable provenance reference.
    pub source: &'static str,
}

impl Evidence {
    /// Creates an operation-scoped record with unknown CLI version and platform.
    /// Custom profiles may set those public fields when the artifact records them.
    ///
    /// ```
    /// use agent_link::agent::compatibility::{Compatibility, Evidence, EvidenceKind};
    /// const RECORDS: &[Evidence] = &[
    ///     Evidence::new("ready screen", EvidenceKind::SyntheticFixture, "tests/ready.txt"),
    /// ];
    /// let mut metadata = Compatibility::default();
    /// metadata.evidence = RECORDS;
    /// ```
    pub const fn new(operation: &'static str, kind: EvidenceKind, source: &'static str) -> Self {
        Self {
            operation,
            kind,
            cli_version: None,
            platform: None,
            source,
        }
    }
}

/// Profile/adapter metadata independent of the installed CLI. Custom profiles default
/// to unknown. A successful install probe does not upgrade these declarations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Compatibility {
    /// Implementation revision; built-ins use the crate version. Not the CLI version.
    pub revision: Option<&'static str>,
    /// PTY screen interpretation and launch support.
    pub pty: Support,
    /// Structured stdio launch support; does not imply a native protocol adapter.
    pub rpc_launch: Support,
    /// A profile sign-in check exists. Unsupported means no check, not signed out.
    pub login_check: Support,
    /// Operation-specific evidence. Absence means untested/undocumented.
    pub evidence: &'static [Evidence],
}
