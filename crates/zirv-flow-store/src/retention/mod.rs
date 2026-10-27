//! The retention settings that are not tied to one table (architecture section 16.3): the
//! windows that were constants, the deployment values the API lists, the per-workspace overrides
//! of the message and decision-log windows, and the last run of each window.

mod runs;
mod workspace;

pub use runs::{RetentionRun, record_retention_run, retention_runs};
pub use workspace::{
    WorkspaceRetention, WorkspaceWindows, find_workspace_retention, set_workspace_retention,
    workspace_windows,
};

use crate::{DecisionRetention, EventRetention, MessageRetention};

/// The inclusive bounds of `retention.job_history_days`.
pub const JOB_HISTORY_DAYS_BOUNDS: (u32, u32) = (1, 90);
/// The inclusive bounds of `retention.audit_days`.
pub const AUDIT_DAYS_BOUNDS: (u32, u32) = (90, 3650);
/// The inclusive bounds of `retention.import_file_days`.
pub const IMPORT_FILE_DAYS_BOUNDS: (u32, u32) = (1, 90);
/// The inclusive bounds of `retention.import_reject_days`.
pub const IMPORT_REJECT_DAYS_BOUNDS: (u32, u32) = (1, 365);
/// The inclusive bounds of `retention.export_hours`.
pub const EXPORT_HOURS_BOUNDS: (u32, u32) = (1, 168);
/// The inclusive bounds of `retention.test_send_months`.
pub const TEST_SEND_MONTHS_BOUNDS: (u32, u32) = (1, 36);

/// The windows that used to be constants, set by the operator. The defaults are the old values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionSettings {
    /// Days the uploaded file of an import is kept after the import ended.
    pub import_file_days: u32,
    /// Days the refused rows of an import are kept.
    pub import_reject_days: u32,
    /// Hours a person export stays downloadable.
    pub export_hours: u32,
    /// Months the test-send log is kept.
    pub test_send_months: u32,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            import_file_days: 7,
            import_reject_days: 30,
            export_hours: 24,
            test_send_months: 13,
        }
    }
}

/// Every deployment-wide retention value the API lists next to the workspace's own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeploymentRetention {
    pub events: EventRetention,
    pub messages: MessageRetention,
    pub decisions: DecisionRetention,
    pub job_history_days: u32,
    pub audit_days: u32,
    pub settings: RetentionSettings,
}

impl Default for DeploymentRetention {
    fn default() -> Self {
        Self {
            events: EventRetention::default(),
            messages: MessageRetention::default(),
            decisions: DecisionRetention::default(),
            job_history_days: 14,
            audit_days: 730,
            settings: RetentionSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_values_that_were_constants() {
        let settings = RetentionSettings::default();

        assert_eq!(
            i32::try_from(settings.import_file_days).unwrap(),
            crate::CHUNK_RETENTION_DAYS
        );
        assert_eq!(
            i32::try_from(settings.import_reject_days).unwrap(),
            crate::REJECT_RETENTION_DAYS
        );
        assert_eq!(
            i32::try_from(settings.test_send_months).unwrap(),
            crate::TEST_SEND_RETENTION_MONTHS
        );
        assert_eq!(settings.export_hours, 24);
    }

    #[test]
    fn every_default_lies_within_its_bounds() {
        let settings = RetentionSettings::default();

        for (value, (low, high)) in [
            (settings.import_file_days, IMPORT_FILE_DAYS_BOUNDS),
            (settings.import_reject_days, IMPORT_REJECT_DAYS_BOUNDS),
            (settings.export_hours, EXPORT_HOURS_BOUNDS),
            (settings.test_send_months, TEST_SEND_MONTHS_BOUNDS),
        ] {
            assert!((low..=high).contains(&value));
        }
    }
}
