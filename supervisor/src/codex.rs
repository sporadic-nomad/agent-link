//! Optional owned-stdio Codex runtime. Payloads are exact UTF-8 text, not JSON.
//! Native turn IDs become durable evidence only after a correlated method response.
use crate::{
    Binding, Error, NativeResult, Observation, Operation, Outcome, Receipt, Result, Runtime,
    RuntimeId, Supervisor,
};
use agent_link::{
    codex::{CodexAppServer, CodexLimits, RequestOutcome},
    portable::EventKind,
    rpc::{RpcReapStatus, RpcSpawnSpec},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Finite native deadlines and retained evidence budgets.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Maximum duration of each native request, including startup.
    pub request_timeout: Duration,
    /// Maximum text payload bytes before native admission.
    pub payload_bytes: usize,
    /// Maximum serialized terminal turn bytes.
    pub result_bytes: usize,
    /// Maximum correlated receipts/early terminal observations retained.
    pub evidence_capacity: usize,
    /// Underlying bounded native adapter limits.
    pub native_limits: CodexLimits,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            payload_bytes: 1024 * 1024,
            result_bytes: 16 * 1024,
            evidence_capacity: 64,
            native_limits: CodexLimits::default(),
        }
    }
}
#[derive(Default)]
struct Evidence {
    receipts: Vec<Receipt>,
    terminals: HashMap<String, Vec<u8>>,
}
/// Exact incarnation of an owned Codex app-server. Does not adopt surviving stdio.
/// Retain this Arc alongside Supervisor to poll receipts and observe teardown.
pub struct CodexRuntime {
    server: Arc<CodexAppServer>,
    binding: Binding,
    id: RuntimeId,
    config: Config,
    stopped: AtomicBool,
    evidence: Arc<Mutex<Evidence>>,
    drain_finished: Arc<AtomicBool>,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
impl CodexRuntime {
    /// Spawns owned stdio; call `resume` to initialize and strictly resume the binding. The
    /// provider namespace must be `codex`. Launch policy is supplied by `spec`;
    /// profile-generated specs default to unattended permissions. Startup failure
    /// requests close; absent returned handle is not cleanup proof for a birth fence.
    pub fn spawn(
        spec: RpcSpawnSpec,
        binding: Binding,
        id: RuntimeId,
        config: Config,
    ) -> Result<Arc<Self>> {
        if binding.provider != "codex"
            || !crate::text_ok(&binding.session, 1024)
            || config.request_timeout.is_zero()
            || config.request_timeout > Duration::from_secs(3600)
            || config.evidence_capacity == 0
            || config.evidence_capacity > 4096
            || config.payload_bytes == 0
            || config.result_bytes == 0
        {
            return Err(Error::InvalidInput);
        }
        let (server, events) =
            CodexAppServer::spawn(spec, config.native_limits).map_err(|_| Error::Stopped)?;
        let server = Arc::new(server);
        let evidence = Arc::new(Mutex::new(Evidence::default()));
        let drain_finished = Arc::new(AtomicBool::new(false));
        let runtime = Arc::new(Self {
            server: server.clone(),
            binding,
            id,
            config,
            stopped: AtomicBool::new(false),
            evidence: evidence.clone(),
            drain_finished: drain_finished.clone(),
        });
        std::thread::Builder::new()
            .name("supervisor-codex-events".into())
            .spawn(move || {
                loop {
                    if let Some(event) =
                        events.recv_until(Instant::now() + Duration::from_millis(10))
                    {
                        if matches!(event.portable.kind, EventKind::TurnCompleted { .. })
                            && event.portable.context.session_id.as_deref()
                                == server.snapshot().thread_id.as_deref()
                        {
                            if let (Some(turn), Some(native)) = (
                                event.portable.context.turn_id.as_ref(),
                                event.native.as_ref(),
                            ) {
                                if let Some(value) =
                                    native.get("params").and_then(|p| p.get("turn"))
                                {
                                    if let Ok(bytes) = serde_json::to_vec(value) {
                                        let mut state = lock(&evidence);
                                        if bytes.len() > config.result_bytes
                                            || (!state.terminals.contains_key(turn)
                                                && state.terminals.len()
                                                    >= config.evidence_capacity)
                                        {
                                            drop(state);
                                            server.close();
                                            break;
                                        }
                                        state.terminals.insert(turn.clone(), bytes);
                                    }
                                }
                            }
                        }
                    } else if server.failure().is_some() {
                        break;
                    }
                }
                drain_finished.store(true, Ordering::Release);
            })
            .map_err(|_| {
                runtime.server.close();
                Error::Stopped
            })?;
        Ok(runtime)
    }
    /// Initializes and strictly resumes the saved binding with finite deadlines.
    /// Call after retaining the spawned handle, inside Supervisor::resume's factory.
    /// Failure requests stop and preserves this handle for cleanup observation.
    pub fn resume(&self) -> Result<()> {
        let runtime = self;
        let config = self.config;
        let deadline = Instant::now() + config.request_timeout;
        let initialized = runtime
            .server
            .initialize("agent-link-supervisor", env!("CARGO_PKG_VERSION"), deadline)
            .ok()
            .and_then(|t| t.wait_until(deadline));
        if !matches!(initialized.as_deref(), Some(RequestOutcome::Response(_))) {
            runtime.stop();
            return Err(Error::Stopped);
        }
        let deadline = Instant::now() + config.request_timeout;
        let resumed = runtime
            .server
            .thread_resume(&runtime.binding.session, json!({}), deadline)
            .ok()
            .and_then(|t| t.wait_until(deadline));
        if !matches!(resumed.as_deref(), Some(RequestOutcome::Response(_)))
            || runtime.server.snapshot().thread_id.as_deref() != Some(&runtime.binding.session)
        {
            runtime.stop();
            return Err(Error::BindingMismatch);
        }
        Ok(())
    }
    /// Pops exact correlated terminal evidence for Supervisor::reconcile. Unknown
    /// starts and unrelated latest snapshots never become receipts. Poll continuously;
    /// saturation closes the runtime conservatively rather than dropping evidence.
    pub fn poll_evidence(&self) -> Option<Receipt> {
        let mut state = lock(&self.evidence);
        let index = state.receipts.iter().position(
            |r| matches!(&r.outcome, Outcome::Accepted(turn) if state.terminals.contains_key(turn)),
        )?;
        let mut receipt = state.receipts.remove(index);
        if let Outcome::Accepted(turn) = &receipt.outcome {
            let bytes = state.terminals.get(turn)?.clone();
            if !state
                .receipts
                .iter()
                .any(|r| matches!(&r.outcome, Outcome::Accepted(other) if other == turn))
            {
                state.terminals.remove(turn);
            }
            receipt.outcome = Outcome::Terminal {
                turn: turn.clone(),
                result: NativeResult(bytes),
            };
        }
        Some(receipt)
    }
    /// True only after event drain and dispatcher return and successful owned reap.
    /// Proves direct-child teardown, not escaped descendants or detached pipe pumps.
    pub fn cleanup_complete(&self) -> bool {
        self.drain_finished.load(Ordering::Acquire)
            && self.server.cleanup_status() == Some(RpcReapStatus::Reaped)
    }
    /// Finite observation only; timeout and OS/external-reaper ambiguity return false.
    pub fn wait_cleanup_until(&self, deadline: Instant) -> bool {
        loop {
            if self.cleanup_complete() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    /// Clears only this matching runtime fence after stop and positive cleanup.
    /// Hosts must separately ensure no escaped descendant owns provider work; this
    /// owned-stdio bridge cannot prove process-tree quiescence on any platform.
    pub fn confirm_stopped(&self, worker: &mut Supervisor) -> Result<()> {
        if worker.binding() != &self.binding || worker.fenced_runtime()? != Some(self.id) {
            return Err(Error::Stale);
        }
        if !self.stopped.load(Ordering::Acquire) || !self.cleanup_complete() {
            return Err(Error::RecoveryRequired);
        }
        worker.confirm_stopped(self.id)
    }
    fn remember(&self, intent: &Receipt, turn: String) -> Observation {
        let mut state = lock(&self.evidence);
        if state.receipts.len() >= self.config.evidence_capacity {
            drop(state);
            self.stop();
            return Observation::Unknown;
        }
        let mut receipt = intent.clone();
        receipt.outcome = Outcome::Accepted(turn.clone());
        state.receipts.push(receipt);
        Observation::Accepted(turn)
    }
}
impl Runtime for CodexRuntime {
    fn binding(&self) -> Binding {
        self.binding.clone()
    }
    fn submit(&self, intent: &Receipt, payload: &[u8]) -> Observation {
        if self.stopped.load(Ordering::Acquire)
            || intent.runtime != self.id
            || !crate::text_ok(&intent.id, 128)
            || intent.operation != Operation::Submit
            || intent.outcome != Outcome::SubmittedUnknown
            || payload.is_empty()
            || payload.len() > self.config.payload_bytes
        {
            return Observation::Unknown;
        }
        if intent.digest != crate::worker::digest(&[b"agent-link-supervisor/submit/v1", payload]) {
            return Observation::Unknown;
        }
        let Ok(text) = std::str::from_utf8(payload) else {
            return Observation::Unknown;
        };
        let deadline = Instant::now() + self.config.request_timeout;
        let outcome = self
            .server
            .turn_start(text, deadline)
            .ok()
            .and_then(|t| t.wait_until(deadline));
        match outcome.as_deref() {
            Some(RequestOutcome::Response(value)) => value
                .get("turn")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
                .map(|t| {
                    if let Some(native_turn) = value.get("turn") {
                        if matches!(
                            native_turn.get("status").and_then(Value::as_str),
                            Some("completed" | "failed" | "interrupted")
                        ) {
                            if let Ok(bytes) = serde_json::to_vec(native_turn) {
                                let mut state = lock(&self.evidence);
                                if bytes.len() > self.config.result_bytes
                                    || (!state.terminals.contains_key(t)
                                        && state.terminals.len() >= self.config.evidence_capacity)
                                {
                                    drop(state);
                                    self.stop();
                                    return Observation::Unknown;
                                }
                                state.terminals.insert(t.to_owned(), bytes);
                            }
                        }
                    }
                    self.remember(intent, t.to_owned())
                })
                .unwrap_or(Observation::Unknown),
            _ => Observation::Unknown,
        }
    }
    fn interrupt(&self, intent: &Receipt, turn: &str) -> Observation {
        if self.stopped.load(Ordering::Acquire)
            || intent.runtime != self.id
            || !crate::text_ok(&intent.id, 128)
            || !crate::text_ok(turn, 1024)
            || intent.outcome != Outcome::SubmittedUnknown
        {
            return Observation::Unknown;
        }
        let Operation::Interrupt(target) = &intent.operation else {
            return Observation::Unknown;
        };
        if !crate::text_ok(target, 128) {
            return Observation::Unknown;
        }
        let expected = crate::worker::digest(&[
            b"agent-link-supervisor/interrupt/v1",
            target.as_bytes(),
            &intent.runtime.worker.0,
            &intent.runtime.generation.to_le_bytes(),
            turn.as_bytes(),
        ]);
        if intent.digest != expected {
            return Observation::Unknown;
        }
        let deadline = Instant::now() + self.config.request_timeout;
        let outcome = self
            .server
            .turn_interrupt(turn, deadline)
            .ok()
            .and_then(|t| t.wait_until(deadline));
        if matches!(outcome.as_deref(), Some(RequestOutcome::Response(_))) {
            self.remember(intent, turn.to_owned())
        } else {
            Observation::Unknown
        }
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.server.close();
    }
}
impl Drop for CodexRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}
