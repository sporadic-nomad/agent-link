use std::collections::HashMap;
use std::time::Instant;

use crate::{Error, Limits, Result, RuntimeId};

/// Process-local attachment capability. Bound to the owning epoch and incarnation;
/// IPC hosts must also bind it to the authenticated connection. Not a resume token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    pub(crate) runtime: RuntimeId,
    pub(crate) epoch: u64,
    pub(crate) serial: u64,
}

pub(crate) struct Leases {
    runtime: RuntimeId,
    epoch: u64,
    serial: u64,
    limits: Limits,
    entries: HashMap<u64, Instant>,
    detached: Instant,
    last_now: Instant,
}

impl Leases {
    pub fn new(runtime: RuntimeId, epoch: u64, limits: Limits, now: Instant) -> Self {
        Self {
            runtime,
            epoch,
            serial: 0,
            limits,
            entries: HashMap::new(),
            detached: now,
            last_now: now,
        }
    }

    pub fn expired(&mut self, now: Instant) -> Result<bool> {
        if now < self.last_now {
            return Err(Error::InvalidInput);
        }
        self.last_now = now;
        if !self.entries.is_empty() {
            // If all leases expire between ticks, detach at the last actual expiry,
            // not at the delayed polling time (which would extend lifetime forever).
            let last = self.entries.values().copied().max();
            self.entries.retain(|_, expiry| *expiry > now);
            if self.entries.is_empty() {
                if let Some(last) = last {
                    self.detached = last;
                }
            }
        }
        Ok(
            self.entries.is_empty()
                && now.duration_since(self.detached) >= self.limits.detached_ttl,
        )
    }

    pub fn attach(&mut self, now: Instant) -> Result<Lease> {
        if self.entries.len() >= self.limits.leases as usize {
            return Err(Error::Capacity);
        }
        self.serial = self.serial.checked_add(1).ok_or(Error::Capacity)?;
        let expiry = now
            .checked_add(self.limits.lease_ttl)
            .ok_or(Error::InvalidInput)?;
        self.entries.insert(self.serial, expiry);
        Ok(Lease {
            runtime: self.runtime,
            epoch: self.epoch,
            serial: self.serial,
        })
    }

    pub fn renew(&mut self, lease: Lease, now: Instant) -> Result<()> {
        self.check(lease)?;
        let expiry = now
            .checked_add(self.limits.lease_ttl)
            .ok_or(Error::InvalidInput)?;
        self.entries.insert(lease.serial, expiry);
        Ok(())
    }

    pub fn detach(&mut self, lease: Lease, now: Instant) -> Result<()> {
        self.check(lease)?;
        self.entries.remove(&lease.serial);
        if self.entries.is_empty() {
            self.detached = now;
        }
        Ok(())
    }

    fn check(&self, lease: Lease) -> Result<()> {
        if lease.runtime != self.runtime
            || lease.epoch != self.epoch
            || !self.entries.contains_key(&lease.serial)
        {
            return Err(Error::Stale);
        }
        Ok(())
    }
}
