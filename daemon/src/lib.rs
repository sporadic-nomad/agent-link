//! Private loopback IPC for a single durable Codex worker. Unix hosting requires
//! an existing owner-only directory; no Windows ACL provisioning is implemented.
//! Clients authenticate every frame. Secrets are never accepted in command arguments.
//! Ordinary native calls run on a bounded dispatcher so leases and stop keep moving.
use agent_link::rpc::RpcSpawnSpec;
use agent_link_supervisor::{
    Binding, Lease, Limits, Observation, Operation, Outcome, Receipt, Runtime, StopHandle,
    Supervisor,
    codex::{CodexRuntime, Config},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Request and response frame ceiling, including authentication and JSON encoding.
pub const FRAME_BYTES: usize = 2 * 1024 * 1024;
/// Total concurrent sockets (including unauthenticated/slow peers).
pub const CLIENTS: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(10);
/// Expected host, filesystem, protocol or durable coordination failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Finite socket/filesystem operation failed, or invalid input was refused.
    #[error("local worker I/O error: {0}")]
    Io(#[from] io::Error),
    /// Supervisor refused an operation or persistence failed.
    #[error(transparent)]
    Supervisor(#[from] agent_link_supervisor::Error),
    /// Invalid JSON without including request contents.
    #[error("invalid local worker JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Endpoint address was invalid.
    #[error("invalid local endpoint address")]
    Address(#[from] std::net::AddrParseError),
}
/// Local service result. Errors never include bearer secrets or prompt bytes.
pub type Result<T> = std::result::Result<T, Error>;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid private IPC input")
}

struct BoundedJson(Vec<u8>);
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > FRAME_BYTES.saturating_sub(self.0.len()) {
            return Err(invalid());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn bounded_json(value: &Value) -> io::Result<Vec<u8>> {
    let mut writer = BoundedJson(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(|_| invalid())?;
    Ok(writer.0)
}
fn finish_dispatcher(evidence: mpsc::Receiver<Receipt>, dispatcher: std::thread::JoinHandle<()>) {
    // The dispatcher may be blocked publishing exact acceptance. Disconnect this
    // receiver before joining; the committed unknown receipt remains authoritative.
    drop(evidence);
    let _ = dispatcher.join();
}
/// Read one bounded u32 big-endian length-prefixed JSON value. EOF ends a client.
pub fn read_frame(stream: &mut impl Read) -> io::Result<Value> {
    let mut len = [0; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > FRAME_BYTES {
        return Err(invalid());
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}
/// Write one bounded frame. Socket callers must install a finite write timeout.
pub fn write_frame(stream: &mut impl Write, value: &Value) -> io::Result<()> {
    let bytes = bounded_json(value)?;
    if bytes.is_empty() || bytes.len() > FRAME_BYTES {
        return Err(invalid());
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)
}
fn socket_exact(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        stream.set_read_timeout(Some(left))?;
        let n = stream.read(bytes)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes = &mut bytes[n..];
    }
    Ok(())
}
fn socket_read(stream: &mut TcpStream) -> io::Result<Value> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut len = [0; 4];
    socket_exact(stream, &mut len, deadline)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > FRAME_BYTES {
        return Err(invalid());
    }
    let mut bytes = vec![0; len];
    socket_exact(stream, &mut bytes, deadline)?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}
fn socket_write(stream: &mut TcpStream, value: &Value) -> io::Result<()> {
    let body = bounded_json(value)?;
    if body.is_empty() || body.len() > FRAME_BYTES {
        return Err(invalid());
    }
    let mut bytes = Vec::with_capacity(body.len() + 4);
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut remaining = bytes.as_slice();
    while !remaining.is_empty() {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        stream.set_write_timeout(Some(left))?;
        let n = stream.write(remaining)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        remaining = &remaining[n..];
    }
    Ok(())
}
fn token() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| invalid())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn secret_matches(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}
/// Validates that the local endpoint/store directory is private. Hosting on Windows
/// is deliberately refused until an ACL ownership policy is supplied.
pub fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = fs::symlink_metadata(path)?;
        // SAFETY: geteuid has no arguments or memory preconditions.
        let owner = unsafe { libc::geteuid() };
        if !meta.is_dir() || meta.uid() != owner || meta.permissions().mode() & 0o077 != 0 {
            return Err(invalid());
        }
        // POSIX mode and ownership do not audit extended filesystem ACLs.
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix private directory hosting only",
        ))
    }
}

struct Work {
    intent: Receipt,
    payload: Vec<u8>,
    turn: Option<String>,
}
struct Proxy {
    native: Arc<CodexRuntime>,
    tx: mpsc::SyncSender<Work>,
    busy: Arc<AtomicBool>,
}
impl Runtime for Proxy {
    fn binding(&self) -> Binding {
        self.native.binding()
    }
    fn submit(&self, intent: &Receipt, payload: &[u8]) -> Observation {
        self.enqueue(intent, payload, None)
    }
    fn interrupt(&self, intent: &Receipt, turn: &str) -> Observation {
        self.enqueue(intent, &[], Some(turn.into()))
    }
    fn stop(&self) {
        self.native.stop();
    }
}
impl Proxy {
    fn enqueue(&self, intent: &Receipt, payload: &[u8], turn: Option<String>) -> Observation {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && self
                .tx
                .try_send(Work {
                    intent: intent.clone(),
                    payload: payload.to_vec(),
                    turn,
                })
                .is_err()
        {
            self.busy.store(false, Ordering::Release);
        }
        Observation::Unknown
    }
}
struct Call {
    connection: u64,
    request: Value,
    reply: mpsc::SyncSender<Value>,
}
struct ClientGuard(Arc<AtomicUsize>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
fn socket(
    mut stream: TcpStream,
    control: bool,
    connection: u64,
    secret: Arc<String>,
    tx: mpsc::SyncSender<Call>,
    count: Arc<AtomicUsize>,
    stop: StopHandle,
) {
    let _guard = ClientGuard(count);
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    while let Ok(request) = socket_read(&mut stream) {
        if !request
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|t| secret_matches(t, &secret))
        {
            break;
        }
        if request.get("op").and_then(Value::as_str) == Some("stop") {
            stop.request();
            let _ = socket_write(&mut stream, &json!({"stopping":true}));
            break;
        }
        if control {
            break;
        }
        let (reply, rx) = mpsc::sync_channel(1);
        if tx
            .try_send(Call {
                connection,
                request,
                reply,
            })
            .is_err()
        {
            let _ = socket_write(&mut stream, &json!({"error":"busy"}));
            break;
        }
        match rx.recv_timeout(IO_TIMEOUT) {
            Ok(value) if socket_write(&mut stream, &value).is_ok() => {}
            _ => break,
        }
    }
    // Leases remain bounded by their finite TTL if disconnect notification is full.
    let (reply, _) = mpsc::sync_channel(1);
    let _ = tx.try_send(Call {
        connection,
        request: json!({"op":"disconnect"}),
        reply,
    });
}
fn receipt(r: Receipt) -> Value {
    let operation = match r.operation {
        Operation::Submit => json!({"kind":"submit"}),
        Operation::Interrupt(target) => json!({"kind":"interrupt","target":target}),
    };
    let outcome = match r.outcome {
        Outcome::SubmittedUnknown => json!({"state":"unknown"}),
        Outcome::Accepted(turn) => json!({"state":"accepted","turn":turn}),
        Outcome::Terminal { turn, result } => {
            json!({"state":"terminal","turn":turn,"result":result.0})
        }
    };
    json!({"id":r.id,"generation":r.runtime.generation,"worker":r.runtime.worker.0,
        "digest":r.digest.0.as_slice(),"operation":operation,"outcome":outcome})
}
fn field<'a>(v: &'a Value, name: &str) -> io::Result<&'a str> {
    v.get(name).and_then(Value::as_str).ok_or_else(invalid)
}
fn execute(
    worker: &mut Supervisor,
    leases: &mut HashMap<u64, (Lease, Instant)>,
    call: &Call,
    busy: bool,
    lease_ttl: Duration,
) -> Result<Value> {
    let now = Instant::now();
    Ok(match field(&call.request, "op")? {
        "status" => {
            json!({"worker":worker.id().0,"generation":worker.fenced_runtime()?.map(|id|id.generation),"native_busy":busy})
        }
        "receipt" => json!({"receipt":worker.receipt(field(&call.request,"id")?)?.map(receipt)}),
        "attach" => {
            if leases.contains_key(&call.connection) {
                return Err(invalid().into());
            }
            leases.insert(call.connection, (worker.attach(now)?, now + lease_ttl));
            json!({"attached":true})
        }
        "renew" => {
            let entry = leases.get_mut(&call.connection).ok_or_else(invalid)?;
            worker.renew(entry.0, now)?;
            entry.1 = now + lease_ttl;
            json!({"renewed":true})
        }
        "detach" | "disconnect" => {
            if let Some(lease) = leases.remove(&call.connection) {
                let _ = worker.detach(lease.0, now);
            }
            json!({"detached":true})
        }
        "submit" => {
            let id = field(&call.request, "id")?;
            // Replays/conflicts must still reach the ledger while native work waits.
            if busy && worker.receipt(id)?.is_none() {
                return Ok(json!({"error":"busy"}));
            }
            receipt(worker.submit(id, field(&call.request, "payload")?.as_bytes())?)
        }
        "interrupt" => {
            let id = field(&call.request, "id")?;
            if busy && worker.receipt(id)?.is_none() {
                return Ok(json!({"error":"busy"}));
            }
            receipt(worker.interrupt(id, field(&call.request, "target")?)?)
        }
        _ => return Err(invalid().into()),
    })
}
/// Runs one worker until authenticated stop or finite detached expiry. The supplied
/// launch is local host configuration, never client input. Existing crash fences are
/// refused before spawn. Cleanup never automatically clears a durable birth fence:
/// operator containment evidence is required before explicit supervisor recovery.
/// The host must have durably provisioned directory ancestry before calling this.
pub fn serve(
    root: &Path,
    binding: Binding,
    spec: RpcSpawnSpec,
    limits: Limits,
    config: Config,
) -> Result<()> {
    private_directory(root)?;
    let mut worker = Supervisor::open(root, binding.clone(), limits)?;
    let (native_tx, native_rx) = mpsc::sync_channel::<Work>(1);
    let (evidence_tx, evidence_rx) = mpsc::sync_channel::<Receipt>(2);
    let busy = Arc::new(AtomicBool::new(false));
    let mut native = None;
    let dispatch_busy = busy.clone();
    worker.resume(Instant::now(), |b, id| {
        let rt = CodexRuntime::spawn(spec, b.clone(), id, config)?;
        native = Some(rt.clone());
        rt.resume()?;
        Ok(Arc::new(Proxy {
            native: rt,
            tx: native_tx,
            busy: dispatch_busy,
        }))
    })?;
    let native = native.ok_or_else(invalid)?;
    let stop = worker.stop_handle()?;
    let dispatch_native = native.clone();
    let dispatch_busy = busy.clone();
    let dispatcher = std::thread::Builder::new()
        .name("daemon-native".into())
        .spawn(move || {
            while let Ok(work) = native_rx.recv() {
                let observation = if let Some(turn) = work.turn {
                    dispatch_native.interrupt(&work.intent, &turn)
                } else {
                    dispatch_native.submit(&work.intent, &work.payload)
                };
                if let Observation::Accepted(turn) = observation {
                    let mut evidence = work.intent;
                    evidence.outcome = Outcome::Accepted(turn);
                    if evidence_tx.send(evidence).is_err() {
                        break;
                    }
                }
                dispatch_busy.store(false, Ordering::Release);
            }
        })?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let control = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    control.set_nonblocking(true)?;
    let control_count = Arc::new(AtomicUsize::new(0));
    let secret = Arc::new(token()?);
    let endpoint = root.join("endpoint.json");
    // Directory is private; atomic replacement prevents partial client reads.
    let temp = root.join(format!("endpoint-{}", token()?));
    fs::write(
        &temp,
        serde_json::to_vec(
            &json!({"address":listener.local_addr()?.to_string(),"control":control.local_addr()?.to_string(),"token":*secret}),
        )?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(temp, &endpoint)?;
    let (tx, rx) = mpsc::sync_channel::<Call>(CLIENTS);
    let count = Arc::new(AtomicUsize::new(0));
    let mut serial = 0u64;
    let mut leases = HashMap::new();
    let result = (|| -> Result<()> {
        loop {
            while let Ok(evidence) = evidence_rx.try_recv() {
                // Terminal notification may beat the dispatcher acceptance message.
                // Ignore only that exact weaker evidence, never a different origin.
                let terminal = worker.receipt(&evidence.id)?.is_some_and(|r| {
                    r.runtime == evidence.runtime && r.digest == evidence.digest
                        && r.operation == evidence.operation
                        && matches!((&r.outcome,&evidence.outcome),
                            (Outcome::Terminal {turn,..},Outcome::Accepted(accepted)) if turn==accepted)
                });
                if !terminal {
                    worker.reconcile(&binding, evidence)?;
                }
            }
            while let Some(evidence) = native.poll_evidence() {
                worker.reconcile(&binding, evidence)?;
            }
            let now = Instant::now();
            leases.retain(|_, (_, expiry): &mut (Lease, Instant)| *expiry > now);
            if worker.tick(now)? {
                break;
            }
            // One accept/request per iteration bounds control latency under load.
            for (listener, control_only, count, maximum) in [
                (&control, true, &control_count, 2),
                (&listener, false, &count, CLIENTS),
            ] {
                if let Ok((stream, _)) = listener.accept() {
                    if count.load(Ordering::Acquire) < maximum {
                        serial = serial.checked_add(1).ok_or_else(invalid)?;
                        count.fetch_add(1, Ordering::AcqRel);
                        let (secret, tx, count, stop) =
                            (secret.clone(), tx.clone(), count.clone(), stop.clone());
                        std::thread::Builder::new()
                            .name("daemon-client".into())
                            .spawn(move || {
                                socket(stream, control_only, serial, secret, tx, count, stop)
                            })?;
                    }
                }
            }
            match rx.recv_timeout(TICK) {
                Ok(call) => {
                    let response = execute(
                        &mut worker,
                        &mut leases,
                        &call,
                        busy.load(Ordering::Acquire),
                        limits.lease_ttl,
                    )
                    .unwrap_or_else(|e| json!({"error":e.to_string()}));
                    let _ = call.reply.try_send(response);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    })();
    stop.request();
    let _ = fs::remove_file(endpoint);
    // Closing native I/O releases the finite native dispatcher. Drop Supervisor
    // retains the exact birth fence, even after observed direct-child cleanup.
    let _ = native.wait_cleanup_until(Instant::now() + Duration::from_secs(5));
    drop(worker);
    finish_dispatcher(evidence_rx, dispatcher);
    result
}

/// Authenticated persistent local client. Debug deliberately omits its bearer secret.
/// Leases belong to this connection and cannot be transferred to another client.
pub struct Client {
    stream: TcpStream,
    control: SocketAddr,
    secret: String,
}
impl Client {
    /// Reads the private endpoint file and refuses any address outside IPv4 loopback.
    pub fn connect(root: &Path) -> Result<Self> {
        private_directory(root)?;
        let path: PathBuf = root.join("endpoint.json");
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(1025).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 {
            return Err(invalid().into());
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        let address: SocketAddr = field(&value, "address")?.parse()?;
        if !matches!(address,SocketAddr::V4(a) if *a.ip()==std::net::Ipv4Addr::LOCALHOST) {
            return Err(invalid().into());
        }
        let control: SocketAddr = field(&value, "control")?.parse()?;
        if !matches!(control,SocketAddr::V4(a) if *a.ip()==std::net::Ipv4Addr::LOCALHOST) {
            return Err(invalid().into());
        }
        let secret = field(&value, "token")?.to_owned();
        if secret.len() != 64 {
            return Err(invalid().into());
        }
        let stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Self {
            stream,
            control,
            secret,
        })
    }
    /// Sends one request and waits a finite interval. A timeout is unknown delivery;
    /// reconnect and query/replay the same ID, never invent a replacement ID.
    pub fn request(&mut self, mut request: Value) -> Result<Value> {
        request
            .as_object_mut()
            .ok_or_else(invalid)?
            .insert("token".into(), json!(self.secret));
        if request.get("op").and_then(Value::as_str) == Some("stop") {
            let mut stream = TcpStream::connect_timeout(&self.control, IO_TIMEOUT)?;
            socket_write(&mut stream, &request)?;
            return Ok(socket_read(&mut stream)?);
        }
        socket_write(&mut self.stream, &request)?;
        Ok(socket_read(&mut self.stream)?)
    }
}

/// Explicit local recovery after an operator/containing host positively proves
/// teardown of the exact persisted incarnation and its provider work/subscriptions.
/// This trusted callback must verify external containment; owner disappearance,
/// PID absence, direct-child reap, timeout and silence are insufficient evidence.
/// There is no IPC recovery command. A false proof or stale identity leaves the
/// fence untouched. The lifetime owner lock excludes an active daemon.
///
/// ```no_run
/// use agent_link_daemon::recover;
/// use agent_link_supervisor::{Binding, Limits, RuntimeId};
/// use std::path::Path;
/// fn after_external_containment_audit(
///     root: &Path, binding: Binding, expected_full_birth: RuntimeId,
///     prove_exact_containment: impl FnOnce(RuntimeId) -> bool,
/// ) -> agent_link_daemon::Result<()> {
///     recover(root, binding, Limits::default(), expected_full_birth,
///         prove_exact_containment)
/// }
/// ```
pub fn recover(
    root: &Path,
    binding: Binding,
    limits: Limits,
    expected: agent_link_supervisor::RuntimeId,
    prove_exact_containment: impl FnOnce(agent_link_supervisor::RuntimeId) -> bool,
) -> Result<()> {
    private_directory(root)?;
    let mut worker = Supervisor::open(root, binding, limits)?;
    if worker.fenced_runtime()? != Some(expected) {
        return Err(agent_link_supervisor::Error::Stale.into());
    }
    if !prove_exact_containment(expected) {
        return Err(agent_link_supervisor::Error::RecoveryRequired.into());
    }
    worker.confirm_stopped(expected)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Unknown(Binding);
    impl Runtime for Unknown {
        fn binding(&self) -> Binding {
            self.0.clone()
        }
        fn submit(&self, _: &Receipt, _: &[u8]) -> Observation {
            Observation::Unknown
        }
        fn interrupt(&self, _: &Receipt, _: &str) -> Observation {
            Observation::Unknown
        }
        fn stop(&self) {}
    }
    #[test]
    fn saturated_acceptance_publication_cannot_block_shutdown_or_settle_unknown() {
        let root = tempfile::tempdir().unwrap();
        let binding = Binding {
            provider: "fixture".into(),
            session: "existing".into(),
        };
        let mut worker = Supervisor::open(root.path(), binding.clone(), Limits::default()).unwrap();
        worker
            .resume(Instant::now(), |b, _| Ok(Arc::new(Unknown(b.clone()))))
            .unwrap();
        let mut evidence = worker.submit("stable", b"payload").unwrap();
        evidence.outcome = Outcome::Accepted("turn".into());
        let (tx, rx) = mpsc::sync_channel(1);
        let (ready, waiting) = mpsc::sync_channel(1);
        let dispatcher = std::thread::spawn(move || {
            tx.send(evidence.clone()).unwrap();
            ready.send(()).unwrap();
            // This publication blocks until shutdown disconnects the full queue.
            let _ = tx.send(evidence);
        });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(worker);
        let (done, finished) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            finish_dispatcher(rx, dispatcher);
            let _ = done.send(());
        });
        finished
            .recv_timeout(Duration::from_secs(3))
            .expect("full evidence queue stalled shutdown");
        let worker = Supervisor::open(root.path(), binding, Limits::default()).unwrap();
        assert_eq!(
            worker.receipt("stable").unwrap().unwrap().outcome,
            Outcome::SubmittedUnknown
        );
        assert!(worker.fenced_runtime().unwrap().is_some());
    }
    #[test]
    fn oversized_serialized_response_never_writes_a_partial_frame() {
        let mut output = Vec::new();
        assert!(write_frame(&mut output, &json!({"payload":"x".repeat(FRAME_BYTES)})).is_err());
        assert!(output.is_empty());
    }
}
