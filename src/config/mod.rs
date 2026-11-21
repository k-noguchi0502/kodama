//! Configuration: defaults, the Lua-facing schema, and command line overrides.
//!
//! The configuration file is a Lua script (see `lua.rs`) returning a table
//! that deserializes into [`Config`]. Command line flags override it.

pub mod lua;
pub mod schemes;
pub mod watch;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, de};

use crate::layout::Direction;
use crate::terminal::CursorShape;

pub type Rgb = [u8; 3];

/// Parses `#rrggbb`, `rrggbb` or `#rgb`.
pub fn parse_hex(s: &str) -> Result<Rgb, String> {
    let hex = s.trim().trim_start_matches('#');
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()
        .ok_or_else(|| format!("invalid color {s:?}"))?;
    match digits[..] {
        [r, g, b] => Ok([r * 17, g * 17, b * 17]),
        [r1, r2, g1, g2, b1, b2] => Ok([r1 << 4 | r2, g1 << 4 | g2, b1 << 4 | b2]),
        _ => Err(format!("invalid color {s:?} (expected #rrggbb)")),
    }
}

pub fn to_hex([r, g, b]: Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn hex<'de, D: Deserializer<'de>>(d: D) -> Result<Rgb, D::Error> {
    parse_hex(&String::deserialize(d)?).map_err(de::Error::custom)
}

fn hex8<'de, D: Deserializer<'de>>(d: D) -> Result<[Rgb; 8], D::Error> {
    let list = Vec::<String>::deserialize(d)?;
    if list.len() != 8 {
        return Err(de::Error::custom(format!(
            "expected 8 colors, got {}",
            list.len()
        )));
    }
    let mut out = [[0; 3]; 8];
    for (slot, s) in out.iter_mut().zip(&list) {
        *slot = parse_hex(s).map_err(de::Error::custom)?;
    }
    Ok(out)
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    #[serde(deserialize_with = "hex")]
    pub foreground: Rgb,
    #[serde(deserialize_with = "hex")]
    pub background: Rgb,
    #[serde(deserialize_with = "hex")]
    pub cursor: Rgb,
    #[serde(deserialize_with = "hex")]
    pub selection: Rgb,
    /// Divider between panes.
    #[serde(deserialize_with = "hex")]
    pub split: Rgb,
    /// Frame around the focused pane when there are several.
    #[serde(deserialize_with = "hex")]
    pub active_border: Rgb,
    /// Background of the focused pane, with `split.highlight = "background"`.
    #[serde(deserialize_with = "hex")]
    pub active_background: Rgb,
    /// Search matches, and the current one.
    #[serde(deserialize_with = "hex")]
    pub search_match: Rgb,
    #[serde(deserialize_with = "hex")]
    pub search_current: Rgb,
    /// Tab bar: its background, the current tab and the other tabs.
    #[serde(deserialize_with = "hex")]
    pub tab_bar: Rgb,
    #[serde(deserialize_with = "hex")]
    pub tab_active: Rgb,
    #[serde(deserialize_with = "hex")]
    pub tab_inactive: Rgb,
    /// ANSI colors 0-7.
    #[serde(deserialize_with = "hex8")]
    pub ansi: [Rgb; 8],
    /// ANSI colors 8-15.
    #[serde(deserialize_with = "hex8")]
    pub brights: [Rgb; 8],
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: [0xc0, 0xca, 0xf5],
            background: [0x1a, 0x1b, 0x26],
            cursor: [0xc0, 0xca, 0xf5],
            selection: [0x33, 0x46, 0x7c],
            split: [0x3b, 0x42, 0x61],
            active_border: [0x7a, 0xa2, 0xf7],
            active_background: [0x24, 0x28, 0x3b],
            search_match: [0xe0, 0xaf, 0x68],
            search_current: [0xff, 0x9e, 0x64],
            tab_bar: [0x16, 0x16, 0x1e],
            tab_active: [0x7a, 0xa2, 0xf7],
            tab_inactive: [0x24, 0x28, 0x3b],
            ansi: [
                [0x15, 0x16, 0x1e],
                [0xf7, 0x76, 0x8e],
                [0x9e, 0xce, 0x6a],
                [0xe0, 0xaf, 0x68],
                [0x7a, 0xa2, 0xf7],
                [0xbb, 0x9a, 0xf7],
                [0x7d, 0xcf, 0xff],
                [0xa9, 0xb1, 0xd6],
            ],
            brights: [
                [0x41, 0x48, 0x68],
                [0xff, 0x89, 0x9d],
                [0xb9, 0xf2, 0x7c],
                [0xff, 0x9e, 0x64],
                [0x8d, 0xb0, 0xff],
                [0xc7, 0xa9, 0xff],
                [0xa4, 0xda, 0xff],
                [0xc0, 0xca, 0xf5],
            ],
        }
    }
}

impl Colors {
    /// Every color as `(config name, value)`; ANSI ones as `ansi1`..`brights8`.
    pub fn named(&self) -> Vec<(String, Rgb)> {
        let mut out: Vec<(String, Rgb)> = [
            ("foreground", self.foreground),
            ("background", self.background),
            ("cursor", self.cursor),
            ("selection", self.selection),
            ("split", self.split),
            ("active_border", self.active_border),
            ("active_background", self.active_background),
            ("search_match", self.search_match),
            ("search_current", self.search_current),
            ("tab_bar", self.tab_bar),
            ("tab_active", self.tab_active),
            ("tab_inactive", self.tab_inactive),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        for (i, (a, b)) in self.ansi.iter().zip(&self.brights).enumerate() {
            out.push((format!("ansi{}", i + 1), *a));
            out.push((format!("brights{}", i + 1), *b));
        }
        out
    }

    pub fn indexed(&self, index: u8) -> Rgb {
        match index {
            0..=7 => self.ansi[index as usize],
            8..=15 => self.brights[index as usize - 8],
            16..=231 => {
                let i = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                [level(i / 36), level((i / 6) % 6), level(i % 6)]
            }
            232..=255 => {
                let v = 8 + (index - 232) * 10;
                [v, v, v]
            }
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// fontconfig pattern, e.g. "JetBrains Mono" or "monospace".
    pub family: String,
    /// Size in points.
    pub size: f32,
    pub ligatures: bool,
    /// Families tried in order for characters the main font lacks, before
    /// fontconfig picks one (e.g. a CJK font, "Symbols Nerd Font").
    pub fallback: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            size: 12.0,
            ligatures: true,
            fallback: Vec::new(),
        }
    }
}

/// Logical-pixel insets on the four sides. Nothing is below 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Insets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Insets {
    pub const ZERO: Self = Self {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: 0.0,
    };

    /// `padding` sets every side. A `Some` value replaces that side.
    pub fn resolve(
        padding: f32,
        top: Option<f32>,
        right: Option<f32>,
        bottom: Option<f32>,
        left: Option<f32>,
    ) -> Self {
        let base = padding.max(0.0);
        let side = |v: Option<f32>| v.unwrap_or(base).max(0.0);
        Self {
            top: side(top),
            right: side(right),
            bottom: side(bottom),
            left: side(left),
        }
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub fn add(self, other: Self) -> Self {
        Self {
            top: self.top + other.top,
            right: self.right + other.right,
            bottom: self.bottom + other.bottom,
            left: self.left + other.left,
        }
    }
}

impl Default for Insets {
    fn default() -> Self {
        Self::ZERO
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Margin between the window edge and the content, on every side, in
    /// logical pixels. A side of 0 stays flush with the window.
    pub padding: f32,
    /// Replaces `padding` on that side. Omitted sides keep `padding`.
    pub padding_top: Option<f32>,
    pub padding_right: Option<f32>,
    pub padding_bottom: Option<f32>,
    pub padding_left: Option<f32>,
    /// Initial size in cells (floating window managers only).
    pub columns: u16,
    pub rows: u16,
    /// Background opacity from 0 (clear) to 1; text stays opaque. Needs a
    /// compositor (any Wayland one).
    pub opacity: f32,
    /// A PNG drawn behind the text. `~/` and paths relative to the config
    /// file's directory work.
    pub background_image: Option<String>,
    /// How strongly the image shows through the background color, 0 to 1.
    pub background_image_opacity: f32,
    pub background_image_fit: ImageFit,
}

/// How the background image fills the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFit {
    /// Covers the window, cropping what overflows.
    #[default]
    Cover,
    /// Fits inside the window, leaving bars of background color.
    Contain,
    /// Fills the window, ignoring the aspect ratio.
    Stretch,
}

impl WindowConfig {
    pub fn insets(&self) -> Insets {
        Insets::resolve(
            self.padding,
            self.padding_top,
            self.padding_right,
            self.padding_bottom,
            self.padding_left,
        )
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            padding: 6.0,
            padding_top: None,
            padding_right: None,
            padding_bottom: None,
            padding_left: None,
            columns: 100,
            rows: 30,
            opacity: 1.0,
            background_image: None,
            background_image_opacity: 0.25,
            background_image_fit: ImageFit::Cover,
        }
    }
}

/// The divider drawn between panes.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SplitConfig {
    /// Space between the divider line and the panes on either side, in
    /// logical pixels; independent of the window padding.
    pub padding: f32,
    /// Thickness of the divider line in logical pixels (0 hides it).
    pub width: f32,
    /// How the focused pane stands out when there are several.
    pub highlight: Highlight,
    /// When the highlight paints a background, cover the focused pane's
    /// padding too: the colour runs out to the window edge and up to the
    /// divider, instead of stopping on the grid. The grid does not move.
    pub highlight_ignores_padding: bool,
    /// Thickness of the focus border in logical pixels. Drawn when
    /// `highlight` is `border` or `both`; only this stroke is painted.
    /// 0 hides it. With `highlight_ignores_padding` it sits on the outer
    /// edge of the focused pane or tree.
    pub active_border: f32,
}

/// Padding inside pane grids. Separate from the divider gap (`split.padding`).
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaneConfig {
    /// Inset of every pane's grid, on every side, in logical pixels.
    /// 0 keeps the grid flush with the pane's box.
    pub padding: f32,
    pub padding_top: Option<f32>,
    pub padding_right: Option<f32>,
    pub padding_bottom: Option<f32>,
    pub padding_left: Option<f32>,
    /// Extra inset around the focused pane only, added to `padding`.
    pub active_padding: f32,
    pub active_padding_top: Option<f32>,
    pub active_padding_right: Option<f32>,
    pub active_padding_bottom: Option<f32>,
    pub active_padding_left: Option<f32>,
}

impl PaneConfig {
    pub fn insets(&self) -> Insets {
        Insets::resolve(
            self.padding,
            self.padding_top,
            self.padding_right,
            self.padding_bottom,
            self.padding_left,
        )
    }

    pub fn active_insets(&self) -> Insets {
        Insets::resolve(
            self.active_padding,
            self.active_padding_top,
            self.active_padding_right,
            self.active_padding_bottom,
            self.active_padding_left,
        )
    }
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            padding: 0.0,
            padding_top: None,
            padding_right: None,
            padding_bottom: None,
            padding_left: None,
            active_padding: 0.0,
            active_padding_top: None,
            active_padding_right: None,
            active_padding_bottom: None,
            active_padding_left: None,
        }
    }
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            padding: 6.0,
            width: 1.0,
            highlight: Highlight::Border,
            highlight_ignores_padding: false,
            active_border: 2.0,
        }
    }
}

/// The file tree sidebar.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TreeConfig {
    /// Width in columns.
    pub width: usize,
    /// List dotfiles.
    pub show_hidden: bool,
    /// Inset of the tree's grid inside the sidebar, in logical pixels.
    /// 0 keeps it flush with the window and the divider. `width` stays in
    /// columns.
    pub padding: f32,
    pub padding_top: Option<f32>,
    pub padding_right: Option<f32>,
    pub padding_bottom: Option<f32>,
    pub padding_left: Option<f32>,
    /// Padding on each side of the divider between the tree and the panes.
    /// `None` uses `split.padding`.
    pub gap: Option<f32>,
}

impl TreeConfig {
    pub fn insets(&self) -> Insets {
        Insets::resolve(
            self.padding,
            self.padding_top,
            self.padding_right,
            self.padding_bottom,
            self.padding_left,
        )
    }

    /// Padding on each side of the tree's divider, if set. Never negative.
    pub fn divider_gap(&self) -> Option<f32> {
        self.gap.map(|g| g.max(0.0))
    }
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            width: 30,
            show_hidden: false,
            padding: 0.0,
            padding_top: None,
            padding_right: None,
            padding_bottom: None,
            padding_left: None,
            gap: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Highlight {
    /// A frame in `colors.active_border`.
    #[default]
    Border,
    /// The pane's background in `colors.active_background`.
    Background,
    Both,
    None,
}

impl Highlight {
    pub fn border(self) -> bool {
        matches!(self, Highlight::Border | Highlight::Both)
    }

    pub fn background(self) -> bool {
        matches!(self, Highlight::Background | Highlight::Both)
    }
}

/// What a BEL from a pane does.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BellConfig {
    /// Flash the pane that rang.
    pub visual: bool,
    /// Ask the window manager for attention while the window is unfocused.
    pub urgent: bool,
    /// Shell command run on a bell (at most twice a second), e.g. a sound.
    pub command: Option<String>,
}

impl Default for BellConfig {
    fn default() -> Self {
        Self {
            visual: true,
            urgent: true,
            command: None,
        }
    }
}

/// When OSC 9 / OSC 777 notifications become desktop notifications.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Notifications {
    #[default]
    Always,
    /// Only while the window is unfocused or the pane is not the focused one.
    Unfocused,
    Never,
}

/// The tab bar across the top of the window.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TabBarConfig {
    pub style: TabStyle,
    pub show: TabBarShow,
    /// Widest a tab gets, in columns. 0 lets the tabs share the whole bar.
    pub max_width: usize,
    pub title: TabTitle,
}

impl Default for TabBarConfig {
    fn default() -> Self {
        Self {
            style: TabStyle::Plain,
            show: TabBarShow::Auto,
            max_width: 0,
            title: TabTitle::Directory,
        }
    }
}

/// What an unnamed tab is labelled with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabTitle {
    /// The name of its active pane's working directory, `~` at home.
    #[default]
    Directory,
    /// The title its active pane's program set.
    Title,
}

/// How tabs are drawn, after kitty's `tab_bar_style`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabStyle {
    /// Rectangles of colour with a gap between them.
    #[default]
    Plain,
    /// Each tab ends in an arrow pointing at the next.
    Powerline,
    /// Tabs with slanted sides.
    Slant,
    /// Rounded pills with a gap between them.
    Round,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabBarShow {
    /// Only while there is more than one tab.
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
}

impl From<CursorStyle> for CursorShape {
    fn from(style: CursorStyle) -> Self {
        match style {
            CursorStyle::Block => CursorShape::Block,
            CursorStyle::Bar => CursorShape::Bar,
            CursorStyle::Underline => CursorShape::Underline,
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorConfig {
    /// Shape used until an application changes it (and on `CSI 0 q`).
    pub style: CursorStyle,
}

#[derive(Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Program to run; `$SHELL` when unset.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub working_directory: Option<String>,
}

/// Something a key binding (or a Lua callback) can do.
#[derive(Clone, Debug)]
pub enum Action {
    /// Do nothing and pass the key to the application (unbinds a default).
    None,
    Copy,
    Paste,
    PastePrimary,
    /// Copy when text is selected, otherwise pass the key on. Ctrl+C is not
    /// bound, so it still interrupts.
    CopyOrPass,
    /// Paste at the shell; pass the key on in full-screen programs (vim,
    /// nano), which use it themselves.
    PasteOrPass,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    ScrollPageUp,
    ScrollPageDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollToTop,
    ScrollToBottom,
    ClearScrollback,
    ReloadConfig,
    /// New pane beside (right) or below the focused one.
    SplitRight,
    SplitDown,
    /// New pane splitting the focused one along its longer side, like
    /// Hyprland's dwindle layout.
    NewPane,
    /// Swap the focused pane with its neighbour in this direction.
    MovePane(Direction),
    /// Show and focus the file tree; hide it when it already has focus.
    ToggleFileTree,
    ClosePane,
    FocusPane(Direction),
    FocusNextPane,
    FocusPrevPane,
    /// Move the nearest divider in this direction.
    ResizePane(Direction),
    /// Show only the focused pane, or restore the layout.
    TogglePaneZoom,
    /// Search the scrollback of the focused pane.
    Search,
    /// Scroll to the previous / next shell prompt (needs OSC 133 marks).
    ScrollToPrevPrompt,
    ScrollToNextPrompt,
    NewTab,
    NextTab,
    PrevTab,
    CloseTab,
    /// Switch to tab n (counting from 1).
    GoToTab(usize),
    /// Move a cursor over the scrollback with vi keys to select and copy.
    CopyMode,
    /// Label URLs, paths and hashes on screen; typing a label copies one.
    QuickSelect,
    /// A searchable list of every action and color scheme.
    CommandPalette,
    ToggleFullscreen,
    /// Start another kodama window with the same config.
    NewWindow,
    /// Swap the current tab with its neighbour (-1 left, 1 right).
    MoveTab(isize),
    /// Type a name for the current tab.
    RenameTab,
    /// Open the focused pane's history in `scrollback_pager` in a new tab.
    ShowScrollback,
    /// Copy what the last command printed (needs OSC 133 C/D marks).
    CopyLastCommandOutput,
    /// Send typed input to every pane of the current tab, or stop.
    ToggleBroadcast,
    /// Write text to the application.
    Send(String),
    /// Call a Lua function with a window handle.
    Lua(mlua::Function),
}

impl Action {
    pub fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "none" => Action::None,
            "copy" => Action::Copy,
            "paste" => Action::Paste,
            "paste_primary" => Action::PastePrimary,
            "copy_or_pass" => Action::CopyOrPass,
            "paste_or_pass" => Action::PasteOrPass,
            "increase_font_size" => Action::IncreaseFontSize,
            "decrease_font_size" => Action::DecreaseFontSize,
            "reset_font_size" => Action::ResetFontSize,
            "scroll_page_up" => Action::ScrollPageUp,
            "scroll_page_down" => Action::ScrollPageDown,
            "scroll_line_up" => Action::ScrollLineUp,
            "scroll_line_down" => Action::ScrollLineDown,
            "scroll_to_top" => Action::ScrollToTop,
            "scroll_to_bottom" => Action::ScrollToBottom,
            "clear_scrollback" => Action::ClearScrollback,
            "reload_config" => Action::ReloadConfig,
            "split_right" => Action::SplitRight,
            "split_down" => Action::SplitDown,
            "new_pane" => Action::NewPane,
            "toggle_file_tree" => Action::ToggleFileTree,
            "move_pane_left" => Action::MovePane(Direction::Left),
            "move_pane_right" => Action::MovePane(Direction::Right),
            "move_pane_up" => Action::MovePane(Direction::Up),
            "move_pane_down" => Action::MovePane(Direction::Down),
            "close_pane" => Action::ClosePane,
            "focus_pane_left" => Action::FocusPane(Direction::Left),
            "focus_pane_right" => Action::FocusPane(Direction::Right),
            "focus_pane_up" => Action::FocusPane(Direction::Up),
            "focus_pane_down" => Action::FocusPane(Direction::Down),
            "focus_next_pane" => Action::FocusNextPane,
            "focus_prev_pane" => Action::FocusPrevPane,
            "resize_pane_left" => Action::ResizePane(Direction::Left),
            "resize_pane_right" => Action::ResizePane(Direction::Right),
            "resize_pane_up" => Action::ResizePane(Direction::Up),
            "resize_pane_down" => Action::ResizePane(Direction::Down),
            "toggle_pane_zoom" => Action::TogglePaneZoom,
            "search" => Action::Search,
            "scroll_to_prev_prompt" => Action::ScrollToPrevPrompt,
            "scroll_to_next_prompt" => Action::ScrollToNextPrompt,
            "new_tab" => Action::NewTab,
            "next_tab" => Action::NextTab,
            "prev_tab" => Action::PrevTab,
            "close_tab" => Action::CloseTab,
            "copy_mode" => Action::CopyMode,
            "quick_select" => Action::QuickSelect,
            "command_palette" => Action::CommandPalette,
            "toggle_fullscreen" => Action::ToggleFullscreen,
            "new_window" => Action::NewWindow,
            "move_tab_left" => Action::MoveTab(-1),
            "move_tab_right" => Action::MoveTab(1),
            "rename_tab" => Action::RenameTab,
            "show_scrollback" => Action::ShowScrollback,
            "copy_last_command_output" => Action::CopyLastCommandOutput,
            "toggle_broadcast" => Action::ToggleBroadcast,
            _ => match name.strip_prefix("go_to_tab_").and_then(|n| n.parse().ok()) {
                Some(n @ 1..) => Action::GoToTab(n),
                _ => return None,
            },
        })
    }
}

/// Actions offered by the command palette, by config name.
pub const PALETTE_ACTIONS: &[&str] = &[
    "copy_mode",
    "quick_select",
    "search",
    "copy",
    "copy_last_command_output",
    "show_scrollback",
    "paste",
    "paste_primary",
    "new_window",
    "new_tab",
    "next_tab",
    "prev_tab",
    "close_tab",
    "move_tab_left",
    "move_tab_right",
    "rename_tab",
    "new_pane",
    "split_right",
    "split_down",
    "close_pane",
    "toggle_pane_zoom",
    "toggle_broadcast",
    "toggle_file_tree",
    "toggle_fullscreen",
    "focus_next_pane",
    "focus_prev_pane",
    "focus_pane_left",
    "focus_pane_right",
    "focus_pane_up",
    "focus_pane_down",
    "move_pane_left",
    "move_pane_right",
    "move_pane_up",
    "move_pane_down",
    "resize_pane_left",
    "resize_pane_right",
    "resize_pane_up",
    "resize_pane_down",
    "scroll_to_prev_prompt",
    "scroll_to_next_prompt",
    "scroll_page_up",
    "scroll_page_down",
    "scroll_to_top",
    "scroll_to_bottom",
    "clear_scrollback",
    "increase_font_size",
    "decrease_font_size",
    "reset_font_size",
    "reload_config",
];

pub mod mods {
    pub const SHIFT: u8 = 1;
    pub const CTRL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;
}

/// Parses `"ctrl|shift"` style modifiers. `super` stands for `super_mods`,
/// the configurable "super" key (Ctrl by default, since the desktop usually
/// owns the real one); `logo` / `win` / `cmd` name the physical Super key.
pub fn parse_mods(s: &str, super_mods: u8) -> Result<u8, String> {
    s.split(['|', '+', ' ', ','])
        .filter(|part| !part.is_empty())
        .try_fold(0, |acc, part| {
            Ok(acc
                | match part.to_ascii_lowercase().as_str() {
                    "shift" => mods::SHIFT,
                    "ctrl" | "control" => mods::CTRL,
                    "alt" | "meta" | "option" => mods::ALT,
                    "super" => super_mods,
                    "cmd" | "win" | "logo" => mods::SUPER,
                    other => return Err(format!("unknown modifier {other:?}")),
                })
        })
}

/// Normalises a key name so config strings and key events compare equal:
/// single characters are lowercased, named keys use winit's names in
/// lowercase (`enter`, `pageup`, `f5`, `arrowup`), with common aliases.
pub fn normalize_key(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "up" => "arrowup".into(),
        "down" => "arrowdown".into(),
        "left" => "arrowleft".into(),
        "right" => "arrowright".into(),
        "return" => "enter".into(),
        "esc" => "escape".into(),
        "plus" => "+".into(),
        "minus" => "-".into(),
        "equal" | "equals" => "=".into(),
        "pgup" => "pageup".into(),
        "pgdown" | "pgdn" => "pagedown".into(),
        "del" => "delete".into(),
        "ins" => "insert".into(),
        _ => lower,
    }
}

#[derive(Clone, Debug)]
pub struct KeyBinding {
    /// Normalised with [`normalize_key`].
    pub key: String,
    pub mods: u8,
    pub action: Action,
}

impl KeyBinding {
    fn new(key: &str, mods: u8, action: Action) -> Self {
        Self {
            key: normalize_key(key),
            mods,
            action,
        }
    }
}

/// Modifiers for the resize bindings. Normally `super+alt`. When `super`
/// already includes alt, that chord is `super` itself, which moves focus,
/// so resize takes the next free modifier: ctrl, then the physical Super key.
fn resize_mods(sup: u8) -> u8 {
    for extra in [mods::ALT, mods::CTRL, mods::SUPER] {
        if sup & extra == 0 {
            return sup | extra;
        }
    }
    sup | mods::SHIFT
}

/// The built-in bindings; `sup` is the configured "super" modifier.
pub fn default_keys(sup: u8) -> Vec<KeyBinding> {
    use Direction::*;
    use mods::*;
    let resize = resize_mods(sup);
    let mut keys = vec![
        // Hyprland-style pane management.
        KeyBinding::new("enter", sup, Action::NewPane),
        KeyBinding::new("q", sup, Action::ClosePane),
        KeyBinding::new("b", sup | SHIFT, Action::ToggleFileTree),
        KeyBinding::new("left", sup, Action::FocusPane(Left)),
        KeyBinding::new("right", sup, Action::FocusPane(Right)),
        KeyBinding::new("up", sup, Action::FocusPane(Up)),
        KeyBinding::new("down", sup, Action::FocusPane(Down)),
        KeyBinding::new("left", sup | SHIFT, Action::MovePane(Left)),
        KeyBinding::new("right", sup | SHIFT, Action::MovePane(Right)),
        KeyBinding::new("up", sup | SHIFT, Action::MovePane(Up)),
        KeyBinding::new("down", sup | SHIFT, Action::MovePane(Down)),
        KeyBinding::new("left", resize, Action::ResizePane(Left)),
        KeyBinding::new("right", resize, Action::ResizePane(Right)),
        KeyBinding::new("up", resize, Action::ResizePane(Up)),
        KeyBinding::new("down", resize, Action::ResizePane(Down)),
    ];
    keys.extend([
        KeyBinding::new("c", ALT, Action::CopyOrPass),
        KeyBinding::new("v", ALT, Action::PasteOrPass),
        KeyBinding::new("c", ALT | SHIFT, Action::Copy),
        KeyBinding::new("v", ALT | SHIFT, Action::Paste),
        KeyBinding::new("insert", SHIFT, Action::PastePrimary),
        KeyBinding::new("=", ALT, Action::IncreaseFontSize),
        KeyBinding::new("=", ALT | SHIFT, Action::IncreaseFontSize),
        KeyBinding::new("+", ALT, Action::IncreaseFontSize),
        KeyBinding::new("-", ALT, Action::DecreaseFontSize),
        KeyBinding::new("0", ALT, Action::ResetFontSize),
        KeyBinding::new("pageup", SHIFT, Action::ScrollPageUp),
        KeyBinding::new("pagedown", SHIFT, Action::ScrollPageDown),
        KeyBinding::new("home", SHIFT, Action::ScrollToTop),
        KeyBinding::new("end", SHIFT, Action::ScrollToBottom),
        KeyBinding::new("r", ALT | SHIFT, Action::ReloadConfig),
        KeyBinding::new("e", ALT | SHIFT, Action::SplitRight),
        KeyBinding::new("o", ALT | SHIFT, Action::SplitDown),
        KeyBinding::new("w", ALT | SHIFT, Action::ClosePane),
        KeyBinding::new("left", ALT, Action::FocusPane(Left)),
        KeyBinding::new("right", ALT, Action::FocusPane(Right)),
        KeyBinding::new("up", ALT, Action::FocusPane(Up)),
        KeyBinding::new("down", ALT, Action::FocusPane(Down)),
        KeyBinding::new("tab", ALT, Action::FocusNextPane),
        KeyBinding::new("tab", ALT | SHIFT, Action::FocusPrevPane),
        KeyBinding::new("z", ALT | SHIFT, Action::TogglePaneZoom),
        KeyBinding::new("f", ALT | SHIFT, Action::Search),
        KeyBinding::new("pageup", ALT | SHIFT, Action::ScrollToPrevPrompt),
        KeyBinding::new("pagedown", ALT | SHIFT, Action::ScrollToNextPrompt),
        KeyBinding::new("t", ALT | SHIFT, Action::NewTab),
        KeyBinding::new("pagedown", ALT, Action::NextTab),
        KeyBinding::new("pageup", ALT, Action::PrevTab),
        KeyBinding::new("1", ALT, Action::GoToTab(1)),
        KeyBinding::new("2", ALT, Action::GoToTab(2)),
        KeyBinding::new("3", ALT, Action::GoToTab(3)),
        KeyBinding::new("4", ALT, Action::GoToTab(4)),
        KeyBinding::new("5", ALT, Action::GoToTab(5)),
        KeyBinding::new("6", ALT, Action::GoToTab(6)),
        KeyBinding::new("7", ALT, Action::GoToTab(7)),
        KeyBinding::new("8", ALT, Action::GoToTab(8)),
        KeyBinding::new("9", ALT, Action::GoToTab(9)),
        KeyBinding::new("x", ALT | SHIFT, Action::CopyMode),
        KeyBinding::new("space", ALT | SHIFT, Action::QuickSelect),
        KeyBinding::new("p", ALT | SHIFT, Action::CommandPalette),
        KeyBinding::new("f11", 0, Action::ToggleFullscreen),
        KeyBinding::new("n", ALT | SHIFT, Action::NewWindow),
        KeyBinding::new(",", ALT | SHIFT, Action::MoveTab(-1)),
        KeyBinding::new(".", ALT | SHIFT, Action::MoveTab(1)),
        KeyBinding::new("i", ALT | SHIFT, Action::RenameTab),
        KeyBinding::new("h", ALT | SHIFT, Action::ShowScrollback),
        KeyBinding::new("y", ALT | SHIFT, Action::CopyLastCommandOutput),
        KeyBinding::new("a", ALT | SHIFT, Action::ToggleBroadcast),
    ]);
    // `super` can be alt, which makes the super+arrow focus bindings the
    // same chord as the plain alt+arrow ones. The first one stands.
    let mut seen = std::collections::HashSet::new();
    keys.retain(|k| seen.insert((k.key.clone(), k.mods)));
    keys
}

/// Behaviour that can be turned off without removing the keys that use it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Features {
    /// Save the tabs, splits and directories, and restore them next launch.
    pub restore_layout: bool,
    /// The file tree sidebar.
    pub file_tree: bool,
    /// In the file tree, `s` opens the selection in a new pane.
    pub tree_open_split: bool,
    /// Label URLs and paths on screen and copy one by typing its label.
    pub quick_select: bool,
    /// Alt plus a quick-select label opens the URL or path.
    pub quick_select_open: bool,
    /// Move through the scrollback with vi keys.
    pub copy_mode: bool,
    /// Search the scrollback.
    pub search: bool,
    /// The searchable list of actions, schemes and languages.
    pub command_palette: bool,
    /// Drag a tab to reorder it, or alt-drag a pane onto another to swap.
    pub drag_reorder: bool,
    /// Clicking a desktop notification focuses the pane that sent it.
    pub notification_focus: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            restore_layout: true,
            file_tree: true,
            tree_open_split: true,
            quick_select: true,
            quick_select_open: true,
            copy_mode: true,
            search: true,
            command_palette: true,
            drag_reorder: true,
            notification_focus: true,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub colors: Colors,
    pub window: WindowConfig,
    pub split: SplitConfig,
    pub pane: PaneConfig,
    pub tree: TreeConfig,
    pub tab_bar: TabBarConfig,
    pub cursor: CursorConfig,
    pub bell: BellConfig,
    pub notifications: Notifications,
    /// A scheme (built in, see `schemes.rs`, or from theme.lua) that
    /// `colors` starts from.
    pub color_scheme: Option<String>,
    /// UI language id (`en`, `ja`, or a file in `lang/`). Only that file is
    /// loaded.
    pub language: String,
    /// Schemes defined in theme.lua, by name.
    #[serde(skip)]
    pub custom_schemes: Vec<(String, Colors)>,
    pub scrollback: usize,
    /// Command show_scrollback opens the history file with (appended).
    pub scrollback_pager: String,
    /// Ignore newlines printed while the screen is empty, so a prompt that
    /// starts with one (starship's `add_newline`, some zsh themes) begins on
    /// the top row after startup and `clear`.
    pub trim_leading_blank_lines: bool,
    /// Ask before closing the window (or its last pane).
    pub confirm_close: bool,
    /// Optional behaviour. Each one is on unless the config turns it off.
    pub features: Features,
    pub shell: ShellConfig,
    /// Extra environment variables for the shell.
    pub env: HashMap<String, String>,
    /// Modifiers that `super` means in key bindings (default `ctrl`).
    #[serde(rename = "super")]
    pub super_key: String,
    /// Drop the built-in key bindings and use only `keys`.
    pub disable_default_keys: bool,
    /// User bindings first (they take precedence), then the defaults.
    #[serde(skip)]
    pub keys: Vec<KeyBinding>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            colors: Colors::default(),
            window: WindowConfig::default(),
            split: SplitConfig::default(),
            pane: PaneConfig::default(),
            tree: TreeConfig::default(),
            tab_bar: TabBarConfig::default(),
            cursor: CursorConfig::default(),
            bell: BellConfig::default(),
            notifications: Notifications::default(),
            color_scheme: None,
            language: "en".into(),
            custom_schemes: Vec::new(),
            scrollback: 10_000,
            scrollback_pager: "less +G".into(),
            trim_leading_blank_lines: true,
            confirm_close: true,
            features: Features::default(),
            shell: ShellConfig::default(),
            env: HashMap::new(),
            super_key: "ctrl".into(),
            disable_default_keys: false,
            keys: default_keys(mods::CTRL),
        }
    }
}

impl Config {
    pub fn find_binding(&self, key: &str, mods: u8) -> Option<&Action> {
        self.keys
            .iter()
            .find(|b| b.key == key && b.mods == mods)
            .map(|b| &b.action)
    }

    pub fn shell_program(&self) -> String {
        self.shell
            .program
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".into())
    }

    pub fn apply_cli(&mut self, cli: &Cli) {
        if let Some(family) = &cli.font {
            self.font.family = family.clone();
        }
        if let Some(size) = cli.size {
            self.font.size = size;
        }
        if cli.no_ligatures {
            self.font.ligatures = false;
        }
        if let Some(lines) = cli.scrollback {
            self.scrollback = lines;
        }
        if let Some((program, args)) = &cli.command {
            self.shell.program = Some(program.clone());
            self.shell.args = args.clone();
        }
    }
}

/// Command line flags; they override the configuration file.
#[derive(Default)]
pub struct Cli {
    pub config: Option<PathBuf>,
    pub font: Option<String>,
    pub size: Option<f32>,
    pub no_ligatures: bool,
    pub scrollback: Option<usize>,
    pub command: Option<(String, Vec<String>)>,
}

const USAGE: &str = "\
kodama - a GPU-accelerated terminal emulator

USAGE:
    kodama [OPTIONS] [-e COMMAND [ARGS...]]

OPTIONS:
    -c, --config <FILE>      Lua config file [default: ~/.config/kodama/config.lua];
                             colors and key bindings are read from theme.lua
                             and keybind.lua beside it
    -f, --font <FAMILY>      Font family (fontconfig pattern)
    -s, --size <POINTS>      Font size in points
        --no-ligatures       Disable font ligatures
        --scrollback <LINES> Scrollback lines
    -e, --command <CMD>...   Run CMD instead of the configured shell
    -h, --help               Print help

ENVIRONMENT:
    KODAMA_CONFIG              Config file path (overridden by --config)
";

impl Cli {
    pub fn parse() -> Self {
        let mut cli = Cli {
            config: std::env::var_os("KODAMA_CONFIG")
                .or_else(|| std::env::var_os("TREX_CONFIG"))
                .map(PathBuf::from),
            ..Cli::default()
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().unwrap_or_else(|| usage_error(&arg));
            match arg.as_str() {
                "-h" | "--help" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                "-c" | "--config" => cli.config = Some(value().into()),
                "-f" | "--font" => cli.font = Some(value()),
                "-s" | "--size" => {
                    cli.size = Some(value().parse().unwrap_or_else(|_| usage_error(&arg)))
                }
                "--no-ligatures" => cli.no_ligatures = true,
                "--scrollback" => {
                    cli.scrollback = Some(value().parse().unwrap_or_else(|_| usage_error(&arg)))
                }
                "-e" | "--command" => {
                    let program = value();
                    cli.command = Some((program, args.by_ref().collect()));
                }
                _ => usage_error(&arg),
            }
        }
        cli
    }
}

fn usage_error(arg: &str) -> ! {
    eprintln!("kodama: invalid or incomplete argument `{arg}`\n\n{USAGE}");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#1a1B26"), Ok([0x1a, 0x1b, 0x26]));
        assert_eq!(parse_hex("fff"), Ok([255, 255, 255]));
        assert!(parse_hex("#12345").is_err());
        assert!(parse_hex("#gg0000").is_err());
    }

    #[test]
    fn default_keys_do_not_shadow_each_other() {
        for sup in [mods::CTRL, mods::SUPER, mods::ALT] {
            let mut seen = std::collections::HashMap::new();
            for k in default_keys(sup) {
                if let Some(first) = seen.insert((k.key.clone(), k.mods), k.action.clone()) {
                    panic!(
                        "super={sup}: {} {:#x} is both {first:?} and {:?}",
                        k.key, k.mods, k.action
                    );
                }
            }
        }
    }

    #[test]
    fn resize_stays_distinct_when_super_is_alt() {
        let keys = default_keys(mods::ALT);
        assert!(keys.iter().any(|k| {
            k.key == "arrowleft"
                && k.mods == mods::ALT | mods::CTRL
                && matches!(k.action, Action::ResizePane(Direction::Left))
        }));
        assert_eq!(
            keys.iter()
                .filter(|k| k.key == "arrowleft" && k.mods == mods::ALT)
                .count(),
            1
        );
        assert!(matches!(
            keys.iter()
                .find(|k| k.key == "arrowleft" && k.mods == mods::ALT)
                .map(|k| &k.action),
            Some(Action::FocusPane(Direction::Left))
        ));
    }

    #[test]
    fn modifiers_and_keys() {
        assert_eq!(parse_mods("ctrl|shift", 0), Ok(mods::CTRL | mods::SHIFT));
        assert_eq!(parse_mods("Alt+Logo", 0), Ok(mods::ALT | mods::SUPER));
        assert_eq!(
            parse_mods("super|shift", mods::CTRL),
            Ok(mods::CTRL | mods::SHIFT)
        );
        assert_eq!(parse_mods("", 0), Ok(0));
        assert!(parse_mods("hyper", 0).is_err());
        assert_eq!(normalize_key("PageUp"), "pageup");
        assert_eq!(normalize_key("Up"), "arrowup");
        assert_eq!(normalize_key("C"), "c");
    }

    #[test]
    fn palette_actions_exist() {
        for name in PALETTE_ACTIONS {
            assert!(Action::from_name(name).is_some(), "{name}");
        }
    }

    #[test]
    fn alt_c_and_v_are_clipboard_keys() {
        let c = Config::default();
        assert!(matches!(
            c.find_binding("c", mods::ALT),
            Some(Action::CopyOrPass)
        ));
        assert!(matches!(
            c.find_binding("v", mods::ALT),
            Some(Action::PasteOrPass)
        ));
        assert!(matches!(
            c.find_binding("c", mods::ALT | mods::SHIFT),
            Some(Action::Copy)
        ));
        assert!(c.find_binding("c", mods::CTRL).is_none());
    }
}
