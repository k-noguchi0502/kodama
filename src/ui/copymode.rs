//! Copy mode: a cursor moved over the scrollback with vi keys, selecting
//! text by characters (`v`) or whole lines (`V`) and copying it (`y`).

use crate::layout::PaneId;
use crate::terminal::{Point, Selection, Terminal, flags};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visual {
    Chars,
    Lines,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordNext,
    WordEnd,
    WordPrev,
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
    ViewTop,
    ViewMiddle,
    ViewBottom,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
}

/// What a key asks for, besides moving.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Stay in copy mode.
    Stay,
    /// Copy this text and leave.
    Copy(String),
    /// Open the scrollback search.
    Search,
    Exit,
}

pub struct CopyMode {
    pub pane: PaneId,
    pub cursor: Point,
    /// Selection kind and where it started.
    pub visual: Option<(Visual, Point)>,
    /// `g` was pressed; a second one goes to the top.
    pending_g: bool,
}

/// Character class for word motions: blank, word, punctuation.
fn class(c: char) -> u8 {
    if c.is_whitespace() || c == '\0' {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

fn char_at(term: &Terminal, p: Point) -> char {
    term.line(p.line).get(p.col).map_or(' ', |c| c.c())
}

fn is_spacer(term: &Terminal, p: Point) -> bool {
    term.line(p.line)
        .get(p.col)
        .is_some_and(|c| c.flags() & flags::WIDE_SPACER != 0)
}

/// Text length of a line, ignoring trailing blanks.
fn line_len(term: &Terminal, line: usize) -> usize {
    let row = term.line(line);
    row.iter().rposition(|c| c.c() != ' ').map_or(0, |i| i + 1)
}

/// The next cell in reading order (skipping wide-char spacers), and
/// whether a line break was crossed. Lines continue past soft wraps.
fn step(term: &Terminal, p: Point, forward: bool) -> Option<(Point, bool)> {
    let cols = term.cols();
    let mut q = p;
    let mut crossed = false;
    loop {
        if forward {
            if q.col + 1 < cols {
                q.col += 1;
            } else if q.line + 1 < term.total_lines() {
                crossed |= !wrapped(term, q.line);
                q = Point {
                    line: q.line + 1,
                    col: 0,
                };
            } else {
                return None;
            }
        } else if q.col > 0 {
            q.col -= 1;
        } else if q.line > 0 {
            crossed |= !wrapped(term, q.line - 1);
            q = Point {
                line: q.line - 1,
                col: cols - 1,
            };
        } else {
            return None;
        }
        if !is_spacer(term, q) {
            return Some((q, crossed));
        }
    }
}

fn wrapped(term: &Terminal, line: usize) -> bool {
    term.line(line)
        .last()
        .is_some_and(|c| c.flags() & flags::WRAPPED != 0)
}

/// Class at `p`, treating cells past the end of the text as blank.
fn class_at(term: &Terminal, p: Point) -> u8 {
    class(char_at(term, p))
}

impl CopyMode {
    /// Starts at the terminal cursor, or the bottom of the view when the
    /// cursor is scrolled out of it.
    pub fn new(pane: PaneId, term: &Terminal) -> Self {
        let (x, y) = term.cursor_position();
        let line = term.history_len() + y;
        let top = term.visible_line(0);
        let cursor = if line >= top && line < top + term.rows() {
            Point { line, col: x }
        } else {
            Point {
                line: top + term.rows() - 1,
                col: 0,
            }
        };
        Self {
            pane,
            cursor,
            visual: None,
            pending_g: false,
        }
    }

    /// The selection to show, if selecting.
    pub fn selection(&self, term: &Terminal) -> Option<Selection> {
        let (kind, anchor) = self.visual?;
        Some(match kind {
            Visual::Chars => Selection {
                anchor,
                head: self.cursor,
            },
            Visual::Lines => {
                let (a, b) = if anchor.line <= self.cursor.line {
                    (anchor.line, self.cursor.line)
                } else {
                    (self.cursor.line, anchor.line)
                };
                Selection {
                    anchor: Point { line: a, col: 0 },
                    head: Point {
                        line: b,
                        col: term.cols() - 1,
                    },
                }
            }
        })
    }

    /// Handles a key: `key` is the typed character (`"h"`, `"G"`) or a
    /// lowercase key name (`"arrowleft"`, `"escape"`), `ctrl` whether Alt
    /// is held (the copy-mode modifier). Moves scroll the view to keep the
    /// cursor shown.
    pub fn key(&mut self, key: &str, ctrl: bool, term: &mut Terminal) -> Outcome {
        let pending_g = std::mem::take(&mut self.pending_g);
        let motion = match (key, ctrl) {
            ("h" | "arrowleft" | "backspace", false) => Some(Motion::Left),
            ("l" | "arrowright", false) => Some(Motion::Right),
            ("k" | "arrowup", false) | ("p", true) => Some(Motion::Up),
            ("j" | "arrowdown", false) | ("n", true) => Some(Motion::Down),
            ("w", false) => Some(Motion::WordNext),
            ("e", false) => Some(Motion::WordEnd),
            ("b", false) => Some(Motion::WordPrev),
            ("0" | "home", false) => Some(Motion::LineStart),
            ("^", false) => Some(Motion::FirstNonBlank),
            ("$" | "end", false) => Some(Motion::LineEnd),
            ("g", false) if pending_g => Some(Motion::Top),
            ("G", false) => Some(Motion::Bottom),
            ("H", false) => Some(Motion::ViewTop),
            ("M", false) => Some(Motion::ViewMiddle),
            ("L", false) => Some(Motion::ViewBottom),
            ("u", true) => Some(Motion::HalfPageUp),
            ("d", true) => Some(Motion::HalfPageDown),
            ("b", true) | ("pageup", _) => Some(Motion::PageUp),
            ("f", true) | ("pagedown", _) => Some(Motion::PageDown),
            _ => None,
        };
        if let Some(motion) = motion {
            self.cursor = self.moved(term, motion);
            term.scroll_into_view(self.cursor.line);
            return Outcome::Stay;
        }
        match (key, ctrl) {
            ("g", false) => self.pending_g = true,
            ("v", false) | (" " | "space", false) => self.toggle_visual(Visual::Chars),
            ("V", false) => self.toggle_visual(Visual::Lines),
            ("v", true) => self.toggle_visual(Visual::Chars),
            ("y" | "enter", false) => {
                // Without a selection, `y` copies the cursor's line.
                let selection = self.selection(term).unwrap_or(Selection {
                    anchor: Point {
                        line: self.cursor.line,
                        col: 0,
                    },
                    head: Point {
                        line: self.cursor.line,
                        col: term.cols() - 1,
                    },
                });
                let saved = term.selection.replace(selection);
                let text = term.selected_text();
                term.selection = saved;
                return text.map_or(Outcome::Exit, Outcome::Copy);
            }
            ("/", false) | ("f", true) => return Outcome::Search,
            ("escape", false) | ("c" | "[", true) if self.visual.is_some() => self.visual = None,
            ("escape" | "q", false) | ("c" | "[", true) => return Outcome::Exit,
            _ => {}
        }
        Outcome::Stay
    }

    fn toggle_visual(&mut self, kind: Visual) {
        self.visual = match self.visual {
            Some((k, _)) if k == kind => None,
            Some((_, anchor)) => Some((kind, anchor)),
            None => Some((kind, self.cursor)),
        };
    }

    fn moved(&self, term: &Terminal, motion: Motion) -> Point {
        let p = self.cursor;
        let last = term.total_lines() - 1;
        let rows = term.rows();
        let top = term.visible_line(0);
        let at = |line: usize, col: usize| {
            let mut q = Point {
                line: line.min(last),
                col: col.min(term.cols() - 1),
            };
            while q.col > 0 && is_spacer(term, q) {
                q.col -= 1;
            }
            q
        };
        match motion {
            Motion::Left => match step(term, p, false) {
                Some((q, false)) if q.line == p.line => q,
                _ => p,
            },
            Motion::Right => match step(term, p, true) {
                Some((q, false)) if q.line == p.line => q,
                _ => p,
            },
            Motion::Up => at(p.line.saturating_sub(1), p.col),
            Motion::Down => at(p.line + 1, p.col),
            Motion::LineStart => at(p.line, 0),
            Motion::FirstNonBlank => {
                let row = term.line(p.line);
                at(p.line, row.iter().position(|c| c.c() != ' ').unwrap_or(0))
            }
            Motion::LineEnd => at(p.line, line_len(term, p.line).saturating_sub(1)),
            Motion::Top => at(0, 0),
            Motion::Bottom => {
                let (_, y) = term.cursor_position();
                at(term.history_len() + y, 0)
            }
            Motion::ViewTop => at(top, p.col),
            Motion::ViewMiddle => at(top + rows / 2, p.col),
            Motion::ViewBottom => at(top + rows - 1, p.col),
            Motion::HalfPageUp => at(p.line.saturating_sub(rows / 2), p.col),
            Motion::HalfPageDown => at(p.line + rows / 2, p.col),
            Motion::PageUp => at(p.line.saturating_sub(rows), p.col),
            Motion::PageDown => at(p.line + rows, p.col),
            Motion::WordNext => {
                let start = class_at(term, p);
                let mut q = p;
                let mut blank = false;
                while let Some((n, crossed)) = step(term, q, true) {
                    q = n;
                    let c = class_at(term, q);
                    blank |= crossed || c == 0;
                    if c != 0 && (blank || c != start) {
                        return q;
                    }
                }
                p
            }
            Motion::WordEnd => {
                let mut q = p;
                // Past the blanks, then to the last cell of that word.
                loop {
                    let Some((n, _)) = step(term, q, true) else {
                        return q;
                    };
                    q = n;
                    if class_at(term, q) != 0 {
                        break;
                    }
                }
                let c = class_at(term, q);
                while let Some((n, false)) = step(term, q, true) {
                    if class_at(term, n) != c {
                        break;
                    }
                    q = n;
                }
                q
            }
            Motion::WordPrev => {
                let mut q = p;
                loop {
                    let Some((n, _)) = step(term, q, false) else {
                        return q;
                    };
                    q = n;
                    if class_at(term, q) != 0 {
                        break;
                    }
                }
                let c = class_at(term, q);
                while let Some((n, false)) = step(term, q, false) {
                    if class_at(term, n) != c {
                        break;
                    }
                    q = n;
                }
                q
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(text: &str) -> Terminal {
        let mut t = Terminal::new(20, 4, 100);
        t.feed(text.as_bytes());
        t
    }

    fn press(mode: &mut CopyMode, t: &mut Terminal, keys: &str) {
        for k in keys.chars() {
            mode.key(&k.to_string(), false, t);
        }
    }

    #[test]
    fn word_motions_cross_lines() {
        let mut t = term("foo.bar baz\r\n  qux");
        let mut m = CopyMode::new(1, &t);
        m.cursor = Point { line: 0, col: 0 };
        press(&mut m, &mut t, "w");
        assert_eq!(m.cursor, Point { line: 0, col: 3 });
        press(&mut m, &mut t, "w");
        assert_eq!(m.cursor, Point { line: 0, col: 4 });
        press(&mut m, &mut t, "w");
        assert_eq!(m.cursor, Point { line: 0, col: 8 });
        press(&mut m, &mut t, "w");
        assert_eq!(m.cursor, Point { line: 1, col: 2 });
        press(&mut m, &mut t, "b");
        assert_eq!(m.cursor, Point { line: 0, col: 8 });
        press(&mut m, &mut t, "e");
        assert_eq!(m.cursor, Point { line: 0, col: 10 });
        press(&mut m, &mut t, "0");
        assert_eq!(m.cursor.col, 0);
        press(&mut m, &mut t, "$");
        assert_eq!(m.cursor.col, 10);
        press(&mut m, &mut t, "j^");
        assert_eq!(m.cursor, Point { line: 1, col: 2 });
        press(&mut m, &mut t, "gg");
        assert_eq!(m.cursor, Point { line: 0, col: 0 });
        press(&mut m, &mut t, "G");
        assert_eq!(m.cursor, Point { line: 1, col: 0 });
    }

    #[test]
    fn visual_selections_copy() {
        let mut t = term("alpha beta\r\ngamma");
        let mut m = CopyMode::new(1, &t);
        m.cursor = Point { line: 0, col: 6 };
        press(&mut m, &mut t, "ve");
        assert_eq!(m.key("y", false, &mut t), Outcome::Copy("beta".into()));
        m.visual = None;
        m.cursor = Point { line: 0, col: 2 };
        press(&mut m, &mut t, "Vj");
        assert_eq!(
            m.key("y", false, &mut t),
            Outcome::Copy("alpha beta\ngamma".into())
        );
        // No selection: the cursor's line.
        m.visual = None;
        assert_eq!(m.key("y", false, &mut t), Outcome::Copy("gamma".into()));
        assert!(t.selection.is_none());
        m.visual = Some((Visual::Chars, m.cursor));
        assert_eq!(m.key("escape", false, &mut t), Outcome::Stay);
        assert!(m.visual.is_none());
        assert_eq!(m.key("escape", false, &mut t), Outcome::Exit);
    }

    #[test]
    fn moving_into_history_scrolls_the_view() {
        let lines: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
        let mut t = term(&lines);
        let mut m = CopyMode::new(1, &t);
        assert_eq!(t.display_offset, 0);
        press(&mut m, &mut t, "gg");
        assert_eq!(m.cursor.line, 0);
        assert_eq!(t.visible_line(0), 0);
        press(&mut m, &mut t, "G");
        assert_eq!(t.display_offset, 0);
    }
}
