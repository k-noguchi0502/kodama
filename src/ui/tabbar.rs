//! The tab bar's line: each tab's label and the caps or separators of the
//! chosen style, after kitty's `tab_bar_style`.

use crate::config::{Colors, Rgb, TabStyle};
use crate::ui::tree;

const POWERLINE: char = '\u{e0b0}';
const POWERLINE_THIN: char = '\u{e0b1}';
const ROUND_LEFT: char = '\u{e0b6}';
const ROUND_RIGHT: char = '\u{e0b4}';
const SLANT_LEFT: char = '\u{e0ba}';
const SLANT_RIGHT: char = '\u{e0bc}';

fn sgr([r, g, b]: Rgb, [fr, fg, fb]: Rgb) -> String {
    format!("\x1b[48;2;{r};{g};{b}m\x1b[38;2;{fr};{fg};{fb}m")
}

/// Columns a tab's caps and gaps take besides its label.
fn chrome(style: TabStyle) -> usize {
    match style {
        TabStyle::Plain => 0,
        TabStyle::Powerline => 1,
        TabStyle::Slant => 2,
        TabStyle::Round => 3,
    }
}

/// The bar's text for `titles` with tab `active` current, in `cols`
/// columns, and each drawn tab's column span (for clicks). The tabs share
/// the whole width, side by side, unless `max_width` (when not 0) caps
/// them. Tabs that don't fit are left out.
pub fn render(
    titles: &[String],
    active: usize,
    cols: usize,
    style: TabStyle,
    max_width: usize,
    colors: &Colors,
) -> (String, Vec<(usize, usize)>) {
    let extra = chrome(style);
    let min = 6 + extra;
    let shown = titles.len().min(cols / min).max(1).min(titles.len());
    let width_of = |i: usize| {
        let even = cols / shown.max(1) + usize::from(i < cols % shown.max(1));
        if max_width > 0 {
            even.min(max_width.max(min))
        } else {
            even
        }
    };
    let labels: Vec<String> = titles
        .iter()
        .take(shown)
        .enumerate()
        .map(|(i, title)| {
            let room = width_of(i).saturating_sub(extra);
            let label = tree::fit(&format!(" {} {title} ", i + 1), room);
            let spare = room.saturating_sub(tree::width(&label));
            let left = spare / 2;
            format!("{}{label}{}", " ".repeat(left), " ".repeat(spare - left))
        })
        .collect();

    let bg_of = |i: usize| {
        if i == active {
            colors.tab_active
        } else {
            colors.tab_inactive
        }
    };
    let fg_of = |i: usize| {
        if i == active {
            colors.background
        } else {
            colors.foreground
        }
    };
    let mut out = String::new();
    let mut spans = Vec::new();
    let mut col = 0;
    for (i, label) in labels.iter().take(shown).enumerate() {
        let (bg, fg) = (bg_of(i), fg_of(i));
        let bold = if i == active { "\x1b[1m" } else { "\x1b[22m" };
        let body = format!("{}{bold}{label}\x1b[22m", sgr(bg, fg));
        match style {
            TabStyle::Plain => out.push_str(&body),
            TabStyle::Powerline => {
                out.push_str(&body);
                let next = if i + 1 < shown {
                    bg_of(i + 1)
                } else {
                    colors.tab_bar
                };
                if next == bg {
                    out.push_str(&format!("{}{POWERLINE_THIN}", sgr(bg, colors.split)));
                } else {
                    out.push_str(&format!("{}{POWERLINE}", sgr(next, bg)));
                }
            }
            TabStyle::Slant => {
                out.push_str(&format!("{}{SLANT_LEFT}", sgr(colors.tab_bar, bg)));
                out.push_str(&body);
                out.push_str(&format!("{}{SLANT_RIGHT}", sgr(colors.tab_bar, bg)));
            }
            TabStyle::Round => {
                out.push_str(&format!("{}{ROUND_LEFT}", sgr(colors.tab_bar, bg)));
                out.push_str(&body);
                out.push_str(&format!("{}{ROUND_RIGHT} ", sgr(colors.tab_bar, bg)));
            }
        }
        let w = tree::width(label) + extra;
        spans.push((col, col + w));
        col += w;
    }
    out.push_str(&sgr(colors.tab_bar, colors.foreground));
    (out, spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("tab{i}")).collect()
    }

    #[test]
    fn spans_cover_each_tab_and_its_caps() {
        let c = Colors::default();
        let (_, power) = render(&titles(2), 0, 80, TabStyle::Powerline, 32, &c);
        assert_eq!(power, vec![(0, 32), (32, 64)]);
        let (text, round) = render(&titles(2), 1, 80, TabStyle::Round, 32, &c);
        assert_eq!(round, vec![(0, 32), (32, 64)]);
        assert_eq!(text.matches(ROUND_LEFT).count(), 2);
    }

    #[test]
    fn tabs_share_the_whole_width_side_by_side() {
        let c = Colors::default();
        let (_, spans) = render(&titles(2), 0, 80, TabStyle::Plain, 0, &c);
        assert_eq!(spans, vec![(0, 40), (40, 80)]);
        // The remainder goes to the first tabs.
        let (text, spans) = render(&titles(3), 0, 80, TabStyle::Plain, 0, &c);
        assert_eq!(spans, vec![(0, 27), (27, 54), (54, 80)]);
        // Labels are centred: 27 columns around " 1 tab0 ".
        assert!(text.contains(&format!("{} 1 tab0 {}", " ".repeat(9), " ".repeat(10))));
    }

    #[test]
    fn powerline_separates_tabs_of_the_same_colour_with_a_thin_line() {
        let c = Colors::default();
        let (text, _) = render(&titles(3), 0, 80, TabStyle::Powerline, 32, &c);
        // Current -> other: arrow; other -> other: thin; last -> bar: arrow.
        assert_eq!(text.matches(POWERLINE).count(), 2);
        assert_eq!(text.matches(POWERLINE_THIN).count(), 1);
    }

    #[test]
    fn tabs_shrink_to_the_width_and_drop_when_out_of_room() {
        let c = Colors::default();
        let long = vec!["a very long title for a tab".to_string(); 2];
        let (_, spans) = render(&long, 0, 80, TabStyle::Plain, 12, &c);
        assert_eq!(spans, vec![(0, 12), (12, 24)]);
        let (_, spans) = render(&titles(5), 0, 20, TabStyle::Plain, 0, &c);
        assert_eq!(spans, vec![(0, 7), (7, 14), (14, 20)]);
    }
}
