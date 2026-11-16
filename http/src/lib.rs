//! Authenticated, read-only loopback HTTP/SSE presentation. See `docs/workspace-crates.md#agent-link-http-http`.
//! No browser assets, commands, mutation endpoints, delivery inference or durable cursors.
use agent_link::{
    ReplayCursor, ReplayError, SessionId, Terminal,
    observers::{self, ObserverHub},
};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// All allocations and waits attributable to a client have finite limits.
#[derive(Clone)]
pub struct Config {
    /// Must be loopback; port zero chooses a free port.
    pub bind: SocketAddr,
    /// Opaque bearer secret, 32..=256 printable ASCII characters; never logged.
    pub token: String,
    /// Includes unauthenticated and incomplete-header connections.
    pub clients: usize,
    /// Maximum entire request header, including Last-Event-ID.
    pub header_bytes: usize,
    /// Maximum JSON response or SSE frame (also bounds serialization).
    pub frame_bytes: usize,
    /// Maximum optional host projection JSON bytes.
    pub projection_bytes: usize,
    /// Maximum total active frame bytes; queues are independently bounded by `observers`.
    pub active_bytes: usize,
    /// Aggregate reservation for client frame + projection copy + header logical bytes.
    /// JSON value/container overhead is separately bounded by projection bytes and clients.
    pub scratch_bytes: usize,
    /// Total request-header deadline and individual socket write timeout.
    pub io_timeout: Duration,
    /// Maximum SSE lifetime, even if actively producing output. Reconnect explicitly.
    pub stream_lifetime: Duration,
    /// Observer history, per-viewer and aggregate queue budgets.
    pub observers: observers::Limits,
}
impl Config {
    /// Loopback defaults. Supply an independently generated bearer secret.
    pub fn new(token: String) -> Self {
        Self {
            bind: (IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into(),
            token,
            clients: 16,
            header_bytes: 8192,
            frame_bytes: 256 * 1024,
            projection_bytes: 64 * 1024,
            active_bytes: 1024 * 1024,
            scratch_bytes: 8 * 1024 * 1024,
            io_timeout: Duration::from_secs(2),
            stream_lifetime: Duration::from_secs(60),
            observers: observers::Limits::default(),
        }
    }
}
struct Projection {
    revision: u64,
    cursor: ReplayCursor,
    json: Value,
}
struct Shared {
    config: Config,
    terminal: Arc<Terminal>,
    hub: ObserverHub,
    incarnation: String,
    stopped: AtomicBool,
    active: AtomicUsize,
    scratch: AtomicUsize,
    projection: Mutex<Option<Projection>>,
}
/// Owns bounded client workers and one continuously drained ObserverHub.
/// Drop/shutdown joins workers; it never terminates the host's terminal.
pub struct Server {
    shared: Arc<Shared>,
    address: SocketAddr,
    worker: Option<JoinHandle<()>>,
}
impl Server {
    /// Bind only loopback. Reject invalid limits before attaching a source or accepting clients.
    /// Recovery capture may wait for core screen/log processing; it does not wait for subscribers.
    pub fn bind(terminal: Arc<Terminal>, config: Config) -> io::Result<Self> {
        if !config.bind.ip().is_loopback()
            || !(32..=256).contains(&config.token.len())
            || !config.token.bytes().all(|b| (33..=126).contains(&b))
            || [
                config.clients,
                config.header_bytes,
                config.frame_bytes,
                config.projection_bytes,
                config.active_bytes,
                config.scratch_bytes,
            ]
            .contains(&0)
            || config.header_bytes < 256
            || config.frame_bytes < 1024
            || config.active_bytes < config.frame_bytes
            || config
                .frame_bytes
                .checked_add(config.projection_bytes)
                .and_then(|n| n.checked_add(config.header_bytes))
                .is_none_or(|n| n > config.scratch_bytes)
            || config.io_timeout.is_zero()
            || config.stream_lifetime.is_zero()
            || Instant::now().checked_add(config.io_timeout).is_none()
            || Instant::now().checked_add(config.stream_lifetime).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid HTTP configuration",
            ));
        }
        let mut random = [0; 16];
        getrandom::getrandom(&mut random)
            .map_err(|_| io::Error::other("server identity entropy unavailable"))?;
        let incarnation = random.iter().map(|b| format!("{b:02x}")).collect();
        let listener = TcpListener::bind(config.bind)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let hub = ObserverHub::new(&terminal, config.observers).map_err(io::Error::other)?;
        let shared = Arc::new(Shared {
            config,
            terminal,
            hub,
            incarnation,
            stopped: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            scratch: AtomicUsize::new(0),
            projection: Mutex::new(None),
        });
        let s = shared.clone();
        let worker = thread::Builder::new()
            .name("agent-link-http".into())
            .spawn(move || {
                let mut workers: Vec<JoinHandle<()>> = Vec::new();
                while !s.stopped.load(Ordering::Acquire) {
                    let mut i = 0;
                    while i < workers.len() {
                        if workers[i].is_finished() {
                            let _ = workers.swap_remove(i).join();
                        } else {
                            i += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if workers.len() >= s.config.clients {
                                drop(stream);
                                continue;
                            }
                            let s = s.clone();
                            if let Ok(w) = thread::Builder::new()
                                .name("agent-link-http-client".into())
                                .spawn(move || {
                                    let _ = serve(stream, &s);
                                })
                            {
                                workers.push(w);
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => break,
                    }
                }
                for w in workers {
                    let _ = w.join();
                }
            })?;
        Ok(Self {
            shared,
            address,
            worker: Some(worker),
        })
    }
    /// Bound local address, suitable for a host-provided client.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
    /// Replace optional host-owned transcript presentation JSON. No callback runs on client threads.
    /// Revision must strictly increase. Cursor scopes provenance, not an exact text-to-byte map.
    /// The host must ensure the supplied projection coherently describes this fence.
    /// JSON input and stored copy are bounded; no update backlog is retained.
    pub fn publish_projection(
        &self,
        revision: u64,
        cursor: ReplayCursor,
        bytes: &[u8],
    ) -> io::Result<()> {
        if bytes.len() > self.shared.config.projection_bytes {
            return Err(io::Error::other("projection too large"));
        }
        let viewer = self
            .shared
            .hub
            .attach(Some(cursor))
            .map_err(io::Error::other)?;
        drop(viewer);
        let value: Value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
        // Canonical serialization can differ in size from the input representation.
        encode(&value, self.shared.config.projection_bytes)?;
        let mut p = self.shared.projection.lock().unwrap();
        if p.as_ref().is_some_and(|p| revision <= p.revision) {
            return Err(io::Error::other("stale projection revision"));
        }
        *p = Some(Projection {
            revision,
            cursor,
            json: value,
        });
        Ok(())
    }
    /// Idempotent; independent of viewer queue capacity. Joins within socket deadlines,
    /// except a client already capturing recovery can wait for the core recovery lock.
    pub fn shutdown(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if let Some(s) = Arc::get_mut(&mut self.shared) {
            s.hub.shutdown();
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn cursor(s: &Shared, c: ReplayCursor) -> String {
    format!("{}:{}:{}", s.incarnation, c.session.0, c.offset)
}
fn parse_cursor(s: &Shared, id: &str) -> io::Result<ReplayCursor> {
    if id.len() > 80 {
        return Err(io::Error::other("invalid cursor"));
    }
    let parts: Vec<_> = id.split(':').collect();
    if parts.len() != 3
        || parts[0] != s.incarnation
        || !parts[1..]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(io::Error::other("invalid cursor"));
    }
    Ok(ReplayCursor {
        session: SessionId(parts[1].parse().map_err(io::Error::other)?),
        offset: parts[2].parse().map_err(io::Error::other)?,
    })
}
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Bounded {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("frame too large"));
        }
        if b.len() > self.bytes.capacity().saturating_sub(self.bytes.len()) {
            self.bytes.reserve_exact(b.len());
        }
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode(value: &Value, limit: usize) -> io::Result<Vec<u8>> {
    let mut out = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut out, value).map_err(io::Error::other)?;
    Ok(out.bytes)
}
struct Charge<'a>(&'a AtomicUsize, usize);
impl Drop for Charge<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Ordering::AcqRel);
    }
}
fn send(stream: &mut TcpStream, s: &Shared, bytes: &[u8]) -> io::Result<()> {
    #[allow(deprecated, reason = "try_update is unavailable on the Rust 1.85 MSRV")]
    s.active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_add(bytes.len())
                .filter(|n| *n <= s.config.active_bytes)
        })
        .map_err(|_| io::Error::other("active write capacity"))?;
    let _charge = Charge(&s.active, bytes.len());
    let deadline = Instant::now() + s.config.io_timeout;
    let mut remaining = bytes;
    while !remaining.is_empty() {
        if s.stopped.load(Ordering::Acquire) {
            return Err(io::Error::other("server stopped"));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "write deadline"));
        }
        stream.set_write_timeout(Some(left))?;
        let n = stream.write(remaining)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write closed"));
        }
        remaining = &remaining[n..];
    }
    Ok(())
}
fn response(stream: &mut TcpStream, s: &Shared, status: &str, value: Value) -> io::Result<()> {
    let body = match encode(&value, s.config.frame_bytes) {
        Ok(b) => b,
        Err(_) => {
            return response(
                stream,
                s,
                "413 Content Too Large",
                json!({"error":"frame_limit","resync":true}),
            );
        }
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    send(stream, s, header.as_bytes())?;
    send(stream, s, &body)
}
fn fresh(s: &Shared) -> io::Result<(Value, ReplayCursor)> {
    let (snapshot, receiver) = s
        .terminal
        .attach_live_recovery_metadata()
        .map_err(io::Error::other)?;
    drop(receiver);
    let c = snapshot.boundary;
    Ok((
        json!({"cursor":cursor(s,c),"session":c.session.0,"offset":c.offset,"next_output_seq":snapshot.next_output_seq,"geometry":{"rows":snapshot.size.rows,"cols":snapshot.size.cols},"parser_checkpoint":false,"resize_history":false,"display":null}),
        c,
    ))
}
fn sse(
    stream: &mut TcpStream,
    s: &Shared,
    event: &str,
    id: Option<ReplayCursor>,
    value: Value,
) -> io::Result<()> {
    let mut bytes = Bounded {
        bytes: Vec::new(),
        limit: s.config.frame_bytes,
    };
    writeln!(bytes, "event: {event}")?;
    if let Some(id) = id {
        writeln!(bytes, "id: {}", cursor(s, id))?;
    }
    bytes.write_all(b"data: ")?;
    serde_json::to_writer(&mut bytes, &value).map_err(io::Error::other)?;
    bytes.write_all(b"\n\n")?;
    send(stream, s, &bytes.bytes)
}
fn output_frame(
    stream: &mut TcpStream,
    s: &Shared,
    start: ReplayCursor,
    end: ReplayCursor,
    seq: u64,
    raw: &[u8],
) -> io::Result<()> {
    let mut frame = Bounded {
        bytes: Vec::new(),
        limit: s.config.frame_bytes,
    };
    writeln!(frame, "event: output")?;
    writeln!(frame, "id: {}", cursor(s, end))?;
    write!(
        frame,
        "data: {{\"start\":{},\"end\":{},\"seq\":{},\"session\":{},\"bytes\":[",
        start.offset, end.offset, seq, start.session.0
    )?;
    for (i, byte) in raw.iter().enumerate() {
        if i != 0 {
            frame.write_all(b",")?;
        }
        write!(frame, "{byte}")?;
    }
    frame.write_all(b"]}\n\n")?;
    send(stream, s, &frame.bytes)
}
fn serve(mut stream: TcpStream, s: &Shared) -> io::Result<()> {
    let scratch_slot = s.config.frame_bytes + s.config.projection_bytes + s.config.header_bytes;
    #[allow(deprecated, reason = "try_update is unavailable on the Rust 1.85 MSRV")]
    s.scratch
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_add(scratch_slot)
                .filter(|n| *n <= s.config.scratch_bytes)
        })
        .map_err(|_| io::Error::other("serialization capacity"))?;
    let _scratch = Charge(&s.scratch, scratch_slot);
    stream.set_write_timeout(Some(s.config.io_timeout))?;
    let deadline = Instant::now() + s.config.io_timeout;
    let mut raw = Vec::new();
    loop {
        if s.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        stream.set_read_timeout(Some(left.min(Duration::from_millis(100))))?;
        let mut b = [0; 512];
        match stream.read(&mut b) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if n > s.config.header_bytes.saturating_sub(raw.len()) {
                    return response(
                        &mut stream,
                        s,
                        "431 Request Header Fields Too Large",
                        json!({"error":"headers"}),
                    );
                }
                raw.extend_from_slice(&b[..n]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    let parsed = request.parse(&raw).map_err(io::Error::other)?;
    if !parsed.is_complete() || request.method != Some("GET") || request.version != Some(1) {
        return response(
            &mut stream,
            s,
            "400 Bad Request",
            json!({"error":"request"}),
        );
    }
    let mut auth = None;
    let mut last = None;
    for h in request.headers.iter() {
        if h.name.eq_ignore_ascii_case("authorization") {
            if auth.is_some() {
                return Ok(());
            }
            auth = Some(h.value);
        }
        if h.name.eq_ignore_ascii_case("last-event-id") {
            if last.is_some() {
                return Ok(());
            }
            last = Some(h.value);
        }
        if h.name.eq_ignore_ascii_case("transfer-encoding")
            || (h.name.eq_ignore_ascii_case("content-length") && h.value != b"0")
        {
            return Ok(());
        }
    }
    let expected = format!("Bearer {}", s.config.token);
    let a = auth.unwrap_or_default();
    let valid = a.len() == expected.len()
        && a.iter()
            .zip(expected.bytes())
            .fold(0u8, |diff, (a, b)| diff | (*a ^ b))
            == 0;
    if !valid {
        return response(
            &mut stream,
            s,
            "401 Unauthorized",
            json!({"error":"authentication"}),
        );
    }
    match request.path {
        Some("/snapshot") => {
            let (mut value, _) = fresh(s)?;
            let p = s.projection.lock().unwrap();
            if let Some(p) = p.as_ref() {
                value["projection"] =
                    json!({"revision":p.revision,"cursor":cursor(s,p.cursor),"value":p.json});
            }
            drop(p);
            response(&mut stream, s, "200 OK", value)
        }
        Some("/events") => {
            let (snapshot, start) = if let Some(last) = last {
                let id = std::str::from_utf8(last).map_err(io::Error::other)?;
                match parse_cursor(s, id) {
                    Ok(c) => (None, c),
                    Err(_) => {
                        return response(
                            &mut stream,
                            s,
                            "409 Conflict",
                            json!({"error":"invalid_cursor","resync":true}),
                        );
                    }
                }
            } else {
                let (v, c) = fresh(s)?;
                (Some(v), c)
            };
            let until = Instant::now() + s.config.io_timeout;
            let viewer = loop {
                match s.hub.attach(Some(start)) {
                    Ok(v) => break v,
                    Err(observers::AttachError::Replay(ReplayError::FutureCursor { .. }))
                        if snapshot.is_some() && Instant::now() < until =>
                    {
                        if s.stopped.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => {
                        return response(
                            &mut stream,
                            s,
                            "409 Conflict",
                            json!({"error":"cursor_or_capacity","resync":true}),
                        );
                    }
                }
            };
            send(&mut stream,s,b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n")?;
            if let Some(gap) = &viewer.gap {
                return sse(
                    &mut stream,
                    s,
                    "resync",
                    None,
                    json!({"reason":"AdapterHistoryUnavailable","start":gap.start,"end":gap.end}),
                );
            }
            if let Some(snapshot) = snapshot {
                sse(&mut stream, s, "snapshot", Some(start), snapshot)?;
            }
            let end = Instant::now() + s.config.stream_lifetime;
            while !s.stopped.load(Ordering::Acquire) && Instant::now() < end {
                match viewer.recv_timeout(Duration::from_millis(100)) {
                    Some(observers::Item::Output {
                        start,
                        end,
                        seq,
                        bytes,
                    }) => {
                        if bytes.len() > s.config.frame_bytes.saturating_sub(1024) / 4 {
                            sse(
                                &mut stream,
                                s,
                                "resync",
                                None,
                                json!({"reason":"frame_limit"}),
                            )?;
                            break;
                        }
                        output_frame(&mut stream, s, start, end, seq, &bytes)?;
                    }
                    Some(observers::Item::Exit { session, status }) => {
                        sse(
                            &mut stream,
                            s,
                            "exit",
                            None,
                            json!({"session":session.0,"status":status}),
                        )?;
                        break;
                    }
                    None => {
                        if let Some(reason) = viewer.reason() {
                            sse(
                                &mut stream,
                                s,
                                "resync",
                                None,
                                json!({"reason":format!("{reason:?}")}),
                            )?;
                            break;
                        }
                    }
                }
            }
            viewer.detach();
            Ok(())
        }
        _ => response(&mut stream, s, "404 Not Found", json!({"error":"route"})),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agent_link::{PtySize, SpawnSpec};
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    fn request(server: &Server, path: &str, extra: &str, auth: bool) -> String {
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\n{}{}\r\n",
            if auth {
                format!("Authorization: Bearer {TOKEN}\r\n")
            } else {
                String::new()
            },
            extra
        )
        .unwrap();
        let mut result = String::new();
        stream.read_to_string(&mut result).unwrap();
        result
    }
    fn body(response: &str) -> Value {
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    }
    fn snapshot(server: &Server) -> Value {
        let r = request(server, "/snapshot", "", true);
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        body(&r)
    }
    #[test]
    fn local_authenticated_fences_raw_bytes_eviction_limits_and_shutdown() {
        let spec = SpawnSpec::direct(
            "/bin/sh",
            ["-c", "read line; printf '\\377\\033[31mhello\\n'; cat"],
            std::env::current_dir().unwrap(),
            PtySize { rows: 24, cols: 80 },
        );
        let (term, receiver) = Terminal::spawn(spec).unwrap();
        drop(receiver);
        let term = Arc::new(term);
        let mut config = Config::new(TOKEN.into());
        config.clients = 2;
        config.header_bytes = 512;
        config.io_timeout = Duration::from_millis(250);
        config.stream_lifetime = Duration::from_millis(500);
        config.observers.history_bytes = 1;
        let mut invalid = config.clone();
        invalid.bind = "0.0.0.0:0".parse().unwrap();
        assert!(Server::bind(term.clone(), invalid).is_err());
        let mut server = Server::bind(term.clone(), config).unwrap();
        assert!(request(&server, "/snapshot", "", false).starts_with("HTTP/1.1 401"));
        assert!(request(&server, "/snapshot?token=secret", "", true).starts_with("HTTP/1.1 404"));
        assert!(
            request(
                &server,
                "/snapshot",
                &format!("X-Fill: {}\r\n", "x".repeat(600)),
                true
            )
            .starts_with("HTTP/1.1 431")
        );
        let initial = snapshot(&server);
        let id = initial["cursor"].as_str().unwrap();
        assert_eq!(initial["parser_checkpoint"], false);
        assert_eq!(initial["geometry"]["rows"], 24);
        for invalid in [
            "foreign:1:0".to_owned(),
            format!("{}:{}:{}", server.shared.incarnation, term.id().0, u64::MAX),
            format!("{}:{}:0", server.shared.incarnation, term.id().0 + 1),
        ] {
            assert!(
                request(
                    &server,
                    "/events",
                    &format!("Last-Event-ID: {invalid}\r\n"),
                    true
                )
                .starts_with("HTTP/1.1 409")
            );
        }
        let fence = parse_cursor(&server.shared, id).unwrap();
        server
            .publish_projection(1, fence, br#"{"text":"<script>\n\u001b"}"#)
            .unwrap();
        assert!(server.publish_projection(1, fence, b"{}").is_err());
        assert!(
            server
                .publish_projection(2, fence, &vec![b' '; 65537])
                .is_err()
        );
        assert_eq!(snapshot(&server)["projection"]["revision"], 1);
        // No cursor means fresh atomic snapshot first, followed only by later bytes.
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "GET /events HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
        )
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\n\n") || !head.windows(15).any(|w| w == b"event: snapshot") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
            assert!(head.len() < 4096);
        }
        term.write(b"go\n").unwrap();
        let mut tail = String::new();
        stream.read_to_string(&mut tail).unwrap();
        let snapshot_data = String::from_utf8(head).unwrap();
        let snapshot_json: Value = serde_json::from_str(
            snapshot_data
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        let mut position = snapshot_json["offset"].as_u64().unwrap();
        let mut sequence = snapshot_json["next_output_seq"].as_u64().unwrap();
        let mut raw = Vec::new();
        for line in tail.lines().filter_map(|l| l.strip_prefix("data: ")) {
            let value: Value = serde_json::from_str(line).unwrap();
            if let Some(bytes) = value["bytes"].as_array() {
                assert_eq!(value["start"], position);
                assert_eq!(value["seq"], sequence);
                position += bytes.len() as u64;
                sequence += 1;
                assert_eq!(value["end"], position);
                raw.extend(bytes.iter().map(|n| n.as_u64().unwrap() as u8));
            }
        }
        assert!(
            raw.windows(b"\xff\x1b[31mhello\r\n".len())
                .any(|w| w == b"\xff\x1b[31mhello\r\n"),
            "raw output: {raw:?}"
        );
        let gap = request(
            &server,
            "/events",
            &format!("Last-Event-ID: {id}\r\n"),
            true,
        );
        assert!(gap.contains("event: resync"), "{gap}");
        assert!(gap.contains("AdapterHistoryUnavailable"));
        assert!(!gap.contains("event: output"));
        // The same bounded socket writer used by HTTP/SSE must time out for a peer
        // that never reads, while another authenticated client remains independent.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stalled_peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stalled_writer, _) = listener.accept().unwrap();
        let shared = server.shared.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, outcome) = std::sync::mpsc::channel();
        let writer = thread::spawn(move || {
            let payload = vec![0; shared.config.frame_bytes];
            started.send(()).unwrap();
            let timed_out = (0..128).any(|_| send(&mut stalled_writer, &shared, &payload).is_err());
            let _ = finished.send(timed_out);
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(snapshot(&server)["session"], term.id().0);
        let result = outcome.recv_timeout(Duration::from_secs(3));
        drop(stalled_peer); // Always release the peer before asserting/joining.
        writer.join().unwrap();
        assert!(
            result.unwrap(),
            "unread peer did not hit a finite write limit"
        );
        // Partial unauthenticated clients count toward finite admission and cannot block shutdown.
        let mut partial = TcpStream::connect(server.local_addr()).unwrap();
        partial.write_all(b"GET /snapshot HTTP/1.1\r\n").unwrap();
        assert_eq!(snapshot(&server)["session"], term.id().0);
        let mut second_partial = TcpStream::connect(server.local_addr()).unwrap();
        second_partial
            .write_all(b"GET /snapshot HTTP/1.1\r\n")
            .unwrap();
        let admitted = Instant::now() + Duration::from_secs(2);
        while server.shared.scratch.load(Ordering::Acquire)
            != 2 * (server.shared.config.frame_bytes
                + server.shared.config.projection_bytes
                + server.shared.config.header_bytes)
        {
            assert!(Instant::now() < admitted, "partial clients not admitted");
            thread::yield_now();
        }
        let mut refused = TcpStream::connect(server.local_addr()).unwrap();
        refused
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut b = [0];
        assert!(matches!(refused.read(&mut b), Ok(0)));
        let before = Instant::now();
        server.shutdown();
        server.shutdown();
        assert!(before.elapsed() < Duration::from_secs(2));
        assert!(server.shared.hub.attach(Some(fence)).is_err());
        drop(partial);
        drop(second_partial);
        drop(server);
        // A bounded initial snapshot refuses oversized optional projection JSON.
        let mut config = Config::new(TOKEN.into());
        config.frame_bytes = 1024;
        config.io_timeout = Duration::from_millis(250);
        config.stream_lifetime = Duration::from_millis(300);
        let mut small = Server::bind(term.clone(), config).unwrap();
        let snap = snapshot(&small);
        let fence = parse_cursor(&small.shared, snap["cursor"].as_str().unwrap()).unwrap();
        let projection = serde_json::to_vec(&json!({"text":"x".repeat(2000)})).unwrap();
        small.publish_projection(1, fence, &projection).unwrap();
        assert!(request(&small, "/snapshot", "", true).starts_with("HTTP/1.1 413"));
        small.shutdown();
        drop(small);
        drop(term);
    }
    #[test]
    fn bounded_encoding_rejects_before_exceeding_budget() {
        let mut out = Bounded {
            bytes: Vec::new(),
            limit: 3,
        };
        assert!(out.write_all(b"four").is_err());
        assert!(out.bytes.is_empty());
        assert!(encode(&json!({"text":"a".repeat(1000)}), 32).is_err());
    }
}
