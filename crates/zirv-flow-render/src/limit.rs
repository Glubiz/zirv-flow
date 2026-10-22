//! The child's own memory limit (architecture 11.9).
//!
//! On Linux the child caps its address space with `RLIMIT_AS` through the `rlimit` crate, a safe
//! wrapper, so this repository holds no `unsafe`. A failed allocation then aborts the child with
//! the standard `memory allocation of N bytes failed` line on standard error, which is how the
//! pool tells a memory limit from any other death. Other operating systems have no such limit
//! here: the child warns once and runs unlimited, which is for development only.

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

/// Apply the limit to this process, before it reads its first request.
///
/// With no limit, or on a system that cannot apply one, this logs one warning and succeeds.
pub fn apply(limit_bytes: Option<u64>) -> io::Result<()> {
    match limit_bytes {
        Some(bytes) if cfg!(target_os = "linux") => set(bytes),
        Some(_) => {
            warn!(
                "the render host has no OS memory limit on this platform; it runs unlimited, \
                 which is for development only"
            );
            Ok(())
        }
        None => {
            warn!("the render host was started without a memory limit; it runs unlimited");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    #[test]
    fn without_a_limit_there_is_one_warning() {
        let (result, logs) = capture(|| apply(None));
        assert!(result.is_ok());
        assert_eq!(logs.matches("WARN").count(), 1, "{logs}");
        assert!(logs.contains("without a memory limit"), "{logs}");
    }

    #[test]
    fn off_linux_a_limit_warns_once_and_is_not_applied() {
        if cfg!(target_os = "linux") {
            eprintln!("skipped: Linux applies the limit; the child tests cover it");
            return;
        }
        let (result, logs) = capture(|| apply(Some(512 * 1024 * 1024)));
        assert!(result.is_ok());
        assert_eq!(logs.matches("WARN").count(), 1, "{logs}");
        assert!(logs.contains("no OS memory limit"), "{logs}");
    }
}
