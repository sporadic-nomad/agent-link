//! Bounded HTTP/1.1 exchanges with a peer on a loopback socket: the OpenCode question
//! feed's (`questions`) and the OpenCode ambient turn feed's (`turns`).
//!
//! - **Loopback only by construction.** A connection is made from a [`SocketAddr`] whose
//!   IP is a loopback address ([`connect`]); nothing resolves a name, reads a proxy
//!   setting or follows a redirect (a 3xx is an ordinary status for the caller).
//! - **Every wait is bounded.** Each exchange runs under one deadline covering the
//!   request write, the response head and its body. An event stream's reads stop at the
//!   caller's deadline or after a short quiet wait.
//! - **Every read is bounded.** Response heads are at most 16 KiB with at most 64 header
//!   fields; bodies at most the exchange's cap (`Content-Length`, chunked or, where the
//!   peer closes the connection, close-delimited); event-stream lines at most the
//!   caller's line limit, longer lines skipped without being buffered.
//! - **Credentials only in the `Authorization` header**, only when an exchange asks for
//!   them; the type's `Debug` never shows them.
//!
//! [`owner`] tells which local process serves the far end of such a connection.

pub(in crate::agent) mod owner;

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 16 * 1024;
const MAX_CHUNKS: usize = 2048;
/// The question feed's per-exchange body limit.
#[cfg(feature = "questions")]
const QUESTION_BODY: usize = 256 * 1024;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid loopback HTTP response")
}

fn timed_out() -> io::Error {
    io::Error::from(io::ErrorKind::TimedOut)
}

/// A response body over its exchange's cap.
#[derive(Debug)]
struct TooLarge;

impl fmt::Display for TooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("loopback HTTP response body over its limit")
    }
}

impl std::error::Error for TooLarge {}

fn too_large_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, TooLarge)
}

/// Whether `e` failed an exchange because its response body exceeded the cap.
pub(in crate::agent) fn too_large(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<TooLarge>())
}

/// Connects to `addr`, which must be a loopback address with a nonzero port
/// ([`io::ErrorKind::InvalidInput`] otherwise), within `until`.
pub(in crate::agent) fn connect(addr: SocketAddr, until: Instant) -> io::Result<TcpStream> {
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let wait = until
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(timed_out)?;
    let stream = TcpStream::connect_timeout(&addr, wait)?;
    if stream.peer_addr()? != addr || !stream.local_addr()?.ip().is_loopback() {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(stream)
}

struct Socket {
    stream: TcpStream,
    until: Instant,
}

impl Socket {
    fn remaining(&self) -> io::Result<Duration> {
        self.until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(timed_out)
    }
}

impl Read for Socket {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        // A blocking socket's elapsed read timeout reads `WouldBlock` on Unix.
        self.stream.read(data).map_err(|e| match e.kind() {
            io::ErrorKind::WouldBlock => timed_out(),
            _ => e,
        })
    }
}

impl Write for Socket {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(data).map_err(|e| match e.kind() {
            io::ErrorKind::WouldBlock => timed_out(),
            _ => e,
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// One request on a [`Connection`].
pub(in crate::agent) struct Exchange<'a> {
    /// The request target: a path (and query) on the peer.
    pub(in crate::agent) path: &'a str,
    /// A JSON body to `POST`; `None`: a `GET`.
    pub(in crate::agent) post: Option<&'a [u8]>,
    /// Send the connection's Basic credentials.
    pub(in crate::agent) auth: bool,
    /// The largest response body read; a larger one fails the exchange.
    pub(in crate::agent) cap: usize,
    /// Write, head and body all complete by then.
    pub(in crate::agent) until: Instant,
}

/// What an event-stream request got.
pub(in crate::agent) enum Opened {
    /// A `200`: the stream.
    Events(EventStream),
    /// Another status. The connection is closed after the response head.
    Status(u16),
}

/// One persistent HTTP/1.1 connection. An exchange that fails, or a response that closes
/// the connection, closes it for good. `Debug` shows the peer only.
pub(in crate::agent) struct Connection {
    reader: BufReader<Socket>,
    peer: SocketAddr,
    basic: String,
    closed: bool,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("peer", &self.peer)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Connection {
    pub(in crate::agent) fn is_closed(&self) -> bool {
        self.closed
    }

    /// `basic`: the base64 `user:password` of the `Authorization: Basic` header.
    pub(in crate::agent) fn new(stream: TcpStream, peer: SocketAddr, basic: String) -> Self {
        Self {
            reader: BufReader::new(Socket {
                stream,
                until: Instant::now(),
            }),
            peer,
            basic,
            closed: false,
        }
    }

    /// An authenticated question-feed exchange with at most 256 KiB of body.
    #[cfg(feature = "questions")]
    pub(in crate::agent) fn exchange(
        &mut self,
        path: &str,
        post: Option<&[u8]>,
        until: Instant,
    ) -> io::Result<(u16, Vec<u8>)> {
        self.request(&Exchange {
            path,
            post,
            auth: true,
            cap: QUESTION_BODY,
            until,
        })
    }

    /// One exchange: its status and body. An authenticated 401 or 403 closes the
    /// connection at the response head and returns an empty body.
    pub(in crate::agent) fn request(
        &mut self,
        exchange: &Exchange<'_>,
    ) -> io::Result<(u16, Vec<u8>)> {
        if self.closed {
            return Err(io::ErrorKind::NotConnected.into());
        }
        self.reader.get_mut().until = exchange.until;
        let result = self.round_trip(exchange);
        if result.is_err() || self.closed {
            self.close();
        }
        result
    }

    /// An unauthenticated GET whose status alone decides whether to proceed. Dropping
    /// this socket after the bounded response head discards any unread body.
    pub(in crate::agent) fn probe(mut self, path: &str, until: Instant) -> io::Result<u16> {
        if self.closed {
            return Err(io::ErrorKind::NotConnected.into());
        }
        self.reader.get_mut().until = until;
        self.write_head(path, None, false, "application/json")?;
        let (status, _) = self.read_head(|_| false)?;
        Ok(status)
    }

    /// `GET path` with the credentials, as a server-sent events stream. The head must come
    /// by `until`; the stream's reads then run on [`EventStream::drain`]'s deadlines.
    pub(in crate::agent) fn into_events(
        mut self,
        path: &str,
        until: Instant,
    ) -> io::Result<Opened> {
        if self.closed {
            return Err(io::ErrorKind::NotConnected.into());
        }
        self.reader.get_mut().until = until;
        let head = self
            .write_head(path, None, true, "text/event-stream")
            .and_then(|()| self.read_head(|status| status == 200));
        let (status, head) = match head {
            Ok(head) => head,
            Err(e) => {
                self.close();
                return Err(e);
            }
        };
        let Some(head) = head else {
            self.close();
            return Ok(Opened::Status(status));
        };
        let framing = match (head.chunked, head.length) {
            (true, None) => Framing::Chunked(Chunked::default()),
            (false, Some(n)) => Framing::Length(n as u64),
            (false, None) => Framing::Close,
            _ => {
                self.close();
                return Err(invalid());
            }
        };
        // Keep any body bytes already buffered while reading the response head.
        Ok(Opened::Events(EventStream {
            reader: self.reader,
            framing,
            lines: Lines::default(),
            ended: None,
        }))
    }

    fn close(&mut self) {
        self.closed = true;
        let _ = self.reader.get_ref().stream.shutdown(Shutdown::Both);
    }

    fn write_head(
        &mut self,
        path: &str,
        post: Option<&[u8]>,
        auth: bool,
        accept: &str,
    ) -> io::Result<()> {
        let method = if post.is_some() { "POST" } else { "GET" };
        let mut header = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n", self.peer);
        if auth {
            header.push_str("Authorization: Basic ");
            header.push_str(&self.basic);
            header.push_str("\r\n");
        }
        header.push_str("Accept: ");
        header.push_str(accept);
        header.push_str("\r\n");
        if let Some(body) = post {
            header.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        header.push_str("\r\n");
        self.reader.get_mut().write_all(header.as_bytes())?;
        if let Some(body) = post {
            self.reader.get_mut().write_all(body)?;
        }
        Ok(())
    }

    fn round_trip(&mut self, exchange: &Exchange<'_>) -> io::Result<(u16, Vec<u8>)> {
        self.write_head(
            exchange.path,
            exchange.post,
            exchange.auth,
            "application/json",
        )?;
        let (status, head) =
            self.read_head(|status| !exchange.auth || !matches!(status, 401 | 403))?;
        let Some(head) = head else {
            return Ok((status, Vec::new()));
        };
        let body = self.body(&head, exchange.cap)?;
        if !self.reader.buffer().is_empty() {
            return Err(invalid());
        }
        Ok((status, body))
    }

    /// Validate the complete head. Interpret body framing only for a status whose
    /// body the caller will read. A status-only response ends the connection.
    fn read_head(
        &mut self,
        read_body: impl FnOnce(u16) -> bool,
    ) -> io::Result<(u16, Option<BodyHeaders>)> {
        let mut raw = Vec::new();
        loop {
            let start = raw.len();
            read_line(&mut self.reader, &mut raw, MAX_HEAD)?;
            if raw.len() == start + 2 {
                break;
            }
        }
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        if response.parse(&raw).map_err(|_| invalid())? != httparse::Status::Complete(raw.len()) {
            return Err(invalid());
        }
        let code = response.code.ok_or_else(invalid)?;
        if !read_body(code) {
            self.closed = true;
            return Ok((code, None));
        }
        let mut length = None;
        let mut chunked = false;
        for field in response.headers.iter() {
            if field.name.eq_ignore_ascii_case("Content-Length") {
                if length.is_some()
                    || field.value.is_empty()
                    || !field.value.iter().all(u8::is_ascii_digit)
                {
                    return Err(invalid());
                }
                length = Some(
                    std::str::from_utf8(field.value)
                        .map_err(|_| invalid())?
                        .parse::<usize>()
                        .map_err(|_| invalid())?,
                );
            } else if field.name.eq_ignore_ascii_case("Transfer-Encoding") {
                if chunked || !field.value.eq_ignore_ascii_case(b"chunked") {
                    return Err(invalid());
                }
                chunked = true;
            } else if field.name.eq_ignore_ascii_case("Connection") {
                self.closed |= field
                    .value
                    .split(|&b| b == b',')
                    .any(|v| v.trim_ascii().eq_ignore_ascii_case(b"close"));
            }
        }
        Ok((
            code,
            Some(BodyHeaders {
                length,
                chunked,
                http10: response.version == Some(0),
            }),
        ))
    }

    fn body(&mut self, head: &BodyHeaders, cap: usize) -> io::Result<Vec<u8>> {
        let mut body = Vec::new();
        match (head.chunked, head.length) {
            (true, None) => read_chunks(&mut self.reader, &mut body, cap)?,
            (false, Some(n)) if n <= cap => {
                body.resize(n, 0);
                self.reader.read_exact(&mut body)?;
            }
            (false, Some(_)) => return Err(too_large_error()),
            // A close-delimited response cannot be followed by another exchange.
            (false, None) if self.closed || head.http10 => {
                self.closed = true;
                self.reader
                    .by_ref()
                    .take((cap as u64).saturating_add(1))
                    .read_to_end(&mut body)?;
                if body.len() > cap {
                    return Err(too_large_error());
                }
            }
            _ => return Err(invalid()),
        }
        Ok(body)
    }
}

struct BodyHeaders {
    length: Option<usize>,
    chunked: bool,
    http10: bool,
}

/// Append one CRLF-terminated line without growing the buffer past the caller's cap.
fn read_line(reader: &mut impl BufRead, target: &mut Vec<u8>, cap: usize) -> io::Result<()> {
    let start = target.len();
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let newline = bytes.iter().position(|&b| b == b'\n');
        let n = newline.map_or(bytes.len(), |end| end + 1);
        if n > cap.saturating_sub(target.len()) {
            return Err(invalid());
        }
        target.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if newline.is_some() {
            return if target[start..].ends_with(b"\r\n") {
                Ok(())
            } else {
                Err(invalid())
            };
        }
    }
}

fn read_chunks(reader: &mut impl BufRead, body: &mut Vec<u8>, cap: usize) -> io::Result<()> {
    let mut line = Vec::new();
    // Every chunk but the last carries data, so a body under the cap needs no more.
    let chunks = MAX_CHUNKS.max(cap / 1024);
    for _ in 0..=chunks {
        line.clear();
        read_line(reader, &mut line, 128)?;
        let size = &line[..line.len() - 2];
        if size.is_empty() || size.len() > 16 || !size.iter().all(u8::is_ascii_hexdigit) {
            return Err(invalid());
        }
        let n = usize::from_str_radix(std::str::from_utf8(size).map_err(|_| invalid())?, 16)
            .map_err(|_| invalid())?;
        if n == 0 {
            line.clear();
            read_line(reader, &mut line, 2)?;
            return Ok(());
        }
        if n > cap - body.len() {
            return Err(too_large_error());
        }
        let start = body.len();
        body.resize(start + n, 0);
        reader.read_exact(&mut body[start..])?;
        let mut end = [0; 2];
        reader.read_exact(&mut end)?;
        if end != *b"\r\n" {
            return Err(invalid());
        }
    }
    Err(invalid())
}

/// How an event stream's body is delimited.
enum Framing {
    Chunked(Chunked),
    /// This many body bytes are left.
    Length(u64),
    /// Until the peer closes the connection.
    Close,
}

impl Framing {
    /// Hands the body bytes in `input` to `out`: the bytes consumed, and whether the body
    /// ended with them.
    fn take(
        &mut self,
        input: &[u8],
        out: &mut impl FnMut(&[u8]) -> usize,
    ) -> io::Result<(usize, bool)> {
        match self {
            Self::Chunked(chunked) => {
                let n = chunked.decode(input, out)?;
                Ok((n, chunked.done()))
            }
            Self::Length(left) => {
                let n = input
                    .len()
                    .min(usize::try_from(*left).unwrap_or(usize::MAX));
                let n = out(&input[..n]);
                *left -= n as u64;
                Ok((n, *left == 0))
            }
            Self::Close => Ok((out(input), false)),
        }
    }
}

/// An incremental chunked-transfer decoder: input arrives in arbitrary pieces, and a
/// piece can end anywhere (inside a size line or a chunk's data).
#[derive(Default)]
struct Chunked {
    state: ChunkState,
    /// The size read so far on the current size line.
    size: u64,
    /// Bytes of the current size line (digits and extension).
    line: usize,
}

#[derive(Clone, Copy, Default)]
enum ChunkState {
    /// The chunk size's hex digits.
    #[default]
    Size,
    /// A chunk extension, up to the CR.
    Extension,
    /// The LF after the size line's CR.
    SizeLf,
    /// This many data bytes are left in the chunk.
    Data(u64),
    /// The CR after a chunk's data.
    DataCr,
    /// The LF after it.
    DataLf,
    /// The last chunk came: the body ended (trailers are not read).
    Done,
}

impl Chunked {
    /// Decodes from `input`, handing body bytes to `out`. Returns the bytes consumed: up to
    /// the body end or the first body byte the sink left for its next drain.
    fn decode(&mut self, input: &[u8], out: &mut impl FnMut(&[u8]) -> usize) -> io::Result<usize> {
        let mut at = 0;
        while at < input.len() {
            match self.state {
                ChunkState::Done => return Ok(at),
                ChunkState::Data(left) => {
                    let n = (input.len() - at).min(usize::try_from(left).unwrap_or(usize::MAX));
                    let used = out(&input[at..at + n]);
                    at += used;
                    let left = left - used as u64;
                    self.state = if left == 0 {
                        ChunkState::DataCr
                    } else {
                        ChunkState::Data(left)
                    };
                    if used < n {
                        return Ok(at);
                    }
                    continue;
                }
                _ => {}
            }
            let byte = input[at];
            at += 1;
            self.state = match self.state {
                ChunkState::Size => {
                    self.line += 1;
                    match (byte as char).to_digit(16) {
                        Some(digit) if self.line <= 16 => {
                            self.size = self.size * 16 + u64::from(digit);
                            ChunkState::Size
                        }
                        Some(_) => return Err(invalid()),
                        None if self.line == 1 => return Err(invalid()),
                        None if byte == b';' => ChunkState::Extension,
                        None if byte == b'\r' => ChunkState::SizeLf,
                        None => return Err(invalid()),
                    }
                }
                ChunkState::Extension => {
                    self.line += 1;
                    if self.line > 128 || byte == b'\n' {
                        return Err(invalid());
                    }
                    if byte == b'\r' {
                        ChunkState::SizeLf
                    } else {
                        ChunkState::Extension
                    }
                }
                ChunkState::SizeLf if byte == b'\n' => {
                    let n = std::mem::take(&mut self.size);
                    self.line = 0;
                    if n == 0 {
                        self.state = ChunkState::Done;
                        return Ok(at);
                    }
                    ChunkState::Data(n)
                }
                ChunkState::DataCr if byte == b'\r' => ChunkState::DataLf,
                ChunkState::DataLf if byte == b'\n' => ChunkState::Size,
                _ => return Err(invalid()),
            };
        }
        Ok(at)
    }

    fn done(&self) -> bool {
        matches!(self.state, ChunkState::Done)
    }
}

/// Splits an event stream's body into lines, at most a line limit long; a longer line is
/// dropped as it arrives (never buffered past the limit) and counted.
#[derive(Default)]
struct Lines {
    line: Vec<u8>,
    /// The line being read is over the limit: dropped up to its newline.
    skipping: bool,
}

impl Lines {
    fn push(&mut self, mut bytes: &[u8], limits: &DrainLimits, drained: &mut Drained) -> usize {
        let len = bytes.len();
        let limit = limits.line_bytes;
        while !bytes.is_empty() && drained.records() < limits.lines {
            let (part, complete) = match bytes.iter().position(|&b| b == b'\n') {
                Some(end) => (&bytes[..end], true),
                None => (bytes, false),
            };
            bytes = &bytes[part.len() + usize::from(complete)..];
            // One more byte than the limit for a CR before the LF.
            if !self.skipping && self.line.len() + part.len() > limit.saturating_add(1) {
                self.skipping = true;
                self.line = Vec::new();
            }
            if !self.skipping {
                self.line.extend_from_slice(part);
            }
            if !complete {
                continue;
            }
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if std::mem::take(&mut self.skipping) || line.len() > limit {
                drained.oversized = drained.oversized.saturating_add(1);
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                drained.data.push(data.to_vec());
            }
        }
        len - bytes.len()
    }
}

/// How an event stream ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum StreamEnd {
    /// The peer closed it or its body ended.
    Closed,
    /// Reading or decoding it failed.
    Failed(io::ErrorKind),
}

/// The bounds of one [`EventStream::drain`].
pub(in crate::agent) struct DrainLimits {
    /// Reading stops by then.
    pub(in crate::agent) until: Instant,
    /// A read that waited this long for nothing ends the drain (the stream stays open).
    pub(in crate::agent) quiet: Duration,
    /// The longest line kept, its line ending excluded.
    pub(in crate::agent) line_bytes: usize,
    /// At most this many `data:` payloads and oversized lines.
    pub(in crate::agent) lines: usize,
    /// At most this many bytes, including transfer framing.
    pub(in crate::agent) bytes: usize,
}

/// What one drain read.
#[derive(Debug, Default)]
pub(in crate::agent) struct Drained {
    /// The `data:` payloads, in order, each at most the line limit (comments, `event:`,
    /// `id:` and blank lines carry nothing read here).
    pub(in crate::agent) data: Vec<Vec<u8>>,
    /// Lines skipped for their length.
    pub(in crate::agent) oversized: u32,
    /// The stream ended.
    pub(in crate::agent) ended: Option<StreamEnd>,
    /// A limit ended the drain, not a quiet read: more may be buffered.
    pub(in crate::agent) cut: bool,
}

impl Drained {
    fn records(&self) -> usize {
        self.data.len().saturating_add(self.oversized as usize)
    }
}

/// A server-sent events response being read. A partial line is kept until its newline,
/// across drains.
pub(in crate::agent) struct EventStream {
    reader: BufReader<Socket>,
    framing: Framing,
    lines: Lines,
    ended: Option<StreamEnd>,
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl EventStream {
    /// Reads what arrived, within `limits`.
    pub(in crate::agent) fn drain(&mut self, limits: &DrainLimits) -> Drained {
        let mut drained = Drained::default();
        let Self {
            reader,
            framing,
            lines,
            ended,
        } = self;
        let mut read = 0;
        while ended.is_none() {
            if drained.records() >= limits.lines || read >= limits.bytes {
                drained.cut = true;
                break;
            }
            let now = Instant::now();
            let Some(left) = limits
                .until
                .checked_duration_since(now)
                .filter(|d| !d.is_zero())
            else {
                drained.cut = true;
                break;
            };
            reader.get_mut().until = now + limits.quiet.min(left);
            let input = match reader.fill_buf() {
                Ok([]) => {
                    *ended = Some(StreamEnd::Closed);
                    break;
                }
                Ok(input) => input,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // Nothing came for the quiet wait (or the deadline): over for now.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    break;
                }
                Err(e) => {
                    *ended = Some(StreamEnd::Failed(e.kind()));
                    break;
                }
            };
            let input = &input[..input.len().min(limits.bytes - read)];
            let mut sink = |body: &[u8]| lines.push(body, limits, &mut drained);
            match framing.take(input, &mut sink) {
                Ok((n, done)) => {
                    reader.consume(n);
                    read += n;
                    if done {
                        *ended = Some(StreamEnd::Closed);
                    }
                }
                Err(e) => *ended = Some(StreamEnd::Failed(e.kind())),
            }
        }
        drained.ended = *ended;
        if ended.is_some() {
            drained.cut = false;
            let _ = reader.get_ref().stream.shutdown(Shutdown::Both);
        }
        drained
    }
}

/// The std-only stub server of the loopback and feed tests.
#[cfg(test)]
#[path = "../../../tests/common/http_stub.rs"]
pub(in crate::agent) mod http_stub;
/// The synthetic OpenCode-like server on it, of the OpenCode feed tests (turns and
/// questions).
#[cfg(test)]
#[path = "../../../tests/common/opencode_stub.rs"]
pub(in crate::agent) mod opencode_stub;
#[cfg(test)]
mod tests;
