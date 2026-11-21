//! File tree sidebar (neo-tree style) for the active pane's directory.
//!
//! The listing is drawn into a small [`Terminal`] so the renderer shows it
//! exactly like a pane: same font, colours and icons.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;

use crate::config::{Colors, Rgb};
use crate::i18n::Language;
use crate::terminal::Terminal;

/// Listing size cap, so huge directories stay responsive.
const MAX_ENTRIES: usize = 10_000;
/// Directory entries a filter search may visit.
const FILTER_BUDGET: usize = 50_000;
/// Folders a filter search doesn't descend into.
const FILTER_SKIP: [&str; 4] = [".git", "node_modules", "target", ".cache"];
/// Minimum time between `git status` runs.
const GIT_INTERVAL: Duration = Duration::from_secs(3);

/// A file's state in git, most important last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitKind {
    Renamed,
    Added,
    Untracked,
    Modified,
    Deleted,
    Conflict,
}

impl GitKind {
    fn symbol(self) -> char {
        match self {
            GitKind::Renamed => 'R',
            GitKind::Added => 'A',
            GitKind::Untracked => '?',
            GitKind::Modified => 'M',
            GitKind::Deleted => 'D',
            GitKind::Conflict => 'U',
        }
    }
}

/// `git status` of the repository containing the tree's root. Folders get
/// the most important state among the files below them.
#[derive(Debug, Default)]
pub struct GitStatus {
    kinds: HashMap<PathBuf, GitKind>,
    /// Untracked folders: everything inside is untracked too.
    untracked_dirs: Vec<PathBuf>,
}

impl GitStatus {
    pub fn get(&self, path: &Path) -> Option<GitKind> {
        self.kinds.get(path).copied().or_else(|| {
            self.untracked_dirs
                .iter()
                .any(|d| path.starts_with(d))
                .then_some(GitKind::Untracked)
        })
    }
}

/// Runs `git status` for the repository containing `dir`.
pub fn git_status(dir: &Path) -> Option<GitStatus> {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
    };
    let top = git(&["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim_end_matches('\n'));
    let out = git(&["status", "--porcelain=v1", "-z"])?;
    Some(parse_git_status(&top, &out))
}

fn parse_git_status(top: &Path, out: &[u8]) -> GitStatus {
    let mut status = GitStatus::default();
    let mut records = out.split(|&b| b == 0).filter(|r| r.len() > 3);
    while let Some(record) = records.next() {
        let (x, y) = (record[0], record[1]);
        let name = String::from_utf8_lossy(&record[3..]);
        if x == b'R' || x == b'C' {
            records.next(); // the old name
        }
        let kind = match (x, y) {
            (b'?', b'?') => GitKind::Untracked,
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => GitKind::Conflict,
            (b'D', _) | (_, b'D') => GitKind::Deleted,
            (b'M' | b'T', _) | (_, b'M' | b'T') => GitKind::Modified,
            (b'R' | b'C', _) => GitKind::Renamed,
            (b'A', _) => GitKind::Added,
            _ => continue,
        };
        let path = top.join(name.trim_end_matches('/'));
        if kind == GitKind::Untracked && name.ends_with('/') {
            status.untracked_dirs.push(path.clone());
        }
        for at in path
            .ancestors()
            .take_while(|a| a.starts_with(top) && *a != top)
        {
            let slot = status.kinds.entry(at.to_path_buf()).or_insert(kind);
            *slot = (*slot).max(kind);
        }
    }
    status
}

/// File operations asked for in the tree's prompt line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    Create,
    Rename,
    Move,
    Copy,
    Trash,
    Delete,
}

#[derive(Debug)]
struct Prompt {
    kind: PromptKind,
    text: String,
    /// The entry acted on, or the folder a new entry goes into.
    target: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub symlink: bool,
    /// A folder shown with its contents.
    pub open: bool,
}

pub struct FileTree {
    root: PathBuf,
    expanded: HashSet<PathBuf>,
    pub show_hidden: bool,
    entries: Vec<Entry>,
    selected: usize,
    /// First entry shown below the header line.
    scroll: usize,
    view: Terminal,
    /// The view must be redrawn.
    dirty: bool,
    /// Scroll the selection into view on the next draw.
    follow: bool,
    focused: bool,
    /// Only entries whose name contains this (and their folders) are shown.
    filter: Option<String>,
    /// Keys go to the filter text.
    filtering: bool,
    prompt: Option<Prompt>,
    /// Result of the last file operation, shown in the header line.
    message: Option<(String, bool)>,
    git: Option<GitStatus>,
    git_incoming: Arc<Mutex<Option<Option<GitStatus>>>>,
    git_running: Arc<AtomicBool>,
    git_checked: Option<Instant>,
    /// Called from the git thread when new status arrives.
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl FileTree {
    pub fn new(root: PathBuf, show_hidden: bool) -> Self {
        let mut tree = Self {
            root,
            expanded: HashSet::new(),
            show_hidden,
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            view: Terminal::new(1, 1, 0),
            dirty: true,
            follow: true,
            focused: false,
            filter: None,
            filtering: false,
            prompt: None,
            message: None,
            git: None,
            git_incoming: Arc::default(),
            git_running: Arc::default(),
            git_checked: None,
            notify: None,
        };
        tree.refresh();
        tree.refresh_git(true);
        tree
    }

    /// `notify` is called (from another thread) when git status arrives.
    pub fn set_notify(&mut self, notify: Arc<dyn Fn() + Send + Sync>) {
        self.notify = Some(notify);
    }

    /// Starts `git status` in the background, at most every few seconds
    /// unless `force`d.
    pub fn refresh_git(&mut self, force: bool) {
        if !force && self.git_checked.is_some_and(|t| t.elapsed() < GIT_INTERVAL) {
            return;
        }
        if self.git_running.swap(true, Ordering::AcqRel) {
            return;
        }
        self.git_checked = Some(Instant::now());
        let (root, incoming, running) = (
            self.root.clone(),
            self.git_incoming.clone(),
            self.git_running.clone(),
        );
        let notify = self.notify.clone();
        std::thread::spawn(move || {
            let status = git_status(&root);
            *incoming.lock().unwrap() = Some(status);
            running.store(false, Ordering::Release);
            if let Some(notify) = notify {
                notify();
            }
        });
    }

    fn take_git(&mut self) {
        if let Some(status) = self.git_incoming.lock().unwrap().take() {
            self.git = status;
            self.dirty = true;
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Follows a new directory; expanded folders are remembered by path.
    pub fn set_root(&mut self, root: PathBuf) {
        if root != self.root {
            self.root = root;
            self.selected = 0;
            self.scroll = 0;
            self.filter = None;
            self.filtering = false;
            self.prompt = None;
            self.git = None;
            self.refresh();
            self.refresh_git(true);
        }
    }

    /// Re-reads the listing, keeping the selected path when it still exists.
    pub fn refresh(&mut self) {
        let keep = self.selected().map(|e| e.path.clone());
        let mut entries = Vec::new();
        match self.filter.as_deref().filter(|f| !f.is_empty()) {
            Some(filter) => {
                let mut budget = FILTER_BUDGET;
                self.walk_filtered(
                    &self.root.clone(),
                    0,
                    &filter.to_lowercase(),
                    &mut entries,
                    &mut budget,
                );
            }
            None => self.walk(&self.root.clone(), 0, &mut entries),
        }
        self.entries = entries;
        if let Some(path) = keep
            && let Some(i) = self.entries.iter().position(|e| e.path == path)
        {
            self.selected = i;
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.dirty = true;
    }

    fn walk(&self, dir: &Path, depth: usize, out: &mut Vec<Entry>) {
        for entry in list_dir(dir, self.show_hidden) {
            if out.len() >= MAX_ENTRIES {
                return;
            }
            let open = entry.is_dir && self.expanded.contains(&entry.path);
            let path = entry.path.clone();
            out.push(Entry {
                depth,
                open,
                ..entry
            });
            if open {
                self.walk(&path, depth + 1, out);
            }
        }
    }

    /// Every entry below `dir` whose name contains `needle`, with the
    /// folders leading to it. Returns whether anything was added.
    fn walk_filtered(
        &self,
        dir: &Path,
        depth: usize,
        needle: &str,
        out: &mut Vec<Entry>,
        budget: &mut usize,
    ) -> bool {
        let mut added = false;
        for entry in list_dir(dir, self.show_hidden) {
            if out.len() >= MAX_ENTRIES || *budget == 0 {
                break;
            }
            *budget -= 1;
            let matches = entry.name.to_lowercase().contains(needle);
            if entry.is_dir && !entry.symlink && !FILTER_SKIP.contains(&entry.name.as_str()) {
                let at = out.len();
                let path = entry.path.clone();
                out.push(Entry {
                    depth,
                    open: true,
                    ..entry
                });
                if self.walk_filtered(&path, depth + 1, needle, out, budget) || matches {
                    added = true;
                } else {
                    out.truncate(at);
                }
            } else if matches {
                out.push(Entry { depth, ..entry });
                added = true;
            }
        }
        added
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    pub fn set_focused(&mut self, focused: bool) {
        if self.focused != focused {
            self.focused = focused;
            self.dirty = true;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.dirty = true;
        self.follow = true;
    }

    pub fn select(&mut self, index: usize) {
        if index < self.entries.len() {
            self.selected = index;
            self.dirty = true;
            self.follow = true;
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.entries.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
        self.dirty = true;
    }

    /// Entry index shown on view row `row` (row 0 is the header).
    pub fn index_at_row(&self, row: usize) -> Option<usize> {
        let i = row.checked_sub(1)? + self.scroll;
        (i < self.entries.len()).then_some(i)
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    // ---- filter ---------------------------------------------------------------

    /// Starts typing a filter.
    pub fn start_filter(&mut self) {
        self.filter.get_or_insert_with(String::new);
        self.filtering = true;
        self.dirty = true;
    }

    pub fn filtering(&self) -> bool {
        self.filtering
    }

    pub fn has_filter(&self) -> bool {
        self.filter.is_some()
    }

    pub fn filter_input(&mut self, text: &str) {
        if let Some(filter) = &mut self.filter {
            filter.push_str(text);
            self.selected = 0;
            self.refresh();
            self.select_first_match();
        }
    }

    pub fn filter_backspace(&mut self) {
        match &mut self.filter {
            Some(filter) if !filter.is_empty() => {
                filter.pop();
                self.refresh();
                self.select_first_match();
            }
            _ => self.clear_filter(),
        }
    }

    /// Stops typing; the filtered listing stays for browsing.
    pub fn finish_filter(&mut self) {
        self.filtering = false;
        self.dirty = true;
    }

    pub fn clear_filter(&mut self) {
        let keep = self.selected().map(|e| e.path.clone());
        self.filter = None;
        self.filtering = false;
        // Show where the selected match is.
        if let Some(path) = &keep {
            for dir in path
                .ancestors()
                .skip(1)
                .take_while(|a| a.starts_with(&self.root) && *a != self.root)
            {
                self.expanded.insert(dir.to_path_buf());
            }
        }
        self.refresh();
    }

    fn select_first_match(&mut self) {
        if let Some(needle) = self.filter.as_deref().map(str::to_lowercase)
            && let Some(i) = self
                .entries
                .iter()
                .position(|e| e.name.to_lowercase().contains(&needle))
        {
            self.select(i);
        }
    }

    // ---- file operations ------------------------------------------------------

    pub fn prompt_kind(&self) -> Option<PromptKind> {
        self.prompt.as_ref().map(|p| p.kind)
    }

    /// Asks for the name or destination of a file operation on the
    /// selected entry (or, for `Create`, the folder it is in).
    pub fn start_prompt(&mut self, kind: PromptKind) {
        let selected = self.selected().cloned();
        let relative = |p: &Path| {
            p.strip_prefix(&self.root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        };
        let (target, text) = match (kind, &selected) {
            (PromptKind::Create, Some(e)) if e.is_dir && e.open => (e.path.clone(), String::new()),
            (PromptKind::Create, Some(e)) => (
                e.path.parent().unwrap_or(&self.root).to_path_buf(),
                String::new(),
            ),
            (PromptKind::Create, None) => (self.root.clone(), String::new()),
            (_, None) => return,
            (PromptKind::Rename, Some(e)) => (e.path.clone(), e.name.clone()),
            (PromptKind::Move | PromptKind::Copy, Some(e)) => (e.path.clone(), relative(&e.path)),
            (PromptKind::Trash | PromptKind::Delete, Some(e)) => (e.path.clone(), String::new()),
        };
        self.prompt = Some(Prompt { kind, text, target });
        self.message = None;
        self.dirty = true;
    }

    pub fn prompt_input(&mut self, text: &str, lang: &Language) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        if matches!(prompt.kind, PromptKind::Trash | PromptKind::Delete) {
            if text.eq_ignore_ascii_case("y") {
                self.commit_prompt(lang);
            } else {
                self.cancel_prompt();
            }
            return;
        }
        prompt.text.push_str(text);
        self.dirty = true;
    }

    pub fn prompt_backspace(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.text.pop();
            self.dirty = true;
        }
    }

    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
        self.dirty = true;
    }

    /// Performs the prompted operation and reports the outcome in the header.
    pub fn commit_prompt(&mut self, lang: &Language) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        self.dirty = true;
        let text = prompt.text.trim();
        if text.is_empty() && !matches!(prompt.kind, PromptKind::Trash | PromptKind::Delete) {
            return;
        }
        let name = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let destination = || {
            let dest = if Path::new(text).is_absolute() {
                PathBuf::from(text)
            } else {
                self.root.join(text)
            };
            if dest.is_dir() {
                dest.join(name(&prompt.target))
            } else {
                dest
            }
        };
        let result = match prompt.kind {
            PromptKind::Create => {
                let path = prompt.target.join(text);
                create(&path, text.ends_with('/')).map(|_| {
                    (
                        lang.format("tree.created", &[("name", &name(&path))]),
                        Some(path),
                    )
                })
            }
            PromptKind::Rename => {
                let path = prompt.target.with_file_name(text);
                move_to(&prompt.target, &path)
                    .map(|_| (lang.format("tree.renamed", &[("name", text)]), Some(path)))
            }
            PromptKind::Move => {
                let path = destination();
                move_to(&prompt.target, &path).map(|_| {
                    (
                        lang.format("tree.moved", &[("name", &self.display(&path))]),
                        Some(path),
                    )
                })
            }
            PromptKind::Copy => {
                let path = destination();
                copy(&prompt.target, &path).map(|_| {
                    (
                        lang.format("tree.copied", &[("name", &self.display(&path))]),
                        Some(path),
                    )
                })
            }
            PromptKind::Trash => trash(&prompt.target).map(|_| {
                (
                    lang.format("tree.trashed", &[("name", &name(&prompt.target))]),
                    None,
                )
            }),
            PromptKind::Delete => {
                let target = &prompt.target;
                let removed = if target.is_dir() && !target.is_symlink() {
                    std::fs::remove_dir_all(target)
                } else {
                    std::fs::remove_file(target)
                };
                removed
                    .map(|_| {
                        (
                            lang.format("tree.deleted", &[("name", &name(target))]),
                            None,
                        )
                    })
                    .map_err(|e| e.to_string())
            }
        };
        match result {
            Ok((message, select)) => {
                self.message = Some((message, false));
                if let Some(path) = select {
                    for dir in path
                        .ancestors()
                        .skip(1)
                        .take_while(|a| a.starts_with(&self.root) && *a != self.root)
                    {
                        self.expanded.insert(dir.to_path_buf());
                    }
                    self.refresh();
                    if let Some(i) = self.entries.iter().position(|e| e.path == path) {
                        self.select(i);
                    }
                } else {
                    self.refresh();
                }
                self.refresh_git(true);
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    /// Drops the message shown after a file operation.
    pub fn clear_message(&mut self) {
        if self.message.take().is_some() {
            self.dirty = true;
        }
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// Opens or closes the selected folder.
    pub fn toggle(&mut self) {
        let Some(entry) = self.selected().filter(|e| e.is_dir) else {
            return;
        };
        let path = entry.path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        self.refresh();
    }

    pub fn expand(&mut self) {
        if let Some(entry) = self
            .selected()
            .filter(|e| e.is_dir && !self.is_expanded(&e.path))
        {
            self.expanded.insert(entry.path.clone());
            self.refresh();
        }
    }

    /// Closes the selected folder, or moves to the folder containing it.
    pub fn collapse(&mut self) {
        let Some(entry) = self.selected().cloned() else {
            return;
        };
        if entry.is_dir && self.expanded.remove(&entry.path) {
            self.refresh();
        } else if entry.depth > 0
            && let Some(parent) = self.entries[..self.selected]
                .iter()
                .rposition(|e| e.depth < entry.depth)
        {
            self.select(parent);
        }
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh();
    }

    /// The listing drawn at `cols` x `rows`, redrawn only after changes.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn view(
        &mut self,
        cols: usize,
        rows: usize,
        colors: &Colors,
        lang: &Language,
    ) -> &Terminal {
        self.take_git();
        if self.dirty || self.view.cols() != cols || self.view.rows() != rows {
            self.dirty = false;
            self.view = self.draw(cols.max(1), rows.max(1), colors, lang);
        }
        &self.view
    }

    fn draw(&mut self, cols: usize, rows: usize, colors: &Colors, lang: &Language) -> Terminal {
        let body_rows = rows.saturating_sub(1).max(1);
        // Keep the selection on screen after it moves.
        if std::mem::take(&mut self.follow) {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + body_rows {
                self.scroll = self.selected + 1 - body_rows;
            }
        }
        self.scroll = self
            .scroll
            .min(self.entries.len().saturating_sub(body_rows));

        let fg = |c: Rgb| format!("\x1b[38;2;{};{};{}m", c[0], c[1], c[2]);
        let bg = |c: Rgb| format!("\x1b[48;2;{};{};{}m", c[0], c[1], c[2]);
        let (dir_color, dim) = (colors.ansi[4], colors.brights[0]);

        let mut out = String::from("\x1b[?25l\x1b[?7l");
        let caret = "\x1b[7m \x1b[27m";
        if let Some(prompt) = &self.prompt {
            let name = prompt
                .target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = match prompt.kind {
                PromptKind::Create => lang.get("tree.create").to_string(),
                PromptKind::Rename => lang.get("tree.rename").to_string(),
                PromptKind::Move => lang.get("tree.move").to_string(),
                PromptKind::Copy => lang.get("tree.copy").to_string(),
                PromptKind::Trash => lang.format("tree.trash", &[("name", &name)]),
                PromptKind::Delete => lang.format("tree.delete", &[("name", &name)]),
            };
            let label = fit(&label, cols);
            let text = fit_tail(&prompt.text, cols.saturating_sub(width(&label) + 1));
            let confirm = matches!(prompt.kind, PromptKind::Trash | PromptKind::Delete);
            let color = if prompt.kind == PromptKind::Delete {
                colors.ansi[1]
            } else {
                colors.ansi[3]
            };
            out.push_str(&format!(
                "{}\x1b[1m{label}\x1b[22m{}{text}{}\x1b[0m",
                fg(color),
                fg(colors.foreground),
                if confirm { "" } else { caret }
            ));
        } else if let Some(filter) = &self.filter {
            let text = fit_tail(filter, cols.saturating_sub(4));
            out.push_str(&format!(
                "{}\u{f002} {}{text}{}\x1b[0m",
                fg(colors.ansi[3]),
                fg(colors.foreground),
                if self.filtering { caret } else { "" }
            ));
        } else if let Some((message, error)) = &self.message {
            let color = if *error {
                colors.ansi[1]
            } else {
                colors.ansi[2]
            };
            out.push_str(&format!("{}{}\x1b[0m", fg(color), fit(message, cols)));
        } else {
            let header = fit_tail(&display_path(&self.root), cols.saturating_sub(2));
            out.push_str(&format!(
                "\x1b[1m{}\u{f07c} {header}\x1b[0m",
                fg(colors.ansi[3])
            ));
        }
        for (row, (i, entry)) in self
            .entries
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(body_rows)
            .enumerate()
        {
            out.push_str(&format!("\x1b[{};1H", row + 2));
            let selected = self.focused && i == self.selected;
            if selected {
                out.push_str(&bg(colors.selection));
            }
            let open = entry.open;
            let (arrow, icon, color) = if entry.is_dir {
                (
                    if open { '\u{f47c}' } else { '\u{f460}' },
                    if open { '\u{f07c}' } else { '\u{f07b}' },
                    dir_color,
                )
            } else {
                (' ', file_icon(&entry.name), colors.foreground)
            };
            let git = self.git.as_ref().and_then(|g| g.get(&entry.path));
            let color = match git {
                Some(GitKind::Modified | GitKind::Renamed) => colors.ansi[3],
                Some(GitKind::Added | GitKind::Untracked) => colors.ansi[2],
                Some(GitKind::Deleted) => colors.ansi[1],
                Some(GitKind::Conflict) => colors.ansi[5],
                None if entry.name.starts_with('.') => dim,
                None => color,
            };
            let mut line = format!("{}{arrow} {icon} {}", "  ".repeat(entry.depth), entry.name);
            if entry.symlink {
                line.push_str(" \u{f481}");
            }
            let room = if git.is_some() {
                cols.saturating_sub(2)
            } else {
                cols
            };
            let line = fit(&line, room);
            out.push_str(&fg(color));
            out.push_str(&line);
            let pad = room.saturating_sub(width(&line));
            if let Some(kind) = git {
                out.push_str(&" ".repeat(pad + 1));
                out.push(kind.symbol());
            } else if selected {
                out.push_str(&" ".repeat(pad));
            }
            out.push_str("\x1b[0m");
        }
        if self.entries.is_empty() {
            out.push_str(&format!(
                "\x1b[2;1H{}  {}\x1b[0m",
                fg(dim),
                lang.get("tree.empty")
            ));
        }
        let mut term = Terminal::new(cols, rows, 0);
        term.feed(out.as_bytes());
        term
    }
}

fn list_dir(dir: &Path, show_hidden: bool) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            let path = e.path();
            let symlink = e.file_type().is_ok_and(|t| t.is_symlink());
            let is_dir = if symlink {
                path.is_dir()
            } else {
                e.file_type().is_ok_and(|t| t.is_dir())
            };
            Some(Entry {
                path,
                name,
                depth: 0,
                is_dir,
                symlink,
                open: false,
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text.into_owned(),
    }
}

pub(crate) fn width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Truncates to `cols` columns, ending in `…` when cut.
pub(crate) fn fit(s: &str, cols: usize) -> String {
    if width(s) <= cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > cols {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn create(path: &Path, dir: bool) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let result = if dir {
        std::fs::create_dir_all(path)
    } else {
        path.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::File::create_new(path).map(drop))
    };
    result.map_err(|e| format!("create failed: {e}"))
}

fn move_to(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
    to.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::rename(from, to))
        .map_err(|e| format!("move failed: {e}"))
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
        if from.is_dir() && !from.is_symlink() {
            std::fs::create_dir(to)?;
            for entry in std::fs::read_dir(from)? {
                let entry = entry?;
                copy_all(&entry.path(), &to.join(entry.file_name()))?;
            }
            Ok(())
        } else {
            std::fs::copy(from, to).map(drop)
        }
    }
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
    if to.starts_with(from) {
        return Err("can't copy a folder into itself".into());
    }
    to.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| copy_all(from, to))
        .map_err(|e| format!("copy failed: {e}"))
}

/// Moves `path` to the desktop trash (`gio trash`).
fn trash(path: &Path) -> Result<(), String> {
    let status = Command::new("gio")
        .arg("trash")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("gio trash: {e} (D deletes permanently)"))?;
    if status.success() {
        Ok(())
    } else {
        Err("couldn't move to trash (D deletes permanently)".into())
    }
}

/// Keeps the end of `s` (the deepest folders of a path) within `cols`.
pub(crate) fn fit_tail(s: &str, cols: usize) -> String {
    if width(s) <= cols {
        return s.to_string();
    }
    let mut tail = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > cols {
            break;
        }
        tail.push(c);
        used += w;
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// Nerd Font icon for a file name.
fn file_icon(name: &str) -> char {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "cargo.toml" | "cargo.lock" => return '\u{e7a8}',
        "makefile" | "justfile" => return '\u{e779}',
        "dockerfile" => return '\u{f308}',
        "license" | "license.md" => return '\u{f0219}',
        ".gitignore" | ".gitmodules" | ".gitattributes" => return '\u{e702}',
        _ => {}
    }
    if lower.starts_with("readme") {
        return '\u{f00ba}';
    }
    match lower.rsplit_once('.').map_or("", |(_, ext)| ext) {
        "rs" => '\u{e7a8}',
        "lua" => '\u{e620}',
        "md" | "markdown" => '\u{f48a}',
        "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" => '\u{e615}',
        "json" | "jsonc" => '\u{e60b}',
        "py" => '\u{e73c}',
        "js" | "mjs" | "cjs" => '\u{e74e}',
        "ts" | "mts" => '\u{e628}',
        "jsx" | "tsx" => '\u{e7ba}',
        "go" => '\u{e627}',
        "c" | "h" => '\u{e61e}',
        "cpp" | "cc" | "hpp" => '\u{e61d}',
        "java" => '\u{e738}',
        "rb" => '\u{e739}',
        "html" | "htm" => '\u{e736}',
        "css" | "scss" => '\u{e749}',
        "sh" | "bash" | "zsh" | "fish" => '\u{f489}',
        "vim" => '\u{e62b}',
        "wgsl" | "glsl" | "frag" | "vert" => '\u{f1b2}',
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" => '\u{f1c5}',
        "zip" | "tar" | "gz" | "xz" | "zst" | "7z" => '\u{f410}',
        "pdf" => '\u{f1c1}',
        "lock" => '\u{f023}',
        "txt" | "log" => '\u{f15c}',
        _ => '\u{f15b}',
    }
}

/// Quotes a path for POSIX shells.
pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kodama-tree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::create_dir_all(dir.join("Docs")).unwrap();
        for f in [
            "b.txt",
            "A.md",
            ".hidden",
            "src/main.rs",
            "src/nested/deep.lua",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        dir
    }

    fn names(tree: &FileTree) -> Vec<String> {
        tree.entries()
            .iter()
            .map(|e| format!("{}{}", "  ".repeat(e.depth), e.name))
            .collect()
    }

    #[test]
    fn folders_first_then_files_and_expansion() {
        let dir = fixture("listing");
        let mut tree = FileTree::new(dir.clone(), false);
        assert_eq!(names(&tree), ["Docs", "src", "A.md", "b.txt"]);

        tree.select(1);
        tree.expand();
        assert_eq!(
            names(&tree),
            ["Docs", "src", "  nested", "  main.rs", "A.md", "b.txt"]
        );
        tree.move_selection(1);
        tree.expand();
        assert_eq!(tree.entries().len(), 7);

        // h on a child jumps to its folder, then closes it.
        tree.move_selection(1);
        assert_eq!(tree.selected().unwrap().name, "deep.lua");
        tree.collapse();
        assert_eq!(tree.selected().unwrap().name, "nested");
        tree.collapse();
        assert_eq!(
            names(&tree),
            ["Docs", "src", "  nested", "  main.rs", "A.md", "b.txt"]
        );

        tree.toggle_hidden();
        assert!(names(&tree).contains(&".hidden".to_string()));
        // The selection follows its path through refreshes.
        assert_eq!(tree.selected().unwrap().name, "nested");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_shows_header_and_selection() {
        let dir = fixture("view");
        let mut tree = FileTree::new(dir.clone(), false);
        tree.set_focused(true);
        let view = tree.view(60, 4, &Colors::default(), &Language::english());
        let row = |r: usize| view.line(r).iter().map(|c| c.c()).collect::<String>();
        assert!(row(0).contains("kodama-tree-view"), "{}", row(0));
        assert!(row(1).contains("Docs"));
        assert!(row(3).contains("A.md"));
        assert_eq!(tree.index_at_row(2), Some(1));
        assert_eq!(tree.index_at_row(0), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filter_shows_matches_with_their_folders() {
        let dir = fixture("filter");
        let mut tree = FileTree::new(dir.clone(), false);
        tree.start_filter();
        tree.filter_input("DEEP");
        assert_eq!(names(&tree), ["src", "  nested", "    deep.lua"]);
        assert_eq!(tree.selected().unwrap().name, "deep.lua");
        tree.finish_filter();
        tree.clear_filter();
        // The match stays selected and its folders open.
        assert_eq!(tree.selected().unwrap().name, "deep.lua");
        assert!(names(&tree).contains(&"    deep.lua".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_operations_from_the_prompt() {
        let dir = fixture("ops");
        let mut tree = FileTree::new(dir.clone(), false);
        let select = |tree: &mut FileTree, name: &str| {
            let i = tree.entries().iter().position(|e| e.name == name).unwrap();
            tree.select(i);
        };
        select(&mut tree, "Docs");
        tree.start_prompt(PromptKind::Create);
        let lang = Language::english();
        tree.prompt_input("notes/today.md", &lang);
        tree.commit_prompt(&lang);
        assert!(dir.join("notes/today.md").is_file());
        assert_eq!(tree.selected().unwrap().name, "today.md");

        tree.start_prompt(PromptKind::Rename);
        tree.prompt_backspace();
        tree.prompt_backspace();
        tree.prompt_input("txt", &lang);
        tree.commit_prompt(&lang);
        assert!(dir.join("notes/today.txt").is_file());

        tree.start_prompt(PromptKind::Copy);
        tree.prompt.as_mut().unwrap().text = "Docs".into();
        tree.commit_prompt(&lang);
        assert!(dir.join("Docs/today.txt").is_file());

        select(&mut tree, "b.txt");
        tree.start_prompt(PromptKind::Move);
        tree.prompt.as_mut().unwrap().text = "src/b.txt".into();
        tree.commit_prompt(&lang);
        assert!(dir.join("src/b.txt").is_file() && !dir.join("b.txt").exists());

        tree.start_prompt(PromptKind::Delete);
        tree.prompt_input("n", &lang);
        assert!(dir.join("src/b.txt").exists());
        tree.start_prompt(PromptKind::Delete);
        tree.prompt_input("y", &lang);
        assert!(!dir.join("src/b.txt").exists());

        select(&mut tree, "A.md");
        tree.start_prompt(PromptKind::Rename);
        tree.prompt.as_mut().unwrap().text = "Docs".into();
        tree.commit_prompt(&lang);
        assert!(tree.message.as_ref().is_some_and(|(_, error)| *error));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_status_marks_files_and_folders() {
        let top = Path::new("/repo");
        let out = b" M src/main.rs\0?? notes/\0A  new.txt\0R  b.rs\0a.rs\0UU src/x.rs\0";
        let status = parse_git_status(top, out);
        assert_eq!(
            status.get(&top.join("src/main.rs")),
            Some(GitKind::Modified)
        );
        assert_eq!(status.get(&top.join("src")), Some(GitKind::Conflict));
        assert_eq!(
            status.get(&top.join("notes/deep/file")),
            Some(GitKind::Untracked)
        );
        assert_eq!(status.get(&top.join("new.txt")), Some(GitKind::Added));
        assert_eq!(status.get(&top.join("b.rs")), Some(GitKind::Renamed));
        assert_eq!(status.get(&top.join("a.rs")), None);
    }

    #[test]
    fn long_names_are_cut_and_paths_quoted() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
        assert_eq!(fit_tail("/home/me/project", 8), "…project");
        assert_eq!(shell_quote(Path::new("/a b/it's")), r"'/a b/it'\''s'");
    }
}
