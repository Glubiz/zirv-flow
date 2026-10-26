//! The test server of the Playwright browser tests (#315).
//!
//! It runs the end-to-end harness (`e2e/harness.rs`) on the real clock behind a loopback listener:
//! the real worker pool, the render host, the fixture DNS resolver and the in-process SMTP sink,
//! in a scratch database it creates and drops. It serves the embedded web bundle and the API, plus
//! the `/__test/` routes, which the release binary does not have. Built only with the
//! `browser-test` feature, which refuses to compile without `ZIRV_FLOW_BROWSER_TEST_BUILD`.

// Makes the feature impossible to compile by accident: `--all-features`, an alias or a default
// list all fail here unless the builder deliberately sets the variable.
const _: () = assert!(
    option_env!("ZIRV_FLOW_BROWSER_TEST_BUILD").is_some(),
    "browser-test is for the browser test server only: set ZIRV_FLOW_BROWSER_TEST_BUILD=1 to build it",
);

// Shared files: the test helpers in them are used only by the main binary's e2e tests.
#[cfg_attr(test, allow(dead_code))]
#[path = "../e2e/harness.rs"]
mod harness;
#[cfg_attr(test, allow(dead_code))]
#[path = "../e2e/mail.rs"]
mod mail;
mod server;

use std::ffi::OsStr;
use std::process::ExitCode;

/// The render pool starts copies of this executable running `render-host`, as the server binary
/// does; the harness asks for the command through this function.
fn render_host_command(
    program: std::path::PathBuf,
    memory_limit_bytes: u64,
) -> zirv_flow_render::ChildCommand {
    zirv_flow_render::ChildCommand::render_host(program, memory_limit_bytes)
}

/// What this process was started to be.
#[derive(Debug, Eq, PartialEq)]
enum Launch {
    /// A render pool child: the first argument is `render-host`.
    RenderHost,
    /// The test server.
    Server,
}

fn launch(first_argument: Option<&OsStr>) -> Launch {
    if first_argument.is_some_and(|argument| argument == "render-host") {
        Launch::RenderHost
    } else {
        Launch::Server
    }
}

fn main() -> ExitCode {
    if launch(std::env::args_os().nth(1).as_deref()) == Launch::RenderHost {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_render_host_argument_makes_a_render_pool_child() {
        assert_eq!(launch(Some(OsStr::new("render-host"))), Launch::RenderHost);
    }

    #[test]
    fn no_argument_or_another_one_starts_the_server() {
        assert_eq!(launch(None), Launch::Server);
        assert_eq!(launch(Some(OsStr::new("serve"))), Launch::Server);
        assert_eq!(launch(Some(OsStr::new("render-host-2"))), Launch::Server);
    }
}
