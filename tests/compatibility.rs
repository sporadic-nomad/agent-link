//! Metadata is tied to its artifacts, never upgraded into a provider-wide claim.
#![cfg(feature = "agent")]
use agent_link::agent::{ProfileRegistry, compatibility::EvidenceKind};
use std::path::Path;

#[test]
fn profile_evidence_artifacts_and_recorded_versions_exist() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry = ProfileRegistry::builtin();
    for id in registry.ids() {
        let metadata = registry.get(&id).unwrap().compatibility();
        assert_eq!(metadata.revision, Some(env!("CARGO_PKG_VERSION")));
        for evidence in metadata.evidence {
            let source = Path::new(evidence.source);
            assert!(
                !source.starts_with("internal-docs"),
                "{id}: public compatibility evidence must not depend on local developer notes: {}",
                source.display()
            );
            let path = root.join(source);
            assert!(path.is_file(), "{id}: {}", path.display());
            if evidence.kind == EvidenceKind::LiveCheck {
                let report = std::fs::read_to_string(&path).unwrap();
                if let Some(version) = evidence.cli_version {
                    assert!(
                        report.contains(version),
                        "{id}: live-check report {} does not record CLI version {version}",
                        path.display()
                    );
                }
            }
            if evidence.kind == EvidenceKind::CapturedTerminal {
                let meta = std::fs::read_to_string(path.with_extension("meta")).unwrap();
                assert!(meta.lines().any(|l| l == format!("agent={id}")));
                assert!(
                    meta.lines()
                        .any(|l| l == format!("version={}", evidence.cli_version.unwrap()))
                );
            }
        }
    }
}

#[cfg(feature = "codex")]
#[test]
fn codex_protocol_provenance_is_synthetic_and_pinned() {
    let provenance: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex-app-server/provenance.json")).unwrap();
    assert_eq!(provenance["revision"], agent_link::codex::PROTOCOL_REVISION);
    assert_eq!(provenance["kind"], "synthetic");
    let evidence = &agent_link::codex::COMPATIBILITY_EVIDENCE[0];
    assert_eq!(evidence.kind, EvidenceKind::SyntheticFixture);
    assert!(evidence.cli_version.is_none());
    assert!(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(evidence.source)
            .exists()
    );
}
