use std::io::{self, Read};
use std::time::Duration;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind, Event,
};

/// Polls stdin (fd 0) to check if input is available within `timeout`.
pub fn poll_stdin(timeout: Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(err);
    }
    Ok(ret > 0 && (pfd.revents & libc::POLLIN) != 0)
}

/// Buffer and parser for incoming terminal escape sequences and characters.
#[derive(Default)]
pub struct InputReader {
    buf: Vec<u8>,
    pub supports_release: bool,
}

impl InputReader {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1024),
            supports_release: false,
        }
    }

    /// Reads all currently available bytes from stdin into the internal buffer.
    pub fn read_available(&mut self) -> io::Result<usize> {
        let mut temp = [0u8; 1024];
        let mut total = 0;
        // Non-blocking drain while bytes are queued on stdin
        while poll_stdin(Duration::ZERO)? {
            let n = match io::stdin().read(&mut temp) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            self.buf.extend_from_slice(&temp[..n]);
            total += n;
            if n < temp.len() {
                break;
            }
        }
        Ok(total)
    }

    /// Appends raw bytes directly (used for testing).
    #[cfg(test)]
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Parses and returns the next Event, or None if buffer is empty or needs more bytes.
    pub fn next_event(&mut self) -> Option<Event> {
        while !self.buf.is_empty() {
            if self.buf[0] == 0x1B {
                // Escape sequence
                if self.buf.len() == 1 {
                    // Could be standalone Esc or start of escape sequence.
                    // If stdin has more bytes arriving immediately, wait for them.
                    if poll_stdin(Duration::from_millis(15)).unwrap_or(false) {
                        return None;
                    }
                    self.buf.remove(0);
                    return Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
                }

                match self.buf[1] {
                    b'[' => {
                        // CSI sequence: find terminating byte in 0x40..=0x7E ('@'..='~')
                        // Win32 input mode uses '_' (0x5F) which is in this range.
                        let mut term_idx = None;
                        for i in 2..self.buf.len() {
                            let b = self.buf[i];
                            if (0x40..=0x7E).contains(&b) {
                                term_idx = Some(i);
                                break;
                            }
                        }

                        let end = match term_idx {
                            Some(idx) => idx,
                            None => {
                                // Still waiting for terminating byte
                                if self.buf.len() > 64 {
                                    // Malformed sequence; discard invalid byte and retry
                                    self.buf.remove(0);
                                    continue;
                                }
                                return None;
                            }
                        };

                        let seq = self.buf.drain(..=end).collect::<Vec<u8>>();
                        if let Some(ev) = parse_csi_sequence(&seq) {
                            if *seq.last().unwrap_or(&0) == b'_' || *seq.last().unwrap_or(&0) == b'u' {
                                self.supports_release = true;
                            }
                            return Some(ev);
                        }
                        // If unhandled CSI, continue loop to next token
                        continue;
                    }
                    b'O' => {
                        // SS3 sequence (e.g. \x1bOP for F1, \x1bOA for Up in some modes)
                        if self.buf.len() < 3 {
                            return None;
                        }
                        let b2 = self.buf[2];
                        self.buf.drain(..3);
                        let key = match b2 {
                            b'P' => KeyCode::F(1),
                            b'Q' => KeyCode::F(2),
                            b'R' => KeyCode::F(3),
                            b'S' => KeyCode::F(4),
                            b'A' => KeyCode::Up,
                            b'B' => KeyCode::Down,
                            b'C' => KeyCode::Right,
                            b'D' => KeyCode::Left,
                            b'H' => KeyCode::Home,
                            b'F' => KeyCode::End,
                            _ => continue,
                        };
                        return Some(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)));
                    }
                    _ => {
                        // Alt + Key sequence (e.g. \x1b followed by a char)
                        let b = self.buf[1];
                        self.buf.drain(..2);
                        if let Some(ch) = char::from_u32(b as u32) {
                            return Some(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::ALT)));
                        }
                        continue;
                    }
                }
            }

            // Standalone control / ASCII characters
            let b0 = self.buf[0];
            match b0 {
                b'\r' | b'\n' => {
                    self.buf.remove(0);
                    return Some(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
                }
                b'\t' => {
                    self.buf.remove(0);
                    return Some(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
                }
                0x7F | 0x08 => {
                    self.buf.remove(0);
                    return Some(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
                }
                1..=26 => {
                    // Ctrl+A through Ctrl+Z
                    self.buf.remove(0);
                    let ch = (b0 + b'a' - 1) as char;
                    return Some(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)));
                }
                _ => {
                    // UTF-8 decode
                    match std::str::from_utf8(&self.buf) {
                        Ok(s) => {
                            if let Some(ch) = s.chars().next() {
                                let len = ch.len_utf8();
                                self.buf.drain(..len);
                                return Some(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
                            }
                        }
                        Err(e) => {
                            let valid_up_to = e.valid_up_to();
                            if valid_up_to > 0 {
                                if let Ok(s) = std::str::from_utf8(&self.buf[..valid_up_to]) {
                                    if let Some(ch) = s.chars().next() {
                                        let len = ch.len_utf8();
                                        self.buf.drain(..len);
                                        return Some(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
                                    }
                                }
                            } else if e.error_len().is_none() {
                                // Incomplete UTF-8 sequence, wait for more bytes
                                return None;
                            } else {
                                // Invalid byte, discard and continue
                                self.buf.remove(0);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

/// Parses a complete CSI sequence (starts with \x1b[ and ends with term char).
fn parse_csi_sequence(seq: &[u8]) -> Option<Event> {
    if seq.len() < 3 || seq[0] != 0x1B || seq[1] != b'[' {
        return None;
    }
    let last = seq[seq.len() - 1];

    // 1. Win32 Input Mode: \x1b[Vk;Sc;Uc;Kd;Cs;Rc_
    if last == b'_' {
        let content = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
        let parts: Vec<&str> = content.split(';').collect();
        if parts.len() >= 6 {
            let vk = parts[0].parse::<u16>().ok()?;
            let _sc = parts[1].parse::<u16>().ok()?;
            let uc = parts[2].parse::<u32>().ok()?;
            let kd = parts[3].parse::<u8>().ok()?;
            let cs = parts[4].parse::<u32>().ok()?;
            let _rc = parts[5].parse::<u16>().ok()?;

            let kind = if kd == 1 {
                KeyEventKind::Press
            } else {
                KeyEventKind::Release
            };

            let mut modifiers = KeyModifiers::empty();
            if cs & (0x0001 | 0x0002) != 0 {
                modifiers |= KeyModifiers::ALT;
            }
            if cs & (0x0004 | 0x0008) != 0 {
                modifiers |= KeyModifiers::CONTROL;
            }
            if cs & 0x0010 != 0 {
                modifiers |= KeyModifiers::SHIFT;
            }

            let mut state = KeyEventState::empty();
            if cs & 0x0080 != 0 {
                state |= KeyEventState::CAPS_LOCK;
            }
            if cs & 0x0020 != 0 {
                state |= KeyEventState::NUM_LOCK;
            }

            let keycode = match vk {
                0x08 => KeyCode::Backspace,
                0x09 => KeyCode::Tab,
                0x0D => KeyCode::Enter,
                0x1B => KeyCode::Esc,
                0x20 => KeyCode::Char(' '),
                0x21 => KeyCode::PageUp,
                0x22 => KeyCode::PageDown,
                0x23 => KeyCode::End,
                0x24 => KeyCode::Home,
                0x25 => KeyCode::Left,
                0x26 => KeyCode::Up,
                0x27 => KeyCode::Right,
                0x28 => KeyCode::Down,
                0x2D => KeyCode::Insert,
                0x2E => KeyCode::Delete,
                0x70..=0x7B => KeyCode::F((vk - 0x70 + 1) as u8),
                _ if uc > 0 => {
                    let ch = char::from_u32(uc)?;
                    KeyCode::Char(ch)
                }
                0x41..=0x5A => {
                    let ch = if modifiers.contains(KeyModifiers::SHIFT) {
                        (vk as u8) as char
                    } else {
                        ((vk as u8) + 32) as char
                    };
                    KeyCode::Char(ch)
                }
                0x30..=0x39 => KeyCode::Char((vk as u8) as char),
                _ => return None,
            };

            return Some(Event::Key(KeyEvent::new_with_kind_and_state(
                keycode, modifiers, kind, state,
            )));
        }
    }

    // 2. SGR Mouse: \x1b[<Cb;Cx;Cy(M|m)
    if (last == b'M' || last == b'm') && seq.len() >= 4 && seq[2] == b'<' {
        let content = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
        let parts: Vec<&str> = content.split(';').collect();
        if parts.len() >= 3 {
            let cb = parts[0].parse::<u16>().ok()?;
            let cx = parts[1].parse::<u16>().ok()?.saturating_sub(1);
            let cy = parts[2].parse::<u16>().ok()?.saturating_sub(1);
            let is_release = last == b'm';

            let mut modifiers = KeyModifiers::empty();
            if cb & 4 != 0 {
                modifiers |= KeyModifiers::SHIFT;
            }
            if cb & 8 != 0 {
                modifiers |= KeyModifiers::ALT;
            }
            if cb & 16 != 0 {
                modifiers |= KeyModifiers::CONTROL;
            }

            let kind = match (cb & 3, cb & 32 != 0, cb & 64 != 0, is_release) {
                (_, _, _, true) => MouseEventKind::Up(MouseButton::Left),
                (0, false, false, false) => MouseEventKind::Down(MouseButton::Left),
                (1, false, false, false) => MouseEventKind::Down(MouseButton::Middle),
                (2, false, false, false) => MouseEventKind::Down(MouseButton::Right),
                (0, true, false, false) => MouseEventKind::Drag(MouseButton::Left),
                (1, true, false, false) => MouseEventKind::Drag(MouseButton::Middle),
                (2, true, false, false) => MouseEventKind::Drag(MouseButton::Right),
                (0, false, true, false) => MouseEventKind::ScrollUp,
                (1, false, true, false) => MouseEventKind::ScrollDown,
                _ => MouseEventKind::Moved,
            };

            return Some(Event::Mouse(MouseEvent {
                kind,
                column: cx,
                row: cy,
                modifiers,
            }));
        }
    }

    // 3. Kitty Keyboard Protocol: \x1b[...u
    if last == b'u' {
        let content = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
        let mut semi = content.split(';');
        let first = semi.next()?;
        let second = semi.next();

        let mut colon = first.split(':');
        let codepoint = colon.next()?.parse::<u32>().ok()?;
        let kind_code = colon.next().and_then(|k| k.parse::<u8>().ok()).unwrap_or(1);

        let modifiers = if let Some(sec) = second {
            let mod_mask = sec.split(':').next().and_then(|m| m.parse::<u8>().ok()).unwrap_or(1);
            parse_kitty_modifiers(mod_mask)
        } else {
            KeyModifiers::empty()
        };

        let kind = match kind_code {
            1 => KeyEventKind::Press,
            2 => KeyEventKind::Repeat,
            3 => KeyEventKind::Release,
            _ => KeyEventKind::Press,
        };

        let keycode = match codepoint {
            27 => KeyCode::Esc,
            13 => KeyCode::Enter,
            9 => KeyCode::Tab,
            127 => KeyCode::Backspace,
            57399..=57408 => KeyCode::Char((b'0' + (codepoint - 57399) as u8) as char),
            57417 => KeyCode::Left,
            57418 => KeyCode::Right,
            57419 => KeyCode::Up,
            57420 => KeyCode::Down,
            57421 => KeyCode::PageUp,
            57422 => KeyCode::PageDown,
            57423 => KeyCode::Home,
            57424 => KeyCode::End,
            57425 => KeyCode::Insert,
            57426 => KeyCode::Delete,
            _ => {
                let ch = char::from_u32(codepoint)?;
                KeyCode::Char(ch)
            }
        };

        return Some(Event::Key(KeyEvent::new_with_kind(keycode, modifiers, kind)));
    }

    // 4. Standard ANSI Arrow & Functional Keys
    if matches!(last, b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'Z') {
        let code = match last {
            b'A' => KeyCode::Up,
            b'B' => KeyCode::Down,
            b'C' => KeyCode::Right,
            b'D' => KeyCode::Left,
            b'H' => KeyCode::Home,
            b'F' => KeyCode::End,
            b'Z' => KeyCode::BackTab,
            _ => return None,
        };
        let mut modifiers = KeyModifiers::empty();
        if seq.len() > 3 {
            if let Ok(s) = std::str::from_utf8(&seq[2..seq.len() - 1]) {
                if let Some(mod_str) = s.split(';').nth(1) {
                    if let Ok(m) = mod_str.parse::<u8>() {
                        modifiers = parse_kitty_modifiers(m);
                    }
                }
            }
        }
        return Some(Event::Key(KeyEvent::new(code, modifiers)));
    }

    // 5. Special ANSI ~ sequences: \x1b[<num>~
    if last == b'~' {
        let content = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
        let num = content.split(';').next()?.parse::<u8>().ok()?;
        let code = match num {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            11..=15 => KeyCode::F(num - 10),
            17..=21 => KeyCode::F(num - 11),
            23..=26 => KeyCode::F(num - 12),
            28..=29 => KeyCode::F(num - 15),
            31..=34 => KeyCode::F(num - 17),
            _ => return None,
        };
        return Some(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    None
}

fn parse_kitty_modifiers(mask: u8) -> KeyModifiers {
    let mut mods = KeyModifiers::empty();
    let m = mask.saturating_sub(1);
    if m & 1 != 0 {
        mods |= KeyModifiers::SHIFT;
    }
    if m & 2 != 0 {
        mods |= KeyModifiers::ALT;
    }
    if m & 4 != 0 {
        mods |= KeyModifiers::CONTROL;
    }
    mods
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_win32_keydown_w() {
        let mut r = InputReader::new();
        // \x1b[87;17;119;1;128;1_
        r.feed(b"\x1b[87;17;119;1;128;1_");
        let ev = r.next_event().expect("should parse win32 keydown");
        if let Event::Key(k) = ev {
            assert_eq!(k.code, KeyCode::Char('w'));
            assert_eq!(k.kind, KeyEventKind::Press);
        } else {
            panic!("expected Key event");
        }
    }

    #[test]
    fn test_win32_keyup_w() {
        let mut r = InputReader::new();
        // \x1b[87;17;119;0;128;1_
        r.feed(b"\x1b[87;17;119;0;128;1_");
        let ev = r.next_event().expect("should parse win32 keyup");
        if let Event::Key(k) = ev {
            assert_eq!(k.code, KeyCode::Char('w'));
            assert_eq!(k.kind, KeyEventKind::Release);
        } else {
            panic!("expected Key event");
        }
    }

    #[test]
    fn test_win32_left_arrow_press_and_release() {
        let mut r = InputReader::new();
        // \x1b[37;75;0;1;384;1_\x1b[37;75;0;0;384;1_
        r.feed(b"\x1b[37;75;0;1;384;1_\x1b[37;75;0;0;384;1_");
        let ev1 = r.next_event().expect("first event");
        if let Event::Key(k) = ev1 {
            assert_eq!(k.code, KeyCode::Left);
            assert_eq!(k.kind, KeyEventKind::Press);
        } else {
            panic!("expected Key event");
        }

        let ev2 = r.next_event().expect("second event");
        if let Event::Key(k) = ev2 {
            assert_eq!(k.code, KeyCode::Left);
            assert_eq!(k.kind, KeyEventKind::Release);
        } else {
            panic!("expected Key event");
        }
    }

    #[test]
    fn test_split_win32_chunks() {
        let mut r = InputReader::new();
        r.feed(b"\x1b[87;17;119;");
        assert!(r.next_event().is_none());

        r.feed(b"1;128;1_");
        let ev = r.next_event().expect("reassembled event");
        if let Event::Key(k) = ev {
            assert_eq!(k.code, KeyCode::Char('w'));
            assert_eq!(k.kind, KeyEventKind::Press);
        } else {
            panic!("expected Key event");
        }
    }

    #[test]
    fn test_ansi_arrow_keys() {
        let mut r = InputReader::new();
        r.feed(b"\x1b[A\x1b[B\x1b[C\x1b[D");
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))));
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))));
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))));
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))));
    }

    #[test]
    fn test_plain_characters() {
        let mut r = InputReader::new();
        r.feed(b"wasd");
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))));
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))));
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))));
        assert_eq!(r.next_event(), Some(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))));
        assert!(!r.supports_release);
    }

    #[test]
    fn test_win32_sets_supports_release() {
        let mut r = InputReader::new();
        assert!(!r.supports_release);
        r.feed(b"\x1b[87;17;119;1;0;1_");
        let _ = r.next_event();
        assert!(r.supports_release);
    }

    #[test]
    fn test_kitty_sets_supports_release() {
        let mut r = InputReader::new();
        assert!(!r.supports_release);
        r.feed(b"\x1b[119;1u");
        let _ = r.next_event();
        assert!(r.supports_release);
    }
}
