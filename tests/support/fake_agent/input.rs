//! The fake agent's input decoder: a byte state machine over raw terminal input.

const PASTE_END: &[u8] = b"\x1b[201~";

/// One decoded input event.
pub enum Key {
    /// A printable byte (UTF-8 passes through byte by byte).
    Text(u8),
    /// The content between `ESC[200~` and `ESC[201~`.
    Paste(Vec<u8>),
    Up,
    Down,
    /// `\r`.
    Enter,
    /// DEL or BS.
    Backspace,
    /// An ESC that starts no sequence: the byte after it is not `[` (or the next ESC).
    Escape,
    /// `\x03` (reaches the program only when the terminal does not turn it into SIGINT).
    Interrupt,
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    Paste,
}

/// Bracketed pastes, CSI arrows, Enter, ^C, backspace, Esc and printable bytes; other
/// controls and escape sequences are ignored. A lone ESC is told apart by the byte after
/// it, so an ESC that ends the input stays pending until more arrives.
#[derive(Default)]
pub struct Parser {
    state: State,
    buf: Vec<u8>,
}

impl Parser {
    /// Feeds one byte; completed keys are appended to `keys`.
    pub fn feed(&mut self, byte: u8, keys: &mut Vec<Key>) {
        match self.state {
            State::Ground => match byte {
                0x1b => self.state = State::Esc,
                b'\r' => keys.push(Key::Enter),
                0x03 => keys.push(Key::Interrupt),
                0x7f | 0x08 => keys.push(Key::Backspace),
                0x20.. => keys.push(Key::Text(byte)),
                _ => {}
            },
            State::Esc => {
                self.buf.clear();
                match byte {
                    b'[' => self.state = State::Csi,
                    // The first ESC was a lone one; this one may start a sequence.
                    0x1b => keys.push(Key::Escape),
                    // A lone ESC; the byte is read as if no ESC came before it.
                    _ => {
                        keys.push(Key::Escape);
                        self.state = State::Ground;
                        self.feed(byte, keys);
                    }
                }
            }
            State::Csi => {
                self.buf.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.state = State::Ground;
                    match self.buf.as_slice() {
                        b"A" => keys.push(Key::Up),
                        b"B" => keys.push(Key::Down),
                        b"200~" => {
                            self.buf.clear();
                            self.state = State::Paste;
                        }
                        _ => {}
                    }
                } else if self.buf.len() > 16 {
                    self.state = State::Ground;
                }
            }
            State::Paste => {
                self.buf.push(byte);
                if self.buf.ends_with(PASTE_END) {
                    self.buf.truncate(self.buf.len() - PASTE_END.len());
                    keys.push(Key::Paste(std::mem::take(&mut self.buf)));
                    self.state = State::Ground;
                }
            }
        }
    }
}
