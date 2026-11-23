use std::net::{Ipv4Addr, TcpListener};
use std::time::{Duration, Instant};

use super::http_stub::{Events, Reply, Stub, basic};
use super::*;

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

fn connection(stub: &Stub) -> Connection {
    let socket = connect(stub.addr(), soon()).expect("connect to the stub");
    Connection::new(socket, stub.addr(), "dTpw".into())
}

fn get(path: &str, auth: bool, cap: usize, until: Instant) -> Exchange<'_> {
    Exchange {
        path,
        post: None,
        auth,
        cap,
        until,
    }
}

#[test]
fn only_a_loopback_address_with_a_port_is_connected() {
    for addr in ["10.0.0.1:80", "0.0.0.0:80", "127.0.0.1:0", "[::]:80"] {
        let addr: SocketAddr = addr.parse().unwrap();
        let e = connect(addr, soon()).expect_err("refused");
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{addr}");
    }
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    // Nothing listens there any more.
    let e = connect(port, soon()).expect_err("refused");
    assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
}

/// The credentials go only into an exchange that asks for them, in the header only.
#[test]
fn credentials_are_sent_only_when_asked_for() {
    let stub = Stub::start(|_| Reply::json(200, "[]"));
    let mut c = connection(&stub);
    assert_eq!(c.request(&get("/a", false, 64, soon())).unwrap().0, 200);
    assert_eq!(c.request(&get("/b", true, 64, soon())).unwrap().0, 200);
    let requests = stub.requests();
    assert_eq!(requests[0].authorization, None);
    assert_eq!(requests[1].authorization.as_deref(), Some("Basic dTpw"));
    assert!(!format!("{c:?}").contains("dTpw"));
}

#[test]
fn chunked_and_fixed_bodies_are_read_and_the_connection_kept() {
    let stub = Stub::start(|request| match request.path.as_str() {
        "/chunked" => Reply::Chunked {
            status: 200,
            chunks: vec![b"[1,".to_vec(), b"2]".to_vec()],
        },
        _ => Reply::json(201, "{}"),
    });
    let mut c = connection(&stub);
    assert_eq!(
        c.request(&get("/chunked", true, 64, soon())).unwrap(),
        (200, b"[1,2]".to_vec())
    );
    assert_eq!(
        c.request(&get("/fixed", true, 64, soon())).unwrap(),
        (201, b"{}".to_vec())
    );
    assert!(!c.is_closed());
}

/// A body over the cap fails the exchange as too large (and closes the connection),
/// whether its length was announced or chunked.
#[test]
fn a_body_over_the_cap_is_too_large() {
    let stub = Stub::start(|request| match request.path.as_str() {
        "/fixed" => Reply::json(200, vec![b'x'; 100]),
        _ => Reply::Chunked {
            status: 200,
            chunks: vec![vec![b'x'; 60], vec![b'x'; 60]],
        },
    });
    for path in ["/fixed", "/chunked"] {
        let mut c = connection(&stub);
        let e = c.request(&get(path, true, 99, soon())).expect_err(path);
        assert!(too_large(&e), "{path}: {e}");
        assert!(c.is_closed());
        assert!(!too_large(&invalid()));
    }
}

/// A head that never completes, a body that stalls, drips past the deadline or never
/// ends: every exchange ends by its deadline.
#[test]
fn slow_stalled_and_endless_answers_end_at_the_deadline() {
    let stub = Stub::start(|request| match request.path.as_str() {
        "/hang" => Reply::Hang,
        "/stall" => Reply::Stall {
            status: 200,
            length: 64,
        },
        "/drip" => Reply::Drip {
            status: 200,
            body: vec![b'x'; 64],
            every: Duration::from_millis(20),
        },
        _ => Reply::Endless {
            chunk: 16,
            every: Duration::from_millis(5),
        },
    });
    for path in ["/hang", "/stall", "/drip"] {
        let mut c = connection(&stub);
        let started = Instant::now();
        let e = c
            .request(&get(
                path,
                true,
                1 << 20,
                Instant::now() + Duration::from_millis(200),
            ))
            .expect_err(path);
        let took = started.elapsed();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{path}: {e}");
        assert!(took < Duration::from_secs(1), "{path}: {took:?}");
        assert!(c.is_closed());
    }
    let mut c = connection(&stub);
    let e = c
        .request(&get("/endless", true, 1 << 20, soon()))
        .expect_err("endless");
    assert!(too_large(&e) || e.kind() == io::ErrorKind::TimedOut, "{e}");
}

#[test]
fn a_connection_closed_without_an_answer_fails_the_exchange() {
    let stub = Stub::start(|_| Reply::Close);
    let mut c = connection(&stub);
    assert!(c.request(&get("/", true, 64, soon())).is_err());
    assert!(c.is_closed());
    assert_eq!(
        c.request(&get("/", true, 64, soon())).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
}

fn drain_limits(line_bytes: usize) -> DrainLimits {
    DrainLimits {
        until: Instant::now() + Duration::from_millis(500),
        quiet: Duration::from_millis(30),
        line_bytes,
        lines: 256,
        bytes: 1 << 20,
    }
}

fn texts(data: &[Vec<u8>]) -> Vec<String> {
    data.iter()
        .map(|d| String::from_utf8_lossy(d).into_owned())
        .collect()
}

fn open_events(stub: &Stub) -> EventStream {
    match connection(stub).into_events("/event", soon()).unwrap() {
        Opened::Events(events) => events,
        Opened::Status(code) => panic!("HTTP {code}"),
    }
}

/// `data:` lines are read (one space after the colon dropped, CR LF too), everything else
/// passed over; a quiet read ends the drain, not the stream; the end is told.
#[test]
fn an_event_stream_tells_its_data_lines_until_it_ends() {
    let events = std::sync::Arc::new(Events::default());
    let served = std::sync::Arc::clone(&events);
    let stub = Stub::start(move |request| {
        assert_eq!(request.authorization.as_deref(), Some("Basic dTpw"));
        served.subscribe()
    });
    let mut stream = open_events(&stub);
    let drained = stream.drain(&drain_limits(1024));
    assert!(drained.data.is_empty() && drained.ended.is_none() && !drained.cut);
    events.send_raw(b"data: {\"a\":1}\n\n: comment\nevent: x\nid: 7\ndata:{\"b\":2}\r\n\r\n");
    std::thread::sleep(Duration::from_millis(50));
    let drained = stream.drain(&drain_limits(1024));
    assert_eq!(texts(&drained.data), [r#"{"a":1}"#, r#"{"b":2}"#]);
    assert_eq!(
        drained.ended, None,
        "a quiet read ends the drain, not the stream"
    );
    events.close_all();
    std::thread::sleep(Duration::from_millis(50));
    let drained = stream.drain(&drain_limits(1024));
    assert_eq!(drained.ended, Some(StreamEnd::Closed));
    assert!(!drained.cut);
    assert_eq!(
        stream.drain(&drain_limits(1024)).ended,
        Some(StreamEnd::Closed)
    );
}

/// A line longer than the limit is never kept: skipped and counted; a line split across
/// chunks and drains is joined.
#[test]
fn an_overlong_line_is_skipped_and_a_split_line_joined() {
    let events = std::sync::Arc::new(Events::default());
    let served = std::sync::Arc::clone(&events);
    let stub = Stub::start(move |_| served.subscribe());
    let mut stream = open_events(&stub);
    events.send_raw(b"data: 0123456789abcdef\ndata: ok\ndata: {\"lo");
    std::thread::sleep(Duration::from_millis(50));
    // The limit is the whole line's: `data: {"long":1}` is 16 bytes.
    let drained = stream.drain(&drain_limits(16));
    assert_eq!(texts(&drained.data), ["ok"]);
    assert_eq!(drained.oversized, 1);
    events.send_raw(b"ng\":1}\r\n");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        texts(&stream.drain(&drain_limits(16)).data),
        [r#"{"long":1}"#]
    );
}

/// A drain that hit its line or byte budget says so (more may be buffered); the rest
/// comes with the next drain.
#[test]
fn a_drain_cut_by_its_budget_is_told() {
    // All records can arrive with the response head or in a single socket read.
    // Exercise cuts both inside chunk data and across chunk framing.
    let body = b":ignored\ndata: first\r\ndata: this line is too long\ndata: second\n";
    for chunked in [false, true] {
        let stub = Stub::start(move |_| {
            if chunked {
                Reply::Chunked {
                    status: 200,
                    chunks: body.chunks(13).map(<[u8]>::to_vec).collect(),
                }
            } else {
                Reply::json(200, body.to_vec())
            }
        });
        let mut stream = open_events(&stub);
        let mut limits = drain_limits(12);
        limits.lines = 1;
        for (wanted, skipped) in [(Some("first"), 0), (None, 1), (Some("second"), 0)] {
            let drained = stream.drain(&limits);
            assert_eq!(texts(&drained.data), wanted.into_iter().collect::<Vec<_>>());
            assert_eq!(drained.oversized, skipped);
            assert!(drained.cut || drained.ended == Some(StreamEnd::Closed));
        }
        assert_eq!(stream.drain(&limits).ended, Some(StreamEnd::Closed));
    }
}

#[test]
fn an_event_request_refused_tells_its_status() {
    let stub = Stub::start(|_| Reply::status(401));
    match connection(&stub).into_events("/event", soon()).unwrap() {
        Opened::Status(code) => assert_eq!(code, 401),
        Opened::Events(_) => panic!("not a stream"),
    }
}

/// The incremental chunk decoder gives the same body for every split of its input, and
/// refuses malformed framing.
#[test]
fn chunks_decode_the_same_however_the_input_is_split() {
    let wire = b"4\r\nWiki\r\n6;ext=1\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\n\r\nNEXT";
    let body = b"Wikipedia in \r\n\r\nchunks.";
    for split in 1..wire.len() {
        let mut chunked = Chunked::default();
        let mut out = Vec::new();
        let mut used = 0;
        for piece in wire.chunks(split) {
            if chunked.done() {
                break;
            }
            used += chunked
                .decode(piece, &mut |b| {
                    out.extend_from_slice(b);
                    b.len()
                })
                .unwrap();
        }
        assert_eq!(out, body, "split {split}");
        assert!(chunked.done());
        assert_eq!(&wire[used..], b"\r\nNEXT", "the trailer is not read");
    }
    for bad in [
        &b"x\r\n"[..],
        b"4\r\nWikiXX",
        b"4\nWiki",
        b"\r\n",
        b"11111111111111111\r\n",
    ] {
        let mut chunked = Chunked::default();
        assert!(
            chunked.decode(bad, &mut |b| b.len()).is_err(),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn the_basic_header_of_the_stub_matches_the_crate_s() {
    use base64::Engine;
    let ours = base64::engine::general_purpose::STANDARD.encode("opencode:0123abcd");
    assert_eq!(basic("opencode", "0123abcd"), format!("Basic {ours}"));
}
