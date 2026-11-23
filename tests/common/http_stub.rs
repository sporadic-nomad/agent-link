//! A std-only HTTP/1.1 stub server on a loopback port for the loopback client's tests:
//! scripted replies per request (fixed, chunked, a server-sent events stream, a hang, a
//! slow drip, an oversized or never-ending body, a close without an answer), and a log of
//! the requests it received (method, path, the `Authorization` header and body as sent). Used by
//! the library's unit tests, the integration tests and the fake agent (which serves an
//! OpenCode-like API from the launched process), so it depends on nothing but `std`.
#![allow(dead_code)]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

/// How long a connection waits for a request before it gives up.
const IDLE: Duration = Duration::from_secs(30);
/// How often a waiting connection looks at the stop flag.
const TICK: Duration = Duration::from_millis(10);

/// One request as received.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// The `Authorization` header's value, as sent.
    pub authorization: Option<String>,
    /// The body, as sent (at most 1 MiB is read).
    pub body: Vec<u8>,
}

/// How the stub answers one request.
pub enum Reply {
    /// `status` with `body` (`Content-Length`), the connection kept open unless `close`.
    Fixed {
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
        close: bool,
    },
    /// `status` with a chunked body of these pieces; the connection kept open.
    Chunked { status: u16, chunks: Vec<Vec<u8>> },
    /// A chunked `text/event-stream`: every message the receiver gets is written as one
    /// chunk; the body ends (last chunk) once every sender is gone, then the connection
    /// closes.
    Events(Receiver<Vec<u8>>),
    /// A head promising `length` bytes of JSON, then only its first byte: the body never
    /// completes.
    Stall { status: u16, length: u128 },
    /// `status`, then `body` one byte every `every` (`Content-Length` set).
    Drip {
        status: u16,
        body: Vec<u8>,
        every: Duration,
    },
    /// A chunked `200` body that never ends: a chunk of `chunk` bytes every `every`.
    Endless { chunk: usize, every: Duration },
    /// No answer at all; the connection is held until the stub stops.
    Hang,
    /// The connection is closed without an answer.
    Close,
}

impl Reply {
    /// `status` with `body` as JSON.
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::Fixed {
            status,
            content_type: "application/json",
            body: body.into(),
            close: false,
        }
    }

    /// `status` with a short text body.
    pub fn status(status: u16) -> Self {
        Self::Fixed {
            status,
            content_type: "text/plain",
            body: b"stub".to_vec(),
            close: false,
        }
    }
}

type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

/// A running stub; dropping it stops it (its threads end within a few milliseconds).
pub struct Stub {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Stub {
    /// A stub on a free `127.0.0.1` port answering with `handler`.
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        Self::bind((Ipv4Addr::LOCALHOST, 0).into(), handler).expect("bind a loopback port")
    }

    /// A stub on `addr`.
    pub fn bind(
        addr: SocketAddr,
        handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let (stopped, log) = (Arc::clone(&stop), Arc::clone(&requests));
        thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let (handler, stopped, log) =
                            (Arc::clone(&handler), Arc::clone(&stopped), Arc::clone(&log));
                        thread::spawn(move || serve(socket, &*handler, &stopped, &log));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
                    Err(_) => thread::sleep(TICK),
                }
            }
        });
        Ok(Self {
            addr,
            stop,
            requests,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The requests received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The requests received so far for `path`.
    pub fn hits(&self, path: &str) -> usize {
        self.requests().iter().filter(|r| r.path == path).count()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Serves one connection: requests in turn until a reply closes it, the peer leaves, or
/// the stub stops.
fn serve(socket: TcpStream, handler: &Handler, stop: &AtomicBool, log: &Mutex<Vec<Request>>) {
    let _ = socket.set_nonblocking(false);
    let _ = socket.set_read_timeout(Some(TICK));
    let mut reader = BufReader::new(match socket.try_clone() {
        Ok(socket) => socket,
        Err(_) => return,
    });
    let mut out = socket;
    while let Some(request) = read_request(&mut reader, stop) {
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        let keep = match handler(&request) {
            Reply::Fixed {
                status,
                content_type,
                body,
                close,
            } => {
                let connection = if close { "Connection: close\r\n" } else { "" };
                let head = format!(
                    "HTTP/1.1 {status} Stub\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{connection}\r\n",
                    body.len()
                );
                out.write_all(head.as_bytes()).is_ok() && out.write_all(&body).is_ok() && !close
            }
            Reply::Chunked { status, chunks } => {
                let head = format!(
                    "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
                );
                let mut ok = out.write_all(head.as_bytes()).is_ok();
                for chunk in chunks.iter().filter(|c| !c.is_empty()) {
                    ok = ok && write_chunk(&mut out, chunk).is_ok();
                }
                ok && out.write_all(b"0\r\n\r\n").is_ok()
            }
            Reply::Events(events) => {
                stream_events(&mut out, &events, stop);
                false
            }
            Reply::Stall { status, length } => {
                let head = format!(
                    "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n["
                );
                let _ = out.write_all(head.as_bytes());
                hold(stop);
                false
            }
            Reply::Drip {
                status,
                body,
                every,
            } => {
                let head = format!(
                    "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let mut ok = out.write_all(head.as_bytes()).is_ok();
                for byte in &body {
                    if !ok || stop.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(every);
                    ok = out.write_all(std::slice::from_ref(byte)).is_ok();
                }
                ok
            }
            Reply::Endless { chunk, every } => {
                let head = "HTTP/1.1 200 Stub\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
                let mut ok = out.write_all(head.as_bytes()).is_ok();
                let piece = vec![b' '; chunk.max(1)];
                while ok && !stop.load(Ordering::SeqCst) {
                    ok = write_chunk(&mut out, &piece).is_ok();
                    thread::sleep(every);
                }
                false
            }
            Reply::Hang => {
                hold(stop);
                false
            }
            Reply::Close => false,
        };
        if !keep {
            break;
        }
    }
    let _ = out.shutdown(Shutdown::Both);
}

fn hold(stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(TICK);
    }
}

fn write_chunk(out: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    out.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
    out.write_all(data)?;
    out.write_all(b"\r\n")
}

fn stream_events(out: &mut TcpStream, events: &Receiver<Vec<u8>>, stop: &AtomicBool) {
    let head = "HTTP/1.1 200 Stub\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n";
    if out.write_all(head.as_bytes()).is_err() {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match events.recv_timeout(TICK) {
            Ok(data) => {
                if write_chunk(out, &data).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = out.write_all(b"0\r\n\r\n");
                return;
            }
        }
    }
}

/// The next request's head (and a body, read and dropped); `None` when the peer left, it
/// was idle too long, or the stub stopped.
fn read_request(reader: &mut BufReader<TcpStream>, stop: &AtomicBool) -> Option<Request> {
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut idle = Duration::ZERO;
    loop {
        if stop.load(Ordering::SeqCst) || idle > IDLE {
            return None;
        }
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return None,
            Ok(_) if line.ends_with(b"\n") => {
                let text = String::from_utf8_lossy(&line).trim_end().to_string();
                line.clear();
                if text.is_empty() {
                    if lines.is_empty() {
                        continue;
                    }
                    break;
                }
                lines.push(text);
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                idle += TICK;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    let mut first = lines.first()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let header = |name: &str| {
        lines.iter().skip(1).find_map(|l| {
            let (key, value) = l.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    let length: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length.min(1 << 20)];
    let _ = reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(5)));
    let _ = reader.read_exact(&mut body);
    let _ = reader.get_mut().set_read_timeout(Some(TICK));
    Some(Request {
        method,
        path,
        authorization: header("authorization"),
        body,
    })
}

/// Server-sent event subscribers: every stream a [`subscribe`](Self::subscribe) opened
/// gets what [`send`](Self::send) sends.
#[derive(Default)]
pub struct Events {
    senders: Mutex<Vec<Sender<Vec<u8>>>>,
}

impl Events {
    /// A new stream (an `/event` request's reply).
    pub fn subscribe(&self) -> Reply {
        let (tx, rx) = mpsc::channel();
        self.lock().push(tx);
        Reply::Events(rx)
    }

    /// `data` as one `data:` line and its blank line, to every open stream.
    pub fn send(&self, data: &str) {
        self.send_raw(format!("data: {data}\n\n").as_bytes());
    }

    /// `bytes` as they are, to every open stream.
    pub fn send_raw(&self, bytes: &[u8]) {
        self.lock().retain(|tx| tx.send(bytes.to_vec()).is_ok());
    }

    /// Ends every open stream.
    pub fn close_all(&self) {
        self.lock().clear();
    }

    /// Streams open now.
    pub fn open(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Sender<Vec<u8>>>> {
        self.senders.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The `Authorization` value of Basic auth as `user:password`.
pub fn basic(user: &str, password: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = format!("{user}:{password}").into_bytes();
    let mut out = String::from("Basic ");
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
