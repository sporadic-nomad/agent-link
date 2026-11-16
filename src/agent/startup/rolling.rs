//! The startup gate's raw output buffer: a streaming UTF-8 decoder and a text buffer that
//! keeps only its newest characters.

/// Decodes a byte stream read in chunks: a UTF-8 sequence split across two reads is joined,
/// invalid bytes become U+FFFD (one per maximal invalid subpart, as a browser's streaming
/// `TextDecoder` does).
#[derive(Clone, Debug, Default)]
pub(crate) struct Utf8Stream {
    /// The incomplete sequence at the end of the last chunk (at most 3 bytes).
    pending: Vec<u8>,
}

impl Utf8Stream {
    /// The text of `bytes`, with any incomplete trailing sequence held back for the next
    /// chunk.
    pub(crate) fn decode(&mut self, bytes: &[u8]) -> String {
        let joined;
        let mut rest: &[u8] = if self.pending.is_empty() {
            bytes
        } else {
            let mut buf = std::mem::take(&mut self.pending);
            buf.extend_from_slice(bytes);
            joined = buf;
            &joined
        };
        let mut out = String::with_capacity(rest.len());
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    return out;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    // Valid by construction.
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match e.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = &after[len..];
                        }
                        None => {
                            // An incomplete sequence at the end: wait for its next bytes.
                            self.pending = after.to_vec();
                            return out;
                        }
                    }
                }
            }
        }
    }

    /// Forgets a held-back partial sequence.
    pub(crate) fn reset(&mut self) {
        self.pending.clear();
    }
}

/// Text that keeps only its last `cap` characters.
#[derive(Clone, Debug)]
pub(crate) struct RollingText {
    cap: usize,
    text: String,
    chars: usize,
}

impl RollingText {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            text: String::new(),
            chars: 0,
        }
    }

    pub(crate) fn push(&mut self, more: &str) {
        self.text.push_str(more);
        self.chars += more.chars().count();
        if self.chars > self.cap {
            let excess = self.chars - self.cap;
            let cut = self
                .text
                .char_indices()
                .nth(excess)
                .map_or(self.text.len(), |(at, _)| at);
            self.text.drain(..cut);
            self.chars = self.cap;
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.chars = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sequence_split_across_chunks_is_joined() {
        let bytes = "a❯b".as_bytes(); // ❯ is 3 bytes
        let mut d = Utf8Stream::default();
        assert_eq!(d.decode(&bytes[..2]), "a");
        assert_eq!(d.decode(&bytes[2..3]), "");
        assert_eq!(d.decode(&bytes[3..]), "❯b");
    }

    #[test]
    fn invalid_bytes_become_replacement_characters() {
        let mut d = Utf8Stream::default();
        assert_eq!(d.decode(b"a\xffb"), "a\u{FFFD}b");
        // A truncated sequence followed by a valid byte is one replacement.
        assert_eq!(d.decode(b"\xe2\x9dX"), "\u{FFFD}X");
        // An incomplete tail is held, and reset drops it.
        assert_eq!(d.decode(b"\xe2"), "");
        d.reset();
        assert_eq!(d.decode(b"ok"), "ok");
    }

    #[test]
    fn the_buffer_keeps_its_newest_characters() {
        let mut r = RollingText::new(5);
        r.push("abc");
        r.push("dé");
        assert_eq!(r.text(), "abcdé");
        r.push("❯xy");
        assert_eq!(r.text(), "dé❯xy");
        r.push("0123456789");
        assert_eq!(r.text(), "56789");
        r.clear();
        assert_eq!(r.text(), "");
        r.push("z");
        assert_eq!(r.text(), "z");
    }
}
