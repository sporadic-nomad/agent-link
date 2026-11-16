//! [`SessionRegistry`]: terminals by key, at most one per key.

use std::collections::HashMap;
use std::hash::Hash;

use super::Terminal;

/// Terminals by key, at most one per key — e.g. one per UI pane, which enforces the host
/// rule that a pane owns one child: inserting under a taken key hands the previous
/// terminal back.
///
/// Dropping a [`Terminal`] kills its tree and reaps it, which blocks the dropping thread
/// (the reap is bounded at about 2 s; on Windows add a `taskkill` round trip without a Job
/// Object, or with one a process snapshot and a job query, plus `taskkill` when the shell
/// has a child outside the job or that check fails). Take terminals out under your lock —
/// `insert`'s return value, `remove`, `drain` — and drop them after releasing it.
#[derive(Debug)]
pub struct SessionRegistry<K> {
    map: HashMap<K, Terminal>,
}

impl<K> Default for SessionRegistry<K> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash> SessionRegistry<K> {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `terminal` under `key` and returns the terminal it replaces, if any. Dropping
    /// that one kills it.
    #[must_use = "dropping the evicted terminal kills it right here; drop it outside your locks"]
    pub fn insert(&mut self, key: K, terminal: Terminal) -> Option<Terminal> {
        self.map.insert(key, terminal)
    }

    /// The terminal under `key`.
    pub fn get(&self, key: &K) -> Option<&Terminal> {
        self.map.get(key)
    }

    /// Takes the terminal under `key` out; dropping it kills it.
    #[must_use = "dropping the removed terminal kills it right here; drop it outside your locks"]
    pub fn remove(&mut self, key: &K) -> Option<Terminal> {
        self.map.remove(key)
    }

    /// Takes every terminal out, in no particular order; each one is killed when dropped.
    pub fn drain(&mut self) -> impl Iterator<Item = (K, Terminal)> + '_ {
        self.map.drain()
    }

    /// How many terminals are stored.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
