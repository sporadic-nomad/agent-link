//! A PTY session with its screen model: the model sees each chunk before any receiver,
//! the tap's signals arrive as events after their `Output`, resize reaches the model, and
//! query replies reach the child only when the session answers queries.
//!
//! The shell's output is computed (`hel''lo`, `$((…))`) so the tty's echo of the typed
//! line never matches what a test waits for.
#![cfg(all(unix, feature = "screen"))]

mod common;

use agent_link::screen::{ScreenModel, Vt100Model};
use agent_link::{Event, PtySize, SpawnSpec};
use common::{deadline, output_until, recv_until, sh_spec, start, start_sh, wait_until};

#[test]
fn the_screen_holds_what_the_shell_printed() {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = start_sh(home.path());
    term.write(b"printf 'hel''lo-%s\\n' $((40+2))\n").unwrap();
    output_until(&events, "hello-42");
    // The model processed every chunk before it was published.
    let screen = term.screen().expect("screen on by default");
    assert!(screen.text().contains("hello-42"), "{}", screen.text());
    assert_eq!(screen.size, PtySize { rows: 24, cols: 80 });
    assert_eq!(screen.rows.len(), 24);

    // The repaint stream rebuilds the same screen elsewhere (retried: the shell may still
    // print between the two reads).
    wait_until("the repaint to rebuild the screen", || {
        let now = term.screen().unwrap();
        let mut copy = Vt100Model::new(now.size, 0, None);
        copy.process(&term.repaint_stream().unwrap());
        copy.snapshot().rows == now.rows
    });
}

#[test]
fn a_title_arrives_as_an_event_after_its_output() {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = start_sh(home.path());
    term.write(b"printf '\\033]0;agent-link-%s\\007' title\n")
        .unwrap();
    // Accumulated: a read may split the sequence.
    let mut output = Vec::new();
    recv_until(&events, deadline(), |event| match event {
        Event::Output { bytes, .. } => {
            output.extend_from_slice(bytes);
            false
        }
        Event::Title { id, title } => {
            assert_eq!(*id, term.id());
            assert!(
                String::from_utf8_lossy(&output).contains("\x1b]0;agent-link-title"),
                "the Title came before its Output"
            );
            title == "agent-link-title"
        }
        _ => false,
    });
    assert_eq!(term.screen().unwrap().title, "agent-link-title");
}

#[test]
fn resize_reaches_the_model() {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = start_sh(home.path());
    let size = PtySize {
        rows: 30,
        cols: 100,
    };
    term.resize(size).unwrap();
    let screen = term.screen().unwrap();
    assert_eq!((screen.size, screen.rows.len()), (size, 30));
    // The shell sees the same size.
    term.write(b"stty size; echo done-$((6*7))\n").unwrap();
    let text = output_until(&events, "done-42");
    assert!(text.contains("30 100"), "{text:?}");
}

/// Every `Output` a receiver gets was processed by the model first: with the receiver
/// subscribed at spawn, chunk `seq` is the `seq + 1`-th, so the model's generation is at
/// least that.
#[test]
fn output_is_published_only_after_the_model_processed_it() {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = start_sh(home.path());
    term.write(
        b"i=0; while [ $i -lt 300 ]; do echo line-$i; i=$((i+1)); done; echo end-$((1+1))\n",
    )
    .unwrap();
    let mut text = String::new();
    let mut chunks = 0;
    recv_until(&events, deadline(), |event| {
        let Event::Output { seq, bytes, .. } = event else {
            return false;
        };
        chunks += 1;
        let generation = term.screen().unwrap().generation;
        assert!(
            generation > *seq,
            "chunk {seq} published at generation {generation}"
        );
        text.push_str(&String::from_utf8_lossy(bytes));
        text.contains("end-2")
    });
    assert!(chunks > 1, "one chunk proves little");
}

#[test]
fn a_session_without_a_model_has_no_screen_and_no_signal_events() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = sh_spec(home.path());
    spec.screen.enabled = false;
    let (term, events) = start(spec);
    assert!(term.screen().is_none());
    assert!(term.repaint_stream().is_none());
    term.write(b"printf '\\033]0;x\\007\\033[?2026h\\033[?2026l'; echo end-$((2+2))\n")
        .unwrap();
    recv_until(&events, deadline(), |event| match event {
        Event::Output { bytes, .. } => String::from_utf8_lossy(bytes).contains("end-4"),
        other => panic!("only output without a model, got {other:?}"),
    });
}

/// A child in raw mode asks for a status report and reads up to 4 bytes for half a second.
fn status_reply(answer_queries: bool) -> String {
    let home = tempfile::tempdir().unwrap();
    let script = "stty raw -echo min 0 time 5; printf '\\033[5n'; \
                  r=$(dd bs=1 count=4 2>/dev/null | tr '\\033' E); stty sane; \
                  printf 'reply:[%s]\\n' \"$r\"";
    let size = PtySize { rows: 24, cols: 80 };
    let mut spec = SpawnSpec::direct("/bin/sh", ["-c", script], home.path(), size);
    spec.screen.answer_queries = answer_queries;
    let (term, events) = common::spawn(spec).unwrap();
    let text = output_until(&events, "]");
    drop(term);
    let start = text.find("reply:[").expect("the reply line") + "reply:[".len();
    text[start..]
        .split(']')
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn queries_are_answered_only_when_asked_to() {
    assert_eq!(status_reply(true), "E[0n");
    assert_eq!(status_reply(false), "");
}
