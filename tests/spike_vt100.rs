//! Spike: does `vt100` panic on real agent output or on known-hard synthetic streams?
//!
//! Replays every recorded fixture stream (`tests/fixtures/*.bin`, size from its `.meta`)
//! and a set of synthetic streams through `vt100::Parser` under `catch_unwind`, in
//! 1-byte, 7-byte and 8 KiB chunks. The screen is also read back under `catch_unwind`
//! (text, every cell, formatted rows and contents — what a snapshot or repaint reads)
//! after every chunk that crosses a 256-byte boundary (so after every 8 KiB chunk, and at
//! arbitrary points inside escape sequences for the small chunkings) and at the end.
//! A panic is counted per side (process / read), its message and first `vt100` frames
//! are reported, and the parser is rebuilt (what the crate's screen model does).
//! Prints a report; asserts nothing.
//!
//! Run: `cargo test -p agent-link --test spike_vt100 -- --ignored --nocapture`
#![cfg(feature = "screen")]

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::fs;
use std::hint::black_box;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CHUNKINGS: [usize; 3] = [1, 7, 8192];
const SCROLLBACK: usize = 200;
/// The screen is read back whenever the bytes fed so far cross a multiple of this.
const READ_EVERY: usize = 256;

thread_local! {
    /// Set only while a measured call runs: the panic hook stays quiet then.
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    /// Backtrace summary captured by the panic hook for the panic being caught.
    static LAST_TRACE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Screen read-backs started (for the report).
    static READS: Cell<usize> = const { Cell::new(0) };
}

/// Where a panic happened: feeding bytes (`process`, `set_size`) or reading the screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Process,
    Read,
}

struct Panic {
    stream: String,
    chunking: usize,
    offset: usize,
    side: Side,
    message: String,
    trace: String,
}

impl Panic {
    fn new(
        stream: &str,
        chunking: usize,
        offset: usize,
        side: Side,
        caught: (String, String),
    ) -> Self {
        let (message, trace) = caught;
        Panic {
            stream: stream.to_string(),
            chunking,
            offset,
            side,
            message,
            trace,
        }
    }
}

/// Text of a panic payload (`&str` or `String`).
fn payload_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string payload>".to_string())
}

/// Runs `f` under `catch_unwind` with the quiet hook; `Some((message, trace))` if it panicked.
/// Panics anywhere else go to the previous hook and fail the test with their message.
fn measure(f: impl FnOnce()) -> Option<(String, String)> {
    MEASURING.with(|m| m.set(true));
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    MEASURING.with(|m| m.set(false));
    let payload = result.err()?;
    Some((
        payload_message(payload.as_ref()),
        LAST_TRACE.with(RefCell::take),
    ))
}

/// Every read a snapshot or repaint makes: plain text and rows, each cell's text and width
/// flags, formatted rows and contents, the cursor. `black_box` keeps the reads alive.
fn read_screen(parser: &vt100::Parser) {
    READS.with(|n| n.set(n.get() + 1));
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    black_box(screen.contents());
    black_box(screen.rows(0, cols).count());
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                black_box((
                    cell.contents().len(),
                    cell.is_wide(),
                    cell.is_wide_continuation(),
                ));
            }
        }
    }
    black_box(
        screen
            .rows_formatted(0, cols)
            .map(|r| r.len())
            .sum::<usize>(),
    );
    black_box(screen.contents_formatted());
    black_box(screen.cursor_position());
}

/// Feeds `bytes` in `chunk`-sized pieces, reading the screen back after each chunk that
/// crosses a `READ_EVERY` boundary and after the last one; returns the panics (the parser
/// is rebuilt after each, process or read).
fn replay(name: &str, bytes: &[u8], rows: u16, cols: u16, chunk: usize) -> Vec<Panic> {
    let fresh = || vt100::Parser::new(rows, cols, SCROLLBACK);
    let mut parser = fresh();
    let mut panics = Vec::new();
    let mut next_read = READ_EVERY;
    for (i, piece) in bytes.chunks(chunk).enumerate() {
        let offset = i * chunk;
        if let Some(caught) = measure(|| parser.process(piece)) {
            panics.push(Panic::new(name, chunk, offset, Side::Process, caught));
            parser = fresh();
            continue;
        }
        let end = offset + piece.len();
        if end >= next_read || end == bytes.len() {
            next_read = (end / READ_EVERY + 1) * READ_EVERY;
            if let Some(caught) = measure(|| read_screen(&parser)) {
                panics.push(Panic::new(name, chunk, offset, Side::Read, caught));
                parser = fresh();
            }
        }
    }
    panics
}

/// Reads `rows`/`cols` from a fixture's `.meta`. Panics with the path and key when the
/// `.meta` cannot be read or a key is missing, not a number, or zero.
fn meta_size(bin: &Path) -> (u16, u16) {
    let path = bin.with_extension("meta");
    let meta = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read fixture meta {}: {e}", path.display()));
    let get = |key: &str| -> u16 {
        let value = meta
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("fixture meta {} has no `{key}=` line", path.display()))
            .trim();
        match value.parse() {
            Ok(n) if n > 0 => n,
            _ => panic!(
                "fixture meta {}: `{key}={value}` is not a positive size",
                path.display()
            ),
        }
    };
    (get("rows"), get("cols"))
}

/// Every `tests/fixtures/*.bin` with its size. Panics when the directory is missing, holds no
/// `.bin`, or a file cannot be read: a spike that replayed nothing must not report 0 panics.
fn fixture_streams() -> Vec<(String, Vec<u8>, u16, u16)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixture dir {}: {e}", dir.display()));
    let mut paths: Vec<PathBuf> = entries
        .map(|e| {
            e.unwrap_or_else(|e| panic!("cannot list fixture dir {}: {e}", dir.display()))
                .path()
        })
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    assert!(!paths.is_empty(), "no .bin fixtures in {}", dir.display());
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let bytes =
                fs::read(&p).unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", p.display()));
            let (rows, cols) = meta_size(&p);
            let name = p
                .file_stem()
                .unwrap_or_else(|| panic!("fixture without a name: {}", p.display()))
                .to_string_lossy()
                .into_owned();
            (name, bytes, rows, cols)
        })
        .collect()
}

/// `ESC [ row ; col H` (1-based).
fn cup(row: u16, col: u16) -> String {
    format!("\x1b[{row};{col}H")
}

fn synthetic_streams(rows: u16, cols: u16) -> Vec<(String, Vec<u8>)> {
    const BRAILLE: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    const WIDE: [&str; 4] = ["😀", "漢", "👍🏽", "👨\u{200d}👩\u{200d}👧"];
    let mut out = Vec::new();

    // 1 000 spinner frames: OSC 0 title + a cell update, the way codex animates both.
    let mut s = String::new();
    for i in 0..1000 {
        let c = BRAILLE[i % BRAILLE.len()];
        s += &format!("\x1b]0;{c} working\x07{}{c} Working ({i}s)", cup(rows, 1));
    }
    out.push(("braille-spinner-1000".to_string(), s.into_bytes()));

    // Wide characters at and just before the last column, with autowrap on and off.
    let mut s = String::new();
    for wrap in ["\x1b[?7h", "\x1b[?7l"] {
        s += wrap;
        for (r, w) in WIDE.iter().enumerate() {
            let row = u16::try_from(r).unwrap_or(0) + 1;
            s += &format!("{}{w}{}{w}{w}", cup(row, cols), cup(row + 5, cols - 1));
        }
        s += &format!("{}{}", cup(rows, cols), "漢".repeat(usize::from(cols)));
    }
    s += "\x1b[?7h";
    out.push(("wide-last-column".to_string(), s.into_bytes()));

    // Wide characters inside a DECSTBM region, then region scrolls, insert/delete lines
    // and characters that split them, reverse index at the top margin.
    let mut s = format!("\x1b[5;{}r", rows - 5);
    for i in 0..60 {
        let w = WIDE[i % WIDE.len()];
        s += &format!("{}{w}{}{w}\n", cup(rows - 5, 1), cup(rows - 5, cols - 1));
        s += &format!("{}\x1b[S{}\x1b[T", cup(10, 3), cup(5, 1));
        s += &format!("{}\x1bM{}{w}\x1b[2L\x1b[3M", cup(5, 1), cup(7, 2));
        s += &format!(
            "{}{w}{w}{w}{}\x1b[@{}\x1b[P\x1b[2X",
            cup(8, 1),
            cup(8, 2),
            cup(8, 3)
        );
    }
    s += "\x1b[r";
    out.push(("wide-in-scroll-region".to_string(), s.into_bytes()));

    // Combining marks: after ASCII, alone at column 1, after a wide char, at the last
    // column, after cursor moves, and a long run on one base.
    let mut s = String::new();
    s += "e\u{301}a\u{308}\u{301}";
    s += &format!("{}\u{301}", cup(2, 1));
    s += &format!("{}漢\u{301}😀\u{20e3}", cup(3, 1));
    s += &format!("{}x\u{301}{}漢\u{301}", cup(4, cols), cup(5, cols - 1));
    s += &format!("{}\u{301}\x1b[3D\u{308}", cup(6, 10));
    s += &format!("{}o{}", cup(7, 1), "\u{301}".repeat(100));
    s += &format!("{}\u{200d}\u{fe0f}\u{301}", cup(rows, cols));
    out.push(("combining-marks".to_string(), s.into_bytes()));

    // Alternate screen, wide chars at the edge, XTWINOPS resize requests (`CSI 8;rows;cols t`,
    // a shrink then a grow — vt100 ignores them), then leave the alternate screen: the
    // fullscreen-renderer pattern.
    let mut s = String::from("\x1b[?1049h\x1b[2J");
    for row in 1..=rows {
        s += &format!(
            "{}{}",
            cup(row, cols - 1),
            WIDE[usize::from(row) % WIDE.len()]
        );
    }
    s += "\x1b[8;20;60t\x1b[8;50;140t\x1b[?1049l";
    out.push(("alt-screen-wide-edges".to_string(), s.into_bytes()));
    out
}

/// A measured step on a prepared parser.
type Step = Box<dyn FnOnce(&mut vt100::Parser)>;

/// Narrow-screen and resize cases: (name, parser after unmeasured setup, measured step).
/// The screen is read back (measured separately) after a step that did not panic.
fn resize_cases(cols: u16) -> Vec<(String, vt100::Parser, Step)> {
    let mut cases: Vec<(String, vt100::Parser, Step)> = Vec::new();
    // Screens 1–3 columns wide (decides the column clamp): a wide character printed from
    // column 1 and at the last column, into a fresh screen and after a resize from `cols`.
    for narrow in 1..=3u16 {
        for (how, at_last) in [("wide", false), ("wide-last-column", true)] {
            let bytes = if at_last {
                format!("{}漢a{}😀\u{301}", cup(1, narrow), cup(2, narrow))
            } else {
                "漢a😀\u{301}".to_string()
            };
            let fresh = bytes.clone();
            cases.push((
                format!("{narrow}-column-{how}"),
                vt100::Parser::new(10, narrow, SCROLLBACK),
                Box::new(move |p| p.process(fresh.as_bytes())),
            ));
            let mut parser = vt100::Parser::new(10, cols, SCROLLBACK);
            parser.process(format!("{}漢😀", cup(1, cols - 3)).as_bytes());
            cases.push((
                format!("resize-{cols}-to-{narrow}-then-{how}"),
                parser,
                Box::new(move |p| {
                    p.screen_mut().set_size(10, narrow);
                    p.process(bytes.as_bytes());
                }),
            ));
        }
    }
    // Wide characters on the last columns, then a resize that cuts through them.
    for new_cols in [cols - 1, cols - 2, 2, 1, cols + 3] {
        let mut parser = vt100::Parser::new(10, cols, SCROLLBACK);
        let mut bytes = String::new();
        for row in 1..=10 {
            bytes += &format!("{}漢😀", cup(row, cols - 3));
        }
        parser.process(bytes.as_bytes());
        cases.push((
            format!("resize-{cols}-to-{new_cols}"),
            parser,
            Box::new(move |p| {
                p.screen_mut().set_size(10, new_cols);
                p.process("\u{301}漢".as_bytes());
            }),
        ));
    }
    cases
}

#[test]
#[ignore = "spike report; run with --ignored --nocapture"]
fn vt100_panics_on_recorded_and_synthetic_streams() {
    // Quiet only inside `measure`; every other panic reaches the previous hook.
    let previous: Arc<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync> =
        Arc::from(panic::take_hook());
    let fallback = Arc::clone(&previous);
    panic::set_hook(Box::new(move |info| {
        if !MEASURING.with(Cell::get) {
            return fallback(info);
        }
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        let summary: Vec<&str> = trace
            .lines()
            .filter(|l| l.contains(": vt100::"))
            .take(4)
            .map(str::trim)
            .collect();
        LAST_TRACE.with(|t| *t.borrow_mut() = summary.join(" <- "));
    }));

    let mut panics = Vec::new();
    let mut runs = 0usize;
    let fixtures = fixture_streams();
    for (name, bytes, rows, cols) in &fixtures {
        for chunk in CHUNKINGS {
            panics.extend(replay(name, bytes, *rows, *cols, chunk));
            runs += 1;
        }
    }
    let (rows, cols) = (40, 120);
    for (name, bytes) in synthetic_streams(rows, cols) {
        for chunk in CHUNKINGS {
            panics.extend(replay(&name, &bytes, rows, cols, chunk));
            runs += 1;
        }
    }
    for (stream, mut parser, step) in resize_cases(cols) {
        runs += 1;
        if let Some(caught) = measure(|| step(&mut parser)) {
            panics.push(Panic::new(&stream, 0, 0, Side::Process, caught));
        } else if let Some(caught) = measure(|| read_screen(&parser)) {
            panics.push(Panic::new(&stream, 0, 0, Side::Read, caught));
        }
    }

    panic::set_hook(Box::new(move |info| previous(info)));
    let reads = panics.iter().filter(|p| p.side == Side::Read).count();
    println!(
        "vt100 spike: {} fixture streams, {runs} runs, {} screen reads, {} process panics, \
         {reads} read panics",
        fixtures.len(),
        READS.with(Cell::get),
        panics.len() - reads
    );
    for p in &panics {
        let side = match p.side {
            Side::Process => "process",
            Side::Read => "read",
        };
        println!(
            "  PANIC ({side}) {} chunk={} offset={}: {} [{}]",
            p.stream, p.chunking, p.offset, p.message, p.trace
        );
    }
}
