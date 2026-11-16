//! The rate gates of the process: one [`RateGate`] per destination, shared by every send job that
//! runs here, so the block of tokens a take brings is spent by all of them and a destination costs
//! one take per block, not one per job (architecture 10.1 gate 8 and 14).
//!
//! A take that found the bucket empty also leaves a *hold*: the instant the bucket will hold a
//! token. A send or forward job that wakes before then is queued again at once, without reading
//! its gates, reserving a cap slot or rendering, only to find the bucket empty again. The hold
//! never grants: the bucket stays authoritative, and a stale hold only delays a take by at most
//! the wait it recorded.
//!
//! The token bucket itself lives in the store, shared by every worker of every process
//! (`destination_rate_buckets`); this only holds the process's view of it. A gate is created on
//! the first send to a destination and dropped when the destination is archived.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use hashbrown::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_store::{Admission, Clock, PgPool, RateGate, StoreError};

/// The rate gates of the destinations this process sends to.
#[derive(Default)]
pub struct RateGates {
    gates: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<RateGate>>>>,
    /// Per destination, the instant its bucket was last found empty until.
    held: Mutex<HashMap<Uuid, OffsetDateTime>>,
}

impl fmt::Debug for RateGates {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let held = self
            .gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        formatter
            .debug_struct("RateGates")
            .field("destinations", &held)
            .finish()
    }
}

impl RateGates {
    /// Asks the destination's gate for one token: [`Admission::Granted`] spends it; `Wait` says
    /// how long until the bucket holds one; `Gone` says the destination is archived or does not
    /// exist, and the gate is dropped. Takers of one destination queue on its gate, so a block of
    /// tokens is never taken twice at once.
    pub(crate) async fn admit(
        &self,
        pool: &PgPool,
        destination: Uuid,
        clock: &Clock,
    ) -> Result<Admission, StoreError> {
        let gate =
            {
                let mut gates = self.gates.lock().unwrap_or_else(PoisonError::into_inner);
                Arc::clone(gates.entry(destination).or_insert_with(|| {
                    Arc::new(tokio::sync::Mutex::new(RateGate::new(destination)))
                }))
            };
        let admission = gate.lock().await.admit(pool, clock).await?;
        if admission == Admission::Granted {
            self.held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&destination);
        }
        if admission == Admission::Gone {
            self.gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&destination);
        }
        Ok(admission)
    }

    /// Remembers that the destination's bucket holds no token until `until`.
    pub(crate) fn hold(&self, destination: Uuid, until: OffsetDateTime) {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(destination, until);
    }

    /// How long from `now` (the job's clock) the destination's bucket is known to be empty, or
    /// none when there is no hold or it has passed.
    pub(crate) fn held(
        &self,
        destination: Uuid,
        now: OffsetDateTime,
    ) -> Option<std::time::Duration> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let until = *held.get(&destination)?;
        match (until - now).try_into() {
            Ok(wait) if until > now => Some(wait),
            _ => {
                held.remove(&destination);
                None
            }
        }
    }

    /// How many destinations the process holds a gate for.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_new_set_of_gates_is_empty_and_prints_its_size_only() {
        let gates = RateGates::default();

        assert_eq!(gates.len(), 0);
        assert_eq!(format!("{gates:?}"), "RateGates { destinations: 0 }");
    }

    #[test]
    fn a_hold_lasts_until_its_instant_and_not_for_another_destination() {
        let gates = RateGates::default();
        let destination = Uuid::from_u128(1);
        let now = time::macros::datetime!(2031-05-06 07:08:09 UTC);

        assert_eq!(gates.held(destination, now), None);
        gates.hold(destination, now + time::Duration::seconds(30));

        assert_eq!(
            gates.held(destination, now + time::Duration::seconds(10)),
            Some(Duration::from_secs(20))
        );
        assert_eq!(gates.held(Uuid::from_u128(2), now), None);
        assert_eq!(
            gates.held(destination, now + time::Duration::seconds(30)),
            None
        );
        assert_eq!(
            gates.held(destination, now),
            None,
            "an expired hold is gone"
        );
    }

    #[tokio::test]
    async fn an_admission_that_cannot_reach_the_store_is_an_error_and_keeps_the_gate() {
        // A pool whose database does not exist: the take fails and nothing is granted.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://127.0.0.1:1/none")
            .unwrap();
        let gates = RateGates::default();
        let destination = Uuid::from_u128(1);

        let result = tokio::time::timeout(
            Duration::from_secs(30),
            gates.admit(&pool, destination, &Clock::default()),
        )
        .await
        .expect("the take fails fast");

        assert!(result.is_err());
        assert_eq!(gates.len(), 1);
    }
}
