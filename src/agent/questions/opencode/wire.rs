use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 256 * 1024;
const MAX_CHUNKS: usize = 2048;
const TIMEOUT: Duration = Duration::from_secs(3);

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid question response")
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
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }
}

impl Read for Socket {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(data)
    }
}

impl Write for Socket {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

pub(super) struct Connection {
    reader: BufReader<Socket>,
    peer: SocketAddr,
    basic: String,
    closed: bool,
}

impl Connection {
    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(super) fn new(stream: TcpStream, peer: SocketAddr, basic: String) -> Self {
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

    pub(super) fn exchange(
        &mut self,
        path: &str,
        post: Option<&[u8]>,
    ) -> io::Result<(u16, Vec<u8>)> {
        if self.closed {
            return Err(io::ErrorKind::NotConnected.into());
        }
        self.reader.get_mut().until = Instant::now() + TIMEOUT;
        let result = self.request(path, post);
        if result.is_err() || self.closed {
            self.closed = true;
            let _ = self.reader.get_ref().stream.shutdown(Shutdown::Both);
        }
        result
    }

    fn request(&mut self, path: &str, post: Option<&[u8]>) -> io::Result<(u16, Vec<u8>)> {
        let method = if post.is_some() { "POST" } else { "GET" };
        let mut header = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {}\r\nAccept: application/json\r\n",
            self.peer, self.basic
        );
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
        let mut body = Vec::new();
        match (chunked, length) {
            (true, None) => read_chunks(&mut self.reader, &mut body)?,
            (false, Some(n)) if n <= MAX_BODY => {
                body.resize(n, 0);
                self.reader.read_exact(&mut body)?;
            }
            // A close-delimited response cannot be used for the subsequent answer.
            (false, None) if self.closed || response.version == Some(0) => {
                self.closed = true;
                self.reader
                    .by_ref()
                    .take((MAX_BODY + 1) as u64)
                    .read_to_end(&mut body)?;
                if body.len() > MAX_BODY {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
        if !self.reader.buffer().is_empty() {
            return Err(invalid());
        }
        Ok((code, body))
    }
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

fn read_chunks(reader: &mut impl BufRead, body: &mut Vec<u8>) -> io::Result<()> {
    let mut line = Vec::new();
    for _ in 0..=MAX_CHUNKS {
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
        if n > MAX_BODY - body.len() {
            return Err(invalid());
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
