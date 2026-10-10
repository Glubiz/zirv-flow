//! Password hashing off the async runtime. Argon2 takes tens of milliseconds and would
//! otherwise stall every other request on the same worker thread.

use std::sync::OnceLock;

use tokio::sync::Semaphore;
use zirv_flow_core::password;

/// Runs `work` on the blocking pool, with at most one task per CPU at a time across the
/// process: each argon2 hash holds ~19 MiB, and sign-in is reachable unauthenticated.
async fn bounded<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    static LIMIT: OnceLock<Semaphore> = OnceLock::new();
    let limit = LIMIT.get_or_init(|| {
        Semaphore::new(std::thread::available_parallelism().map_or(4, usize::from))
    });
    // The permit is held until the blocking task finishes.
    let _permit = limit
        .acquire()
        .await
        .expect("the password semaphore is never closed");
    tokio::task::spawn_blocking(work)
        .await
        .expect("the password task does not panic")
}

pub(crate) async fn hash(plain: String) -> String {
    bounded(move || password::hash(&plain)).await
}

pub(crate) async fn verify(plain: String, hash: String) -> bool {
    bounded(move || password::verify(&plain, &hash)).await
}

/// Spends the time of one verification, for sign-in paths that have no hash to check.
pub(crate) async fn dummy_verify() {
    bounded(password::dummy_verify).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hash_verifies_only_the_original_password() {
        let hashed = hash("correct horse battery".into()).await;

        assert!(verify("correct horse battery".into(), hashed.clone()).await);
        assert!(!verify("incorrect horse battery".into(), hashed).await);
    }

    #[tokio::test]
    async fn the_dummy_verification_completes() {
        dummy_verify().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_calls_never_exceed_the_limit() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let limit = std::thread::available_parallelism().map_or(4, usize::from);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let tasks: Vec<_> = (0..limit * 3)
            .map(|_| {
                let (running, peak) = (running.clone(), peak.clone());
                tokio::spawn(bounded(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    running.fetch_sub(1, Ordering::SeqCst);
                }))
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }

        assert!(peak.load(Ordering::SeqCst) <= limit);
    }
}
