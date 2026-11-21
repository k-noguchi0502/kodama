//! Command palette: every action, color scheme and language in a list
//! filtered by fuzzy matching as you type.

use crate::config::Colors;
use crate::terminal::Terminal;
use crate::ui::tree;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// An action by config name.
    Action(String),
    Scheme(String),
    /// A language id (`en`, `ja`, ...).
    Language(String),
}

pub struct Entry {
    pub title: String,
    /// Also matched, so a translated title can still be found in English.
    pub alias: String,
    /// Key binding shown beside the title.
    pub keys: String,
    pub item: Item,
}

pub struct Palette {
    pub query: String,
    empty: String,
    entries: Vec<Entry>,
    /// Indices into `entries` matching the query, best first.
    matches: Vec<usize>,
    selected: usize,
    /// First match shown.
    scroll: usize,
}

/// Scores `text` against `query` typed as a subsequence (case ignored):
/// higher for matches at word starts and consecutive letters. `None` when
/// the letters don't all appear in order.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0;
    let mut at = 0;
    let mut previous: Option<usize> = None;
    for q in query.chars().flat_map(char::to_lowercase) {
        if q == ' ' {
            continue;
        }
        let i = at + text[at..].iter().position(|&c| c == q)?;
        let word_start = i == 0 || !text[i - 1].is_alphanumeric();
        score += 1
            + if word_start { 8 } else { 0 }
            + if previous == Some(i.wrapping_sub(1)) {
                5
            } else {
                0
            };
        previous = Some(i);
        at = i + 1;
    }
    // Shorter titles win ties.
    Some(score * 100 - text.len() as i32)
}

/// "split_right" -> "Split right".
pub fn title_of(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

impl Palette {
    pub fn new(entries: Vec<Entry>, empty: String) -> Self {
        let mut palette = Self {
            query: String::new(),
            empty,
            matches: Vec::new(),
            entries,
            selected: 0,
            scroll: 0,
        };
        palette.refilter();
        palette
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                score(&self.query, &e.title)
                    .into_iter()
                    .chain(score(&self.query, &e.alias))
                    .max()
                    .map(|s| (s, i))
            })
            .collect();
        if !self.query.is_empty() {
            scored.sort_by_key(|&(s, i)| (std::cmp::Reverse(s), i));
        }
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn input(&mut self, text: &str) {
        self.query.push_str(text);
        self.refilter();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.refilter();
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.matches.len();
        if n > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        }
    }

    pub fn chosen(&self) -> Option<&Item> {
        self.matches
            .get(self.selected)
            .map(|&i| &self.entries[i].item)
    }

    /// Rows the palette wants: the query line plus up to `max` entries.
    pub fn rows(&self, max: usize) -> usize {
        1 + self.matches.len().clamp(1, max)
    }

    /// Draws the palette into a `cols` x `rows` terminal.
    pub fn view(&mut self, cols: usize, rows: usize, colors: &Colors) -> Terminal {
        let mut t = Terminal::new(cols, rows, 0);
        let sgr = |[r, g, b]: [u8; 3], [fr, fg, fb]: [u8; 3]| {
            format!("\x1b[48;2;{r};{g};{b}m\x1b[38;2;{fr};{fg};{fb}m")
        };
        let normal = sgr(colors.tab_bar, colors.foreground);
        t.feed(format!("\x1b[?7l\x1b[?25l{normal}\x1b[2J\x1b[H").as_bytes());

        let count = format!("{}/{} ", self.matches.len(), self.entries.len());
        let prefix = " \u{f054} ";
        let room = cols.saturating_sub(tree::width(prefix) + count.len() + 1);
        let query = tree::fit_tail(&self.query, room);
        t.feed(format!("\x1b[1m{prefix}\x1b[22m{query}").as_bytes());
        let cursor = tree::width(prefix) + tree::width(&query);
        t.feed(
            format!(
                "\x1b[1;{}H\x1b[2m{count}\x1b[22m",
                cols.saturating_sub(count.len()) + 1
            )
            .as_bytes(),
        );

        let shown = rows.saturating_sub(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if shown > 0 && self.selected >= self.scroll + shown {
            self.scroll = self.selected + 1 - shown;
        }
        if self.matches.is_empty() {
            t.feed(format!("\x1b[2;1H\x1b[2m   {}\x1b[22m", self.empty).as_bytes());
        }
        for (row, &i) in self
            .matches
            .iter()
            .skip(self.scroll)
            .take(shown)
            .enumerate()
        {
            let entry = &self.entries[i];
            let selected = self.scroll + row == self.selected;
            let style = if selected {
                sgr(colors.selection, colors.foreground)
            } else {
                normal.clone()
            };
            let keys = tree::fit(&entry.keys, cols / 2);
            let keys_w = tree::width(&keys);
            let title = tree::fit(&entry.title, cols.saturating_sub(keys_w + 5));
            let marker = if selected { "\u{25b8}" } else { " " };
            t.feed(format!("\x1b[{};1H{style}\x1b[2K {marker} {title}", row + 2).as_bytes());
            if keys_w > 0 {
                t.feed(
                    format!("\x1b[{};{}H\x1b[2m{keys}\x1b[22m", row + 2, cols - keys_w).as_bytes(),
                );
            }
            t.feed(normal.as_bytes());
        }
        t.feed(format!("\x1b[1;{}H\x1b[?25h", cursor + 1).as_bytes());
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str) -> Entry {
        Entry {
            title: title.into(),
            alias: String::new(),
            keys: String::new(),
            item: Item::Action(title.into()),
        }
    }

    #[test]
    fn fuzzy_matching_prefers_word_starts() {
        assert!(
            score("sr", "Split right").unwrap() > score("sr", "Scroll to prev prompt").unwrap_or(0)
        );
        assert!(score("xyz", "Split right").is_none());
        assert!(score("", "anything").is_some());
        let mut p = Palette::new(
            vec![
                entry("Scroll page up"),
                entry("Split right"),
                entry("Split down"),
            ],
            "no matching commands".into(),
        );
        p.input("spd");
        assert_eq!(p.chosen(), Some(&Item::Action("Split down".into())));
        p.clear();
        assert_eq!(p.chosen(), Some(&Item::Action("Scroll page up".into())));
        p.move_by(-1);
        assert_eq!(p.chosen(), Some(&Item::Action("Split down".into())));
        p.input("zzz");
        assert_eq!(p.chosen(), None);
    }

    #[test]
    fn japanese_queries_match_and_erase_by_character() {
        let mut p = Palette::new(
            vec![
                entry("右に分割"),
                entry("下に分割"),
                entry("ファイルツリー"),
            ],
            "一致するコマンドはありません".into(),
        );
        p.input("下分");
        assert_eq!(p.chosen(), Some(&Item::Action("下に分割".into())));
        p.backspace();
        assert_eq!(p.query, "下");
        assert_eq!(p.chosen(), Some(&Item::Action("下に分割".into())));
        p.input("ツ");
        assert_eq!(p.chosen(), None);
        let view = p.view(40, 3, &Colors::default());
        assert_eq!(view.cursor_position().0, 3 + 4);
    }

    #[test]
    fn titles() {
        assert_eq!(title_of("toggle_file_tree"), "Toggle file tree");
    }
}
