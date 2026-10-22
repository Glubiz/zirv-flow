//! The `delivery_attempts` row of one attempt, written one way for every outbound job.

use std::time::Duration;

use tracing::warn;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{
    AttemptOutcome, NewDeliveryAttempt, StatusClass, SubjectKind, count_delivery_attempt,
    count_delivery_attempt_record_failure, record_delivery_attempt,
};

use super::{Attempt, Verdict};
use crate::{JobContext, JobError};

/// The destination row and the item an attempt is recorded against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttemptTarget {
    pub(crate) destination_id: Uuid,
    pub(crate) subject_kind: SubjectKind,
    pub(crate) subject_id: Uuid,
}

/// The row for attempt number `number` that ended in `verdict` after `elapsed`. A 429 is
/// `Throttled`, any other retried failure `Retryable`.
pub(crate) fn attempt_of(
    target: &AttemptTarget,
    number: i32,
    verdict: &Verdict,
    elapsed: Duration,
) -> NewDeliveryAttempt {
    let (outcome, status) = match verdict {
        Verdict::Succeeded { status } => (AttemptOutcome::Delivered, Some(*status)),
        Verdict::Terminal { status, .. } => (AttemptOutcome::Terminal, *status),
        Verdict::Retry { status, reason, .. } if *reason == "http_429" => {
            (AttemptOutcome::Throttled, *status)
        }
        Verdict::Retry { status, .. } => (AttemptOutcome::Retryable, *status),
    };
    NewDeliveryAttempt {
        destination_id: target.destination_id,
        subject_kind: target.subject_kind,
        subject_id: target.subject_id,
        attempt: number,
        outcome,
        status_class: StatusClass::of(status),
        duration_ms: i32::try_from(elapsed.as_millis()).unwrap_or(i32::MAX),
    }
}

/// Records the attempt in a short transaction of its own on the worker pool and counts it after
/// that commits, so it persists whether the job commits, retries or fails. With no `target` (a call
/// to a raw URL, which has no destination row) nothing is recorded.
///
/// The recording runs as a spawned task, so a handler that is cancelled while it waits (a lost
/// lease) still leaves the row. The send has already happened: a recording that fails is warned
/// about and counted (`zirv_flow_delivery_attempt_record_failures_total`) and never changes the
/// job's result, because failing the job would send the message again. A context with no pool is
/// a programming error and fails only under `cfg(test)`.
pub(crate) async fn settle(
    ctx: &mut JobContext<'_>,
    target: Option<&AttemptTarget>,
    number: i32,
    done: &Attempt,
) -> Result<Option<NewDeliveryAttempt>, JobError> {
    let (Some(target), Some(workspace)) = (target, ctx.workspace_id()) else {
        return Ok(None);
    };
    let new = attempt_of(target, number, &done.verdict, done.elapsed);
    let Some(pool) = ctx.pool().cloned() else {
        if cfg!(test) {
            return Err(JobError::new(
                "the worker has no pool to record the attempt on",
            ));
        }
        record_failed(workspace, &new, "no pool");
        return Ok(Some(new));
    };
    let row = new.clone();
    let task = tokio::spawn(async move {
        let result = async {
            let mut tx = zirv_flow_store::begin(&pool).await?;
            record_delivery_attempt(&mut *tx, workspace, &row).await?;
            zirv_flow_store::commit(tx).await
        }
        .await;
        match result {
            Ok(()) => count_delivery_attempt(workspace, &row),
            Err(_) => record_failed(workspace, &row, "database error"),
        }
    });
    if task.await.is_err() {
        record_failed(workspace, &new, "task failed");
    }
    Ok(Some(new))
}

/// An attempt that was made and could not be recorded: logged by ids, never by URL, and counted.
fn record_failed(workspace: WorkspaceId, attempt: &NewDeliveryAttempt, why: &'static str) {
    warn!(
        workspace = %workspace.as_uuid(),
        destination = %attempt.destination_id,
        attempt = attempt.attempt,
        why,
        "a delivery attempt could not be recorded"
    );
    count_delivery_attempt_record_failure(workspace, attempt.destination_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> AttemptTarget {
        AttemptTarget {
            destination_id: Uuid::from_u128(1),
            subject_kind: SubjectKind::Message,
            subject_id: Uuid::from_u128(2),
        }
    }

    #[test]
    fn a_verdict_maps_to_an_outcome_and_a_status_class() {
        let wait = Duration::from_millis(1500);
        let retry = |status, reason| Verdict::Retry {
            status,
            reason,
            message: "m",
            retry_after: None,
        };
        let cases = [
            (
                Verdict::Succeeded { status: 204 },
                AttemptOutcome::Delivered,
                StatusClass::Success,
            ),
            (
                Verdict::Terminal {
                    status: Some(404),
                    reason: "http_4xx",
                },
                AttemptOutcome::Terminal,
                StatusClass::ClientError,
            ),
            (
                Verdict::Terminal {
                    status: Some(302),
                    reason: "http_status",
                },
                AttemptOutcome::Terminal,
                StatusClass::Redirect,
            ),
            (
                retry(Some(429), "http_429"),
                AttemptOutcome::Throttled,
                StatusClass::ClientError,
            ),
            (
                retry(Some(503), "http_5xx"),
                AttemptOutcome::Retryable,
                StatusClass::ServerError,
            ),
            (
                retry(None, "timeout"),
                AttemptOutcome::Retryable,
                StatusClass::None,
            ),
        ];

        for (verdict, outcome, class) in cases {
            let row = attempt_of(&target(), 3, &verdict, wait);

            assert_eq!(row.outcome, outcome, "{verdict:?}");
            assert_eq!(row.status_class, class, "{verdict:?}");
            assert_eq!((row.attempt, row.duration_ms), (3, 1500));
            assert_eq!(row.destination_id, Uuid::from_u128(1));
        }
    }

    #[test]
    fn a_duration_over_the_column_saturates() {
        let row = attempt_of(
            &target(),
            1,
            &Verdict::Succeeded { status: 200 },
            Duration::from_secs(u64::from(u32::MAX)),
        );

        assert_eq!(row.duration_ms, i32::MAX);
    }
}
