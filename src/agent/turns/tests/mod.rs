mod lifecycle;
pub(in crate::agent) mod peer;
mod wait;

use std::fs;
use std::os::unix::fs::symlink;

use super::*;
use crate::agent::{AbortReason, DeliveryOutcome, SendOptions};
use peer::*;

const HISTORY: &[u8] = b"\"user\"\n{\"end\":\"history\"}\n";

#[test]
fn late_binding_skips_partial_history_and_needs_a_new_user_turn() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "original", false, None);
    fs::remove_file(&peer.file).unwrap();
    let (clock, options) = clock();
    let rx = peer
        .term
        .attach_turns(peer.source(), JsonParser::default(), options)
        .unwrap();
    // Rendezvous: the first lookup completed without a file.
    tick(&clock);
    assert_eq!(rx.status(), Status::Waiting);
    let old = b"\"user\"\n{\"end\":\"history\"}\n\"us";
    // Install a nonempty file atomically so the first successful open has a known EOF.
    let late = home.path().join("late");
    fs::write(&late, old).unwrap();
    fs::rename(late, &peer.file).unwrap();
    tick(&clock);
    observed(&rx, &peer.file);
    assert_eq!(
        rx.status(),
        Status::Active {
            start: old.len() as u64,
            offset: old.len() as u64,
            incomplete_turns: 0
        }
    );
    append(
        &peer.file,
        b"er\"\n{\"end\":\"pre-attach\"}\n\"user\"\n{\"end\":\"caf\xc3",
    );
    tick(&clock);
    observed(&rx, &peer.file);
    assert!(rx.try_recv().is_none());
    // Temporary EOF (including repeated clock ticks) cannot complete this split UTF-8 record.
    tick(&clock);
    tick(&clock);
    assert!(rx.try_recv().is_none());
    append(
        &peer.file,
        b"\xa9\"}\n{\"end\":\"duplicate\"}\n\"user\"\n{\"end\":null}\n\"user\"\n\"user\"\n",
    );
    turn(&peer.file, "café");
    tick(&clock);
    observed(&rx, &peer.file);
    assert_eq!(rx.try_recv().unwrap().text, "café");
    assert_eq!(rx.try_recv().unwrap().text, "café");
    assert!(rx.try_recv().is_none());
    assert!(matches!(
        rx.status(),
        Status::Active {
            incomplete_turns: 2,
            ..
        }
    ));
}

#[test]
fn shared_project_and_root_keep_interleaved_direct_and_shell_writers_separate() {
    let (_lock, home) = project();
    let a = Peer::start(home.path(), "one", false, None);
    let b = Peer::start(home.path(), "two", true, None);
    // The local terminal tag plus the other writer's acknowledgement cannot bind.
    let foreign = a.source_with_witness(&b.witness);
    let wrong = a
        .term
        .attach_turns(foreign, JsonParser::default(), Options::default())
        .unwrap();
    gap(&wrong, Gap::Uncorroborated);
    let ar = a.attach();
    let br = b.attach();
    observed(&ar, &a.file);
    observed(&br, &b.file);
    fs::write(a.witness.with_extension("reply"), "only-first").unwrap();
    fs::write(b.witness.with_extension("reply"), "only-second").unwrap();
    for peer in [&b, &a, &a, &b] {
        peer.input("record-turn");
    }
    for (rx, expected) in [(&ar, "only-first"), (&br, "only-second")] {
        for _ in 0..2 {
            until("writer's next final reply", || {
                rx.try_recv().is_some_and(|r| {
                    assert_eq!(r.text, expected);
                    true
                })
            });
        }
        assert!(rx.try_recv().is_none());
    }
}

#[test]
fn reset_dialog_manual_clear_and_resume_follow_only_writer_evidence() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "a", false, None);
    let rx = peer.attach();
    observed(&rx, &peer.file);
    fs::write(peer.witness.with_extension("refuse"), "").unwrap();
    assert_eq!(
        peer.term.reset().wait(WAIT),
        Some(DeliveryOutcome::Aborted(AbortReason::ResetRejected))
    );
    turn(&peer.file, "after refusal");
    observed(&rx, &peer.file);
    assert_eq!(rx.try_recv().unwrap().text, "after refusal");
    peer.input("show-dialog");
    until("question visible", || {
        peer.term
            .terminal()
            .screen()
            .unwrap()
            .text()
            .contains("choose an option")
    });
    assert_eq!(
        peer.term.send("answer", SendOptions::default()).wait(WAIT),
        Some(DeliveryOutcome::Aborted(AbortReason::DialogBlocking))
    );
    assert!(matches!(rx.status(), Status::Active { .. }));
    assert!(rx.try_recv().is_none());
    peer.input("dismiss");
    until("question dismissed", || {
        !peer
            .term
            .terminal()
            .screen()
            .unwrap()
            .text()
            .contains("choose an option")
    });

    fs::remove_file(peer.witness.with_extension("refuse")).unwrap();
    assert_eq!(
        peer.term.reset().wait(WAIT),
        Some(DeliveryOutcome::Delivered)
    );
    gap(&rx, Gap::SessionChanged);
    let b = peer.file.with_file_name("a-1.jsonl");
    fs::copy(&peer.file, &b).unwrap(); // fork with a copied final reply
    let br = peer.attach();
    observed(&br, &b);
    assert!(br.try_recv().is_none());
    turn(&b, "second conversation");
    observed(&br, &b);
    assert_eq!(br.try_recv().unwrap().text, "second conversation");

    peer.input("/clear");
    gap(&br, Gap::SessionChanged);
    peer.input("/resume");
    until("writer resumed original session", || {
        fs::read_to_string(&peer.witness)
            .unwrap()
            .starts_with("a\na\n3\n")
    });
    turn(&peer.file, "written before reattachment");
    let resumed = peer.attach();
    observed(&resumed, &peer.file);
    assert!(resumed.try_recv().is_none());
    turn(&peer.file, "after resume");
    observed(&resumed, &peer.file);
    assert_eq!(resumed.try_recv().unwrap().text, "after resume");
}

#[test]
fn reads_bound_bytes_records_partial_lines_and_utf8_reply_blocks() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "bounded", false, None);
    let (clock, mut options) = clock();
    options.limits.records = 2;
    options.limits.read_bytes = 19;
    options.limits.line_bytes = 32;
    options.limits.reply_bytes = 4;
    let rx = peer
        .term
        .attach_turns(peer.source(), JsonParser::default(), options)
        .unwrap();
    observed(&rx, &peer.file);
    append(&peer.file, b"\"user\"\n".repeat(32).as_slice());
    for i in 1..=16 {
        tick(&clock);
        until(
            "exact two-record pass",
            || matches!(rx.status(), Status::Active { offset, incomplete_turns, .. } if offset == i*14 && u64::from(incomplete_turns) == i*2-1),
        );
    }
    let base = peer.file.metadata().unwrap().len();
    append(&peer.file, "{\"end\":[\"abc\",\"é\",\"z\"]}\n".as_bytes());
    tick(&clock);
    until(
        "byte-capped partial record",
        || matches!(rx.status(), Status::Active { offset, .. } if offset == base + 19),
    );
    assert!(rx.try_recv().is_none());
    tick(&clock);
    observed(&rx, &peer.file);
    let reply = rx.try_recv().unwrap();
    assert_eq!(reply.text, "abc");
    assert!(reply.truncated);
    assert!(!format!("{reply:?}").contains("abc"));
    append(&peer.file, &[b' '; 32]);
    tick(&clock);
    tick(&clock);
    observed(&rx, &peer.file);
    append(&peer.file, b"x"); // a boundary could be hidden even before the next newline
    tick(&clock);
    gap(&rx, Gap::Oversized);
    drop(rx);

    // Each loss needs a fresh EOF baseline. Finish the historical line, then recover.
    let fresh = peer.attach();
    observed(&fresh, &peer.file);
    append(&peer.file, b"\n");
    turn(&peer.file, "recovered");
    observed(&fresh, &peer.file);
    assert_eq!(fresh.try_recv().unwrap().text, "recovered");
    append(&peer.file, b"\"user\"\n{\"end\":42}\n");
    gap(&fresh, Gap::Format);
}

#[test]
fn file_identity_containment_missing_source_and_overlap_rewrites_fail_closed() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "object", false, None);
    let outside = home.path().join("outside");
    fs::write(&outside, HISTORY).unwrap();
    for reason in [Gap::Replaced, Gap::Truncated, Gap::Rewritten, Gap::Missing] {
        fs::write(&peer.file, HISTORY).unwrap();
        let rx = peer.attach();
        observed(&rx, &peer.file);
        turn(&peer.file, "queued");
        observed(&rx, &peer.file);
        match reason {
            Gap::Replaced => {
                let replacement = home.path().join("replacement");
                fs::copy(&peer.file, &replacement).unwrap();
                fs::rename(replacement, &peer.file).unwrap();
            }
            Gap::Truncated => fs::write(&peer.file, b"").unwrap(),
            Gap::Rewritten => {
                let len = peer.file.metadata().unwrap().len() as usize;
                fs::write(&peer.file, vec![b' '; len + 2]).unwrap();
            }
            Gap::Missing => fs::remove_file(&peer.file).unwrap(),
            _ => unreachable!(),
        }
        gap(&rx, reason);
    }
    symlink(&outside, &peer.file).unwrap();
    let rx = peer.attach();
    until("symlink refused", || matches!(rx.status(), Status::Gap(_)));
    assert!(rx.try_recv().is_none());
    fs::remove_file(&peer.file).unwrap();
    fs::create_dir(&peer.file).unwrap();
    gap(&peer.attach(), Gap::Uncorroborated);
}

#[test]
fn replacing_the_launch_root_invalidates_first_and_active_attachments() {
    let (_lock, home) = project();
    let waiting = Peer::start(home.path(), "waiting", false, None);
    let active = Peer::start(home.path(), "active", false, None);
    let (clock, options) = clock();
    let rx = active
        .term
        .attach_turns(active.source(), JsonParser::default(), options)
        .unwrap();
    observed(&rx, &active.file);
    turn(&active.file, "already queued");
    tick(&clock);
    observed(&rx, &active.file);

    let root = waiting.file.parent().unwrap();
    let original = home.path().join("original-records");
    fs::rename(root, &original).unwrap();
    fs::create_dir(root).unwrap();
    fs::write(&waiting.file, HISTORY).unwrap();
    // Even the original open file, moved into the replacement directory with the
    // same name and link count, cannot authorize that directory.
    fs::rename(original.join("active.jsonl"), &active.file).unwrap();
    gap(&waiting.attach(), Gap::Replaced);
    tick(&clock);
    gap(&rx, Gap::Replaced);
    gap(&active.attach(), Gap::Replaced);
}

#[test]
fn malformed_batch_or_missing_witness_discards_previously_queued_and_staged_replies() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "witness", false, None);
    let (clock, options) = clock();
    let rx = peer
        .term
        .attach_turns(peer.source(), JsonParser::default(), options)
        .unwrap();
    observed(&rx, &peer.file);
    turn(&peer.file, "already queued");
    tick(&clock);
    observed(&rx, &peer.file);
    turn(&peer.file, "not committed");
    append(&peer.file, b"malformed\n");
    tick(&clock);
    gap(&rx, Gap::Format);
    let rx = peer.attach();
    observed(&rx, &peer.file);
    fs::remove_file(&peer.witness).unwrap();
    gap(&rx, Gap::Missing);
}

#[test]
fn pilot_ceilings_accept_a_full_line_reply_queue_and_record_burst() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "ceilings", false, None);
    let (clock, options) = clock();
    let rx = peer
        .term
        .attach_turns(peer.source(), JsonParser::default(), options)
        .unwrap();
    observed(&rx, &peer.file);

    let catch_up = || {
        let end = peer.file.metadata().unwrap().len();
        let offset = || match rx.status() {
            Status::Active { offset, .. } => offset,
            other => panic!("unexpected observation status {other:?}"),
        };
        while offset() < end {
            let before = offset();
            tick(&clock);
            until("bounded pass committed", || offset() > before);
        }
    };
    let ignore = b"\"ignore\"\n";
    append(&peer.file, &ignore.repeat(257));
    tick(&clock);
    until(
        "256 complete records in first pass",
        || matches!(rx.status(), Status::Active { offset, .. } if offset == (256 * ignore.len()) as u64),
    );
    catch_up();

    // One exact 1 MiB JSON line, split across reads. The parser copies at most
    // 256 KiB from it. Three exact, untruncated replies fill the 1 MiB queue.
    let bytes = CEILINGS.line_bytes - b"{\"end\":\"\"}".len();
    turn(&peer.file, &"a".repeat(bytes));
    let exact = "b".repeat(CEILINGS.reply_bytes);
    for _ in 0..3 {
        turn(&peer.file, &exact);
    }
    catch_up();
    let first = rx.try_recv().unwrap();
    assert!(first.truncated);
    assert_eq!(first.text.len(), CEILINGS.reply_bytes);
    for _ in 0..3 {
        let reply = rx.try_recv().unwrap();
        assert!(!reply.truncated);
        assert_eq!(reply.text, exact);
    }
    for _ in 0..CEILINGS.queue_items {
        turn(&peer.file, "small");
    }
    catch_up();
    turn(&peer.file, "one too many");
    tick(&clock);
    gap(&rx, Gap::Overflow);
}

#[test]
fn writer_read_failure_is_sticky_and_file_aliases_are_ambiguous() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "read-failure", false, None);
    let alias = home.path().join("outside-root");
    fs::hard_link(&peer.file, &alias).unwrap();
    gap(&peer.attach(), Gap::Uncorroborated);
    fs::remove_file(alias).unwrap();
    let rx = peer.attach();
    observed(&rx, &peer.file);
    turn(&peer.file, "queued");
    observed(&rx, &peer.file);
    // Atomic replacement gives the source a real read error even when running as root.
    let bad = home.path().join("unreadable-witness");
    fs::write(&bad, [0xff]).unwrap();
    fs::rename(bad, &peer.witness).unwrap();
    gap(&rx, Gap::Io);
}

#[test]
fn shell_departure_invalidates_an_intact_old_writer_record() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "gone", true, None);
    let rx = peer.attach();
    observed(&rx, &peer.file);
    turn(&peer.file, "queued");
    observed(&rx, &peer.file);
    peer.input("exit-peer");
    gap(&rx, Gap::Lifecycle);
    assert!(peer.witness.is_file());
}
