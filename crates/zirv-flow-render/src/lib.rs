//! Isolated template rendering (architecture 11.9, ruling f5213fbb).
//!
//! Fuel and the end-of-render caps do not bound a template's intermediate memory, so a render
//! that allocates without bound could take the API or a worker down with it. This crate renders
//! in a child process instead:
//!
//! - [`RenderPool`] is the client used by `api` (preview) and `jobs` (send): a small fixed pool of
//!   children, a deadline per render, and kill-and-replace for a child that overruns or dies. A
//!   failure is a typed [`RenderFailure`] that costs one render, never the caller.
//! - [`host`] is the child side: a synchronous loop over `impl Read` and `impl Write` that the
//!   binary's hidden `render-host` subcommand runs on standard input and output, after
//!   [`limit`] has capped the child's address space (Linux).
//!
//! The wire shape (a request, a response) lives in `zirv-flow-templates`; the framing is a
//! 4-byte big-endian length followed by JSON, bounded in both directions.

mod failure;
mod frame;
pub mod host;
pub mod limit;
mod pool;

pub use failure::RenderFailure;
pub use frame::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};
pub use pool::{ChildCommand, PoolConfig, RenderPool};

/// Helpers for tests that need a child process: unit tests get no `CARGO_BIN_EXE_*`, so the
/// child is this test binary itself, re-run filtered to [`child_entry`], which does nothing
/// unless `ZIRV_RENDER_TEST_CHILD` names a mode.
///
/// libtest writes its banner to standard output before the test runs, so the command skips
/// stdout up to the banner's last line and the frames follow it. `-q` (terse) keeps libtest from
/// printing the test's name after the banner.
#[cfg(test)]
mod test_support {
    use std::ffi::OsStr;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use tracing_subscriber::fmt::MakeWriter;
    use zirv_flow_templates::{RenderResponse, RenderedWebhook};

    use crate::frame::{MAX_REQUEST_BYTES, read_frame, write_frame};
    use crate::{ChildCommand, host, limit};

    const BANNER: &[u8] = b"running 1 test\n";

    pub(crate) fn child(mode: &str) -> ChildCommand {
        child_with(mode, [("ZIRV_RENDER_TEST_UNUSED", "")])
    }

    pub(crate) fn child_with<V: AsRef<OsStr>>(
        mode: &str,
        envs: impl IntoIterator<Item = (&'static str, V)>,
    ) -> ChildCommand {
        let exe = std::env::current_exe().expect("the test binary has a path");
        let mut command = ChildCommand::new(exe)
            .arg("--exact")
            .arg("test_support::child_entry")
            .arg("--nocapture")
            .arg("--test-threads")
            .arg("1")
            .arg("-q")
            .env("ZIRV_RENDER_TEST_CHILD", mode)
            .skip_stdout_until(BANNER);
        for (key, value) in envs {
            command = command.env(key, value.as_ref());
        }
        command
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    /// Run `f` with a subscriber that records its log lines, and return them as text.
    pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, String) {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let logs =
            String::from_utf8_lossy(&buffer.0.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned();
        (result, logs)
    }

    fn first_run(marker: &str) -> bool {
        let path = std::path::Path::new(marker);
        if path.exists() {
            return false;
        }
        std::fs::write(path, b"x").expect("the marker can be written");
        true
    }

    fn serve_stdio() -> i32 {
        i32::from(host::serve(io::stdin().lock(), io::stdout().lock()).is_err())
    }

    fn take_request() {
        read_frame(&mut io::stdin().lock(), MAX_REQUEST_BYTES).expect("a request arrives");
    }

    /// The child: a no-op in a normal test run, a render host (or a broken one) when the parent
    /// sets `ZIRV_RENDER_TEST_CHILD`.
    #[test]
    fn child_entry() {
        let Ok(mode) = std::env::var("ZIRV_RENDER_TEST_CHILD") else {
            return;
        };
        if let Some(bytes) = std::env::var("ZIRV_RENDER_TEST_LIMIT_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            limit::apply(Some(bytes)).expect("the limit applies");
        }
        let marker = std::env::var("ZIRV_RENDER_TEST_MARKER").unwrap_or_default();
        let mut stdout = io::stdout().lock();
        let code = match mode.as_str() {
            "serve" => serve_stdio(),
            "hang_first" => {
                if first_run(&marker) {
                    take_request();
                    loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    }
                }
                serve_stdio()
            }
            "die_first" => {
                if first_run(&marker) {
                    take_request();
                    std::process::abort();
                }
                serve_stdio()
            }
            "fake_oom" => {
                take_request();
                eprintln!("memory allocation of 1073741824 bytes failed");
                std::process::abort();
            }
            "oversized" => {
                take_request();
                stdout.write_all(&[0xff; 4]).expect("stdout");
                stdout.flush().expect("stdout");
                0
            }
            "truncated" => {
                take_request();
                stdout
                    .write_all(&[0, 0, 1, 0, 1, 2, 3, 4, 5])
                    .expect("stdout");
                stdout.flush().expect("stdout");
                0
            }
            "malformed" => {
                take_request();
                write_frame(&mut stdout, b"this is not json").expect("stdout");
                0
            }
            "wrong_kind" => {
                take_request();
                let answer = RenderResponse::Webhook(RenderedWebhook {
                    headers: Vec::new(),
                    body: "{}".into(),
                });
                write_frame(&mut stdout, &serde_json::to_vec(&answer).expect("json"))
                    .expect("stdout");
                0
            }
            other => panic!("unknown test child mode {other}"),
        };
        drop(stdout);
        std::process::exit(code);
    }
}
