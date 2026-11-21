//! Quick select: everything on screen that looks worth copying (URLs, file
//! paths, git hashes, IP addresses, long numbers) gets a short label, and
//! typing the label copies it.

use crate::layout::PaneId;
use crate::terminal::Point;

/// Label letters, home row first.
const ALPHABET: &str = "asdfqwerzxcvjklmiuopghtybn";

pub struct Hit {
    pub label: String,
    pub text: String,
    pub start: Point,
    pub end: Point,
}

pub struct QuickSelect {
    pub pane: PaneId,
    pub hits: Vec<Hit>,
    /// Label letters typed so far.
    pub typed: String,
}

/// What typing a letter did.
#[derive(Debug, PartialEq, Eq)]
pub enum Pick {
    /// More letters are needed (or the typed ones matched nothing).
    Pending,
    /// A label was completed: its text, and whether to paste it too.
    Done(String, bool),
}

/// `n` distinct labels, none a prefix of another: single letters while
/// they last, otherwise two letters each.
pub fn labels(n: usize) -> Vec<String> {
    let letters: Vec<char> = ALPHABET.chars().collect();
    if n <= letters.len() {
        return letters.iter().take(n).map(|c| c.to_string()).collect();
    }
    letters
        .iter()
        .flat_map(|a| letters.iter().map(move |b| format!("{a}{b}")))
        .take(n)
        .collect()
}

impl QuickSelect {
    /// Labels `targets` (text and span, in screen order): the ones nearest
    /// the bottom get labels first, and repeated text shares a label.
    pub fn new(pane: PaneId, targets: Vec<(String, Point, Point)>) -> Self {
        let mut distinct: Vec<&str> = Vec::new();
        for (text, ..) in targets.iter().rev() {
            if !distinct.contains(&text.as_str()) {
                distinct.push(text);
            }
        }
        let names = labels(distinct.len());
        let label_of = |text: &str| {
            let i = distinct.iter().position(|t| *t == text).unwrap_or(0);
            names.get(i).cloned().unwrap_or_default()
        };
        let hits = targets
            .iter()
            .map(|(text, start, end)| Hit {
                label: label_of(text),
                text: text.clone(),
                start: *start,
                end: *end,
            })
            .collect();
        Self {
            pane,
            hits,
            typed: String::new(),
        }
    }

    /// Types one label letter; an uppercase one asks to paste as well.
    pub fn input(&mut self, c: char) -> Pick {
        let paste = c.is_uppercase();
        self.typed.extend(c.to_lowercase());
        if let Some(hit) = self.hits.iter().find(|h| h.label == self.typed) {
            return Pick::Done(hit.text.clone(), paste);
        }
        if !self.hits.iter().any(|h| h.label.starts_with(&self.typed)) {
            self.typed.clear();
        }
        Pick::Pending
    }
}

fn is_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | ';'
                | '|'
                | '='
        )
        || c == '\0'
}

fn is_hash(s: &str) -> bool {
    (7..=64).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_hexdigit())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_ipv4(s: &str) -> bool {
    let (addr, port) = s.split_once(':').unwrap_or((s, "0"));
    let parts: Vec<&str> = addr.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
        && port.parse::<u16>().is_ok()
}

fn is_path(s: &str) -> bool {
    s.contains('/')
        && s.chars().any(|c| c.is_alphanumeric())
        && !s.contains("://")
        && s.chars()
            .all(|c| c.is_alphanumeric() || "/._-~+:@%#".contains(c) || !c.is_ascii())
}

fn is_number(s: &str) -> bool {
    s.len() >= 4 && s.chars().all(|c| c.is_ascii_digit())
}

/// Spans `[start, end)` of `text` worth labelling, other than URLs.
pub fn find(text: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if is_delimiter(text[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < text.len() && !is_delimiter(text[i]) {
            i += 1;
        }
        // Sentence punctuation after a word isn't part of it.
        let mut end = i;
        while end > start && matches!(text[end - 1], '.' | ':' | '!' | '?') {
            end -= 1;
        }
        let token: String = text[start..end].iter().collect();
        if is_path(&token)
            || is_hash(&token)
            || is_uuid(&token)
            || is_ipv4(&token)
            || is_number(&token)
        {
            out.push((start, end));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(s: &str) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        find(&chars)
            .into_iter()
            .map(|(a, b)| chars[a..b].iter().collect())
            .collect()
    }

    #[test]
    fn patterns() {
        assert_eq!(
            found("edit src/app.rs:120:5, then ~/notes.md. commit 3f9a2c1d (HEAD)"),
            ["src/app.rs:120:5", "~/notes.md", "3f9a2c1d"]
        );
        assert_eq!(
            found("ping 192.168.1.20:8080 pid=12345 id 550e8400-e29b-41d4-a716-446655440000 x 42"),
            [
                "192.168.1.20:8080",
                "12345",
                "550e8400-e29b-41d4-a716-446655440000"
            ]
        );
        assert!(found("hello world, deadbeef and / alone").is_empty());
    }

    #[test]
    fn labels_are_prefix_free() {
        assert_eq!(labels(3), ["a", "s", "d"]);
        let many = labels(30);
        assert_eq!(many.len(), 30);
        assert!(many.iter().all(|l| l.len() == 2));
        for (i, a) in many.iter().enumerate() {
            assert!(many[i + 1..].iter().all(|b| a != b));
        }
    }

    #[test]
    fn typing_a_label_picks_it() {
        let p = |col| Point { line: 0, col };
        let targets = vec![
            ("one".to_string(), p(0), p(2)),
            ("two".to_string(), p(4), p(6)),
            ("one".to_string(), p(8), p(10)),
        ];
        let mut q = QuickSelect::new(1, targets);
        // Bottom-most first; the repeated text shares its label.
        assert_eq!(q.hits[2].label, "a");
        assert_eq!(q.hits[0].label, "a");
        assert_eq!(q.hits[1].label, "s");
        assert_eq!(q.input('S'), Pick::Done("two".into(), true));
        assert_eq!(q.input('z'), Pick::Pending);
        assert!(q.typed.is_empty());
    }
}
