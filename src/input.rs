//! Parser for terminal input: keys (legacy and Kitty CSI-u), SGR mouse reports,
//! focus events, bracketed paste, and replies to our startup queries.

use servo::{Key, Modifiers, NamedKey};

#[derive(Debug)]
pub enum Ev {
    Key { key: Key, mods: Modifiers },
    Paste(String),
    /// Raw SGR mouse report: button code, x, y (1-based, cells or pixels), press/release.
    Mouse { btn: u16, x: i32, y: i32, press: bool },
    Focus(bool),
    /// DECRPM reply for mode 1016 (SGR pixel mouse).
    PixelMouse(bool),
    TextAreaPx { w: u32, h: u32 },
    CellPx { w: u32, h: u32 },
}

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    in_paste: bool,
}

const PASTE_END: &[u8] = b"\x1b[201~";

fn key(key: Key, mods: Modifiers) -> Ev {
    Ev::Key { key, mods }
}
fn named(k: NamedKey, mods: Modifiers) -> Ev {
    key(Key::Named(k), mods)
}
fn chr(c: char, mods: Modifiers) -> Ev {
    key(Key::Character(c.to_string()), mods)
}

/// xterm modifier parameter (1 + bitmask) to Modifiers.
fn mods_from_param(p: Option<u32>) -> Modifiers {
    let bits = p.unwrap_or(1).saturating_sub(1);
    let mut m = Modifiers::empty();
    if bits & 1 != 0 {
        m |= Modifiers::SHIFT;
    }
    if bits & 2 != 0 {
        m |= Modifiers::ALT;
    }
    if bits & 4 != 0 {
        m |= Modifiers::CONTROL;
    }
    if bits & 8 != 0 {
        m |= Modifiers::META;
    }
    m
}

/// A single control byte (or printable ASCII) as a key.
fn byte_key(b: u8, extra: Modifiers) -> Option<Ev> {
    Some(match b {
        0x0d => named(NamedKey::Enter, extra),
        0x09 => named(NamedKey::Tab, extra),
        0x7f | 0x08 => named(NamedKey::Backspace, extra),
        0x1b => named(NamedKey::Escape, extra),
        0x00 => chr(' ', extra | Modifiers::CONTROL),
        0x01..=0x1a => chr((b'a' + b - 1) as char, extra | Modifiers::CONTROL),
        0x1c..=0x1f => chr((b'\\' + b - 0x1c) as char, extra | Modifiers::CONTROL),
        _ => return None,
    })
}

/// Decode one UTF-8 char at the start of `s`. None = incomplete.
fn decode_char(s: &[u8]) -> Option<(char, usize)> {
    let len = match s[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Some(('\u{fffd}', 1)),
    };
    if s.len() < len {
        return None;
    }
    match std::str::from_utf8(&s[..len]) {
        Ok(st) => Some((st.chars().next().unwrap(), len)),
        Err(_) => Some(('\u{fffd}', 1)),
    }
}

fn char_event(c: char, mods: Modifiers) -> Ev {
    let mods = if c.is_uppercase() { mods | Modifiers::SHIFT } else { mods };
    chr(c, mods)
}

impl Parser {
    /// True if bytes are waiting for the rest of a sequence.
    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn feed(&mut self, data: &[u8], out: &mut Vec<Ev>) {
        self.buf.extend_from_slice(data);
        let consumed = self.parse(out, false);
        self.buf.drain(..consumed);
    }

    /// Called after a short silence: whatever is left is a complete input
    /// (most often a lone ESC, which means the Escape key).
    pub fn flush(&mut self, out: &mut Vec<Ev>) {
        let consumed = self.parse(out, true);
        self.buf.drain(..consumed);
        if !self.in_paste {
            self.buf.clear();
        }
    }

    fn parse(&mut self, out: &mut Vec<Ev>, flushing: bool) -> usize {
        let buf = std::mem::take(&mut self.buf);
        let mut i = 0;
        while i < buf.len() {
            if self.in_paste {
                match buf[i..].windows(PASTE_END.len()).position(|w| w == PASTE_END) {
                    Some(p) => {
                        out.push(Ev::Paste(String::from_utf8_lossy(&buf[i..i + p]).into_owned()));
                        i += p + PASTE_END.len();
                        self.in_paste = false;
                        continue;
                    }
                    None => break,
                }
            }
            let b = buf[i];
            if b == 0x1b {
                if i + 1 >= buf.len() {
                    if flushing {
                        out.push(named(NamedKey::Escape, Modifiers::empty()));
                        i += 1;
                    }
                    break;
                }
                match buf[i + 1] {
                    b'[' => {
                        // CSI: params/intermediates until a final byte 0x40..=0x7e.
                        let Some(rel) = buf[i + 2..].iter().position(|c| (0x40..=0x7e).contains(c))
                        else {
                            if flushing {
                                i = buf.len();
                            }
                            break;
                        };
                        let end = i + 2 + rel;
                        let params = String::from_utf8_lossy(&buf[i + 2..end]).into_owned();
                        self.csi(&params, buf[end], out);
                        i = end + 1;
                    }
                    b'O' => {
                        if i + 2 >= buf.len() {
                            if flushing {
                                out.push(chr('O', Modifiers::ALT));
                                i += 2;
                            }
                            break;
                        }
                        if let Some(k) = ss3_key(buf[i + 2]) {
                            out.push(named(k, Modifiers::empty()));
                        }
                        i += 3;
                    }
                    b if b < 0x20 || b == 0x7f => {
                        if let Some(e) = byte_key(b, Modifiers::ALT) {
                            out.push(e);
                        }
                        i += 2;
                    }
                    _ => match decode_char(&buf[i + 1..]) {
                        Some((c, n)) => {
                            out.push(char_event(c, Modifiers::ALT));
                            i += 1 + n;
                        }
                        None => break,
                    },
                }
            } else if b < 0x20 || b == 0x7f {
                if let Some(e) = byte_key(b, Modifiers::empty()) {
                    out.push(e);
                }
                i += 1;
            } else {
                match decode_char(&buf[i..]) {
                    Some((c, n)) => {
                        out.push(char_event(c, Modifiers::empty()));
                        i += n;
                    }
                    None => break,
                }
            }
        }
        self.buf = buf;
        i
    }

    fn csi(&mut self, params: &str, fin: u8, out: &mut Vec<Ev>) {
        // SGR mouse: CSI < b ; x ; y M|m
        if let Some(rest) = params.strip_prefix('<') {
            if fin == b'M' || fin == b'm' {
                let v: Vec<i32> = rest.split(';').filter_map(|s| s.parse().ok()).collect();
                if v.len() == 3 {
                    out.push(Ev::Mouse { btn: v[0] as u16, x: v[1], y: v[2], press: fin == b'M' });
                }
            }
            return;
        }
        // DECRPM: CSI ? 1016 ; Ps $ y
        if let Some(rest) = params.strip_prefix('?') {
            if fin == b'y' {
                let rest = rest.trim_end_matches('$');
                let v: Vec<u32> = rest.split(';').filter_map(|s| s.parse().ok()).collect();
                if v.len() == 2 && v[0] == 1016 {
                    out.push(Ev::PixelMouse(matches!(v[1], 1 | 2 | 3)));
                }
            }
            return;
        }
        if params.starts_with('>') || params.starts_with('=') {
            return;
        }

        // Split "a:b;c:d" into fields of subfields.
        let fields: Vec<Vec<Option<u32>>> = params
            .split(';')
            .map(|f| f.split(':').map(|s| s.parse().ok()).collect())
            .collect();
        let field = |n: usize| fields.get(n).and_then(|f| f.first().copied().flatten());
        let mods = mods_from_param(field(1));

        match fin {
            b't' => {
                if let (Some(kind), Some(h), Some(w)) = (field(0), field(1), field(2)) {
                    match kind {
                        4 => out.push(Ev::TextAreaPx { w, h }),
                        6 => out.push(Ev::CellPx { w, h }),
                        _ => {}
                    }
                }
            }
            b'I' if params.is_empty() => out.push(Ev::Focus(true)),
            b'O' if params.is_empty() => out.push(Ev::Focus(false)),
            b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'P' | b'Q' | b'R' | b'S' => {
                if let Some(k) = ss3_key(fin) {
                    out.push(named(k, mods));
                }
            }
            b'Z' => out.push(named(NamedKey::Tab, Modifiers::SHIFT)),
            b'~' => {
                let k = match field(0) {
                    Some(200) => {
                        self.in_paste = true;
                        return;
                    }
                    Some(1 | 7) => NamedKey::Home,
                    Some(4 | 8) => NamedKey::End,
                    Some(2) => NamedKey::Insert,
                    Some(3) => NamedKey::Delete,
                    Some(5) => NamedKey::PageUp,
                    Some(6) => NamedKey::PageDown,
                    Some(11) => NamedKey::F1,
                    Some(12) => NamedKey::F2,
                    Some(13) => NamedKey::F3,
                    Some(14) => NamedKey::F4,
                    Some(15) => NamedKey::F5,
                    Some(17) => NamedKey::F6,
                    Some(18) => NamedKey::F7,
                    Some(19) => NamedKey::F8,
                    Some(20) => NamedKey::F9,
                    Some(21) => NamedKey::F10,
                    Some(23) => NamedKey::F11,
                    Some(24) => NamedKey::F12,
                    _ => return,
                };
                out.push(named(k, mods));
            }
            b'u' => {
                // Kitty keyboard: CSI code[:shifted[:base]] ; mods[:event] u
                // Event type 3 is a release; we send our own key-up events.
                if fields.get(1).and_then(|f| f.get(1).copied().flatten()) == Some(3) {
                    return;
                }
                let Some(cp) = field(0) else { return };
                let ev = match cp {
                    13 => named(NamedKey::Enter, mods),
                    9 => named(NamedKey::Tab, mods),
                    127 | 8 => named(NamedKey::Backspace, mods),
                    27 => named(NamedKey::Escape, mods),
                    57344.. => return, // Kitty private-use keys (keypad, media, lone modifiers).
                    _ => {
                        let Some(mut c) = char::from_u32(cp) else { return };
                        if mods.contains(Modifiers::SHIFT) {
                            c = c.to_uppercase().next().unwrap_or(c);
                        }
                        chr(c, mods)
                    }
                };
                out.push(ev);
            }
            _ => {}
        }
    }
}

fn ss3_key(fin: u8) -> Option<NamedKey> {
    Some(match fin {
        b'A' => NamedKey::ArrowUp,
        b'B' => NamedKey::ArrowDown,
        b'C' => NamedKey::ArrowRight,
        b'D' => NamedKey::ArrowLeft,
        b'H' => NamedKey::Home,
        b'F' => NamedKey::End,
        b'P' => NamedKey::F1,
        b'Q' => NamedKey::F2,
        b'R' => NamedKey::F3,
        b'S' => NamedKey::F4,
        _ => return None,
    })
}
