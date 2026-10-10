//! Password hashing off the async runtime. Argon2 takes tens of milliseconds and would
//! otherwise stall every other request on the same worker thread.

use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;
use zirv_flow_core::password;

/// Runs `work` on the blocking pool, with at most one task per CPU at a time across the
/// process: each argon2 hash holds ~19 MiB, and sign-in is reachable unauthenticated.
async fn bounded<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let limit = LIMIT.get_or_init(|| {
        Arc::new(Semaphore::new(
            std::thread::available_parallelism().map_or(4, usize::from),
        ))
    });
    let permit = limit
        .clone()
        .acquire_owned()
        .await
        .expect("the password semaphore is never closed");
    // The blocking task owns the permit, so a dropped request cannot release it while
    // the hash is still running.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
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

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Counts how many `bounded` tasks run at once, each blocking for `millis`.
    #[derive(Clone, Default)]
    struct Gauge {
        running: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl Gauge {
        fn spawn(&self, millis: u64) -> tokio::task::JoinHandle<()> {
            let gauge = self.clone();
            tokio::spawn(bounded(move || {
                let now = gauge.running.fetch_add(1, Ordering::SeqCst) + 1;
                gauge.peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(millis));
                gauge.running.fetch_sub(1, Ordering::SeqCst);
            }))
        }
    }

    fn limit() -> usize {
        std::thread::available_parallelism().map_or(4, usize::from)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_calls_never_exceed_the_limit() {
        let gauge = Gauge::default();

        let tasks: Vec<_> = (0..limit() * 3).map(|_| gauge.spawn(20)).collect();
        for task in tasks {
            task.await.unwrap();
        }

        assert!(gauge.peak.load(Ordering::SeqCst) <= limit());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dropped_caller_keeps_its_permit_until_the_work_ends() {
        let gauge = Gauge::default();
        let dropped: Vec<_> = (0..limit()).map(|_| gauge.spawn(400)).collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        for task in &dropped {
            task.abort();
        }

        let tasks: Vec<_> = (0..limit()).map(|_| gauge.spawn(20)).collect();
        for task in tasks {
            task.await.unwrap();
        }

        assert!(gauge.peak.load(Ordering::SeqCst) <= limit());
    }
}
