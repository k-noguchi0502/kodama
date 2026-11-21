//! UI language. English is built in. Any other language comes from exactly
//! one `lang/<id>.lua`; the rest are only listed, never parsed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mlua::{Lua, Value};

use crate::config::PALETTE_ACTIONS;

/// A language the palette can offer. Built from `index.lua` and file names,
/// without reading the translation tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LangInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Language {
    pub id: String,
    pub name: String,
    strings: HashMap<String, String>,
}

/// Chrome that is not an action title. Action titles are derived from the
/// config name (`split_right` -> "Split right") so they stay in step with it.
const CHROME: &[(&str, &str)] = &[
    ("palette.no_matches", "no matching commands"),
    ("palette.go_to_tab", "Go to tab {n}"),
    ("palette.color_scheme", "Color scheme: {name}"),
    ("palette.current", "current"),
    ("palette.language", "Language: {name}"),
    ("search.no_matches", "no matches"),
    ("mode.quick_tag", "QUICK SELECT"),
    (
        "mode.quick_hint",
        "type a label to copy, {key:shift}+label to paste  {key:esc} cancel  ",
    ),
    (
        "mode.quick_hint_open",
        "label copies, {key:shift}+label pastes, {key:alt}+label opens  {key:esc} cancel  ",
    ),
    ("quick.no_open", "nothing to open there"),
    ("mode.copy", "COPY"),
    ("mode.visual", "VISUAL"),
    ("mode.visual_line", "VISUAL LINE"),
    (
        "mode.copy_hint",
        "{key:v}/{key:V} select  {key:y} copy  {key:/} search  {key:esc} exit",
    ),
    ("tab.rename", "Tab name:"),
    (
        "tab.rename_hint",
        "{key:enter} set  {key:esc} cancel  empty for automatic",
    ),
    ("tab.scrollback", "scrollback"),
    (
        "output.none",
        "no command output to copy (needs shell integration)",
    ),
    ("quit.title", "Close kodama?"),
    ("quit.shell", "its shell"),
    ("quit.panes", "all {n} panes"),
    ("quit.detail", "This ends {what}."),
    ("quit.hint", "{key:y} close   {key:n}/esc cancel"),
    ("tree.create", "new (end in / for a folder): "),
    ("tree.rename", "rename: "),
    ("tree.move", "move to: "),
    ("tree.copy", "copy to: "),
    ("tree.trash", "trash {name}? (y/N)"),
    ("tree.delete", "DELETE {name} permanently? (y/N)"),
    ("tree.empty", "(empty)"),
    ("tree.created", "created {name}"),
    ("tree.renamed", "renamed to {name}"),
    ("tree.moved", "moved to {name}"),
    ("tree.copied", "copied to {name}"),
    ("tree.trashed", "{name} moved to trash"),
    ("tree.deleted", "deleted {name}"),
    (
        "config.scheme_save_failed",
        "couldn't save the color scheme: {error}",
    ),
    (
        "config.language_save_failed",
        "couldn't save the language: {error}",
    ),
];

impl Language {
    /// English, with no language file read.
    pub fn english() -> Self {
        let mut strings = HashMap::new();
        for name in PALETTE_ACTIONS {
            strings.insert(format!("action.{name}"), action_title(name));
        }
        for (key, value) in CHROME {
            strings.insert((*key).to_string(), (*value).to_string());
        }
        Self {
            id: "en".into(),
            name: "English".into(),
            strings,
        }
    }

    pub fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.strings.get(key).map(String::as_str).unwrap_or(key)
    }

    /// `get(key)` with `{name}` placeholders replaced.
    pub fn format(&self, key: &str, vars: &[(&str, &str)]) -> String {
        let mut text = self.get(key).to_string();
        for (name, value) in vars {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }
}

/// `{key:esc}` becomes a bold `esc`, for hints in the bars.
pub fn emphasize_keys(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{key:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        let Some(end) = after.find('}') else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&format!("\x1b[1m{}\x1b[22m", &after[..end]));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn action_title(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Directories searched for language files: the config's `lang/` first
/// (so a user file replaces a built-in one), then the copy shipped with kodama.
pub fn lang_dirs(config_path: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(parent) = config_path.parent() {
        dirs.push(parent.join("lang"));
    }
    if let Some(root) = option_env!("CARGO_MANIFEST_DIR") {
        dirs.push(PathBuf::from(root).join("lang"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let lang = dir.join("lang");
        if !dirs.iter().any(|d| d == &lang) {
            dirs.push(lang);
        }
    }
    dirs
}

/// Languages on offer. Reads `index.lua` for display names and the file
/// names of other `*.lua` files. Translation files are not parsed.
pub fn catalog(dirs: &[PathBuf]) -> Vec<LangInfo> {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut order = Vec::new();
    let add =
        |order: &mut Vec<String>, names: &mut HashMap<String, String>, id: String, name: String| {
            if id.is_empty() || names.contains_key(&id) {
                return;
            }
            order.push(id.clone());
            names.insert(id, name);
        };
    for dir in dirs {
        if let Some(index) = read_index(&dir.join("index.lua")) {
            for (id, name) in index {
                add(&mut order, &mut names, id, name);
            }
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem == "index" {
                continue;
            }
            add(&mut order, &mut names, stem.to_string(), stem.to_string());
        }
    }
    order
        .into_iter()
        .filter_map(|id| names.remove(&id).map(|name| LangInfo { id, name }))
        .collect()
}

/// English overlaid with `lang/<id>.lua` from the first directory that has
/// it. A missing or broken file leaves English in place.
pub fn load(id: &str, dirs: &[PathBuf]) -> Language {
    let id = if id.is_empty() { "en" } else { id };
    let mut lang = Language::english();
    lang.id = id.to_string();
    if let Some(info) = catalog(dirs).into_iter().find(|c| c.id == id) {
        lang.name = info.name;
    }
    match find_file(id, dirs) {
        Some(path) => match read_strings(&path) {
            Ok(map) => {
                for (key, value) in map {
                    lang.strings.insert(key, value);
                }
            }
            Err(e) => log::warn!("language {id}: {e}"),
        },
        None if id != "en" => log::warn!("language {id:?} not found; using English"),
        None => {}
    }
    lang
}

fn find_file(id: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    dirs.iter()
        .map(|dir| dir.join(format!("{id}.lua")))
        .find(|path| path.is_file())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "index"
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn read_index(path: &Path) -> Option<Vec<(String, String)>> {
    if !path.is_file() {
        return None;
    }
    match read_strings(path) {
        Ok(map) => Some(map.into_iter().collect()),
        Err(e) => {
            log::warn!("{}: {e}", path.display());
            None
        }
    }
}

fn read_strings(path: &Path) -> Result<HashMap<String, String>, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let lua = Lua::new();
    let value: Value = lua
        .load(&source)
        .set_name(format!("@{}", path.display()))
        .eval()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let Value::Table(table) = value else {
        return Err(format!("{}: must return a table", path.display()));
    };
    let mut map = HashMap::new();
    for pair in table.pairs::<String, Value>() {
        let (key, value) = pair.map_err(|e| format!("{}: {e}", path.display()))?;
        let Value::String(text) = value else {
            return Err(format!("{}: `{key}` must be a string", path.display()));
        };
        let text = text
            .to_str()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .to_string();
        map.insert(key, text);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_file_matches_the_builtin_strings() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lang/en.lua");
        let file = read_strings(&path).unwrap();
        let builtin = Language::english();
        for (key, value) in &builtin.strings {
            assert_eq!(
                file.get(key).map(String::as_str),
                Some(value.as_str()),
                "{key}"
            );
        }
        assert_eq!(file.len(), builtin.strings.len());
    }

    #[test]
    fn japanese_file_has_every_english_key() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lang");
        let en = read_strings(&root.join("en.lua")).unwrap();
        let ja = read_strings(&root.join("ja.lua")).unwrap();
        for key in en.keys() {
            assert!(ja.contains_key(key), "ja.lua missing {key}");
        }
        assert_ne!(ja.get("action.copy").map(String::as_str), Some("Copy"));
    }

    #[test]
    fn catalog_lists_files_it_does_not_parse() {
        let dir = std::env::temp_dir().join(format!("kodama-lang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.lua"), "return { en = \"English\" }\n").unwrap();
        std::fs::write(
            dir.join("en.lua"),
            "return { [\"palette.current\"] = \"now\" }\n",
        )
        .unwrap();
        // Invalid Lua. Listing it must not execute it.
        std::fs::write(dir.join("zz.lua"), "error('loaded zz')\n").unwrap();
        let dirs = vec![dir.clone()];
        let listed = catalog(&dirs);
        assert!(listed.iter().any(|c| c.id == "en" && c.name == "English"));
        assert!(listed.iter().any(|c| c.id == "zz" && c.name == "zz"));
        let lang = load("en", &dirs);
        assert_eq!(lang.get("palette.current"), "now");
        assert_eq!(lang.get("palette.no_matches"), "no matching commands");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholders_and_bold_keys() {
        let lang = Language::english();
        assert_eq!(
            lang.format("palette.go_to_tab", &[("n", "3")]),
            "Go to tab 3"
        );
        assert_eq!(
            emphasize_keys("{key:esc} cancel"),
            "\x1b[1mesc\x1b[22m cancel"
        );
    }
}
