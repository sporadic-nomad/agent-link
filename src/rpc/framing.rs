//! A line reader that never accumulates more than the configured frame limit.

use std::io::{self, BufRead};

pub(super) enum ReadError {
    TooLong,
    Io(io::ErrorKind),
}

pub(super) fn read_frame(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    limit: usize,
) -> Result<usize, ReadError> {
    buf.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(ReadError::Io(err.kind())),
        };
        if available.is_empty() {
            return Ok(buf.len());
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if count > limit - buf.len() {
            return Err(ReadError::TooLong);
        }
        // Grow geometrically for fragmented streams, capped by the frame budget.
        // Vec's unrestricted geometric growth could otherwise exceed that budget.
        if buf.capacity() < buf.len() + count {
            let capacity = buf
                .capacity()
                .saturating_mul(2)
                .max(8192)
                .max(buf.len() + count)
                .min(limit);
            buf.reserve_exact(capacity - buf.len());
        }
        buf.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(buf.len());
        }
    }
}

/// Discards the unread remainder of a line without retaining bytes. Returns false when
/// cancellation wins between chunks; a blocking read still waits for pipe progress.
pub(super) fn discard_line(
    reader: &mut impl BufRead,
    stopped: impl Fn() -> bool,
) -> Result<bool, ReadError> {
    loop {
        if stopped() {
            return Ok(false);
        }
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(ReadError::Io(err.kind())),
        };
        if available.is_empty() {
            return Ok(true);
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        reader.consume(count);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn discarding_oversized_lines_recovers_at_newline_or_eof() {
        for capacity in [1, 2, 64] {
            for raw in [&b"abcdef\r\nnext\n"[..], &b"abcdef"[..]] {
                let mut reader = BufReader::with_capacity(capacity, Cursor::new(raw));
                let mut buf = Vec::new();
                assert!(matches!(
                    read_frame(&mut reader, &mut buf, 5),
                    Err(ReadError::TooLong)
                ));
                buf.clear();
                assert!(matches!(discard_line(&mut reader, || false), Ok(true)));
                assert!(buf.capacity() <= 5);
                let expected = if raw.ends_with(b"next\n") {
                    &b"next\n"[..]
                } else {
                    &b""[..]
                };
                assert_eq!(
                    read_frame(&mut reader, &mut buf, 5).ok(),
                    Some(expected.len())
                );
                assert_eq!(buf, expected);
            }
        }
    }

    #[test]
    fn discard_checks_cancellation_between_chunks() {
        let mut reader = BufReader::with_capacity(2, Cursor::new(b"abcdef\nnext"));
        let calls = std::cell::Cell::new(0);
        assert!(matches!(
            discard_line(&mut reader, || {
                calls.set(calls.get() + 1);
                calls.get() > 2
            }),
            Ok(false)
        ));
        assert_eq!(reader.buffer(), b"");
        let mut remaining = String::new();
        std::io::Read::read_to_string(&mut reader, &mut remaining).unwrap();
        assert_eq!(remaining, "ef\nnext");
    }

    #[test]
    fn bounded_frames_cover_crlf_eof_and_delimiter_boundaries() {
        let mut reader = BufReader::with_capacity(2, Cursor::new(b"abc\r\nx\nlast"));
        let mut buf = Vec::new();
        assert_eq!(read_frame(&mut reader, &mut buf, 5).ok(), Some(5));
        assert_eq!(buf, b"abc\r\n");
        assert_eq!(read_frame(&mut reader, &mut buf, 5).ok(), Some(2));
        assert_eq!(buf, b"x\n");
        assert_eq!(read_frame(&mut reader, &mut buf, 5).ok(), Some(4));
        assert_eq!(buf, b"last");
        assert_eq!(read_frame(&mut reader, &mut buf, 5).ok(), Some(0));
    }

    #[test]
    fn one_byte_fragments_preserve_multibyte_utf8_and_literal_replacement_characters() {
        let raw = "é�\r\n".as_bytes();
        let mut reader = BufReader::with_capacity(1, Cursor::new(raw));
        let mut buf = Vec::new();
        assert_eq!(
            read_frame(&mut reader, &mut buf, raw.len()).ok(),
            Some(raw.len())
        );
        assert_eq!(std::str::from_utf8(&buf).unwrap(), "é�\r\n");
    }

    #[test]
    fn oversized_frames_never_grow_past_the_limit() {
        for payload in [&b"abcde\n"[..], &b"abcdef"[..]] {
            let mut reader = BufReader::with_capacity(2, Cursor::new(payload));
            let mut buf = Vec::new();
            assert!(matches!(
                read_frame(&mut reader, &mut buf, 5),
                Err(ReadError::TooLong)
            ));
            assert!(buf.len() <= 5);
            assert!(buf.capacity() <= 5);
        }
    }

    #[test]
    fn a_read_failure_returns_only_the_error_category() {
        struct Failed;
        impl io::Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("PRIVATE_ERROR_DETAILS"))
            }
        }
        let mut reader = BufReader::new(Failed);
        let mut buf = Vec::new();
        assert!(matches!(
            read_frame(&mut reader, &mut buf, 8),
            Err(ReadError::Io(io::ErrorKind::Other))
        ));
        assert!(buf.is_empty());
        assert!(matches!(
            discard_line(&mut reader, || false),
            Err(ReadError::Io(io::ErrorKind::Other))
        ));
    }
}
