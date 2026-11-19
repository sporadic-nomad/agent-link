//! One pull-driven transcript reader on its own worker. No parser, resolver, file or
//! provider-owned destructor runs on a controller thread or under a publication lock.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crossbeam_channel::{bounded, select_biased};

use super::stream::{Batch, Stream};
use super::transcript::{MAX_NEW_BYTES, Pass, Prefix, Tail};
use super::{
    AttachError, Binding, Controller, Gap, Launch, Limits, Opened, Options, Parser, Permit, Record,
    Source, Status, TurnReceiver,
};

pub(super) fn spawn(
    ctl: &Arc<Controller>,
    launch: Launch,
    source: impl Source,
    parser: impl Parser,
    options: Options,
    permit: Permit,
) -> Result<TurnReceiver, AttachError> {
    let (cancel, cancelled) = bounded(1);
    let closed = ctl.term().closed_signal();
    let stream = Arc::new(Stream::new(ctl, &launch, options.limits, cancel));
    let launch = Arc::new(launch);
    let witness = source.witness();
    let receiver = TurnReceiver::new(stream.clone(), &launch, &witness);
    std::thread::Builder::new()
        .name(format!("agent-link-turns-{}", launch.session))
        .spawn(move || {
            let _permit = permit;
            // Keep the slot through provider reset, destruction and unwind as well as I/O.
            let (mut source, mut parser, _witness) = (source, parser, witness);
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut reader = Reader::new(options.limits);
                #[cfg(feature = "turns")]
                let mut snapshots = source
                    .snapshot_marker()
                    .map(|marker| super::snapshots::Reader::new(marker, options.limits));
                while stream.sample_terminal(reader.bound.is_some()) {
                    #[cfg(feature = "turns")]
                    let pass = if let Some(snapshots) = &mut snapshots {
                        snapshots.poll(&launch, &mut source, &mut parser, &stream)
                    } else {
                        reader.poll(&launch, &mut source, &mut parser, &stream)
                    };
                    #[cfg(not(feature = "turns"))]
                    let pass = reader.poll(&launch, &mut source, &mut parser, &stream);
                    match pass {
                        Ok(Pass::VerifyMore) => continue,
                        Ok(Pass::Complete | Pass::RetryOnTick) => {}
                        Err(reason) => {
                            // Publish loss before closing the file or dropping parser state.
                            stream.finish(Status::Gap(reason));
                            break;
                        }
                    }
                    select_biased! {
                        recv(cancelled) -> _ => break,
                        recv(closed) -> _ => break,
                        recv(options.ticks) -> tick => if tick.is_err() { break },
                    }
                }
            }));
            stream.finish(if result.is_err() {
                Status::Gap(Gap::WorkerPanic)
            } else {
                Status::Closed
            });
            parser.reset();
        })
        .map_err(|_| AttachError::Spawn)?;
    Ok(receiver)
}

struct Reader {
    opening: Option<Opened>,
    bound: Option<(Tail, Framer)>,
    pending: Option<Candidate>,
    buf: Vec<u8>,
    limits: Limits,
    last_growth: std::time::Instant,
}

struct Candidate {
    used: usize,
    // End of the last newline-terminated record in this batch.
    complete_bytes: usize,
    replies: Batch,
    prefix: Prefix,
}

impl Reader {
    fn new(limits: Limits) -> Self {
        Self {
            opening: None,
            bound: None,
            pending: None,
            buf: vec![0; limits.read_bytes.min(MAX_NEW_BYTES)],
            limits,
            last_growth: std::time::Instant::now(),
        }
    }

    fn poll(
        &mut self,
        launch: &Launch,
        source: &mut impl Source,
        parser: &mut impl Parser,
        stream: &Stream,
    ) -> Result<Pass, Gap> {
        let Some((tail, framer)) = &mut self.bound else {
            let mut opened = match self.opening.take().map_or_else(|| source.open(launch), Ok) {
                Ok(opened) => opened,
                // A missing path is retryable only before the first open.
                Err(Gap::Missing) => {
                    launch.claim.root.check_name()?;
                    return Ok(Pass::Complete);
                }
                Err(reason) => return Err(reason),
            };
            if !stream.status().live() {
                return Ok(Pass::Complete);
            }
            opened.confirm(launch, source.current(launch)?)?;
            if !source.header_ready(&mut opened)? {
                self.opening = Some(opened);
                return Ok(Pass::Complete);
            }
            let tail = Tail::at_eof(opened)?;
            tail.opened.confirm(launch, source.current(launch)?)?;
            let framer = Framer::new(self.limits, tail.mid_record());
            if stream.sample_terminal(true) && stream.bind(framer.status(&tail), &tail.opened) {
                self.bound = Some((tail, framer));
            }
            return Ok(Pass::Complete);
        };
        let (mut candidate, prefix_budget) = if let Some(pending) = self.pending.take() {
            (pending, self.buf.len())
        } else {
            tail.opened.confirm(launch, source.current(launch)?)?;
            let n = tail.read(&mut self.buf)?;
            if !stream.status().live() {
                return Ok(Pass::Complete);
            }
            if n == 0 {
                if parser.has_quiet_candidate()
                    && framer.turn.armed
                    && !framer.skip
                    && framer.partial.is_empty()
                    && self.last_growth.elapsed() >= std::time::Duration::from_secs(3)
                {
                    tail.opened.confirm(launch, source.current(launch)?)?;
                    if stream.sample_terminal(true) && tail.read(&mut self.buf)? == 0 {
                        if let Some(reply) = parser.quiet() {
                            let mut batch = Batch::new();
                            stream.stage(&mut batch, reply)?;
                            framer.turn.armed = false;
                            stream.commit(framer.status(tail), tail.cursor, batch);
                        }
                    }
                }
                return Ok(Pass::Complete);
            }
            self.last_growth = std::time::Instant::now();
            let candidate = framer.parse(&tail.opened.binding, &self.buf[..n], parser, stream)?;
            (candidate, self.buf.len().min(MAX_NEW_BYTES - n))
        };
        if !stream.status().live() {
            return Ok(Pass::Complete);
        }
        tail.opened.confirm(launch, source.current(launch)?)?;
        let bytes = &self.buf[..candidate.used];
        let mut pass =
            tail.validate_batch(&mut candidate.prefix, &framer.partial, bytes, prefix_budget)?;
        if matches!(pass, Pass::Complete) {
            if !stream.sample_terminal(true) {
                return Ok(Pass::Complete);
            }
            tail.opened.confirm(launch, source.current(launch)?)?;
            pass = tail.confirm_version(&mut candidate.prefix)?;
        }
        if !matches!(pass, Pass::Complete) {
            self.pending = Some(candidate);
        } else {
            framer.advance(bytes, candidate.complete_bytes);
            tail.cursor.advance(bytes);
            stream.commit(framer.status(tail), tail.cursor, candidate.replies);
        }
        Ok(pass)
    }
}

/// Require a complete post-baseline prompt before accepting its completion.
#[derive(Default)]
pub(super) struct TurnState {
    armed: bool,
    pub(super) incomplete: u32,
}

impl TurnState {
    pub(super) fn apply(
        &mut self,
        record: Record,
        replies: &mut Batch,
        stream: &Stream,
    ) -> Result<(), Gap> {
        match record {
            Record::Ignore => {}
            Record::User => {
                if self.armed {
                    self.incomplete = self.incomplete.saturating_add(1);
                }
                self.armed = true;
            }
            #[cfg(feature = "turns")]
            Record::Next(reply) => {
                if self.armed {
                    if let Some(reply) = reply {
                        stream.stage(replies, reply)?;
                    } else {
                        self.incomplete = self.incomplete.saturating_add(1);
                    }
                }
                self.armed = true;
            }
            Record::End(reply) => {
                if std::mem::take(&mut self.armed) {
                    if let Some(reply) = reply {
                        stream.stage(replies, reply)?;
                    }
                }
            }
        }
        Ok(())
    }
}

struct Framer {
    limits: Limits,
    partial: Vec<u8>,
    skip: bool,
    turn: TurnState,
}

impl Framer {
    fn new(limits: Limits, skip: bool) -> Self {
        Self {
            limits,
            partial: Vec::with_capacity(limits.line_bytes),
            skip,
            turn: TurnState::default(),
        }
    }

    fn status(&self, tail: &Tail) -> Status {
        Status::Active {
            start: tail.start,
            offset: tail.cursor.offset,
            incomplete_turns: self.turn.incomplete,
        }
    }

    fn parse(
        &mut self,
        binding: &Binding,
        bytes: &[u8],
        parser: &mut impl Parser,
        stream: &Stream,
    ) -> Result<Candidate, Gap> {
        let mut replies = Batch::new();
        let retained = self.partial.len();
        let mut used = 0;
        let mut complete = 0;
        for segment in bytes
            .split_inclusive(|&b| b == b'\n')
            .take(self.limits.records)
        {
            if !stream.status().live() {
                break;
            }
            let first = used == 0;
            used += segment.len();
            let terminated = segment.ends_with(b"\n");
            if terminated {
                complete = used;
            }
            if self.skip {
                self.skip = !terminated;
                continue;
            }
            let text = &segment[..segment.len() - usize::from(terminated)];
            let prefix_len = if first { retained } else { 0 };
            if prefix_len + text.len() > self.limits.line_bytes {
                return Err(Gap::Oversized);
            }
            if !terminated {
                break;
            }
            let line = if prefix_len > 0 {
                // Keep the prefix intact until the candidate passes its final byte
                // comparisons. Later complete records borrow directly from the batch.
                self.partial.extend_from_slice(text);
                &self.partial
            } else {
                text
            };
            let record = parser.record(binding, line, self.limits.reply_bytes)?;
            self.turn.apply(record, &mut replies, stream)?;
        }
        Ok(Candidate {
            used,
            complete_bytes: complete,
            replies,
            prefix: Prefix::new(if complete > 0 { retained } else { 0 }),
        })
    }

    fn advance(&mut self, bytes: &[u8], complete: usize) {
        if complete > 0 {
            self.partial.clear();
        }
        if !self.skip {
            self.partial.extend_from_slice(&bytes[complete..]);
        }
    }
}
