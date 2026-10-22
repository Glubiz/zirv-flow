//! The child's own memory limit (architecture 11.9).
//!
//! On Linux the child caps its address space with `RLIMIT_AS` through the `rlimit` crate, a safe
//! wrapper, so this repository holds no `unsafe`. A failed allocation then aborts the child with
//! the standard `memory allocation of N bytes failed` line on standard error, which is how the
//! pool tells a memory limit from any other death. Other operating systems have no such limit
//! here: the pool warns once at start and the children run unlimited, which is for development
//! only.

use std::io;

use tracing::warn;

#[cfg(target_os = "linux")]
fn set(bytes: u64) -> io::Result<()> {
    rlimit::setrlimit(rlimit::Resource::AS, bytes, bytes)
}

#[cfg(not(target_os = "linux"))]
fn set(_bytes: u64) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Apply the limit to this process, before it reads its first request. Without a limit, or on a
/// system that cannot apply one, this does nothing and says nothing: the pool warns once for the
/// whole process (see [`warn_unenforced`]), not every child on every respawn.
pub fn apply(limit_bytes: Option<u64>) -> io::Result<()> {
    match limit_bytes {
        Some(bytes) if cfg!(target_os = "linux") => set(bytes),
        _ => Ok(()),
    }
}

/// Warn, once per call, that children asked to limit their memory to `limit_bytes` cannot: this
/// platform has no such limit here, so they run unlimited, which is for development only. The pool
/// calls it once when it starts.
pub fn warn_unenforced(limit_bytes: Option<u64>) {
    if limit_bytes.is_some() && !cfg!(target_os = "linux") {
        warn!(
            "the render host has no OS memory limit on this platform; its children run \
             unlimited, which is for development only"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    #[test]
    fn a_child_applying_a_limit_never_logs() {
        let (result, logs) = capture(|| apply(None));
        assert!(result.is_ok());
        assert!(logs.is_empty(), "{logs}");
        if !cfg!(target_os = "linux") {
            let (result, logs) = capture(|| apply(Some(512 * 1024 * 1024)));
            assert!(result.is_ok());
            assert!(logs.is_empty(), "{logs}");
        }
    }

    #[test]
    fn off_linux_the_pool_warns_once_per_call_and_only_for_a_limit_it_cannot_enforce() {
        let ((), none) = capture(|| warn_unenforced(None));
        assert!(none.is_empty(), "{none}");
        if cfg!(target_os = "linux") {
            eprintln!("skipped: Linux applies the limit; the child tests cover it");
            return;
        }
        let ((), logs) = capture(|| warn_unenforced(Some(512 * 1024 * 1024)));
        assert_eq!(logs.matches("WARN").count(), 1, "{logs}");
        assert!(logs.contains("no OS memory limit"), "{logs}");
    }
}
