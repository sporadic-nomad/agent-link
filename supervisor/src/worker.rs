use std::path::Path;
use std::sync::{Arc, atomic::AtomicU8};
use std::time::Instant;

use sha2::{Digest as _, Sha256};

use crate::leases::Leases;
use crate::store::Store;
use crate::{
    Binding, Digest, Error, Lease, Limits, Observation, Operation, Outcome, Receipt, Result,
    Runtime, RuntimeId, StopHandle, WorkerId,
};

/// One exclusively owned durable worker. Keep this object in the supervisor process,
/// not in a viewer. Dropping observation/leases never implicitly resubmits or kills a
/// turn. Dropping this owner requests runtime stop but leaves its durable cleanup fence.
///
/// Ordinary calls require `&mut self`, so the host serializes them. They may block on
/// disk or native callbacks; retain a [`StopHandle`] on a separate control path.
pub struct Supervisor {
    runtime: Option<Arc<dyn Runtime>>,
    stop: Option<StopHandle>,
    leases: Option<Leases>,
    // Drop the owned runtime before releasing the lifetime ownership lock. The
    // durable fence still protects callbacks/externally retained runtime references.
    store: Store,
}

impl Supervisor {
    /// Opens/creates one worker in an existing application-private directory on a local
    /// filesystem. Refuses a concurrent owner, mismatched binding/budgets, unsupported
    /// schema or corrupt store. Never deletes old records or takes over a live lock.
    ///
    /// SQLite DELETE journaling with EXTRA synchronization commits before dispatch.
    /// The filesystem/device must honor locking and sync; network filesystems, copying
    /// an active store, external edits and deletion/replacement of lock files are outside
    /// the contract. The host must durably provision the directory/ancestors and protect
    /// its permissions before opening; this method never creates missing directories.
    pub fn open(path: impl AsRef<Path>, binding: Binding, limits: Limits) -> Result<Self> {
        Ok(Self {
            store: Store::open(path.as_ref(), binding, limits)?,
            runtime: None,
            stop: None,
            leases: None,
        })
    }

    /// Durable full worker identity; not a provider session ID or process ID.
    pub fn id(&self) -> WorkerId {
        self.store.id
    }

    /// Immutable provider-session binding.
    pub fn binding(&self) -> &Binding {
        &self.store.binding
    }

    /// Exact incarnation whose teardown/unsubscribe is not yet confirmed. On reopen,
    /// Some means recovery is required even if its previous owner PID no longer exists.
    pub fn fenced_runtime(&self) -> Result<Option<RuntimeId>> {
        self.store.live()
    }

    /// Reads one authoritative receipt; None proves only absence of a committed intent
    /// in this intact store. Does not infer provider history or authorize a resend.
    pub fn receipt(&self, id: &str) -> Result<Option<Receipt>> {
        if !crate::text_ok(id, 128) {
            return Err(Error::InvalidInput);
        }
        self.store.receipt(id)
    }

    /// Persists a fresh runtime birth fence, then invokes strict native resume of the
    /// saved conversation. The factory must return its actual binding, never silently
    /// create a conversation. A failed/panicking factory leaves the fence in place.
    /// The host must clean up any partial spawn and positively confirm stopped before
    /// trying again. Existing runtime fences refuse the factory without invoking it.
    ///
    /// `now` starts detached expiry; callers supply monotonic time for all lease APIs.
    /// A resume may inspect/reconcile unknown work but cannot submit a new turn until
    /// exact terminal evidence settles all unknown/active submits.
    pub fn resume(
        &mut self,
        now: Instant,
        factory: impl FnOnce(&Binding, RuntimeId) -> Result<Arc<dyn Runtime>>,
    ) -> Result<RuntimeId> {
        let id = self.store.begin_runtime()?;
        let runtime = factory(&self.store.binding, id)?;
        self.install(id, now, runtime)?;
        Ok(id)
    }

    /// Re-adopts an existing fenced incarnation before considering any replacement.
    /// The factory must authenticate the surviving endpoint's full worker/birth token,
    /// verify that pending unload/subscription fences have settled, and attach to that
    /// SAME runtime. It must not spawn. Provider session identity alone is insufficient.
    ///
    /// This trusted host boundary supports keeper/borrowed-endpoint implementations;
    /// owned stdio cannot usually be recovered after its owning process dies. A failed
    /// adoption retains the fence and never falls back to resume or fresh creation.
    pub fn adopt(
        &mut self,
        id: RuntimeId,
        now: Instant,
        factory: impl FnOnce(&Binding, RuntimeId) -> Result<Arc<dyn Runtime>>,
    ) -> Result<()> {
        self.store.healthy()?;
        if self.runtime.is_some() {
            return Err(Error::Busy);
        }
        if self.store.live()? != Some(id) {
            return Err(Error::Stale);
        }
        let runtime = factory(&self.store.binding, id)?;
        self.install(id, now, runtime)
    }

    fn install(&mut self, id: RuntimeId, now: Instant, runtime: Arc<dyn Runtime>) -> Result<()> {
        if runtime.binding() != self.store.binding {
            runtime.stop();
            return Err(Error::BindingMismatch);
        }
        let stop = StopHandle {
            runtime: Arc::downgrade(&runtime),
            state: Arc::new(AtomicU8::new(0)),
        };
        self.runtime = Some(runtime);
        self.stop = Some(stop);
        self.leases = Some(Leases::new(id, self.store.epoch, self.store.limits, now));
        Ok(())
    }

    /// Confirms positive host evidence that this exact incarnation and every pending
    /// unsubscribe/unload callback have finished. PID absence, expired leases, owner
    /// death, `Runtime::stop` returning, and lack of output are NOT such evidence.
    ///
    /// This is a trusted integration boundary, not an OS liveness probe. The host must
    /// join owned teardown or use an equivalent exact-generation acknowledgement. An
    /// attached runtime must first have stop requested. Stale generations are refused.
    /// Operation uncertainty survives cleanup and continues to block new submissions.
    pub fn confirm_stopped(&mut self, id: RuntimeId) -> Result<()> {
        self.store.healthy()?;
        if self.store.live()? != Some(id) {
            return Err(Error::Stale);
        }
        if self.runtime.is_some() && !self.stop.as_ref().is_some_and(StopHandle::settled) {
            return Err(Error::Busy);
        }
        // Dispose the old instance before opening the reuse fence. Stop handles use
        // Weak and can never be retargeted to a replacement runtime.
        self.runtime = None;
        self.stop = None;
        self.leases = None;
        self.store.clear_runtime(id)
    }

    /// Stop capability for this exact attached incarnation. No persistence or ordinary
    /// operation admission is needed to request stop, even when all budgets are full.
    pub fn stop_handle(&self) -> Result<StopHandle> {
        self.stop.clone().ok_or(Error::Stopped)
    }

    /// Commit unknown intent, dispatch once, and commit exact acceptance if returned.
    /// Same ID/bytes returns the existing receipt in any state without a native call;
    /// different bytes/kind conflicts. Empty/oversized payloads refuse before hashing.
    /// Unknown or active prior turns refuse new IDs. Errors after intent commit leave
    /// uncertainty; inspect the receipt on reopen, never retry with a new operation ID.
    pub fn submit(&mut self, id: &str, payload: &[u8]) -> Result<Receipt> {
        self.validate_input(id, payload)?;
        let digest = digest(&[b"agent-link-supervisor/submit/v1", payload]);
        if let Some(receipt) = self.replay(id, digest, &Operation::Submit)? {
            return Ok(receipt);
        }
        let runtime = self.active()?;
        if self.store.unresolved()? {
            return Err(Error::Busy);
        }
        let mut receipt = self.intent(id, digest, Operation::Submit)?;
        // Native stop must itself fence admission to resolve a concurrent stop/send
        // race after this check. The intent remains unknown if stop wins here.
        if !self.stopping() {
            let observation = runtime.submit(&receipt, payload);
            self.accept(&mut receipt, observation, None)?;
        }
        Ok(receipt)
    }

    /// Independently budgeted, durable, exact-turn interruption. `target` names a
    /// previously accepted submit in the CURRENT incarnation. Unknown acceptance,
    /// completed targets and predecessor incarnations are refused without native I/O.
    /// Replaying an existing cancellation ID never sends again. Acceptance means only
    /// an interrupt acknowledgement; matching terminal evidence settles both records.
    pub fn interrupt(&mut self, id: &str, target: &str) -> Result<Receipt> {
        if !crate::text_ok(id, 128) || !crate::text_ok(target, 128) {
            return Err(Error::InvalidInput);
        }
        let target_receipt = self.store.receipt(target)?.ok_or(Error::Stale)?;
        if target_receipt.operation != Operation::Submit {
            return Err(Error::Stale);
        }
        let turn = match &target_receipt.outcome {
            Outcome::Accepted(turn) | Outcome::Terminal { turn, .. } => turn.clone(),
            Outcome::SubmittedUnknown => return Err(Error::Busy),
        };
        let digest = digest(&[
            b"agent-link-supervisor/interrupt/v1",
            target.as_bytes(),
            &target_receipt.runtime.worker.0,
            &target_receipt.runtime.generation.to_le_bytes(),
            turn.as_bytes(),
        ]);
        let operation = Operation::Interrupt(target.to_owned());
        if let Some(receipt) = self.replay(id, digest, &operation)? {
            return Ok(receipt);
        }
        let runtime = self.active()?;
        if self.store.live()? != Some(target_receipt.runtime)
            || !matches!(target_receipt.outcome, Outcome::Accepted(_))
        {
            return Err(Error::Stale);
        }
        let mut receipt = self.intent(id, digest, operation)?;
        if !self.stopping() {
            let observation = runtime.interrupt(&receipt, &turn);
            self.accept(&mut receipt, observation, Some(&turn))?;
        }
        Ok(receipt)
    }

    /// Reconcile exact native evidence, including after a restart. Supply the saved
    /// receipt identity/digest/incarnation plus its stronger outcome and exact binding.
    /// The adapter must correlate native evidence to THIS operation, not merely the
    /// latest turn in a conversation. Missing/ambiguous history must stay unknown.
    ///
    /// Wrong origins/turns, evidence regression, and changed terminal results are
    /// refused. Identical evidence is idempotent. Results commit before this returns;
    /// publish to observers only afterward. No callback or dispatch occurs here.
    pub fn reconcile(&mut self, binding: &Binding, evidence: Receipt) -> Result<Receipt> {
        if binding != &self.store.binding {
            return Err(Error::BindingMismatch);
        }
        if !crate::text_ok(&evidence.id, 128) {
            return Err(Error::InvalidInput);
        }
        let previous = self.store.receipt(&evidence.id)?.ok_or(Error::Stale)?;
        if previous.runtime != evidence.runtime
            || previous.digest != evidence.digest
            || previous.operation != evidence.operation
        {
            return Err(Error::Stale);
        }
        let turn = match &evidence.outcome {
            Outcome::SubmittedUnknown => None,
            Outcome::Accepted(turn) => Some(turn),
            Outcome::Terminal { turn, result } => {
                if result.0.len() > self.store.limits.result_bytes as usize {
                    return Err(Error::Capacity);
                }
                Some(turn)
            }
        };
        if turn.is_some_and(|turn| !crate::text_ok(turn, 1024)) {
            return Err(Error::InvalidInput);
        }
        if let Operation::Interrupt(target) = &evidence.operation {
            let target = self.store.receipt(target)?.ok_or(Error::Stale)?;
            let target_turn = match &target.outcome {
                Outcome::Accepted(turn) | Outcome::Terminal { turn, .. } => Some(turn),
                _ => None,
            };
            if turn.is_some() && turn != target_turn {
                return Err(Error::Stale);
            }
            // Only terminal evidence for the original turn can settle interruption.
            if matches!(evidence.outcome, Outcome::Terminal { .. })
                && evidence.outcome != target.outcome
            {
                return Err(Error::Stale);
            }
        }
        if previous == evidence {
            return Ok(previous);
        }
        match &previous.outcome {
            Outcome::SubmittedUnknown => {}
            Outcome::Accepted(old) if matches!(&evidence.outcome,Outcome::Terminal {turn,..} if turn==old) =>
                {}
            _ => return Err(Error::Conflict),
        }
        self.store.save(&evidence)?;
        Ok(evidence)
    }

    /// Expire leases and request stop after the detached deadline. Returns whether
    /// stop has been requested. Call periodically even without clients; this crate
    /// installs no timer thread. Backwards supplied times are refused.
    pub fn tick(&mut self, now: Instant) -> Result<bool> {
        let leases = self.leases.as_mut().ok_or(Error::Stopped)?;
        if leases.expired(now)? {
            if let Some(stop) = &self.stop {
                stop.request();
            }
        }
        Ok(self.stopping())
    }

    /// Attaches a bounded lease. Does not transfer ownership or buffer native output.
    pub fn attach(&mut self, now: Instant) -> Result<Lease> {
        if self.tick(now)? {
            return Err(Error::Stopped);
        }
        self.leases.as_mut().ok_or(Error::Stopped)?.attach(now)
    }

    /// Renews only an unexpired current-epoch lease. Expired leases cannot resurrect.
    pub fn renew(&mut self, lease: Lease, now: Instant) -> Result<()> {
        if self.tick(now)? {
            return Err(Error::Stopped);
        }
        self.leases
            .as_mut()
            .ok_or(Error::Stopped)?
            .renew(lease, now)
    }

    /// Detaches an existing lease. Stale/repeated detach cannot remove another lease.
    /// Final detach starts expiry; it does not cancel active native work immediately.
    pub fn detach(&mut self, lease: Lease, now: Instant) -> Result<()> {
        if self.tick(now)? {
            return Err(Error::Stopped);
        }
        self.leases
            .as_mut()
            .ok_or(Error::Stopped)?
            .detach(lease, now)
    }

    fn stopping(&self) -> bool {
        self.stop.as_ref().is_none_or(StopHandle::is_requested)
    }

    fn active(&self) -> Result<Arc<dyn Runtime>> {
        self.store.healthy()?;
        if self.stopping() {
            return Err(Error::Stopped);
        }
        self.runtime.clone().ok_or(Error::Stopped)
    }

    fn validate_input(&self, id: &str, payload: &[u8]) -> Result<()> {
        if !crate::text_ok(id, 128)
            || payload.is_empty()
            || payload.len() > self.store.limits.payload_bytes as usize
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }

    fn replay(&self, id: &str, digest: Digest, operation: &Operation) -> Result<Option<Receipt>> {
        let receipt = self.store.receipt(id)?;
        if receipt
            .as_ref()
            .is_some_and(|receipt| receipt.digest != digest || &receipt.operation != operation)
        {
            return Err(Error::Conflict);
        }
        Ok(receipt)
    }

    fn intent(&mut self, id: &str, digest: Digest, operation: Operation) -> Result<Receipt> {
        let receipt = Receipt {
            id: id.to_owned(),
            digest,
            operation,
            runtime: self.store.live()?.ok_or(Error::Stopped)?,
            outcome: Outcome::SubmittedUnknown,
        };
        self.store.insert(&receipt)?;
        Ok(receipt)
    }

    fn accept(
        &mut self,
        receipt: &mut Receipt,
        observation: Observation,
        expected: Option<&str>,
    ) -> Result<()> {
        if let Observation::Accepted(turn) = observation {
            if !crate::text_ok(&turn, 1024) || expected.is_some_and(|expected| turn != expected) {
                return Err(Error::Stale);
            }
            receipt.outcome = Outcome::Accepted(turn);
            self.store.save(receipt)?;
        }
        Ok(())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.request();
        }
        // Never claim cleanup from Drop and never clear the persisted runtime fence.
    }
}

pub(crate) fn digest(parts: &[&[u8]]) -> Digest {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    Digest(hash.finalize().into())
}
