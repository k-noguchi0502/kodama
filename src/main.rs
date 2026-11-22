mod app;
mod config;
mod diag;
mod i18n;
mod layout;
mod pty;
mod renderer;
mod session;
mod terminal;
mod ui;

use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};
use crate::config::Cli;

fn main() {
    // Writing to a closed pipe raises SIGPIPE and would kill the process.
    // SAFETY: no other threads exist yet, so the disposition is process-wide.
    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    // Mesa prints a "not a conformant Vulkan implementation" banner on every
    // start for GPUs newer than its conformance submission (e.g. RDNA4).
    // SAFETY: no other threads exist yet.
    unsafe { pty::set_private_env("MESA_VK_IGNORE_CONFORMANCE_WARNING", "1") };

    // Every pane's reader thread would otherwise get its own malloc arena,
    // each keeping freed pages. Two arenas hold the same work in less memory.
    #[cfg(target_env = "gnu")]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: mallopt before any other thread starts.
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
    }

    diag::init();
    log::info!(
        "start version {} pid {} log {}",
        config::version(),
        std::process::id(),
        diag::path().display(),
    );

    let cli = Cli::parse();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("failed to create event loop");
    let mut app = App::new(cli, event_loop.create_proxy());
    event_loop.run_app(&mut app).expect("event loop error");
}
