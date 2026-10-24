//! The send limits of a workspace: frequency caps and quiet hours (gates 5 and 6 of the send
//! pipeline, architecture 10.1). Reading is for anyone who can read the workspace; replacing both
//! settings in one go is for workspace admins and is audited.
//!
//! A cap says at most `limit` messages on a channel to one person in any `window` (an ISO 8601
//! duration of whole hours, such as `PT12H` or `P1D`, up to 31 days), and what happens to a message
//! over the limit: `skip` ends it as `frequency_cap`, `defer` holds it until a slot frees up. All
//! of a workspace's caps must pass. Quiet hours are local `HH:MM` times in the recipient's time
//! zone on the weekdays a quiet period starts; an end at or before the start ends on the next day.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::Time;
use utoipa::ToSchema;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::Channel;
use zirv_flow_segments::{DurationError, format_duration, parse_duration};
use zirv_flow_store::{
    CapMode, NewFrequencyCap, PgPool, QuietHours, SendLimits, read_send_limits, replace_send_limits,
};

use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::problems;

/// The longest window of a cap, in hours: 31 days.
const MAX_WINDOW_HOURS: u64 = 31 * 24;
/// The most caps a workspace may have.
const MAX_CAPS: usize = 20;
/// The largest limit of a cap.
const MAX_LIMIT: i64 = 1_000_000;
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// One frequency cap.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
pub(crate) struct FrequencyCapBody {
    /// The channel the cap counts: `email`.
    #[schema(example = "email")]
    channel: String,
    /// The window, an ISO 8601 duration in whole hours or days, from `PT1H` to `P31D`.
    #[schema(example = "P1D")]
    window: String,
    /// The most messages one person gets in the window, at least 1.
    #[schema(example = 3, minimum = 1)]
    limit: i64,
    /// `skip` ends a message over the limit as `frequency_cap`; `defer` holds it until a slot
    /// leaves the window.
    #[schema(example = "skip")]
    mode: String,
}

/// The quiet hours of a workspace.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
pub(crate) struct QuietHoursBody {
    /// When a quiet period starts, local `HH:MM`.
    #[schema(example = "21:00")]
    start: String,
    /// When it ends, local `HH:MM`, different from the start. An end at or before the start is on
    /// the next day.
    #[schema(example = "08:00")]
    end: String,
    /// The weekdays (`mon` to `sun`) a quiet period starts on; not empty. A period that wraps
    /// midnight belongs to the day it starts on.
    days: Vec<String>,
}

/// The send limits of a workspace.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
pub(crate) struct SendSettingsBody {
    /// The frequency caps; all must pass.
    #[serde(default)]
    frequency_caps: Vec<FrequencyCapBody>,
    /// The quiet hours; null or absent for none.
    #[serde(default)]
    quiet_hours: Option<QuietHoursBody>,
}

impl From<&SendLimits> for SendSettingsBody {
    fn from(limits: &SendLimits) -> Self {
        Self {
            frequency_caps: limits
                .caps
                .iter()
                .map(|cap| FrequencyCapBody {
                    channel: cap.channel.clone(),
                    window: format_duration(u64::try_from(cap.window_hours).unwrap_or(0)),
                    limit: i64::from(cap.limit),
                    mode: cap.mode.as_str().to_owned(),
                })
                .collect(),
            quiet_hours: limits.quiet_hours.as_ref().map(|quiet| QuietHoursBody {
                start: clock_text(quiet.start),
                end: clock_text(quiet.end),
                days: quiet.days.clone(),
            }),
        }
    }
}

fn clock_text(time: Time) -> String {
    format!("{:02}:{:02}", time.hour(), time.minute())
}

/// `HH:MM`, two digits each, on a 24-hour clock.
fn parse_clock(text: &str) -> Option<Time> {
    let bytes = text.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let digits = |pair: &[u8]| {
        pair.iter()
            .all(u8::is_ascii_digit)
            .then(|| (pair[0] - b'0') * 10 + (pair[1] - b'0'))
    };
    Time::from_hms(digits(&bytes[..2])?, digits(&bytes[3..])?, 0).ok()
}

/// The settings as the store takes them, or every problem found, each at its JSON pointer.
fn validate(
    body: &SendSettingsBody,
) -> Result<(Vec<NewFrequencyCap>, Option<QuietHours>), ApiError> {
    check(body).map_err(problems)
}

fn check(
    body: &SendSettingsBody,
) -> Result<(Vec<NewFrequencyCap>, Option<QuietHours>), Vec<(String, String)>> {
    let mut found: Vec<(String, String)> = Vec::new();
    let mut caps = Vec::new();
    if body.frequency_caps.len() > MAX_CAPS {
        found.push((
            "/frequency_caps".to_owned(),
            format!("must have at most {MAX_CAPS} caps"),
        ));
    }
    for (index, cap) in body.frequency_caps.iter().enumerate().take(MAX_CAPS) {
        let before = found.len();
        let at = |field: &str| format!("/frequency_caps/{index}/{field}");
        if cap.channel.parse::<Channel>().is_err() {
            found.push((at("channel"), "must be a known channel: email".to_owned()));
        }
        let mode = match cap.mode.as_str() {
            "skip" => CapMode::Skip,
            "defer" => CapMode::Defer,
            _ => {
                found.push((at("mode"), "must be skip or defer".to_owned()));
                CapMode::Skip
            }
        };
        let limit = match i32::try_from(cap.limit) {
            Ok(limit) if (1..=MAX_LIMIT).contains(&cap.limit) => limit,
            _ => {
                found.push((at("limit"), format!("must be between 1 and {MAX_LIMIT}")));
                1
            }
        };
        let window_hours = match parse_duration(&cap.window) {
            Ok(hours) if hours <= MAX_WINDOW_HOURS => i32::try_from(hours).unwrap_or(1),
            Ok(_) | Err(DurationError::TooLong) => {
                found.push((at("window"), "must be at most 31 days".to_owned()));
                1
            }
            Err(DurationError::Invalid) => {
                found.push((
                    at("window"),
                    "must be an ISO 8601 duration in whole hours or days, such as PT12H or P1D"
                        .to_owned(),
                ));
                1
            }
        };
        if found.len() == before {
            caps.push(NewFrequencyCap {
                channel: cap.channel.clone(),
                window_hours,
                limit,
                mode,
            });
        }
    }
    let quiet = body
        .quiet_hours
        .as_ref()
        .and_then(|quiet| validate_quiet(quiet, &mut found));
    if found.is_empty() {
        Ok((caps, quiet))
    } else {
        Err(found)
    }
}

fn validate_quiet(quiet: &QuietHoursBody, found: &mut Vec<(String, String)>) -> Option<QuietHours> {
    let start = parse_clock(&quiet.start);
    if start.is_none() {
        found.push((
            "/quiet_hours/start".to_owned(),
            "must be a time of day as HH:MM".to_owned(),
        ));
    }
    let end = parse_clock(&quiet.end);
    if end.is_none() {
        found.push((
            "/quiet_hours/end".to_owned(),
            "must be a time of day as HH:MM".to_owned(),
        ));
    }
    if start.is_some() && start == end {
        found.push((
            "/quiet_hours/end".to_owned(),
            "must differ from the start".to_owned(),
        ));
    }
    let mut days = Vec::new();
    if quiet.days.is_empty() {
        found.push((
            "/quiet_hours/days".to_owned(),
            "must name at least one weekday".to_owned(),
        ));
    }
    for (index, day) in quiet.days.iter().enumerate() {
        if WEEKDAYS.contains(&day.as_str()) {
            days.push(day.clone());
        } else {
            found.push((
                format!("/quiet_hours/days/{index}"),
                "must be a weekday: mon, tue, wed, thu, fri, sat or sun".to_owned(),
            ));
        }
    }
    // The week's own order, each day once.
    let days: Vec<String> = WEEKDAYS
        .iter()
        .filter(|day| days.iter().any(|named| named == *day))
        .map(|day| (*day).to_owned())
        .collect();
    Some(QuietHours {
        start: start?,
        end: end?,
        days,
    })
}

/// Replaces the workspace's frequency caps and quiet hours in one transaction and audits
/// `send_settings.updated`. Returns the settings as stored.
pub(crate) async fn update_send_settings(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    request: &SendSettingsBody,
) -> Result<SendSettingsBody, ApiError> {
    let (caps, quiet) = validate(request)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    replace_send_limits(&mut tx, workspace, &caps, quiet.as_ref()).await?;
    let stored = read_send_limits(&mut tx, workspace).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "send_settings.updated",
        None,
        json!({"frequency_caps": stored.caps.len(), "quiet_hours": stored.quiet_hours.is_some()}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(SendSettingsBody::from(&stored))
}

/// The workspace's frequency caps and quiet hours. Anyone who can read the workspace.
#[utoipa::path(
    get,
    operation_id = "get_send_settings",
    path = "/api/v1/workspaces/{ws}/sending/settings",
    tag = "sending",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The send settings", body = SendSettingsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<SendSettingsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let limits = read_send_limits(&mut tx, workspace.id).await?;
    Ok(Json(SendSettingsBody::from(&limits)))
}

/// Replaces the workspace's frequency caps and quiet hours. Both are replaced together: a cap or
/// quiet hours left out are removed. Workspace admins only. Audited as `send_settings.updated`.
#[utoipa::path(
    put,
    operation_id = "put_send_settings",
    path = "/api/v1/workspaces/{ws}/sending/settings",
    tag = "sending",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = SendSettingsBody,
    responses(
        (status = 200, description = "The settings as stored", body = SendSettingsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An unknown channel or mode, a limit below 1, a window that does not parse or is over 31 days, an invalid time, equal start and end, or no days; each error names its JSON pointer", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<SendSettingsBody>,
) -> Result<Json<SendSettingsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let stored =
        update_send_settings(&state.pool, &principal.context(), workspace.id, &body).await?;
    Ok(Json(stored))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    fn cap(window: &str, limit: i64) -> FrequencyCapBody {
        FrequencyCapBody {
            channel: "email".to_owned(),
            window: window.to_owned(),
            limit,
            mode: "skip".to_owned(),
        }
    }

    fn quiet(start: &str, end: &str, days: &[&str]) -> QuietHoursBody {
        QuietHoursBody {
            start: start.to_owned(),
            end: end.to_owned(),
            days: days.iter().map(|day| (*day).to_owned()).collect(),
        }
    }

    fn pointers(body: SendSettingsBody) -> Vec<String> {
        let found = check(&body).err().expect("the settings are refused");
        let error = validate(&body).err().expect("the settings are refused");
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        found.into_iter().map(|(pointer, _)| pointer).collect()
    }

    #[test]
    fn a_clock_time_is_two_digits_a_colon_and_two_digits() {
        assert_eq!(
            parse_clock("08:30"),
            Some(Time::from_hms(8, 30, 0).unwrap())
        );
        assert_eq!(
            parse_clock("23:59"),
            Some(Time::from_hms(23, 59, 0).unwrap())
        );
        for bad in [
            "8:30", "24:00", "12:60", "12-30", "ab:cd", "", "12:300", "+1:00",
        ] {
            assert_eq!(parse_clock(bad), None, "{bad}");
        }
    }

    #[test]
    fn valid_settings_become_whole_hours_and_the_days_are_put_in_week_order() {
        let (caps, quiet) = validate(&SendSettingsBody {
            frequency_caps: vec![cap("P1D", 3), cap("PT12H", 1), cap("P31D", 10)],
            quiet_hours: Some(self::quiet("21:00", "08:00", &["fri", "mon", "fri"])),
        })
        .unwrap();

        let windows: Vec<i32> = caps.iter().map(|cap| cap.window_hours).collect();
        assert_eq!(windows, [24, 12, 744]);
        let quiet = quiet.unwrap();
        assert_eq!(quiet.days, ["mon", "fri"]);
        assert_eq!(quiet.start, Time::from_hms(21, 0, 0).unwrap());
    }

    #[test]
    fn every_refusal_names_the_json_pointer_of_the_field() {
        let mut unknown = cap("P1D", 1);
        "sms".clone_into(&mut unknown.channel);
        let mut sloppy = cap("P1D", 1);
        "sometimes".clone_into(&mut sloppy.mode);
        let body = SendSettingsBody {
            frequency_caps: vec![
                unknown,
                cap("P1D", 0),
                cap("P32D", 1),
                cap("tomorrow", 1),
                cap("PT30M", 1),
                sloppy,
            ],
            quiet_hours: Some(quiet("25:00", "08:00", &["mon", "someday"])),
        };

        assert_eq!(
            pointers(body),
            [
                "/frequency_caps/0/channel",
                "/frequency_caps/1/limit",
                "/frequency_caps/2/window",
                "/frequency_caps/3/window",
                "/frequency_caps/4/window",
                "/frequency_caps/5/mode",
                "/quiet_hours/start",
                "/quiet_hours/days/1",
            ]
        );
    }

    #[test]
    fn equal_start_and_end_and_no_days_are_refused() {
        let same = SendSettingsBody {
            frequency_caps: Vec::new(),
            quiet_hours: Some(quiet("08:00", "08:00", &["mon"])),
        };
        let none = SendSettingsBody {
            frequency_caps: Vec::new(),
            quiet_hours: Some(quiet("08:00", "09:00", &[])),
        };

        assert_eq!(pointers(same), ["/quiet_hours/end"]);
        assert_eq!(pointers(none), ["/quiet_hours/days"]);
    }

    #[test]
    fn stored_settings_read_back_as_the_text_they_were_written_in() {
        let body = SendSettingsBody {
            frequency_caps: vec![cap("PT36H", 2), cap("P2D", 5)],
            quiet_hours: Some(quiet("21:05", "08:00", &["sat", "sun"])),
        };
        let (caps, quiet) = validate(&body).unwrap();
        let limits = SendLimits {
            caps: caps
                .iter()
                .map(|cap| zirv_flow_store::FrequencyCap {
                    id: uuid::Uuid::nil(),
                    channel: cap.channel.clone(),
                    window_hours: cap.window_hours,
                    limit: cap.limit,
                    mode: cap.mode,
                })
                .collect(),
            quiet_hours: quiet,
        };

        let back = SendSettingsBody::from(&limits);

        assert_eq!(back.frequency_caps[0].window, "PT36H");
        assert_eq!(back.frequency_caps[1].window, "P2D");
        assert_eq!(back.quiet_hours.unwrap().start, "21:05");
    }
}
