//! Lua configuration runtime.
//!
//! `config.lua` is an ordinary Lua script that returns a table. Being code,
//! it can branch on the host, compute values, `require` sibling modules and
//! bind keys to Lua functions that drive the terminal through a window handle.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, Table, Value, Variadic};

use crate::config::schemes;
use crate::config::{
    Action, Colors, Config, KeyBinding, default_keys, mods, normalize_key, parse_mods, to_hex,
};

pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("kodama");
    // Kodama used to be called trex; keep reading that directory until it's moved.
    let legacy = base.join("trex");
    if !dir.exists() && legacy.is_dir() {
        return legacy.join("config.lua");
    }
    dir.join("config.lua")
}

/// A loaded configuration plus the Lua state that owns its callbacks.
pub struct LoadedConfig {
    pub config: Config,
    pub lua: Lua,
    /// Problems that didn't stop the config from loading (unknown settings).
    pub warnings: Vec<String>,
}

/// Key bindings file read alongside the config.
pub fn keybind_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("keybind.lua")
}

/// Colors file read alongside the config.
pub fn theme_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("theme.lua")
}

/// Makes `name` the scheme theme.lua picks, editing the file in place so
/// the rest of it (comments, colors, own schemes) stays as written:
/// the first `color_scheme = "..."` line gets the new name, a theme that
/// returns just a name gets the new one, and otherwise the setting is added
/// at the top of the returned table. A missing theme.lua is created.
pub fn save_color_scheme(config_path: &Path, name: &str) -> Result<PathBuf, String> {
    let path = theme_path(config_path);
    let quoted = format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            String::from("-- kodama colors (see examples/theme.lua)\nreturn {\n}\n")
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let updated = set_field(&source, "color_scheme", &quoted, true)
        .ok_or_else(|| format!("{}: no `return {{` to add color_scheme to", path.display()))?;
    std::fs::write(&path, updated).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Writes `language = "..."` into config.lua, leaving the rest of the file
/// (comments, other settings) as written.
pub fn save_language(config_path: &Path, id: &str) -> Result<(), String> {
    if id.is_empty()
        || id == "index"
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("invalid language id {id:?}"));
    }
    let quoted = format!("\"{}\"", id.replace('\\', "\\\\").replace('"', "\\\""));
    let source = std::fs::read_to_string(config_path)
        .map_err(|e| format!("{}: {e}", config_path.display()))?;
    let updated = set_field(&source, "language", &quoted, false).ok_or_else(|| {
        format!(
            "{}: no `return {{` to add language to",
            config_path.display()
        )
    })?;
    std::fs::write(config_path, updated).map_err(|e| format!("{}: {e}", config_path.display()))?;
    Ok(())
}

/// Sets `field = quoted` in a Lua config. `replace_bare_return` also rewrites
/// a file whose whole value is `return "name"` (how a theme can be written).
fn set_field(source: &str, field: &str, quoted: &str, replace_bare_return: bool) -> Option<String> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    fn code(line: &str) -> Option<&str> {
        let t = line.trim_start();
        (!t.starts_with("--")).then_some(t)
    }
    // The string literal right after `prefix` (and an `=` for settings).
    let literal_after = |rest: &str, needs_eq: bool| -> Option<(usize, usize)> {
        let mut rest_start = rest.len() - rest.trim_start().len();
        if needs_eq {
            let after = &rest[rest_start..];
            let after = after.strip_prefix('=')?;
            rest_start = rest.len() - after.trim_start().len();
        }
        let s = &rest[rest_start..];
        let q = s.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let mut escaped = false;
        for (i, c) in s.char_indices().skip(1) {
            match c {
                '\\' if !escaped => escaped = true,
                c if c == q && !escaped => return Some((rest_start, rest_start + i + 1)),
                _ => escaped = false,
            }
        }
        None
    };
    let replace = |i: usize, prefix_end: usize, (a, b): (usize, usize)| {
        let line = lines[i];
        let at = prefix_end;
        let mut out: String = lines[..i].concat();
        out.push_str(&line[..at + a]);
        out.push_str(quoted);
        out.push_str(&line[at + b..]);
        out.push_str(&lines[i + 1..].concat());
        out
    };
    for (i, line) in lines.iter().enumerate() {
        let Some(t) = code(line) else { continue };
        if let Some(rest) = t.strip_prefix(field)
            && let Some(span) = literal_after(rest, true)
        {
            let prefix_end = line.len() - rest.len();
            return Some(replace(i, prefix_end, span));
        }
    }
    if replace_bare_return {
        for (i, line) in lines.iter().enumerate() {
            let Some(t) = code(line) else { continue };
            if let Some(rest) = t.strip_prefix("return")
                && let Some(span) = literal_after(rest, false)
            {
                let prefix_end = line.len() - rest.len();
                return Some(replace(i, prefix_end, span));
            }
        }
    }
    let (i, line) = lines.iter().enumerate().find(|(_, line)| {
        code(line).is_some_and(|t| {
            t.strip_prefix("return")
                .is_some_and(|rest| rest.trim_start().starts_with('{'))
        })
    })?;
    let brace = line.find('{')? + 1;
    let mut out: String = lines[..i].concat();
    out.push_str(&line[..brace]);
    out.push_str(&format!("\n  {field} = {quoted},"));
    let rest = &line[brace..];
    if !rest.starts_with('\n') && !rest.trim().is_empty() {
        out.push('\n');
        out.push_str(rest.trim_start());
    } else {
        out.push_str(rest);
    }
    out.push_str(&lines[i + 1..].concat());
    Some(out)
}

/// Runs a Lua file and returns its result; a missing file gives `nil`.
fn eval_file(lua: &Lua, path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(Value::Nil);
    }
    let source = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    lua.load(&source)
        .set_name(format!("@{}", path.display()))
        .eval()
        .map_err(|e| e.to_string())
}

/// Evaluate the config file and `theme.lua` and `keybind.lua` next to it.
/// Missing files yield the defaults. The theme's colors go over the
/// config's. Bindings from `keybind.lua` take precedence over the config's
/// `keys`, which take precedence over the built-in ones.
pub fn load(path: &Path) -> Result<LoadedConfig, String> {
    let lua = Lua::new();
    let mut warnings = Vec::new();
    install_api(&lua, path).map_err(|e| e.to_string())?;

    let table = match eval_file(&lua, path)? {
        Value::Table(table) => table,
        Value::Nil => lua.create_table().map_err(|e| e.to_string())?,
        other => {
            return Err(format!(
                "config must return a table, got {}",
                other.type_name()
            ));
        }
    };
    let config_keys: Value = table.get("keys").map_err(|e| e.to_string())?;
    table.set("keys", Value::Nil).map_err(|e| e.to_string())?;

    let theme_path = theme_path(path);
    let in_theme = |msg: String| format!("{}: {msg}", theme_path.display());
    let theme = eval_file(&lua, &theme_path)?;
    let custom = apply_theme(&lua, &table, theme, &mut warnings)
        .and_then(|schemes| {
            schemes
                .into_iter()
                .map(|(name, t)| {
                    let colors =
                        scheme_colors(&lua, &t).map_err(|e| format!("schemes[{name:?}]: {e}"))?;
                    Ok((name, colors))
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .map_err(in_theme)?;
    apply_scheme(&lua, &table, &custom, &mut warnings).map_err(|e| e.to_string())?;
    let mut config = parse_config(&lua, &table, &mut warnings)?;
    config.custom_schemes = custom;

    let kb_path = keybind_path(path);
    let in_keybind = |msg: String| format!("{}: {msg}", kb_path.display());
    let keybind = match eval_file(&lua, &kb_path)? {
        // Either a plain list of bindings or `{ keys = {...}, disable_default_keys = ..., super = ... }`;
        // a list may also carry `disable_default_keys` and `super`.
        Value::Table(table) => {
            let disable: Option<bool> = table
                .get("disable_default_keys")
                .map_err(|e| in_keybind(e.to_string()))?;
            if disable == Some(true) {
                config.disable_default_keys = true;
            }
            let super_key: Option<String> =
                table.get("super").map_err(|e| in_keybind(e.to_string()))?;
            if let Some(super_key) = super_key {
                config.super_key = super_key;
            }
            match table
                .get::<Value>("keys")
                .map_err(|e| in_keybind(e.to_string()))?
            {
                Value::Nil => Value::Table(table),
                keys => keys,
            }
        }
        Value::Nil => Value::Nil,
        other => {
            return Err(in_keybind(format!(
                "must return a list of bindings, got {}",
                other.type_name()
            )));
        }
    };
    let sup = parse_mods(&config.super_key, mods::CTRL).map_err(|e| format!("super: {e}"))?;
    if sup == 0 {
        return Err("super: needs at least one modifier".into());
    }
    let mut keys = parse_keys(keybind, sup).map_err(in_keybind)?;
    keys.extend(parse_keys(config_keys, sup)?);
    if !config.disable_default_keys {
        keys.extend(default_keys(sup));
    }
    config.keys = keys;
    Ok(LoadedConfig {
        config,
        lua,
        warnings,
    })
}

/// The `colors` table for `colors` as the config writes it.
pub fn colors_table(lua: &Lua, colors: &Colors) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let (ansi, brights) = (lua.create_table()?, lua.create_table()?);
    for (name, value) in colors.named() {
        if let Some(i) = name.strip_prefix("ansi") {
            ansi.set(i.parse::<usize>().unwrap_or(1), to_hex(value))?;
        } else if let Some(i) = name.strip_prefix("brights") {
            brights.set(i.parse::<usize>().unwrap_or(1), to_hex(value))?;
        } else {
            t.set(name, to_hex(value))?;
        }
    }
    t.set("ansi", ansi)?;
    t.set("brights", brights)?;
    Ok(t)
}

/// Copies every entry of `from` into `to`.
fn overlay(to: &Table, from: &Table) -> mlua::Result<()> {
    for pair in from.pairs::<Value, Value>() {
        let (key, value) = pair?;
        to.set(key, value)?;
    }
    Ok(())
}

/// Folds theme.lua's result into the config table: its `color_scheme`
/// replaces the config's and its `colors` go over the config's. A plain
/// string names the scheme. Returns the schemes it defines, in name order.
fn apply_theme(
    lua: &Lua,
    table: &Table,
    theme: Value,
    warnings: &mut Vec<String>,
) -> Result<Vec<(String, Table)>, String> {
    let err = |e: mlua::Error| e.to_string();
    let theme = match theme {
        Value::Nil => return Ok(Vec::new()),
        Value::String(name) => {
            table.set("color_scheme", name).map_err(err)?;
            return Ok(Vec::new());
        }
        Value::Table(t) => t,
        other => {
            return Err(format!(
                "must return a table or a scheme name, got {}",
                other.type_name()
            ));
        }
    };
    let mut custom = Vec::new();
    for pair in theme.pairs::<String, Value>() {
        let (key, value) = pair.map_err(err)?;
        match (key.as_str(), value) {
            ("color_scheme", value) => table.set("color_scheme", value).map_err(err)?,
            ("colors", Value::Table(colors)) => {
                let merged = match table.get::<Option<Table>>("colors").map_err(err)? {
                    Some(own) => own,
                    None => lua.create_table().map_err(err)?,
                };
                overlay(&merged, &colors).map_err(err)?;
                table.set("colors", merged).map_err(err)?;
            }
            ("schemes", Value::Table(schemes)) => {
                for pair in schemes.pairs::<String, Table>() {
                    custom.push(pair.map_err(|e| format!("schemes: {e}"))?);
                }
            }
            ("colors" | "schemes", other) => {
                return Err(format!(
                    "`{key}` must be a table, got {}",
                    other.type_name()
                ));
            }
            _ => warnings.push(format!("unknown theme setting `{key}` ignored")),
        }
    }
    custom.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(custom)
}

/// A scheme theme.lua defines: `foreground`, `background`, `ansi` and
/// `brights` are required; the rest are derived like the built-in ones'.
fn scheme_colors(lua: &Lua, scheme: &Table) -> Result<Colors, String> {
    for key in ["foreground", "background", "ansi", "brights"] {
        if !scheme.contains_key(key).map_err(|e| e.to_string())? {
            return Err(format!("missing `{key}`"));
        }
    }
    let given: Colors = lua
        .from_value(Value::Table(scheme.clone()))
        .map_err(|e| e.to_string())?;
    let has = |key| scheme.contains_key(key).unwrap_or(false);
    let base = schemes::derive(
        given.foreground,
        given.background,
        has("cursor").then_some(given.cursor),
        has("selection").then_some(given.selection),
        given.ansi,
        given.brights,
    );
    let full = colors_table(lua, &base).map_err(|e| e.to_string())?;
    overlay(&full, scheme).map_err(|e| e.to_string())?;
    lua.from_value(Value::Table(full))
        .map_err(|e| e.to_string())
}

/// With `color_scheme` set, `colors` starts from that scheme (one of
/// `custom` or a built-in one) and the colors the config gives override it.
fn apply_scheme(
    lua: &Lua,
    table: &Table,
    custom: &[(String, Colors)],
    warnings: &mut Vec<String>,
) -> mlua::Result<()> {
    let Some(name) = table.get::<Option<String>>("color_scheme")? else {
        return Ok(());
    };
    let Some((proper, scheme)) = schemes::resolve(custom, &name) else {
        warnings.push(format!("unknown color scheme {name:?}"));
        return Ok(());
    };
    table.set("color_scheme", proper)?;
    let colors = colors_table(lua, &scheme)?;
    if let Some(own) = table.get::<Option<Table>>("colors")? {
        overlay(&colors, &own)?;
    }
    table.set("colors", colors)
}

/// Deserializes the config table. Settings this version doesn't know (a
/// typo, or a config written for a newer kodama) are dropped with a warning
/// instead of discarding the whole config.
fn parse_config(lua: &Lua, table: &Table, warnings: &mut Vec<String>) -> Result<Config, String> {
    for _ in 0..64 {
        let err = match lua.from_value::<Config>(Value::Table(table.clone())) {
            Ok(config) => return Ok(config),
            Err(e) => e.to_string(),
        };
        let unknown = err
            .split_once("unknown field `")
            .and_then(|(_, rest)| rest.split_once('`'))
            .map(|(field, _)| field.to_string());
        match unknown.and_then(|field| remove_key(table, &field, "", 0)) {
            Some(path) => warnings.push(format!("unknown setting `{path}` ignored")),
            None => return Err(format!("invalid config: {err}")),
        }
    }
    Err("invalid config: too many unknown settings".into())
}

/// Removes the first table entry named `key`, searching nested tables, and
/// returns its dotted path.
fn remove_key(table: &Table, key: &str, prefix: &str, depth: usize) -> Option<String> {
    if table.contains_key(key).unwrap_or(false) {
        table.set(key, Value::Nil).ok()?;
        return Some(format!("{prefix}{key}"));
    }
    if depth >= 8 {
        return None;
    }
    table
        .pairs::<Value, Value>()
        .flatten()
        .find_map(|(k, v)| match (k, v) {
            (Value::String(name), Value::Table(child)) => {
                let name = name.to_str().ok()?.to_string();
                remove_key(&child, key, &format!("{prefix}{name}."), depth + 1)
            }
            _ => None,
        })
}

fn parse_keys(value: Value, sup: u8) -> Result<Vec<KeyBinding>, String> {
    let table = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(t) => t,
        other => return Err(format!("keys must be a list, got {}", other.type_name())),
    };
    let mut bindings = Vec::new();
    for (i, entry) in table.sequence_values::<Table>().enumerate() {
        let at = |msg: String| format!("keys[{}]: {msg}", i + 1);
        let entry = entry.map_err(|e| at(e.to_string()))?;
        let key: String = entry.get("key").map_err(|_| at("missing `key`".into()))?;
        let mods: Option<String> = entry.get("mods").map_err(|e| at(e.to_string()))?;
        let mods = parse_mods(mods.as_deref().unwrap_or(""), sup).map_err(at)?;
        let action =
            parse_action(entry.get("action").map_err(|e| at(e.to_string()))?).map_err(at)?;
        bindings.push(KeyBinding {
            key: normalize_key(&key),
            mods,
            action,
        });
    }
    Ok(bindings)
}

fn parse_action(value: Value) -> Result<Action, String> {
    match value {
        Value::String(s) => {
            let name = s.to_str().map_err(|e| e.to_string())?.to_string();
            Action::from_name(&name).ok_or_else(|| format!("unknown action {name:?}"))
        }
        Value::Function(f) => Ok(Action::Lua(f)),
        Value::Table(t) => {
            let text: Option<String> = t.get("send").map_err(|e| e.to_string())?;
            text.map(Action::Send)
                .ok_or_else(|| "action table needs a `send` field".into())
        }
        Value::Nil => Err("missing `action`".into()),
        other => Err(format!("invalid action of type {}", other.type_name())),
    }
}

/// Installs the `kodama` global and makes `require` search the config directory.
fn install_api(lua: &Lua, path: &Path) -> mlua::Result<()> {
    let dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .to_string_lossy()
        .into_owned();
    let package: Table = lua.globals().get("package")?;
    let search: String = package.get("path")?;
    package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua;{search}"))?;

    let kodama = lua.create_table()?;
    kodama.set("version", include_str!("../../VERSION").trim())?;
    kodama.set("config_dir", dir)?;
    kodama.set("home", std::env::var("HOME").unwrap_or_default())?;
    kodama.set("os", std::env::consts::OS)?;
    kodama.set(
        "color_schemes",
        lua.create_sequence_from(schemes::SCHEMES.iter().map(|s| s.name))?,
    )?;
    kodama.set(
        "hostname",
        lua.create_function(|_, ()| {
            Ok(std::fs::read_to_string("/proc/sys/kernel/hostname")
                .or_else(|_| std::fs::read_to_string("/etc/hostname"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default())
        })?,
    )?;
    kodama.set(
        "log",
        lua.create_function(|_, args: Variadic<Value>| {
            let parts: Vec<String> = args
                .iter()
                .map(|v| v.to_string().unwrap_or_default())
                .collect();
            log::info!("[config] {}", parts.join(" "));
            Ok(())
        })?,
    )?;
    kodama.set(
        "send",
        lua.create_function(|lua, text: String| {
            let t = lua.create_table()?;
            t.set("send", text)?;
            Ok(t)
        })?,
    )?;
    lua.globals().set("trex", &kodama)?;
    lua.globals().set("kodama", kodama)
}

/// State exposed to Lua callbacks as fields of the window handle.
pub struct WindowInfo {
    pub cols: usize,
    pub rows: usize,
    pub font_size: f32,
    pub title: String,
    pub selection: Option<String>,
    pub alt_screen: bool,
    pub pane_count: usize,
}

/// What a Lua callback asked the terminal to do, applied after it returns.
pub enum Command {
    Action(Action),
    SetFontSize(f32),
    Scroll(isize),
}

/// Methods are callable as `win:send("x")` or `win.send("x")`: the handle
/// itself (a table) is dropped from the arguments.
fn without_self(args: Variadic<Value>) -> Vec<Value> {
    args.into_iter()
        .filter(|v| !matches!(v, Value::Table(_)))
        .collect()
}

pub fn call_action(
    lua: &Lua,
    callback: &Function,
    info: WindowInfo,
) -> Result<Vec<Command>, String> {
    let queue: Rc<RefCell<Vec<Command>>> = Rc::default();
    let build = || -> mlua::Result<Table> {
        let win = lua.create_table()?;
        win.set("cols", info.cols)?;
        win.set("rows", info.rows)?;
        win.set("font_size", info.font_size)?;
        win.set("title", info.title.as_str())?;
        win.set("selection", info.selection.as_deref())?;
        win.set("alt_screen", info.alt_screen)?;
        win.set("pane_count", info.pane_count)?;

        let q = queue.clone();
        win.set(
            "send",
            lua.create_function(move |_, args: Variadic<Value>| {
                for v in without_self(args) {
                    let text = v.to_string()?;
                    q.borrow_mut().push(Command::Action(Action::Send(text)));
                }
                Ok(())
            })?,
        )?;
        let q = queue.clone();
        win.set(
            "action",
            lua.create_function(move |_, args: Variadic<Value>| {
                let name = without_self(args)
                    .first()
                    .map(|v| v.to_string())
                    .transpose()?
                    .unwrap_or_default();
                let action = Action::from_name(&name)
                    .ok_or_else(|| mlua::Error::runtime(format!("unknown action {name:?}")))?;
                q.borrow_mut().push(Command::Action(action));
                Ok(())
            })?,
        )?;
        for (name, action) in [("copy", Action::Copy), ("paste", Action::Paste)] {
            let q = queue.clone();
            win.set(
                name,
                lua.create_function(move |_, _: Variadic<Value>| {
                    q.borrow_mut().push(Command::Action(action.clone()));
                    Ok(())
                })?,
            )?;
        }
        let q = queue.clone();
        win.set(
            "set_font_size",
            lua.create_function(move |_, args: Variadic<Value>| {
                let size = match without_self(args).first() {
                    Some(Value::Integer(n)) => *n as f32,
                    Some(Value::Number(n)) => *n as f32,
                    _ => return Err(mlua::Error::runtime("set_font_size expects a number")),
                };
                q.borrow_mut().push(Command::SetFontSize(size));
                Ok(())
            })?,
        )?;
        let q = queue.clone();
        win.set(
            "scroll",
            lua.create_function(move |_, args: Variadic<Value>| {
                let lines = match without_self(args).first() {
                    Some(Value::Integer(n)) => *n as isize,
                    Some(Value::Number(n)) => *n as isize,
                    _ => return Err(mlua::Error::runtime("scroll expects a number of lines")),
                };
                q.borrow_mut().push(Command::Scroll(lines));
                Ok(())
            })?,
        )?;
        Ok(win)
    };
    let win = build().map_err(|e| e.to_string())?;
    callback.call::<()>(win).map_err(|e| e.to_string())?;
    Ok(queue.take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mods;
    use crate::layout::Direction;

    fn load_files(config: Option<&str>, keybind: Option<&str>) -> Result<LoadedConfig, String> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kodama-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lua");
        if let Some(source) = config {
            std::fs::write(&path, source).unwrap();
        }
        if let Some(source) = keybind {
            std::fs::write(keybind_path(&path), source).unwrap();
        }
        let result = load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn load_str(source: &str) -> Result<LoadedConfig, String> {
        load_files(Some(source), None)
    }

    #[test]
    fn keybind_file_overrides_config_keys() {
        let loaded = load_files(
            Some(r#"return { keys = { { key = "t", mods = "ctrl|shift", action = "copy" } } }"#),
            Some(
                r#"return {
                  { key = "t", mods = "ctrl|shift", action = "paste" },
                  { key = "d", mods = "ctrl|shift", action = function(win) win:send("x") end },
                }"#,
            ),
        )
        .unwrap();
        let c = &loaded.config;
        let both = mods::CTRL | mods::SHIFT;
        assert!(matches!(c.find_binding("t", both), Some(Action::Paste)));
        assert!(matches!(c.find_binding("d", both), Some(Action::Lua(_))));
        // Defaults still follow, on alt rather than ctrl.
        assert!(matches!(
            c.find_binding("e", mods::ALT | mods::SHIFT),
            Some(Action::SplitRight)
        ));
    }

    #[test]
    fn keybind_file_works_without_config_and_can_drop_defaults() {
        let loaded = load_files(
            None,
            Some(r#"return { disable_default_keys = true, keys = { { key = "q", mods = "ctrl", action = kodama.send("hi") } } }"#),
        )
        .unwrap();
        let c = &loaded.config;
        assert!(matches!(c.find_binding("q", mods::CTRL), Some(Action::Send(s)) if s == "hi"));
        assert!(c.find_binding("e", mods::CTRL | mods::SHIFT).is_none());
        assert_eq!(c.keys.len(), 1);
    }

    #[test]
    fn super_defaults_to_ctrl_and_can_be_changed() {
        let c = load_files(None, None).unwrap().config;
        assert!(matches!(
            c.find_binding("enter", mods::CTRL),
            Some(Action::NewPane)
        ));
        assert!(matches!(
            c.find_binding("arrowleft", mods::CTRL | mods::SHIFT),
            Some(Action::MovePane(Direction::Left))
        ));

        let c = load_files(
            None,
            Some(r#"return { super = "alt", keys = { { key = "x", mods = "super|shift", action = "copy" } } }"#),
        )
        .unwrap()
        .config;
        assert!(matches!(
            c.find_binding("x", mods::ALT | mods::SHIFT),
            Some(Action::Copy)
        ));
        assert!(matches!(
            c.find_binding("arrowup", mods::ALT),
            Some(Action::FocusPane(Direction::Up))
        ));
        assert!(c.find_binding("enter", mods::CTRL).is_none());

        // The physical Super key, from config.lua.
        let c = load_files(Some(r#"return { super = "logo" }"#), None)
            .unwrap()
            .config;
        assert!(matches!(
            c.find_binding("enter", mods::SUPER),
            Some(Action::NewPane)
        ));
        assert!(load_files(Some(r#"return { super = "" }"#), None).is_err());
    }

    #[test]
    fn keybind_errors_name_the_file() {
        let err = load_files(None, Some("return { { key = 'x', action = 'nope' } }"))
            .err()
            .unwrap();
        assert!(err.contains("keybind.lua") && err.contains("nope"), "{err}");
        let err = load_files(None, Some("return 42")).err().unwrap();
        assert!(err.contains("keybind.lua"), "{err}");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let loaded = load(Path::new("/nonexistent/kodama/config.lua")).unwrap();
        assert_eq!(loaded.config.font.size, 12.0);
        assert!(!loaded.config.keys.is_empty());
    }

    #[test]
    fn config_is_code() {
        let loaded = load_str(
            r##"
            local size = 10
            for _ = 1, 3 do size = size + 1 end
            return {
              font = { family = "Fira Code", size = size },
              colors = { background = "#000000" },
              scrollback = 500,
              keys = {
                { key = "Enter", mods = "alt", action = kodama.send("\x1b\r") },
                { key = "t", mods = "ctrl|shift", action = function(win) win:send("hi") end },
                { key = "c", mods = "ctrl|shift", action = "none" },
              },
            }
            "##,
        )
        .unwrap();
        let c = &loaded.config;
        assert_eq!(c.font.family, "Fira Code");
        assert_eq!(c.font.size, 13.0);
        assert_eq!(c.colors.background, [0, 0, 0]);
        assert_eq!(c.scrollback, 500);
        assert!(
            matches!(c.find_binding("enter", mods::ALT), Some(Action::Send(s)) if s == "\x1b\r")
        );
        // User bindings shadow the default copy binding.
        assert!(matches!(
            c.find_binding("c", mods::CTRL | mods::SHIFT),
            Some(Action::None)
        ));

        let Some(Action::Lua(f)) = c.find_binding("t", mods::CTRL | mods::SHIFT) else {
            panic!()
        };
        let info = WindowInfo {
            cols: 80,
            rows: 24,
            font_size: 13.0,
            title: String::new(),
            selection: None,
            alt_screen: false,
            pane_count: 1,
        };
        let commands = call_action(&loaded.lua, f, info).unwrap();
        assert!(matches!(&commands[..], [Command::Action(Action::Send(s))] if s == "hi"));
    }

    #[test]
    fn example_config_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.lua");
        let loaded = load(&path).unwrap();
        let c = &loaded.config;
        assert!(matches!(
            c.find_binding("\\", mods::ALT | mods::SHIFT),
            Some(Action::SplitRight)
        ));
        assert!(matches!(
            c.find_binding("e", mods::ALT | mods::SHIFT),
            Some(Action::SplitRight)
        ));
        assert!(matches!(
            c.find_binding("enter", mods::ALT | mods::SHIFT),
            Some(Action::Lua(_))
        ));
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(c.color_scheme.as_deref(), Some("Tokyo Night"));
        assert_eq!(c.custom_schemes[0].0, "Gruvbox Amber");
        assert_eq!(c.custom_schemes[0].1.active_border, [0xfa, 0xbd, 0x2f]);
    }

    #[test]
    fn spacing_can_be_set_per_side() {
        use crate::config::Insets;
        let c = load_str(
            r#"return {
                window = { padding = 4, padding_left = 1, padding_bottom = -3 },
                pane = { padding = 2, padding_top = 5, active_padding = 1, active_padding_right = 0 },
                tree = { padding_right = 8, gap = -2 },
            }"#,
        )
        .unwrap()
        .config;
        assert_eq!(
            c.window.insets(),
            Insets {
                top: 4.0,
                right: 4.0,
                bottom: 0.0,
                left: 1.0
            }
        );
        assert_eq!(
            c.pane.insets(),
            Insets {
                top: 5.0,
                right: 2.0,
                bottom: 2.0,
                left: 2.0
            }
        );
        assert_eq!(
            c.pane.active_insets(),
            Insets {
                top: 1.0,
                right: 0.0,
                bottom: 1.0,
                left: 1.0
            }
        );
        assert_eq!(c.tree.insets().right, 8.0);
        assert_eq!(c.tree.insets().left, 0.0);
        assert_eq!(c.tree.divider_gap(), Some(0.0));

        let c = load_str("return {}").unwrap().config;
        assert_eq!(
            c.window.insets(),
            Insets {
                top: 6.0,
                right: 6.0,
                bottom: 6.0,
                left: 6.0
            }
        );
        assert!(c.pane.insets().is_zero());
        assert!(c.pane.active_insets().is_zero());
        assert!(c.tree.insets().is_zero());
        assert_eq!(c.tree.divider_gap(), None);
        assert_eq!((c.split.padding, c.split.width), (6.0, 1.0));
        assert!(!c.split.highlight_ignores_padding);
    }

    #[test]
    fn highlight_can_ignore_padding() {
        let c = load_str(
            r#"return { split = { highlight = "background", highlight_ignores_padding = true } }"#,
        )
        .unwrap()
        .config;
        assert_eq!(c.split.highlight, crate::config::Highlight::Background);
        assert!(c.split.highlight_ignores_padding);
    }

    #[test]
    fn split_spacing_is_separate_from_window_padding() {
        let c =
            load_str("return { window = { padding = 0 }, split = { padding = 10, width = 2 } }")
                .unwrap()
                .config;
        assert_eq!(
            (c.window.padding, c.split.padding, c.split.width),
            (0.0, 10.0, 2.0)
        );
        let c = load_str("return { window = { padding = 20 } }")
            .unwrap()
            .config;
        assert_eq!((c.split.padding, c.split.width), (6.0, 1.0));
    }

    #[test]
    fn color_scheme_is_the_base_for_colors() {
        let c = load_str(
            r##"return { color_scheme = "nord", colors = { cursor = "#ff0000", brights = { "#000000", "#111111", "#222222", "#333333", "#444444", "#555555", "#666666", "#777777" } } }"##,
        )
        .unwrap()
        .config;
        assert_eq!(c.colors.background, [0x2e, 0x34, 0x40]);
        assert_eq!(c.colors.cursor, [0xff, 0, 0]);
        assert_eq!(c.colors.ansi[1], [0xbf, 0x61, 0x6a]);
        assert_eq!(c.colors.brights[7], [0x77, 0x77, 0x77]);
        let loaded = load_files(Some(r#"return { color_scheme = "nope" }"#), None).unwrap();
        assert_eq!(loaded.warnings, ["unknown color scheme \"nope\""]);
        let names = load_str("return { color_scheme = kodama.color_schemes[1] }")
            .unwrap()
            .config;
        assert_eq!(
            names.colors.background,
            crate::config::Colors::default().background
        );
    }

    fn load_with_theme(config: &str, theme: &str) -> Result<LoadedConfig, String> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kodama-theme-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lua");
        std::fs::write(&path, config).unwrap();
        std::fs::write(theme_path(&path), theme).unwrap();
        let result = load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn language_is_read_and_saved_into_the_config() {
        let loaded = load_str("return { language = 'ja' }").unwrap();
        assert_eq!(loaded.config.language, "ja");
        let dir = std::env::temp_dir().join(format!("kodama-lang-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lua");
        std::fs::write(&path, "return {\n  font = { size = 12 },\n}\n").unwrap();
        save_language(&path, "ja").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("language = \"ja\""), "{text}");
        save_language(&path, "en").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("language = \"en\""), "{text}");
        assert!(!text.contains("\"ja\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picked_scheme_is_written_into_the_theme() {
        let set = |source: &str| set_field(source, "color_scheme", "\"Nord\"", true);
        assert_eq!(
            set("-- color_scheme = \"x\"\nreturn {\n  color_scheme = 'Dracula', -- dark\n  colors = {},\n}\n")
                .unwrap(),
            "-- color_scheme = \"x\"\nreturn {\n  color_scheme = \"Nord\", -- dark\n  colors = {},\n}\n"
        );
        assert_eq!(
            set("local x = 1\nreturn {\n  colors = {},\n}\n").unwrap(),
            "local x = 1\nreturn {\n  color_scheme = \"Nord\",\n  colors = {},\n}\n"
        );
        assert_eq!(
            set("return {}").unwrap(),
            "return {\n  color_scheme = \"Nord\",\n}"
        );
        assert_eq!(set("return \"Kanagawa\"\n").unwrap(), "return \"Nord\"\n");
        assert_eq!(
            set("color_scheme = \"a \\\" b\" return {}").unwrap(),
            "color_scheme = \"Nord\" return {}"
        );
        assert!(set("return require('colors')\n").is_none());

        let dir = std::env::temp_dir().join(format!("kodama-save-{}", std::process::id()));
        let config = dir.join("config.lua");
        let saved = save_color_scheme(&config, "Rosé Pine").unwrap();
        let c = load(&config).unwrap().config;
        assert_eq!(c.color_scheme.as_deref(), Some("Rosé Pine"));
        save_color_scheme(&config, "Nord").unwrap();
        assert!(
            std::fs::read_to_string(&saved)
                .unwrap()
                .matches("color_scheme")
                .count()
                == 1
        );
        assert_eq!(
            load(&config).unwrap().config.color_scheme.as_deref(),
            Some("Nord")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_file_goes_over_the_config_colors() {
        let c = load_with_theme(
            r##"return { color_scheme = "nord", colors = { cursor = "#ff0000", background = "#000000" } }"##,
            r##"return { color_scheme = "gruvbox-dark", colors = { background = "#101010" } }"##,
        )
        .unwrap()
        .config;
        assert_eq!(c.color_scheme.as_deref(), Some("Gruvbox Dark"));
        assert_eq!(c.colors.background, [0x10, 0x10, 0x10]);
        assert_eq!(c.colors.cursor, [0xff, 0, 0]);
        assert_eq!(c.colors.foreground, [0xeb, 0xdb, 0xb2]);

        let c = load_with_theme("return {}", r#"return "Dracula""#)
            .unwrap()
            .config;
        assert_eq!(c.colors.background, [0x28, 0x2a, 0x36]);
    }

    #[test]
    fn theme_file_defines_schemes() {
        let theme = r##"return {
          color_scheme = "my dark",
          schemes = {
            ["My Dark"] = {
              foreground = "#eeeeee", background = "#111111", tab_active = "#123456",
              ansi = { "#000000", "#aa0000", "#00aa00", "#aaaa00", "#0000aa", "#aa00aa", "#00aaaa", "#aaaaaa" },
              brights = { "#555555", "#ff5555", "#55ff55", "#ffff55", "#5555ff", "#ff55ff", "#55ffff", "#ffffff" },
            },
          },
        }"##;
        let c = load_with_theme("return {}", theme).unwrap().config;
        assert_eq!(c.color_scheme.as_deref(), Some("My Dark"));
        assert_eq!(c.colors.background, [0x11, 0x11, 0x11]);
        assert_eq!(c.colors.tab_active, [0x12, 0x34, 0x56]);
        // Derived like a built-in scheme's.
        assert_eq!(c.colors.cursor, [0xee, 0xee, 0xee]);
        assert_eq!(c.colors.active_border, [0, 0, 0xaa]);
        assert_eq!(c.custom_schemes.len(), 1);

        let err = load_with_theme(
            "return {}",
            r##"return { schemes = { Half = { foreground = "#ffffff" } } }"##,
        )
        .err()
        .unwrap();
        assert!(
            err.contains("theme.lua") && err.contains("Half") && err.contains("background"),
            "{err}"
        );
        let err = load_with_theme("return {}", "return 42").err().unwrap();
        assert!(err.contains("theme.lua"), "{err}");
        let loaded = load_with_theme("return {}", "return { colour = {} }").unwrap();
        assert_eq!(loaded.warnings, ["unknown theme setting `colour` ignored"]);
    }

    #[test]
    fn focused_pane_highlight_is_configurable() {
        use crate::config::Highlight;
        assert_eq!(
            load_str("return {}").unwrap().config.split.highlight,
            Highlight::Border
        );
        let c = load_str(r##"return { split = { highlight = "background" }, colors = { active_background = "#102030" } }"##)
            .unwrap()
            .config;
        assert_eq!(c.split.highlight, Highlight::Background);
        assert_eq!(c.colors.active_background, [0x10, 0x20, 0x30]);
        let err = load_str(r#"return { split = { highlight = "glow" } }"#)
            .err()
            .unwrap();
        assert!(err.contains("glow"), "{err}");
    }

    #[test]
    fn unknown_settings_are_dropped_with_a_warning() {
        let loaded = load_str(
            r##"return { font = { size = 15 }, split = { glow = 3, padding = 9 }, sparkle = true, colors = { split = "#112233" } }"##,
        )
        .unwrap();
        assert_eq!(loaded.config.font.size, 15.0);
        assert_eq!(loaded.config.split.padding, 9.0);
        assert_eq!(loaded.config.colors.split, [0x11, 0x22, 0x33]);
        let mut warnings = loaded.warnings;
        warnings.sort();
        assert_eq!(
            warnings,
            [
                "unknown setting `sparkle` ignored",
                "unknown setting `split.glow` ignored"
            ]
        );
    }

    #[test]
    fn errors_are_reported() {
        let loaded = load_str("return { fnt = {} }").unwrap();
        assert!(loaded.warnings[0].contains("fnt"), "{:?}", loaded.warnings);
        let err = load_str("return { keys = { { key = 'x', action = 'nope' } } }")
            .err()
            .unwrap();
        assert!(err.contains("keys[1]") && err.contains("nope"), "{err}");
        let err = load_str("return { colors = { background = 'blue' } }")
            .err()
            .unwrap();
        assert!(err.contains("blue"), "{err}");
        let err = load_str("return {").err().unwrap();
        assert!(err.contains("config.lua"), "{err}");
    }
}
