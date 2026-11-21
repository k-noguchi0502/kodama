//! Watches the config directory and calls back as soon as a `.lua` file in
//! it is written, created, renamed or removed. Uses inotify on Linux and
//! falls back to polling modification times elsewhere (or if inotify fails).

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Editors often save with several syscalls (truncate + write, or write a
/// temp file and rename it); events closer together than this are merged.
const SETTLE: Duration = Duration::from_millis(15);

pub fn spawn(config_path: PathBuf, on_change: impl FnMut() -> bool + Send + 'static) {
    thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            let on_change = match inotify::run(&config_path, on_change) {
                Ok(()) => return,
                Err((e, f)) => {
                    log::warn!("inotify unavailable ({e}); polling the config instead");
                    f
                }
            };
            poll(&config_path, on_change);
        })
        .expect("failed to spawn config watcher");
}

fn is_lua(name: &Path) -> bool {
    name.extension().is_some_and(|e| e == "lua")
}

fn poll(path: &Path, mut on_change: impl FnMut() -> bool) {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let stamps = || {
        let mut stamps = vec![(path.to_path_buf(), mtime(path))];
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for p in entries.flatten().map(|e| e.path()) {
                if p != path && is_lua(&p) {
                    stamps.push((p.clone(), mtime(&p)));
                }
            }
        }
        stamps.sort();
        stamps
    };
    let mut last = stamps();
    loop {
        thread::sleep(Duration::from_millis(250));
        let now = stamps();
        if now != last {
            last = now;
            if !on_change() {
                return;
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod inotify {
    use super::{SETTLE, is_lua};
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    const DIR_EVENTS: u32 = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;

    struct Watcher {
        fd: OwnedFd,
        /// Watched directory for each watch descriptor.
        dirs: HashMap<i32, PathBuf>,
    }

    impl Watcher {
        fn add(&mut self, dir: &Path, mask: u32) -> bool {
            let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
                return false;
            };
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), mask) };
            if wd < 0 {
                return false;
            }
            self.dirs.insert(wd, dir.to_path_buf());
            true
        }

        /// Watch the config directory (or, until it exists, the nearest
        /// existing ancestor) plus the real directories of symlinked `.lua`
        /// files, so dotfile-managed configs reload too.
        fn refresh(&mut self, dir: &Path) {
            let mut wanted: Vec<(PathBuf, u32)> = Vec::new();
            if dir.is_dir() {
                wanted.push((dir.to_path_buf(), DIR_EVENTS));
                for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let p = entry.path();
                    if is_lua(&p)
                        && p.is_symlink()
                        && let Some(target) = std::fs::canonicalize(&p)
                            .ok()
                            .and_then(|t| t.parent().map(Path::to_path_buf))
                    {
                        wanted.push((target, DIR_EVENTS));
                    }
                }
            } else if let Some(a) = dir.ancestors().skip(1).find(|a| a.is_dir()) {
                wanted.push((
                    a.to_path_buf(),
                    libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE_SELF,
                ));
            }

            let stale: Vec<i32> = self
                .dirs
                .iter()
                .filter(|(_, d)| !wanted.iter().any(|(w, _)| w == *d))
                .map(|(&wd, _)| wd)
                .collect();
            for wd in stale {
                self.dirs.remove(&wd);
                unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
            }
            for (path, mask) in wanted {
                // Re-adding an existing watch just updates its mask in place.
                self.add(&path, mask);
            }
        }

        /// Block until events arrive and report whether any concerns the
        /// config: a `.lua` file, or the appearance or loss of a watched
        /// directory (which needs `refresh`).
        fn wait(&self, buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<(bool, bool)>> {
            let mut pfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::Interrupted {
                    Ok(Some((false, false)))
                } else {
                    Err(e)
                };
            }
            if n == 0 {
                return Ok(None);
            }
            let len =
                unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if len < 0 {
                return Err(io::Error::last_os_error());
            }
            let (mut lua, mut structure) = (false, false);
            let header = std::mem::size_of::<libc::inotify_event>();
            let mut off = 0;
            while off + header <= len as usize {
                let ev: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf[off..].as_ptr().cast()) };
                let name = &buf[off + header..off + header + ev.len as usize];
                let name = OsStr::from_bytes(
                    &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())],
                );
                off += header + ev.len as usize;
                if ev.mask & libc::IN_IGNORED != 0
                    || (ev.wd >= 0 && !self.dirs.contains_key(&ev.wd))
                {
                    // Our own inotify_rm_watch, or a watch already dropped.
                    continue;
                }
                if ev.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_Q_OVERFLOW) != 0
                {
                    structure = true;
                    lua = true;
                } else if ev.mask & libc::IN_ISDIR != 0 {
                    structure = true;
                } else if is_lua(Path::new(name)) {
                    lua = true;
                    // A new symlink may point somewhere we don't watch yet.
                    structure |= ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0;
                }
            }
            Ok(Some((lua, structure)))
        }
    }

    pub fn run<F: FnMut() -> bool>(
        config_path: &Path,
        mut on_change: F,
    ) -> Result<(), (io::Error, F)> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err((io::Error::last_os_error(), on_change));
        }
        let mut w = Watcher {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            dirs: HashMap::new(),
        };
        let dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        w.refresh(&dir);

        let mut buf = vec![0u8; 16 * 1024];
        let settle = SETTLE.as_millis() as i32;
        loop {
            let (mut lua, mut structure) = match w.wait(&mut buf, -1) {
                Ok(Some(flags)) => flags,
                Ok(None) => continue,
                Err(e) => return Err((e, on_change)),
            };
            if !lua && !structure {
                continue;
            }
            while let Ok(Some((l, s))) = w.wait(&mut buf, settle) {
                lua |= l;
                structure |= s;
            }
            if structure {
                let had_dir = w.dirs.values().any(|d| d == &dir);
                w.refresh(&dir);
                // The directory appeared with files already in it.
                lua |= !had_dir && w.dirs.values().any(|d| d == &dir);
            }
            if lua && !on_change() {
                return Ok(());
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    fn wait(rx: &mpsc::Receiver<()>) -> bool {
        rx.recv_timeout(Duration::from_secs(2)).is_ok()
    }

    #[test]
    fn reports_writes_renames_and_late_directories() {
        let root = std::env::temp_dir().join(format!("kodama-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.join("kodama");
        let config = dir.join("config.lua");

        let (tx, rx) = mpsc::channel();
        super::spawn(config.clone(), move || tx.send(()).is_ok());
        std::thread::sleep(Duration::from_millis(100));

        // The directory doesn't exist yet; creating it with a file reloads.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&config, "return {}").unwrap();
        assert!(wait(&rx), "directory creation");
        while rx.recv_timeout(Duration::from_millis(100)).is_ok() {}

        std::fs::write(dir.join("keybind.lua"), "return {}").unwrap();
        assert!(wait(&rx), "keybind.lua write");
        while rx.recv_timeout(Duration::from_millis(100)).is_ok() {}

        // Save-by-rename, as vim and many editors do.
        std::fs::write(dir.join(".config.lua.swp"), "return { scrollback = 1 }").unwrap();
        std::fs::rename(dir.join(".config.lua.swp"), &config).unwrap();
        assert!(wait(&rx), "rename");
        while rx.recv_timeout(Duration::from_millis(100)).is_ok() {}

        // Unrelated files are ignored.
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "non-lua file"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
