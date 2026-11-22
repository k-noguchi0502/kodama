//! Pseudo-terminal management: spawns the shell on a new PTY.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};

/// Environment set only for kodama's own GPU setup, not for its children.
static PRIVATE_ENV: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

/// Set `key=value` for this process unless the user already set it, and keep
/// it out of the environment of spawned shells.
///
/// # Safety
/// Must be called before any other thread is started.
pub unsafe fn set_private_env(key: &'static str, value: &str) {
    if std::env::var_os(key).is_none() {
        // SAFETY: single-threaded per the caller's contract.
        unsafe { std::env::set_var(key, value) };
        PRIVATE_ENV.lock().unwrap().push(key);
    }
}

pub struct Pty {
    master: File,
    child: Child,
}

fn winsize(cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: cols.saturating_mul(cell_w),
        ws_ypixel: rows.saturating_mul(cell_h),
    }
}

impl Pty {
    pub fn spawn(
        program: &str,
        args: &[String],
        env: &HashMap<String, String>,
        working_directory: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let ws = winsize(cols, rows, 0, 0);
        // SAFETY: valid out-pointers; name/termios are optional (null).
        let ret = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty succeeded, so both descriptors are open and owned by us.
        let (master, slave) = unsafe {
            (
                OwnedFd::from_raw_fd(master_fd),
                OwnedFd::from_raw_fd(slave_fd),
            )
        };
        set_cloexec(master.as_raw_fd())?;
        set_cloexec(slave.as_raw_fd())?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "kodama")
            .env_remove("LINES")
            .env_remove("COLUMNS");
        for key in PRIVATE_ENV.lock().unwrap().iter() {
            cmd.env_remove(key);
        }
        cmd.envs(env);
        if let Some(dir) = working_directory.and_then(resolve_working_directory) {
            cmd.current_dir(dir);
        }

        // SAFETY: only async-signal-safe libc calls are made between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                // stdin is the PTY slave: make it our controlling terminal.
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for sig in [
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                ] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        Ok(Self {
            master: File::from(master),
            child,
        })
    }

    pub fn reader(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn writer(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    /// Working directory of the foreground process (falling back to the
    /// shell), so new panes can start where the user currently is.
    pub fn cwd(&self) -> Option<PathBuf> {
        // SAFETY: tcgetpgrp on the master fd we own.
        let pgrp = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        [pgrp, self.child.id() as libc::pid_t]
            .into_iter()
            .filter(|&pid| pid > 0)
            .find_map(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
        let ws = winsize(cols, rows, cell_w, cell_h);
        // SAFETY: the master fd is valid for the lifetime of self.
        let ret = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if ret != 0 {
            log::warn!("TIOCSWINSZ failed: {}", io::Error::last_os_error());
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: plain kill(2) on our own child's pid.
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
        // The shell takes a moment to exit after SIGHUP. Reap it off the
        // event loop so it doesn't stay behind as a zombie.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = std::thread::Builder::new()
                .name("pty-reap".into())
                .stack_size(64 * 1024)
                .spawn(move || {
                    // SAFETY: waitpid on our own child; the status is not read.
                    unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
                });
        }
    }
}

/// Expands `~/` and drops a directory that is not there.
///
/// `Command` reports a missing current directory as "No such file or
/// directory" on the program itself. A restored pane whose directory was
/// removed would otherwise exit kodama at startup.
fn resolve_working_directory(dir: &str) -> Option<PathBuf> {
    let expanded = match dir.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| dir.to_string()),
        None => dir.to_string(),
    };
    let path = PathBuf::from(&expanded);
    if path.is_dir() {
        Some(path)
    } else {
        log::warn!(
            "working directory {expanded} does not exist; starting in the current directory"
        );
        None
    }
}

fn set_cloexec(fd: i32) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_keeps_an_existing_directory() {
        let dir = std::env::temp_dir();
        let got = resolve_working_directory(dir.to_str().unwrap()).unwrap();
        assert_eq!(got.canonicalize().unwrap(), dir.canonicalize().unwrap());
    }

    #[test]
    fn resolve_expands_a_home_relative_directory() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let got = resolve_working_directory("~/").unwrap();
        assert_eq!(got.canonicalize().unwrap(), home.canonicalize().unwrap());
    }

    #[test]
    fn resolve_drops_a_missing_directory() {
        assert!(resolve_working_directory("/no/such/kodama/directory").is_none());
    }

    #[test]
    fn missing_directory_still_spawns_the_program() {
        Pty::spawn(
            "/bin/true",
            &[],
            &HashMap::new(),
            Some("/no/such/kodama/directory"),
            80,
            24,
        )
        .expect("a missing directory must not fail the spawn");
    }
}
