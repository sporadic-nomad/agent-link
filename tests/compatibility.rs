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

#[test]
fn question_recognition_names_the_recorded_layout_for_each_builtin() {
    let registry = ProfileRegistry::builtin();
    for (id, version, fixture) in [
        ("claude", "2.1.296", "claude-2.1.296-question-single.bin"),
        ("codex", "0.154.0", "codex-0.154.0-question-plan.bin"),
        ("opencode", "1.18.21", "opencode-1.18.21-question.bin"),
        (
            "cursor",
            "2026.11.13-eacade1",
            "cursor-2026.11.13-shell-approval-renderer.bin",
        ),
    ] {
        let profile = registry.get(id).unwrap();
        let record = profile
            .compatibility()
            .evidence
            .iter()
            .find(|e| e.operation == "question dialog recognition")
            .unwrap();
        assert_eq!(record.kind, EvidenceKind::CapturedTerminal);
        assert_eq!(record.source, format!("tests/fixtures/{fixture}"));
        assert_eq!(record.cli_version, Some(version));
        assert_eq!(record.platform, Some("macos-aarch64"));
    }
    assert!(
        registry
            .get("pi")
            .unwrap()
            .compatibility()
            .evidence
            .iter()
            .all(|e| e.operation != "question dialog recognition")
    );
}

#[test]
fn claude_writer_binding_and_final_reply_evidence_match_the_paired_capture() {
    const SOURCE: &str = "tests/fixtures/transcripts/claude-2.1.296/capture.json";
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(SOURCE);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["kind"], "captured-transcript");
    assert_eq!(manifest["provider"], "claude");
    assert_eq!(manifest["launchMode"], "direct");
    assert_eq!(manifest["version"], "2.1.296");
    assert_eq!(manifest["platformId"], "macos-aarch64");
    let operations = [
        "direct PTY transcript writer binding with host-enforced exclusive access",
        "direct PTY final reply parsing",
    ];
    assert_eq!(manifest["operations"], serde_json::json!(operations));

    let profile = ProfileRegistry::builtin().get("claude").unwrap();
    let metadata = profile.compatibility();
    for operation in operations {
        let evidence = metadata
            .evidence
            .iter()
            .find(|e| e.operation == operation)
            .unwrap();
        assert_eq!(evidence.kind, EvidenceKind::CapturedTranscript);
        assert_eq!(evidence.source, SOURCE);
        assert_eq!(evidence.cli_version, manifest["version"].as_str());
        assert_eq!(evidence.platform, manifest["platformId"].as_str());
    }

    let pair = &manifest["twoTerminals"];
    for writer in pair["writers"].as_object().unwrap().values() {
        assert_eq!(writer["version"], manifest["version"]);
        assert_eq!(writer["kind"], "interactive");
        assert_eq!(writer["entrypoint"], "cli");
        assert_eq!(writer["pidDomain"], "darwin");
    }
    let recordings = pair["transcripts"]
        .as_array()
        .unwrap()
        .iter()
        .chain(std::iter::once(
            &manifest["overlappingResume"]["transcript"],
        ));
    for recording in recordings {
        let path = manifest_path
            .parent()
            .unwrap()
            .join(recording["file"].as_str().unwrap());
        let data = std::fs::read_to_string(path).unwrap();
        assert_eq!(
            recording["projectedRecords"].as_u64(),
            Some(data.lines().count() as u64)
        );
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
