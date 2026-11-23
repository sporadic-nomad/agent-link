//! One attachment's capacity follows each turn from staging through dequeue. Dropping
//! speculative history or an evicted file returns its reservations to the same pool.
use std::sync::{Arc, Mutex};

use super::{AmbientTurn, TurnOptions};

#[derive(Debug)]
struct Room {
    items: usize,
    bytes: usize,
}

#[derive(Clone, Debug)]
pub(super) struct Capacity {
    pub(super) limits: TurnOptions,
    room: Arc<Mutex<Room>>,
}

impl Capacity {
    pub(super) fn new(limits: TurnOptions) -> Self {
        Self {
            limits,
            room: Arc::new(Mutex::new(Room {
                items: limits.queue_items,
                bytes: limits.queue_bytes,
            })),
        }
    }

    /// A cooperating feed can return this many maximum-size candidates at once.
    /// Even a full queue must sample one event to distinguish a duplicate or an
    /// empty poll from a new turn that overflows.
    pub(super) fn feed_events(&self) -> usize {
        let room = self.room.lock().unwrap_or_else(|e| e.into_inner());
        room.items.min(room.bytes / self.limits.reply_bytes).max(1)
    }

    fn admit(&self, mut turn: AmbientTurn) -> Option<StagedTurn> {
        // A custom parser or feed can return a short string with arbitrary spare capacity.
        let bytes = turn.reply.as_mut().map_or(0, |reply| {
            let text = std::mem::take(&mut reply.text);
            reply.text = text.into_boxed_str().into_string();
            reply.text.capacity()
        });
        let mut room = self.room.lock().unwrap_or_else(|e| e.into_inner());
        if room.items == 0 || room.bytes < bytes {
            return None;
        }
        room.items -= 1;
        room.bytes -= bytes;
        Some(StagedTurn {
            turn,
            _reservation: Reservation {
                room: self.room.clone(),
                bytes,
            },
        })
    }
}

#[derive(Debug)]
struct Reservation {
    room: Arc<Mutex<Room>>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut room = self.room.lock().unwrap_or_else(|e| e.into_inner());
        room.items += 1;
        room.bytes += self.bytes;
    }
}

#[derive(Debug)]
pub(super) struct StagedTurn {
    pub(super) turn: AmbientTurn,
    _reservation: Reservation,
}

#[derive(Default, Debug)]
pub(super) struct TurnBatch {
    pub(super) turns: Vec<StagedTurn>,
    pub(super) overflow: bool,
}

impl TurnBatch {
    pub(super) fn push(&mut self, turn: AmbientTurn, capacity: &Capacity) {
        if !self.overflow {
            match capacity.admit(turn) {
                Some(staged) => self.turns.push(staged),
                None => self.overflow = true,
            }
        }
    }

    pub(super) fn append(&mut self, mut later: Self) {
        if !self.overflow {
            self.turns.append(&mut later.turns);
            self.overflow = later.overflow;
        }
    }
}
