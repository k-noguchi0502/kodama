//! Compact scrollback rows. A screen row keeps a full [`Cell`] (14 bytes)
//! per column, but history is written once and read rarely, and most of it
//! is plain text in a handful of styles. So a history row stores just the
//! characters (one byte each when they all fit) and its colors, flags and
//! links as runs, and is expanded back into cells when it is read.
//!
//! The whole row is one allocation behind one pointer: a header, the runs,
//! then the text. Rows sit in the scrollback by the tens of thousands, so a
//! second allocation and a fatter struct per row add up.

use super::{CHAR_MASK, Cell, Color};

/// Attributes shared by the cells from `start` up to the next run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Run {
    start: u16,
    fg: Color,
    bg: Color,
    link: u16,
    flags: u16,
}

/// `kind` byte, the runs' length in bytes (u32) and the last cell's flags.
const HEADER: usize = 7;
const NARROW: u8 = 0;
const WIDE: u8 = 1;

/// Which of a run's attributes follow its start. The rest are defaults.
const HAS_FG_INDEXED: u8 = 1;
const HAS_FG_RGB: u8 = 2;
const HAS_BG_INDEXED: u8 = 4;
const HAS_BG_RGB: u8 = 8;
const HAS_LINK: u8 = 16;
const HAS_FLAGS: u8 = 32;

impl Run {
    /// A run is its start, a byte saying which attributes are set, then only
    /// those: most runs are a colour or two on the default background.
    fn write(&self, out: &mut Vec<u8>) {
        let color_bits = |c: Color, indexed: u8, rgb: u8| match c {
            Color::Default => 0,
            Color::Indexed(_) => indexed,
            Color::Rgb(..) => rgb,
        };
        let mask = color_bits(self.fg, HAS_FG_INDEXED, HAS_FG_RGB)
            | color_bits(self.bg, HAS_BG_INDEXED, HAS_BG_RGB)
            | if self.link != 0 { HAS_LINK } else { 0 }
            | if self.flags != 0 { HAS_FLAGS } else { 0 };
        out.extend_from_slice(&self.start.to_le_bytes());
        out.push(mask);
        for c in [self.fg, self.bg] {
            match c {
                Color::Default => {}
                Color::Indexed(i) => out.push(i),
                Color::Rgb(r, g, b) => out.extend_from_slice(&[r, g, b]),
            }
        }
        if self.link != 0 {
            out.extend_from_slice(&self.link.to_le_bytes());
        }
        if self.flags != 0 {
            out.extend_from_slice(&self.flags.to_le_bytes());
        }
    }

    /// The run at the front of `b`, and the bytes after it.
    fn read(b: &[u8]) -> (Self, &[u8]) {
        let start = u16::from_le_bytes([b[0], b[1]]);
        let mask = b[2];
        let mut rest = &b[3..];
        let mut color = |indexed: u8, rgb: u8| {
            if mask & indexed != 0 {
                let c = Color::Indexed(rest[0]);
                rest = &rest[1..];
                c
            } else if mask & rgb != 0 {
                let c = Color::Rgb(rest[0], rest[1], rest[2]);
                rest = &rest[3..];
                c
            } else {
                Color::Default
            }
        };
        let fg = color(HAS_FG_INDEXED, HAS_FG_RGB);
        let bg = color(HAS_BG_INDEXED, HAS_BG_RGB);
        let mut word = |bit: u8| {
            if mask & bit != 0 {
                let v = u16::from_le_bytes([rest[0], rest[1]]);
                rest = &rest[2..];
                v
            } else {
                0
            }
        };
        let link = word(HAS_LINK);
        let flags = word(HAS_FLAGS);
        (
            Run {
                start,
                fg,
                bg,
                link,
                flags,
            },
            rest,
        )
    }
}

#[derive(Debug)]
pub struct Line {
    /// Empty for an empty row. Otherwise the header, the runs, the text.
    data: Box<[u8]>,
}

impl Line {
    pub fn new(cells: &[Cell]) -> Self {
        let cells = &cells[..cells.len().min(u16::MAX as usize)];
        if cells.is_empty() {
            return Self { data: Box::new([]) };
        }
        let mut runs: Vec<Run> = Vec::new();
        for (i, cell) in cells.iter().enumerate() {
            let run = Run {
                start: i as u16,
                fg: cell.fg,
                bg: cell.bg,
                link: cell.link,
                flags: cell.flags(),
            };
            let same = runs.last().is_some_and(|r| {
                (r.fg, r.bg, r.link, r.flags) == (run.fg, run.bg, run.link, run.flags)
            });
            if !same {
                runs.push(run);
            }
        }
        let code = |c: &Cell| c.packed() & CHAR_MASK;
        let narrow = cells.iter().all(|c| code(c) <= 0xff);
        let text_bytes = cells.len() * if narrow { 1 } else { 4 };
        let mut data = Vec::with_capacity(HEADER + runs.len() * 14 + text_bytes);
        data.push(if narrow { NARROW } else { WIDE });
        data.extend_from_slice(&[0; 4]);
        let last_flags = runs.last().map_or(0, |r| r.flags);
        data.extend_from_slice(&last_flags.to_le_bytes());
        for run in &runs {
            run.write(&mut data);
        }
        let runs_len = (data.len() - HEADER) as u32;
        data[1..5].copy_from_slice(&runs_len.to_le_bytes());
        if narrow {
            data.extend(cells.iter().map(|c| code(c) as u8));
        } else {
            for c in cells {
                data.extend_from_slice(&code(c).to_le_bytes());
            }
        }
        Self {
            data: data.into_boxed_slice(),
        }
    }

    fn wide(&self) -> bool {
        self.data.first() == Some(&WIDE)
    }

    fn runs_len(&self) -> usize {
        match self.data.get(1..5) {
            Some(n) => u32::from_le_bytes([n[0], n[1], n[2], n[3]]) as usize,
            None => 0,
        }
    }

    fn runs(&self) -> &[u8] {
        self.data
            .get(HEADER..HEADER + self.runs_len())
            .unwrap_or(&[])
    }

    fn text(&self) -> &[u8] {
        self.data.get(HEADER + self.runs_len()..).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        let text = self.text().len();
        if self.wide() { text / 4 } else { text }
    }

    /// Flags of the last cell, without expanding the row.
    pub fn last_flags(&self) -> Option<u16> {
        self.data
            .get(5..HEADER)
            .map(|f| u16::from_le_bytes([f[0], f[1]]))
    }

    pub fn cells(&self) -> Vec<Cell> {
        let len = self.len();
        let text = self.text();
        let wide = self.wide();
        let code = |i: usize| {
            if wide {
                u32::from_le_bytes([
                    text[i * 4],
                    text[i * 4 + 1],
                    text[i * 4 + 2],
                    text[i * 4 + 3],
                ])
            } else {
                text[i] as u32
            }
        };
        let mut out = Vec::with_capacity(len);
        let mut rest = self.runs();
        let mut next = None;
        if !rest.is_empty() {
            let (run, after) = Run::read(rest);
            next = Some(run);
            rest = after;
        }
        while let Some(run) = next.take() {
            if !rest.is_empty() {
                let (following, after) = Run::read(rest);
                next = Some(following);
                rest = after;
            }
            let end = next.map_or(len, |r| r.start as usize);
            for i in run.start as usize..end {
                let mut cell = Cell {
                    fg: run.fg,
                    bg: run.bg,
                    link: run.link,
                    ..Cell::default()
                };
                cell.set_packed(code(i) | (run.flags as u32) << super::CHAR_BITS);
                out.push(cell);
            }
        }
        out
    }

    /// Bytes this row holds on the heap.
    #[cfg(test)]
    pub fn heap_size(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{Terminal, flags};

    fn row(t: &Terminal, y: usize) -> Vec<Cell> {
        t.line(t.history_len() + y).to_vec()
    }

    #[test]
    fn rows_survive_the_round_trip() {
        let mut t = Terminal::new(40, 3, 0);
        t.feed("plain \x1b[1;31mbold red\x1b[0m \x1b]8;;https://x.y\x1b\\link\x1b]8;;\x1b\\ 漢字 👍🏽 é\r\n".as_bytes());
        t.feed(b"\x1b[44m   \x1b[0m \x1b[38;2;1;2;3;48;5;200mend\x1b[0m");
        for y in 0..2 {
            let cells = row(&t, y);
            assert_eq!(Line::new(&cells).cells(), cells);
        }
        assert!(Line::new(&[]).cells().is_empty());
        assert_eq!(Line::new(&[]).last_flags(), None);
    }

    #[test]
    fn plain_text_is_a_byte_per_cell() {
        let mut t = Terminal::new(80, 1, 0);
        t.feed(b"line 12345 of the scrollback with some text in it");
        let cells = row(&t, 0);
        let used = cells.iter().rposition(|c| *c != Cell::default()).unwrap() + 1;
        let line = Line::new(&cells[..used]);
        assert_eq!(line.len(), 49);
        assert!(line.heap_size() < 49 + 20, "{}", line.heap_size());
        assert!(line.cells()[0].flags() & flags::BOLD == 0);
    }

    #[test]
    fn colour_runs_store_only_what_is_set() {
        let mut t = Terminal::new(120, 1, 0);
        t.feed(
            b"\x1b[32m      1\x1b[0m \x1b[1;34mINFO\x1b[0m request status=\x1b[33m200\x1b[0m done",
        );
        let cells = row(&t, 0);
        let used = cells.iter().rposition(|c| *c != Cell::default()).unwrap() + 1;
        let line = Line::new(&cells[..used]);
        assert_eq!(line.cells(), &cells[..used]);
        // Seven runs: four plain, a green, a bold blue and a yellow one.
        assert!(
            line.heap_size() <= HEADER + used + 7 * 3 + 3 + 3 + 2 + 1,
            "{}",
            line.heap_size()
        );
    }

    #[test]
    fn a_row_is_one_pointer() {
        assert_eq!(std::mem::size_of::<Line>(), 16);
    }
}
