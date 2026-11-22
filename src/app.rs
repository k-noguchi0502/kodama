//! Window/event-loop glue between the PTYs, the terminal state of each pane
//! and the renderer.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use mlua::Lua;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, Window, WindowId};

use crate::config::lua::{self, Command, WindowInfo};
use crate::config::schemes;
use crate::config::watch;
use crate::config::{
    Action, BellConfig, Cli, Colors, Config, Highlight, Insets, Notifications, PALETTE_ACTIONS,
    TabBarConfig, TabBarShow, TabStyle, TabTitle, mods, normalize_key,
};
use crate::i18n;
use crate::layout::{Axis, Divider, Layout, PaneId, Rect, Shape};
use crate::pty::Pty;
use crate::renderer::font::FontSet;
use crate::renderer::{Mark, MarkKind, PaneView, Renderer};
use crate::terminal::input;
use crate::terminal::{MouseMode, Point, Selection, Terminal};
use crate::ui::copymode::{CopyMode, Outcome};
use crate::ui::palette::{self, Entry, Item, Palette};
use crate::ui::quickselect::{Pick, QuickSelect};
use crate::ui::tabbar;
use crate::ui::tree::{self, FileTree};

#[derive(Debug)]
pub enum UserEvent {
    /// New PTY output has been processed for a pane.
    Wakeup(PaneId),
    ChildExited(PaneId),
    /// The configuration file changed on disk.
    ConfigChanged,
    /// Something drawn changed off the main thread (file tree git status).
    Redraw,
    /// A desktop notification for this pane was clicked.
    FocusPane(PaneId),
}

pub struct App {
    cli: Cli,
    config_path: PathBuf,
    config: Config,
    /// Owns the Lua functions referenced by `config.keys`.
    lua: Lua,
    /// Errors and warnings from loading the config at startup, shown once
    /// the window exists.
    startup_messages: Vec<String>,
    proxy: EventLoopProxy<UserEvent>,
    state: Option<State>,
}

/// A tab: its own pane layout. The current tab's fields live in [`State`].
struct Tab {
    layout: Layout,
    active: PaneId,
    zoomed: bool,
    /// A name given with rename_tab; `None` shows the pane's title.
    title: Option<String>,
    /// Typed input goes to every pane of the tab (toggle_broadcast).
    broadcast: bool,
}

impl Tab {
    fn placeholder() -> Self {
        Self {
            layout: Layout::new(0),
            active: 0,
            zoomed: false,
            title: None,
            broadcast: false,
        }
    }
}

/// An open scrollback search.
struct Search {
    pane: PaneId,
    query: String,
    matches: Vec<(Point, Point)>,
    current: Option<usize>,
    updated: Instant,
}

/// A terminal with its own shell.
struct Pane {
    term: Arc<Mutex<Terminal>>,
    pty: Pty,
    writer: File,
    wakeup_pending: Arc<AtomicBool>,
    /// Where the grid is drawn, in physical pixels.
    rect: Rect,
    /// History lines dropped when freed memory was last handed back.
    trimmed_at: std::cell::Cell<u64>,
}

impl Pane {
    fn term(&self) -> MutexGuard<'_, Terminal> {
        // A reader-thread panic poisons this lock. Recovering keeps the
        // window up; the panic itself is already in the crash log.
        self.term
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Err(e) = self.writer.write_all(bytes) {
            log::warn!("failed to write to PTY: {e}");
        }
    }
}

struct State {
    window: Arc<Window>,
    renderer: Renderer,
    proxy: EventLoopProxy<UserEvent>,
    panes: HashMap<PaneId, Pane>,
    layout: Layout,
    /// The pane with keyboard focus.
    active: PaneId,
    next_id: PaneId,
    /// Only the active pane is shown, filling the window.
    zoomed: bool,
    /// The current tab's name from rename_tab.
    tab_title: Option<String>,
    /// Typed input goes to every pane of the current tab.
    broadcast: bool,
    /// The tab name being typed after rename_tab.
    renaming: Option<String>,
    /// Window margin, in physical pixels. A side of 0 is flush with the window.
    window_pad: Insets,
    /// Padding inside every pane's grid, in physical pixels.
    pane_pad: Insets,
    /// Extra padding around the active pane, in physical pixels.
    active_pad: Insets,
    /// Padding inside the file tree, in physical pixels.
    tree_pad: Insets,
    /// Padding on each side of the tree's divider. `None` uses `split_padding`.
    tree_gap: Option<f32>,
    /// Divider line thickness and the space on each side of it, in
    /// physical pixels.
    split_width: f32,
    split_padding: f32,
    active_border: f32,
    highlight: Highlight,
    /// The background highlight covers the focused pane's padding as well
    /// as its grid.
    highlight_ignores_padding: bool,
    tab_bar_config: TabBarConfig,
    modifiers: ModifiersState,
    focused: bool,
    cursor_pos: PhysicalPosition<f64>,
    cursor_icon: CursorIcon,
    /// Divider being dragged with the mouse.
    drag: Option<Drag>,
    /// Anchor of an in-progress mouse selection in the active pane.
    select_anchor: Option<Point>,
    /// Button held while application mouse reporting is active.
    reported_button: Option<u8>,
    last_reported_cell: Option<(usize, usize)>,
    scroll_accum: f64,
    clipboard: Option<arboard::Clipboard>,
    preedit: String,
    font_pt: f32,
    scale: f64,
    ime_area: Option<(i32, i32)>,
    /// The file tree sidebar, when shown.
    tree: Option<FileTree>,
    /// Keys go to the file tree instead of the active pane.
    tree_focused: bool,
    tree_config: crate::config::TreeConfig,
    /// When the tree last followed the active pane's directory.
    tree_synced: Instant,
    /// Last click in the tree, to detect double clicks.
    tree_click: Option<(usize, Instant)>,
    search: Option<Search>,
    /// Link under the pointer while alt is held: pane, target and cells.
    hover: Option<(PaneId, String, Point, Point)>,
    /// An alt+click opened a link; its release is not passed on.
    swallow_release: bool,
    /// Every tab; the entry at `tab_index` is a placeholder for the current
    /// one, whose layout is in `layout`/`active`/`zoomed`.
    tabs: Vec<Tab>,
    tab_index: usize,
    /// Tab bar cells covered by each tab, for clicks.
    tab_spans: Vec<(usize, usize)>,
    /// Panes in other tabs that rang the bell; their tab shows a mark.
    rang: std::collections::HashSet<PaneId>,
    /// The pane flashing for a bell, until when.
    bell_flash: Option<(PaneId, Instant)>,
    /// Start of the bar-cursor blink cycle.
    cursor_epoch: Instant,
    /// Whether the blinking bar is currently drawn.
    cursor_blink_on: bool,
    /// When `bell.command` last ran.
    bell_command_at: Option<Instant>,
    hostname: String,
    /// The config file this window was started with, for new windows.
    config_path: PathBuf,
    /// Which optional features are on. Reloaded with the config.
    features: crate::config::Features,
    /// Started with `-e`, so the layout wasn't restored and isn't saved over
    /// the one a normal window left.
    one_off: bool,
    /// Asking whether to close the window.
    confirm_quit: bool,
    /// The user confirmed closing; the event loop exits.
    quit: bool,
    copy_mode: Option<CopyMode>,
    quick_select: Option<QuickSelect>,
    palette: Option<Palette>,
    /// A color scheme picked in the palette, applied by the app.
    pending_scheme: Option<String>,
    /// A language picked in the palette, written into config.lua.
    pending_language: Option<String>,
    /// Strings for kodama's own UI. Only the selected language file is loaded.
    lang: i18n::Language,
    /// Where language files are looked up, user directory first.
    lang_dirs: Vec<std::path::PathBuf>,
}

fn pt_to_px(pt: f32, scale: f64) -> f32 {
    pt * 96.0 / 72.0 * scale as f32
}

fn load_config(path: &Path, cli: &Cli) -> Result<(Config, Lua, Vec<String>), String> {
    let loaded = lua::load(path)?;
    let mut config = loaded.config;
    config.apply_cli(cli);
    Ok((config, loaded.lua, loaded.warnings))
}

impl App {
    pub fn new(cli: Cli, proxy: EventLoopProxy<UserEvent>) -> Self {
        let config_path = cli.config.clone().unwrap_or_else(lua::default_config_path);
        let (config, lua, startup_messages) = match load_config(&config_path, &cli) {
            Ok((config, lua, warnings)) => {
                let messages: Vec<String> = warnings
                    .into_iter()
                    .map(|w| format!("config: {w}"))
                    .collect();
                messages.iter().for_each(|m| log::warn!("{m}"));
                (config, lua, messages)
            }
            Err(e) => {
                eprintln!("kodama: config error: {e}");
                let mut config = Config::default();
                config.apply_cli(&cli);
                (config, Lua::new(), vec![format!("config error: {e}")])
            }
        };
        log::info!("config: {}", config_path.display());
        let watcher_proxy = proxy.clone();
        watch::spawn(config_path.clone(), move || {
            watcher_proxy.send_event(UserEvent::ConfigChanged).is_ok()
        });
        Self {
            cli,
            config_path,
            config,
            lua,
            startup_messages,
            proxy,
            state: None,
        }
    }

    fn reload_config(&mut self) {
        match load_config(&self.config_path, &self.cli) {
            Ok((config, lua, warnings)) => {
                if let Some(state) = &mut self.state {
                    let lang = i18n::load(&config.language, &state.lang_dirs);
                    state.apply_config(&self.config, &config);
                    state.lang = lang;
                    if let Some(tree) = &mut state.tree {
                        tree.invalidate();
                    }
                    for w in &warnings {
                        log::warn!("config: {w}");
                        state.show_message(&format!("config: {w}"));
                    }
                }
                self.config = config;
                self.lua = lua;
                log::info!("config reloaded");
            }
            Err(e) => {
                log::warn!("config error: {e}");
                if let Some(state) = &mut self.state {
                    state.show_message(&format!("config error: {e}"));
                }
            }
        }
    }

    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<State, String> {
        let mut attrs = Window::default_attributes()
            .with_title("kodama")
            .with_inner_size(LogicalSize::new(960.0, 600.0))
            .with_transparent(true);
        #[cfg(target_os = "linux")]
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs = WindowAttributesExtWayland::with_name(attrs, "kodama", "kodama");
        }
        // Wayland compositors take the icon from kodama.desktop instead.
        #[cfg(target_os = "linux")]
        if !winit::platform::wayland::ActiveEventLoopExtWayland::is_wayland(event_loop) {
            attrs = attrs.with_window_icon(window_icon());
        }
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        window.set_ime_allowed(true);
        window.set_cursor(CursorIcon::Text);

        let config = &self.config;
        let scale = window.scale_factor();
        let font = FontSet::new(
            &config.font.family,
            pt_to_px(config.font.size, scale),
            &config.font.fallback,
        );
        let window_pad = scale_insets(config.window.insets(), scale);
        let mut renderer = pollster::block_on(Renderer::new(window.clone(), font));
        renderer.ligatures = config.font.ligatures;
        renderer.opacity = config.window.opacity;
        let background_error =
            set_background_image(&mut renderer, &config.window, &self.config_path).err();
        let (cw, ch) = renderer.cell_size();
        // Floating window managers honour this; tiling ones resize us anyway.
        let _ = window.request_inner_size(PhysicalSize::new(
            (config.window.columns as f32 * cw + window_pad.left + window_pad.right) as u32,
            (config.window.rows as f32 * ch + window_pad.top + window_pad.bottom) as u32,
        ));

        let (w, h) = renderer.surface_size();
        let rect = content_area(w, h, window_pad);
        let lang_dirs = i18n::lang_dirs(&self.config_path);
        let lang = i18n::load(&config.language, &lang_dirs);
        let saved = if self.cli.command.is_none() && config.features.restore_layout {
            crate::session::load(&self.config_path)
                .map_err(|e| log::warn!("not restoring the layout: {e}"))
                .ok()
                .flatten()
        } else {
            None
        };
        // The first shell becomes the restored current tab's active pane, so
        // restoring starts no shell only to close it again.
        let saved_pane = saved.as_ref().and_then(|s| s.tabs.get(s.current)).map(|t| {
            (
                t.dirs.get(&t.active).cloned(),
                t.screens.get(&t.active).cloned(),
            )
        });
        let (cwd, screen) = saved_pane.unwrap_or((None, None));
        let cwd = cwd.or_else(|| config.shell.working_directory.clone());
        let pane = spawn_pane(
            &renderer,
            &self.proxy,
            config,
            cwd.as_deref(),
            screen.as_deref(),
            rect,
            1,
        )?;
        for message in self.startup_messages.iter().chain(&background_error) {
            pane.term().feed(error_banner(message).as_bytes());
        }

        let clipboard = arboard::Clipboard::new()
            .map_err(|e| log::warn!("clipboard unavailable: {e}"))
            .ok();

        let mut state = State {
            window,
            renderer,
            proxy: self.proxy.clone(),
            panes: HashMap::from([(1, pane)]),
            layout: Layout::new(1),
            active: 1,
            next_id: 2,
            zoomed: false,
            tab_title: None,
            broadcast: false,
            renaming: None,
            window_pad,
            pane_pad: scale_insets(config.pane.insets(), scale),
            active_pad: scale_insets(config.pane.active_insets(), scale),
            tree_pad: scale_insets(config.tree.insets(), scale),
            tree_gap: config.tree.divider_gap().map(|g| scaled(g, scale)),
            split_width: scaled(config.split.width, scale),
            split_padding: scaled(config.split.padding, scale),
            active_border: scaled(config.split.active_border, scale),
            highlight: config.split.highlight,
            highlight_ignores_padding: config.split.highlight_ignores_padding,
            tab_bar_config: config.tab_bar.clone(),
            modifiers: ModifiersState::empty(),
            focused: true,
            cursor_pos: PhysicalPosition::new(0.0, 0.0),
            cursor_icon: CursorIcon::Text,
            drag: None,
            select_anchor: None,
            reported_button: None,
            last_reported_cell: None,
            scroll_accum: 0.0,
            clipboard,
            preedit: String::new(),
            font_pt: self.config.font.size,
            scale,
            ime_area: None,
            tree: None,
            tree_focused: false,
            tree_config: config.tree.clone(),
            tree_synced: Instant::now(),
            tree_click: None,
            search: None,
            hover: None,
            swallow_release: false,
            tabs: vec![Tab::placeholder()],
            tab_index: 0,
            tab_spans: Vec::new(),
            rang: Default::default(),
            bell_flash: None,
            cursor_epoch: Instant::now(),
            cursor_blink_on: true,
            bell_command_at: None,
            confirm_quit: false,
            quit: false,
            copy_mode: None,
            quick_select: None,
            palette: None,
            pending_scheme: None,
            pending_language: None,
            lang,
            lang_dirs,
            hostname: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|h| h.trim().to_string())
                .unwrap_or_default(),
            config_path: self.config_path.clone(),
            features: config.features,
            one_off: self.cli.command.is_some(),
        };
        if let Some(saved) = saved {
            match state.restore_session(saved, config) {
                Ok(()) if !state.panes.contains_key(&1) => {
                    for message in self.startup_messages.iter().chain(&background_error) {
                        state.show_message(message);
                    }
                }
                Ok(()) => {}
                Err(e) => log::warn!("not restoring the layout: {e}"),
            }
        }
        // The first pane was sized to the content box. Pane padding shrinks
        // its grid; with none, the size already matches.
        if !state.pane_pad.is_zero() || !state.active_pad.is_zero() {
            state.relayout();
        }
        Ok(state)
    }
}

/// Logical to physical pixels, rounded so cells land on whole pixels.
fn scaled(logical: f32, scale: f64) -> f32 {
    (logical.max(0.0) * scale as f32).round()
}

fn scale_insets(insets: Insets, scale: f64) -> Insets {
    Insets {
        top: scaled(insets.top, scale),
        right: scaled(insets.right, scale),
        bottom: scaled(insets.bottom, scale),
        left: scaled(insets.left, scale),
    }
}

/// The lines drawn for `dividers`: centred in their gap, and stretched
/// lengthwise to the edge of `bounds` (the window, or up to the file tree's
/// divider) or through the gap of the divider they end on, so every line
/// joins up.
fn divider_lines(
    dividers: &[Divider],
    area: Rect,
    bounds: Rect,
    width: f32,
    padding: f32,
) -> Vec<Rect> {
    let reach = padding + width;
    let stretch = |start: f32, len: f32, lo: f32, hi: f32, (min, max): (f32, f32)| {
        let s = if start <= lo + 0.5 {
            min
        } else {
            (start - reach).max(min)
        };
        let e = if start + len >= hi - 0.5 {
            max
        } else {
            (start + len + reach).min(max)
        };
        (s, e - s)
    };
    dividers
        .iter()
        .map(|d| {
            let r = d.rect;
            match d.axis {
                Axis::Horizontal => {
                    let (y, h) = stretch(
                        r.y,
                        r.h,
                        area.y,
                        area.y + area.h,
                        (bounds.y, bounds.y + bounds.h),
                    );
                    Rect {
                        x: (r.x + (r.w - width) / 2.0).round(),
                        y,
                        w: width,
                        h,
                    }
                }
                Axis::Vertical => {
                    let (x, w) = stretch(
                        r.x,
                        r.w,
                        area.x,
                        area.x + area.w,
                        (bounds.x, bounds.x + bounds.w),
                    );
                    Rect {
                        x,
                        y: (r.y + (r.h - width) / 2.0).round(),
                        w,
                        h: width,
                    }
                }
            }
        })
        .collect()
}

/// Left edge of a `thickness` line centred on the cut before slice `index`.
/// Two slices meet on this edge, which is also where a half pane split
/// draws its divider when the two spans share a centre.
fn slice_line_x(origin: f32, width: f32, index: usize, count: usize, thickness: f32) -> f32 {
    let mid = origin + width * index as f32 / count.max(1) as f32;
    (mid - thickness / 2.0).round()
}

/// Slice `index` of `count` equal parts of `[origin, origin + width)`.
/// Neighbours meet on a whole pixel, the centred line between them.
fn equal_slice(origin: f32, width: f32, index: usize, count: usize, thickness: f32) -> (f32, f32) {
    let count = count.max(1);
    let start = if index == 0 {
        origin
    } else {
        slice_line_x(origin, width, index, count, thickness)
    };
    let end = if index + 1 >= count {
        origin + width
    } else {
        slice_line_x(origin, width, index + 1, count, thickness)
    };
    (start, (end - start).max(0.0))
}

/// Which equal slice of `[origin, origin + width)` contains `x`.
fn slice_index(x: f32, origin: f32, width: f32, count: usize, thickness: f32) -> Option<usize> {
    if count == 0 || width <= 0.0 {
        return None;
    }
    let mut index = 0;
    for i in 1..count {
        if x < slice_line_x(origin, width, i, count, thickness) {
            break;
        }
        index = i;
    }
    Some(index)
}

/// `span` can be split into two cells of `cell` with `gap` between them.
fn split_has_room(span: f32, gap: f32, cell: f32) -> bool {
    cell.is_finite() && cell > 0.0 && span.is_finite() && span >= gap + cell * 2.0
}

/// Hands freed heap pages back to the system. glibc keeps them after large
/// frees (a closed pane's history, a rewrapped one), so they would otherwise
/// stay in kodama's resident memory.
fn release_freed_memory() {
    #[cfg(target_env = "gnu")]
    // SAFETY: malloc_trim only releases free memory and is thread-safe.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn window_icon() -> Option<winit::window::Icon> {
    let (rgba, w, h) =
        crate::renderer::font::decode_png(include_bytes!("../assets/kodama-64.png"))?;
    winit::window::Icon::from_rgba(rgba, w as u32, h as u32).ok()
}

/// What a mouse drag is resizing.
enum Drag {
    /// The divider between two panes.
    Split(Divider),
    /// The edge between the file tree and the panes.
    Tree,
    /// A tab being dragged along the bar. `moved` is set once it leaves
    /// the click, so a plain click still just switches.
    Tab { start: (f32, f32), moved: bool },
    /// A pane being dragged onto another. Alt starts it; releasing over a
    /// different pane swaps the two.
    Pane {
        id: PaneId,
        start: (f32, f32),
        moved: bool,
    },
}

/// Where a grid in `rect` starts and stops before its own padding.
/// `divider` is `(x, right)` of the file tree's divider when the tree is
/// shown. An edge that meets the content box stops there. The content box
/// is already inset by the window margin, so a side whose margin is 0 is
/// flush with the window. With no left margin, the tree and the pane beside
/// it also meet the divider, across `split_padding`. With a margin they keep
/// that gap, so every edge has the same space. The top stays at `rect.y`.
fn grid_span(
    rect: Rect,
    content: Rect,
    divider: Option<(f32, f32)>,
    split_padding: f32,
) -> (f32, f32, f32) {
    let divider = divider.filter(|_| content.x <= 0.5);
    let left = if rect.x <= content.x + 1.0 {
        content.x
    } else if let Some((div_x, div_right)) = divider
        && rect.x + 1.0 >= div_x
        && rect.x <= div_right + split_padding + 1.0
    {
        div_right
    } else {
        rect.x
    };
    let right = if rect.x + rect.w >= content.x + content.w - 1.0 {
        content.x + content.w
    } else if let Some((div_x, _)) = divider
        && (rect.x + rect.w - (div_x - split_padding)).abs() <= 1.0
    {
        div_x
    } else {
        rect.x + rect.w
    };
    let bottom = if rect.y + rect.h >= content.y + content.h - 1.0 {
        content.y + content.h
    } else {
        rect.y + rect.h
    };
    (left, right, bottom)
}

/// The drawn grid after `pad` is taken off each side of the flush span.
/// A side of 0 stays flush with the window or the divider.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GridBox {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl GridBox {
    fn rect(self) -> Rect {
        Rect {
            x: self.left,
            y: self.top,
            w: (self.right - self.left).max(1.0),
            h: (self.bottom - self.top).max(1.0),
        }
    }
}

fn inset_box(left: f32, top: f32, right: f32, bottom: f32, pad: Insets) -> GridBox {
    let left = left + pad.left;
    let top = top + pad.top;
    let right = right - pad.right;
    let bottom = bottom - pad.bottom;
    GridBox {
        left,
        top,
        right: right.max(left + 1.0),
        bottom: bottom.max(top + 1.0),
    }
}

fn content_area(width: f32, height: f32, pad: Insets) -> Rect {
    Rect {
        x: pad.left,
        y: pad.top,
        w: (width - pad.left - pad.right).max(1.0),
        h: (height - pad.top - pad.bottom).max(1.0),
    }
}

/// Background highlight of a focused pane that ignores padding. `outer` is
/// `(left, right, bottom)` from `grid_span`; the top starts at `rect.y`.
/// An edge on the content box continues to the window. An edge against a
/// divider gap stops at the line, so the colour covers this pane's side of
/// the gap and not the pane beyond it. `tab` keeps the fill under the tab
/// bar, but only when this pane spans the whole bar: a side-by-side pane
/// keeps its own top, so its frame does not run up to the tab beside the
/// divider. Inner padding is not applied: the fill is the whole area.
fn highlight_extent(
    rect: Rect,
    content: Rect,
    window: (f32, f32),
    outer: (f32, f32, f32),
    gaps: &[(Axis, Rect)],
    line_width: f32,
    tab: Option<Rect>,
) -> Rect {
    let (mut left, mut right, mut bottom) = outer;
    let mut top = rect.y;
    if left <= content.x + 1.0 {
        left = 0.0;
    }
    if right >= content.x + content.w - 1.0 {
        right = window.0;
    }
    if top <= content.y + 1.0 {
        top = 0.0;
    }
    if bottom >= content.y + content.h - 1.0 {
        bottom = window.1;
    }
    let mut below_gap = false;
    for (axis, gap) in gaps {
        match axis {
            Axis::Horizontal => {
                let line_x = (gap.x + (gap.w - line_width) / 2.0).round();
                if (rect.x + rect.w - gap.x).abs() <= 1.0 {
                    right = line_x;
                }
                if (rect.x - (gap.x + gap.w)).abs() <= 1.0 {
                    left = line_x + line_width;
                }
            }
            Axis::Vertical => {
                let line_y = (gap.y + (gap.h - line_width) / 2.0).round();
                if (rect.y + rect.h - gap.y).abs() <= 1.0 {
                    bottom = line_y;
                }
                if (rect.y - (gap.y + gap.h)).abs() <= 1.0 {
                    top = line_y + line_width;
                    below_gap = true;
                }
            }
        }
    }
    // Only a pane as wide as the tab bar owns the strip under it. A narrower
    // one would draw its edge up to the tab, past where the divider starts.
    if !below_gap
        && let Some(bar) = tab
        && left <= bar.x + 1.0
        && right >= bar.x + bar.w - 1.0
    {
        top = bar.y + bar.h;
    }
    Rect {
        x: left,
        y: top,
        w: (right - left).max(1.0),
        h: (bottom - top).max(1.0),
    }
}

/// A border of `width` along the inside of `extent`, and the area it
/// encloses. The edges are only the stroke: nothing else in that strip is
/// part of the fill. `width` of 0 leaves the whole extent as the fill.
fn border_frame(extent: Rect, width: f32) -> (Rect, Vec<Rect>) {
    if width <= 0.0 {
        return (extent, Vec::new());
    }
    let width = width.min(extent.w / 2.0).min(extent.h / 2.0);
    let fill = Rect {
        x: extent.x + width,
        y: extent.y + width,
        w: (extent.w - 2.0 * width).max(1.0),
        h: (extent.h - 2.0 * width).max(1.0),
    };
    (
        fill,
        vec![
            Rect {
                x: extent.x,
                y: extent.y,
                w: extent.w,
                h: width,
            },
            Rect {
                x: extent.x,
                y: extent.y + extent.h - width,
                w: extent.w,
                h: width,
            },
            Rect {
                x: extent.x,
                y: extent.y,
                w: width,
                h: extent.h,
            },
            Rect {
                x: extent.x + extent.w - width,
                y: extent.y,
                w: width,
                h: extent.h,
            },
        ],
    )
}

/// Border of `width` immediately outside `grid`, in the padding around it.
/// The fill is the grid itself, so a background highlight does not cover
/// that padding. No edges when `width` is 0.
fn frame_around(grid: Rect, width: f32) -> (Rect, Vec<Rect>) {
    if width <= 0.0 {
        return (grid, Vec::new());
    }
    let x = grid.x - width;
    let y = grid.y - width;
    let w = grid.w + 2.0 * width;
    let h = grid.h + 2.0 * width;
    (
        grid,
        vec![
            Rect { x, y, w, h: width },
            Rect {
                x,
                y: y + h - width,
                w,
                h: width,
            },
            Rect { x, y, w: width, h },
            Rect {
                x: x + w - width,
                y,
                w: width,
                h,
            },
        ],
    )
}

fn configure_terminal(term: &mut Terminal, config: &Config) {
    term.report_fg = config.colors.foreground;
    term.report_bg = config.colors.background;
    term.set_default_cursor_shape(config.cursor.style.into());
    term.set_scrollback_limit(config.scrollback);
    term.trim_leading_blank_lines = config.trim_leading_blank_lines;
}

fn spawn_pane(
    renderer: &Renderer,
    proxy: &EventLoopProxy<UserEvent>,
    config: &Config,
    cwd: Option<&str>,
    screen: Option<&str>,
    rect: Rect,
    id: PaneId,
) -> Result<Pane, String> {
    let (cols, rows) = renderer.grid_size(rect.w, rect.h);
    let (cw, ch) = renderer.cell_size();
    let mut term = Terminal::new(cols, rows, config.scrollback);
    term.cell_px = (cw, ch);
    configure_terminal(&mut term, config);
    // Before the shell starts, so its prompt is printed under the old text.
    if let Some(text) = screen.filter(|text| !text.is_empty()) {
        term.show_transcript(text);
    }
    let term = Arc::new(Mutex::new(term));

    let program = config.shell_program();
    let pty = Pty::spawn(
        &program,
        &config.shell.args,
        &config.env,
        cwd,
        cols as u16,
        rows as u16,
    )
    .map_err(|e| format!("failed to spawn {program}: {e}"))?;
    pty.resize(cols as u16, rows as u16, cw as u16, ch as u16);
    let writer = pty.writer().map_err(|e| e.to_string())?;
    let reader = pty.reader().map_err(|e| e.to_string())?;
    let responder = pty.writer().map_err(|e| e.to_string())?;

    let wakeup_pending = Arc::new(AtomicBool::new(false));
    spawn_reader(
        id,
        reader,
        responder,
        term.clone(),
        wakeup_pending.clone(),
        proxy.clone(),
    )
    .map_err(|e| format!("failed to spawn PTY reader: {e}"))?;
    Ok(Pane {
        term,
        pty,
        writer,
        wakeup_pending,
        rect,
        trimmed_at: std::cell::Cell::new(0),
    })
}

/// Highlight around pane `rect`: the area inside a `width`-thick frame
/// centred in the `padding` that separates the pane from the divider, and the
/// frame's four edges (none when `width` is 0).
fn active_frame(rect: Rect, width: f32, padding: f32) -> (Rect, Vec<Rect>) {
    let inset = ((padding - width) / 2.0).max(0.0).floor();
    let fill = Rect {
        x: rect.x - inset,
        y: rect.y - inset,
        w: rect.w + 2.0 * inset,
        h: rect.h + 2.0 * inset,
    };
    if width <= 0.0 {
        return (fill, Vec::new());
    }
    let (x, y) = (rect.x - inset - width, rect.y - inset - width);
    let (w, h) = (
        rect.w + 2.0 * (inset + width),
        rect.h + 2.0 * (inset + width),
    );
    (
        fill,
        vec![
            Rect { x, y, w, h: width },
            Rect {
                x,
                y: y + h - width,
                w,
                h: width,
            },
            Rect { x, y, w: width, h },
            Rect {
                x: x + w - width,
                y,
                w: width,
                h,
            },
        ],
    )
}

fn remember_dirs(shape: &Shape, dirs: &mut HashMap<u32, String>, state: &State) {
    match shape {
        Shape::Leaf(id) => {
            if let Some(dir) = state.pane_cwd(*id) {
                dirs.insert(*id, dir.to_string_lossy().into_owned());
            }
        }
        Shape::Split { first, second, .. } => {
            remember_dirs(first, dirs, state);
            remember_dirs(second, dirs, state);
        }
    }
}

fn remember_screens(shape: &Shape, screens: &mut HashMap<u32, String>, state: &State) {
    match shape {
        Shape::Leaf(id) => {
            if let Some(pane) = state.panes.get(id) {
                let text = pane.term().transcript();
                if !text.is_empty() {
                    screens.insert(*id, text);
                }
            }
        }
        Shape::Split { first, second, .. } => {
            remember_screens(first, screens, state);
            remember_screens(second, screens, state);
        }
    }
}

/// What every restored pane is started with.
struct Spawn<'a> {
    renderer: &'a Renderer,
    proxy: &'a EventLoopProxy<UserEvent>,
    config: &'a Config,
    rect: Rect,
    /// A saved leaf that takes over an already running pane instead of a new
    /// shell. Used once.
    reuse: std::cell::Cell<Option<(u32, PaneId)>>,
}

/// Spawns a shell for every leaf, remapping saved ids onto fresh ones.
fn spawn_shape(
    shape: Shape,
    dirs: &HashMap<u32, String>,
    screens: &HashMap<u32, String>,
    ids: &mut HashMap<u32, PaneId>,
    next_id: &mut PaneId,
    spawned: &mut HashMap<PaneId, Pane>,
    spawn: &Spawn<'_>,
) -> Result<Shape, String> {
    match shape {
        Shape::Leaf(old) => {
            if let Some((leaf, id)) = spawn.reuse.get()
                && leaf == old
            {
                spawn.reuse.set(None);
                ids.insert(old, id);
                return Ok(Shape::Leaf(id));
            }
            let id = *next_id;
            *next_id += 1;
            let cwd = dirs.get(&old).map(String::as_str);
            let screen = screens.get(&old).map(String::as_str);
            let pane = spawn_pane(
                spawn.renderer,
                spawn.proxy,
                spawn.config,
                cwd,
                screen,
                spawn.rect,
                id,
            )?;
            spawned.insert(id, pane);
            ids.insert(old, id);
            Ok(Shape::Leaf(id))
        }
        Shape::Split {
            axis,
            ratio,
            first,
            second,
        } => Ok(Shape::Split {
            axis,
            ratio,
            first: Box::new(spawn_shape(
                *first, dirs, screens, ids, next_id, spawned, spawn,
            )?),
            second: Box::new(spawn_shape(
                *second, dirs, screens, ids, next_id, spawned, spawn,
            )?),
        }),
    }
}

/// Opens a link with the desktop's handler.
fn open_link(target: &str) {
    const SCHEMES: [&str; 6] = [
        "http://", "https://", "mailto:", "ftp://", "file://", "ssh://",
    ];
    if !SCHEMES.iter().any(|s| target.starts_with(s)) {
        log::warn!("not opening link with an unknown scheme: {target}");
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    spawn_detached(opener, &[target]);
}

/// Loads window.background_image into `renderer`, or clears it when unset.
fn set_background_image(
    renderer: &mut Renderer,
    window: &crate::config::WindowConfig,
    config_path: &Path,
) -> Result<(), String> {
    renderer.background_image_opacity = window.background_image_opacity;
    renderer.background_image_fit = window.background_image_fit;
    let Some(name) = &window.background_image else {
        renderer.set_background_image(None);
        return Ok(());
    };
    let path = match name.strip_prefix("~/") {
        Some(rest) => std::env::home_dir().unwrap_or_default().join(rest),
        None => config_path.parent().unwrap_or(Path::new(".")).join(name),
    };
    let image = std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|data| {
            crate::renderer::font::decode_png(&data).ok_or_else(|| "not a PNG image".to_string())
        });
    match image {
        Ok(image) => {
            renderer.set_background_image(Some(image));
            Ok(())
        }
        Err(e) => {
            renderer.set_background_image(None);
            Err(format!("background_image {}: {e}", path.display()))
        }
    }
}

/// A tab label for directory `dir`: its last component, `~` for the home
/// directory and `/` for the root.
fn dir_label(dir: &Path) -> String {
    if std::env::home_dir().is_some_and(|home| home == dir) {
        return "~".to_string();
    }
    dir.file_name()
        .map_or_else(|| "/".to_string(), |n| n.to_string_lossy().into_owned())
}

/// Writes `text` to a new file only the user can read, in the temp
/// directory.
fn write_private_temp(kind: &str, text: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path =
        std::env::temp_dir().join(format!("kodama-{kind}-{}-{stamp}.txt", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(text.as_bytes())?;
    Ok(path)
}

/// Runs `program` with no terminal attached and reaps it in the background.
fn spawn_detached(program: &str, args: &[&str]) {
    match std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
        }
        Err(e) => log::warn!("failed to run {program}: {e}"),
    }
}

/// A one-row terminal for bars (tabs, search) drawn with `bg`/`fg`.
fn bar_terminal(cols: usize, bg: [u8; 3], fg: [u8; 3]) -> Terminal {
    let mut t = Terminal::new(cols, 1, 0);
    let ([r, g, b], [fr, fg, fb]) = (bg, fg);
    t.feed(
        format!("\x1b[?7l\x1b[?25l\x1b[48;2;{r};{g};{b}m\x1b[38;2;{fr};{fg};{fb}m\x1b[2K")
            .as_bytes(),
    );
    t
}

fn error_banner(message: &str) -> String {
    format!(
        "\x1b[1;31m[kodama]\x1b[0m {}\r\n",
        message.replace('\n', "\r\n")
    )
}

fn key_name(event: &KeyEvent) -> String {
    match event.key_without_modifiers() {
        Key::Character(s) => normalize_key(&s),
        Key::Named(named) => normalize_key(&format!("{named:?}")),
        _ => String::new(),
    }
}

/// The first key bound to action `name`, as `ctrl+shift+x`.
fn binding_label(config: &Config, name: &str) -> String {
    let Some(action) = Action::from_name(name) else {
        return String::new();
    };
    let want = format!("{action:?}");
    let Some(b) = config
        .keys
        .iter()
        .find(|b| format!("{:?}", b.action) == want)
    else {
        return String::new();
    };
    let mut out = String::new();
    for (bit, label) in [
        (mods::CTRL, "ctrl+"),
        (mods::SHIFT, "shift+"),
        (mods::ALT, "alt+"),
        (mods::SUPER, "super+"),
    ] {
        if b.mods & bit != 0 {
            out.push_str(label);
        }
    }
    out.push_str(b.key.strip_prefix("arrow").unwrap_or(&b.key));
    out
}

fn mod_bits(state: ModifiersState) -> u8 {
    let mut bits = 0;
    if state.shift_key() {
        bits |= mods::SHIFT;
    }
    if state.control_key() {
        bits |= mods::CTRL;
    }
    if state.alt_key() {
        bits |= mods::ALT;
    }
    if state.super_key() {
        bits |= mods::SUPER;
    }
    bits
}

fn spawn_reader(
    id: PaneId,
    mut reader: File,
    mut writer: File,
    term: Arc<Mutex<Terminal>>,
    wakeup_pending: Arc<AtomicBool>,
    proxy: EventLoopProxy<UserEvent>,
) -> std::io::Result<()> {
    thread::Builder::new()
        .name(format!("pty-reader-{id}"))
        .spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // EIO: the child closed the slave side.
                    Err(_) => break,
                };
                let responses = {
                    let mut term = term.lock().unwrap();
                    term.feed(&buf[..n]);
                    std::mem::take(&mut term.responses)
                };
                if !responses.is_empty() {
                    let _ = writer.write_all(&responses);
                }
                if !wakeup_pending.swap(true, Ordering::AcqRel) {
                    let _ = proxy.send_event(UserEvent::Wakeup(id));
                }
            }
            let _ = proxy.send_event(UserEvent::ChildExited(id));
        })
        .map(|_| ())
}

impl State {
    fn active_pane(&mut self) -> &mut Pane {
        self.panes
            .get_mut(&self.active)
            .expect("active pane exists")
    }

    fn term(&self) -> MutexGuard<'_, Terminal> {
        self.term_of(self.active)
    }

    fn term_of(&self, id: PaneId) -> MutexGuard<'_, Terminal> {
        self.panes[&id].term()
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        self.active_pane().write(bytes);
    }

    /// Writes what the user typed or pasted: to every pane of the tab while
    /// broadcasting, else to the focused one.
    fn write_input(&mut self, bytes: &[u8]) {
        if !self.broadcast {
            return self.write_pty(bytes);
        }
        for id in self.layout.ids() {
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.write(bytes);
            }
        }
    }

    // ---- layout -----------------------------------------------------------

    /// The window minus its margin.
    fn content_rect(&self) -> Rect {
        let (w, h) = self.renderer.surface_size();
        content_area(w, h, self.window_pad)
    }

    /// The tab bar: by default shown while there is more than one tab. A
    /// band from the window's top edge across the panes, through the window
    /// margin to the right edge; the file tree keeps the full height. The
    /// tabs fill it, with their labels on the middle row (`tab_text_rect`).
    fn tab_bar_rect(&self) -> Option<Rect> {
        let shown = match self.tab_bar_config.show {
            TabBarShow::Auto => self.tabs.len() > 1,
            TabBarShow::Always => true,
            TabBarShow::Never => false,
        };
        if !shown {
            return None;
        }
        let full = self.content_rect();
        let (_, ch) = self.renderer.cell_size();
        let (win_w, _) = self.renderer.surface_size();
        let x = self.tree_divider().map_or(0.0, |d| d.x + d.w);
        Some(Rect {
            x,
            y: 0.0,
            w: (win_w - x).max(1.0),
            h: (ch + 2.0 * full.y).round(),
        })
    }

    /// The row of the tab bar the labels are drawn on, centred in `band`.
    fn tab_text_rect(&self, band: Rect) -> Rect {
        let (_, ch) = self.renderer.cell_size();
        Rect {
            y: band.y + ((band.h - ch) / 2.0).floor(),
            h: ch,
            ..band
        }
    }

    /// The window minus its margin and the tab bar. Panes and their
    /// dividers start on the bar's bottom edge, with no gap under the tabs.
    fn base_area(&self) -> Rect {
        let full = self.content_rect();
        match self.tab_bar_rect() {
            Some(bar) => {
                let y = bar.y + bar.h;
                Rect {
                    y,
                    h: (full.y + full.h - y).max(1.0),
                    ..full
                }
            }
            None => full,
        }
    }

    /// Padding on each side of the tree's divider.
    fn tree_side_pad(&self) -> f32 {
        self.tree_gap.unwrap_or(self.split_padding)
    }

    /// Flush edges of `rect`: the content box, or the tree's divider.
    /// `(left, right, bottom)`; the top stays at `rect.y`.
    fn outer_span(&self, rect: Rect) -> (f32, f32, f32) {
        let divider = self.tree_divider().map(|d| (d.x, d.x + d.w));
        grid_span(rect, self.base_area(), divider, self.tree_side_pad())
    }

    /// Where the grid of `rect` is drawn once `pad` is taken off.
    fn grid_box(&self, rect: Rect, pad: Insets) -> GridBox {
        let (left, right, bottom) = self.outer_span(rect);
        inset_box(left, rect.y, right, bottom, pad)
    }

    /// Background highlight covering `rect` and the padding around it.
    fn highlight_extent(&self, rect: Rect) -> Rect {
        let mut gaps: Vec<(Axis, Rect)> = if self.zoomed {
            Vec::new()
        } else {
            self.layout
                .dividers(self.area(), self.gap())
                .into_iter()
                .map(|d| (d.axis, d.rect))
                .collect()
        };
        if let Some(t) = self.tree_rect() {
            gaps.push((
                Axis::Horizontal,
                Rect {
                    x: t.x + t.w,
                    w: self.tree_separator(),
                    ..t
                },
            ));
        }
        highlight_extent(
            rect,
            self.content_rect(),
            self.renderer.surface_size(),
            self.outer_span(rect),
            &gaps,
            self.split_width,
            self.tab_bar_rect().filter(|bar| rect.x + rect.w > bar.x),
        )
    }

    /// Pane padding, plus the active pane's extra padding while it has focus.
    fn insets_for_pane(&self, id: PaneId) -> Insets {
        if id == self.active && !self.tree_focused {
            self.pane_pad.add(self.active_pad)
        } else {
            self.pane_pad
        }
    }

    /// Columns and rows of a pane. With no pane padding this is the layout
    /// rect, so a flush grid still stretches across the leftover pixels.
    fn pane_grid_count(&self, id: PaneId, rect: Rect) -> (usize, usize) {
        let pad = self.insets_for_pane(id);
        if pad.is_zero() {
            self.renderer.grid_size(rect.w, rect.h)
        } else {
            let b = self.grid_box(rect, pad);
            self.renderer.grid_size(b.right - b.left, b.bottom - b.top)
        }
    }

    /// Columns and rows of the tree. `tree.width` is still the sidebar's
    /// width in columns; padding only shrinks the grid that has to fit.
    fn tree_grid_count(&self, rect: Rect) -> (usize, usize) {
        let (cw, ch) = self.renderer.cell_size();
        if self.tree_pad.is_zero() {
            (
                (rect.w / cw).floor() as usize,
                (rect.h / ch).floor() as usize,
            )
        } else {
            let b = self.grid_box(rect, self.tree_pad);
            self.renderer.grid_size(b.right - b.left, b.bottom - b.top)
        }
    }

    /// The tree's clickable area. It matches the drawn grid. The gap on the
    /// right of the sidebar stays the drag handle, unless right padding has
    /// already pulled the grid off that edge.
    fn tree_hit_rect(&self) -> Option<Rect> {
        let t = self.tree_rect()?;
        let b = self.grid_box(t, self.tree_pad);
        let right = if self.tree_pad.right > 0.0 {
            b.right
        } else {
            t.x + t.w
        };
        Some(Rect {
            x: b.left,
            y: b.top,
            w: (right - b.left).max(1.0),
            h: (b.bottom - b.top).max(1.0),
        })
    }

    /// Where panes go: the window minus its padding, the tab bar and the
    /// file tree.
    fn area(&self) -> Rect {
        let full = self.base_area();
        match self.tree_rect() {
            Some(t) => {
                let used = t.w + self.tree_separator();
                Rect {
                    x: full.x + used,
                    w: (full.w - used).max(1.0),
                    ..full
                }
            }
            None => full,
        }
    }

    /// The file tree's area, when it is shown.
    fn tree_rect(&self) -> Option<Rect> {
        self.tree.as_ref()?;
        let full = self.content_rect();
        let (cw, _) = self.renderer.cell_size();
        // Leave at least half the window to the panes.
        let width =
            (self.tree_config.width.max(8) as f32 * cw).min((full.w / 2.0 / cw).floor() * cw);
        Some(Rect { w: width, ..full })
    }

    /// The divider line between the file tree and the panes. It runs
    /// through the window margin to the top and bottom edges.
    fn tree_divider(&self) -> Option<Rect> {
        let t = self.tree_rect()?;
        Some(Rect {
            x: t.x + t.w + self.tree_side_pad(),
            y: 0.0,
            w: self.split_width,
            h: self.renderer.surface_size().1.max(1.0),
        })
    }

    /// Whether (`x`, `y`) is on the gap right of the file tree, which
    /// drags to set its width.
    fn tree_handle_at(&self, x: f32, y: f32) -> bool {
        let Some(t) = self.tree_rect() else {
            return false;
        };
        let gap = self.tree_separator();
        let grow = ((8.0 * self.scale as f32 - gap) / 2.0).max(0.0);
        Rect {
            x: t.x + t.w - grow,
            y: t.y,
            w: gap + 2.0 * grow,
            h: t.h,
        }
        .contains(x, y)
    }

    /// Sets the file tree's width from the pointer's position over its edge.
    fn drag_tree_edge(&mut self, x: f32) {
        let Some(t) = self.tree_rect() else {
            return;
        };
        let (cw, _) = self.renderer.cell_size();
        let cols = ((x - t.x - self.tree_separator() / 2.0) / cw).round() as isize;
        self.set_tree_width(cols);
    }

    /// Widest the file tree may be: half the window, but never under 8 columns.
    fn tree_col_limit(&self) -> usize {
        let (cw, _) = self.renderer.cell_size();
        let full = self.base_area();
        ((full.w / 2.0 / cw).floor() as usize).max(8)
    }

    /// Sets the file tree's width in columns, clamped to 8..=half the window.
    fn set_tree_width(&mut self, cols: isize) {
        let cols = cols.clamp(8, self.tree_col_limit() as isize) as usize;
        if cols != self.tree_config.width {
            self.tree_config.width = cols;
            self.relayout();
        }
    }

    /// Moves the file tree's edge the way a pane divider moves: one step is
    /// 5% of the window width, and at least one column.
    fn resize_tree(&mut self, dir: crate::layout::Direction) {
        if self.tree.is_none() {
            return;
        }
        let (cw, _) = self.renderer.cell_size();
        let step = ((self.base_area().w * 0.05) / cw).round().max(1.0) as isize;
        let delta = match dir {
            crate::layout::Direction::Right => step,
            crate::layout::Direction::Left => -step,
            crate::layout::Direction::Up | crate::layout::Direction::Down => return,
        };
        self.set_tree_width(self.tree_config.width as isize + delta);
    }

    /// Space between panes: the divider line plus its padding on each side.
    fn gap(&self) -> f32 {
        self.split_width + 2.0 * self.split_padding
    }

    /// The pane's layout rect. While zoomed, `pane.rect` is the whole window,
    /// so the split has to be measured on the layout underneath.
    fn unzoomed_rect(&self, id: PaneId) -> Rect {
        if self.zoomed {
            self.layout
                .rects(self.area(), self.gap())
                .into_iter()
                .find(|(pane, _)| *pane == id)
                .map(|(_, rect)| rect)
                .unwrap_or_else(|| self.area())
        } else {
            self.panes
                .get(&id)
                .map(|pane| pane.rect)
                .unwrap_or_else(|| self.area())
        }
    }

    /// Both children of a split need a full cell once the divider is removed.
    fn split_fits(&self, axis: Axis) -> bool {
        let rect = self.unzoomed_rect(self.active);
        let (cw, ch) = self.renderer.cell_size();
        let (span, cell) = match axis {
            Axis::Horizontal => (rect.w, cw),
            Axis::Vertical => (rect.h, ch),
        };
        split_has_room(span, self.gap(), cell)
    }

    /// Pane geometry and process size, appended to the crash log.
    fn diag(&self) -> String {
        let rect = self.unzoomed_rect(self.active);
        let (cw, ch) = self.renderer.cell_size();
        let (rss, threads) = crate::diag::process_stats();
        format!(
            "panes={} rect={:.0}x{:.0} gap={:.1} cell={:.1}x{:.1} rss={rss}KiB threads={threads}",
            self.panes.len(),
            rect.w,
            rect.h,
            self.gap(),
            cw,
            ch,
        )
    }

    /// Space between the tree and the panes. `tree.gap` replaces the padding
    /// on each side of that divider; otherwise it is the pane gap.
    fn tree_separator(&self) -> f32 {
        self.split_width + 2.0 * self.tree_side_pad()
    }

    fn set_spacing(&mut self, config: &Config) {
        let scale = self.scale;
        self.window_pad = scale_insets(config.window.insets(), scale);
        self.pane_pad = scale_insets(config.pane.insets(), scale);
        self.active_pad = scale_insets(config.pane.active_insets(), scale);
        self.tree_pad = scale_insets(config.tree.insets(), scale);
        self.tree_gap = config.tree.divider_gap().map(|g| scaled(g, scale));
        self.split_width = scaled(config.split.width, scale);
        self.split_padding = scaled(config.split.padding, scale);
        self.features = config.features;
        if !self.features.file_tree {
            self.tree = None;
            self.tree_focused = false;
        }
        if !self.features.quick_select {
            self.quick_select = None;
        }
        if !self.features.copy_mode && self.copy_mode.is_some() {
            self.exit_copy_mode();
        }
        if !self.features.search {
            self.search = None;
        }
        if !self.features.command_palette {
            self.palette = None;
        }
        self.active_border = scaled(config.split.active_border, scale);
        self.highlight = config.split.highlight;
        self.highlight_ignores_padding = config.split.highlight_ignores_padding;
        self.tab_bar_config = config.tab_bar.clone();
    }

    fn visible_rects(&self) -> Vec<(PaneId, Rect)> {
        if self.zoomed {
            vec![(self.active, self.area())]
        } else {
            self.layout.rects(self.area(), self.gap())
        }
    }

    /// Recomputes pane rectangles and resizes terminals whose grid changed.
    fn relayout(&mut self) {
        let (cw, ch) = self.renderer.cell_size();
        let mut resized = false;
        for (id, rect) in self.visible_rects() {
            let (cols, rows) = self.pane_grid_count(id, rect);
            let Some(pane) = self.panes.get_mut(&id) else {
                continue;
            };
            pane.rect = rect;
            let changed = {
                let mut term = pane.term();
                let changed = term.cols() != cols || term.rows() != rows;
                term.cell_px = (cw, ch);
                term.resize(cols, rows);
                changed
            };
            if changed {
                pane.pty
                    .resize(cols as u16, rows as u16, cw as u16, ch as u16);
                resized = true;
            }
        }
        // Rewrapping a history frees as much as it allocates; while a divider
        // is dragged that happens on every move, so wait for the release.
        if resized && self.drag.is_none() {
            release_freed_memory();
        }
        self.window.request_redraw();
    }

    fn pane_at(&self, x: f32, y: f32) -> Option<PaneId> {
        self.visible_rects()
            .into_iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(id, _)| id)
    }

    fn divider_at(&self, x: f32, y: f32) -> Option<Divider> {
        if self.zoomed {
            return None;
        }
        self.layout
            .dividers(self.area(), self.gap())
            .into_iter()
            .find(|d| {
                // Keep thin dividers easy to grab.
                let grow = ((8.0 * self.scale as f32 - d.rect.w.min(d.rect.h)) / 2.0).max(0.0);
                let r = match d.axis {
                    Axis::Horizontal => Rect {
                        x: d.rect.x - grow,
                        w: d.rect.w + 2.0 * grow,
                        ..d.rect
                    },
                    Axis::Vertical => Rect {
                        y: d.rect.y - grow,
                        h: d.rect.h + 2.0 * grow,
                        ..d.rect
                    },
                };
                r.contains(x, y)
            })
    }

    fn split(&mut self, axis: Axis, config: &Config) {
        // Holding the key repeats this. Once a pane cannot hold two cells
        // and the divider, another split is a zero-width shell. Enough of
        // those stall the event loop until the compositor kills the process.
        if !self.split_fits(axis) {
            log::info!("split refused: axis={axis:?} {}", self.diag());
            return;
        }
        log::info!("split begin: axis={axis:?} {}", self.diag());
        let cwd = self
            .pane_cwd(self.active)
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| config.shell.working_directory.clone());
        let id = self.next_id;
        self.zoomed = false;
        self.layout.split(self.active, id, axis);
        let rect = self
            .layout
            .rects(self.area(), self.gap())
            .into_iter()
            .find(|(p, _)| *p == id)
            .map_or_else(|| self.area(), |(_, r)| r);
        match spawn_pane(
            &self.renderer,
            &self.proxy,
            config,
            cwd.as_deref(),
            None,
            rect,
            id,
        ) {
            Ok(pane) => {
                self.next_id += 1;
                self.panes.insert(id, pane);
                self.relayout();
                self.set_active(id);
                log::info!("split ok: pane={id} {}", self.diag());
            }
            Err(e) => {
                log::warn!("split failed: {e} {}", self.diag());
                self.layout.remove(id);
                self.show_message(&e);
            }
        }
    }

    /// Closes a pane. When the last one goes, `panes` is left empty and the
    /// caller exits.
    fn close_pane(&mut self, id: PaneId) {
        if !self.panes.contains_key(&id) {
            return;
        }
        self.rang.remove(&id);
        let order = self.layout.ids();
        if !order.contains(&id) {
            self.close_background_pane(id);
            return;
        }
        if self.search.as_ref().is_some_and(|s| s.pane == id) {
            self.search = None;
        }
        if self.copy_mode.as_ref().is_some_and(|c| c.pane == id) {
            self.copy_mode = None;
        }
        if self.quick_select.as_ref().is_some_and(|q| q.pane == id) {
            self.quick_select = None;
        }
        self.hover = None;
        if order == [id] && self.tabs.len() > 1 {
            self.panes.remove(&id);
            self.tabs.remove(self.tab_index);
            let next = self.tab_index.min(self.tabs.len() - 1);
            self.tab_index = next;
            let tab = std::mem::replace(&mut self.tabs[next], Tab::placeholder());
            self.enter_tab(tab);
            return;
        }
        if self.panes.remove(&id).is_none() || self.panes.is_empty() {
            return;
        }
        self.layout.remove(id);
        if self.active == id {
            let ids = self.layout.ids();
            let index = order.iter().position(|&p| p == id).unwrap_or(0);
            self.active = ids[index.saturating_sub(1).min(ids.len() - 1)];
            self.zoomed = false;
            self.select_anchor = None;
            self.preedit.clear();
            if self.focused && self.term().modes.focus_events {
                self.write_pty(b"\x1b[I");
            }
            self.update_title();
        }
        self.relayout();
    }

    /// Closes a pane of a tab that isn't shown (its shell exited).
    fn close_background_pane(&mut self, id: PaneId) {
        let Some(i) = self.tabs.iter().position(|t| t.layout.ids().contains(&id)) else {
            return;
        };
        self.panes.remove(&id);
        let tab = &mut self.tabs[i];
        if tab.layout.ids().len() == 1 {
            self.tabs.remove(i);
            if i < self.tab_index {
                self.tab_index -= 1;
            }
            self.relayout();
        } else {
            tab.layout.remove(id);
            if tab.active == id {
                tab.active = tab.layout.ids()[0];
                tab.zoomed = false;
            }
        }
        self.window.request_redraw();
    }

    // ---- tabs -------------------------------------------------------------------

    fn send_focus(&mut self, focused: bool) {
        if self.focused && self.panes.contains_key(&self.active) && self.term().modes.focus_events {
            self.write_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    fn take_tab(&mut self) -> Tab {
        Tab {
            layout: std::mem::replace(&mut self.layout, Layout::new(0)),
            active: self.active,
            zoomed: self.zoomed,
            title: self.tab_title.take(),
            broadcast: self.broadcast,
        }
    }

    /// Makes `tab` (already removed from `tabs[tab_index]`) the shown one.
    fn enter_tab(&mut self, tab: Tab) {
        for id in tab.layout.ids() {
            self.rang.remove(&id);
        }
        self.layout = tab.layout;
        self.active = tab.active;
        self.zoomed = tab.zoomed;
        self.tab_title = tab.title;
        self.broadcast = tab.broadcast;
        self.renaming = None;
        self.select_anchor = None;
        self.preedit.clear();
        self.search = None;
        self.exit_copy_mode();
        self.quick_select = None;
        self.hover = None;
        self.send_focus(true);
        self.relayout();
        self.update_title();
        self.sync_tree(true);
    }

    fn switch_tab(&mut self, index: usize) {
        if index == self.tab_index || index >= self.tabs.len() {
            return;
        }
        self.send_focus(false);
        let current = self.take_tab();
        self.tabs[self.tab_index] = current;
        let tab = std::mem::replace(&mut self.tabs[index], Tab::placeholder());
        self.tab_index = index;
        self.enter_tab(tab);
    }

    /// Opens a tab after the current one, named `title` if given.
    fn new_tab(&mut self, config: &Config, title: Option<String>) {
        let cwd = self
            .pane_cwd(self.active)
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| config.shell.working_directory.clone());
        self.send_focus(false);
        let id = self.next_id;
        let current = self.take_tab();
        self.tabs[self.tab_index] = current;
        self.tabs.insert(self.tab_index + 1, Tab::placeholder());
        self.tab_index += 1;
        self.layout = Layout::new(id);
        let rect = self.area();
        match spawn_pane(
            &self.renderer,
            &self.proxy,
            config,
            cwd.as_deref(),
            None,
            rect,
            id,
        ) {
            Ok(pane) => {
                self.next_id += 1;
                self.panes.insert(id, pane);
                self.enter_tab(Tab {
                    layout: Layout::new(id),
                    active: id,
                    zoomed: false,
                    title,
                    broadcast: false,
                });
            }
            Err(e) => {
                self.tabs.remove(self.tab_index);
                self.tab_index -= 1;
                let tab = std::mem::replace(&mut self.tabs[self.tab_index], Tab::placeholder());
                self.enter_tab(tab);
                self.show_message(&e);
            }
        }
    }

    /// Opens the focused pane's history in `scrollback_pager` in a new tab;
    /// quitting the pager closes the tab and removes the file.
    fn show_scrollback(&mut self, config: &Config) {
        let text = self.term().history_text();
        let path = match write_private_temp("scrollback", &text) {
            Ok(path) => path,
            Err(e) => return self.show_message(&format!("scrollback: {e}")),
        };
        let mut pager = config.clone();
        pager.shell.program = Some("sh".into());
        pager.shell.args = vec![
            "-c".into(),
            // less shows private-use glyphs (Nerd Font icons) as <U+XXXX>
            // unless told they print.
            format!(
                "export LESSUTFCHARDEF=\"${{LESSUTFCHARDEF:-e000-f8ff:p,f0000-10fffd:p}}\"; \
                 {} \"$1\"; rm -f -- \"$1\"",
                config.scrollback_pager
            ),
            "sh".into(),
            path.to_string_lossy().into_owned(),
        ];
        let title = self.lang.get("tab.scrollback").to_string();
        self.new_tab(&pager, Some(title));
    }

    fn copy_last_command_output(&mut self) {
        let text = self.term().last_command_output();
        match text {
            Some(text) => self.set_clipboard(false, text),
            None => {
                let message = self.lang.get("output.none").to_string();
                self.show_message(&message);
            }
        }
    }

    /// Swaps the current tab with the one `by` places away (-1 left, 1 right).
    fn move_tab(&mut self, by: isize) {
        let to = self.tab_index as isize + by;
        if to < 0 || to as usize >= self.tabs.len() {
            return;
        }
        self.tabs.swap(self.tab_index, to as usize);
        self.tab_index = to as usize;
        self.window.request_redraw();
    }

    /// Moves the current tab into slot `to`. The current tab's own data lives
    /// outside the vec, so only the placeholder slot moves with the index.
    fn move_tab_to(&mut self, to: usize) {
        if to == self.tab_index || to >= self.tabs.len() {
            return;
        }
        let from = self.tab_index;
        if to > from {
            self.tabs[from..=to].rotate_left(1);
        } else {
            self.tabs[to..=from].rotate_right(1);
        }
        self.tab_index = to;
        self.window.request_redraw();
    }

    fn start_rename(&mut self) {
        self.renaming = Some(self.tab_title.clone().unwrap_or_default());
        self.window.request_redraw();
    }

    /// Keys while a tab name is being typed. Enter keeps it (empty goes
    /// back to the pane's title); Escape cancels.
    fn handle_rename_key(&mut self, event: &KeyEvent) {
        use winit::keyboard::NamedKey;
        let Some(name) = &mut self.renaming else {
            return;
        };
        let alt = self.modifiers.alt_key();
        match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => self.renaming = None,
            Key::Named(NamedKey::Enter) => {
                let name = name.trim().to_string();
                self.tab_title = (!name.is_empty()).then_some(name);
                self.renaming = None;
            }
            Key::Named(NamedKey::Backspace) => {
                name.pop();
            }
            _ if alt && event.key_without_modifiers().as_ref() == Key::Character("u") => {
                name.clear();
            }
            _ => {
                if let Some(text) = event
                    .text
                    .as_deref()
                    .filter(|t| !alt && !t.chars().any(char::is_control))
                {
                    name.push_str(text);
                }
            }
        }
        self.window.request_redraw();
    }

    /// The tab name prompt along the bottom of the window: the bar, its
    /// rect and its backdrop.
    fn rename_bar(&self, colors: &Colors) -> Option<(Terminal, Rect, Rect)> {
        let name = self.renaming.as_ref()?;
        let (w, h) = self.renderer.surface_size();
        let full = content_area(w, h, self.window_pad);
        let window = Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        };
        let (backdrop, r, cols) = self.bottom_bar(full, window);
        let mut bar = bar_terminal(cols, colors.tab_bar, colors.foreground);
        let [cr, cg, cb] = colors.active_border;
        let prompt = format!(" \u{f044} {} ", self.lang.get("tab.rename"));
        let hint_text = self.lang.get("tab.rename_hint");
        let hint = i18n::emphasize_keys(hint_text);
        let hint_w = tree::width(&hint_text.replace("{key:", "").replace('}', ""));
        let room = cols.saturating_sub(tree::width(&prompt) + hint_w + 3);
        let shown = tree::fit_tail(name, room);
        let cursor = tree::width(&prompt) + tree::width(&shown);
        bar.feed(
            format!(
                "\x1b[1;38;2;{cr};{cg};{cb}m{prompt}\x1b[22;39m{shown}\x1b[1;{}H\x1b[2m{hint}\x1b[22m\x1b[1;{}H\x1b[?25h",
                cols.saturating_sub(hint_w + 1) + 1,
                cursor + 1
            )
            .as_bytes(),
        );
        Some((bar, r, backdrop))
    }

    /// Starts another kodama with the same config, in the focused pane's
    /// directory.
    fn new_window(&mut self) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => return self.show_message(&format!("new window: {e}")),
        };
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--config")
            .arg(&self.config_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(cwd) = self.pane_cwd(self.active).filter(|d| d.is_dir()) {
            cmd.current_dir(cwd);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                thread::spawn(move || child.wait());
            }
            Err(e) => self.show_message(&format!("new window: {e}")),
        }
    }

    fn close_tab(&mut self) {
        for id in self.layout.ids() {
            self.close_pane(id);
        }
    }

    /// The tab bar's contents; also records where each tab is for clicks.
    fn tab_bar(&mut self, cols: usize, colors: &Colors) -> Terminal {
        let mut bar = bar_terminal(cols, colors.tab_bar, colors.foreground);
        let titles: Vec<String> = (0..self.tabs.len())
            .map(|i| {
                let (active, name) = if i == self.tab_index {
                    (self.active, self.tab_title.clone())
                } else {
                    (self.tabs[i].active, self.tabs[i].title.clone())
                };
                let title = name.unwrap_or_else(|| match self.tab_bar_config.title {
                    TabTitle::Directory => self
                        .pane_cwd(active)
                        .map(|d| dir_label(&d))
                        .unwrap_or_default(),
                    TabTitle::Title => self
                        .panes
                        .get(&active)
                        .map(|p| p.term().title.clone())
                        .unwrap_or_default(),
                });
                let title = if title.is_empty() {
                    "shell".to_string()
                } else {
                    title
                };
                if self.tab_rang(i) {
                    format!("\u{f0f3} {title}")
                } else {
                    title
                }
            })
            .collect();
        let (text, spans) = tabbar::render(
            &titles,
            self.tab_index,
            cols,
            self.tab_bar_config.style,
            self.tab_bar_config.max_width,
            colors,
        );
        bar.feed(text.as_bytes());
        self.tab_spans = spans;
        bar
    }

    // ---- search -----------------------------------------------------------------

    fn open_search(&mut self) {
        if self.search.is_none() {
            self.search = Some(Search {
                pane: self.active,
                query: String::new(),
                matches: Vec::new(),
                current: None,
                updated: Instant::now(),
            });
        }
        self.set_tree_focus(false);
        self.window.request_redraw();
    }

    /// Re-runs the search after the query or the scrollback changed. A new
    /// query jumps to the newest match.
    fn update_search(&mut self, new_query: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        let Some(pane) = self.panes.get(&search.pane) else {
            return;
        };
        let mut term = pane.term();
        let previous = search.current.map(|i| search.matches[i]);
        search.matches = term.search(&search.query);
        search.updated = Instant::now();
        search.current = if search.matches.is_empty() {
            None
        } else if new_query {
            Some(search.matches.len() - 1)
        } else {
            previous
                .and_then(|p| search.matches.iter().position(|m| *m == p))
                .or(Some(search.matches.len() - 1))
        };
        if new_query && let Some(i) = search.current {
            term.reveal(search.matches[i].0.line);
        }
        drop(term);
        self.window.request_redraw();
    }

    /// Moves to the next older (`back`) or newer match.
    fn search_step(&mut self, back: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        let n = search.matches.len();
        if n == 0 {
            return;
        }
        let i = match (search.current, back) {
            (Some(i), true) => (i + n - 1) % n,
            (Some(i), false) => (i + 1) % n,
            (None, _) => n - 1,
        };
        search.current = Some(i);
        let line = search.matches[i].0.line;
        if let Some(pane) = self.panes.get(&search.pane) {
            pane.term().reveal(line);
        }
        self.window.request_redraw();
    }

    fn close_search(&mut self) {
        // In copy mode, the cursor continues from the match shown.
        if let Some(search) = self.search.take()
            && let Some(copy) = &mut self.copy_mode
            && copy.pane == search.pane
            && let Some(i) = search.current
        {
            copy.cursor = search.matches[i].0;
            if let Some(pane) = self.panes.get(&copy.pane) {
                let mut term = pane.term();
                term.selection = copy.selection(&term);
            }
        }
        self.window.request_redraw();
    }

    // ---- copy mode, quick select and the command palette ------------------------

    fn toggle_copy_mode(&mut self) {
        if self.copy_mode.is_some() {
            self.exit_copy_mode();
            return;
        }
        let copy = CopyMode::new(self.active, &self.term());
        self.term().selection = None;
        self.select_anchor = None;
        self.copy_mode = Some(copy);
        self.set_tree_focus(false);
        self.window.request_redraw();
    }

    fn exit_copy_mode(&mut self) {
        if let Some(copy) = self.copy_mode.take()
            && let Some(pane) = self.panes.get(&copy.pane)
        {
            pane.term().selection = None;
        }
        self.window.request_redraw();
    }

    fn handle_copy_key(&mut self, event: &KeyEvent, config: &Config, lua: &Lua) -> bool {
        let m = mod_bits(self.modifiers);
        let alt = m & mods::ALT != 0;
        // Ctrl / Super, and Alt+Shift, still run key bindings. Plain Alt is
        // the copy-mode modifier (half a page, and so on).
        if m & (mods::CTRL | mods::SUPER) != 0 || (alt && m & mods::SHIFT != 0) {
            match config.find_binding(&key_name(event), m) {
                Some(Action::CopyMode) => {
                    self.exit_copy_mode();
                    return false;
                }
                Some(Action::None | Action::CopyOrPass | Action::PasteOrPass) | None => {}
                Some(action) => return self.perform(action.clone(), config, lua),
            }
        }
        let text = event
            .text
            .as_deref()
            .filter(|t| !alt && !t.is_empty() && !t.chars().any(char::is_control));
        let key = match text {
            Some(t) => t.to_string(),
            None => key_name(event),
        };
        let Some(copy) = &mut self.copy_mode else {
            return false;
        };
        let Some(pane) = self.panes.get(&copy.pane) else {
            return false;
        };
        let outcome = {
            let mut term = pane.term();
            let outcome = copy.key(&key, alt, &mut term);
            term.selection = copy.selection(&term);
            outcome
        };
        match outcome {
            Outcome::Stay => {}
            Outcome::Copy(text) => {
                self.set_clipboard(false, text.clone());
                self.set_clipboard(true, text);
                self.exit_copy_mode();
            }
            Outcome::Search => self.open_search(),
            Outcome::Exit => self.exit_copy_mode(),
        }
        self.window.request_redraw();
        false
    }

    fn start_quick_select(&mut self) {
        if self.quick_select.take().is_some() {
            self.window.request_redraw();
            return;
        }
        let targets = self.term().quick_select_targets();
        if targets.is_empty() {
            return;
        }
        self.quick_select = Some(QuickSelect::new(self.active, targets));
        self.set_tree_focus(false);
        self.window.request_redraw();
    }

    fn handle_quick_key(&mut self, event: &KeyEvent, config: &Config) {
        use winit::keyboard::NamedKey;
        let Some(quick) = &mut self.quick_select else {
            return;
        };
        // Alt usually suppresses the text, so the letter comes from the key.
        let letter = event
            .text
            .as_deref()
            .and_then(|t| t.chars().next())
            .filter(|c| c.is_alphabetic())
            .or_else(|| match event.logical_key.as_ref() {
                Key::Character(s) => {
                    let mut chars = s.chars();
                    chars
                        .next()
                        .filter(|c| c.is_alphabetic() && chars.next().is_none())
                }
                _ => None,
            });
        match (event.logical_key.as_ref(), letter) {
            (Key::Named(NamedKey::Shift | NamedKey::Control | NamedKey::Alt), _) => return,
            (Key::Named(NamedKey::Backspace), _) => {
                quick.typed.pop();
            }
            (_, Some(c))
                if self.modifiers.alt_key()
                    && self.features.quick_select
                    && self.features.quick_select_open =>
            {
                if let Pick::Done(text, _) = quick.input(c) {
                    self.quick_select = None;
                    self.open_quick_target(&text, config);
                }
            }
            (_, Some(c)) if !self.modifiers.alt_key() => {
                if let Pick::Done(text, paste) = quick.input(c) {
                    self.quick_select = None;
                    self.set_clipboard(false, text.clone());
                    self.set_clipboard(true, text.clone());
                    if paste {
                        self.paste(&text);
                    }
                }
            }
            _ => self.quick_select = None,
        }
        self.window.request_redraw();
    }

    /// Opens a quick-select hit: a URL in the browser, a file in `$EDITOR`,
    /// a directory as `cd`.
    fn open_quick_target(&mut self, text: &str, config: &Config) {
        let text = text.trim();
        if text.starts_with("www.") {
            open_link(&format!("https://{text}"));
            return;
        }
        if text.contains("://") {
            open_link(text);
            return;
        }
        let raw = Path::new(text);
        let path = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.pane_cwd(self.active).unwrap_or_default().join(raw)
        };
        if path.is_dir() {
            self.scroll_to_bottom();
            self.write_pty(format!("\x15cd {}\r", tree::shell_quote(&path)).as_bytes());
            return;
        }
        if path.is_file() {
            self.open_in_editor(&path, config);
            return;
        }
        let message = self.lang.get("quick.no_open").to_string();
        self.show_message(&message);
    }

    /// Types `$EDITOR` for `path` into the focused pane, or a new one when a
    /// full-screen program is running there.
    fn open_in_editor(&mut self, path: &Path, config: &Config) {
        let command = format!("${{EDITOR:-vi}} {}\r", tree::shell_quote(path));
        if self.term().is_alt_screen() {
            let rect = self.panes[&self.active].rect;
            self.split(
                if rect.w > rect.h {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                },
                config,
            );
            self.write_pty(command.as_bytes());
        } else {
            self.scroll_to_bottom();
            self.write_pty(format!("\x15{command}").as_bytes());
        }
    }

    fn open_palette(&mut self, config: &Config) {
        if self.palette.take().is_some() {
            self.window.request_redraw();
            return;
        }
        let mut entries: Vec<Entry> = PALETTE_ACTIONS
            .iter()
            .map(|name| {
                let key = format!("action.{name}");
                let translated = self.lang.get(&key);
                let english = palette::title_of(name);
                Entry {
                    title: if translated == key {
                        english.clone()
                    } else {
                        translated.to_string()
                    },
                    alias: format!("{english} {name}"),
                    keys: binding_label(config, name),
                    item: Item::Action(name.to_string()),
                }
            })
            .collect();
        for i in 1..=self.tabs.len().min(9) {
            if self.tabs.len() > 1 {
                let name = format!("go_to_tab_{i}");
                let n = i.to_string();
                entries.push(Entry {
                    title: self.lang.format("palette.go_to_tab", &[("n", &n)]),
                    alias: format!("Go to tab {i} {name}"),
                    keys: binding_label(config, &name),
                    item: Item::Action(name),
                });
            }
        }
        let names = config
            .custom_schemes
            .iter()
            .map(|(name, _)| (name.as_str(), "theme.lua"))
            .chain(schemes::SCHEMES.iter().map(|s| (s.name, "")));
        for (name, origin) in names {
            if entries
                .iter()
                .any(|e| matches!(&e.item, Item::Scheme(n) if schemes::same(n, name)))
            {
                continue;
            }
            let current = config
                .color_scheme
                .as_deref()
                .is_some_and(|c| schemes::same(c, name));
            entries.push(Entry {
                title: self.lang.format("palette.color_scheme", &[("name", name)]),
                alias: format!("Color scheme: {name}"),
                keys: if current {
                    self.lang.get("palette.current").to_string()
                } else {
                    origin.into()
                },
                item: Item::Scheme(name.to_string()),
            });
        }
        let current_lang = self.lang.id.clone();
        for info in i18n::catalog(&self.lang_dirs) {
            entries.push(Entry {
                title: self
                    .lang
                    .format("palette.language", &[("name", &info.name)]),
                alias: format!("Language: {} {}", info.name, info.id),
                keys: if info.id == current_lang {
                    self.lang.get("palette.current").to_string()
                } else {
                    String::new()
                },
                item: Item::Language(info.id),
            });
        }
        let empty = self.lang.get("palette.no_matches").to_string();
        self.palette = Some(Palette::new(entries, empty));
        self.window.request_redraw();
    }

    fn handle_palette_key(&mut self, event: &KeyEvent, config: &Config, lua: &Lua) -> bool {
        use winit::keyboard::NamedKey;
        let alt = self.modifiers.alt_key();
        if matches!(
            config.find_binding(&key_name(event), mod_bits(self.modifiers)),
            Some(Action::CommandPalette)
        ) {
            self.palette = None;
            self.window.request_redraw();
            return false;
        }
        let Some(palette) = &mut self.palette else {
            return false;
        };
        let page = 8;
        let named = key_name(event);
        match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => self.palette = None,
            Key::Named(NamedKey::Enter) => {
                let item = palette.chosen().cloned();
                self.palette = None;
                self.window.request_redraw();
                match item {
                    Some(Item::Action(name)) => {
                        if let Some(action) = Action::from_name(&name) {
                            return self.perform(action, config, lua);
                        }
                    }
                    Some(Item::Scheme(name)) => self.pending_scheme = Some(name),
                    Some(Item::Language(id)) if id != self.lang.id => {
                        self.pending_language = Some(id);
                    }
                    Some(Item::Language(_)) | None => {}
                }
            }
            Key::Named(NamedKey::ArrowUp) => palette.move_by(-1),
            Key::Named(NamedKey::ArrowDown | NamedKey::Tab) => palette.move_by(1),
            Key::Named(NamedKey::PageUp) => palette.move_by(-page),
            Key::Named(NamedKey::PageDown) => palette.move_by(page),
            Key::Named(NamedKey::Backspace) => palette.backspace(),
            _ if alt && (named == "p" || named == "k") => palette.move_by(-1),
            _ if alt && (named == "n" || named == "j") => palette.move_by(1),
            _ if alt && named == "u" => palette.clear(),
            _ => {
                if let Some(text) = event
                    .text
                    .as_deref()
                    .filter(|t| !alt && !t.chars().any(char::is_control))
                {
                    palette.input(text);
                }
            }
        }
        self.window.request_redraw();
        false
    }

    /// The palette's place: centred in the window at its full height, so
    /// its top stays put while the list shrinks to the matches.
    fn palette_rect(&self) -> Option<(Rect, usize, usize)> {
        let palette = self.palette.as_ref()?;
        let (w, h) = self.renderer.surface_size();
        let (cw, ch) = self.renderer.cell_size();
        let full = content_area(w, h, self.window_pad);
        let total_cols = (full.w / cw).floor() as usize;
        let total_rows = (full.h / ch).floor() as usize;
        let cols = total_cols.saturating_sub(4).clamp(10, 76);
        let max = 14.min(total_rows.saturating_sub(3).max(1));
        let rows = palette.rows(max);
        let x = full.x + ((total_cols.saturating_sub(cols)) / 2) as f32 * cw;
        let y = full.y + ((full.h - (1 + max) as f32 * ch) / 2.0).max(0.0).floor();
        Some((
            Rect {
                x,
                y,
                w: cols as f32 * cw,
                h: rows as f32 * ch,
            },
            cols,
            rows,
        ))
    }

    /// Padding that applies to `rect`: a pane's, the tree's, or none for a
    /// bar that spans the content area.
    fn pad_for_rect(&self, rect: Rect) -> Insets {
        if let Some(id) = self
            .visible_rects()
            .into_iter()
            .find(|(_, r)| *r == rect)
            .map(|(id, _)| id)
        {
            return self.insets_for_pane(id);
        }
        if self.tree_rect().is_some_and(|t| t == rect) {
            return self.tree_pad;
        }
        Insets::ZERO
    }

    /// Where a one-row bar over the bottom of `rect` goes: its backdrop
    /// reaches from the last grid row down to `corners`' bottom-left and
    /// bottom-right corners, and its text is centred in it vertically,
    /// aligned with the grid's columns. Returns the backdrop, the bar and
    /// its columns.
    fn bottom_bar(&self, rect: Rect, corners: Rect) -> (Rect, Rect, usize) {
        let (cw, ch) = self.renderer.cell_size();
        let pad = self.pad_for_rect(rect);
        let b = self.grid_box(rect, pad);
        let rows = if pad.is_zero() {
            (rect.h / ch).floor().max(1.0)
        } else {
            ((b.bottom - b.top) / ch).floor().max(1.0)
        };
        let stride = crate::renderer::grid_pitch(b.top, b.bottom, rows as usize, ch);
        let top = (b.top + (rows - 1.0) * stride).floor();
        let backdrop = Rect {
            x: corners.x,
            y: top,
            w: corners.w,
            h: (corners.y + corners.h - top).max(ch),
        };
        let bar = Rect {
            x: b.left,
            y: (top + (backdrop.h - ch) / 2.0).floor(),
            w: (b.right - b.left).max(cw),
            h: ch,
        };
        (backdrop, bar, (bar.w / cw).floor().max(1.0) as usize)
    }

    /// The bar along the bottom of a pane in copy mode or quick select.
    fn mode_bar(&self, id: PaneId, cols: usize, colors: &Colors) -> Option<Terminal> {
        let [r, g, b] = colors.active_border;
        let tag = |name: &str| format!(" \x1b[1;38;2;{r};{g};{b}m{name}\x1b[22;39m  ");
        let mut bar = bar_terminal(cols, colors.tab_bar, colors.foreground);
        if let Some(quick) = self.quick_select.as_ref().filter(|q| q.pane == id) {
            bar.feed(
                format!(
                    "{}{}{}",
                    tag(self.lang.get("mode.quick_tag")),
                    i18n::emphasize_keys(self.lang.get(if self.features.quick_select_open {
                        "mode.quick_hint_open"
                    } else {
                        "mode.quick_hint"
                    })),
                    quick.typed
                )
                .as_bytes(),
            );
            return Some(bar);
        }
        let copy = self.copy_mode.as_ref().filter(|c| c.pane == id)?;
        let kind = match copy.visual {
            Some((crate::ui::copymode::Visual::Chars, _)) => self.lang.get("mode.visual"),
            Some((crate::ui::copymode::Visual::Lines, _)) => self.lang.get("mode.visual_line"),
            None => self.lang.get("mode.copy"),
        };
        let position = format!(
            "{}/{}",
            copy.cursor.line + 1,
            self.term_of(id).total_lines()
        );
        bar.feed(
            format!(
                "{}{}",
                tag(kind),
                i18n::emphasize_keys(self.lang.get("mode.copy_hint"))
            )
            .as_bytes(),
        );
        let at = cols.saturating_sub(position.len() + 1);
        bar.feed(format!("\x1b[1;{}H\x1b[2m{position}\x1b[22m", at + 1).as_bytes());
        Some(bar)
    }

    fn handle_search_key(&mut self, event: &KeyEvent, config: &Config, lua: &Lua) -> bool {
        use winit::keyboard::NamedKey;
        match config.find_binding(&key_name(event), mod_bits(self.modifiers)) {
            Some(Action::Search) => {
                self.search_step(true);
                return false;
            }
            Some(Action::Paste | Action::PasteOrPass) => {
                let text = self
                    .clipboard
                    .as_mut()
                    .and_then(|cb| cb.get_text().ok())
                    .unwrap_or_default();
                if let Some(search) = &mut self.search {
                    search.query.push_str(text.lines().next().unwrap_or(""));
                }
                self.update_search(true);
                return false;
            }
            Some(Action::CopyOrPass) => {
                self.close_search();
                return false;
            }
            Some(Action::None | Action::Copy) | None => {}
            Some(action) => {
                self.close_search();
                return self.perform(action.clone(), config, lua);
            }
        }
        let Some(search) = &mut self.search else {
            return false;
        };
        let alt = self.modifiers.alt_key();
        match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => self.close_search(),
            Key::Named(NamedKey::Enter) if self.modifiers.shift_key() => self.search_step(false),
            Key::Named(NamedKey::Enter | NamedKey::ArrowUp) => self.search_step(true),
            Key::Named(NamedKey::ArrowDown) => self.search_step(false),
            Key::Named(NamedKey::Backspace) => {
                search.query.pop();
                self.update_search(true);
            }
            _ if alt && event.key_without_modifiers().as_ref() == Key::Character("u") => {
                search.query.clear();
                self.update_search(true);
            }
            _ => {
                if let Some(text) = event
                    .text
                    .as_deref()
                    .filter(|t| !alt && !t.chars().any(char::is_control))
                {
                    search.query.push_str(text);
                    self.update_search(true);
                }
            }
        }
        false
    }

    /// The search bar along the bottom of the searched pane.
    fn search_bar(&self, cols: usize, colors: &Colors) -> Option<Terminal> {
        let search = self.search.as_ref()?;
        let count = match (search.current, search.matches.len()) {
            (Some(i), n) => format!(" {}/{n} ", i + 1),
            (None, _) if !search.query.is_empty() => {
                format!(" {} ", self.lang.get("search.no_matches"))
            }
            _ => String::new(),
        };
        let prefix = " \u{f002} ";
        let count_w = tree::width(&count);
        let mut bar = bar_terminal(cols, colors.tab_bar, colors.foreground);
        let room = cols.saturating_sub(tree::width(prefix) + count_w + 1);
        let query = tree::fit_tail(&search.query, room);
        bar.feed(format!("{prefix}{query}").as_bytes());
        let cursor = tree::width(prefix) + tree::width(&query);
        let count_at = cols.saturating_sub(count_w);
        bar.feed(
            format!(
                "\x1b[1;{}H\x1b[2m{count}\x1b[22m\x1b[1;{}H\x1b[?25h",
                count_at + 1,
                cursor + 1
            )
            .as_bytes(),
        );
        Some(bar)
    }

    /// Highlights for pane `id`: visible search matches and the hovered link.
    fn marks_for(
        search: Option<&Search>,
        hover: Option<&(PaneId, String, Point, Point)>,
        copy: Option<&CopyMode>,
        quick: Option<&QuickSelect>,
        id: PaneId,
        term: &Terminal,
    ) -> Vec<Mark> {
        let mut out = Vec::new();
        if let Some(quick) = quick.filter(|q| q.pane == id) {
            out.extend(quick.hits.iter().map(|h| Mark {
                start: h.start,
                end: h.end,
                kind: MarkKind::Match,
            }));
        }
        if let Some(copy) = copy.filter(|c| c.pane == id) {
            out.push(Mark {
                start: copy.cursor,
                end: copy.cursor,
                kind: MarkKind::Cursor,
            });
        }
        if let Some(search) = search
            && search.pane == id
        {
            let top = term.visible_line(0);
            let bottom = top + term.rows();
            let first = search.matches.partition_point(|m| m.1.line < top);
            for (i, m) in search.matches.iter().enumerate().skip(first) {
                if m.0.line >= bottom {
                    break;
                }
                let kind = if search.current == Some(i) {
                    MarkKind::Current
                } else {
                    MarkKind::Match
                };
                out.push(Mark {
                    start: m.0,
                    end: m.1,
                    kind,
                });
            }
        }
        if let Some((pane, _, start, end)) = hover
            && *pane == id
        {
            out.push(Mark {
                start: *start,
                end: *end,
                kind: MarkKind::Link,
            });
        }
        out
    }

    // ---- links and clipboard ----------------------------------------------------

    /// Finds the link under the pointer while alt is held and updates the
    /// pointer shape.
    fn refresh_pointer(&mut self) {
        let (x, y) = (self.cursor_pos.x as f32, self.cursor_pos.y as f32);
        let hover =
            if self.modifiers.alt_key() && self.drag.is_none() && self.select_anchor.is_none() {
                self.pane_at(x, y).and_then(|id| {
                    let (col, row) = self.cell_at(id, self.cursor_pos);
                    let term = self.term_of(id);
                    let point = Point {
                        line: term.visible_line(row),
                        col,
                    };
                    term.link_at(point).map(|(target, a, b)| (id, target, a, b))
                })
            } else {
                None
            };
        let changed = match (&hover, &self.hover) {
            (Some(a), Some(b)) => a.0 != b.0 || a.2 != b.2,
            (None, None) => false,
            _ => true,
        };
        self.hover = hover;
        if changed {
            self.window.request_redraw();
        }
        let over_tree = self.tree_hit_rect().is_some_and(|t| t.contains(x, y));
        let over_bar = self.tab_bar_rect().is_some_and(|t| t.contains(x, y));
        let resizing = match &self.drag {
            Some(Drag::Tree) => Some(Axis::Horizontal),
            Some(Drag::Split(d)) => Some(d.axis),
            Some(Drag::Tab { .. } | Drag::Pane { .. }) => {
                self.window.set_cursor(CursorIcon::Grabbing);
                self.cursor_icon = CursorIcon::Grabbing;
                return;
            }
            None if self.tree_handle_at(x, y) => Some(Axis::Horizontal),
            None => self.divider_at(x, y).map(|d| d.axis),
        };
        let icon = match resizing {
            Some(Axis::Horizontal) => CursorIcon::ColResize,
            Some(Axis::Vertical) => CursorIcon::RowResize,
            None if self.hover.is_some() || over_bar => CursorIcon::Pointer,
            None if over_tree => CursorIcon::Default,
            None => CursorIcon::Text,
        };
        if icon != self.cursor_icon {
            self.cursor_icon = icon;
            self.window.set_cursor(icon);
        }
    }

    #[cfg(target_os = "linux")]
    fn set_clipboard(&mut self, primary: bool, text: String) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        let Some(cb) = &mut self.clipboard else {
            return;
        };
        let kind = if primary {
            LinuxClipboardKind::Primary
        } else {
            LinuxClipboardKind::Clipboard
        };
        if let Err(e) = cb.set().clipboard(kind).text(text) {
            log::warn!("clipboard write failed: {e}");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn set_clipboard(&mut self, _primary: bool, text: String) {
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(text);
        }
    }

    /// Writes the tabs, splits and directories next to the config file.
    fn save_session(&self) {
        if !self.features.restore_layout || self.one_off {
            return;
        }
        let session = self.session_snapshot();
        match crate::session::save(&self.config_path, &session) {
            Ok(()) => log::debug!("saved the session"),
            Err(e) => log::warn!("session: {e}"),
        }
    }

    fn session_snapshot(&self) -> crate::session::Session {
        let tabs = (0..self.tabs.len())
            .map(|i| {
                let (shape, active, zoomed, title) = if i == self.tab_index {
                    (
                        self.layout.shape(),
                        self.active,
                        self.zoomed,
                        self.tab_title.clone(),
                    )
                } else {
                    let tab = &self.tabs[i];
                    (
                        tab.layout.shape(),
                        tab.active,
                        tab.zoomed,
                        tab.title.clone(),
                    )
                };
                let mut dirs = HashMap::new();
                let mut screens = HashMap::new();
                remember_dirs(&shape, &mut dirs, self);
                remember_screens(&shape, &mut screens, self);
                crate::session::SavedTab {
                    title,
                    active,
                    zoomed,
                    shape,
                    dirs,
                    screens,
                }
            })
            .collect();
        crate::session::Session {
            current: self.tab_index,
            tabs,
            tree: self.features.file_tree && self.tree.is_some(),
            tree_width: Some(self.tree_config.width),
        }
    }

    /// Rebuilds the window from `saved`. The starter pane becomes the current
    /// tab's active pane; on failure it is still the only one.
    fn restore_session(
        &mut self,
        saved: crate::session::Session,
        config: &Config,
    ) -> Result<(), String> {
        let rect = self.area();
        let mut spawned: HashMap<PaneId, Pane> = HashMap::new();
        let mut next_id = self.next_id;
        let mut built = Vec::new();
        let mut reused = false;
        for (index, tab) in saved.tabs.into_iter().enumerate() {
            let mut ids = HashMap::new();
            let spawn = Spawn {
                renderer: &self.renderer,
                proxy: &self.proxy,
                config,
                rect,
                reuse: std::cell::Cell::new(
                    (index == saved.current && !reused).then_some((tab.active, 1)),
                ),
            };
            let shape = spawn_shape(
                tab.shape,
                &tab.dirs,
                &tab.screens,
                &mut ids,
                &mut next_id,
                &mut spawned,
                &spawn,
            )?;
            reused |= index == saved.current && spawn.reuse.get().is_none();
            let layout = Layout::from_shape(shape);
            let active = ids
                .get(&tab.active)
                .copied()
                .or_else(|| layout.ids().first().copied())
                .ok_or_else(|| "session tab has no panes".to_string())?;
            built.push((layout, active, tab.zoomed, tab.title));
        }
        if built.is_empty() {
            return Err("session has no tabs".into());
        }
        // The new shells are up. Drop the starter pane only now, unless a
        // restored pane took it over.
        if !reused {
            self.panes.remove(&1);
        }
        self.panes.extend(spawned);
        self.next_id = next_id;
        let index = saved.current.min(built.len() - 1);
        self.tabs = (0..built.len()).map(|_| Tab::placeholder()).collect();
        self.tab_index = index;
        for (i, (layout, active, zoomed, title)) in built.into_iter().enumerate() {
            if i == index {
                self.layout = layout;
                self.active = active;
                self.zoomed = zoomed;
                self.tab_title = title;
            } else {
                self.tabs[i] = Tab {
                    layout,
                    active,
                    zoomed,
                    title,
                    broadcast: false,
                };
            }
        }
        self.relayout();
        self.update_title();
        if self.features.file_tree {
            if let Some(width) = saved.tree_width {
                self.set_tree_width(width as isize);
            }
            if saved.tree {
                self.show_tree();
            }
        }
        Ok(())
    }

    /// The pane's working directory: what the shell reported with OSC 7 when
    /// it is on this machine, else the shell process's own.
    fn pane_cwd(&self, id: PaneId) -> Option<PathBuf> {
        let pane = self.panes.get(&id)?;
        let reported = pane.term().reported_cwd.clone();
        if let Some((host, path)) = reported
            && (host.is_empty() || host == "localhost" || host == self.hostname)
            && path.is_dir()
        {
            return Some(path);
        }
        pane.pty.cwd()
    }

    fn set_active(&mut self, id: PaneId) {
        self.set_tree_focus(false);
        if id == self.active || !self.panes.contains_key(&id) {
            return;
        }
        if self.focused && self.term().modes.focus_events {
            self.write_pty(b"\x1b[O");
        }
        self.active = id;
        if self.focused && self.term().modes.focus_events {
            self.write_pty(b"\x1b[I");
        }
        self.select_anchor = None;
        self.preedit.clear();
        if self.zoomed {
            self.zoomed = false;
            self.relayout();
        } else if !self.active_pad.is_zero() {
            // The extra padding moves from the old pane to this one.
            self.relayout();
        }
        self.update_title();
        self.sync_tree(true);
        self.window.request_redraw();
    }

    // ---- file tree --------------------------------------------------------------

    /// Shows the tree without moving the keyboard focus.
    fn show_tree(&mut self) {
        if !self.features.file_tree || self.tree.is_some() {
            return;
        }
        let mut tree = FileTree::new(self.active_cwd(), self.tree_config.show_hidden);
        let proxy = self.proxy.clone();
        tree.set_notify(Arc::new(move || {
            let _ = proxy.send_event(UserEvent::Redraw);
        }));
        self.tree = Some(tree);
        self.tree_synced = Instant::now();
        self.relayout();
    }

    /// Shows and focuses the tree, or hides it when it has focus.
    fn toggle_tree(&mut self) {
        if !self.features.file_tree {
            return;
        }
        if self.tree.is_none() {
            self.show_tree();
            self.set_tree_focus(true);
        } else if self.tree_focused {
            self.tree = None;
            self.set_tree_focus(false);
            self.relayout();
        } else {
            self.sync_tree(true);
            self.set_tree_focus(true);
        }
    }

    fn set_tree_focus(&mut self, focused: bool) {
        let focused = focused && self.tree.is_some();
        if let Some(tree) = &mut self.tree {
            tree.set_focused(focused);
        }
        if focused != self.tree_focused {
            self.tree_focused = focused;
            if !self.active_pad.is_zero() {
                self.relayout();
            } else {
                self.window.request_redraw();
            }
        }
    }

    fn active_cwd(&self) -> PathBuf {
        self.pane_cwd(self.active)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Points the tree at the active pane's directory. The listing itself is
    /// re-read at most once a second unless `force`d, since this runs on
    /// every bit of output.
    fn sync_tree(&mut self, force: bool) {
        if self.tree.is_none() {
            return;
        }
        let cwd = self.active_cwd();
        let stale = force || self.tree_synced.elapsed() >= Duration::from_secs(1);
        let Some(tree) = &mut self.tree else { return };
        if tree.root() != cwd {
            tree.set_root(cwd);
        } else if stale {
            // A filtered listing walks the whole tree; it updates on typing.
            if !tree.has_filter() {
                tree.refresh();
            }
            tree.refresh_git(force);
        } else {
            return;
        }
        self.tree_synced = Instant::now();
        self.window.request_redraw();
    }

    fn tree_rows(&self) -> usize {
        self.tree_rect()
            .map(|t| self.tree_grid_count(t).1.max(1))
            .unwrap_or(1)
    }

    fn handle_tree_key(&mut self, event: &KeyEvent, config: &Config) {
        use crate::ui::tree::PromptKind;
        use winit::keyboard::NamedKey;
        let lang = self.lang.clone();
        let page = self.tree_rows().saturating_sub(2).max(1) as isize;
        let text = event
            .text
            .as_deref()
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
        let Some(tree) = &mut self.tree else { return };
        tree.clear_message();
        let key = event.logical_key.as_ref();

        if tree.prompt_kind().is_some() {
            match key {
                Key::Named(NamedKey::Escape) => tree.cancel_prompt(),
                Key::Named(NamedKey::Enter) => tree.commit_prompt(&lang),
                Key::Named(NamedKey::Backspace) => tree.prompt_backspace(),
                _ => {
                    if let Some(text) = text {
                        tree.prompt_input(text, &lang);
                    }
                }
            }
            self.window.request_redraw();
            return;
        }
        if tree.filtering() {
            match key {
                Key::Named(NamedKey::Escape) => tree.clear_filter(),
                Key::Named(NamedKey::Enter) => tree.finish_filter(),
                Key::Named(NamedKey::Backspace) => tree.filter_backspace(),
                Key::Named(NamedKey::ArrowUp) => tree.move_selection(-1),
                Key::Named(NamedKey::ArrowDown) => tree.move_selection(1),
                _ => {
                    if let Some(text) = text {
                        tree.filter_input(text);
                    }
                }
            }
            self.window.request_redraw();
            return;
        }

        let last = tree.entries().len().saturating_sub(1);
        match key {
            Key::Named(NamedKey::ArrowUp) | Key::Character("k") => tree.move_selection(-1),
            Key::Named(NamedKey::ArrowDown) | Key::Character("j") => tree.move_selection(1),
            Key::Named(NamedKey::PageUp) => tree.move_selection(-page),
            Key::Named(NamedKey::PageDown) => tree.move_selection(page),
            Key::Named(NamedKey::Home) | Key::Character("g") => tree.select(0),
            Key::Named(NamedKey::End) | Key::Character("G") => tree.select(last),
            Key::Named(NamedKey::ArrowRight) | Key::Character("l") => tree.expand(),
            Key::Named(NamedKey::ArrowLeft) | Key::Character("h") => tree.collapse(),
            Key::Named(NamedKey::Space) => tree.toggle(),
            Key::Character(".") => tree.toggle_hidden(),
            Key::Character("r") => {
                tree.refresh();
                tree.refresh_git(true);
            }
            Key::Character("/") => tree.start_filter(),
            Key::Character("a") => tree.start_prompt(PromptKind::Create),
            Key::Character("R") => tree.start_prompt(PromptKind::Rename),
            Key::Character("m") => tree.start_prompt(PromptKind::Move),
            Key::Character("C") => tree.start_prompt(PromptKind::Copy),
            Key::Character("d") => tree.start_prompt(PromptKind::Trash),
            Key::Character("D") => tree.start_prompt(PromptKind::Delete),
            Key::Named(NamedKey::Escape) if tree.has_filter() => tree.clear_filter(),
            Key::Named(NamedKey::Enter) | Key::Character("o") => self.tree_open(config),
            Key::Character("s") if self.features.file_tree && self.features.tree_open_split => {
                self.tree_open_split(config)
            }
            Key::Character("c") => self.tree_cd(),
            Key::Character("i") => self.tree_insert(),
            Key::Character("y") => self.tree_copy(),
            Key::Named(NamedKey::Escape) | Key::Character("q") => self.set_tree_focus(false),
            _ => {}
        }
        self.window.request_redraw();
    }

    /// Enter on a folder opens or closes it; on a file it opens the editor in
    /// the active pane, or in a new pane when a full-screen program runs there.
    fn tree_open(&mut self, config: &Config) {
        let Some(entry) = self.tree.as_ref().and_then(|t| t.selected()).cloned() else {
            return;
        };
        if entry.is_dir {
            if let Some(tree) = &mut self.tree {
                tree.toggle();
            }
            return;
        }
        self.open_in_editor(&entry.path, config);
        self.set_tree_focus(false);
    }

    /// Opens the selection in a new pane: a directory with `cd`, a file in
    /// `$EDITOR`.
    fn tree_open_split(&mut self, config: &Config) {
        let Some(entry) = self.tree.as_ref().and_then(|t| t.selected()).cloned() else {
            return;
        };
        let rect = self.panes[&self.active].rect;
        self.split(
            if rect.w >= rect.h {
                Axis::Horizontal
            } else {
                Axis::Vertical
            },
            config,
        );
        if entry.is_dir {
            self.scroll_to_bottom();
            self.write_pty(format!("\x15cd {}\r", tree::shell_quote(&entry.path)).as_bytes());
        } else {
            let command = format!("${{EDITOR:-vi}} {}\r", tree::shell_quote(&entry.path));
            self.scroll_to_bottom();
            self.write_pty(format!("\x15{command}").as_bytes());
        }
        self.set_tree_focus(false);
    }

    /// `cd` the active pane's shell into the selected folder (or the file's).
    fn tree_cd(&mut self) {
        let Some(entry) = self.tree.as_ref().and_then(|t| t.selected()).cloned() else {
            return;
        };
        let dir = if entry.is_dir {
            entry.path
        } else {
            entry
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(entry.path)
        };
        if !self.term().is_alt_screen() {
            self.scroll_to_bottom();
            self.write_pty(format!("\x15cd {}\r", tree::shell_quote(&dir)).as_bytes());
        }
    }

    /// Types the selected path at the active pane's cursor.
    fn tree_insert(&mut self) {
        let Some(entry) = self.tree.as_ref().and_then(|t| t.selected()).cloned() else {
            return;
        };
        self.set_tree_focus(false);
        self.paste(&format!("{} ", tree::shell_quote(&entry.path)));
    }

    fn tree_copy(&mut self) {
        let Some(entry) = self.tree.as_ref().and_then(|t| t.selected()).cloned() else {
            return;
        };
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(entry.path.to_string_lossy().into_owned());
        }
    }

    /// Click: select (and open/close a folder); double click opens a file.
    fn tree_click(&mut self, y: f32, config: &Config) {
        let Some(t) = self.tree_rect() else { return };
        let (_, ch) = self.renderer.cell_size();
        let b = self.grid_box(t, self.tree_pad);
        let rows = self.tree_grid_count(t).1.max(1);
        let stride = crate::renderer::grid_pitch(b.top, b.bottom, rows, ch);
        let row = ((y - b.top) / stride).floor().max(0.0) as usize;
        self.set_tree_focus(true);
        let Some(tree) = &mut self.tree else { return };
        let Some(index) = tree.index_at_row(row) else {
            return;
        };
        let double = self
            .tree_click
            .is_some_and(|(i, at)| i == index && at.elapsed() < Duration::from_millis(400));
        self.tree_click = Some((index, Instant::now()));
        tree.select(index);
        if tree.entries()[index].is_dir {
            tree.toggle();
        } else if double {
            self.tree_open(config);
        }
        self.window.request_redraw();
    }

    fn cycle_focus(&mut self, step: isize) {
        let ids = self.layout.ids();
        let index = ids.iter().position(|&p| p == self.active).unwrap_or(0) as isize;
        let next = (index + step).rem_euclid(ids.len() as isize) as usize;
        self.set_active(ids[next]);
    }

    fn update_title(&self) {
        let title = self.term().title.clone();
        self.window
            .set_title(if title.is_empty() { "kodama" } else { &title });
    }

    // ---- bell and notifications -------------------------------------------------

    /// Pane `id` rang the bell.
    fn ring_bell(&mut self, id: PaneId, bell: &BellConfig) {
        const FLASH: Duration = Duration::from_millis(150);
        const COMMAND_EVERY: Duration = Duration::from_millis(500);
        if !self.layout.ids().contains(&id) {
            self.rang.insert(id);
        } else if bell.visual {
            self.bell_flash = Some((id, Instant::now() + FLASH));
        }
        if bell.urgent && !self.focused {
            self.window
                .request_user_attention(Some(winit::window::UserAttentionType::Informational));
        }
        if let Some(command) = bell.command.as_deref().filter(|c| !c.trim().is_empty())
            && self
                .bell_command_at
                .is_none_or(|at| at.elapsed() >= COMMAND_EVERY)
        {
            self.bell_command_at = Some(Instant::now());
            spawn_detached("sh", &["-c", command]);
        }
        self.window.request_redraw();
    }

    /// Shows pane `id`'s notifications on the desktop (with `notify-send`).
    /// When notification focus is on, clicking the notification focuses the
    /// pane. The wait happens off the event loop.
    fn notify(&self, id: PaneId, notes: Vec<(String, String)>, mode: Notifications) {
        let show = match mode {
            Notifications::Always => true,
            Notifications::Unfocused => !self.focused || id != self.active,
            Notifications::Never => false,
        };
        if !show {
            return;
        }
        for (title, body) in notes {
            let title = if title.is_empty() {
                let pane_title = self.term_of(id).title.clone();
                if pane_title.is_empty() {
                    "kodama".to_string()
                } else {
                    pane_title
                }
            } else {
                title
            };
            if !self.features.notification_focus {
                spawn_detached("notify-send", &["--app-name=kodama", "--", &title, &body]);
                continue;
            }
            let proxy = self.proxy.clone();
            thread::spawn(move || {
                let output = std::process::Command::new("notify-send")
                    .args([
                        "--app-name=kodama",
                        "--action=default=Show",
                        "--",
                        &title,
                        &body,
                    ])
                    .output();
                match output {
                    Ok(output) if output.status.success() => {
                        let chosen = String::from_utf8_lossy(&output.stdout);
                        if chosen.trim() == "default" {
                            let _ = proxy.send_event(UserEvent::FocusPane(id));
                        }
                    }
                    Ok(output) => {
                        log::warn!(
                            "notify-send: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        );
                        // The action flag was refused, so show the note plainly.
                        let _ = std::process::Command::new("notify-send")
                            .args(["--app-name=kodama", "--", &title, &body])
                            .status();
                    }
                    Err(e) => log::warn!("notify-send: {e}"),
                }
            });
        }
    }

    /// Switches to the tab that holds `id` and focuses that pane.
    fn focus_saved_pane(&mut self, id: PaneId) {
        if !self.layout.ids().contains(&id) {
            let Some(index) = self
                .tabs
                .iter()
                .enumerate()
                .find(|(i, tab)| *i != self.tab_index && tab.layout.ids().contains(&id))
                .map(|(i, _)| i)
            else {
                return;
            };
            self.switch_tab(index);
        }
        if self.layout.ids().contains(&id) {
            self.set_active(id);
        }
        self.window.focus_window();
    }

    /// Whether tab `index` (not the current one) has a pane that rang.
    fn tab_rang(&self, index: usize) -> bool {
        index != self.tab_index
            && self
                .tabs
                .get(index)
                .is_some_and(|t| t.layout.ids().iter().any(|id| self.rang.contains(id)))
    }

    // ---- terminal actions ---------------------------------------------------

    fn set_font_size(&mut self, pt: f32) {
        self.font_pt = pt.clamp(4.0, 72.0);
        self.renderer
            .set_font_size(pt_to_px(self.font_pt, self.scale));
        self.relayout();
    }

    /// Cell of pane `id` under the given window position, clamped to its grid.
    fn cell_at(&self, id: PaneId, pos: PhysicalPosition<f64>) -> (usize, usize) {
        let (cw, ch) = self.renderer.cell_size();
        let rect = self.panes[&id].rect;
        let b = self.grid_box(rect, self.insets_for_pane(id));
        let term = self.term_of(id);
        let col_stride = crate::renderer::grid_pitch(b.left, b.right, term.cols(), cw);
        let row_stride = crate::renderer::grid_pitch(b.top, b.bottom, term.rows(), ch);
        let col = ((pos.x - b.left as f64) / col_stride as f64)
            .floor()
            .max(0.0) as usize;
        let row = ((pos.y - b.top as f64) / row_stride as f64)
            .floor()
            .max(0.0) as usize;
        (col.min(term.cols() - 1), row.min(term.rows() - 1))
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.term().selected_text() else {
            return;
        };
        if let Some(cb) = &mut self.clipboard
            && let Err(e) = cb.set_text(text)
        {
            log::warn!("copy failed: {e}");
        }
    }

    #[cfg(target_os = "linux")]
    fn set_primary_selection(&mut self) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        let Some(text) = self.term().selected_text() else {
            return;
        };
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set().clipboard(LinuxClipboardKind::Primary).text(text);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn set_primary_selection(&mut self) {}

    fn paste_from(&mut self, primary: bool) {
        let Some(cb) = &mut self.clipboard else {
            return;
        };
        #[cfg(target_os = "linux")]
        let text = if primary {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            cb.get().clipboard(LinuxClipboardKind::Primary).text()
        } else {
            cb.get_text()
        };
        #[cfg(not(target_os = "linux"))]
        let text = {
            let _ = primary;
            cb.get_text()
        };
        if let Ok(text) = text {
            self.paste(&text);
        }
    }

    fn paste(&mut self, text: &str) {
        let bracketed = self.term().modes.bracketed_paste;
        let body = text.replace("\r\n", "\r").replace('\n', "\r");
        let mut bytes = Vec::with_capacity(body.len() + 12);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(body.replace("\x1b[201~", "").as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            bytes.extend_from_slice(body.as_bytes());
        }
        self.scroll_to_bottom();
        self.write_input(&bytes);
    }

    fn scroll_to_bottom(&mut self) {
        let mut term = self.term();
        if term.display_offset != 0 {
            term.display_offset = 0;
            drop(term);
            self.window.request_redraw();
        }
    }

    fn mouse_report(
        &mut self,
        id: PaneId,
        code: u8,
        pressed: bool,
        motion: bool,
        col: usize,
        row: usize,
    ) {
        let sgr = self.term_of(id).modes.mouse_sgr;
        let mods = self.modifiers;
        let mut b = code
            + if motion { 32 } else { 0 }
            + if mods.shift_key() { 4 } else { 0 }
            + if mods.alt_key() { 8 } else { 0 }
            + if mods.control_key() { 16 } else { 0 };
        let bytes = if sgr {
            format!(
                "\x1b[<{b};{};{}{}",
                col + 1,
                row + 1,
                if pressed { 'M' } else { 'm' }
            )
            .into_bytes()
        } else {
            if !pressed {
                b = (b & !0b11) | 3;
            }
            if col + 33 > 255 || row + 33 > 255 {
                return;
            }
            vec![0x1b, b'[', b'M', 32 + b, (33 + col) as u8, (33 + row) as u8]
        };
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.write(&bytes);
        }
    }

    /// Returns true when the config should be reloaded.
    fn handle_key(&mut self, event: &KeyEvent, config: &Config, lua: &Lua) -> bool {
        if self.confirm_quit {
            use winit::keyboard::NamedKey;
            match event.logical_key.as_ref() {
                Key::Character("y" | "Y") | Key::Named(NamedKey::Enter) => self.quit = true,
                // Modifier presses alone don't answer.
                Key::Named(
                    NamedKey::Shift | NamedKey::Control | NamedKey::Alt | NamedKey::Super,
                ) => {}
                _ => self.confirm_quit = false,
            }
            self.window.request_redraw();
            return false;
        }
        let (has_selection, alt_screen) = {
            let term = self.term();
            (term.selection.is_some(), term.is_alt_screen())
        };
        if self.renaming.is_some() {
            self.handle_rename_key(event);
            return false;
        }
        if self.palette.is_some() {
            return self.handle_palette_key(event, config, lua);
        }
        if self.quick_select.is_some() {
            self.handle_quick_key(event, config);
            return false;
        }
        if self.search.is_some() {
            return self.handle_search_key(event, config, lua);
        }
        if self.copy_mode.is_some() {
            return self.handle_copy_key(event, config, lua);
        }
        let binding = config.find_binding(&key_name(event), mod_bits(self.modifiers));
        if self.tree_focused {
            match binding {
                Some(Action::None | Action::CopyOrPass | Action::PasteOrPass) | None => {
                    self.handle_tree_key(event, config);
                    return false;
                }
                Some(action) => return self.perform(action.clone(), config, lua),
            }
        }
        match binding {
            Some(Action::None) | None => {}
            Some(Action::CopyOrPass) if !has_selection => {}
            Some(Action::PasteOrPass) if alt_screen => {}
            Some(action) => return self.perform(action.clone(), config, lua),
        }
        let bytes = {
            let term = self.term();
            input::encode_key(event, self.modifiers, &term.modes, term.keyboard_flags())
        };
        if let Some(bytes) = bytes {
            self.scroll_to_bottom();
            self.write_input(&bytes);
        }
        false
    }

    /// Scrolls the viewport through history; a no-op on the alternate screen.
    fn scroll_view(&mut self, lines: isize) {
        let mut term = self.term();
        if !term.is_alt_screen() {
            term.scroll_display(lines);
            drop(term);
            self.window.request_redraw();
        }
    }

    /// Returns true when the config should be reloaded.
    fn perform(&mut self, action: Action, config: &Config, lua: &Lua) -> bool {
        if self.panes.is_empty() {
            return false;
        }
        let page = self.term().rows() as isize - 1;
        match action {
            Action::None => {}
            Action::Copy => self.copy_selection(),
            Action::Paste => self.paste_from(false),
            Action::PastePrimary => self.paste_from(true),
            Action::CopyOrPass => {
                // Dropping the highlight shows the copy happened, and the
                // next ctrl+c interrupts again.
                self.copy_selection();
                self.term().selection = None;
                self.window.request_redraw();
            }
            Action::PasteOrPass => {
                if !self.term().is_alt_screen() {
                    self.paste_from(false);
                }
            }
            Action::IncreaseFontSize => self.set_font_size(self.font_pt + 1.0),
            Action::DecreaseFontSize => self.set_font_size(self.font_pt - 1.0),
            Action::ResetFontSize => self.set_font_size(config.font.size),
            Action::ScrollPageUp => self.scroll_view(page),
            Action::ScrollPageDown => self.scroll_view(-page),
            Action::ScrollLineUp => self.scroll_view(1),
            Action::ScrollLineDown => self.scroll_view(-1),
            Action::ScrollToTop => self.scroll_view(isize::MAX / 2),
            Action::ScrollToBottom => self.scroll_to_bottom(),
            Action::ClearScrollback => {
                self.term().feed(b"\x1b[3J");
                release_freed_memory();
                self.window.request_redraw();
            }
            Action::ReloadConfig => return true,
            Action::SplitRight => self.split(Axis::Horizontal, config),
            Action::SplitDown => self.split(Axis::Vertical, config),
            Action::NewPane => {
                let rect = self.unzoomed_rect(self.active);
                let axis = if rect.w > rect.h {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                let other = match axis {
                    Axis::Horizontal => Axis::Vertical,
                    Axis::Vertical => Axis::Horizontal,
                };
                if self.split_fits(axis) {
                    self.split(axis, config);
                } else if self.split_fits(other) {
                    self.split(other, config);
                } else {
                    log::info!("new pane refused: {}", self.diag());
                }
            }
            Action::MovePane(dir) => {
                if !self.zoomed
                    && let Some(id) =
                        self.layout
                            .neighbor(self.active, dir, self.area(), self.gap())
                {
                    self.layout.swap(self.active, id);
                    self.relayout();
                }
            }
            Action::ClosePane if config.confirm_close && self.panes.len() == 1 => {
                self.request_quit(true)
            }
            Action::ClosePane => self.close_pane(self.active),
            Action::CloseTab if config.confirm_close && self.tabs.len() == 1 => {
                self.request_quit(true)
            }
            Action::FocusPane(dir) => {
                if self.tree_focused {
                    if dir == crate::layout::Direction::Right {
                        self.set_tree_focus(false);
                    }
                } else if let Some(id) =
                    self.layout
                        .neighbor(self.active, dir, self.area(), self.gap())
                {
                    self.set_active(id);
                } else if dir == crate::layout::Direction::Left && self.tree.is_some() {
                    self.set_tree_focus(true);
                }
            }
            Action::ToggleFileTree if self.features.file_tree => self.toggle_tree(),
            Action::Search if self.features.search => self.open_search(),
            Action::CopyMode if self.features.copy_mode => self.toggle_copy_mode(),
            Action::QuickSelect if self.features.quick_select => self.start_quick_select(),
            Action::CommandPalette if self.features.command_palette => self.open_palette(config),
            Action::ToggleFileTree
            | Action::Search
            | Action::CopyMode
            | Action::QuickSelect
            | Action::CommandPalette => {}
            Action::NewWindow => self.new_window(),
            Action::MoveTab(by) => self.move_tab(by),
            Action::RenameTab => self.start_rename(),
            Action::ShowScrollback => self.show_scrollback(config),
            Action::CopyLastCommandOutput => self.copy_last_command_output(),
            Action::ToggleBroadcast => {
                self.broadcast = !self.broadcast;
                self.window.request_redraw();
            }
            Action::ToggleFullscreen => {
                let full = self.window.fullscreen().is_none();
                self.window
                    .set_fullscreen(full.then_some(winit::window::Fullscreen::Borderless(None)));
            }
            Action::ScrollToPrevPrompt | Action::ScrollToNextPrompt => {
                let up = matches!(action, Action::ScrollToPrevPrompt);
                if self.term().scroll_to_prompt(up) {
                    self.window.request_redraw();
                }
            }
            Action::NewTab => self.new_tab(config, None),
            Action::NextTab => self.switch_tab((self.tab_index + 1) % self.tabs.len()),
            Action::PrevTab => {
                self.switch_tab((self.tab_index + self.tabs.len() - 1) % self.tabs.len())
            }
            Action::GoToTab(n) => self.switch_tab(n - 1),
            Action::CloseTab => self.close_tab(),
            Action::FocusNextPane => self.cycle_focus(1),
            Action::FocusPrevPane => self.cycle_focus(-1),
            Action::ResizePane(dir) => {
                // The tree is the focused column, so the same keys move its edge.
                if self.tree_focused
                    && matches!(
                        dir,
                        crate::layout::Direction::Left | crate::layout::Direction::Right
                    )
                {
                    self.resize_tree(dir);
                } else if !self.zoomed {
                    self.layout.resize(self.active, dir, 0.05);
                    self.relayout();
                }
            }
            Action::TogglePaneZoom => {
                if self.panes.len() > 1 {
                    self.zoomed = !self.zoomed;
                    self.relayout();
                }
            }
            Action::Send(text) => {
                self.scroll_to_bottom();
                self.write_input(text.as_bytes());
            }
            Action::Lua(func) => {
                let info = {
                    let term = self.term();
                    WindowInfo {
                        cols: term.cols(),
                        rows: term.rows(),
                        font_size: self.font_pt,
                        title: term.title.clone(),
                        selection: term.selected_text(),
                        alt_screen: term.is_alt_screen(),
                        pane_count: self.panes.len(),
                    }
                };
                let commands = match lua::call_action(lua, &func, info) {
                    Ok(commands) => commands,
                    Err(e) => {
                        self.show_message(&format!("lua error: {e}"));
                        return false;
                    }
                };
                let mut reload = false;
                for command in commands {
                    reload |= match command {
                        Command::Action(action) => self.perform(action, config, lua),
                        Command::SetFontSize(pt) => {
                            self.set_font_size(pt);
                            false
                        }
                        Command::Scroll(lines) => {
                            self.scroll_view(lines);
                            false
                        }
                    };
                }
                return reload;
            }
        }
        false
    }

    /// Asks to confirm closing the window; asking again (a second close
    /// request) closes. Without `confirm` it closes right away.
    fn request_quit(&mut self, confirm: bool) {
        if !confirm || self.confirm_quit {
            self.quit = true;
        } else {
            self.confirm_quit = true;
            self.window.request_redraw();
        }
    }

    /// The close confirmation along the bottom of the window, from corner
    /// to corner: the bar, its rect and its backdrop.
    fn quit_bar(&self, colors: &Colors) -> Option<(Terminal, Rect, Rect)> {
        if !self.confirm_quit {
            return None;
        }
        let (w, h) = self.renderer.surface_size();
        let full = content_area(w, h, self.window_pad);
        let [r, g, b] = colors.active_border;
        let panes = self.panes.len();
        let n = panes.to_string();
        let what = if panes == 1 {
            self.lang.get("quit.shell").to_string()
        } else {
            self.lang.format("quit.panes", &[("n", &n)])
        };
        let detail = self.lang.format("quit.detail", &[("what", &what)]);
        let text = format!(
            " \x1b[1;38;2;{r};{g};{b}m\u{f011} {}\x1b[22;39m {detail}  {}",
            self.lang.get("quit.title"),
            i18n::emphasize_keys(self.lang.get("quit.hint"))
        );
        let window = Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        };
        let (backdrop, r, cols) = self.bottom_bar(full, window);
        let mut bar = bar_terminal(cols, colors.tab_bar, colors.foreground);
        bar.feed(text.as_bytes());
        Some((bar, r, backdrop))
    }

    /// Prints a kodama message into the active pane so mistakes are visible.
    fn show_message(&mut self, message: &str) {
        let mut term = self.term();
        let at_line_start = term.cursor_position().0 == 0;
        let mut text = String::new();
        if !at_line_start {
            text.push_str("\r\n");
        }
        text.push_str(&error_banner(message));
        term.feed(text.as_bytes());
        drop(term);
        self.window.request_redraw();
    }

    fn apply_config(&mut self, old: &Config, new: &Config) {
        if old.font.family != new.font.family || old.font.fallback != new.font.fallback {
            self.font_pt = new.font.size;
            let font = FontSet::new(
                &new.font.family,
                pt_to_px(self.font_pt, self.scale),
                &new.font.fallback,
            );
            self.renderer.set_font(font);
        } else if old.font.size != new.font.size {
            self.font_pt = new.font.size;
            self.renderer
                .set_font_size(pt_to_px(self.font_pt, self.scale));
        }
        self.renderer.ligatures = new.font.ligatures;
        self.renderer.opacity = new.window.opacity;
        if old.window.background_image != new.window.background_image
            && let Err(e) = set_background_image(&mut self.renderer, &new.window, &self.config_path)
        {
            self.show_message(&e);
        }
        self.renderer.background_image_opacity = new.window.background_image_opacity;
        self.renderer.background_image_fit = new.window.background_image_fit;
        self.set_spacing(new);
        if let Some(tree) = &mut self.tree
            && old.tree.show_hidden != new.tree.show_hidden
            && tree.show_hidden != new.tree.show_hidden
        {
            tree.toggle_hidden();
        }
        // A width set by dragging stays until the config's own one changes.
        let width = if old.tree.width == new.tree.width {
            self.tree_config.width
        } else {
            new.tree.width
        };
        self.tree_config = crate::config::TreeConfig {
            width,
            ..new.tree.clone()
        };
        for pane in self.panes.values() {
            configure_terminal(&mut pane.term(), new);
        }
        if old.shell != new.shell || old.env != new.env {
            log::info!("shell/env changes apply to new panes only");
        }
        self.relayout();
    }

    // ---- mouse ----------------------------------------------------------------

    /// Which tab the pointer is over. `x` is clamped into the bar, so a drag
    /// that leaves the bar lands on the tab at that end.
    fn tab_index_at(&self, x: f32) -> Option<usize> {
        let bar = self.tab_bar_rect()?;
        let x = x.clamp(bar.x, (bar.x + bar.w - 0.01).max(bar.x));
        if self.tab_bar_config.style == TabStyle::Plain {
            let line = (self.scale as f32).round().max(1.0);
            return slice_index(x, bar.x, bar.w, self.tab_spans.len(), line);
        }
        let (cw, _) = self.renderer.cell_size();
        let cols = (bar.w / cw).floor() as usize;
        let pitch = crate::renderer::grid_pitch(bar.x, bar.x + bar.w, cols, cw);
        let mut col = ((x - bar.x) / pitch) as usize;
        // A last tab that reaches the end also takes the pixels past its last
        // column.
        if let Some(&(_, end)) = self.tab_spans.last()
            && end == cols
        {
            col = col.min(end.saturating_sub(1));
        }
        self.tab_spans
            .iter()
            .position(|&(a, b)| (a..b).contains(&col))
    }

    /// How far the pointer must travel before a click becomes a drag.
    fn drag_started(&self, start: (f32, f32), x: f32, y: f32) -> bool {
        let scale = (self.scale as f32).max(1.0);
        (x - start.0).abs().max((y - start.1).abs()) >= 4.0 * scale
    }

    fn drag_tab(&mut self, x: f32, y: f32) {
        let Some(Drag::Tab { start, moved }) = self.drag else {
            return;
        };
        if !moved && !self.drag_started(start, x, y) {
            return;
        }
        self.drag = Some(Drag::Tab { start, moved: true });
        if let Some(index) = self.tab_index_at(x) {
            self.move_tab_to(index);
        }
        self.window.set_cursor(CursorIcon::Grabbing);
    }

    fn drag_pane(&mut self, x: f32, y: f32) {
        let Some(Drag::Pane { id, start, moved }) = self.drag else {
            return;
        };
        if !moved && !self.drag_started(start, x, y) {
            return;
        }
        self.drag = Some(Drag::Pane {
            id,
            start,
            moved: true,
        });
        self.window.set_cursor(CursorIcon::Grabbing);
    }

    /// A pane drag that travelled swaps with the pane under the pointer.
    fn finish_drag(&mut self, x: f32, y: f32) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if let Drag::Pane {
            id, moved: true, ..
        } = drag
            && let Some(other) = self.pane_at(x, y)
            && other != id
            && self.layout.ids().contains(&id)
        {
            self.layout.swap(id, other);
            self.relayout();
        }
    }

    fn handle_mouse_button(&mut self, button: MouseButton, state: ElementState, config: &Config) {
        let pressed = state == ElementState::Pressed;
        let (x, y) = (self.cursor_pos.x as f32, self.cursor_pos.y as f32);
        if !pressed && std::mem::take(&mut self.swallow_release) {
            return;
        }
        // Wherever the pointer is let go (even over the tab bar or tree).
        if !pressed && button == MouseButton::Left && self.drag.is_some() {
            self.finish_drag(x, y);
            release_freed_memory();
            return;
        }
        if pressed
            && button == MouseButton::Left
            && let Some((.., target, _, _)) = self.hover.clone()
        {
            open_link(&target);
            self.swallow_release = true;
            return;
        }
        if let Some(bar) = self.tab_bar_rect()
            && bar.contains(x, y)
        {
            if pressed && let Some(i) = self.tab_index_at(x) {
                match button {
                    MouseButton::Left => {
                        self.switch_tab(i);
                        if self.features.drag_reorder {
                            self.drag = Some(Drag::Tab {
                                start: (x, y),
                                moved: false,
                            });
                        }
                    }
                    MouseButton::Middle => {
                        self.switch_tab(i);
                        self.close_tab();
                    }
                    _ => {}
                }
            }
            return;
        }
        if button == MouseButton::Left
            && pressed
            && self.select_anchor.is_none()
            && self.tree_handle_at(x, y)
        {
            self.drag = Some(Drag::Tree);
            return;
        }
        if self.drag.is_none()
            && self.select_anchor.is_none()
            && self.tree_hit_rect().is_some_and(|t| t.contains(x, y))
        {
            if pressed && button == MouseButton::Left {
                self.tree_click(y, config);
            }
            return;
        }
        if button == MouseButton::Left
            && pressed
            && let Some(divider) = self.divider_at(x, y)
        {
            self.drag = Some(Drag::Split(divider));
            return;
        }
        if button == MouseButton::Left
            && pressed
            && self.features.drag_reorder
            && self.modifiers.alt_key()
            && let Some(id) = self.pane_at(x, y)
        {
            self.set_active(id);
            self.drag = Some(Drag::Pane {
                id,
                start: (x, y),
                moved: false,
            });
            return;
        }
        if pressed {
            match self.pane_at(x, y) {
                Some(id) => self.set_active(id),
                None => return,
            }
        }

        let id = self.active;
        let (col, row) = self.cell_at(id, self.cursor_pos);
        let reporting = self.term().modes.mouse != MouseMode::None && !self.modifiers.shift_key();

        if reporting {
            let code = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                _ => return,
            };
            self.reported_button = pressed.then_some(code);
            self.last_reported_cell = Some((col, row));
            self.mouse_report(id, code, pressed, false, col, row);
            return;
        }

        match (button, pressed) {
            (MouseButton::Left, true) => {
                let mut term = self.term();
                let point = Point {
                    line: term.visible_line(row),
                    col,
                };
                term.selection = None;
                drop(term);
                self.select_anchor = Some(point);
                self.window.request_redraw();
            }
            (MouseButton::Left, false) => {
                if self.select_anchor.take().is_some() && self.term().selection.is_some() {
                    self.set_primary_selection();
                }
            }
            (MouseButton::Middle, true) => self.paste_from(true),
            _ => {}
        }
    }

    fn handle_cursor_moved(&mut self, pos: PhysicalPosition<f64>) {
        self.cursor_pos = pos;
        let (x, y) = (pos.x as f32, pos.y as f32);

        if let Some(Drag::Tree) = self.drag {
            self.drag_tree_edge(x);
            return;
        }
        if matches!(self.drag, Some(Drag::Tab { .. })) {
            self.drag_tab(x, y);
            return;
        }
        if matches!(self.drag, Some(Drag::Pane { .. })) {
            self.drag_pane(x, y);
            return;
        }
        if let Some(Drag::Split(divider)) = &self.drag {
            let gap = self.gap();
            let p = divider.parent;
            let ratio = match divider.axis {
                Axis::Horizontal => (x - p.x - gap / 2.0) / (p.w - gap),
                Axis::Vertical => (y - p.y - gap / 2.0) / (p.h - gap),
            };
            let path = divider.path.clone();
            self.layout.set_ratio(&path, ratio);
            self.relayout();
            return;
        }
        self.refresh_pointer();
        let over_tree = self.tree_hit_rect().is_some_and(|t| t.contains(x, y));
        let over_bar = self.tab_bar_rect().is_some_and(|t| t.contains(x, y));
        if (over_tree || over_bar) && self.select_anchor.is_none() {
            return;
        }
        let id = self.active;
        let (col, row) = self.cell_at(id, pos);
        let mode = self.term().modes.mouse;

        if mode != MouseMode::None && !self.modifiers.shift_key() {
            let report = match mode {
                MouseMode::Drag => self.reported_button,
                MouseMode::Motion => Some(self.reported_button.unwrap_or(3)),
                _ => None,
            };
            if let Some(code) = report
                && self.last_reported_cell != Some((col, row))
            {
                self.last_reported_cell = Some((col, row));
                self.mouse_report(id, code, true, true, col, row);
            }
            return;
        }

        if let Some(anchor) = self.select_anchor {
            let mut term = self.term();
            let head = Point {
                line: term.visible_line(row),
                col,
            };
            if term.selection.is_some() || head != anchor {
                term.selection = Some(Selection { anchor, head });
                drop(term);
                self.window.request_redraw();
            }
        }
    }

    /// Scrolls the pane under the pointer, focused or not.
    fn handle_wheel(&mut self, delta: MouseScrollDelta) {
        let (_, ch) = self.renderer.cell_size();
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y as f64 * 3.0,
            MouseScrollDelta::PixelDelta(p) => p.y / ch as f64,
        };
        self.scroll_accum += lines;
        let steps = self.scroll_accum.trunc() as isize;
        if steps == 0 {
            return;
        }
        self.scroll_accum -= steps as f64;

        let pos = self.cursor_pos;
        if self
            .tree_hit_rect()
            .is_some_and(|t| t.contains(pos.x as f32, pos.y as f32))
        {
            if let Some(tree) = &mut self.tree {
                tree.scroll_by(-steps);
            }
            self.window.request_redraw();
            return;
        }
        let id = self
            .pane_at(pos.x as f32, pos.y as f32)
            .unwrap_or(self.active);
        let (mode, alt, app_cursor) = {
            let term = self.term_of(id);
            (
                term.modes.mouse,
                term.is_alt_screen(),
                term.modes.app_cursor,
            )
        };
        if mode != MouseMode::None && !self.modifiers.shift_key() {
            let (col, row) = self.cell_at(id, pos);
            let code = if steps > 0 { 64 } else { 65 };
            for _ in 0..steps.unsigned_abs() {
                self.mouse_report(id, code, true, false, col, row);
            }
        } else if alt {
            // Alternate scroll: translate the wheel into arrow keys (less, man, ...).
            let seq: &[u8] = match (steps > 0, app_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let bytes = seq.repeat(steps.unsigned_abs());
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.write(&bytes);
            }
        } else {
            self.term_of(id).scroll_display(steps);
            self.window.request_redraw();
        }
    }

    // ---- drawing ----------------------------------------------------------------

    fn redraw(&mut self, colors: &Colors) {
        let mut dividers = Vec::new();
        let tree_divider = self.tree_divider().filter(|_| self.split_width > 0.0);
        // Divider lines run through the window margin to the window's edges
        // (below the tab bar when it is shown), like the focus background.
        let (win_w, win_h) = self.renderer.surface_size();
        let top = self.tab_bar_rect().map_or(0.0, |_| self.base_area().y);
        let mut bounds = Rect {
            x: 0.0,
            y: top,
            w: win_w,
            h: (win_h - top).max(1.0),
        };
        if let Some(line) = tree_divider {
            dividers.push((line, colors.split));
            bounds = Rect {
                x: line.x + line.w,
                w: (win_w - line.x - line.w).max(1.0),
                ..bounds
            };
        }
        if !self.zoomed && self.split_width > 0.0 {
            let found = self.layout.dividers(self.area(), self.gap());
            for line in divider_lines(
                &found,
                self.area(),
                bounds,
                self.split_width,
                self.split_padding,
            ) {
                dividers.push((line, colors.split));
            }
        }
        let visible: Vec<PaneId> = self.visible_rects().into_iter().map(|(id, _)| id).collect();
        let tree_rect = self.tree_rect();
        let focus_rect = match tree_rect {
            Some(t) if self.tree_focused => Some(t),
            _ if visible.len() > 1 || tree_rect.is_some() => {
                self.panes.get(&self.active).map(|p| p.rect)
            }
            _ => None,
        };
        // One pane has nothing to stand out from, except a highlight that
        // is asked to cover the window padding as well.
        let fill_rect = if let Some(rect) = focus_rect {
            Some(rect)
        } else if self.highlight_ignores_padding
            && (self.highlight.background() || self.highlight.border())
        {
            self.panes.get(&self.active).map(|pane| pane.rect)
        } else {
            None
        };
        if let Some(rect) = fill_rect {
            let border = if self.highlight.border() {
                self.active_border
            } else {
                0.0
            };
            let is_tree = tree_rect.is_some_and(|t| t == rect);
            let pad = if is_tree {
                self.tree_pad
            } else {
                self.insets_for_pane(self.active)
            };
            // The focused pane and the focused tree share this. With
            // `highlight_ignores_padding`, the background covers that whole
            // area and the border is only a stroke of `active_border` on
            // its outer edge. Otherwise the frame stays in the divider gap,
            // or just outside the grid when the grid has its own padding.
            let (fill, frame) = if self.highlight_ignores_padding {
                border_frame(self.highlight_extent(rect), border)
            } else if pad.is_zero() {
                let (mut fill, frame) = active_frame(rect, border, self.split_padding);
                if is_tree {
                    let (left, right, bottom) = self.outer_span(rect);
                    fill = Rect {
                        x: left,
                        y: rect.y,
                        w: (right - left).max(1.0),
                        h: (bottom - rect.y).max(1.0),
                    };
                }
                (fill, frame)
            } else {
                frame_around(self.grid_box(rect, pad).rect(), border)
            };
            if self.highlight.background() {
                dividers.push((fill, colors.active_background));
            }
            if focus_rect.is_some() || self.highlight_ignores_padding {
                dividers.extend(frame.into_iter().map(|r| (r, colors.active_border)));
            }
        }
        if let Some((id, until)) = self.bell_flash
            && Instant::now() < until
            && visible.contains(&id)
            && let Some(p) = self.panes.get(&id)
        {
            let flash = schemes::mix(colors.background, colors.foreground, 0.2);
            dividers.push((self.highlight_extent(p.rect), flash));
        }
        if self.broadcast {
            let width = self.active_border.max(2.0);
            for id in &visible {
                if let Some(p) = self.panes.get(id) {
                    let (_, frame) = border_frame(self.highlight_extent(p.rect), width);
                    dividers.extend(frame.into_iter().map(|r| (r, colors.ansi[1])));
                }
            }
        }
        let (cw, _) = self.renderer.cell_size();
        let mut tab_lines = Vec::new();
        let tab_bar = self.tab_bar_rect().map(|band| {
            let cols = (band.w / cw).floor() as usize;
            let term = self.tab_bar(cols, colors);
            dividers.push((band, colors.tab_bar));
            // Plain tabs fill the band's height; the other styles' caps are
            // glyphs one row tall, so they stay on the label row.
            if self.tab_bar_config.style == TabStyle::Plain {
                // Each tab is a box: a border between neighbours and one
                // along the bar's bottom, drawn over the label row.
                let line = (self.scale as f32).round().max(1.0);
                let border = schemes::mix(colors.tab_bar, colors.foreground, 0.3);
                tab_lines.push((
                    Rect {
                        y: band.y + band.h - line,
                        h: line,
                        ..band
                    },
                    border,
                ));
                // Equal parts of the band, not of the column grid. Two tabs
                // meet at the midpoint, the same place a half pane split
                // draws its divider when the side margins match.
                let n = self.tab_spans.len();
                for i in 0..n {
                    let (x, w) = equal_slice(band.x, band.w, i, n, line);
                    let color = if i == self.tab_index {
                        colors.tab_active
                    } else {
                        colors.tab_inactive
                    };
                    dividers.push((Rect { x, w, ..band }, color));
                    // The line starts where this box ends, so the colour
                    // stops on the line instead of spilling past it.
                    if i + 1 != n {
                        tab_lines.push((
                            Rect {
                                x: x + w,
                                w: line,
                                ..band
                            },
                            border,
                        ));
                    }
                }
            }
            (term, self.tab_text_rect(band))
        });
        // The close prompt takes the bottom edge. Search, copy mode, quick
        // select and the rename prompt sit there too, so they stay hidden
        // until the prompt is answered.
        let quit_bar = self.quit_bar(colors);
        let search_bar = quit_bar
            .is_none()
            .then(|| {
                self.search.as_ref().and_then(|s| {
                    let rect = visible
                        .contains(&s.pane)
                        .then(|| self.panes.get(&s.pane).map(|p| p.rect))
                        .flatten()?;
                    let (backdrop, r, cols) = self.bottom_bar(rect, self.highlight_extent(rect));
                    let bar = self.search_bar(cols, colors)?;
                    Some((bar, r, backdrop))
                })
            })
            .flatten();
        let rename_bar = quit_bar
            .is_none()
            .then(|| self.rename_bar(colors))
            .flatten();
        let mode_bar = self
            .copy_mode
            .as_ref()
            .map(|c| c.pane)
            .or(self.quick_select.as_ref().map(|q| q.pane))
            .filter(|id| quit_bar.is_none() && self.search.is_none() && visible.contains(id))
            .and_then(|id| {
                let rect = self.panes.get(&id)?.rect;
                let (backdrop, r, cols) = self.bottom_bar(rect, self.highlight_extent(rect));
                let bar = self.mode_bar(id, cols, colors)?;
                Some((bar, r, backdrop))
            });
        let palette_view = self.palette_rect().map(|(r, cols, rows)| {
            let border = scaled(1.0, self.scale).max(1.0);
            let outer = Rect {
                x: r.x - border,
                y: r.y - border,
                w: r.w + 2.0 * border,
                h: r.h + 2.0 * border,
            };
            dividers.push((outer, colors.active_border));
            let view = self
                .palette
                .as_mut()
                .map(|p| p.view(cols, rows, colors))
                .expect("palette is open");
            (view, r)
        });
        // Text typed into the tab name prompt or the palette goes there, so
        // the input method's window follows that cursor.
        let overlay_input = rename_bar
            .as_ref()
            .map(|(term, r, _)| (term, r))
            .or(palette_view.as_ref().map(|(term, r)| (term, r)));
        let palette_ime = overlay_input.map(|(term, r)| {
            let (cw, ch) = self.renderer.cell_size();
            let (cx, cy) = term.cursor_position();
            ((r.x + cx as f32 * cw) as i32, (r.y + cy as f32 * ch) as i32)
        });
        // Grid boxes are taken before the views borrow the panes and the tree.
        // A side with no padding stays flush with the window or the divider;
        // a side with padding keeps that much background outside the cells.
        let pane_boxes: HashMap<PaneId, GridBox> = visible
            .iter()
            .filter_map(|id| {
                self.panes
                    .get(id)
                    .map(|p| (*id, self.grid_box(p.rect, self.insets_for_pane(*id))))
            })
            .collect();
        let tree_box = tree_rect.map(|t| self.grid_box(t, self.tree_pad));
        let tree_dims = tree_rect.map(|t| self.tree_grid_count(t));
        let tree_view = match (self.tree.as_mut(), tree_rect, tree_dims) {
            (Some(tree), Some(t), Some((cols, rows))) => {
                Some((tree.view(cols, rows, colors, &self.lang), t))
            }
            _ => None,
        };

        let cursor = {
            let guards: Vec<(PaneId, Rect, MutexGuard<Terminal>)> = visible
                .iter()
                .filter_map(|id| self.panes.get(id).map(|p| (*id, p.rect, p.term())))
                .collect();
            if guards.iter().any(|(_, _, term)| term.sync_in_progress()) {
                return;
            }
            let pane_focused = self.focused
                && !self.tree_focused
                && self.search.is_none()
                && self.palette.is_none()
                && self.renaming.is_none()
                && self.quick_select.is_none()
                && self.copy_mode.is_none();
            let marks: Vec<Vec<Mark>> = guards
                .iter()
                .map(|(id, _, term)| {
                    Self::marks_for(
                        self.search.as_ref(),
                        self.hover.as_ref(),
                        self.copy_mode.as_ref(),
                        self.quick_select.as_ref(),
                        *id,
                        term,
                    )
                })
                .collect();
            let labels: Vec<Vec<(Point, String)>> = guards
                .iter()
                .map(|(id, ..)| {
                    self.quick_select
                        .as_ref()
                        .filter(|q| q.pane == *id)
                        .map(|q| {
                            q.hits
                                .iter()
                                .filter(|h| h.label.starts_with(&q.typed))
                                .map(|h| (h.start, h.label[q.typed.len()..].to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect();
            let mut views: Vec<PaneView> = guards
                .iter()
                .zip(marks.iter().zip(&labels))
                .map(|((id, _, term), (marks, labels))| {
                    let b = pane_boxes[id];
                    PaneView {
                        term,
                        origin: (b.left, b.top),
                        focused: pane_focused && *id == self.active,
                        cursor_on: self.cursor_blink_on,
                        preedit: if *id == self.active
                            && self.palette.is_none()
                            && self.renaming.is_none()
                        {
                            &self.preedit
                        } else {
                            ""
                        },
                        marks,
                        labels,
                        overlay: false,
                        cell_backgrounds: true,
                        backdrop: None,
                        fill_right: b.right,
                        fill_bottom: b.bottom,
                    }
                })
                .collect();
            let bar = |term, r: Rect, focused, overlay| PaneView {
                term,
                origin: (r.x, r.y),
                focused,
                cursor_on: true,
                preedit: "",
                marks: &[],
                labels: &[],
                overlay,
                cell_backgrounds: true,
                backdrop: None,
                // Bars are exactly one row; stretching them would paint over
                // the pane they sit on.
                fill_right: r.x + r.w,
                fill_bottom: r.y + r.h,
            };
            let bottom = |term, r: Rect, backdrop: Rect, focused| PaneView {
                backdrop: Some((backdrop, colors.tab_bar)),
                ..bar(term, r, focused, true)
            };
            if let Some((term, t)) = tree_view {
                let mut view = bar(term, t, false, false);
                if let Some(b) = tree_box {
                    view.origin = (b.left, b.top);
                    view.fill_right = b.right;
                    view.fill_bottom = b.bottom;
                }
                views.push(view);
            }
            if let Some((term, r)) = &tab_bar {
                let mut view = bar(term, *r, false, false);
                // Plain tabs are painted as boxes. Cell backgrounds follow
                // the column grid and would run past the box into the next tab.
                view.cell_backgrounds = self.tab_bar_config.style != TabStyle::Plain;
                views.push(view);
            }
            if let Some((term, r, b)) = &search_bar {
                views.push(bottom(term, *r, *b, self.focused));
            }
            if let Some((term, r, b)) = &mode_bar {
                views.push(bottom(term, *r, *b, false));
            }
            if let Some((term, r)) = &palette_view {
                views.push(PaneView {
                    preedit: &self.preedit,
                    ..bar(term, *r, self.focused, true)
                });
            }
            if let Some((term, r, b)) = &rename_bar {
                views.push(PaneView {
                    preedit: &self.preedit,
                    ..bottom(term, *r, *b, self.focused)
                });
            }
            if let Some((term, r, b)) = &quit_bar {
                views.push(bottom(term, *r, *b, false));
            }
            self.renderer.prepare(&views, &dividers, &tab_lines, colors);
            guards
                .iter()
                .find(|(id, ..)| *id == self.active)
                .map(|(_, rect, term)| (*rect, term.cursor_position()))
        };
        if !self.renderer.present(colors) {
            self.window.request_redraw();
        }

        let (cw, ch) = self.renderer.cell_size();
        let area = if let Some(area) = palette_ime {
            area
        } else {
            let Some((rect, (cx, cy))) = cursor else {
                return;
            };
            let (cols, rows) = {
                let term = self.term();
                (term.cols(), term.rows())
            };
            let b = self.grid_box(rect, self.insets_for_pane(self.active));
            let col_stride = crate::renderer::grid_pitch(b.left, b.right, cols, cw);
            let row_stride = crate::renderer::grid_pitch(b.top, b.bottom, rows, ch);
            (
                (b.left + cx as f32 * col_stride) as i32,
                (b.top + cy as f32 * row_stride) as i32,
            )
        };
        if self.ime_area != Some(area) {
            self.ime_area = Some(area);
            self.window.set_ime_cursor_area(
                PhysicalPosition::new(area.0, area.1),
                PhysicalSize::new(cw as u32, ch as u32),
            );
        }
    }
}

/// Half-period of the bar cursor. On for this long, then off.
const CURSOR_BLINK: Duration = Duration::from_millis(500);

fn cursor_phase_on(elapsed: Duration) -> bool {
    (elapsed.as_millis() / CURSOR_BLINK.as_millis()).is_multiple_of(2)
}

fn cursor_blink_deadline(epoch: Instant) -> Instant {
    let step = CURSOR_BLINK.as_millis();
    let into = epoch.elapsed().as_millis() % step;
    let remain = if into == 0 { step } else { step - into };
    Instant::now() + Duration::from_millis(remain as u64)
}

impl State {
    /// The bar cursor blinks while the active pane has keyboard focus.
    fn cursor_blinks(&self) -> bool {
        if !self.focused
            || self.tree_focused
            || self.search.is_some()
            || self.palette.is_some()
            || self.renaming.is_some()
            || self.quick_select.is_some()
            || self.copy_mode.is_some()
        {
            return false;
        }
        let Some(pane) = self.panes.get(&self.active) else {
            return false;
        };
        let term = pane.term();
        term.modes.show_cursor && term.cursor_shape == crate::terminal::CursorShape::Bar
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        match self.init(event_loop) {
            Ok(state) => self.state = Some(state),
            Err(e) => {
                log::error!("startup: {e}");
                eprintln!("kodama: {e}");
                event_loop.exit();
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        log::info!("exit");
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        use winit::event_loop::ControlFlow;
        let Some(state) = &mut self.state else { return };
        let now = Instant::now();
        let mut wake = None;
        if let Some((_, until)) = state.bell_flash {
            if now >= until {
                state.bell_flash = None;
                state.window.request_redraw();
            } else {
                wake = Some(until);
            }
        }
        if state.cursor_blinks() {
            let on = cursor_phase_on(state.cursor_epoch.elapsed());
            if on != state.cursor_blink_on {
                state.cursor_blink_on = on;
                state.window.request_redraw();
            }
            let until = cursor_blink_deadline(state.cursor_epoch);
            wake = Some(wake.map_or(until, |at| at.min(until)));
        } else if !state.cursor_blink_on {
            state.cursor_blink_on = true;
            state.window.request_redraw();
        }
        match wake {
            Some(until) => event_loop.set_control_flow(ControlFlow::WaitUntil(until)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        if let UserEvent::ConfigChanged = event {
            self.reload_config();
            return;
        }
        let Some(state) = &mut self.state else { return };
        match event {
            UserEvent::Wakeup(id) => {
                let Some(pane) = state.panes.get(&id) else {
                    return;
                };
                pane.wakeup_pending.store(false, Ordering::Release);
                let (title_changed, clipboard, bell, notes, dropped) = {
                    let mut term = pane.term();
                    (
                        std::mem::take(&mut term.title_changed),
                        std::mem::take(&mut term.clipboard_writes),
                        std::mem::take(&mut term.bell),
                        std::mem::take(&mut term.notifications),
                        term.lines_dropped(),
                    )
                };
                // A full scrollback frees its oldest row for every new one.
                // malloc keeps those holes, so hand them back now and then.
                if dropped.saturating_sub(pane.trimmed_at.get()) >= 16_384 {
                    pane.trimmed_at.set(dropped);
                    release_freed_memory();
                }
                if bell {
                    state.ring_bell(id, &self.config.bell);
                }
                if !notes.is_empty() {
                    state.notify(id, notes, self.config.notifications);
                }
                if title_changed && id == state.active {
                    state.update_title();
                }
                for (primary, text) in clipboard {
                    state.set_clipboard(primary, text);
                }
                if state.search.as_ref().is_some_and(|s| {
                    s.pane == id && s.updated.elapsed() > Duration::from_millis(200)
                }) {
                    state.update_search(false);
                }
                if id == state.active {
                    state.sync_tree(false);
                }
                state.window.request_redraw();
            }
            UserEvent::ChildExited(id) => {
                log::info!("pane {id} exited: {}", state.diag());
                state.close_pane(id);
                if state.panes.is_empty() {
                    state.save_session();
                    event_loop.exit();
                }
            }
            UserEvent::Redraw => state.window.request_redraw(),
            UserEvent::FocusPane(id) => state.focus_saved_pane(id),
            UserEvent::ConfigChanged => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = &mut self.state else { return };
        if state.panes.is_empty() {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                state.request_quit(self.config.confirm_close);
                if state.quit {
                    state.save_session();
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width, size.height);
                state.relayout();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.scale = scale_factor;
                state.set_spacing(&self.config);
                state.set_font_size(state.font_pt);
            }
            WindowEvent::Focused(focused) => {
                state.focused = focused;
                if !focused {
                    state.save_session();
                }
                if state.term().modes.focus_events {
                    state.write_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
                }
                state.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(mods) => {
                state.modifiers = mods.state();
                state.refresh_pointer();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    if !state.tree_focused && state.search.is_none() {
                        let bytes = {
                            let term = state.term();
                            let flags = term.keyboard_flags();
                            (flags & input::kitty::EVENT_TYPES != 0)
                                .then(|| {
                                    input::encode_key(&event, state.modifiers, &term.modes, flags)
                                })
                                .flatten()
                        };
                        if let Some(bytes) = bytes {
                            state.write_input(&bytes);
                        }
                    }
                    return;
                }
                let reload = state.handle_key(&event, &self.config, &self.lua);
                if let Some(name) = state.pending_scheme.take()
                    && let Some((name, colors)) =
                        schemes::resolve(&self.config.custom_schemes, &name)
                {
                    // Saving reloads the config, which then also applies the
                    // theme's own color changes on top of the scheme.
                    match lua::save_color_scheme(&self.config_path, &name) {
                        Ok(path) => log::info!("color scheme {name:?} saved to {}", path.display()),
                        Err(e) => state.show_message(
                            &state
                                .lang
                                .format("config.scheme_save_failed", &[("error", &e)]),
                        ),
                    }
                    self.config.colors = colors;
                    self.config.color_scheme = Some(name);
                    for pane in state.panes.values() {
                        configure_terminal(&mut pane.term(), &self.config);
                    }
                    state.window.request_redraw();
                }
                if let Some(id) = state.pending_language.take() {
                    match lua::save_language(&self.config_path, &id) {
                        Ok(()) => {
                            log::info!("language {id:?} saved");
                            self.config.language = id.clone();
                            state.lang = i18n::load(&id, &state.lang_dirs);
                            if let Some(tree) = &mut state.tree {
                                tree.invalidate();
                            }
                            state.window.request_redraw();
                        }
                        Err(e) => state.show_message(
                            &state
                                .lang
                                .format("config.language_save_failed", &[("error", &e)]),
                        ),
                    }
                }
                if state.panes.is_empty() || state.quit {
                    state.save_session();
                    event_loop.exit();
                } else if reload {
                    self.reload_config();
                }
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Commit(text) => {
                    state.preedit.clear();
                    if let Some(name) = &mut state.renaming {
                        name.push_str(&text);
                    } else if let Some(palette) = &mut state.palette {
                        palette.input(&text);
                    } else {
                        state.scroll_to_bottom();
                        state.write_input(text.as_bytes());
                    }
                    state.window.request_redraw();
                }
                Ime::Preedit(text, _) => {
                    state.preedit = text;
                    state.window.request_redraw();
                }
                Ime::Disabled => state.preedit.clear(),
                Ime::Enabled => {}
            },
            WindowEvent::MouseInput {
                state: button_state,
                button,
                ..
            } => state.handle_mouse_button(button, button_state, &self.config),
            WindowEvent::CursorMoved { position, .. } => state.handle_cursor_moved(position),
            WindowEvent::MouseWheel { delta, .. } => state.handle_wheel(delta),
            WindowEvent::RedrawRequested => state.redraw(&self.config.colors),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_needs_room_for_two_cells_and_the_divider() {
        assert!(split_has_room(100.0, 13.0, 10.0));
        assert!(split_has_room(33.0, 13.0, 10.0));
        assert!(!split_has_room(32.0, 13.0, 10.0));
        assert!(!split_has_room(20.0, 13.0, 22.0));
        assert!(!split_has_room(100.0, 13.0, 0.0));
        assert!(!split_has_room(f32::NAN, 13.0, 10.0));
    }

    #[test]
    fn the_bar_cursor_blinks_on_then_off() {
        assert!(cursor_phase_on(Duration::ZERO));
        assert!(cursor_phase_on(Duration::from_millis(499)));
        assert!(!cursor_phase_on(Duration::from_millis(500)));
        assert!(!cursor_phase_on(Duration::from_millis(999)));
        assert!(cursor_phase_on(Duration::from_millis(1000)));
    }

    #[test]
    fn the_window_icon_decodes() {
        assert!(window_icon().is_some());
    }

    #[test]
    fn tabs_are_labelled_with_the_directory_name() {
        assert_eq!(dir_label(Path::new("/home/me/Projects/kodama")), "kodama");
        assert_eq!(dir_label(Path::new("/")), "/");
        if let Some(home) = std::env::home_dir() {
            assert_eq!(dir_label(&home), "~");
        }
    }

    #[test]
    fn active_frame_sits_between_the_pane_and_the_divider() {
        let pane = Rect {
            x: 100.0,
            y: 50.0,
            w: 200.0,
            h: 100.0,
        };
        let (fill, frame) = active_frame(pane, 2.0, 6.0);
        assert_eq!(
            fill,
            Rect {
                x: 98.0,
                y: 48.0,
                w: 204.0,
                h: 104.0
            }
        );
        // 2px inset from the pane, so it stays clear of a divider 6px away.
        assert_eq!(
            frame[0],
            Rect {
                x: 96.0,
                y: 46.0,
                w: 208.0,
                h: 2.0
            }
        );
        assert_eq!(
            frame[1],
            Rect {
                x: 96.0,
                y: 152.0,
                w: 208.0,
                h: 2.0
            }
        );
        assert_eq!(
            frame[3],
            Rect {
                x: 302.0,
                y: 46.0,
                w: 2.0,
                h: 108.0
            }
        );
        let (fill, frame) = active_frame(pane, 0.0, 6.0);
        assert!(frame.is_empty());
        assert_eq!(
            fill,
            Rect {
                x: 97.0,
                y: 47.0,
                w: 206.0,
                h: 106.0
            }
        );
    }

    #[test]
    fn divider_lines_reach_the_window_edges_and_each_other() {
        // 0 | 1 side by side, then 1 split into 1 over 2.
        let mut layout = Layout::new(0);
        layout.split(0, 1, Axis::Horizontal);
        layout.split(1, 2, Axis::Vertical);
        let window = (400.0, 300.0);
        let area = content_area(
            window.0,
            window.1,
            Insets {
                top: 10.0,
                right: 10.0,
                bottom: 10.0,
                left: 10.0,
            },
        );
        let (width, padding) = (1.0, 6.0);
        let found = layout.dividers(area, width + 2.0 * padding);
        let lines = divider_lines(
            &found,
            area,
            Rect {
                x: 0.0,
                y: 0.0,
                w: window.0,
                h: window.1,
            },
            width,
            padding,
        );
        let vertical = lines.iter().find(|r| r.w == width).unwrap();
        let horizontal = lines.iter().find(|r| r.h == width).unwrap();
        // The full-height line runs through the window padding to both edges.
        assert_eq!((vertical.y, vertical.h), (0.0, 300.0));
        // The nested line reaches the right edge and overlaps the vertical one.
        assert_eq!(horizontal.x + horizontal.w, 400.0);
        assert!(
            horizontal.x <= vertical.x && horizontal.x + horizontal.w >= vertical.x + vertical.w
        );
    }

    #[test]
    fn two_tabs_share_the_centre_line_with_a_half_split() {
        let pad = Insets {
            top: 6.0,
            right: 6.0,
            bottom: 6.0,
            left: 6.0,
        };
        let (width, padding) = (1.0, 6.0);
        // Odd and even widths: the column grid would miss the midpoint, the
        // equal slices must not.
        for window_w in [941.0, 1000.0, 1896.0] {
            let area = content_area(window_w, 800.0, pad);
            let mut layout = Layout::new(0);
            layout.split(0, 1, Axis::Horizontal);
            let lines = divider_lines(
                &layout.dividers(area, width + 2.0 * padding),
                area,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: window_w,
                    h: 800.0,
                },
                width,
                padding,
            );
            let pane = lines.iter().find(|r| r.w == width).unwrap();
            let (left, left_w) = equal_slice(0.0, window_w, 0, 2, width);
            let (right, right_w) = equal_slice(0.0, window_w, 1, 2, width);
            // The first box ends on the divider, so its colour cannot cross it.
            assert_eq!(pane.x, left + left_w, "window {window_w}");
            assert_eq!(right, left + left_w, "window {window_w}");
            assert!((left_w - right_w).abs() <= 1.0, "window {window_w}");
            assert_eq!(slice_index(right - 0.1, 0.0, window_w, 2, width), Some(0));
            assert_eq!(slice_index(right + 0.1, 0.0, window_w, 2, width), Some(1));
        }
    }

    #[test]
    fn tree_grid_meets_the_window_and_the_divider() {
        let window = (400.0, 300.0);
        let content = content_area(window.0, window.1, Insets::ZERO);
        let tree = Rect {
            w: 120.0,
            ..content
        };
        let pad = 6.0;
        let div_x = tree.x + tree.w + pad;
        let div_w = 1.0;
        let divider = Some((div_x, div_x + div_w));
        // No window margin: the grid is flush with the window and the divider.
        let (left, right, bottom) = grid_span(tree, content, divider, pad);
        assert_eq!((left, right, bottom), (0.0, div_x, window.1));

        let pane = Rect {
            x: div_x + div_w + pad,
            w: content.x + content.w - (div_x + div_w + pad),
            ..content
        };
        let (left, right, bottom) = grid_span(pane, content, divider, pad);
        assert_eq!((left, right, bottom), (div_x + div_w, window.0, window.1));

        // A margin keeps the terminal background on that side only.
        let margin = Insets {
            top: 0.0,
            right: 10.0,
            bottom: 8.0,
            left: 0.0,
        };
        let content = content_area(window.0, window.1, margin);
        let tree = Rect {
            w: 120.0,
            ..content
        };
        let div_x = tree.x + tree.w + pad;
        let divider = Some((div_x, div_x + div_w));
        let (left, right, bottom) = grid_span(tree, content, divider, pad);
        assert_eq!((left, right, bottom), (0.0, div_x, window.1 - 8.0));
        let pane = Rect {
            x: div_x + div_w + pad,
            w: content.x + content.w - (div_x + div_w + pad),
            ..content
        };
        let (left, right, bottom) = grid_span(pane, content, divider, pad);
        assert_eq!(
            (left, right, bottom),
            (div_x + div_w, window.0 - 10.0, window.1 - 8.0)
        );

        // Inner padding insets the flush span; a side of 0 stays put.
        let (l, r, b) = grid_span(pane, content, divider, pad);
        let boxed = inset_box(
            l,
            pane.y,
            r,
            b,
            Insets {
                left: 4.0,
                ..Insets::ZERO
            },
        );
        assert_eq!(
            (boxed.left, boxed.top, boxed.right, boxed.bottom),
            (l + 4.0, pane.y, r, b)
        );

        // A pane that does not touch the tree keeps its own edges.
        let inner = Rect {
            x: pane.x + 80.0,
            w: 40.0,
            ..pane
        };
        let (left, right, _) = grid_span(inner, content, divider, pad);
        assert_eq!((left, right), (inner.x, inner.x + inner.w));
    }

    #[test]
    fn background_highlight_covers_the_padding_when_asked() {
        let window = (400.0, 300.0);
        let margin = Insets {
            top: 10.0,
            right: 12.0,
            bottom: 8.0,
            left: 6.0,
        };
        let content = content_area(window.0, window.1, margin);
        let outer = (content.x, content.x + content.w, content.y + content.h);
        let fill = highlight_extent(content, content, window, outer, &[], 1.0, None);
        assert_eq!(
            fill,
            Rect {
                x: 0.0,
                y: 0.0,
                w: window.0,
                h: window.1
            }
        );
        // A tab bar stays above the fill when the pane spans it.
        let tab = Rect {
            x: 0.0,
            y: 0.0,
            w: window.0,
            h: 28.0,
        };
        let fill = highlight_extent(content, content, window, outer, &[], 1.0, Some(tab));
        assert_eq!((fill.y, fill.h), (28.0, window.1 - 28.0));
        // A pane beside another keeps its own top: its frame does not run
        // up to the tab past the divider.
        let side = Rect {
            x: content.x,
            y: content.y + 40.0,
            w: 100.0,
            h: content.h - 40.0,
        };
        let fill = highlight_extent(
            side,
            content,
            window,
            (content.x, content.x + 100.0, content.y + content.h),
            &[],
            1.0,
            Some(tab),
        );
        assert_eq!(fill.y, side.y);

        let line_w = 1.0;
        let gap = Rect {
            x: 200.0,
            y: content.y,
            w: 13.0,
            h: content.h,
        };
        let line_x = (gap.x + (gap.w - line_w) / 2.0).round();
        let left = Rect {
            x: content.x,
            y: content.y,
            w: gap.x - content.x,
            h: content.h,
        };
        let fill = highlight_extent(
            left,
            content,
            window,
            (content.x, left.x + left.w, content.y + content.h),
            &[(Axis::Horizontal, gap)],
            line_w,
            None,
        );
        assert_eq!(
            (fill.x, fill.y, fill.x + fill.w, fill.y + fill.h),
            (0.0, 0.0, line_x, window.1)
        );
        let right = Rect {
            x: gap.x + gap.w,
            y: content.y,
            w: content.x + content.w - (gap.x + gap.w),
            h: content.h,
        };
        let fill = highlight_extent(
            right,
            content,
            window,
            (right.x, content.x + content.w, content.y + content.h),
            &[(Axis::Horizontal, gap)],
            line_w,
            None,
        );
        assert_eq!((fill.x, fill.x + fill.w), (line_x + line_w, window.0));

        // The tree is the same: its fill runs to the window and stops at
        // the divider, and a border is only the stroke inside that area.
        let tree = Rect {
            x: content.x,
            y: content.y,
            w: 120.0,
            h: content.h,
        };
        let div_x = tree.x + tree.w + 6.0;
        let fill = highlight_extent(
            tree,
            content,
            window,
            (content.x, div_x, content.y + content.h),
            &[],
            1.0,
            None,
        );
        assert_eq!(
            (fill.x, fill.y, fill.x + fill.w, fill.y + fill.h),
            (0.0, 0.0, div_x, window.1)
        );
        let (inner, edges) = border_frame(fill, 4.0);
        assert_eq!(inner.x, fill.x + 4.0);
        assert_eq!(inner.w, fill.w - 8.0);
        assert_eq!(edges[0].h, 4.0);
        assert_eq!(edges[2].w, 4.0);
        assert_eq!(edges[3].x, fill.x + fill.w - 4.0);
        let (inner, edges) = border_frame(fill, 0.0);
        assert_eq!(inner, fill);
        assert!(edges.is_empty());
    }

    #[test]
    fn padded_highlight_sits_outside_the_grid() {
        let grid = Rect {
            x: 20.0,
            y: 10.0,
            w: 100.0,
            h: 40.0,
        };
        let (fill, frame) = frame_around(grid, 2.0);
        assert_eq!(fill, grid);
        assert_eq!(
            frame[0],
            Rect {
                x: 18.0,
                y: 8.0,
                w: 104.0,
                h: 2.0
            }
        );
        assert_eq!(frame[2].x, 18.0);
        assert_eq!(frame[3].x, grid.x + grid.w);
        let (fill, frame) = frame_around(grid, 0.0);
        assert_eq!(fill, grid);
        assert!(frame.is_empty());
    }
}
