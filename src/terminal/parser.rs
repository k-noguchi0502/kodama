//! VT500-series escape sequence parser.
//!
//! A byte-oriented state machine modelled after Paul Williams' DEC ANSI parser
//! (https://vt100.net/emu/dec_ansi_parser), with UTF-8 decoding in the ground
//! state. The parser has no knowledge of terminal semantics; it only turns a
//! byte stream into high level actions delivered through [`Perform`].

const MAX_PARAMS: usize = 32;
const MAX_INTERMEDIATES: usize = 4;
const MAX_OSC_LEN: usize = 64 * 1024;
/// APC strings carry inline image data (kitty graphics protocol).
const MAX_APC_LEN: usize = 64 * 1024 * 1024;

pub trait Perform {
    /// A printable character.
    fn print(&mut self, c: char);
    /// A C0 control byte (0x00..=0x1f).
    fn execute(&mut self, byte: u8);
    /// `CSI <intermediates> <params> <action>`; private markers (`?`, `>`,
    /// `<`, `=`) are included in `intermediates`.
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], action: char);
    /// `ESC <intermediates> <byte>`.
    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8);
    /// `OSC <params separated by ';'> ST`.
    fn osc_dispatch(&mut self, params: &[&[u8]]);
    /// `APC <data> ST`.
    fn apc_dispatch(&mut self, _data: &[u8]) {}
}

/// CSI parameters. Parameters separated by `:` are sub-parameters of the
/// preceding parameter and are grouped together.
#[derive(Default)]
pub struct Params {
    values: Vec<u16>,
    is_sub: Vec<bool>,
}

impl Params {
    fn clear(&mut self) {
        self.values.clear();
        self.is_sub.clear();
    }

    fn push(&mut self, value: u16, is_sub: bool) {
        if self.values.len() < MAX_PARAMS {
            self.values.push(value);
            self.is_sub.push(is_sub);
        }
    }

    /// Iterate over parameter groups (a parameter followed by its sub-parameters).
    pub fn groups(&self) -> impl Iterator<Item = &[u16]> {
        let mut start = 0;
        std::iter::from_fn(move || {
            if start >= self.values.len() {
                return None;
            }
            let mut end = start + 1;
            while end < self.values.len() && self.is_sub[end] {
                end += 1;
            }
            let group = &self.values[start..end];
            start = end;
            Some(group)
        })
    }

    /// First value of the `index`-th group, or `default` when missing or zero.
    pub fn get(&self, index: usize, default: u16) -> u16 {
        match self.groups().nth(index).map(|g| g[0]) {
            Some(0) | None => default,
            Some(v) => v,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    ApcString,
    /// DCS / SOS / PM: consumed and ignored until ST.
    IgnoredString,
}

pub struct Parser {
    state: State,
    params: Params,
    current_param: u16,
    current_is_sub: bool,
    param_present: bool,
    intermediates: Vec<u8>,
    osc: Vec<u8>,
    apc: Vec<u8>,
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_needed: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            state: State::Ground,
            params: Params::default(),
            current_param: 0,
            current_is_sub: false,
            param_present: false,
            intermediates: Vec::with_capacity(MAX_INTERMEDIATES),
            osc: Vec::new(),
            apc: Vec::new(),
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_needed: 0,
        }
    }
}

impl Parser {
    pub fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        for &byte in bytes {
            self.advance_byte(performer, byte);
        }
    }

    fn clear_sequence(&mut self) {
        self.params.clear();
        self.current_param = 0;
        self.current_is_sub = false;
        self.param_present = false;
        self.intermediates.clear();
    }

    fn advance_byte<P: Perform>(&mut self, p: &mut P, byte: u8) {
        if self.state == State::Ground && (self.utf8_needed > 0 || byte >= 0x80) {
            self.advance_utf8(p, byte);
            return;
        }

        // Transitions valid from any state.
        match byte {
            0x18 | 0x1a => {
                self.state = State::Ground;
                return;
            }
            0x1b => {
                match self.state {
                    State::OscString => self.dispatch_osc(p),
                    State::ApcString => self.dispatch_apc(p),
                    _ => {}
                }
                self.clear_sequence();
                self.state = State::Escape;
                return;
            }
            _ => {}
        }

        match self.state {
            State::Ground => match byte {
                0x00..=0x1f => p.execute(byte),
                0x20..=0x7e => p.print(byte as char),
                _ => {}
            },
            State::Escape => match byte {
                0x00..=0x1f => p.execute(byte),
                0x20..=0x2f => {
                    self.collect(byte);
                    self.state = State::EscapeIntermediate;
                }
                b'[' => {
                    self.clear_sequence();
                    self.state = State::CsiEntry;
                }
                b']' => {
                    self.osc.clear();
                    self.state = State::OscString;
                }
                b'_' => {
                    self.apc.clear();
                    self.state = State::ApcString;
                }
                b'P' | b'X' | b'^' => self.state = State::IgnoredString,
                0x30..=0x7e => {
                    p.esc_dispatch(&self.intermediates, byte);
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::EscapeIntermediate => match byte {
                0x00..=0x1f => p.execute(byte),
                0x20..=0x2f => self.collect(byte),
                0x30..=0x7e => {
                    p.esc_dispatch(&self.intermediates, byte);
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::CsiEntry => match byte {
                0x00..=0x1f => p.execute(byte),
                b'0'..=b'9' | b';' | b':' => {
                    self.param_byte(byte);
                    self.state = State::CsiParam;
                }
                0x3c..=0x3f => {
                    self.collect(byte);
                    self.state = State::CsiParam;
                }
                0x20..=0x2f => {
                    self.collect(byte);
                    self.state = State::CsiIntermediate;
                }
                0x40..=0x7e => self.dispatch_csi(p, byte),
                _ => {}
            },
            State::CsiParam => match byte {
                0x00..=0x1f => p.execute(byte),
                b'0'..=b'9' | b';' | b':' => self.param_byte(byte),
                0x3c..=0x3f => self.state = State::CsiIgnore,
                0x20..=0x2f => {
                    self.collect(byte);
                    self.state = State::CsiIntermediate;
                }
                0x40..=0x7e => self.dispatch_csi(p, byte),
                _ => {}
            },
            State::CsiIntermediate => match byte {
                0x00..=0x1f => p.execute(byte),
                0x20..=0x2f => self.collect(byte),
                0x30..=0x3f => self.state = State::CsiIgnore,
                0x40..=0x7e => self.dispatch_csi(p, byte),
                _ => {}
            },
            State::CsiIgnore => match byte {
                0x00..=0x1f => p.execute(byte),
                0x40..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::OscString => match byte {
                0x07 => {
                    self.dispatch_osc(p);
                    self.state = State::Ground;
                }
                0x00..=0x1f => {}
                _ => {
                    if self.osc.len() < MAX_OSC_LEN {
                        self.osc.push(byte);
                    }
                }
            },
            State::ApcString => match byte {
                0x07 => {
                    self.dispatch_apc(p);
                    self.state = State::Ground;
                }
                0x00..=0x1f => {}
                _ => {
                    if self.apc.len() < MAX_APC_LEN {
                        self.apc.push(byte);
                    }
                }
            },
            State::IgnoredString => {
                if byte == 0x07 {
                    self.state = State::Ground;
                }
            }
        }
    }

    fn advance_utf8<P: Perform>(&mut self, p: &mut P, byte: u8) {
        if self.utf8_needed > 0 {
            if byte & 0xc0 == 0x80 {
                self.utf8_buf[self.utf8_len] = byte;
                self.utf8_len += 1;
                self.utf8_needed -= 1;
                if self.utf8_needed == 0 {
                    let c = std::str::from_utf8(&self.utf8_buf[..self.utf8_len])
                        .ok()
                        .and_then(|s| s.chars().next())
                        .unwrap_or(char::REPLACEMENT_CHARACTER);
                    self.utf8_len = 0;
                    p.print(c);
                }
                return;
            }
            // Truncated sequence: emit a replacement and reprocess this byte.
            self.utf8_needed = 0;
            self.utf8_len = 0;
            p.print(char::REPLACEMENT_CHARACTER);
            self.advance_byte(p, byte);
            return;
        }

        let needed = match byte {
            0xc2..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            _ => {
                p.print(char::REPLACEMENT_CHARACTER);
                return;
            }
        };
        self.utf8_buf[0] = byte;
        self.utf8_len = 1;
        self.utf8_needed = needed;
    }

    fn collect(&mut self, byte: u8) {
        if self.intermediates.len() < MAX_INTERMEDIATES {
            self.intermediates.push(byte);
        }
    }

    fn param_byte(&mut self, byte: u8) {
        self.param_present = true;
        match byte {
            b';' | b':' => {
                self.params.push(self.current_param, self.current_is_sub);
                self.current_param = 0;
                self.current_is_sub = byte == b':';
            }
            _ => {
                self.current_param = self
                    .current_param
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u16);
            }
        }
    }

    fn dispatch_csi<P: Perform>(&mut self, p: &mut P, byte: u8) {
        if self.param_present {
            self.params.push(self.current_param, self.current_is_sub);
        }
        p.csi_dispatch(&self.params, &self.intermediates, byte as char);
        self.state = State::Ground;
    }

    fn dispatch_apc<P: Perform>(&mut self, p: &mut P) {
        p.apc_dispatch(&self.apc);
        // Image chunks can be large; don't keep their buffer around.
        if self.apc.capacity() > 64 * 1024 {
            self.apc = Vec::new();
        }
    }

    fn dispatch_osc<P: Perform>(&mut self, p: &mut P) {
        let parts: Vec<&[u8]> = self.osc.split(|&b| b == b';').collect();
        p.osc_dispatch(&parts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        out: Vec<String>,
    }

    impl Perform for Recorder {
        fn print(&mut self, c: char) {
            self.out.push(format!("print {c}"));
        }
        fn execute(&mut self, byte: u8) {
            self.out.push(format!("exec {byte:#x}"));
        }
        fn csi_dispatch(&mut self, params: &Params, inter: &[u8], action: char) {
            let groups: Vec<Vec<u16>> = params.groups().map(|g| g.to_vec()).collect();
            self.out.push(format!(
                "csi {:?} {} {action}",
                groups,
                String::from_utf8_lossy(inter)
            ));
        }
        fn esc_dispatch(&mut self, inter: &[u8], byte: u8) {
            self.out.push(format!(
                "esc {} {}",
                String::from_utf8_lossy(inter),
                byte as char
            ));
        }
        fn osc_dispatch(&mut self, params: &[&[u8]]) {
            let parts: Vec<String> = params
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            self.out.push(format!("osc {parts:?}"));
        }
        fn apc_dispatch(&mut self, data: &[u8]) {
            self.out
                .push(format!("apc {}", String::from_utf8_lossy(data)));
        }
    }

    fn run(input: &[u8]) -> Vec<String> {
        let mut parser = Parser::default();
        let mut rec = Recorder::default();
        parser.advance(&mut rec, input);
        rec.out
    }

    #[test]
    fn utf8_and_controls() {
        assert_eq!(
            run("aあ\r\n".as_bytes()),
            ["print a", "print あ", "exec 0xd", "exec 0xa"]
        );
    }

    #[test]
    fn csi_with_params_and_subparams() {
        assert_eq!(run(b"\x1b[1;31m"), ["csi [[1], [31]]  m"]);
        assert_eq!(
            run(b"\x1b[38:2::10:20:30m"),
            ["csi [[38, 2, 0, 10, 20, 30]]  m"]
        );
        assert_eq!(run(b"\x1b[?1049h"), ["csi [[1049]] ? h"]);
        assert_eq!(run(b"\x1b[m"), ["csi []  m"]);
    }

    #[test]
    fn osc_terminators() {
        assert_eq!(run(b"\x1b]0;title\x07"), ["osc [\"0\", \"title\"]"]);
        assert_eq!(run(b"\x1b]2;hi\x1b\\"), ["osc [\"2\", \"hi\"]", "esc  \\"]);
    }

    #[test]
    fn apc_strings_are_dispatched() {
        assert_eq!(
            run(b"\x1b_Ga=q;AAAA\x1b\\x"),
            ["apc Ga=q;AAAA", "esc  \\", "print x"]
        );
    }

    #[test]
    fn invalid_utf8() {
        assert_eq!(run(&[0xe3, 0x41]), ["print \u{fffd}", "print A"]);
    }
}
