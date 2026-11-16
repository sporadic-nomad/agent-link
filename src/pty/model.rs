//! A session's screen model slot: the reader feeds it every chunk before publishing the
//! chunk, `Terminal::screen` reads it, resize goes through it, teardown empties it. It
//! also counts newly read chunks before publication can block, notes the last one that
//! left a synchronized-output frame open (the agent controller's waits read both) and
//! the last one in which the child turned bracketed paste on (its shell fallback reads
//! that). Without the `screen` feature it holds nothing and every call is a no-op.
//!
//! Lock order: the slot's lock, then the PTY master's (resize) — never the reverse; the
//! slot's lock is never held during fanout. The bus recovery-boundary lock wraps model
//! processing plus log commit, and resize; ordinary screen reads need only this slot.

use super::input::Input;

use super::{Event, PtySize, ScreenConfig, SessionId};
use crate::error::Result;

#[cfg(feature = "screen")]
pub(crate) use with_screen::ModelSlot;
#[cfg(not(feature = "screen"))]
pub(crate) use without_screen::ModelSlot;

#[cfg(feature = "screen")]
mod with_screen {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};

    use super::*;
    use crate::screen::{ScreenModel, ScreenSnapshot, TapSignal, Vt100Model};

    pub(crate) struct ModelSlot {
        model: Mutex<Option<Box<dyn ScreenModel>>>,
        /// Chunks read so far, announced before waiting on publication or model locks:
        /// a screen read before this count holds no chunk past it. Numbered like the
        /// `Output` events a receiver subscribed before the first read gets (first is 1).
        fed: AtomicU64,
        /// Last chunk fully rendered; updated under `model`, unlike the reader's fence.
        rendered: AtomicU64,
        /// Invalidates screen observations on processing, resizing, and teardown.
        revision: AtomicU64,
        /// A synchronized-output frame is open after the last chunk (the tap's edges,
        /// replayed; only the reader writes it).
        in_frame: AtomicBool,
        /// The number of the last chunk after which a frame was still open; 0 for none.
        open_after: AtomicU64,
        /// The number of the last chunk that turned bracketed paste on; 0 for none.
        paste_on_at: AtomicU64,
        /// Finds that switch in the stream (only the reader locks it).
        paste_on: Mutex<PasteOnScan>,
    }

    impl ModelSlot {
        /// A model per `config` (none when disabled). When it answers queries it holds a
        /// clone of `writer`, the session's writer queue: [`clear`](Self::clear) must drop
        /// it in teardown, or the writer thread outlives the session.
        pub(crate) fn new(
            config: &ScreenConfig,
            id: SessionId,
            size: PtySize,
            writer: &Input,
        ) -> Self {
            let model = config.enabled.then(|| {
                let replies = config.answer_queries.then(|| writer.clone());
                let model = Vt100Model::for_session(id, size, config.scrollback_lines, replies);
                Box::new(model) as Box<dyn ScreenModel>
            });
            Self {
                model: Mutex::new(model),
                fed: AtomicU64::new(0),
                rendered: AtomicU64::new(0),
                revision: AtomicU64::new(0),
                in_frame: AtomicBool::new(false),
                open_after: AtomicU64::new(0),
                paste_on_at: AtomicU64::new(0),
                paste_on: Mutex::new(PasteOnScan::default()),
            }
        }

        fn lock(&self) -> MutexGuard<'_, Option<Box<dyn ScreenModel>>> {
            self.model.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Announce already-read output before waiting on publication or recovery locks.
        /// This invalidates old delivery observations even while rendering is blocked.
        /// The single reader must process this chunk before announcing another.
        pub(crate) fn announce_output(&self) -> u64 {
            self.fed.fetch_add(1, Ordering::SeqCst) + 1
        }

        /// Render a chunk previously counted by `announce_output`, exactly once and in
        /// read order. Returns derived events to publish after its `Output`. Rendering
        /// and log commit share the recovery boundary; announcement does not take it.
        pub(crate) fn process_announced(
            &self,
            id: SessionId,
            chunk: u64,
            bytes: &[u8],
        ) -> Vec<Event> {
            let paste_on = self
                .paste_on
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .feed(bytes);
            if paste_on {
                self.paste_on_at.store(chunk, Ordering::SeqCst);
            }
            let signals = {
                let mut model = self.lock();
                self.revision.fetch_add(1, Ordering::SeqCst);
                let signals = model
                    .as_mut()
                    .map_or_else(Vec::new, |model| model.process(bytes));
                self.rendered.store(chunk, Ordering::SeqCst);
                signals
            };
            // Frame begins are not published: whether this chunk left a frame open (it
            // ended one and began the next, say) is kept here instead.
            let mut in_frame = self.in_frame.load(Ordering::Relaxed);
            for signal in &signals {
                match signal {
                    TapSignal::FrameBegin => in_frame = true,
                    TapSignal::FrameEnd => in_frame = false,
                    _ => {}
                }
            }
            self.in_frame.store(in_frame, Ordering::Relaxed);
            if in_frame {
                self.open_after.store(chunk, Ordering::SeqCst);
            }
            signals
                .into_iter()
                .filter_map(|signal| event_for(id, signal))
                .collect()
        }

        /// Chunks read so far, including any waiting to render (see the field).
        #[cfg_attr(not(feature = "agent"), allow(dead_code))] // read by the agent controller
        pub(crate) fn fed(&self) -> u64 {
            self.fed.load(Ordering::SeqCst)
        }

        /// The number of the last chunk after which a synchronized-output frame was still
        /// open (0: none yet).
        #[cfg_attr(not(feature = "agent"), allow(dead_code))] // read by the agent controller
        pub(crate) fn open_after(&self) -> u64 {
            self.open_after.load(Ordering::SeqCst)
        }

        /// The number of the last chunk in which the child turned bracketed paste on (0:
        /// none yet) — counted like [`fed`](Self::fed), so a fence read from that count
        /// tells a switch after it.
        #[cfg_attr(not(feature = "agent"), allow(dead_code))] // read by the agent controller
        pub(crate) fn paste_on_at(&self) -> u64 {
            self.paste_on_at.load(Ordering::SeqCst)
        }

        /// Runs `resize_pty`, then resizes the model, both under the slot's lock: a reader
        /// of the screen never sees the new PTY size with the old layout.
        pub(crate) fn resize(
            &self,
            size: PtySize,
            resize_pty: impl FnOnce() -> Result<()>,
        ) -> Result<()> {
            let mut model = self.lock();
            self.revision.fetch_add(1, Ordering::SeqCst);
            resize_pty()?;
            if let Some(model) = model.as_mut() {
                model.resize(size);
            }
            Ok(())
        }

        pub(crate) fn snapshot(&self) -> Option<ScreenSnapshot> {
            self.lock().as_ref().map(|model| model.snapshot())
        }

        /// Screen, completed output fence, and model revision from the same locked read.
        /// The reader may already have counted a chunk which has not acquired this lock;
        /// `observation_current` rejects that gap instead of assigning it to an old screen.
        #[cfg(feature = "agent")]
        pub(crate) fn observation(&self) -> Option<(ScreenSnapshot, u64, u64)> {
            let model = self.lock();
            Some((
                model.as_ref()?.snapshot(),
                self.rendered.load(Ordering::SeqCst),
                self.revision.load(Ordering::SeqCst),
            ))
        }

        #[cfg(feature = "agent")]
        pub(crate) fn observation_current(&self, rendered: u64, revision: u64) -> bool {
            self.revision.load(Ordering::SeqCst) == revision && self.fed() == rendered
        }

        pub(crate) fn repaint_stream(&self) -> Option<Vec<u8>> {
            self.lock().as_ref().map(|model| model.repaint_stream())
        }

        /// Drops the model (and with it its writer clone); later calls see no model.
        pub(crate) fn clear(&self) {
            let mut model = self.lock();
            self.revision.fetch_add(1, Ordering::SeqCst);
            drop(model.take());
        }
    }

    #[cfg(all(test, feature = "agent"))]
    mod observation_tests {
        use super::*;
        use crate::pty::InputLimits;

        #[test]
        fn observations_reject_announced_but_unrendered_output_and_resizes() {
            let (input, _rx) = Input::new(InputLimits::default());
            let size = PtySize { rows: 5, cols: 20 };
            let slot = ModelSlot::new(&ScreenConfig::default(), SessionId(1), size, &input);
            slot.process(SessionId(1), b"old frame");
            let (screen, rendered, revision) = slot.observation().unwrap();
            assert!(screen.text().contains("old frame"));
            assert!(slot.observation_current(rendered, revision));

            // Pause the reader after it announces its next chunk and before it acquires
            // the model lock. A snapshot must still name the completed old chunk.
            let pending = slot.announce_output();
            let (_, still_rendered, still_revision) = slot.observation().unwrap();
            assert_eq!((still_rendered, still_revision), (rendered, revision));
            assert!(!slot.observation_current(still_rendered, still_revision));

            slot.process_announced(SessionId(1), pending, b" new frame");
            let (_, rendered, revision) = slot.observation().unwrap();
            assert!(slot.observation_current(rendered, revision));
            slot.resize(PtySize { rows: 6, cols: 21 }, || Ok(()))
                .unwrap();
            assert_eq!(slot.fed(), rendered, "resize is not a PTY chunk");
            assert!(!slot.observation_current(rendered, revision));
            let (_, rendered, revision) = slot.observation().unwrap();
            slot.clear();
            assert!(!slot.observation_current(rendered, revision));
            assert!(slot.observation().is_none());
        }
    }

    /// The event a signal is published as; frame begins and prompt marks are not
    /// published.
    fn event_for(id: SessionId, signal: TapSignal) -> Option<Event> {
        match signal {
            TapSignal::Title(title) => Some(Event::Title { id, title }),
            TapSignal::Progress(state) => Some(Event::Progress { id, state }),
            TapSignal::FrameEnd => Some(Event::Frame { id }),
            TapSignal::Reset(reason) => Some(Event::ScreenReset { id, reason }),
            TapSignal::FrameBegin | TapSignal::Prompt(_) => None,
        }
    }

    /// What a shell with bracketed paste (bash 5.1+, zsh, fish) writes as it starts reading
    /// each new line.
    const PASTE_ON: &[u8] = b"\x1b[?2004h";

    /// Finds [`PASTE_ON`] in a stream fed chunk by chunk, also when a read splits it. (`vt100`
    /// tracks the mode but reports no switch, and a snapshot cannot tell a mode a shell just
    /// set from one a crashed program left on.)
    #[derive(Default)]
    pub(super) struct PasteOnScan {
        /// The end of the stream so far: too short to hold the whole sequence.
        tail: Vec<u8>,
    }

    impl PasteOnScan {
        /// Whether the sequence ends in `bytes` (the next chunk).
        pub(super) fn feed(&mut self, bytes: &[u8]) -> bool {
            let keep = PASTE_ON.len() - 1;
            // Across the junction: the kept tail, then the start of this chunk.
            let mut junction = std::mem::take(&mut self.tail);
            junction.extend_from_slice(&bytes[..bytes.len().min(keep)]);
            let found = contains(&junction, PASTE_ON) || contains(bytes, PASTE_ON);
            // The last bytes of the stream: all a later chunk can complete.
            let end = if bytes.len() >= keep {
                bytes
            } else {
                &junction
            };
            self.tail = end[end.len().saturating_sub(keep)..].to_vec();
            found
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}

/// No model: nothing is kept and nothing derived.
#[cfg(not(feature = "screen"))]
mod without_screen {
    use super::*;

    pub(crate) struct ModelSlot;

    impl ModelSlot {
        pub(crate) fn new(_: &ScreenConfig, _: SessionId, _: PtySize, _: &Input) -> Self {
            Self
        }

        pub(crate) fn announce_output(&self) -> u64 {
            0
        }

        pub(crate) fn process_announced(&self, _: SessionId, _: u64, _: &[u8]) -> Vec<Event> {
            Vec::new()
        }

        pub(crate) fn resize(
            &self,
            _: PtySize,
            resize_pty: impl FnOnce() -> Result<()>,
        ) -> Result<()> {
            resize_pty()
        }

        pub(crate) fn clear(&self) {}
    }
}

#[cfg(test)]
impl ModelSlot {
    /// Tests without publication can announce and render in one step.
    pub(crate) fn process(&self, id: SessionId, bytes: &[u8]) -> Vec<Event> {
        self.process_announced(id, self.announce_output(), bytes)
    }
}

#[cfg(all(test, feature = "screen"))]
mod tests {
    use super::super::InputLimits;
    use super::*;
    use crate::pty::ProgressState;
    fn input() -> (Input, super::super::input::InputReceiver) {
        Input::new(InputLimits::default())
    }

    const ID: SessionId = SessionId(9);
    const SIZE: PtySize = PtySize { rows: 5, cols: 20 };

    fn config(enabled: bool, answer_queries: bool) -> ScreenConfig {
        ScreenConfig {
            enabled,
            scrollback_lines: 10,
            answer_queries,
        }
    }

    #[test]
    fn signals_become_events_after_processing() {
        let (tx, _rx) = input();
        let slot = ModelSlot::new(&config(true, true), ID, SIZE, &tx);
        let events = slot.process(
            ID,
            b"\x1b]0;t\x07\x1b]9;4;3\x07\x1b[?2026hx\x1b[?2026l\x1b]133;A\x07\x1b]agent-link-test-panic\x07",
        );
        assert_eq!(
            events,
            [
                Event::Title {
                    id: ID,
                    title: "t".into()
                },
                Event::Progress {
                    id: ID,
                    state: ProgressState::Indeterminate
                },
                Event::Frame { id: ID },
                Event::ScreenReset {
                    id: ID,
                    reason: "test panic inside the parser".into()
                },
            ]
        );
        assert_eq!(slot.snapshot().unwrap().title, "t");
    }

    /// Every chunk is counted, with or without a model; a chunk that ends one frame and
    /// begins the next leaves one open, as does a chunk inside a frame.
    #[test]
    fn chunks_are_counted_and_an_open_frame_is_noted() {
        let (tx, _rx) = input();
        let slot = ModelSlot::new(&config(true, false), ID, SIZE, &tx);
        assert_eq!((slot.fed(), slot.open_after()), (0, 0));
        slot.process(ID, b"\x1b[?2026hframe\x1b[?2026l");
        assert_eq!((slot.fed(), slot.open_after()), (1, 0), "closed again");
        let events = slot.process(ID, b"\x1b[?2026hone\x1b[?2026l\x1b[?2026htw");
        assert_eq!(events, [Event::Frame { id: ID }]);
        assert_eq!(
            (slot.fed(), slot.open_after()),
            (2, 2),
            "the next one is open"
        );
        slot.process(ID, b"o");
        assert_eq!((slot.fed(), slot.open_after()), (3, 3), "still inside it");
        slot.process(ID, b"\x1b[?2026l");
        assert_eq!((slot.fed(), slot.open_after()), (4, 3));
        let off = ModelSlot::new(&config(false, false), ID, SIZE, &tx);
        off.process(ID, b"x");
        assert_eq!(off.fed(), 1);
    }

    /// The last chunk that turned bracketed paste on is noted — also when two reads split
    /// the sequence, or the chunk turns it off again after (bash: `?2004l` … `?2004h` in one
    /// write); a chunk that only turns it off is not.
    #[test]
    fn the_chunk_turning_bracketed_paste_on_is_noted() {
        let (tx, _rx) = input();
        let slot = ModelSlot::new(&config(true, false), ID, SIZE, &tx);
        slot.process(ID, b"$ ");
        assert_eq!(slot.paste_on_at(), 0);
        slot.process(
            ID,
            b"^C\x1b[?2004l\r\x1b[?2004h\x1b[?2004l\r\r\n\x1b[?2004h$ ",
        );
        assert_eq!(slot.paste_on_at(), 2);
        slot.process(ID, b"\x1b[?2004l");
        assert_eq!(slot.paste_on_at(), 2, "turned off");
        slot.process(ID, b"\r\n\x1b[?20");
        assert_eq!(slot.paste_on_at(), 2);
        slot.process(ID, b"04h$ ");
        assert_eq!(slot.paste_on_at(), 5, "split across two reads");
        for (n, byte) in b"\x1b[?2004h".iter().enumerate() {
            slot.process(ID, std::slice::from_ref(byte));
            let chunk = 6 + n as u64;
            assert_eq!(slot.paste_on_at(), if n == 7 { chunk } else { 5 });
        }
        slot.process(ID, b"\x1b[?2004l\x1b[?2004");
        slot.process(ID, b"l");
        assert_eq!(slot.paste_on_at(), 13);
    }

    #[test]
    fn a_disabled_model_is_none() {
        let (tx, _rx) = input();
        let slot = ModelSlot::new(&config(false, true), ID, SIZE, &tx);
        assert!(slot.process(ID, b"\x1b]0;t\x07").is_empty());
        assert!(slot.snapshot().is_none());
        assert!(slot.repaint_stream().is_none());
        slot.resize(PtySize { rows: 2, cols: 2 }, || Ok(()))
            .unwrap();
    }

    /// The model's reply sender is the only writer clone besides the session's: once the
    /// slot is cleared, the writer queue disconnects when the session drops its sender.
    #[test]
    fn clear_drops_the_writer_clone() {
        let (tx, rx) = input();
        let slot = ModelSlot::new(&config(true, true), ID, SIZE, &tx);
        slot.process(ID, b"\x1b[5n");
        assert_eq!(rx.rx.try_recv().unwrap().bytes, b"\x1b[0n");
        slot.clear();
        drop(tx);
        assert!(rx.rx.recv().is_err(), "every sender is gone");
        assert!(slot.snapshot().is_none());
        assert!(slot.process(ID, b"x").is_empty());
    }

    #[test]
    fn answering_off_keeps_no_writer_clone() {
        let (tx, rx) = input();
        let slot = ModelSlot::new(&config(true, false), ID, SIZE, &tx);
        slot.process(ID, b"\x1b[5n\x1b[c");
        drop(tx);
        assert!(rx.rx.recv().is_err(), "no reply and no sender left");
        assert!(slot.snapshot().is_some());
    }

    #[test]
    fn a_failed_pty_resize_leaves_the_model() {
        let (tx, _rx) = input();
        let slot = ModelSlot::new(&config(true, false), ID, SIZE, &tx);
        let err = slot
            .resize(PtySize { rows: 9, cols: 9 }, || Err(crate::Error::Closed))
            .unwrap_err();
        assert!(matches!(err, crate::Error::Closed));
        assert_eq!(slot.snapshot().unwrap().size, SIZE);
        slot.resize(PtySize { rows: 9, cols: 9 }, || Ok(()))
            .unwrap();
        assert_eq!(slot.snapshot().unwrap().size, PtySize { rows: 9, cols: 9 });
    }
}
