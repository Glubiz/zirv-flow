//! The rate gates of the process: one [`RateGate`] per destination, shared by every send job that
//! runs here, so the block of tokens a take brings is spent by all of them and a destination costs
//! one take per block, not one per job (architecture 10.1 gate 8 and 14).
//!
//! The token bucket itself lives in the store, shared by every worker of every process
//! (`destination_rate_buckets`); this only holds the process's view of it. A gate is created on
//! the first send to a destination and dropped when the destination is archived.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use hashbrown::HashMap;
use uuid::Uuid;
use zirv_flow_store::{Admission, Clock, PgPool, RateGate, StoreError};

/// The rate gates of the destinations this process sends to.
#[derive(Default)]
pub struct RateGates {
    gates: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<RateGate>>>>,
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
        if admission == Admission::Gone {
            self.gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&destination);
        }
        Ok(admission)
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
