//! Drops terminal query replies from forwarded stdin. In an interactive capture the child's
//! output is shown on the developer's real terminal, which answers the child's queries
//! (primary/secondary DA, DSR, CPR, DECRQM, XTVERSION, kitty keyboard flags, OSC 10/11
//! colours) on stdin. Forwarding those replies would hand the child the real terminal's
//! identity, answer a query twice (the tap answers too) and put garbage into agent input.
//!
//! Reply shapes recognised (ESC = `\x1b`, ST = `ESC \` or BEL for OSC):
//! `ESC [ ? … c`, `ESC [ > … c` (DA1, DA2), `ESC [ <digits> n` (DSR),
//! `ESC [ [?]<row>;<col> R` (CPR), `ESC [ [?]… $ y` (DECRQM), `ESC [ ? <flags> u` (kitty
//! keyboard), `ESC P > | … ESC \` (XTVERSION), `ESC ] 10|11 ; … ST` (colours).
//!
//! Trade-off: `ESC [ 1 ; <mod> R` is also F3 with a modifier on terminals that send PC-style
//! function keys; it has the CPR shape and is dropped. A reply split across two reads is
//! held back only once its prefix cannot be a keystroke (`ESC [ ?`, `ESC [ >`, `ESC P >`,
//! `ESC ] 10;`, `ESC ] 11;`); a split before that point is forwarded as it came.

/// Longest partial reply held back between reads; longer input is forwarded as is.
const MAX_PENDING: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// What [`ReplyFilter::filter`] made of one read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filtered {
    /// Bytes to forward to the child.
    pub pass: Vec<u8>,
    /// Replies removed, in order.
    pub dropped: Vec<Vec<u8>>,
}

/// Stateful filter over the stdin byte stream.
#[derive(Default)]
pub struct ReplyFilter {
    pending: Vec<u8>,
}

/// How a byte sequence starting with ESC relates to the reply shapes.
enum Match {
    /// A complete reply of this many bytes.
    Reply(usize),
    /// A prefix that can only continue as a reply: hold it until more bytes come.
    Partial,
    /// Not a reply (or a prefix that may be a keystroke).
    No,
}

impl ReplyFilter {
    /// Filters one read. Bytes held back from the previous read come first.
    pub fn filter(&mut self, chunk: &[u8]) -> Filtered {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let mut out = Filtered::default();
        let mut i = 0;
        while i < data.len() {
            if data[i] != ESC {
                out.pass.push(data[i]);
                i += 1;
                continue;
            }
            match classify(&data[i..]) {
                Match::Reply(n) => {
                    out.dropped.push(data[i..i + n].to_vec());
                    i += n;
                }
                Match::Partial if data.len() - i <= MAX_PENDING => {
                    self.pending = data[i..].to_vec();
                    return out;
                }
                Match::Partial | Match::No => {
                    out.pass.push(ESC);
                    i += 1;
                }
            }
        }
        out
    }
}

fn classify(s: &[u8]) -> Match {
    match s.get(1) {
        Some(b'[') => csi(s),
        Some(b'P') => dcs(s),
        Some(b']') => osc(s),
        _ => Match::No,
    }
}

/// `ESC [ <params 0x30-0x3F> <intermediates 0x20-0x2F> <final 0x40-0x7E>`.
fn csi(s: &[u8]) -> Match {
    let body = &s[2..];
    let params_len = body
        .iter()
        .take_while(|b| (0x30..=0x3f).contains(*b))
        .count();
    let inter_len = body[params_len..]
        .iter()
        .take_while(|b| (0x20..=0x2f).contains(*b))
        .count();
    let (params, inter) = (
        &body[..params_len],
        &body[params_len..params_len + inter_len],
    );
    let unambiguous = matches!(params.first(), Some(b'?' | b'>'));
    let Some(&fin) = body.get(params_len + inter_len) else {
        return if unambiguous {
            Match::Partial
        } else {
            Match::No
        };
    };
    let digits = |p: &[u8]| !p.is_empty() && p.iter().all(u8::is_ascii_digit);
    let numbers = |p: &[u8]| !p.is_empty() && p.iter().all(|b| b.is_ascii_digit() || *b == b';');
    let unprefixed = params.strip_prefix(b"?").unwrap_or(params);
    let reply = match (params.first(), inter, fin) {
        (Some(b'?' | b'>'), [], b'c') => numbers(&params[1..]) || params.len() == 1,
        (_, [], b'n') => digits(params),
        (_, [], b'R') => unprefixed
            .split(|&b| b == b';')
            .map(digits)
            .eq([true, true]),
        (_, [b'$'], b'y') => numbers(unprefixed),
        (Some(b'?'), [], b'u') => params.len() == 1 || digits(&params[1..]),
        _ => false,
    };
    if reply {
        Match::Reply(2 + params_len + inter_len + 1)
    } else {
        Match::No
    }
}

/// `ESC P > | <text> ESC \` (XTVERSION).
fn dcs(s: &[u8]) -> Match {
    let intro = b"\x1bP>|";
    if s.len() < intro.len() {
        // `ESC P` alone is Alt-Shift-P; only `ESC P >` commits to a reply.
        return if s.len() >= 3 && intro.starts_with(s) {
            Match::Partial
        } else {
            Match::No
        };
    }
    if !s.starts_with(intro) {
        return Match::No;
    }
    match s.windows(2).position(|w| w == b"\x1b\\") {
        Some(at) => Match::Reply(at + 2),
        None => Match::Partial,
    }
}

/// `ESC ] 10 ; … ST` or `ESC ] 11 ; … ST`, ST being BEL or `ESC \`.
fn osc(s: &[u8]) -> Match {
    let intro_ok = s.starts_with(b"\x1b]10;") || s.starts_with(b"\x1b]11;");
    if !intro_ok {
        return Match::No;
    }
    for (i, &b) in s.iter().enumerate().skip(5) {
        if b == BEL {
            return Match::Reply(i + 1);
        }
        if b == ESC {
            return match s.get(i + 1) {
                Some(b'\\') => Match::Reply(i + 2),
                Some(_) => Match::No,
                None => Match::Partial,
            };
        }
    }
    Match::Partial
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut f = ReplyFilter::default();
        let (mut pass, mut dropped) = (Vec::new(), Vec::new());
        for chunk in chunks {
            let out = f.filter(chunk);
            pass.extend(out.pass);
            dropped.extend(out.dropped);
        }
        (pass, dropped)
    }

    #[test]
    fn drops_every_reply_shape() {
        let replies: &[&[u8]] = &[
            b"\x1b[?1;2c",
            b"\x1b[?62;22c",
            b"\x1b[>1;10;0c",
            b"\x1b[0n",
            b"\x1b[12;40R",
            b"\x1b[?12;40R",
            b"\x1b[?2026;2$y",
            b"\x1b[4;1$y",
            b"\x1b[?0u",
            b"\x1b[?u",
            b"\x1bP>|iTerm2 3.5\x1b\\",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
        ];
        for reply in replies {
            let mut chunk = b"a".to_vec();
            chunk.extend_from_slice(reply);
            chunk.push(b'b');
            assert_eq!(run(&[&chunk]), (b"ab".to_vec(), vec![reply.to_vec()]));
        }
    }

    #[test]
    fn keeps_keystrokes() {
        let keys: &[&[u8]] = &[
            b"\x1b",
            b"\x1b[A",
            b"\x1bOA",
            b"\x1b[1;5A",
            b"\x1b[3~",
            b"\x1b[13;5~",
            b"\x1b[97;5u",
            b"\x1b[<0;10;5M",
            b"\x1b[200~x\x1b[201~",
            b"\x1bP",
            b"\x1b]",
            b"\x1b[",
            b"\x1b[12;",
            b"q\r",
        ];
        for key in keys {
            assert_eq!(run(&[key]), (key.to_vec(), Vec::new()), "{key:?}");
        }
    }

    #[test]
    fn modified_f3_has_the_cpr_shape() {
        // Documented trade-off: PC-style Ctrl-F3 looks like a cursor position report.
        assert_eq!(run(&[b"\x1b[1;5R"]).0, b"");
    }

    #[test]
    fn holds_a_reply_split_across_reads() {
        let (pass, dropped) = run(&[b"x\x1b[?1;", b"2cy"]);
        assert_eq!(pass, b"xy");
        assert_eq!(dropped, vec![b"\x1b[?1;2c".to_vec()]);
        let (pass, dropped) = run(&[b"\x1b]11;rgb:00", b"/00/00\x1b", b"\\z"]);
        assert_eq!(pass, b"z");
        assert_eq!(dropped.len(), 1);
        // An ambiguous prefix is forwarded as it came.
        let (pass, dropped) = run(&[b"\x1b[12;", b"40R"]);
        assert_eq!(pass, b"\x1b[12;40R");
        assert!(dropped.is_empty());
    }
}
