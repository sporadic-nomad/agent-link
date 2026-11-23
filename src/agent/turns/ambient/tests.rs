//! Queue, status, option and budget contracts of the receiver, without a terminal.

mod wait;

use super::*;

fn turn(text: &str) -> AmbientTurn {
    let reply = TurnReply {
        text: text.into(),
        ..TurnReply::default()
    };
    AmbientTurn {
        reply: Some(reply),
        evidence: AmbientEvidence::ExplicitEnd,
    }
}

fn receiver(limits: TurnOptions) -> (AmbientReceiver, crossbeam_channel::Receiver<()>) {
    let (cancel, cancelled) = crossbeam_channel::bounded(1);
    let shared = Arc::new(Shared::new(limits, cancel));
    (AmbientReceiver { shared }, cancelled)
}

fn active() -> Option<AmbientStatus> {
    Some(AmbientStatus::Active {
        unrecognised: false,
        oversized_lines: 0,
    })
}

#[test]
fn a_new_receiver_is_starting_and_empty() {
    let (rx, _) = receiver(TurnOptions::default());
    assert_eq!(
        rx.status(),
        AmbientStatus::Waiting(AmbientWaiting::Starting)
    );
    assert!(rx.try_recv().is_none());
}

#[test]
fn turns_come_out_in_order_after_their_status() {
    let (rx, _) = receiver(TurnOptions {
        queue_bytes: 2,
        reply_bytes: 2,
        ..TurnOptions::default()
    });
    assert!(rx.shared.publish(active(), vec![turn("a"), turn("b")]));
    assert_eq!(rx.status(), active().unwrap());
    let texts: Vec<String> = std::iter::from_fn(|| rx.try_recv())
        .map(|t| t.reply.unwrap().text)
        .collect();
    assert_eq!(texts, ["a", "b"]);
    assert!(
        rx.shared.publish(None, vec![turn("cd")]),
        "both bytes available again"
    );
}

#[test]
fn a_turn_beyond_the_item_budget_closes_with_an_overflow_and_keeps_the_rest() {
    let limits = TurnOptions {
        queue_items: 2,
        ..TurnOptions::default()
    };
    let (rx, _) = receiver(limits);
    assert!(
        !rx.shared
            .publish(active(), vec![turn("a"), turn("b"), turn("c")])
    );
    assert_eq!(rx.status(), AmbientStatus::Closed(AmbientClosed::Overflow));
    assert!(!rx.shared.publish(None, vec![turn("d")]), "closed is final");
    let texts: Vec<String> = std::iter::from_fn(|| rx.try_recv())
        .map(|t| t.reply.unwrap().text)
        .collect();
    assert_eq!(texts, ["a", "b"], "queued turns stay available");
}

#[test]
fn a_turn_beyond_the_byte_budget_closes_with_an_overflow() {
    let limits = TurnOptions {
        queue_bytes: 5,
        reply_bytes: 5,
        ..TurnOptions::default()
    };
    let (rx, _) = receiver(limits);
    assert!(rx.shared.publish(active(), vec![turn("abc")]));
    // A turn without text costs no bytes.
    let empty = AmbientTurn {
        reply: None,
        evidence: AmbientEvidence::InferredAfterQuiet,
    };
    assert!(rx.shared.publish(None, vec![empty]));
    assert!(!rx.shared.publish(None, vec![turn("def")]));
    assert_eq!(rx.status(), AmbientStatus::Closed(AmbientClosed::Overflow));
    assert_eq!(rx.try_recv().unwrap().reply.unwrap().text, "abc");
    assert!(rx.try_recv().unwrap().reply.is_none());
    assert!(rx.try_recv().is_none());
}

#[test]
fn stop_discards_wakes_the_worker_and_nothing_is_published_after_it() {
    let (rx, cancelled) = receiver(TurnOptions::default());
    assert!(rx.shared.publish(active(), vec![turn("a")]));
    rx.stop();
    assert!(cancelled.try_recv().is_ok(), "the worker is woken");
    assert_eq!(rx.status(), AmbientStatus::Closed(AmbientClosed::Stopped));
    assert!(rx.try_recv().is_none(), "queued turns are discarded");
    assert!(!rx.shared.publish(active(), vec![turn("late")]));
    assert!(rx.try_recv().is_none());
    rx.shared.close(AmbientClosed::TerminalEnded);
    assert_eq!(
        rx.status(),
        AmbientStatus::Closed(AmbientClosed::Stopped),
        "the first reason stays"
    );
    rx.stop();
}

#[test]
fn stop_after_a_close_keeps_the_reason_and_discards() {
    let (rx, _) = receiver(TurnOptions::default());
    assert!(rx.shared.publish(active(), vec![turn("a")]));
    rx.shared.close(AmbientClosed::TerminalEnded);
    assert_eq!(
        rx.status(),
        AmbientStatus::Closed(AmbientClosed::TerminalEnded)
    );
    rx.stop();
    assert_eq!(
        rx.status(),
        AmbientStatus::Closed(AmbientClosed::TerminalEnded)
    );
    assert!(rx.try_recv().is_none());
}

#[test]
fn dropping_the_receiver_stops_it() {
    let (rx, cancelled) = receiver(TurnOptions::default());
    let shared = Arc::clone(&rx.shared);
    drop(rx);
    assert!(cancelled.try_recv().is_ok());
    assert!(!shared.live());
}

#[test]
fn debug_never_shows_reply_text() {
    let shown = format!("{:?}", turn("secret reply"));
    assert!(!shown.contains("secret"), "{shown}");
    assert!(shown.contains("bytes"), "{shown}");
    let (rx, _) = receiver(TurnOptions::default());
    assert!(rx.shared.publish(active(), vec![turn("secret reply")]));
    let shown = format!("{rx:?}");
    assert!(!shown.contains("secret"), "{shown}");
}

#[test]
fn options_are_validated() {
    assert!(AmbientOptions::default().validate().is_ok());
    let invalid = |change: fn(&mut AmbientOptions)| {
        let mut options = AmbientOptions::default();
        change(&mut options);
        options.validate()
    };
    assert_eq!(
        invalid(|o| o.poll = Duration::ZERO),
        Err(AttachError::InvalidLimits)
    );
    assert_eq!(
        invalid(|o| o.poll = Duration::from_secs(61)),
        Err(AttachError::InvalidLimits)
    );
    assert_eq!(
        invalid(|o| o.quiet = Duration::from_millis(9)),
        Err(AttachError::InvalidLimits)
    );
    assert_eq!(
        invalid(|o| o.quiet = Duration::from_secs(3601)),
        Err(AttachError::InvalidLimits)
    );
    assert_eq!(
        invalid(|o| o.limits.queue_items = 0),
        Err(AttachError::InvalidLimits)
    );
    assert_eq!(
        invalid(|o| o.limits.read_bytes = TurnOptions::default().read_bytes + 1),
        Err(AttachError::InvalidLimits)
    );
    assert_eq!(
        invalid(|o| {
            o.limits.queue_bytes = 10;
            o.limits.reply_bytes = 11;
        }),
        Err(AttachError::InvalidLimits)
    );
    assert!(
        invalid(|o| {
            o.poll = Duration::from_millis(10);
            o.quiet = Duration::from_millis(10);
        })
        .is_ok()
    );
}

/// The process-wide budget: bounded settings, and a permit is held until dropped.
#[test]
fn the_worker_budget_is_bounded_and_released() {
    assert_eq!(set_ambient_worker_limit(0), Err(AttachError::InvalidLimits));
    assert_eq!(
        set_ambient_worker_limit(MAX_AMBIENT_WORKERS + 1),
        Err(AttachError::InvalidLimits)
    );
    let before = ambient_worker_limit();
    let running = ambient_workers();
    set_ambient_worker_limit(running + 1).unwrap();
    let permit = Permit::acquire().unwrap();
    assert_eq!(ambient_workers(), running + 1);
    assert!(matches!(Permit::acquire(), Err(AttachError::Busy)));
    drop(permit);
    assert_eq!(ambient_workers(), running);
    let again = Permit::acquire();
    set_ambient_worker_limit(before).unwrap();
    assert!(again.is_ok());
}

#[test]
fn the_context_reads_the_agents_environment() {
    let mut env = EnvPolicy {
        inherit: false,
        ..EnvPolicy::default()
    };
    env.set("TOOL_HOME", "/cfg").set("GONE", "x").remove("GONE");
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let ctx = AmbientContext::new("/project", env.clone());
    assert_eq!(ctx.var("TOOL_HOME"), Some("/cfg".into()));
    assert_eq!(ctx.var("GONE"), None);
    assert_eq!(ctx.var("PATH"), None, "nothing inherited");
    assert_eq!(ctx.home(), None);
    env.set(home, "");
    assert_eq!(AmbientContext::new("/p", env.clone()).home(), None, "empty");
    env.set(home, "/home/me");
    assert_eq!(
        AmbientContext::new("/p", env).home(),
        Some(PathBuf::from("/home/me"))
    );
    // An inheriting policy falls back to this process's value.
    let inherit = AmbientContext::new("/p", EnvPolicy::default());
    assert_eq!(inherit.var("PATH"), std::env::var_os("PATH"));
}

#[test]
fn evidence_and_status_helpers() {
    assert!(AmbientEvidence::ExplicitEnd.is_explicit());
    assert!(!AmbientEvidence::InferredAfterQuiet.is_explicit());
    assert!(AmbientStatus::Closed(AmbientClosed::Monitor).is_closed());
    assert!(!AmbientStatus::Waiting(AmbientWaiting::NoAgent).is_closed());
}
