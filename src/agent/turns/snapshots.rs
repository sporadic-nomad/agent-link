//! Cursor can reconstruct a transcript in place, dropping historical end markers.
//! Match every historical data record by position and SHA-256, including repeated
//! text. A surviving marker must agree with its prior observation. Only a new suffix
//! can publish. A short image during truncate-and-rewrite is held for a bounded time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::reader::TurnState;
use super::stream::{Batch, Position, Stream};
use super::transcript::{FileVersion, Pass};
use super::{Gap, Launch, Limits, Opened, Parser, Source, Status};

pub(in crate::agent) type Marker = fn(&[u8]) -> Result<bool, Gap>;
type Hash = [u8; 32];

#[derive(Clone)]
struct Row {
    data: Hash,
    end: Option<Hash>,
}

struct Snapshot {
    version: FileVersion,
    history: Vec<Row>,
    bytes: Vec<u8>,
}

impl Snapshot {
    fn scan(opened: &Opened, marker: Marker, limits: Limits) -> Result<Option<Self>, Gap> {
        // A host can lower read_bytes to even one byte. Bound both the total
        // snapshot and the number of synchronous reads for such a configuration.
        let limit = limits.line_bytes.min(limits.read_bytes * 16);
        let Some((bytes, version)) = opened.snapshot(limit, limits.read_bytes)? else {
            return Ok(None);
        };
        if !bytes.ends_with(b"\n") {
            return Ok(None);
        }
        let mut history: Vec<Row> = Vec::new();
        for (index, line) in bytes[..bytes.len() - 1].split(|&b| b == b'\n').enumerate() {
            if index >= limits.records {
                return Err(Gap::Oversized);
            }
            let fingerprint = Sha256::digest(line).into();
            if marker(line)? {
                let row = history.last_mut().ok_or(Gap::Format)?;
                if row.end.replace(fingerprint).is_some() {
                    return Err(Gap::Format);
                }
            } else {
                history.push(Row {
                    data: fingerprint,
                    end: None,
                });
            }
        }
        Ok(Some(Self {
            version,
            history,
            bytes,
        }))
    }
}

pub(super) struct Proof {
    rows: Vec<Row>,
    marker: Marker,
    limits: Limits,
}

impl Proof {
    fn agrees(&self, new: &Snapshot) -> Result<bool, Gap> {
        for (old, new) in self.rows.iter().zip(&new.history) {
            if old.data != new.data || matches!((old.end,new.end),(Some(a),Some(b)) if a!=b) {
                return Err(Gap::Rewritten);
            }
        }
        Ok(new.history.len() >= self.rows.len())
    }

    /// A pull must withhold queued data while a rewrite is still shorter than
    /// the previously verified history. The worker owns the retry deadline.
    pub(super) fn confirm(&self, opened: &Opened) -> Result<bool, Gap> {
        match Snapshot::scan(opened, self.marker, self.limits)? {
            Some(new) => self.agrees(&new),
            None => Ok(false),
        }
    }
}

pub(super) struct Reader {
    marker: Marker,
    limits: Limits,
    opened: Option<Arc<Opened>>,
    opening: Option<Opened>,
    proof: Option<Arc<Proof>>,
    version: Option<FileVersion>,
    pending: Option<(Arc<Proof>, FileVersion, Status, Batch)>,
    partial_since: Option<Instant>,
    start: u64,
    turn: TurnState,
}

impl Reader {
    pub(super) fn new(marker: Marker, limits: Limits) -> Self {
        Self {
            marker,
            limits,
            opened: None,
            opening: None,
            proof: None,
            version: None,
            pending: None,
            partial_since: None,
            start: 0,
            turn: TurnState::default(),
        }
    }

    fn rewriting(&mut self) -> Result<Pass, Gap> {
        if self
            .partial_since
            .get_or_insert_with(Instant::now)
            .elapsed()
            >= Duration::from_secs(3)
        {
            return Err(Gap::Truncated);
        }
        Ok(Pass::RetryOnTick)
    }

    pub(super) fn poll(
        &mut self,
        launch: &Launch,
        source: &mut impl Source,
        parser: &mut impl Parser,
        stream: &Stream,
    ) -> Result<Pass, Gap> {
        let opened = if let Some(opened) = &self.opened {
            opened.clone()
        } else {
            let mut opened = match self.opening.take().map_or_else(|| source.open(launch), Ok) {
                Ok(opened) => opened,
                Err(Gap::Missing) => {
                    launch.claim.root.check_name()?;
                    return Ok(Pass::Complete);
                }
                Err(gap) => return Err(gap),
            };
            opened.confirm(launch, source.current(launch)?)?;
            if !source.header_ready(&mut opened)? {
                self.opening = Some(opened);
                return Ok(Pass::Complete);
            }
            let opened = Arc::new(opened);
            self.opened = Some(opened.clone());
            opened
        };
        opened.confirm(launch, source.current(launch)?)?;
        if self.pending.is_some() {
            return self.publish(launch, source, stream, &opened);
        }
        if self.version == Some(opened.snapshot_version()?) {
            self.partial_since = None;
            return Ok(Pass::Complete);
        }
        let Some(mut new) = Snapshot::scan(&opened, self.marker, self.limits)? else {
            return self.rewriting();
        };
        let length = new.bytes.len() as u64;
        let mut batch = Batch::new();
        if let Some(previous) = &self.proof {
            if !previous.agrees(&new)? {
                return self.rewriting();
            }
            let old_count = previous.rows.len();
            let mut count = 0;
            for line in new.bytes[..new.bytes.len() - 1].split(|&b| b == b'\n') {
                let end = (self.marker)(line)?;
                if !end {
                    count += 1;
                }
                if count < old_count
                    || (count == old_count && (!end || previous.rows[count - 1].end.is_some()))
                {
                    continue;
                }
                let record = parser.record(&opened.binding, line, self.limits.reply_bytes)?;
                self.turn.apply(record, &mut batch, stream)?;
            }
            // A later rewrite can restore an earlier, already handled marker.
            for (row, old) in new.history.iter_mut().zip(&previous.rows) {
                if row.end.is_none() {
                    row.end = old.end;
                }
            }
        } else {
            self.start = length;
        }
        let proof = Arc::new(Proof {
            rows: new.history,
            marker: self.marker,
            limits: self.limits,
        });
        let status = Status::Active {
            start: self.start,
            offset: length,
            incomplete_turns: self.turn.incomplete,
        };
        self.pending = Some((proof, new.version, status, batch));
        self.publish(launch, source, stream, &opened)
    }

    fn publish(
        &mut self,
        launch: &Launch,
        source: &mut impl Source,
        stream: &Stream,
        opened: &Arc<Opened>,
    ) -> Result<Pass, Gap> {
        let (proof, version, status, batch) = self.pending.take().expect("staged snapshot");
        opened.confirm(launch, source.current(launch)?)?;
        // Keep both advanced parser state and the staged batch while a later
        // rewrite is short. Never parse the same message twice.
        if !proof.confirm(opened)? {
            self.pending = Some((proof, version, status, batch));
            return self.rewriting();
        }
        opened.confirm(launch, source.current(launch)?)?;
        if !stream.sample_terminal(true) {
            return Ok(Pass::Complete);
        }
        if self.proof.is_none() && !stream.bind(status, opened) {
            return Ok(Pass::Complete);
        }
        stream.commit_at(status, Position::Snapshot(proof.clone()), batch);
        self.proof = Some(proof);
        self.version = Some(version);
        self.partial_since = None;
        Ok(Pass::Complete)
    }
}
