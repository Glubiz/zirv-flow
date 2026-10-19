//! The error a job handler returns.

use std::fmt;
use std::time::Duration;

use zirv_flow_store::StoreError;

/// Why a handler failed. The message is stored as the job's `last_error` and shown to
/// operators, so it is always a fixed description and never carries payload values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobError {
    message: &'static str,
    retry_after: Option<Duration>,
}

impl JobError {
    pub const fn new(message: &'static str) -> Self {
        Self {
            message,
            retry_after: None,
        }
    }

    /// Asks the worker to wait at least this long before the next attempt, such as the time a
    /// server named in a `Retry-After` header. The worker never waits less than its own backoff
    /// and never more than the backoff cap.
    pub const fn with_retry_after(mut self, wait: Duration) -> Self {
        self.retry_after = Some(wait);
        self
    }

    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for JobError {}

// Database errors can quote the values involved (a duplicate key names the key), so only
// a fixed description survives the conversion.
impl From<sqlx::Error> for JobError {
    fn from(_: sqlx::Error) -> Self {
        Self::new("database query failed")
    }
}

impl From<StoreError> for JobError {
    fn from(error: StoreError) -> Self {
        if error.is_conflict() {
            Self::new("database write conflicted with an existing value")
        } else {
            Self::new("database query failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_is_the_fixed_message() {
        let error = JobError::new("the upstream refused the request");

        assert_eq!(error.to_string(), "the upstream refused the request");
        assert_eq!(error.message(), "the upstream refused the request");
    }

    #[test]
    fn an_error_can_ask_for_a_later_retry_without_changing_its_message() {
        let error = JobError::new("the destination asked for a pause")
            .with_retry_after(Duration::from_secs(30));

        assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(error.to_string(), "the destination asked for a pause");
        assert_eq!(JobError::new("x").retry_after(), None);
    }

    #[test]
    fn a_database_error_does_not_carry_its_detail() {
        let error = JobError::from(sqlx::Error::Protocol("Key (email)=(a@b.c) exists".into()));

        assert_eq!(error.to_string(), "database query failed");
    }

    #[tokio::test]
    async fn a_store_error_becomes_a_fixed_message() {
        let store_error = zirv_flow_store::connect("not a database url", 1)
            .await
            .unwrap_err();
        let error = JobError::from(store_error);

        assert_eq!(error.to_string(), "database query failed");
    }
}
