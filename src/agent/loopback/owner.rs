//! Which local process serves the far end of a loopback TCP connection this process made:
//! the process holding the accepted socket whose local address is the connection's peer
//! and whose remote address is its local one. Only that process reads what is written on
//! the connection, so a caller that finds it is the program it launched can send that
//! program a secret on it.
//!
//! - **macOS**: `libproc` (`proc_listpgrppids`, `proc_pidinfo(PROC_PIDLISTFDS)`,
//!   `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`). An accepted socket is visible once the server
//!   called `accept`; until then the connection is [`Owner::Unseen`]. Processes of other
//!   users cannot be inspected: one of them holding it reads `Unseen`, never `Agent`.
//! - **Linux and Android**: `/proc/net/tcp{,6}` gives the accepted socket's inode (0 until
//!   accepted), `/proc/<pid>/stat` the process group and `/proc/<pid>/fd` who holds it.
//! - **Windows**: `GetExtendedTcpTable` gives the owning process of the connection's
//!   server end. The socket is checked again with its owner's process handle open.
//!   Toolhelp parent links must lead to the program through strictly older processes.
//! - Elsewhere: [`Owner::Unsupported`].
//!
//! On Unix "the program" is a process group: the agent's foreground group for a shell
//! launch, the direct child's (which leads its own session) otherwise. A process outside
//! the session cannot join that group. A descendant that left the group (`setsid`,
//! `setpgid`) does not count.

use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Instant;

/// Who holds the server end of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum Owner {
    /// A process of the program (Unix: in its process group; Windows: it or a process
    /// below it).
    Agent,
    /// A process that is not the program's.
    Other,
    /// The holder or its relationship to the program could not be verified by the deadline.
    Unseen,
    /// This platform cannot tell.
    #[cfg_attr(
        any(
            target_os = "macos",
            target_os = "linux",
            target_os = "android",
            windows
        ),
        allow(dead_code)
    )]
    Unsupported,
}

/// Who serves `stream`'s far end, `program` being the launched agent (see the module
/// docs). Waits until `until` for the server to accept the connection (polling every few
/// milliseconds); performs no I/O on `stream`.
pub(in crate::agent) fn server_of(stream: &TcpStream, program: u32, until: Instant) -> Owner {
    let (Ok(client), Ok(server)) = (stream.local_addr(), stream.peer_addr()) else {
        return Owner::Unseen;
    };
    imp::server_of(server, client, program, until)
}

/// How long one look waits before the next.
#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
))]
const POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// `a` and `b` name the same address, an IPv4-mapped IPv6 address counting as IPv4.
#[cfg_attr(
    not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    )),
    allow(dead_code)
)]
fn same_ip(a: IpAddr, b: IpAddr) -> bool {
    let plain = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    plain(a) == plain(b)
}

#[cfg_attr(
    not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    )),
    allow(dead_code)
)]
fn same(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && same_ip(a.ip(), b.ip())
}

#[cfg(target_os = "macos")]
mod imp {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::os::raw::{c_int, c_void};
    use std::time::Instant;

    use super::{Owner, POLL, same};

    // <sys/proc_info.h>: `struct socket_fdinfo` and the offsets read from it, the same on
    // arm64 and x86_64 (taken from the SDK headers; the owner tests check them against
    // real sockets).
    const PROC_PIDFDSOCKETINFO: c_int = 3;
    const SOCKET_FDINFO_SIZE: usize = 792;
    const SOI_KIND: usize = 256;
    const INSI_FPORT: usize = 264;
    const INSI_LPORT: usize = 268;
    const INSI_VFLAG: usize = 288;
    const INSI_FADDR: usize = 296;
    const INSI_LADDR: usize = 312;
    const SOCKINFO_TCP: i32 = 2;
    const INI_IPV4: u8 = 1;
    /// Processes listed at most, and descriptors per process.
    const MAX_PIDS: usize = 16 * 1024;
    const MAX_FDS: usize = 16 * 1024;
    /// Socket lookups per check, in all.
    const MAX_LOOKUPS: usize = 64 * 1024;

    pub(super) fn server_of(
        server: SocketAddr,
        client: SocketAddr,
        program: u32,
        until: Instant,
    ) -> Owner {
        let Ok(group) = c_int::try_from(program) else {
            return Owner::Unseen;
        };
        // Give the server a short chance to accept before inspecting unrelated processes.
        let now = Instant::now();
        let search_others = now + POLL.min(until.saturating_duration_since(now) / 2);
        let mut lookups = MAX_LOOKUPS;
        while Instant::now() < until {
            let members = pids(|buf, size| {
                // SAFETY: `buf` has room for `size` bytes of pids.
                unsafe { libc::proc_listpgrppids(group, buf, size) }
            });
            if let Some(owner) = scan(members, server, client, group, &mut lookups, until) {
                return owner;
            }
            if Instant::now() >= until {
                return Owner::Unseen;
            }
            if Instant::now() < search_others {
                std::thread::sleep(search_others.saturating_duration_since(Instant::now()));
                continue;
            }
            let all = pids(|buf, size| {
                // SAFETY: as above.
                unsafe { libc::proc_listallpids(buf, size) }
            });
            if let Some(owner) = scan(all, server, client, group, &mut lookups, until) {
                return owner;
            }
            std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
        }
        Owner::Unseen
    }

    fn scan(
        candidates: Vec<c_int>,
        server: SocketAddr,
        client: SocketAddr,
        group: c_int,
        lookups: &mut usize,
        until: Instant,
    ) -> Option<Owner> {
        for pid in candidates {
            if Instant::now() >= until || *lookups == 0 {
                return Some(Owner::Unseen);
            }
            if holds(pid, server, client, lookups, until) {
                // Either listing can be stale. Classify the holder after matching the socket.
                // SAFETY: getpgid takes a process id and reports failure with -1.
                let actual = unsafe { libc::getpgid(pid) };
                return Some(if actual < 0 || Instant::now() >= until {
                    Owner::Unseen
                } else if actual == group {
                    Owner::Agent
                } else {
                    Owner::Other
                });
            }
        }
        None
    }

    #[test]
    fn stale_group_listing_does_not_authorize_a_socket_holder() {
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = listener.local_addr().unwrap();
        let stream = TcpStream::connect(server).unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let mut lookups = MAX_LOOKUPS;
        // Supply our PID as a stale candidate in a different group.
        // SAFETY: getpgrp has no arguments.
        let other = unsafe { libc::getpgrp() } ^ 1;
        assert_eq!(
            scan(
                vec![std::process::id() as c_int],
                server,
                stream.local_addr().unwrap(),
                other,
                &mut lookups,
                Instant::now() + std::time::Duration::from_secs(1)
            ),
            Some(Owner::Other)
        );
    }

    /// The pids a `libproc` list call returns (it returns their count).
    fn pids(list: impl Fn(*mut c_void, c_int) -> c_int) -> Vec<c_int> {
        let mut pids: Vec<c_int> = vec![0; MAX_PIDS];
        let size = c_int::try_from(MAX_PIDS * size_of::<c_int>()).unwrap_or(c_int::MAX);
        let n = list(pids.as_mut_ptr().cast(), size);
        pids.truncate(usize::try_from(n).unwrap_or(0).min(MAX_PIDS));
        pids.retain(|&pid| pid > 0);
        pids
    }

    /// `pid` holds a TCP socket whose local end is `server` and remote end `client`.
    fn holds(
        pid: c_int,
        server: SocketAddr,
        client: SocketAddr,
        lookups: &mut usize,
        until: Instant,
    ) -> bool {
        let entry = size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks for the size the descriptor list needs.
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if Instant::now() >= until {
            return false;
        }
        let Ok(needed) = usize::try_from(needed) else {
            return false;
        };
        // Room for a few descriptors opened meanwhile.
        let room = (needed / entry + 16).min(MAX_FDS);
        let mut fds = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            room
        ];
        let size = c_int::try_from(room * entry).unwrap_or(c_int::MAX);
        // SAFETY: `fds` has room for `size` bytes of `proc_fdinfo` records.
        let got = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size)
        };
        fds.truncate(usize::try_from(got).unwrap_or(0) / entry);
        for fd in fds {
            if Instant::now() >= until {
                return false;
            }
            if fd.proc_fdtype != libc::PROX_FDTYPE_SOCKET as u32 {
                continue;
            }
            if *lookups == 0 {
                return false;
            }
            *lookups -= 1;
            // 8-aligned room for one `struct socket_fdinfo`.
            let mut info = [0u64; SOCKET_FDINFO_SIZE / 8];
            // SAFETY: `info` has room for `SOCKET_FDINFO_SIZE` bytes.
            let got = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDSOCKETINFO,
                    info.as_mut_ptr().cast(),
                    SOCKET_FDINFO_SIZE as c_int,
                )
            };
            if usize::try_from(got) != Ok(SOCKET_FDINFO_SIZE) {
                continue;
            }
            let bytes: Vec<u8> = info.iter().flat_map(|w| w.to_ne_bytes()).collect();
            if endpoints(&bytes)
                .is_some_and(|(local, remote)| same(local, server) && same(remote, client))
            {
                return Instant::now() < until;
            }
        }
        false
    }

    /// A TCP socket's local and remote address, from its `socket_fdinfo` bytes.
    pub(super) fn endpoints(info: &[u8]) -> Option<(SocketAddr, SocketAddr)> {
        let int = |at: usize| -> Option<i32> {
            Some(i32::from_ne_bytes(info.get(at..at + 4)?.try_into().ok()?))
        };
        if int(SOI_KIND)? != SOCKINFO_TCP {
            return None;
        }
        // Ports are kept in network byte order in an int.
        let port = |at: usize| int(at).map(|raw| u16::from_be(raw as u16));
        let v4 = info.get(INSI_VFLAG)? & INI_IPV4 != 0;
        let ip = |at: usize| -> Option<IpAddr> {
            let raw: [u8; 16] = info.get(at..at + 16)?.try_into().ok()?;
            Some(if v4 {
                IpAddr::V4(Ipv4Addr::new(raw[12], raw[13], raw[14], raw[15]))
            } else {
                IpAddr::V6(Ipv6Addr::from(raw))
            })
        };
        Some((
            SocketAddr::new(ip(INSI_LADDR)?, port(INSI_LPORT)?),
            SocketAddr::new(ip(INSI_FADDR)?, port(INSI_FPORT)?),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "android", test))]
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
mod procfs {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// One `/proc/net/tcp{,6}` row's local and remote address and inode.
    pub(super) fn row(line: &str) -> Option<(SocketAddr, SocketAddr, u64)> {
        let fields: Vec<&str> = line.split_whitespace().take(10).collect();
        if fields.len() < 10 || !fields[0].ends_with(':') {
            return None;
        }
        Some((
            endpoint(fields[1])?,
            endpoint(fields[2])?,
            fields[9].parse().ok()?,
        ))
    }

    /// `ADDR:PORT` in the kernel's hex: the address as native-endian 32-bit words, the port
    /// as a number.
    fn endpoint(field: &str) -> Option<SocketAddr> {
        let (addr, port) = field.split_once(':')?;
        let port = u16::from_str_radix(port, 16).ok()?;
        let word = |hex: &str| u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes);
        let ip = match addr.len() {
            8 => IpAddr::V4(Ipv4Addr::from(word(addr)?)),
            32 => {
                let mut bytes = [0u8; 16];
                for (i, chunk) in bytes.chunks_mut(4).enumerate() {
                    chunk.copy_from_slice(&word(addr.get(i * 8..i * 8 + 8)?)?);
                }
                IpAddr::V6(Ipv6Addr::from(bytes))
            }
            _ => return None,
        };
        Some(SocketAddr::new(ip, port))
    }

    /// Inspect socket rows only while the owner check still has time. A read that
    /// completes after the deadline cannot supply an ownership result.
    pub(super) fn socket_inode(
        mut lines: impl Iterator<Item = std::io::Result<String>>,
        server: SocketAddr,
        client: SocketAddr,
        until: std::time::Instant,
    ) -> Option<u64> {
        while std::time::Instant::now() < until {
            let line = lines.next()?.ok()?;
            if let Some((local, remote, inode)) = row(&line) {
                if super::same(local, server) && super::same(remote, client) {
                    return (std::time::Instant::now() < until).then_some(inode);
                }
            }
        }
        None
    }

    /// The process group in a `/proc/<pid>/stat` line (the command name can hold spaces
    /// and parentheses: the fields after its last `)`).
    pub(super) fn pgrp(stat: &str) -> Option<u32> {
        let (_, rest) = stat.rsplit_once(')')?;
        rest.split_whitespace().nth(2)?.parse().ok()
    }

    /// Classify only a holder we actually found. The directory and descriptor budgets,
    /// hidden processes and permission failures can all leave ownership unresolved.
    pub(super) fn holder(
        root: &std::path::Path,
        inode: u64,
        group: u32,
        max_entries: usize,
        mut links: usize,
        until: std::time::Instant,
    ) -> super::Owner {
        use super::Owner;
        if std::time::Instant::now() >= until {
            return Owner::Unseen;
        }
        let Ok(processes) = std::fs::read_dir(root) else {
            return Owner::Unseen;
        };
        let target = format!("socket:[{inode}]");
        for entry in processes.take(max_entries).flatten() {
            if std::time::Instant::now() >= until || links == 0 {
                break;
            }
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.parse::<u32>().is_ok())
            {
                continue;
            }
            let path = entry.path();
            let Ok(fds) = std::fs::read_dir(path.join("fd")) else {
                continue;
            };
            for fd in fds.take(links.min(max_entries)) {
                if std::time::Instant::now() >= until {
                    return Owner::Unseen;
                }
                links -= 1;
                let Ok(fd) = fd else { continue };
                let matches = || {
                    std::fs::read_link(fd.path())
                        .is_ok_and(|link| link.as_os_str() == target.as_str())
                };
                if !matches() {
                    continue;
                }
                let Some(owner) = std::fs::read_to_string(path.join("stat"))
                    .ok()
                    .and_then(|s| pgrp(&s))
                else {
                    continue;
                };
                // Sample the descriptor again in case the process or FD changed
                // during the group lookup.
                if matches() && std::time::Instant::now() < until {
                    return if owner == group {
                        Owner::Agent
                    } else {
                        Owner::Other
                    };
                }
            }
        }
        Owner::Unseen
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::io::{BufRead, BufReader, Read};
    use std::net::SocketAddr;
    use std::time::Instant;

    use super::procfs::{holder, socket_inode};
    use super::{Owner, POLL};

    /// `/proc` entries and descriptors looked at, in all.
    const MAX_ENTRIES: usize = 64 * 1024;
    const MAX_LINKS: usize = 256 * 1024;
    /// The largest socket table read.
    const MAX_TABLE: u64 = 64 << 20;

    pub(super) fn server_of(
        server: SocketAddr,
        client: SocketAddr,
        program: u32,
        until: Instant,
    ) -> Owner {
        loop {
            match inode(server, client, until) {
                Some(inode) if inode != 0 => {
                    return holder(
                        std::path::Path::new("/proc"),
                        inode,
                        program,
                        MAX_ENTRIES,
                        MAX_LINKS,
                        until,
                    );
                }
                _ => {}
            }
            if Instant::now() >= until {
                return Owner::Unseen;
            }
            std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
        }
    }

    /// The inode of the socket whose local end is `server` and remote end `client` (0
    /// while not accepted); `None` when no table lists it.
    fn inode(server: SocketAddr, client: SocketAddr, until: Instant) -> Option<u64> {
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if Instant::now() >= until {
                return None;
            }
            let Ok(file) = std::fs::File::open(table) else {
                continue;
            };
            let lines = BufReader::new(file.take(MAX_TABLE)).lines();
            if let Some(inode) = socket_inode(lines, server, client, until) {
                return Some(inode);
            }
        }
        None
    }
}

#[cfg(any(windows, test))]
mod ancestry {
    use super::Owner;

    /// The handle keeps this PID assigned to the process whose creation time we read.
    pub(super) struct Process<H> {
        pub(super) pid: u32,
        pub(super) created: u64,
        pub(super) _handle: H,
    }

    /// The socket owner is pinned before the parent snapshot. At each step an older
    /// parent must therefore also predate the snapshot. A missing or recycled link,
    /// including one with indistinguishable timestamps, cannot establish ownership.
    pub(super) fn owner<H>(
        mut child: Process<H>,
        program: &Process<H>,
        mut parent_of: impl FnMut(u32) -> Option<u32>,
        mut open: impl FnMut(u32) -> Option<Process<H>>,
    ) -> Owner {
        while child.pid != program.pid {
            let Some(pid) = parent_of(child.pid) else {
                return Owner::Unseen;
            };
            if pid == 0 {
                return Owner::Other;
            }
            if pid == program.pid {
                return if program.created < child.created {
                    Owner::Agent
                } else {
                    Owner::Unseen
                };
            }
            let Some(parent) = open(pid) else {
                return Owner::Unseen;
            };
            if parent.created >= child.created {
                return Owner::Unseen;
            }
            child = parent;
        }
        Owner::Agent
    }
}

#[cfg(windows)]
mod imp {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, FILETIME, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        TCP_TABLE_OWNER_PID_CONNECTIONS,
    };

    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use super::{Owner, POLL, ancestry, same};

    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    /// The largest connection table read.
    const MAX_TABLE: usize = 64 << 20;

    pub(super) fn server_of(
        server: SocketAddr,
        client: SocketAddr,
        program: u32,
        until: Instant,
    ) -> Owner {
        let Some(program) = open_process(program, until) else {
            return Owner::Unseen;
        };
        let pid = loop {
            if Instant::now() >= until {
                return Owner::Unseen;
            }
            if let Some(pid) = owning_pid(server, client, until) {
                break pid;
            }
            std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
        };
        let Some(socket_owner) = open_process(pid, until) else {
            return Owner::Unseen;
        };
        // The table lookup and OpenProcess are separate. Bind the TCP row to the
        // handle while both the program's and the owner's PIDs are pinned.
        if owning_pid(server, client, until) != Some(pid) || Instant::now() >= until {
            return Owner::Unseen;
        }
        if pid == program.pid {
            return Owner::Agent;
        }
        let Ok(entries) = crate::proc::job::snapshot_while(|| Instant::now() < until) else {
            return Owner::Unseen;
        };
        let mut parents = std::collections::HashMap::new();
        for entry in entries {
            if Instant::now() >= until {
                return Owner::Unseen;
            }
            parents.insert(entry.pid, entry.parent);
        }
        let owner = ancestry::owner(
            socket_owner,
            &program,
            |pid| parents.remove(&pid),
            |pid| open_process(pid, until),
        );
        if Instant::now() < until {
            owner
        } else {
            Owner::Unseen
        }
    }

    fn open_process(pid: u32, until: Instant) -> Option<ancestry::Process<OwnedHandle>> {
        if Instant::now() >= until {
            return None;
        }
        // SAFETY: OpenProcess takes a PID and returns a non-inheritable query handle.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: OpenProcess returned a fresh handle owned by this caller.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        if Instant::now() >= until {
            return None;
        }
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        // SAFETY: the handle has query access and each output points to its own FILETIME.
        if unsafe {
            GetProcessTimes(
                handle.as_raw_handle(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return None;
        }
        Some(ancestry::Process {
            pid,
            created: u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime),
            _handle: handle,
        })
    }

    /// The process owning the connection whose local end is `server` and remote end
    /// `client`.
    fn owning_pid(server: SocketAddr, client: SocketAddr, until: Instant) -> Option<u32> {
        let family = if server.is_ipv4() { AF_INET } else { AF_INET6 };
        let mut size: u32 = 0;
        // 8-aligned, as the table's rows require at most 4.
        let mut table: Vec<u64> = Vec::new();
        for _ in 0..4 {
            if Instant::now() >= until {
                return None;
            }
            let ptr = if table.is_empty() {
                std::ptr::null_mut()
            } else {
                table.as_mut_ptr().cast()
            };
            // SAFETY: `ptr` is null with `size` 0, or `table` has room for `size` bytes.
            let status = unsafe {
                GetExtendedTcpTable(
                    ptr,
                    &mut size,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_CONNECTIONS,
                    0,
                )
            };
            if Instant::now() >= until {
                return None;
            }
            if status == NO_ERROR && !table.is_empty() {
                break;
            }
            let room = size as usize;
            if status != ERROR_INSUFFICIENT_BUFFER && status != NO_ERROR || room > MAX_TABLE {
                return None;
            }
            table = vec![0u64; room.div_ceil(8) + 1];
            size = u32::try_from(table.len() * 8).ok()?;
        }
        // SAFETY: the initialized u64 buffer is readable as bytes for its full length.
        let bytes = unsafe {
            std::slice::from_raw_parts(table.as_ptr().cast::<u8>(), size_of_val(table.as_slice()))
        };
        let count = u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
        let rows = bytes.get(4..)?;
        let port = |raw: u32| u16::from_be(raw as u16);
        if server.is_ipv4() {
            let width = size_of::<MIB_TCPROW_OWNER_PID>();
            for i in 0..count.min(rows.len() / width) {
                if Instant::now() >= until {
                    return None;
                }
                // SAFETY: the row lies within `rows`; plain data, read unaligned.
                let row: MIB_TCPROW_OWNER_PID =
                    unsafe { std::ptr::read_unaligned(rows[i * width..].as_ptr().cast()) };
                let local = SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
                    port(row.dwLocalPort),
                );
                let remote = SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes())),
                    port(row.dwRemotePort),
                );
                if same(local, server) && same(remote, client) {
                    return Some(row.dwOwningPid);
                }
            }
        } else {
            let width = size_of::<MIB_TCP6ROW_OWNER_PID>();
            for i in 0..count.min(rows.len() / width) {
                if Instant::now() >= until {
                    return None;
                }
                // SAFETY: as above.
                let row: MIB_TCP6ROW_OWNER_PID =
                    unsafe { std::ptr::read_unaligned(rows[i * width..].as_ptr().cast()) };
                let local = SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
                    port(row.dwLocalPort),
                );
                let remote = SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(row.ucRemoteAddr)),
                    port(row.dwRemotePort),
                );
                if same(local, server) && same(remote, client) {
                    return Some(row.dwOwningPid);
                }
            }
        }
        None
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
)))]
mod imp {
    use std::net::SocketAddr;
    use std::time::Instant;

    use super::Owner;

    pub(super) fn server_of(_: SocketAddr, _: SocketAddr, _: u32, _: Instant) -> Owner {
        Owner::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::procfs::{pgrp, row};
    use super::*;

    #[test]
    fn windows_parent_links_require_a_provably_older_process_at_each_step() {
        use ancestry::Process;

        let process = |pid, created| Process {
            pid,
            created,
            _handle: (),
        };
        let program = process(10, 100);
        let classify = |pid, created, links: &[(u32, u32)], times: &[(u32, u64)]| {
            ancestry::owner(
                process(pid, created),
                &program,
                |child| links.iter().find(|(id, _)| *id == child).map(|&(_, p)| p),
                |pid| {
                    times
                        .iter()
                        .find(|(id, _)| *id == pid)
                        .map(|&(_, t)| process(pid, t))
                },
            )
        };
        let chain = [(30, 20), (20, 10)];
        assert_eq!(classify(10, 100, &[], &[]), Owner::Agent);
        assert_eq!(classify(30, 300, &chain, &[(20, 200)]), Owner::Agent);
        // The recorded parent PID now names the newly launched program or a newer middle process.
        assert_eq!(classify(20, 90, &chain, &[]), Owner::Unseen);
        assert_eq!(classify(30, 300, &chain, &[(20, 400)]), Owner::Unseen);
        assert_eq!(classify(20, 100, &chain, &[]), Owner::Unseen);
        assert_eq!(classify(30, 300, &chain, &[(20, 300)]), Owner::Unseen);
        // The snapshot omits a link or OpenProcess/GetProcessTimes fails.
        assert_eq!(classify(30, 300, &chain[..1], &[(20, 200)]), Owner::Unseen);
        assert_eq!(classify(30, 300, &chain, &[]), Owner::Unseen);
        assert_eq!(classify(30, 300, &[(30, 0)], &[]), Owner::Other);
        assert_eq!(classify(30, 300, &chain, &[(20, 80)]), Owner::Unseen);
    }

    /// Rows as a little-endian kernel prints them.
    #[cfg(target_endian = "little")]
    #[test]
    fn proc_net_tcp_rows_give_both_ends_and_the_inode() {
        let v4 = "   3: 0100007F:1F90 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 424242 1 0000000000000000 20 4 30 10 -1";
        let (local, remote, inode) = row(v4).expect("a row");
        assert_eq!(local, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(remote, "127.0.0.1:50000".parse().unwrap());
        assert_eq!(inode, 424242);
        let v6 = "   0: 00000000000000000000000001000000:1F90 0000000000000000FFFF00000100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 7 1";
        let (local, remote, inode) = row(v6).expect("a row");
        assert_eq!(local, "[::1]:8080".parse().unwrap());
        assert!(same(remote, "127.0.0.1:50000".parse().unwrap()), "{remote}");
        assert_eq!(inode, 7);
        for header in [
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode",
            "",
            "   1: 0100007F 0100007F:C350 01",
        ] {
            assert!(row(header).is_none(), "{header:?}");
        }
    }

    #[test]
    fn an_expired_scan_does_not_read_another_socket_table_row() {
        let endpoint = "127.0.0.1:42".parse().unwrap();
        let rows = std::iter::from_fn(|| panic!("read past the deadline"));
        assert_eq!(
            procfs::socket_inode(rows, endpoint, endpoint, Instant::now()),
            None
        );
    }

    #[test]
    fn the_process_group_follows_the_last_parenthesis() {
        assert_eq!(pgrp("4242 (opencode) S 1 4242 4242 34816 ..."), Some(4242));
        assert_eq!(pgrp("77 (a) b) (c) R 76 75 74 0"), Some(75));
        assert_eq!(pgrp("77 no-parens R 1 2 3"), None);
    }

    #[cfg(unix)]
    #[test]
    fn procfs_owner_requires_a_matching_descriptor_and_known_group() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let proc = tempfile::tempdir().unwrap();
        let pid = proc.path().join("123");
        fs::create_dir(&pid).unwrap();
        fs::write(pid.join("stat"), "123 (test) S 1 22").unwrap();
        let scan = |group, links| {
            super::procfs::holder(
                proc.path(),
                42,
                group,
                10,
                links,
                Instant::now() + std::time::Duration::from_secs(1),
            )
        };
        assert_eq!(scan(22, 10), Owner::Unseen); // descriptors unavailable
        fs::create_dir(pid.join("fd")).unwrap();
        assert_eq!(scan(22, 10), Owner::Unseen); // no descriptor seen
        symlink("socket:[42]", pid.join("fd/3")).unwrap();
        assert_eq!(scan(22, 0), Owner::Unseen);
        assert_eq!(scan(22, 10), Owner::Agent);
        assert_eq!(scan(33, 10), Owner::Other);
        fs::remove_file(pid.join("stat")).unwrap();
        assert_eq!(scan(22, 10), Owner::Unseen);
    }

    #[test]
    fn mapped_addresses_count_as_ipv4() {
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(same_ip(mapped, "127.0.0.1".parse().unwrap()));
        assert!(!same_ip(
            "::1".parse().unwrap(),
            "127.0.0.1".parse().unwrap()
        ));
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    ))]
    mod sockets {
        use std::net::{TcpListener, TcpStream};
        use std::time::{Duration, Instant};

        use super::super::{Owner, server_of};

        /// This process's own program id: its process group (Unix) or itself (Windows).
        fn own_program() -> u32 {
            #[cfg(unix)]
            {
                // SAFETY: no arguments, always succeeds.
                u32::try_from(unsafe { libc::getpgrp() }).unwrap()
            }
            #[cfg(windows)]
            {
                std::process::id()
            }
        }

        fn soon() -> Instant {
            Instant::now() + Duration::from_millis(500)
        }

        #[test]
        fn an_accepted_connection_is_served_by_the_process_that_accepted_it() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (_server, _) = listener.accept().unwrap();
            assert_eq!(server_of(&client, own_program(), soon()), Owner::Agent);
        }

        #[test]
        fn an_expired_deadline_never_classifies_even_an_accepted_socket() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (_server, _) = listener.accept().unwrap();
            assert_eq!(
                server_of(&client, own_program(), Instant::now()),
                Owner::Unseen
            );
        }

        /// The same connection, `program` being another process (group): not the agent's.
        #[cfg(unix)]
        #[test]
        fn a_connection_served_outside_the_program_is_another_process_s() {
            use std::os::unix::process::CommandExt;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (_server, _) = listener.accept().unwrap();
            let mut other = std::process::Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap();
            let owner = server_of(&client, other.id(), soon());
            let _ = other.kill();
            let _ = other.wait();
            assert_eq!(owner, Owner::Other);
        }

        /// A connection nobody accepted yet is held by no process.
        #[cfg(unix)]
        #[test]
        fn a_connection_not_accepted_yet_is_unseen() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let started = Instant::now();
            let owner = server_of(
                &client,
                own_program(),
                Instant::now() + Duration::from_millis(50),
            );
            assert_eq!(owner, Owner::Unseen);
            assert!(started.elapsed() >= Duration::from_millis(50));
            drop(listener);
        }
    }
}
