//! What a provider's failed response means: the manifest's rules in order, then the defaults.

use serde_json::Value;

use super::super::error::{DestinationError, Refusal};
use crate::manifest::{ErrorAction, ErrorRule};

/// The text of a JSON scalar, as the manifest's literal lists write it.
pub(crate) fn literal_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        _ => None,
    }
}

/// The action for a response with `status` and `body` (if it was JSON): the first rule whose
/// status range holds the status and whose body condition, if it has one, holds; otherwise the
/// defaults: 408, 429 and 5xx retry, any other status is terminal.
pub(crate) fn classify(rules: &[ErrorRule], status: u16, body: Option<&Value>) -> ErrorAction {
    let matched = rules.iter().find(|rule| {
        rule.status.contains(status)
            && rule.body.as_ref().is_none_or(|condition| {
                body.and_then(|body| body.pointer(&condition.pointer))
                    .and_then(literal_of)
                    .is_some_and(|found| condition.values.contains(&found))
            })
    });
    match matched {
        Some(rule) => rule.action,
        None => match status {
            408 | 429 | 500..=599 => ErrorAction::Retry,
            _ => ErrorAction::Terminal,
        },
    }
}

/// The error a classified response is. `retry_after` is the raw header; a retry keeps it, and
/// the pipeline reads it against its own clock. A recipient failure is a hard bounce, as a mail
/// server's permanent refusal of the recipient is.
pub(crate) fn error_for(
    action: ErrorAction,
    status: u16,
    retry_after: Option<String>,
) -> DestinationError {
    if !(400..=599).contains(&status) {
        // A redirect or an unexpected informational status: never followed, never retried.
        return DestinationError::Answered {
            status,
            retry_after: None,
        };
    }
    match action {
        ErrorAction::RecipientFailure => DestinationError::Refused(Refusal::HardBounce),
        ErrorAction::Retry if matches!(status, 408 | 429 | 500..=599) => {
            DestinationError::Answered {
                status,
                retry_after,
            }
        }
        ErrorAction::Retry => DestinationError::Other("the provider asked to retry".to_owned()),
        ErrorAction::Terminal if matches!(status, 400..=499) && !matches!(status, 408 | 429) => {
            DestinationError::Answered {
                status,
                retry_after: None,
            }
        }
        ErrorAction::Terminal => DestinationError::Refused(Refusal::Rejected),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::destinations::error::Disposition;
    use crate::manifest::{BodyMatch, StatusRange};

    fn rule(min: u16, max: u16, body: Option<(&str, &[&str])>, action: ErrorAction) -> ErrorRule {
        ErrorRule {
            status: StatusRange { min, max },
            body: body.map(|(pointer, values)| BodyMatch {
                pointer: pointer.to_owned(),
                values: values.iter().map(|value| (*value).to_owned()).collect(),
            }),
            action,
        }
    }

    #[test]
    fn the_defaults_retry_408_429_and_5xx_and_stop_at_any_other_status() {
        for status in [408, 429, 500, 502, 599] {
            assert_eq!(classify(&[], status, None), ErrorAction::Retry, "{status}");
        }
        for status in [400, 401, 403, 404, 409, 422, 301, 100] {
            assert_eq!(
                classify(&[], status, None),
                ErrorAction::Terminal,
                "{status}"
            );
        }
    }

    #[test]
    fn the_first_matching_rule_wins_and_a_body_condition_must_hold() {
        let rules = [
            rule(
                422,
                422,
                Some(("/error/code", &["no_such_mailbox", "7"])),
                ErrorAction::RecipientFailure,
            ),
            rule(400, 499, None, ErrorAction::Terminal),
            rule(500, 599, None, ErrorAction::Terminal),
        ];

        let bounce = json!({"error": {"code": "no_such_mailbox"}});
        let numeric = json!({"error": {"code": 7}});
        let other = json!({"error": {"code": "too_big"}});
        assert_eq!(
            classify(&rules, 422, Some(&bounce)),
            ErrorAction::RecipientFailure
        );
        assert_eq!(
            classify(&rules, 422, Some(&numeric)),
            ErrorAction::RecipientFailure
        );
        assert_eq!(classify(&rules, 422, Some(&other)), ErrorAction::Terminal);
        assert_eq!(classify(&rules, 422, None), ErrorAction::Terminal);
        assert_eq!(
            classify(&rules, 503, None),
            ErrorAction::Terminal,
            "a rule overrides the retry default"
        );
        assert_eq!(
            classify(&rules, 429, None),
            ErrorAction::Terminal,
            "the first rule that matches wins, a broad one included"
        );
    }

    #[test]
    fn each_action_is_the_error_the_pipeline_judges_as_expected() {
        let retry = error_for(ErrorAction::Retry, 503, Some("30".into()));
        assert_eq!(
            retry,
            DestinationError::Answered {
                status: 503,
                retry_after: Some("30".into())
            }
        );
        assert_eq!(retry.disposition(), Disposition::Retryable);
        assert_eq!(
            error_for(ErrorAction::Retry, 429, Some("5".into())).disposition(),
            Disposition::Throttled { retry_after: None }
        );
        assert_eq!(
            error_for(ErrorAction::Retry, 409, None).disposition(),
            Disposition::Retryable,
            "a rule can retry a status the defaults would stop at"
        );
        assert_eq!(
            error_for(ErrorAction::Terminal, 422, None).disposition(),
            Disposition::Terminal
        );
        assert_eq!(
            error_for(ErrorAction::Terminal, 503, None),
            DestinationError::Refused(Refusal::Rejected),
            "a rule can stop at a status the defaults would retry"
        );
        assert_eq!(
            error_for(ErrorAction::RecipientFailure, 422, None),
            DestinationError::Refused(Refusal::HardBounce)
        );
        assert_eq!(
            error_for(ErrorAction::Terminal, 302, None),
            DestinationError::Answered {
                status: 302,
                retry_after: None
            }
        );
    }
}
