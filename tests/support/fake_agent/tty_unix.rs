//! The fake agent's terminal on unix: raw input with `ISIG` kept, ^C routed to the main
//! loop, the size.

use std::sync::OnceLock;

/// The terminal settings before raw mode, restored at exit.
static SAVED: OnceLock<libc::termios> = OnceLock::new();

/// Blocks SIGINT in the calling thread (and so in every thread it spawns later) and
/// starts a thread that takes it with `sigwait` and calls `on_interrupt`. No signal
/// handler runs: the main loop exits on its own terms, never in the middle of a write.
/// Call it before spawning any other thread.
pub fn route_interrupt(on_interrupt: impl FnOnce() + Send + 'static) {
    // SAFETY: a zeroed `sigset_t` initialised by `sigemptyset`; the mask and the wait use it.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        std::thread::spawn(move || {
            let mut signal = 0;
            if libc::sigwait(&set, &mut signal) == 0 {
                on_interrupt();
            }
        });
    }
}

/// `ICANON`, `ECHO`, `IEXTEN` off (no line editing, no echo, no literal-next), `ICRNL`,
/// `INLCR`, `IGNCR`, `IXON` off (Enter stays `\r`, ^S/^Q pass); `ISIG` kept, so ^C raises
/// SIGINT (see [`route_interrupt`]).
pub fn enter_raw() {
    // SAFETY: termios calls on fd 0 with a zeroed struct the call fills.
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut saved) != 0 {
            return; // not a terminal: nothing to switch or restore
        }
        let _ = SAVED.set(saved);
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN);
        raw.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSANOW, &raw);
    }
}

/// Restores the settings [`enter_raw`] replaced.
pub fn restore() {
    if let Some(saved) = SAVED.get() {
        // SAFETY: settings read from the same fd.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
    }
}

/// Blocks until stdin has input, without reading it.
pub fn wait_readable() {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the duration of the call.
    while unsafe { libc::poll(&mut fd, 1, -1) } < 0
        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
    {}
}

/// `(rows, cols)`, 24x80 when unknown.
pub fn size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ fills the zeroed struct.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            (ws.ws_row, ws.ws_col)
        } else {
            (24, 80)
        }
    }
}
