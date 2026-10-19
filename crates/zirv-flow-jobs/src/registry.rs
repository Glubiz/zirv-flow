//! The job-kind registry: one of the few places new implementations plug in. A kind is
//! one module under `kinds/` plus one line in `kinds/mod.rs`.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::{JobContext, JobError};

/// A kind of background work.
pub trait JobKind: Send + Sync + 'static {
    /// The `kind` column value jobs of this kind carry, such as `maintenance.noop`.
    const KIND: &'static str;

    /// Runs one attempt. Writes go through `ctx` so they commit atomically with the job's
    /// completion; the work must be idempotent, since a lost lease runs the job again.
    fn run(
        &self,
        ctx: &mut JobContext<'_>,
        payload: Value,
    ) -> impl Future<Output = Result<(), JobError>> + Send;
}

/// [`JobKind`] without the associated constant, so kinds can sit in one list.
pub trait ErasedKind: Send + Sync {
    fn kind(&self) -> &'static str;

    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
        payload: Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>>;
}

impl<K: JobKind> ErasedKind for K {
    fn kind(&self) -> &'static str {
        K::KIND
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
        payload: Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(JobKind::run(self, ctx, payload))
    }
}

/// The kinds a worker can run.
#[derive(Clone)]
pub struct Registry {
    kinds: Vec<&'static dyn ErasedKind>,
}

impl Registry {
    /// Every kind this build ships.
    pub fn built_in() -> Self {
        Self {
            kinds: crate::kinds::ALL.to_vec(),
        }
    }

    /// Adds a kind. Tests register their own kinds this way; release builds have none.
    #[cfg(test)]
    pub(crate) fn with(mut self, kind: &'static dyn ErasedKind) -> Self {
        self.kinds.push(kind);
        self
    }

    pub fn get(&self, kind: &str) -> Option<&'static dyn ErasedKind> {
        self.kinds.iter().copied().find(|k| k.kind() == kind)
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.kinds.iter().map(|kind| kind.kind()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct First;
    struct Second;

    impl JobKind for First {
        const KIND: &'static str = "test.first";

        async fn run(&self, _ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
            Ok(())
        }
    }

    impl JobKind for Second {
        const KIND: &'static str = "test.second";

        async fn run(&self, _ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
            Err(JobError::new("always fails"))
        }
    }

    #[test]
    fn the_built_in_registry_ships_every_built_in_kind() {
        let registry = Registry::built_in();

        assert_eq!(
            registry.kinds(),
            vec![
                "maintenance.noop",
                "person.merge.rewrite",
                "traits.recompute",
                "traits.slot_backfill",
                "traits.slot_clear",
                "events.retention",
                "maintenance.ai_transcripts",
                "ingest.drain",
                "ingest.replay",
                "privacy.erase",
                "privacy.export",
                "import.dry_run",
                "import.run",
                "sync.run",
                "kafka.consume",
                "segment.refresh",
                "segment.reevaluate",
                "flow.step",
                "flow.enroll"
            ]
        );
        assert!(registry.get("maintenance.noop").is_some());
        assert!(registry.get("sync.run").is_some());
        assert!(registry.get("kafka.consume").is_some());
        assert!(registry.get("sync.nope").is_none());
    }

    #[test]
    fn built_in_kind_names_are_unique() {
        let mut kinds = Registry::built_in().kinds();
        kinds.sort_unstable();
        let total = kinds.len();
        kinds.dedup();

        assert_eq!(kinds.len(), total);
    }

    #[test]
    fn a_registered_test_kind_is_found_by_name() {
        let registry = Registry::built_in().with(&First).with(&Second);

        assert_eq!(registry.get("test.second").unwrap().kind(), "test.second");
        assert_eq!(
            registry.kinds(),
            vec![
                "maintenance.noop",
                "person.merge.rewrite",
                "traits.recompute",
                "traits.slot_backfill",
                "traits.slot_clear",
                "events.retention",
                "maintenance.ai_transcripts",
                "ingest.drain",
                "ingest.replay",
                "privacy.erase",
                "privacy.export",
                "import.dry_run",
                "import.run",
                "sync.run",
                "kafka.consume",
                "segment.refresh",
                "segment.reevaluate",
                "flow.step",
                "flow.enroll",
                "test.first",
                "test.second"
            ]
        );
        assert!(registry.get("test.third").is_none());
    }
}
