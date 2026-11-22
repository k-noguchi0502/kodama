//! The saved window: tabs, splits, each pane's directory and the text that
//! was on its screen, beside the config file. A missing file is not an error.
//! Restored text is shown again; the commands are not run a second time.

use std::collections::HashMap;
use std::path::Path;

use crate::layout::{Axis, Shape};

/// One tab in a saved window.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedTab {
    pub title: Option<String>,
    pub active: u32,
    pub zoomed: bool,
    pub shape: Shape,
    /// Working directory of each leaf, when one was known.
    pub dirs: HashMap<u32, String>,
    /// Commands and output that were on each leaf's primary screen.
    pub screens: HashMap<u32, String>,
}

/// Every tab, and which one was current.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub current: usize,
    pub tabs: Vec<SavedTab>,
    /// The file tree was open.
    pub tree: bool,
    /// The tree's width in columns, when one was saved.
    pub tree_width: Option<usize>,
}

const HEADER: &str = "kodama-session 2";
/// Layout only, from before pane text was saved.
const HEADER_V1: &str = "kodama-session 1";
/// Written before the rename from trex.
const LEGACY_HEADER: &str = "trex-session 1";

/// `config` is the config file. The session sits next to it.
pub fn path_for(config: &Path) -> Option<std::path::PathBuf> {
    Some(config.parent()?.join("session"))
}

pub fn load(config: &Path) -> Result<Option<Session>, String> {
    let Some(path) = path_for(config) else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    decode(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn save(config: &Path, session: &Session) -> Result<(), String> {
    let Some(path) = path_for(config) else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = encode(session);
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn encode(session: &Session) -> String {
    let mut out = format!("{HEADER}\ncurrent {}\n", session.current);
    out.push_str(&format!("tree {}\n", u8::from(session.tree)));
    if let Some(width) = session.tree_width {
        out.push_str(&format!("tree_width {width}\n"));
    }
    for tab in &session.tabs {
        out.push_str("tab\n");
        out.push_str(&format!(
            "active {}\nzoomed {}\n",
            tab.active,
            u8::from(tab.zoomed)
        ));
        match &tab.title {
            // `-` on its own means "no title", so a title of `-` is escaped.
            Some(title) if title == "-" => out.push_str("title \\-\n"),
            Some(title) => out.push_str(&format!("title {}\n", escape(title))),
            None => out.push_str("title -\n"),
        }
        write_shape(&mut out, &tab.shape, &tab.dirs, &tab.screens);
    }
    out
}

pub fn decode(text: &str) -> Result<Session, String> {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if !matches!(
        lines.first().copied(),
        Some(HEADER | HEADER_V1 | LEGACY_HEADER)
    ) {
        return Err("not a kodama session".into());
    }
    lines.remove(0);
    let current = lines
        .first()
        .and_then(|l| l.strip_prefix("current "))
        .ok_or("session has no current tab")?
        .parse::<usize>()
        .map_err(|_| "bad current tab".to_string())?;
    lines.remove(0);
    let mut tree = false;
    let mut tree_width = None;
    while let Some(line) = lines.first().copied() {
        if let Some(value) = line.strip_prefix("tree_width ") {
            tree_width = value.parse().ok();
            lines.remove(0);
        } else if let Some(value) = line.strip_prefix("tree ") {
            tree = value != "0";
            lines.remove(0);
        } else {
            break;
        }
    }
    let mut tabs = Vec::new();
    while lines.first() == Some(&"tab") {
        lines.remove(0);
        tabs.push(decode_tab(&mut lines)?);
    }
    if tabs.is_empty() {
        return Err("session has no tabs".into());
    }
    if current >= tabs.len() {
        return Err("current tab is out of range".into());
    }
    Ok(Session {
        current,
        tabs,
        tree,
        tree_width,
    })
}

fn decode_tab(lines: &mut Vec<&str>) -> Result<SavedTab, String> {
    let active = take_prefix(lines, "active ")?
        .parse::<u32>()
        .map_err(|_| "bad active pane".to_string())?;
    let zoomed = take_prefix(lines, "zoomed ")? == "1";
    let title = match take_prefix(lines, "title ")? {
        "-" => None,
        text => Some(unescape(text)),
    };
    let mut dirs = HashMap::new();
    let mut screens = HashMap::new();
    let shape = decode_shape(lines, &mut dirs, &mut screens)?;
    Ok(SavedTab {
        title,
        active,
        zoomed,
        shape,
        dirs,
        screens,
    })
}

fn decode_shape(
    lines: &mut Vec<&str>,
    dirs: &mut HashMap<u32, String>,
    screens: &mut HashMap<u32, String>,
) -> Result<Shape, String> {
    let line = lines
        .first()
        .copied()
        .ok_or("session ended inside a split")?;
    lines.remove(0);
    if let Some(rest) = line.strip_prefix("H ").or_else(|| line.strip_prefix("V ")) {
        let axis = if line.starts_with('H') {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        let ratio = rest
            .parse::<f32>()
            .map_err(|_| format!("bad split ratio `{rest}`"))?;
        let first = decode_shape(lines, dirs, screens)?;
        let second = decode_shape(lines, dirs, screens)?;
        return Ok(Shape::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        });
    }
    let rest = line
        .strip_prefix("leaf ")
        .ok_or_else(|| format!("bad session line `{line}`"))?;
    let (id, path) = rest
        .split_once(' ')
        .ok_or_else(|| format!("bad pane `{rest}`"))?;
    let id = id
        .parse::<u32>()
        .map_err(|_| format!("bad pane id `{id}`"))?;
    let path = unescape(path);
    if path != "-" {
        dirs.insert(id, path);
    }
    if let Some(line) = lines.first().copied()
        && let Some(data) = line.strip_prefix("scrollback ")
    {
        lines.remove(0);
        // A bad payload keeps the layout and drops just this pane's text.
        if let Some(bytes) = crate::terminal::graphics::base64_decode(data.as_bytes())
            && let Ok(text) = String::from_utf8(bytes)
            && !text.is_empty()
        {
            screens.insert(id, text);
        }
    }
    Ok(Shape::Leaf(id))
}

fn take_prefix<'a>(lines: &mut Vec<&'a str>, prefix: &str) -> Result<&'a str, String> {
    let line = lines
        .first()
        .copied()
        .ok_or_else(|| format!("missing {prefix}"))?;
    let rest = line
        .strip_prefix(prefix)
        .ok_or_else(|| format!("expected {prefix}, got `{line}`"))?;
    lines.remove(0);
    Ok(rest)
}

fn write_shape(
    out: &mut String,
    shape: &Shape,
    dirs: &HashMap<u32, String>,
    screens: &HashMap<u32, String>,
) {
    match shape {
        Shape::Leaf(id) => {
            let dir = dirs.get(id).map(String::as_str).unwrap_or("-");
            out.push_str(&format!("leaf {id} {}\n", escape(dir)));
            if let Some(text) = screens.get(id).filter(|t| !t.is_empty()) {
                let encoded = crate::terminal::graphics::base64_encode(text.as_bytes());
                out.push_str(&format!("scrollback {encoded}\n"));
            }
        }
        Shape::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let axis = match axis {
                Axis::Horizontal => 'H',
                Axis::Vertical => 'V',
            };
            out.push_str(&format!("{axis} {ratio:.6}\n"));
            write_shape(out, first, dirs, screens);
            write_shape(out, second, dirs, screens);
        }
    }
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Session {
        let mut dirs = HashMap::new();
        dirs.insert(1, "/tmp/left".into());
        dirs.insert(2, "/tmp/right dir".into());
        let mut screens = HashMap::new();
        screens.insert(1, "$ ls\nfile\nこんにちは".into());
        Session {
            current: 0,
            tree: true,
            tree_width: Some(24),
            tabs: vec![SavedTab {
                title: Some("work".into()),
                active: 2,
                zoomed: false,
                shape: Shape::Split {
                    axis: Axis::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Shape::Leaf(1)),
                    second: Box::new(Shape::Leaf(2)),
                },
                dirs,
                screens,
            }],
        }
    }

    #[test]
    fn a_session_roundtrips() {
        let session = sample();
        let again = decode(&encode(&session)).unwrap();
        assert_eq!(again, session);
    }

    #[test]
    fn a_title_of_dash_roundtrips() {
        let mut session = sample();
        session.tabs[0].title = Some("-".into());
        let again = decode(&encode(&session)).unwrap();
        assert_eq!(again.tabs[0].title.as_deref(), Some("-"));
    }

    #[test]
    fn a_session_without_a_tree_line_stays_closed() {
        let text = encode(&sample()).replace("tree 1\ntree_width 24\n", "");
        let again = decode(&text).unwrap();
        assert!(!again.tree);
        assert_eq!(again.tree_width, None);
        assert_eq!(again.tabs.len(), 1);
    }

    #[test]
    fn a_session_saved_by_trex_still_loads() {
        let text = encode(&sample()).replace(HEADER, LEGACY_HEADER);
        assert_eq!(decode(&text).unwrap().tabs.len(), 1);
    }

    #[test]
    fn a_v1_session_loads_without_pane_text() {
        let text = "\
kodama-session 1
current 0
tree 0
tab
active 1
zoomed 0
title -
leaf 1 /tmp
";
        let session = decode(text).unwrap();
        assert!(session.tabs[0].screens.is_empty());
        assert_eq!(session.tabs[0].dirs.get(&1).unwrap(), "/tmp");
    }

    #[test]
    fn a_broken_scrollback_keeps_the_layout() {
        let text = "\
kodama-session 2
current 0
tree 0
tab
active 1
zoomed 0
title -
leaf 1 /tmp
scrollback ???
";
        let session = decode(text).unwrap();
        assert!(session.tabs[0].screens.is_empty());
        assert_eq!(session.tabs[0].dirs.get(&1).unwrap(), "/tmp");
    }
}
