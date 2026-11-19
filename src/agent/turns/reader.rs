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
                while stream.sample_terminal(reader.bound.is_some()) {
                    match reader.poll(&launch, &mut source, &mut parser, &stream) {
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
            if !stream.status().live() || n == 0 {
                return Ok(Pass::Complete);
            }
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

struct Framer {
    limits: Limits,
    partial: Vec<u8>,
    skip: bool,
    armed: bool,
    incomplete: u32,
}

impl Framer {
    fn new(limits: Limits, skip: bool) -> Self {
        Self {
            limits,
            partial: Vec::with_capacity(limits.line_bytes),
            skip,
            armed: false,
            incomplete: 0,
        }
    }

    fn status(&self, tail: &Tail) -> Status {
        Status::Active {
            start: tail.start,
            offset: tail.cursor.offset,
            incomplete_turns: self.incomplete,
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
            match parser.record(binding, line, self.limits.reply_bytes)? {
                Record::Ignore => {}
                Record::User => {
                    if self.armed {
                        self.incomplete = self.incomplete.saturating_add(1);
                    }
                    self.armed = true;
                }
                Record::End(reply) => {
                    if std::mem::take(&mut self.armed) {
                        if let Some(reply) = reply {
                            stream.stage(&mut replies, reply)?;
                        }
                    }
                }
            }
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
