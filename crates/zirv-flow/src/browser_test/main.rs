//! The test server of the Playwright browser tests (#315).
//!
//! It runs the end-to-end harness (`e2e/harness.rs`) on the real clock behind a loopback listener:
//! the real worker pool, the render host, the fixture DNS resolver and the in-process SMTP sink,
//! in a scratch database it creates and drops. It serves the embedded web bundle and the API, plus
//! `/__test/sink`, which the release binary does not have. Built only with the `browser-test`
//! feature, which refuses to compile without `ZIRV_FLOW_BROWSER_TEST_BUILD` (`browser_guard.rs`).

#[path = "../browser_guard.rs"]
mod browser_guard;
#[path = "../e2e/harness.rs"]
mod harness;
#[path = "../e2e/mail.rs"]
mod mail;
mod server;

use std::process::ExitCode;

/// The render pool starts copies of this executable running `render-host`, as the server binary
/// does; the harness asks for the command through this function.
fn render_host_command(
    program: std::path::PathBuf,
    memory_limit_bytes: u64,
) -> zirv_flow_render::ChildCommand {
    zirv_flow_render::ChildCommand::render_host(program, memory_limit_bytes)
}

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "render-host")
    {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        return ExitCode::from(zirv_flow_render::host::run(None));
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: could not start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(server::run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}
