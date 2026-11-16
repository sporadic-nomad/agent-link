//! The fake agent's terminal on Windows: console modes for raw VT input and output, the
//! size. ^C reaches the program as `\x03` (processed input off).

use std::sync::OnceLock;

use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleMode,
};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

/// The input and output console modes before raw mode, restored at exit.
static SAVED: OnceLock<(u32, u32)> = OnceLock::new();

/// Nothing to route: with processed input off, ^C arrives as the byte `\x03`.
pub fn route_interrupt(_on_interrupt: impl FnOnce() + Send + 'static) {}

/// Line input, echo and processed input off, VT input on; VT processing on for output.
pub fn enter_raw() {
    // SAFETY: console mode calls on the process's standard handles.
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE);
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let (mut in_mode, mut out_mode) = (0u32, 0u32);
        if GetConsoleMode(input, &mut in_mode) == 0 || GetConsoleMode(output, &mut out_mode) == 0 {
            return; // not a console: nothing to switch or restore
        }
        let _ = SAVED.set((in_mode, out_mode));
        let off = ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT;
        SetConsoleMode(input, (in_mode & !off) | ENABLE_VIRTUAL_TERMINAL_INPUT);
        SetConsoleMode(
            output,
            out_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        );
    }
}

/// Restores the modes [`enter_raw`] replaced.
pub fn restore() {
    if let Some((in_mode, out_mode)) = SAVED.get() {
        // SAFETY: modes read from the same handles.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), *in_mode);
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), *out_mode);
        }
    }
}

/// Blocks until the console has input, without reading it (any input event signals the
/// handle).
pub fn wait_readable() {
    // SAFETY: waits on the process's standard input handle.
    unsafe {
        WaitForSingleObject(GetStdHandle(STD_INPUT_HANDLE), INFINITE);
    }
}

/// `(rows, cols)` of the visible window, 24x80 when unknown.
pub fn size() -> (u16, u16) {
    // SAFETY: fills a zeroed struct for the output handle.
    unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
            return (24, 80);
        }
        let rows = info.srWindow.Bottom - info.srWindow.Top + 1;
        let cols = info.srWindow.Right - info.srWindow.Left + 1;
        match (u16::try_from(rows), u16::try_from(cols)) {
            (Ok(rows), Ok(cols)) if rows > 0 && cols > 0 => (rows, cols),
            _ => (24, 80),
        }
    }
}
