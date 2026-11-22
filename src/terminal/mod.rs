//! Terminal state machine: screen grid, cursor, modes and scrollback.
//!
//! Implements the subset of xterm behaviour needed by shells and full-screen
//! applications (vim, tmux, htop, less, ...).

pub mod cluster;
pub mod graphics;
mod history;
pub mod input;
pub mod parser;

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;

use self::graphics::{Graphics, base64_decode};
use self::history::Line;
use self::parser::{Params, Parser, Perform};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub mod flags {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const HIDDEN: u16 = 1 << 5;
    pub const STRIKE: u16 = 1 << 6;
    /// First half of a double-width character.
    pub const WIDE: u16 = 1 << 7;
    /// Second (placeholder) half of a double-width character.
    pub const WIDE_SPACER: u16 = 1 << 8;
    /// On a row's last cell: the line continues on the next row (soft wrap),
    /// so resizing rewraps them together.
    pub const WRAPPED: u16 = 1 << 9;
    /// Blank left in the last column when a wide character wrapped early.
    pub const WIDE_PAD: u16 = 1 << 10;
}

/// One grid cell, 14 bytes: the character and its [`flags`] share 32 bits
/// (21 + 11), stored as two `u16`s so the cell is only 2-byte aligned, which
/// keeps large scrollbacks small.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    char_and_flags: [u16; 2],
    pub fg: Color,
    pub bg: Color,
    /// OSC 8 hyperlink: an index into [`Terminal::hyperlink`], 0 for none.
    pub link: u16,
}

const CHAR_BITS: u32 = 21;
const CHAR_MASK: u32 = (1 << CHAR_BITS) - 1;

impl Default for Cell {
    fn default() -> Self {
        Self {
            char_and_flags: [' ' as u16, 0],
            fg: Color::Default,
            bg: Color::Default,
            link: 0,
        }
    }
}

impl Cell {
    fn packed(&self) -> u32 {
        self.char_and_flags[0] as u32 | (self.char_and_flags[1] as u32) << 16
    }

    fn set_packed(&mut self, v: u32) {
        self.char_and_flags = [v as u16, (v >> 16) as u16];
    }

    /// The character, or the first one of a cluster.
    pub fn c(&self) -> char {
        let code = self.packed() & CHAR_MASK;
        match char::from_u32(code) {
            Some(c) => c,
            None => cluster::get(code)
                .and_then(|t| t.chars().next())
                .unwrap_or(' '),
        }
    }

    /// The full text of a multi-char cluster (emoji sequence, combining marks).
    pub fn cluster(&self) -> Option<&'static str> {
        let code = self.packed() & CHAR_MASK;
        if code >= cluster::BASE {
            cluster::get(code)
        } else {
            None
        }
    }

    fn set_code(&mut self, code: u32) {
        self.set_packed((self.packed() & !CHAR_MASK) | code);
    }

    /// Appends this cell's text (the whole cluster) to `out`.
    pub fn push_text(&self, out: &mut String) {
        match self.cluster() {
            Some(text) => out.push_str(text),
            None => out.push(self.c()),
        }
    }

    pub fn flags(&self) -> u16 {
        (self.packed() >> CHAR_BITS) as u16
    }

    pub fn set_c(&mut self, c: char) {
        self.set_packed((self.packed() & !CHAR_MASK) | c as u32);
    }

    pub fn set_flags(&mut self, flags: u16) {
        self.set_packed((self.packed() & CHAR_MASK) | (flags as u32) << CHAR_BITS);
    }

    pub fn insert(&mut self, flags: u16) {
        self.set_flags(self.flags() | flags);
    }

    pub fn remove(&mut self, flags: u16) {
        self.set_flags(self.flags() & !flags);
    }

    /// This cell's colours with another character and flags.
    pub fn with(mut self, c: char, flags: u16) -> Self {
        self.set_c(c);
        self.set_flags(flags);
        self
    }
}

type Row = Vec<Cell>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, Default)]
struct Charsets {
    /// Whether G0 / G1 are designated as DEC special graphics.
    line_drawing: [bool; 2],
    active: usize,
}

#[derive(Clone, Copy, Default)]
struct Cursor {
    x: usize,
    y: usize,
    /// Attributes applied to newly printed characters.
    template: Cell,
    /// Set after printing in the last column; the next print wraps first.
    wrap_pending: bool,
}

#[derive(Clone, Copy)]
struct SavedCursor {
    cursor: Cursor,
    origin_mode: bool,
    charsets: Charsets,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    None,
    /// 1000: press/release.
    Click,
    /// 1002: press/release and motion while a button is held.
    Drag,
    /// 1003: all motion.
    Motion,
}

pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub show_cursor: bool,
    pub focus_events: bool,
    pub mouse: MouseMode,
    pub mouse_sgr: bool,
    autowrap: bool,
    origin: bool,
    insert: bool,
    linefeed_newline: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            app_cursor: false,
            app_keypad: false,
            bracketed_paste: false,
            show_cursor: true,
            focus_events: false,
            mouse: MouseMode::None,
            mouse_sgr: false,
            autowrap: true,
            origin: false,
            insert: false,
            linefeed_newline: false,
        }
    }
}

/// A point in the full history: line 0 is the oldest scrollback line.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Point {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
}

impl Selection {
    pub fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn contains(&self, p: Point) -> bool {
        let (start, end) = self.ordered();
        p >= start && p <= end
    }
}

const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

pub struct Terminal {
    cols: usize,
    rows: usize,
    grid: Vec<Row>,
    /// The inactive screen (primary while the alternate screen is shown).
    other_grid: Vec<Row>,
    alt_screen: bool,
    scrollback: VecDeque<Line>,
    scrollback_limit: usize,
    /// History lines discarded so far; line `n` of [`Terminal::line`] has
    /// the stable absolute number `lines_dropped + n`.
    lines_dropped: u64,
    /// Images of the shown screen and of the inactive one.
    graphics: Graphics,
    other_graphics: Graphics,
    /// Cell size in pixels, for sizing images and pixel reports.
    pub cell_px: (f32, f32),
    cursor: Cursor,
    saved_cursor: Option<SavedCursor>,
    alt_saved_cursor: Option<SavedCursor>,
    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,
    charsets: Charsets,
    last_printed: Option<char>,
    /// Cell (row, column) of the last printed character, which following
    /// zero-width characters and emoji modifiers join.
    last_cell: Option<(usize, usize)>,
    parser: Parser,

    pub modes: Modes,
    pub cursor_shape: CursorShape,
    default_cursor_shape: CursorShape,
    pub title: String,
    pub title_changed: bool,
    /// Bytes to send back to the application (device reports etc.).
    pub responses: Vec<u8>,
    /// Number of lines the view is scrolled back into history.
    pub display_offset: usize,
    pub selection: Option<Selection>,
    sync_started: Option<Instant>,
    /// Colors reported for OSC 10/11 queries.
    pub report_fg: [u8; 3],
    pub report_bg: [u8; 3],
    /// Drop line feeds while the screen is still empty, so prompts that
    /// start with a newline (starship's `add_newline`, many zsh themes) sit
    /// on the top row after startup and `clear`.
    pub trim_leading_blank_lines: bool,
    /// OSC 8 targets; [`Cell::link`] `n` is `links[n - 1]`.
    links: Vec<String>,
    link_ids: HashMap<String, u16>,
    /// Link given to newly printed characters.
    current_link: u16,
    /// Clipboard writes requested with OSC 52: (primary selection, text).
    pub clipboard_writes: Vec<(bool, String)>,
    /// A BEL arrived since the app last looked.
    pub bell: bool,
    /// Desktop notifications asked for with OSC 9 or OSC 777: (title, body).
    pub notifications: Vec<(String, String)>,
    /// Directory reported by the shell with OSC 7: (host, path).
    pub reported_cwd: Option<(String, PathBuf)>,
    /// Absolute lines where shell prompts start (OSC 133;A), oldest first.
    prompts: VecDeque<u64>,
    /// Absolute line where the running command's output began (OSC 133;C).
    output_start: Option<u64>,
    /// The last finished command's output: absolute lines, end exclusive.
    last_output: Option<(u64, u64)>,
    /// Kitty keyboard protocol flags pushed by applications, for the shown
    /// screen and the inactive one.
    keyboard: Vec<u16>,
    other_keyboard: Vec<u16>,
}

const MAX_KEYBOARD_STACK: usize = 16;

/// OSC 52 payloads larger than this are ignored.
const MAX_CLIPBOARD: usize = 8 << 20;
const MAX_PROMPTS: usize = 10_000;

/// Text of a line (soft-wrapped rows joined) with each character's position.
type LogicalLine = Vec<(char, Point)>;

impl Terminal {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            grid: vec![vec![Cell::default(); cols]; rows],
            other_grid: vec![vec![Cell::default(); cols]; rows],
            alt_screen: false,
            scrollback: VecDeque::new(),
            scrollback_limit,
            lines_dropped: 0,
            graphics: Graphics::default(),
            other_graphics: Graphics::default(),
            cell_px: (8.0, 16.0),
            cursor: Cursor::default(),
            saved_cursor: None,
            alt_saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            tabs: default_tabs(cols),
            charsets: Charsets::default(),
            last_printed: None,
            last_cell: None,
            parser: Parser::default(),
            modes: Modes::default(),
            cursor_shape: CursorShape::Block,
            default_cursor_shape: CursorShape::Block,
            title: String::new(),
            title_changed: false,
            responses: Vec::new(),
            display_offset: 0,
            selection: None,
            sync_started: None,
            report_fg: [0xff; 3],
            report_bg: [0; 3],
            trim_leading_blank_lines: false,
            links: Vec::new(),
            link_ids: HashMap::new(),
            current_link: 0,
            clipboard_writes: Vec::new(),
            bell: false,
            notifications: Vec::new(),
            reported_cwd: None,
            prompts: VecDeque::new(),
            output_start: None,
            last_output: None,
            keyboard: Vec::new(),
            other_keyboard: Vec::new(),
        }
    }

    /// Cursor on the top row of an empty primary screen with no history.
    fn at_blank_top(&self) -> bool {
        let blank =
            |c: &Cell| c.c() == ' ' && c.bg == Color::Default && c.flags() & flags::INVERSE == 0;
        !self.alt_screen
            && self.cursor.y == 0
            && self.scrollback.is_empty()
            && self.grid.iter().all(|row| row.iter().all(blank))
    }

    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        if self.scrollback.len() > limit {
            let excess = self.scrollback.len() - limit;
            self.scrollback.drain(..excess);
            self.drop_lines(excess);
            self.selection = None;
            self.display_offset = self.display_offset.min(limit);
        }
    }

    /// Shape used until an application picks one, and restored by `CSI 0 q`.
    pub fn set_default_cursor_shape(&mut self, shape: CursorShape) {
        if self.cursor_shape == self.default_cursor_shape {
            self.cursor_shape = shape;
        }
        self.default_cursor_shape = shape;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn is_alt_screen(&self) -> bool {
        self.alt_screen
    }

    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor.x, self.cursor.y)
    }

    pub fn history_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Stable number of history line `line` (as passed to [`Terminal::line`]),
    /// unaffected by old lines being discarded.
    pub fn abs_line(&self, line: usize) -> u64 {
        self.lines_dropped + line as u64
    }

    pub fn graphics(&self) -> &Graphics {
        &self.graphics
    }

    /// Absolute line of screen row 0.
    fn screen_top(&self) -> u64 {
        self.abs_line(self.scrollback.len())
    }

    /// History lines discarded so far.
    pub fn lines_dropped(&self) -> u64 {
        self.lines_dropped
    }

    /// Accounts for `n` lines removed from the front of the history.
    fn drop_lines(&mut self, n: usize) {
        self.lines_dropped += n as u64;
        if !self.graphics.placements.is_empty() {
            self.graphics.forget_before(self.lines_dropped);
        }
    }

    /// True while the application has requested synchronized output (mode 2026).
    pub fn sync_in_progress(&self) -> bool {
        self.sync_started
            .is_some_and(|t| t.elapsed() < SYNC_TIMEOUT)
    }

    /// Absolute history line of the `row`-th visible row.
    pub fn visible_line(&self, row: usize) -> usize {
        self.scrollback.len() - self.display_offset + row
    }

    /// History rows are shorter than the grid when they end in blanks;
    /// missing cells are blank.
    pub fn line(&self, line: usize) -> Cow<'_, [Cell]> {
        if line < self.scrollback.len() {
            Cow::Owned(self.scrollback[line].cells())
        } else {
            Cow::Borrowed(&self.grid[line - self.scrollback.len()])
        }
    }

    pub fn scroll_display(&mut self, delta: isize) {
        let max = self.scrollback.len() as isize;
        self.display_offset = (self.display_offset as isize + delta).clamp(0, max) as usize;
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection?.ordered();
        Some(self.text_between(start, end))
    }

    /// Everything in the history and on screen, trailing blank lines left
    /// out.
    pub fn history_text(&self) -> String {
        let last = (0..self.total_lines())
            .rev()
            .find(|&l| self.line(l).iter().any(|c| !matches!(c.c(), ' ' | '\0')));
        match last {
            Some(last) => self.text_between(
                Point { line: 0, col: 0 },
                Point {
                    line: last,
                    col: usize::MAX - 1,
                },
            ),
            None => String::new(),
        }
    }

    /// Commands and output on the primary screen. A full-screen program
    /// (the alternate screen) is left out, so quitting one does not replace
    /// the shell transcript that was behind it.
    pub fn transcript(&mut self) -> String {
        if !self.alt_screen {
            return self.history_text();
        }
        std::mem::swap(&mut self.grid, &mut self.other_grid);
        let text = self.history_text();
        std::mem::swap(&mut self.grid, &mut self.other_grid);
        text
    }

    /// Shows `text` as if the shell had printed it, and leaves the cursor on
    /// the following line. Control characters other than tab and newline are
    /// dropped so a saved screen cannot replay escape sequences.
    pub fn show_transcript(&mut self, text: &str) {
        let mut bytes = Vec::with_capacity(text.len() + 2);
        for c in text.chars() {
            if c == '\n' {
                bytes.extend_from_slice(b"\r\n");
            } else if c == '\t' || !c.is_control() {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        if bytes.is_empty() {
            return;
        }
        if !bytes.ends_with(b"\n") {
            bytes.extend_from_slice(b"\r\n");
        }
        self.feed(&bytes);
    }

    /// The output of the last finished command, between its OSC 133;C and
    /// D marks. `None` without marks or once it left the history.
    pub fn last_command_output(&self) -> Option<String> {
        let (start, end) = self.last_output?;
        let end = end.checked_sub(self.lines_dropped)? as usize;
        let start = start.saturating_sub(self.lines_dropped) as usize;
        if end <= start || end > self.total_lines() {
            return None;
        }
        let text = self.text_between(
            Point {
                line: start,
                col: 0,
            },
            Point {
                line: end - 1,
                col: usize::MAX - 1,
            },
        );
        Some(text.trim_end_matches('\n').to_string())
    }

    /// Text from `start` to `end` (inclusive) with soft wraps joined and
    /// trailing blanks of each line dropped.
    fn text_between(&self, start: Point, end: Point) -> String {
        let mut out = String::new();
        for line in start.line..=end.line {
            let row = self.line(line);
            let from = if line == start.line { start.col } else { 0 };
            let to = if line == end.line {
                (end.col + 1).min(row.len())
            } else {
                row.len()
            };
            let mut text: String = row
                .get(from..to)
                .unwrap_or(&[])
                .iter()
                .filter(|c| c.flags() & flags::WIDE_SPACER == 0)
                .fold(String::new(), |mut s, c| {
                    c.push_text(&mut s);
                    s
                });
            let wrapped = row.last().is_some_and(|c| c.flags() & flags::WRAPPED != 0);
            if !wrapped {
                text.truncate(text.trim_end().len());
            }
            out.push_str(&text);
            if line != end.line && !wrapped {
                out.push('\n');
            }
        }
        out
    }

    /// Target of OSC 8 link `id` (from [`Cell::link`]).
    pub fn hyperlink(&self, id: u16) -> Option<&str> {
        self.links
            .get((id as usize).checked_sub(1)?)
            .map(String::as_str)
    }

    /// Active kitty keyboard protocol flags (0: legacy encoding).
    pub fn keyboard_flags(&self) -> u16 {
        self.keyboard.last().copied().unwrap_or(0)
    }

    /// Scrolls the view, if needed, so `line` is shown (near the middle).
    pub fn reveal(&mut self, line: usize) {
        let top = self.visible_line(0);
        if line >= top && line < top + self.rows {
            return;
        }
        let want_top = line.saturating_sub(self.rows / 2);
        self.display_offset = self.scrollback.len().saturating_sub(want_top);
    }

    /// Scrolls the view as little as possible so `line` is shown.
    pub fn scroll_into_view(&mut self, line: usize) {
        let top = self.visible_line(0);
        if line < top {
            self.display_offset = self.scrollback.len() - line;
        } else if line >= top + self.rows {
            self.display_offset = self.scrollback.len().saturating_sub(line + 1 - self.rows);
        }
    }

    /// Things on screen worth copying (URLs, paths, hashes, ...; see
    /// `quickselect.rs`): text and the cells it covers, in screen order.
    pub fn quick_select_targets(&self) -> Vec<(String, Point, Point)> {
        let top = self.visible_line(0);
        let bottom = top + self.rows;
        let mut out = Vec::new();
        let mut line = top;
        while line < bottom.min(self.total_lines()) {
            let text = self.logical_line(line);
            let last = text.last().map_or(line, |(_, p)| p.line);
            let chars: Vec<char> = text.iter().map(|(c, _)| *c).collect();
            let urls = find_urls(&text);
            let others = crate::ui::quickselect::find(&chars)
                .into_iter()
                .filter(|&(a, b)| urls.iter().all(|&(ua, ub)| b <= ua || a >= ub));
            let mut spans: Vec<(usize, usize)> = urls.iter().copied().chain(others).collect();
            spans.sort();
            for (a, b) in spans {
                let (start, end) = (text[a].1, text[b - 1].1);
                if start.line >= top && start.line < bottom {
                    out.push((chars[a..b].iter().collect(), start, end));
                }
            }
            line = last + 1;
        }
        out
    }

    /// Total lines: history plus screen.
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    fn wrapped(&self, line: usize) -> bool {
        let last = match self.scrollback.get(line) {
            Some(history) => history.last_flags(),
            None => self.line(line).last().map(Cell::flags),
        };
        last.is_some_and(|f| f & flags::WRAPPED != 0)
    }

    /// The logical line (rows joined across soft wraps) containing `line`.
    fn logical_line(&self, line: usize) -> LogicalLine {
        let mut start = line;
        while start > 0 && self.wrapped(start - 1) {
            start -= 1;
        }
        let mut out = Vec::new();
        let mut l = start;
        loop {
            self.push_line_chars(l, &mut out);
            if !self.wrapped(l) || l + 1 >= self.total_lines() {
                break;
            }
            l += 1;
        }
        out
    }

    fn push_line_chars(&self, line: usize, out: &mut LogicalLine) {
        let row = self.line(line);
        for col in 0..self.cols {
            let cell = row.get(col).copied().unwrap_or_default();
            if cell.flags() & (flags::WIDE_SPACER | flags::WIDE_PAD) == 0 {
                out.push((cell.c(), Point { line, col }));
            }
        }
    }

    /// The link at `p`: an OSC 8 hyperlink, or a URL written in the text.
    /// Returns its target and the span of cells it covers (inclusive).
    pub fn link_at(&self, p: Point) -> Option<(String, Point, Point)> {
        if p.line >= self.total_lines() {
            return None;
        }
        let row = self.line(p.line);
        let id = row.get(p.col).map_or(0, |c| c.link);
        if let Some(uri) = self.hyperlink(id) {
            let same = |c: usize| row.get(c).is_some_and(|cell| cell.link == id);
            let mut start = p.col;
            while start > 0 && same(start - 1) {
                start -= 1;
            }
            let mut end = p.col;
            while same(end + 1) {
                end += 1;
            }
            return Some((
                uri.to_string(),
                Point {
                    line: p.line,
                    col: start,
                },
                Point {
                    line: p.line,
                    col: end,
                },
            ));
        }
        let text = self.logical_line(p.line);
        find_urls(&text).into_iter().find_map(|(a, b)| {
            let (start, end) = (text[a].1, text[b - 1].1);
            (start <= p && p <= end)
                .then(|| (text[a..b].iter().map(|(c, _)| c).collect(), start, end))
        })
    }

    /// Every match of `query` in the history and screen, oldest first.
    /// Case is ignored unless the query has capitals.
    pub fn search(&self, query: &str) -> Vec<(Point, Point)> {
        let fold = !query.chars().any(char::is_uppercase);
        let norm = |c: char| {
            if fold {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c
            }
        };
        let needle: Vec<char> = query.chars().map(norm).collect();
        let mut found = Vec::new();
        if needle.is_empty() {
            return found;
        }
        let mut text = Vec::new();
        for line in 0..self.total_lines() {
            self.push_line_chars(line, &mut text);
            if self.wrapped(line) && line + 1 < self.total_lines() {
                continue;
            }
            if text.len() >= needle.len() {
                for i in 0..=text.len() - needle.len() {
                    if needle
                        .iter()
                        .zip(&text[i..])
                        .all(|(n, (c, _))| *n == norm(*c))
                    {
                        found.push((text[i].1, text[i + needle.len() - 1].1));
                    }
                }
            }
            text.clear();
        }
        found
    }

    /// Scrolls so the previous (`up`) or next shell prompt is on the top
    /// row. Returns false when there is none.
    pub fn scroll_to_prompt(&mut self, up: bool) -> bool {
        let top = self.abs_line(self.visible_line(0));
        let target = if up {
            self.prompts.iter().rev().find(|&&l| l < top)
        } else {
            self.prompts.iter().find(|&&l| l > top)
        };
        let Some(&abs) = target else {
            if !up && self.display_offset > 0 {
                self.display_offset = 0;
                return true;
            }
            return false;
        };
        let line = abs.saturating_sub(self.lines_dropped) as usize;
        self.display_offset = self.scrollback.len().saturating_sub(line);
        true
    }

    fn shift_prompts(&mut self, delta: i64) {
        if delta != 0 {
            for line in &mut self.prompts {
                *line = line.saturating_add_signed(delta);
            }
            let shift = |l: u64| l.saturating_add_signed(delta);
            self.output_start = self.output_start.map(shift);
            self.last_output = self.last_output.map(|(a, b)| (shift(a), shift(b)));
        }
    }

    fn intern_link(&mut self, key: String, uri: &str) -> u16 {
        if let Some(&id) = self.link_ids.get(&key) {
            return id;
        }
        if self.links.len() >= u16::MAX as usize - 1 {
            return 0;
        }
        self.links.push(uri.to_string());
        let id = self.links.len() as u16;
        self.link_ids.insert(key, id);
        id
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.selection = None;
        self.display_offset = 0;
        let (old_cols, old_rows) = (self.cols, self.rows);
        let limit = self.scrollback_limit;

        // The primary screen is rewrapped; full-screen programs on the
        // alternate screen redraw themselves, so it is just cropped.
        // Rewrapping changes how many rows precede a line, so primary-screen
        // images keep their distance (in rows) from the cursor instead.
        let cursor_line = |t: &Self, y: usize| t.screen_top() as i64 + y as i64;
        if self.alt_screen {
            crop(&mut self.grid, &mut self.cursor, cols, rows);
            let mut primary = self.saved_cursor.map(|s| s.cursor).unwrap_or_default();
            let before = cursor_line(self, primary.y);
            reflow(
                &mut self.scrollback,
                &mut self.other_grid,
                &mut primary,
                old_cols,
                (cols, rows),
                old_rows,
                limit,
            );
            self.other_graphics
                .shift(cursor_line(self, primary.y) - before);
            if let Some(saved) = &mut self.saved_cursor {
                saved.cursor = primary;
            }
        } else {
            let before = cursor_line(self, self.cursor.y);
            reflow(
                &mut self.scrollback,
                &mut self.grid,
                &mut self.cursor,
                old_cols,
                (cols, rows),
                old_rows,
                limit,
            );
            let delta = cursor_line(self, self.cursor.y) - before;
            self.graphics.shift(delta);
            self.shift_prompts(delta);
            self.other_grid = vec![vec![Cell::default(); cols]; rows];
            if let Some(saved) = &mut self.saved_cursor {
                saved.cursor.x = saved.cursor.x.min(cols - 1);
                saved.cursor.y = saved.cursor.y.min(rows - 1);
            }
        }
        if let Some(saved) = &mut self.alt_saved_cursor {
            saved.cursor.x = saved.cursor.x.min(cols - 1);
            saved.cursor.y = saved.cursor.y.min(rows - 1);
        }

        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.tabs = default_tabs(cols);
    }

    // ---- primitives -------------------------------------------------------

    fn blank(&self) -> Cell {
        Cell {
            bg: self.cursor.template.bg,
            ..Cell::default()
        }
    }

    fn blank_row(&self) -> Row {
        vec![self.blank(); self.cols]
    }

    fn push_scrollback(&mut self, line: &[Cell]) {
        if self.scrollback_limit == 0 {
            return;
        }
        // History rows may be shorter than the grid; readers treat missing
        // cells as blank.
        let used = line
            .iter()
            .rposition(|c| *c != Cell::default())
            .map_or(0, |i| i + 1);
        let line = Line::new(&line[..used]);
        if self.scrollback.len() >= self.scrollback_limit {
            self.scrollback.pop_front();
            self.drop_lines(1);
            if let Some(sel) = &mut self.selection {
                if sel.anchor.line == 0 || sel.head.line == 0 {
                    self.selection = None;
                } else {
                    sel.anchor.line -= 1;
                    sel.head.line -= 1;
                }
            }
        } else if self.display_offset > 0 {
            self.display_offset += 1;
        }
        self.scrollback.push_back(line);
    }

    fn scroll_up(&mut self, count: usize) {
        let count = count.min(self.scroll_bottom - self.scroll_top + 1);
        let into_history = self.scroll_top == 0 && !self.alt_screen && self.scrollback_limit > 0;
        if !into_history {
            self.scroll_images(-(count as i64));
        }
        for _ in 0..count {
            let mut line = self.grid.remove(self.scroll_top);
            if self.scroll_top == 0 && !self.alt_screen {
                self.push_scrollback(&line);
            }
            line.fill(self.blank());
            self.grid.insert(self.scroll_bottom, line);
        }
    }

    /// Moves images inside the scroll region along with its text.
    fn scroll_images(&mut self, delta: i64) {
        if !self.graphics.placements.is_empty() {
            let top = self.screen_top();
            let region = top + self.scroll_top as u64..top + self.scroll_bottom as u64 + 1;
            self.graphics.scroll(region, delta);
        }
    }

    fn scroll_down(&mut self, count: usize) {
        let count = count.min(self.scroll_bottom - self.scroll_top + 1);
        self.scroll_images(count as i64);
        for _ in 0..count {
            self.grid.remove(self.scroll_bottom);
            let blank = self.blank_row();
            self.grid.insert(self.scroll_top, blank);
        }
    }

    fn linefeed(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.y == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.y + 1 < self.rows {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.y == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    fn goto(&mut self, x: usize, y: usize) {
        let (min_y, max_y) = if self.modes.origin {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.rows - 1)
        };
        self.cursor.x = x.min(self.cols - 1);
        self.cursor.y = (y + min_y).min(max_y);
        self.cursor.wrap_pending = false;
    }

    fn goto_row(&mut self, y: usize) {
        let min_y = if self.modes.origin {
            self.scroll_top
        } else {
            0
        };
        self.goto(self.cursor.x, y.saturating_sub(min_y));
    }

    fn move_up(&mut self, n: usize) {
        let limit = if self.cursor.y >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cursor.y = self.cursor.y.saturating_sub(n).max(limit);
        self.cursor.wrap_pending = false;
    }

    fn move_down(&mut self, n: usize) {
        let limit = if self.cursor.y <= self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.rows - 1
        };
        self.cursor.y = (self.cursor.y + n).min(limit);
        self.cursor.wrap_pending = false;
    }

    /// Clear `[from, to)` on row `y`, fixing up split wide characters.
    fn clear_cells(&mut self, y: usize, from: usize, to: usize) {
        let blank = self.blank();
        let Some(row) = self.grid.get_mut(y) else {
            return;
        };
        let to = to.min(row.len());
        if from >= to {
            return;
        }
        if from > 0 && row[from].flags() & flags::WIDE_SPACER != 0 {
            row[from - 1] = blank;
        }
        if to < row.len() && row[to].flags() & flags::WIDE_SPACER != 0 {
            row[to] = blank;
        }
        row[from..to].fill(blank);
    }

    fn print_char(&mut self, c: char) {
        let c = if self.charsets.line_drawing[self.charsets.active] {
            dec_special(c)
        } else {
            c
        };
        if self.join_cluster(c) {
            return;
        }
        let width = match c.width() {
            Some(0) | None => return,
            Some(w) => w,
        };

        if self.cursor.wrap_pending && self.modes.autowrap {
            let (last, y) = (self.cols - 1, self.cursor.y);
            self.grid[y][last].insert(flags::WRAPPED);
            self.cursor.x = 0;
            self.linefeed();
        }
        if width == 2 && self.cursor.x + 1 >= self.cols {
            if !self.modes.autowrap || self.cols < 2 {
                return;
            }
            let (x, y) = (self.cursor.x, self.cursor.y);
            self.clear_cells(y, x, x + 1);
            self.grid[y][x].insert(flags::WRAPPED | flags::WIDE_PAD);
            self.cursor.x = 0;
            self.linefeed();
        }

        let (x, y) = (self.cursor.x, self.cursor.y);
        if self.modes.insert {
            let blank = self.blank();
            let row = &mut self.grid[y];
            for _ in 0..width {
                row.insert(x, blank);
                row.pop();
            }
        }
        self.clear_cells(y, x, x + width);

        let mut cell = self.cursor.template;
        cell.set_c(c);
        cell.link = self.current_link;
        cell.remove(flags::WIDE | flags::WIDE_SPACER | flags::WRAPPED | flags::WIDE_PAD);
        if width == 2 {
            cell.insert(flags::WIDE);
            self.grid[y][x] = cell;
            self.grid[y][x + 1] =
                cell.with(' ', (cell.flags() & !flags::WIDE) | flags::WIDE_SPACER);
        } else {
            self.grid[y][x] = cell;
        }
        self.last_printed = Some(c);
        self.last_cell = Some((y, x));

        if x + width >= self.cols {
            self.cursor.x = self.cols - 1;
            self.cursor.wrap_pending = true;
        } else {
            self.cursor.x = x + width;
        }
    }

    /// Adds `c` to the cluster of the character just printed when it
    /// continues it (see [`cluster::extends`]). An emoji presentation
    /// selector or a completed flag widens a narrow cell to two columns.
    fn join_cluster(&mut self, c: char) -> bool {
        let Some((y, x)) = self.last_cell else {
            return false;
        };
        let Some(cell) = self.grid.get(y).and_then(|row| row.get(x)).copied() else {
            return false;
        };
        let wide = cell.flags() & flags::WIDE != 0;
        let after = x + if wide { 2 } else { 1 };
        let in_place = self.cursor.y == y
            && if self.cursor.wrap_pending {
                after == self.cols
            } else {
                self.cursor.x == after
            };
        if !in_place {
            self.last_cell = None;
            return false;
        }
        let mut text = String::new();
        cell.push_text(&mut text);
        if !cluster::extends(&text, c) {
            return false;
        }
        text.push(c);
        let Some(code) = cluster::intern(&text) else {
            return true;
        };
        self.grid[y][x].set_code(code);
        let widen = c == cluster::VS16 || cluster::is_regional_indicator(c);
        if widen && !wide && !self.cursor.wrap_pending && x + 1 < self.cols {
            self.grid[y][x].insert(flags::WIDE);
            let spacer = self.grid[y][x].with(
                ' ',
                (self.grid[y][x].flags() & !flags::WIDE) | flags::WIDE_SPACER,
            );
            self.grid[y][x + 1] = spacer;
            if x + 2 >= self.cols {
                self.cursor.x = self.cols - 1;
                self.cursor.wrap_pending = true;
            } else {
                self.cursor.x = x + 2;
            }
        }
        true
    }

    fn save_cursor(&mut self) {
        let saved = SavedCursor {
            cursor: self.cursor,
            origin_mode: self.modes.origin,
            charsets: self.charsets,
        };
        if self.alt_screen {
            self.alt_saved_cursor = Some(saved);
        } else {
            self.saved_cursor = Some(saved);
        }
    }

    fn restore_cursor(&mut self) {
        let saved = if self.alt_screen {
            self.alt_saved_cursor
        } else {
            self.saved_cursor
        };
        match saved {
            Some(s) => {
                self.cursor = s.cursor;
                self.modes.origin = s.origin_mode;
                self.charsets = s.charsets;
            }
            None => {
                self.cursor = Cursor::default();
                self.modes.origin = false;
                self.charsets = Charsets::default();
            }
        }
        self.cursor.x = self.cursor.x.min(self.cols - 1);
        self.cursor.y = self.cursor.y.min(self.rows - 1);
    }

    fn set_alt_screen(&mut self, enable: bool, clear: bool) {
        if enable == self.alt_screen {
            return;
        }
        std::mem::swap(&mut self.grid, &mut self.other_grid);
        std::mem::swap(&mut self.graphics, &mut self.other_graphics);
        std::mem::swap(&mut self.keyboard, &mut self.other_keyboard);
        if enable {
            self.keyboard.clear();
        }
        self.alt_screen = enable;
        self.selection = None;
        self.display_offset = 0;
        if enable && clear {
            let blank = self.blank_row();
            self.grid.iter_mut().for_each(|r| *r = blank.clone());
            self.graphics.placements.clear();
        }
    }

    fn reset(&mut self) {
        let (cols, rows, limit) = (self.cols, self.rows, self.scrollback_limit);
        let (fg, bg, shape) = (self.report_fg, self.report_bg, self.default_cursor_shape);
        let (trim, cell_px) = (self.trim_leading_blank_lines, self.cell_px);
        *self = Terminal::new(cols, rows, limit);
        self.cell_px = cell_px;
        self.report_fg = fg;
        self.report_bg = bg;
        self.trim_leading_blank_lines = trim;
        self.set_default_cursor_shape(shape);
        self.title_changed = true;
    }

    // ---- control sequence handlers ---------------------------------------

    fn erase_display(&mut self, mode: u16) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        match mode {
            0 => {
                self.clear_cells(y, x, self.cols);
                for row in y + 1..self.rows {
                    self.clear_cells(row, 0, self.cols);
                }
            }
            1 => {
                for row in 0..y {
                    self.clear_cells(row, 0, self.cols);
                }
                self.clear_cells(y, 0, x + 1);
            }
            2 => {
                for row in 0..self.rows {
                    self.clear_cells(row, 0, self.cols);
                }
                let top = self.screen_top();
                self.graphics.clear_lines(top..top + self.rows as u64);
            }
            3 => {
                let top = self.screen_top();
                self.graphics.clear_lines(0..top);
                self.lines_dropped += self.scrollback.len() as u64;
                self.scrollback.clear();
                self.display_offset = 0;
                self.selection = None;
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        match mode {
            0 => self.clear_cells(y, x, self.cols),
            1 => self.clear_cells(y, 0, x + 1),
            2 => self.clear_cells(y, 0, self.cols),
            _ => {}
        }
    }

    fn insert_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - self.cursor.y + 1);
        for _ in 0..n {
            self.grid.remove(self.scroll_bottom);
            let blank = self.blank_row();
            self.grid.insert(self.cursor.y, blank);
        }
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - self.cursor.y + 1);
        for _ in 0..n {
            self.grid.remove(self.cursor.y);
            let blank = self.blank_row();
            self.grid.insert(self.scroll_bottom, blank);
        }
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
    }

    fn insert_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let blank = self.blank();
        let n = n.min(self.cols - x);
        let row = &mut self.grid[y];
        row.splice(x..x, std::iter::repeat_n(blank, n));
        row.truncate(self.cols);
        self.cursor.wrap_pending = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let blank = self.blank();
        let n = n.min(self.cols - x);
        let row = &mut self.grid[y];
        row.drain(x..x + n);
        row.extend(std::iter::repeat_n(blank, n));
        self.cursor.wrap_pending = false;
    }

    fn set_mode(&mut self, private: bool, mode: u16, on: bool) {
        if !private {
            match mode {
                4 => self.modes.insert = on,
                20 => self.modes.linefeed_newline = on,
                _ => log::debug!("unhandled mode {mode} = {on}"),
            }
            return;
        }
        match mode {
            1 => self.modes.app_cursor = on,
            6 => {
                self.modes.origin = on;
                self.goto(0, 0);
            }
            7 => self.modes.autowrap = on,
            25 => self.modes.show_cursor = on,
            47 | 1047 => self.set_alt_screen(on, on),
            1048 => {
                if on {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
            }
            1049 => {
                if on {
                    self.save_cursor();
                    self.set_alt_screen(true, true);
                } else {
                    self.set_alt_screen(false, false);
                    self.restore_cursor();
                }
            }
            1000 => {
                self.modes.mouse = if on {
                    MouseMode::Click
                } else {
                    MouseMode::None
                }
            }
            1002 => self.modes.mouse = if on { MouseMode::Drag } else { MouseMode::None },
            1003 => {
                self.modes.mouse = if on {
                    MouseMode::Motion
                } else {
                    MouseMode::None
                }
            }
            1004 => self.modes.focus_events = on,
            1006 => self.modes.mouse_sgr = on,
            2004 => self.modes.bracketed_paste = on,
            2026 => self.sync_started = on.then(Instant::now),
            12 | 1005 | 1015 => {}
            _ => log::debug!("unhandled private mode {mode} = {on}"),
        }
    }

    fn report_mode(&mut self, private: bool, mode: u16) {
        // DECRQM: 1 = set, 2 = reset, 0 = not recognised.
        let flag = |on: bool| if on { 1 } else { 2 };
        let state = match (private, mode) {
            (true, 1) => flag(self.modes.app_cursor),
            (true, 7) => flag(self.modes.autowrap),
            (true, 25) => flag(self.modes.show_cursor),
            (true, 47 | 1047 | 1049) => flag(self.alt_screen),
            (true, 2004) => flag(self.modes.bracketed_paste),
            (true, 2026) => flag(self.sync_started.is_some()),
            (false, 4) => flag(self.modes.insert),
            _ => 0,
        };
        let prefix = if private { "?" } else { "" };
        self.respond(&format!("\x1b[{prefix}{mode};{state}$y"));
    }

    fn sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.groups().collect();
        if groups.is_empty() {
            self.cursor.template = Cell::default();
            return;
        }
        let t = &mut self.cursor.template;
        let mut i = 0;
        while i < groups.len() {
            let g = groups[i];
            match g[0] {
                0 => *t = Cell::default(),
                1 => t.insert(flags::BOLD),
                2 => t.insert(flags::DIM),
                3 => t.insert(flags::ITALIC),
                4 => {
                    if g.get(1) == Some(&0) {
                        t.remove(flags::UNDERLINE)
                    } else {
                        t.insert(flags::UNDERLINE)
                    }
                }
                7 => t.insert(flags::INVERSE),
                8 => t.insert(flags::HIDDEN),
                9 => t.insert(flags::STRIKE),
                21 => t.insert(flags::UNDERLINE),
                22 => t.remove(flags::BOLD | flags::DIM),
                23 => t.remove(flags::ITALIC),
                24 => t.remove(flags::UNDERLINE),
                27 => t.remove(flags::INVERSE),
                28 => t.remove(flags::HIDDEN),
                29 => t.remove(flags::STRIKE),
                30..=37 => t.fg = Color::Indexed((g[0] - 30) as u8),
                39 => t.fg = Color::Default,
                40..=47 => t.bg = Color::Indexed((g[0] - 40) as u8),
                49 => t.bg = Color::Default,
                90..=97 => t.fg = Color::Indexed((g[0] - 90 + 8) as u8),
                100..=107 => t.bg = Color::Indexed((g[0] - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let (color, consumed) = parse_extended_color(&groups[i..]);
                    i += consumed;
                    if let Some(color) = color {
                        match g[0] {
                            38 => t.fg = color,
                            48 => t.bg = color,
                            _ => {} // underline color: not rendered
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn respond(&mut self, s: &str) {
        self.responses.extend_from_slice(s.as_bytes());
    }
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell = true,
            0x08 => {
                self.cursor.x = self.cursor.x.saturating_sub(1);
                self.cursor.wrap_pending = false;
            }
            0x09 => {
                let next = (self.cursor.x + 1..self.cols).find(|&x| self.tabs[x]);
                self.cursor.x = next.unwrap_or(self.cols - 1);
                self.cursor.wrap_pending = false;
            }
            0x0a..=0x0c if self.trim_leading_blank_lines && self.at_blank_top() => {
                self.cursor.wrap_pending = false;
            }
            0x0a..=0x0c => {
                self.linefeed();
                if self.modes.linefeed_newline {
                    self.cursor.x = 0;
                }
            }
            0x0d => {
                self.cursor.x = 0;
                self.cursor.wrap_pending = false;
            }
            0x0e => self.charsets.active = 1,
            0x0f => self.charsets.active = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], action: char) {
        let private = intermediates
            .first()
            .copied()
            .filter(|b| (0x3c..=0x3f).contains(b));
        let trailing = intermediates
            .last()
            .copied()
            .filter(|b| (0x20..=0x2f).contains(b));
        let n = |i: usize| params.get(i, 1) as usize;

        match (private, trailing, action) {
            (None, None, '@') => self.insert_chars(n(0)),
            (None, None, 'A') => self.move_up(n(0)),
            (None, None, 'B' | 'e') => self.move_down(n(0)),
            (None, None, 'C' | 'a') => {
                self.cursor.x = (self.cursor.x + n(0)).min(self.cols - 1);
                self.cursor.wrap_pending = false;
            }
            (None, None, 'D') => {
                self.cursor.x = self.cursor.x.saturating_sub(n(0));
                self.cursor.wrap_pending = false;
            }
            (None, None, 'E') => {
                self.move_down(n(0));
                self.cursor.x = 0;
            }
            (None, None, 'F') => {
                self.move_up(n(0));
                self.cursor.x = 0;
            }
            (None, None, 'G' | '`') => {
                self.cursor.x = (n(0) - 1).min(self.cols - 1);
                self.cursor.wrap_pending = false;
            }
            (None, None, 'H' | 'f') => self.goto(n(1) - 1, n(0) - 1),
            (None, None, 'I') => {
                for _ in 0..n(0) {
                    self.execute(0x09);
                }
            }
            (None | Some(b'?'), None, 'J') => self.erase_display(params.get(0, 0)),
            (None | Some(b'?'), None, 'K') => self.erase_line(params.get(0, 0)),
            (None, None, 'L') => self.insert_lines(n(0)),
            (None, None, 'M') => self.delete_lines(n(0)),
            (None, None, 'P') => self.delete_chars(n(0)),
            (None, None, 'S') => self.scroll_up(n(0)),
            (None, None, 'T') => self.scroll_down(n(0)),
            (None, None, 'X') => {
                let (x, y) = (self.cursor.x, self.cursor.y);
                self.clear_cells(y, x, x + n(0));
            }
            (None, None, 'Z') => {
                for _ in 0..n(0) {
                    let prev = (0..self.cursor.x).rev().find(|&x| self.tabs[x]);
                    self.cursor.x = prev.unwrap_or(0);
                }
            }
            (None, None, 'b') => {
                if let Some(c) = self.last_printed {
                    for _ in 0..n(0).min(self.cols * self.rows) {
                        self.print_char(c);
                    }
                }
            }
            (None, None, 'c') => self.respond("\x1b[?62;22c"),
            (Some(b'>'), None, 'c') => self.respond("\x1b[>0;100;0c"),
            (None, None, 'd') => self.goto_row(n(0) - 1),
            (None, None, 'g') => match params.get(0, 0) {
                0 => self.tabs[self.cursor.x] = false,
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            (p, None, 'h' | 'l') => {
                for group in params.groups() {
                    self.set_mode(p == Some(b'?'), group[0], action == 'h');
                }
            }
            (None, None, 'm') => self.sgr(params),
            (None, None, 'n') => match params.get(0, 0) {
                5 => self.respond("\x1b[0n"),
                6 => {
                    let y = if self.modes.origin {
                        self.cursor.y - self.scroll_top
                    } else {
                        self.cursor.y
                    };
                    self.respond(&format!("\x1b[{};{}R", y + 1, self.cursor.x + 1));
                }
                _ => {}
            },
            (p, Some(b'$'), 'p') => self.report_mode(p == Some(b'?'), params.get(0, 0)),
            (None, Some(b'!'), 'p') => {
                self.modes = Modes::default();
                self.cursor.template = Cell::default();
                self.scroll_top = 0;
                self.scroll_bottom = self.rows - 1;
                self.charsets = Charsets::default();
            }
            (None, Some(b' '), 'q') => {
                self.cursor_shape = match params.get(0, 0) {
                    1 | 2 => CursorShape::Block,
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => self.default_cursor_shape,
                }
            }
            (None, None, 'r') => {
                let top = n(0) - 1;
                let bottom = (params.get(1, self.rows as u16) as usize).min(self.rows) - 1;
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.goto(0, 0);
                }
            }
            (None, None, 's') => self.save_cursor(),
            (None, None, 'u') => self.restore_cursor(),
            (Some(b'?'), None, 'u') => self.respond(&format!("\x1b[?{}u", self.keyboard_flags())),
            (Some(b'>'), None, 'u') => {
                if self.keyboard.len() >= MAX_KEYBOARD_STACK {
                    self.keyboard.remove(0);
                }
                self.keyboard.push(params.get(0, 0) & 0x1f);
            }
            (Some(b'<'), None, 'u') => {
                let n = (params.get(0, 1) as usize).max(1);
                self.keyboard
                    .truncate(self.keyboard.len().saturating_sub(n));
            }
            (Some(b'='), None, 'u') => {
                let flags = params.get(0, 0) & 0x1f;
                if self.keyboard.is_empty() {
                    self.keyboard.push(0);
                }
                let top = self.keyboard.last_mut().unwrap();
                *top = match params.get(1, 1) {
                    2 => *top | flags,
                    3 => *top & !flags,
                    _ => flags,
                };
            }
            (None, None, 't') => {
                let (cw, ch) = (self.cell_px.0 as usize, self.cell_px.1 as usize);
                match params.get(0, 0) {
                    14 => self.respond(&format!("\x1b[4;{};{}t", self.rows * ch, self.cols * cw)),
                    16 => self.respond(&format!("\x1b[6;{ch};{cw}t")),
                    18 => self.respond(&format!("\x1b[8;{};{}t", self.rows, self.cols)),
                    _ => {}
                }
            }
            _ => log::debug!(
                "unhandled CSI {:?} {:?} {action}",
                String::from_utf8_lossy(intermediates),
                params.groups().collect::<Vec<_>>()
            ),
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.cursor.x = 0;
                self.linefeed();
            }
            ([], b'H') => self.tabs[self.cursor.x] = true,
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.reset(),
            ([], b'=') => self.modes.app_keypad = true,
            ([], b'>') => self.modes.app_keypad = false,
            ([], b'\\') => {}
            ([b'('], c) => self.charsets.line_drawing[0] = c == b'0',
            ([b')'], c) => self.charsets.line_drawing[1] = c == b'0',
            ([b'#'], b'8') => {
                let cell = Cell::default().with('E', 0);
                self.grid.iter_mut().for_each(|r| r.fill(cell));
            }
            _ => log::debug!("unhandled ESC {:?} {}", intermediates, byte as char),
        }
    }

    fn apc_dispatch(&mut self, data: &[u8]) {
        let Some(body) = data.strip_prefix(b"G") else {
            return;
        };
        let ctx = graphics::Context {
            cursor_line: self.screen_top() + self.cursor.y as u64,
            cursor_col: self.cursor.x,
            screen_top: self.screen_top(),
            rows: self.rows,
            cell: self.cell_px,
        };
        let outcome = self.graphics.command(body, &ctx);
        if let Some(response) = outcome.response {
            self.respond(&response);
        }
        // The cursor ends on the image's last row, just right of it.
        if let Some((cols, rows)) = outcome.move_cursor {
            for _ in 1..rows {
                self.linefeed();
            }
            self.cursor.x = (self.cursor.x + cols).min(self.cols - 1);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]]) {
        match params {
            [b"0" | b"2", rest @ ..] => {
                self.title = String::from_utf8_lossy(&rest.join(&b';')).into_owned();
                self.title_changed = true;
            }
            [b"10", b"?"] => {
                let [r, g, b] = self.report_fg;
                self.respond(&format!(
                    "\x1b]10;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
                ));
            }
            [b"11", b"?"] => {
                let [r, g, b] = self.report_bg;
                self.respond(&format!(
                    "\x1b]11;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
                ));
            }
            [b"7", rest @ ..] => {
                let url = String::from_utf8_lossy(&rest.join(&b';')).into_owned();
                self.reported_cwd = parse_file_url(&url);
            }
            [b"8", params, uri @ ..] => {
                let uri = String::from_utf8_lossy(&uri.join(&b';')).into_owned();
                self.current_link = if uri.is_empty() {
                    0
                } else {
                    let id = String::from_utf8_lossy(params)
                        .split(':')
                        .find_map(|kv| kv.strip_prefix("id=").map(str::to_string))
                        .unwrap_or_default();
                    self.intern_link(format!("{id}\0{uri}"), &uri)
                };
            }
            [b"52", target, data] if data.len() <= MAX_CLIPBOARD * 4 / 3 + 4 => {
                // Reading the clipboard back ("?") is not offered: any program,
                // even over ssh, could read it silently.
                if *data != b"?"
                    && let Some(text) = base64_decode(data).and_then(|d| String::from_utf8(d).ok())
                {
                    let primary =
                        !target.is_empty() && target.iter().all(|&c| c == b'p' || c == b's');
                    self.clipboard_writes.push((primary, text));
                }
            }
            // OSC 9 ; 4 ; ... is ConEmu's progress report, not a notification.
            [b"9", body @ ..] if !body.is_empty() && body[0] != b"4" => {
                let body = String::from_utf8_lossy(&body.join(&b';')).into_owned();
                self.push_notification(String::new(), body);
            }
            [b"777", b"notify", title, body @ ..] => {
                let title = String::from_utf8_lossy(title).into_owned();
                let body = String::from_utf8_lossy(&body.join(&b';')).into_owned();
                self.push_notification(title, body);
            }
            [b"133", kind, ..] if kind.first() == Some(&b'C') && !self.alt_screen => {
                self.output_start = Some(self.screen_top() + self.cursor.y as u64);
            }
            [b"133", kind, ..] if kind.first() == Some(&b'D') && !self.alt_screen => {
                if let Some(start) = self.output_start.take() {
                    let end =
                        self.screen_top() + self.cursor.y as u64 + u64::from(self.cursor.x > 0);
                    self.last_output = (end > start).then_some((start, end));
                }
            }
            [b"133", kind, ..] if kind.first() == Some(&b'A') && !self.alt_screen => {
                let line = self.screen_top() + self.cursor.y as u64;
                if self.prompts.back() != Some(&line) {
                    self.prompts.retain(|&l| l < line);
                    self.prompts.push_back(line);
                    if self.prompts.len() > MAX_PROMPTS {
                        self.prompts.pop_front();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Terminal {
    /// Queues a notification; a flood keeps only the latest few.
    fn push_notification(&mut self, title: String, body: String) {
        const MAX_PENDING: usize = 8;
        const MAX_LEN: usize = 1024;
        let cut = |s: String| s.chars().take(MAX_LEN).collect::<String>();
        if self.notifications.len() >= MAX_PENDING {
            self.notifications.remove(0);
        }
        self.notifications.push((cut(title), cut(body)));
    }
}

/// `file://host/path` (OSC 7) to its host and percent-decoded path.
fn parse_file_url(url: &str) -> Option<(String, PathBuf)> {
    let rest = url.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let (host, path) = rest.split_at(slash);
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some((
        host.to_string(),
        PathBuf::from(std::ffi::OsString::from_vec(out)),
    ))
}

/// URLs in `text`, as index ranges.
fn find_urls(text: &LogicalLine) -> Vec<(usize, usize)> {
    const SCHEMES: [&str; 6] = [
        "https://", "http://", "file://", "ftp://", "mailto:", "ssh://",
    ];
    let chars: Vec<char> = text.iter().map(|(c, _)| *c).collect();
    let starts_with = |i: usize, s: &str| {
        s.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    let allowed = |c: char| !c.is_whitespace() && !c.is_control() && !"<>\"'`{}|\\^".contains(c);
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let at_word = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        let Some(scheme) = SCHEMES.iter().find(|s| at_word && starts_with(i, s)) else {
            i += 1;
            continue;
        };
        let mut end = i + scheme.len();
        while end < chars.len() && allowed(chars[end]) {
            end += 1;
        }
        // Trailing punctuation and unmatched closing brackets end a sentence,
        // not the URL.
        while let Some(&last) = chars[..end].last() {
            let count = |c: char| chars[i..end].iter().filter(|&&x| x == c).count();
            let unmatched = match last {
                ')' => count(')') > count('('),
                ']' => count(']') > count('['),
                '.' | ',' | ';' | ':' | '!' | '?' => true,
                _ => false,
            };
            if !unmatched || end <= i + scheme.len() {
                break;
            }
            end -= 1;
        }
        if end > i + scheme.len() {
            found.push((i, end));
        }
        i = end.max(i + 1);
    }
    found
}

/// Parse `38;5;n`, `38;2;r;g;b` or their colon forms. Returns the color and
/// how many *additional* groups were consumed.
fn parse_extended_color(groups: &[&[u16]]) -> (Option<Color>, usize) {
    let first = groups[0];
    if first.len() > 1 {
        let color = match first[1] {
            5 => first.get(2).map(|&n| Color::Indexed(n as u8)),
            2 => {
                let rgb = if first.len() >= 6 {
                    &first[3..6]
                } else {
                    first.get(2..5).unwrap_or(&[])
                };
                (rgb.len() == 3).then(|| Color::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
            }
            _ => None,
        };
        return (color, 0);
    }
    match groups.get(1).map(|g| g[0]) {
        Some(5) => (groups.get(2).map(|g| Color::Indexed(g[0] as u8)), 2),
        Some(2) if groups.len() >= 5 => (
            Some(Color::Rgb(
                groups[2][0] as u8,
                groups[3][0] as u8,
                groups[4][0] as u8,
            )),
            4,
        ),
        _ => (None, groups.len() - 1),
    }
}

fn resize_row(row: &mut Row, cols: usize) {
    row.resize(cols, Cell::default());
    if let Some(last) = row.last_mut() {
        if last.flags() & flags::WIDE != 0 {
            *last = Cell::default();
        }
        last.remove(flags::WRAPPED | flags::WIDE_PAD);
    }
}

/// Resize a screen without rewrapping, dropping rows above the cursor when
/// it would fall off the bottom.
fn crop(grid: &mut Vec<Row>, cursor: &mut Cursor, cols: usize, rows: usize) {
    let overflow = (cursor.y + 1).saturating_sub(rows);
    grid.drain(..overflow.min(grid.len()));
    cursor.y -= overflow;
    grid.resize(rows, vec![Cell::default(); cols]);
    for row in grid.iter_mut() {
        resize_row(row, cols);
    }
    cursor.x = cursor.x.min(cols - 1);
    cursor.y = cursor.y.min(rows - 1);
    cursor.wrap_pending = false;
}

fn is_wrapped(row: &[Cell]) -> bool {
    row.last().is_some_and(|c| c.flags() & flags::WRAPPED != 0)
}

fn trimmed_len(row: &[Cell]) -> usize {
    row.iter()
        .rposition(|c| *c != Cell::default())
        .map_or(0, |i| i + 1)
}

/// Rewrap history plus `grid` from `old_cols` to `size`, joining rows that
/// were soft-wrapped so long lines (prompts, command output) follow the new
/// width. The cursor stays on the same character, and the screen keeps
/// showing the same lines where possible.
fn reflow(
    scrollback: &mut VecDeque<Line>,
    grid: &mut Vec<Row>,
    cursor: &mut Cursor,
    old_cols: usize,
    (cols, rows): (usize, usize),
    old_rows: usize,
    limit: usize,
) {
    let grid_start = scrollback.len();
    let cursor_abs = grid_start + cursor.y.min(grid.len().saturating_sub(1));
    let content_end = grid
        .iter()
        .rposition(|r| trimmed_len(r) > 0)
        .map_or(0, |i| grid_start + i)
        .max(cursor_abs);

    // Split into logical lines, noting where the cursor and the top of the
    // screen fall within them. Rows are expanded one at a time and lines
    // kept compact, so a long history never exists as full cells at once.
    let mut lines: Vec<Line> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let (mut cursor_at, mut top_at) = ((0, 0), (0, 0));
    // Rows between the start of the cursor's line and the cursor.
    let (mut line_start, mut cursor_depth) = (0, 0);
    let old = scrollback
        .drain(..)
        .map(|line| line.cells())
        .chain(grid.drain(..))
        .take(content_end + 1);
    for (i, row) in old.enumerate() {
        if current.is_empty() {
            line_start = i;
        }
        if i == grid_start {
            top_at = (lines.len(), current.len());
        }
        if i == cursor_abs {
            let x = cursor.x.min(old_cols - 1) + (cursor.wrap_pending as usize);
            cursor_at = (lines.len(), current.len() + x);
            cursor_depth = i - line_start;
        }
        if is_wrapped(&row) && i != content_end {
            current.extend(
                row.into_iter()
                    .filter(|c| c.flags() & flags::WIDE_PAD == 0)
                    .map(|mut c| {
                        c.remove(flags::WRAPPED);
                        c
                    }),
            );
        } else {
            let len = trimmed_len(&row);
            current.extend(row.into_iter().take(len).map(|mut c| {
                c.remove(flags::WRAPPED | flags::WIDE_PAD);
                c
            }));
            // The cursor may sit past the line's last character.
            if lines.len() == cursor_at.0 && i >= cursor_abs && current.len() < cursor_at.1 {
                current.resize(cursor_at.1, Cell::default());
            }
            lines.push(Line::new(&current));
            current.clear();
        }
    }
    if lines.is_empty() {
        lines.push(Line::new(&[]));
    }

    // Rewrap every logical line at the new width.
    let mut out: Vec<Line> = Vec::new();
    let push = |out: &mut Vec<Line>, row: &Row| out.push(Line::new(&row[..trimmed_len(row)]));
    let (mut cursor_pos, mut top_row, mut cursor_line_row) = (None, None, 0);
    let mark = |pos: &mut Option<(usize, usize, bool)>,
                at: (usize, usize),
                here: (usize, usize),
                row: &Row,
                out: &Vec<Line>| {
        if pos.is_none() && at == here {
            *pos = Some((out.len(), row.len(), false));
        }
    };
    for (li, line) in lines.into_iter().enumerate() {
        let line = line.cells();
        if li == cursor_at.0 {
            cursor_line_row = out.len();
        }
        let mut row: Row = Vec::with_capacity(cols);
        let mut k = 0;
        loop {
            if k >= line.len() {
                for (pos, at) in [(&mut cursor_pos, cursor_at), (&mut top_row, top_at)] {
                    if pos.is_none() && at.0 == li && at.1 >= k {
                        *pos = Some(if row.len() >= cols {
                            (out.len(), cols - 1, true)
                        } else {
                            (out.len(), row.len(), false)
                        });
                    }
                }
                break;
            }
            let mut cell = line[k];
            let wide = cell.flags() & flags::WIDE != 0;
            if wide && cols < 2 {
                cell.set_c(' ');
                cell.remove(flags::WIDE);
            }
            let width = if cell.flags() & flags::WIDE != 0 {
                2
            } else {
                1
            };
            if row.len() + width > cols {
                if row.len() < cols {
                    row.push(Cell::default().with(' ', flags::WIDE_PAD));
                }
                row[cols - 1].insert(flags::WRAPPED);
                push(&mut out, &row);
                row.clear();
            }
            mark(&mut cursor_pos, cursor_at, (li, k), &row, &out);
            mark(&mut top_row, top_at, (li, k), &row, &out);
            row.push(cell);
            k += 1;
            if width == 2 {
                if wide
                    && line
                        .get(k)
                        .is_some_and(|c| c.flags() & flags::WIDE_SPACER != 0)
                {
                    mark(&mut cursor_pos, cursor_at, (li, k), &row, &out);
                    row.push(line[k]);
                    k += 1;
                } else {
                    row.push(cell.with(' ', (cell.flags() & !flags::WIDE) | flags::WIDE_SPACER));
                }
            } else if wide {
                // A wide character flattened for a one-column screen.
                if line
                    .get(k)
                    .is_some_and(|c| c.flags() & flags::WIDE_SPACER != 0)
                {
                    k += 1;
                }
            }
        }
        push(&mut out, &row);
    }

    let (mut cursor_row, mut cursor_x, mut pending) =
        cursor_pos.unwrap_or((out.len() - 1, 0, false));
    // Shells redraw the line being edited on SIGWINCH by returning to its
    // first row with the row count from the old width. Leave the cursor that
    // many rows below the line's new start so the redraw lands on it instead
    // of stacking a second prompt above or eating the line before it.
    let shell_row = cursor_line_row + cursor_depth;
    if shell_row != cursor_row {
        cursor_row = shell_row;
        cursor_x = cursor_x.min(cols - 1);
        pending = false;
        while out.len() <= cursor_row {
            out.push(Line::new(&[]));
        }
    }
    let mut start = top_row.map_or(0, |(r, _, _)| r);
    if rows > old_rows {
        start = start.saturating_sub(rows - old_rows);
    }
    start = start.min(cursor_row);
    if out.len() > start + rows {
        start = (out.len() - rows).max(start);
    }
    // Showing the bottom of a long reflow can place that window below the
    // cursor. `cursor_row - start` would then wrap, and the next erase
    // indexes a one-row screen with a huge row.
    if cursor_row < start {
        start = cursor_row;
    }
    if cursor_row >= start + rows {
        start = cursor_row + 1 - rows;
    }

    let mut screen: Vec<Row> = out
        .split_off(start)
        .into_iter()
        .take(rows)
        .map(|line| {
            let mut row = line.cells();
            row.resize(cols, Cell::default());
            row
        })
        .collect();
    screen.resize(rows, vec![Cell::default(); cols]);
    *grid = screen;

    let excess = out.len().saturating_sub(limit);
    scrollback.extend(out.into_iter().skip(excess));

    cursor.y = cursor_row.saturating_sub(start).min(rows.saturating_sub(1));
    cursor.x = cursor_x.min(cols - 1);
    cursor.wrap_pending = pending;
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|x| x % 8 == 0 && x != 0).collect()
}

/// DEC special graphics character set (used by `ESC ( 0`).
fn dec_special(c: char) -> char {
    match c {
        '`' => '◆',
        'a' => '▒',
        'f' => '°',
        'g' => '±',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &Terminal, row: usize) -> String {
        t.grid[row]
            .iter()
            .filter(|c| c.flags() & flags::WIDE_SPACER == 0)
            .map(|c| c.c())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn text_at(t: &Terminal, (a, b): (Point, Point)) -> String {
        (a.col..=b.col).map(|c| t.line(a.line)[c].c()).collect()
    }

    #[test]
    fn hyperlinks_urls_and_search() {
        let mut t = Terminal::new(30, 4, 100);
        t.feed(b"see \x1b]8;;https://example.com/x\x1b\\here\x1b]8;;\x1b\\ ok\r\n");
        t.feed(b"(visit https://rust-lang.org/learn).\r\n");
        let (uri, a, b) = t.link_at(Point { line: 0, col: 5 }).unwrap();
        assert_eq!(
            (uri.as_str(), text_at(&t, (a, b)).as_str()),
            ("https://example.com/x", "here")
        );
        assert!(t.link_at(Point { line: 0, col: 9 }).is_none());
        let (uri, ..) = t.link_at(Point { line: 1, col: 10 }).unwrap();
        assert_eq!(uri, "https://rust-lang.org/learn");

        let found = t.search("RUST");
        assert!(found.is_empty());
        let found = t.search("rust");
        assert_eq!(found.len(), 1);
        assert_eq!(text_at(&t, found[0]), "rust");
    }

    #[test]
    fn search_and_urls_span_wrapped_rows() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed(b"xx https://a.io/abc def");
        let found = t.search("abc d");
        assert_eq!(
            found,
            [(Point { line: 1, col: 6 }, Point { line: 2, col: 0 })]
        );
        let (uri, a, b) = t.link_at(Point { line: 1, col: 1 }).unwrap();
        assert_eq!(
            (uri.as_str(), a, b),
            (
                "https://a.io/abc",
                Point { line: 0, col: 3 },
                Point { line: 1, col: 8 }
            )
        );
    }

    #[test]
    fn long_wrapped_urls_are_found_from_any_row() {
        let mut t = Terminal::new(92, 44, 100);
        for i in 0..50 {
            t.feed(format!("https://example.com/very/long/path/{i:0250}\r\n").as_bytes());
        }
        let row = t.visible_line(25);
        for col in [0, 40, 87] {
            assert!(
                t.link_at(Point { line: row, col }).is_some(),
                "row {row} col {col}"
            );
        }
    }

    #[test]
    fn last_command_output_and_history_text() {
        let mut t = Terminal::new(10, 4, 100);
        assert_eq!(t.last_command_output(), None);
        t.feed(b"\x1b]133;A\x1b\\$ ls\r\n\x1b]133;C\x1b\\one\r\ntwo three four\r\n");
        t.feed(b"\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\$ ");
        assert_eq!(
            t.last_command_output().as_deref(),
            Some("one\ntwo three four")
        );
        t.feed(b"true\r\n\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\");
        assert_eq!(t.last_command_output(), None, "no output");
        // Scrolled into the history, the next one is still found.
        t.feed(b"\x1b]133;A\x1b\\$ x\r\n\x1b]133;C\x1b\\x1\r\nx2\r\n\x1b]133;D\x1b\\$ ");
        assert_eq!(t.last_command_output().as_deref(), Some("x1\nx2"));
        assert!(t.history_len() > 0);
        let all = t.history_text();
        assert!(all.starts_with("$ ls\none\ntwo three four\n"), "{all:?}");
        assert!(all.ends_with("x1\nx2\n$"), "{all:?}");
    }

    #[test]
    fn a_transcript_is_shown_again_and_ignores_the_alternate_screen() {
        let mut t = Terminal::new(20, 8, 100);
        t.show_transcript("$ echo hi\nhi\n\x1b[2J");
        t.feed(b"$ ");
        // The escape is not replayed, so the earlier lines stay put.
        assert_eq!(t.transcript(), "$ echo hi\nhi\n[2J\n$");
        t.feed(b"\x1b[?1049hhtop");
        assert!(t.is_alt_screen());
        assert_eq!(t.transcript(), "$ echo hi\nhi\n[2J\n$");
    }

    #[test]
    fn bell_and_notifications() {
        let mut t = Terminal::new(20, 3, 100);
        t.feed(b"\x1b]0;title\x07");
        assert!(!t.bell, "a BEL ending an OSC is not a bell");
        t.feed(b"ding\x07");
        assert!(t.bell);
        t.feed(b"\x1b]9;build done\x07\x1b]9;4;1;50\x07");
        t.feed(b"\x1b]777;notify;make;all; good\x1b\\");
        assert_eq!(
            t.notifications,
            [
                (String::new(), "build done".to_string()),
                ("make".to_string(), "all; good".to_string()),
            ]
        );
    }

    #[test]
    fn osc_clipboard_cwd_and_prompts() {
        let mut t = Terminal::new(20, 3, 100);
        t.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;?\x07");
        assert_eq!(t.clipboard_writes, [(false, "hello".to_string())]);
        t.feed(b"\x1b]7;file://box/home/me/a%20b\x07");
        assert_eq!(
            t.reported_cwd,
            Some(("box".into(), PathBuf::from("/home/me/a b")))
        );

        for i in 0..6 {
            t.feed(format!("\x1b]133;A\x07$ cmd{i}\r\nout\r\n").as_bytes());
        }
        // Screen shows the last prompt's output; walk back through prompts.
        assert!(t.scroll_to_prompt(true));
        let top = |t: &Terminal| {
            t.line(t.visible_line(0))
                .iter()
                .map(|c| c.c())
                .collect::<String>()
        };
        assert!(top(&t).starts_with("$ cmd4"), "{}", top(&t));
        assert!(t.scroll_to_prompt(true));
        assert!(top(&t).starts_with("$ cmd3"));
        assert!(t.scroll_to_prompt(false));
        assert!(top(&t).starts_with("$ cmd4"));
    }

    #[test]
    fn emoji_sequences_share_a_cell() {
        let mut t = Terminal::new(20, 3, 0);
        // Heart + VS16 widens to two cells; family (ZWJ), thumbs up with a
        // skin tone and a flag each take one wide cell; e + combining acute.
        t.feed("\u{2764}\u{fe0f}|👨\u{200d}👩\u{200d}👧|👍🏽|🇯🇵|e\u{301}|".as_bytes());
        let row = &t.grid[0];
        assert_eq!(row[0].cluster(), Some("\u{2764}\u{fe0f}"));
        assert!(row[0].flags() & flags::WIDE != 0);
        assert_eq!(row[2].c(), '|');
        assert_eq!(row[3].cluster(), Some("👨\u{200d}👩\u{200d}👧"));
        assert_eq!(row[5].c(), '|');
        assert_eq!(row[6].cluster(), Some("👍🏽"));
        assert_eq!(row[8].c(), '|');
        assert_eq!(row[9].cluster(), Some("🇯🇵"));
        assert!(row[9].flags() & flags::WIDE != 0);
        assert_eq!(row[11].c(), '|');
        assert_eq!(row[12].cluster(), Some("e\u{301}"));
        assert_eq!(row[12].c(), 'e');
        assert_eq!(t.cursor_position(), (14, 0));
        t.selection = Some(Selection {
            anchor: Point { line: 0, col: 0 },
            head: Point { line: 0, col: 13 },
        });
        assert_eq!(
            t.selected_text().as_deref(),
            Some("\u{2764}\u{fe0f}|👨\u{200d}👩\u{200d}👧|👍🏽|🇯🇵|e\u{301}|")
        );
        // After moving the cursor, a combining mark doesn't reach back.
        t.feed("\x1b[2;1Ha\x1b[3;1H\u{301}b".as_bytes());
        assert_eq!(t.grid[1][0].cluster(), None);
        assert_eq!(t.grid[2][0].c(), 'b');
    }

    #[test]
    fn keyboard_flags_stack_per_screen() {
        let mut t = Terminal::new(10, 3, 0);
        t.feed(b"\x1b[>1u\x1b[>11u\x1b[?u");
        assert_eq!(std::mem::take(&mut t.responses), b"\x1b[?11u");
        t.feed(b"\x1b[=4;2u");
        assert_eq!(t.keyboard_flags(), 15);
        t.feed(b"\x1b[?1049h");
        assert_eq!(t.keyboard_flags(), 0);
        t.feed(b"\x1b[>1u\x1b[?1049l");
        assert_eq!(t.keyboard_flags(), 15);
        t.feed(b"\x1b[<u");
        assert_eq!(t.keyboard_flags(), 1);
        t.feed(b"\x1b[<5u");
        assert_eq!(t.keyboard_flags(), 0);
    }

    #[test]
    fn cells_pack_char_and_flags() {
        assert_eq!(std::mem::size_of::<Cell>(), 14);
        let mut cell = Cell::default().with(char::MAX, flags::WIDE_PAD | flags::BOLD);
        assert_eq!(
            (cell.c(), cell.flags()),
            (char::MAX, flags::WIDE_PAD | flags::BOLD)
        );
        cell.remove(flags::BOLD);
        cell.set_c('é');
        assert_eq!((cell.c(), cell.flags()), ('é', flags::WIDE_PAD));
    }

    #[test]
    fn images_move_the_cursor_and_scroll_with_text() {
        let mut t = Terminal::new(20, 5, 100);
        t.cell_px = (10.0, 20.0);
        // 2x2 RGBA pixels shown over 3 columns and 2 rows.
        t.feed(b"ab\x1b_Ga=T,f=32,s=2,v=2,c=3,r=2,i=9;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
        assert_eq!(std::mem::take(&mut t.responses), b"\x1b_Gi=9;OK\x1b\\");
        assert_eq!(t.cursor_position(), (5, 1));
        let placed = t.graphics().placements[0].clone();
        assert_eq!((placed.line, placed.col, placed.cells), (0, 2, (3, 2)));

        // Scrolling the image off the top keeps it on the same text line.
        t.feed(b"\r\n\n\n\n\n");
        assert_eq!(t.history_len(), 2);
        assert_eq!(t.graphics().placements[0].line, t.abs_line(0));

        // `clear` removes images on screen, and ED 3 the rest.
        t.feed(b"\x1b[H\x1b[2J");
        assert_eq!(t.graphics().placements.len(), 1);
        t.feed(b"\x1b[3J");
        assert!(t.graphics().placements.is_empty());
    }

    #[test]
    fn alternate_screen_images_are_separate() {
        let mut t = Terminal::new(20, 5, 100);
        t.feed(b"\x1b[?1049h\x1b_Ga=T,f=24,s=1,v=1,q=2;AAAA\x1b\\");
        assert_eq!(t.graphics().placements.len(), 1);
        t.feed(b"\x1b[?1049l");
        assert!(t.graphics().placements.is_empty());
    }

    #[test]
    fn cell_pixel_size_reports() {
        let mut t = Terminal::new(80, 24, 100);
        t.cell_px = (9.0, 18.0);
        t.feed(b"\x1b[16t\x1b[14t");
        assert_eq!(t.responses, b"\x1b[6;18;9t\x1b[4;432;720t");
    }

    #[test]
    fn print_and_wrap() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed(b"hello world");
        assert_eq!(text(&t, 0), "hello");
        assert_eq!(text(&t, 1), " worl");
        assert_eq!(text(&t, 2), "d");
    }

    #[test]
    fn wide_chars_wrap_as_a_unit() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("ab日本".as_bytes());
        assert_eq!(text(&t, 0), "ab日");
        assert_eq!(text(&t, 1), "本");
        assert_eq!(t.cursor_position(), (2, 1));
    }

    #[test]
    fn resize_rewraps_soft_wrapped_lines() {
        let mut t = Terminal::new(10, 4, 100);
        t.feed(b"$ abcdefghijklmn\r\nok\r\n$ xy");
        assert_eq!(text(&t, 0), "$ abcdefgh");
        assert_eq!(text(&t, 1), "ijklmn");
        // Wider: the wrapped command joins back into one row.
        t.resize(20, 4);
        assert_eq!(text(&t, 0), "$ abcdefghijklmn");
        assert_eq!(text(&t, 1), "ok");
        assert_eq!(text(&t, 2), "$ xy");
        assert_eq!(t.cursor_position(), (4, 2));
        // Narrower: everything rewraps, history takes what no longer fits,
        // and the cursor stays after "xy".
        t.resize(6, 4);
        assert_eq!(t.history_len(), 1);
        let all: Vec<String> = (0..t.history_len() + 4)
            .map(|l| {
                t.line(l)
                    .iter()
                    .map(|c| c.c())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(all, ["$ abcd", "efghij", "klmn", "ok", "$ xy"]);
        assert_eq!(t.cursor_position(), (4, 3));
        // Back to the original width restores the original layout.
        t.resize(10, 4);
        assert_eq!(t.history_len(), 0);
        assert_eq!(text(&t, 0), "$ abcdefgh");
        assert_eq!(text(&t, 1), "ijklmn");
        assert_eq!(text(&t, 3), "$ xy");
        t.feed(b"z");
        assert_eq!(text(&t, 3), "$ xyz");
    }

    #[test]
    fn resize_keeps_wide_characters_whole() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("ab日本語".as_bytes());
        assert_eq!(text(&t, 0), "ab日");
        assert_eq!(text(&t, 1), "本語");
        t.resize(4, 3);
        assert_eq!(text(&t, 0), "ab日");
        assert_eq!(text(&t, 1), "本語");
        t.resize(3, 3);
        assert_eq!(t.history_len(), 1);
        assert_eq!(text(&t, 0), "日");
        assert_eq!(text(&t, 1), "本");
        assert_eq!(text(&t, 2), "語");
        t.resize(8, 3);
        assert_eq!(text(&t, 0), "ab日本語");
    }

    #[test]
    fn shell_redraw_after_resize_leaves_one_prompt() {
        // Shells answer SIGWINCH by returning to the start of the line they
        // are editing using the row count from the old width, then redrawing.
        let mut t = Terminal::new(20, 4, 100);
        t.feed(b"out\r\n$ abcdefghijklmn");
        t.resize(10, 4);
        t.feed(b"\r\x1b[J$ abcdefghijklmn");
        assert_eq!(text(&t, 0), "out");
        assert_eq!(text(&t, 1), "$ abcdefgh");
        assert_eq!(text(&t, 2), "ijklmn");
        assert_eq!(text(&t, 3), "");

        // The line now spans two rows, so the shell goes up one.
        t.resize(20, 4);
        t.feed(b"\r\x1b[A\x1b[J$ abcdefghijklmn");
        assert_eq!(text(&t, 0), "out");
        assert_eq!(text(&t, 1), "$ abcdefghijklmn");
        assert_eq!(text(&t, 2), "");
    }

    #[test]
    fn resize_to_one_row_keeps_the_cursor_on_the_screen() {
        let mut t = Terminal::new(10, 6, 100);
        t.feed(b"aaaa\r\nbbbb\r\ncccc\r\n");
        t.feed(b"\x1b[2A");
        t.resize(10, 1);
        assert!(t.cursor_position().1 < t.rows());
        t.feed(b"\x1b[J");
        assert_eq!(t.cursor_position().1, 0);
    }

    #[test]
    fn copying_a_wrapped_line_has_no_newline() {
        let mut t = Terminal::new(4, 3, 100);
        t.feed(b"abcdef\r\ngh");
        t.selection = Some(Selection {
            anchor: Point { line: 0, col: 0 },
            head: Point { line: 2, col: 3 },
        });
        assert_eq!(t.selected_text().unwrap(), "abcdef\ngh");
    }

    #[test]
    fn scrollback_and_clear() {
        let mut t = Terminal::new(4, 2, 100);
        t.feed(b"1\r\n2\r\n3\r\n");
        assert_eq!(t.history_len(), 2);
        assert_eq!(text(&t, 0), "3");
        t.feed(b"\x1b[2J\x1b[3J");
        assert_eq!(t.history_len(), 0);
        assert_eq!(text(&t, 0), "");
    }

    #[test]
    fn scrollback_rows_are_trimmed_and_restored() {
        let mut t = Terminal::new(10, 2, 100);
        t.feed(b"ab\r\n\x1b[41m  \x1b[0m\r\n\r\n");
        assert_eq!(t.scrollback[0].len(), 2);
        // Colored blanks are content, not padding.
        assert_eq!(t.scrollback[1].len(), 2);
        t.resize(10, 4);
        assert_eq!(text(&t, 0), "ab");
        assert!(t.grid.iter().all(|row| row.len() == 10));
    }

    #[test]
    fn sgr_colors() {
        let mut t = Terminal::new(10, 1, 0);
        t.feed(b"\x1b[1;38;2;1;2;3;48;5;200mX\x1b[0mY");
        let x = t.grid[0][0];
        assert_eq!(x.fg, Color::Rgb(1, 2, 3));
        assert_eq!(x.bg, Color::Indexed(200));
        assert!(x.flags() & flags::BOLD != 0);
        assert_eq!(t.grid[0][1].fg, Color::Default);
    }

    #[test]
    fn leading_blank_lines_are_trimmed_on_an_empty_screen() {
        let mut t = Terminal::new(10, 3, 100);
        t.trim_leading_blank_lines = true;
        // zsh's PROMPT_SP mark, erased again, then a prompt starting with "\n".
        t.feed(b"\x1b[7m%\x1b[27m         \r \r\r\n$ ls\r\n\r\nout");
        assert_eq!(text(&t, 0), "$ ls");
        assert_eq!(text(&t, 1), "");
        assert_eq!(text(&t, 2), "out");
        // `clear` empties the screen and history, so the next prompt is trimmed too.
        t.feed(b"\x1b[H\x1b[2J\x1b[3J\r\n$ ");
        assert_eq!(text(&t, 0), "$");
        assert_eq!(t.cursor_position(), (2, 0));

        let mut t = Terminal::new(10, 3, 100);
        t.feed(b"\r\n$ ");
        assert_eq!(t.cursor_position(), (2, 1));
    }

    #[test]
    fn alt_screen_restores_primary() {
        let mut t = Terminal::new(10, 2, 0);
        t.feed(b"primary\x1b[?1049h\x1b[HALT");
        assert_eq!(text(&t, 0), "ALT");
        t.feed(b"\x1b[?1049l");
        assert_eq!(text(&t, 0), "primary");
        assert_eq!(t.cursor_position(), (7, 0));
    }

    #[test]
    fn scroll_region() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed(b"a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\n");
        assert_eq!(
            (0..4).map(|r| text(&t, r)).collect::<Vec<_>>(),
            ["a", "c", "", "d"]
        );
        assert_eq!(t.history_len(), 0);
    }

    #[test]
    fn cursor_report() {
        let mut t = Terminal::new(10, 5, 0);
        t.feed(b"\x1b[3;4H\x1b[6n");
        assert_eq!(t.responses, b"\x1b[3;4R");
    }

    #[test]
    fn selection_text() {
        let mut t = Terminal::new(6, 2, 0);
        t.feed("ab日本\r\nxyz".as_bytes());
        t.selection = Some(Selection {
            anchor: Point { line: 0, col: 1 },
            head: Point { line: 1, col: 1 },
        });
        assert_eq!(t.selected_text().unwrap(), "b日本\nxy");
    }
}
