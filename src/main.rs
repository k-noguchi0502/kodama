mod app;
mod config;
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

    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("warn,wgpu_core=error,wgpu_hal=error,naga=error,calloop=error"),
    )
    .init();

    let cli = Cli::parse();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("failed to create event loop");
    let mut app = App::new(cli, event_loop.create_proxy());
    event_loop.run_app(&mut app).expect("event loop error");
}
