//! Crash log. A force-quit from the compositor is SIGKILL, which cannot be
//! caught, so every line is written through to the kernel. The last split
//! still on disk is what was happening when the process died.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, Log, Metadata, Record};

/// Dup of the log file for the panic hook and fatal-signal handler.
static LOG_FD: AtomicI32 = AtomicI32::new(-1);

const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Opens the log file, records fatal signals, and keeps kodama's info lines.
pub fn init() {
    let path = path();
    let file = match open_log(&path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("kodama: log {}: {e}", path.display());
            fallback_logger();
            return;
        }
    };
    // SAFETY: `file` is open. The duplicate is closed only by process exit.
    // CLOEXEC keeps spawned shells from inheriting the log.
    let fd = unsafe { libc::dup(std::os::unix::io::AsRawFd::as_raw_fd(&file)) };
    if fd >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        LOG_FD.store(fd, Ordering::Relaxed);
    }
    install_fatal_signals();
    install_panic_hook();

    let stderr = env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("warn,wgpu_core=error,wgpu_hal=error,naga=error,calloop=error"),
    )
    .build();
    let logger = DiagLogger {
        stderr,
        file: Mutex::new(file),
    };
    // Trace lets `RUST_LOG=debug` reach both targets. Each target filters.
    log::set_max_level(log::LevelFilter::Trace);
    if let Err(e) = log::set_boxed_logger(Box::new(logger)) {
        eprintln!("kodama: logger: {e}");
    }
}

/// `VmRSS` in KiB and the number of threads, from `/proc/self/status`.
pub fn process_stats() -> (u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0);
    };
    let mut rss = 0;
    let mut threads = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Threads:") {
            threads = rest.trim().parse().unwrap_or(0);
        }
    }
    (rss, threads)
}

pub fn path() -> PathBuf {
    log_path_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::home_dir().as_deref(),
    )
}

fn log_path_from(xdg_state_home: Option<&std::ffi::OsStr>, home: Option<&Path>) -> PathBuf {
    let dir = xdg_state_home
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    dir.join("kodama/kodama.log")
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    rotate_if_big(path);
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn rotate_if_big(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < MAX_LOG_BYTES {
        return;
    }
    let _ = std::fs::rename(path, path.with_extension("log.1"));
}

fn fallback_logger() {
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("warn,wgpu_core=error,wgpu_hal=error,naga=error,calloop=error"),
    )
    .init();
}

struct DiagLogger {
    stderr: env_logger::Logger,
    file: Mutex<std::fs::File>,
}

impl DiagLogger {
    fn write_file(&self, record: &Record<'_>) {
        let line = format!(
            "{} {:<5} {}: {}\n",
            timestamp(),
            record.level(),
            record.target(),
            record.args()
        );
        let mut file = match self.file.lock() {
            Ok(file) => file,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
}

impl Log for DiagLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        file_wants(metadata) || self.stderr.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if file_wants(record.metadata()) || self.stderr.enabled(record.metadata()) {
            self.write_file(record);
        }
        self.stderr.log(record);
    }

    fn flush(&self) {
        let mut file = match self.file.lock() {
            Ok(file) => file,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = file.flush();
        self.stderr.flush();
    }
}

/// Warnings from every crate, and kodama's own info (pane splits).
fn file_wants(metadata: &Metadata<'_>) -> bool {
    metadata.level() <= Level::Warn
        || (metadata.level() <= Level::Info && metadata.target().starts_with("kodama"))
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info.location().map_or_else(
            || "unknown location".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let message = panic_message(info.payload());
        let trace = std::backtrace::Backtrace::force_capture();
        let text = format!("panic at {loc}: {message}\n{trace}\n");
        write_raw(text.as_bytes());
        previous(info);
    }));
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "Box<dyn Any>".to_string()
    }
}

/// `signal` is process-wide. Handlers only `write` a fixed message, then
/// restore the default action and re-raise so a core dump can still be taken.
fn install_fatal_signals() {
    for sig in [
        libc::SIGSEGV,
        libc::SIGABRT,
        libc::SIGBUS,
        libc::SIGFPE,
        libc::SIGTERM,
        libc::SIGQUIT,
    ] {
        // SAFETY: the handler is async-signal-safe (one `write`, then reset).
        unsafe { libc::signal(sig, on_fatal as *const () as libc::sighandler_t) };
    }
}

extern "C" fn on_fatal(sig: libc::c_int) {
    let msg: &[u8] = match sig {
        libc::SIGSEGV => b"\nfatal signal SIGSEGV\n",
        libc::SIGABRT => b"\nfatal signal SIGABRT\n",
        libc::SIGBUS => b"\nfatal signal SIGBUS\n",
        libc::SIGFPE => b"\nfatal signal SIGFPE\n",
        libc::SIGTERM => b"\nfatal signal SIGTERM\n",
        libc::SIGQUIT => b"\nfatal signal SIGQUIT\n",
        _ => b"\nfatal signal\n",
    };
    write_raw(msg);
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn write_raw(mut bytes: &[u8]) {
    let fd = LOG_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        let Ok(written) = usize::try_from(n) else {
            break;
        };
        if written == 0 || written > bytes.len() {
            break;
        }
        bytes = &bytes[written..];
    }
}

fn timestamp() -> String {
    let Ok(since) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "0000-00-00T00:00:00.000Z".to_string();
    };
    format_utc(since.as_secs(), since.subsec_millis())
}

fn format_utc(secs: u64, millis: u32) -> String {
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (hour, min, sec) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`. `days` is days since the Unix epoch.
fn civil_from_days(days: u64) -> (i32, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        i32::try_from(year).unwrap_or(0),
        u32::try_from(month).unwrap_or(0),
        u32::try_from(day).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_prefers_xdg_state_home() {
        let path = log_path_from(
            Some(std::ffi::OsStr::new("/xdg")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(path, PathBuf::from("/xdg/kodama/kodama.log"));
    }

    #[test]
    fn log_path_falls_back_to_the_home_state_dir() {
        let path = log_path_from(None, Some(Path::new("/home/u")));
        assert_eq!(
            path,
            PathBuf::from("/home/u/.local/state/kodama/kodama.log")
        );
    }

    #[test]
    fn unix_epoch_and_a_later_day_format() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-11-21 00:00:00 UTC.
        assert_eq!(civil_from_days(20_778), (2026, 11, 21));
        assert_eq!(
            format_utc(20_778 * 86_400 + 7 * 3600, 5),
            "2026-11-21T07:00:00.005Z"
        );
    }
}
