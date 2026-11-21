//! Grapheme clusters that don't fit in one `char` (emoji sequences, flags,
//! combining marks). They are interned process-wide and a cell stores the
//! cluster's code, taken from the range above the last Unicode scalar.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// First code used for clusters; everything below is a plain `char`.
pub const BASE: u32 = 0x11_0000;
/// Cell chars are 21 bits wide.
const LIMIT: u32 = 1 << 21;

#[derive(Default)]
struct Table {
    texts: Vec<&'static str>,
    codes: HashMap<&'static str, u32>,
}

fn table() -> &'static RwLock<Table> {
    static TABLE: OnceLock<RwLock<Table>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

/// The code for `text`, or `None` once the table is full. Texts are kept
/// for the life of the process; distinct clusters are few in practice.
pub fn intern(text: &str) -> Option<u32> {
    if let Some(&code) = table().read().unwrap().codes.get(text) {
        return Some(code);
    }
    let mut t = table().write().unwrap();
    if let Some(&code) = t.codes.get(text) {
        return Some(code);
    }
    let code = BASE + t.texts.len() as u32;
    if code >= LIMIT {
        return None;
    }
    let text: &'static str = Box::leak(text.into());
    t.texts.push(text);
    t.codes.insert(text, code);
    Some(code)
}

/// The text of cluster `code` (at least [`BASE`]).
pub fn get(code: u32) -> Option<&'static str> {
    let i = code.checked_sub(BASE)? as usize;
    table().read().unwrap().texts.get(i).copied()
}

pub const ZWJ: char = '\u{200d}';
pub const VS16: char = '\u{fe0f}';

pub fn is_regional_indicator(c: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
}

pub fn is_skin_tone(c: char) -> bool {
    ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
}

/// Whether `c` continues the cluster `text` instead of starting a new cell.
pub fn extends(text: &str, c: char) -> bool {
    use unicode_width::UnicodeWidthChar;
    let mut chars = text.chars();
    let first = chars.next();
    let last = text.chars().next_back();
    // Zero-width: combining marks, variation selectors, ZWJ, emoji tags.
    c.width() == Some(0)
        || last == Some(ZWJ)
        || (is_skin_tone(c) && first.is_some_and(|f| f.width() == Some(2)))
        // A flag is a pair of regional indicators.
        || (is_regional_indicator(c)
            && first.is_some_and(is_regional_indicator)
            && chars.next().is_none())
}

/// Whether a cluster is drawn as a colour emoji.
pub fn is_emoji(text: &str) -> bool {
    text.chars()
        .any(|c| c == VS16 || c == ZWJ || is_regional_indicator(c) || is_skin_tone(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable() {
        let a = intern("👍🏽").unwrap();
        assert!(a >= BASE);
        assert_eq!(intern("👍🏽"), Some(a));
        assert_eq!(get(a), Some("👍🏽"));
        assert_ne!(intern("e\u{301}"), Some(a));
    }

    #[test]
    fn sequences_extend() {
        assert!(extends("❤", VS16));
        assert!(extends("👨", ZWJ));
        assert!(extends("👨\u{200d}", '👩'));
        assert!(extends("👍", '🏽'));
        assert!(extends("🇯", '🇵'));
        assert!(!extends("🇯🇵", '🇺'));
        assert!(extends("e", '\u{301}'));
        assert!(!extends("a", 'b'));
        assert!(!extends("a", '🏽'));
    }
}
