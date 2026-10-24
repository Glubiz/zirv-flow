//! Whether an open looks automated (architecture 10.2, OQ-10).
//!
//! Mail clients and gateways fetch the pixel without a person looking, so every `email.opened`
//! says whether it looks automated, and why. The verdict is computed at request time from the
//! user agent and the time since delivery, and only the verdict is stored: neither the user agent
//! nor the IP address is.
//!
//! The user agent is matched against a few generic markers and against the operator's own list
//! (`tracking.prefetch_user_agents`). A generic marker matches a token of the lower-cased user
//! agent, split on non-alphanumeric characters, that ends with it, so a fused fetcher name is
//! caught and `bot` in the middle of a longer word is a person. An operator entry is a
//! case-insensitive substring match, because operators
//! supply exact fragments. No product, provider or client is named here; an operator
//! who knows the fetchers in front of their audience adds them to the list.

use time::{Duration, OffsetDateTime};

/// Fragments any automated fetcher is likely to carry.
const GENERIC_MARKERS: [&str; 6] = ["proxy", "prefetch", "preview", "scanner", "crawler", "bot"];

/// An open this soon after delivery is a machine looking at the message, not a person reading it.
const DELIVERY_DELAY: Duration = Duration::seconds(10);

/// The reason when the user agent marks the open as automated.
pub(super) const USER_AGENT: &str = "user_agent";
/// The reason when the open came too soon after delivery.
pub(super) const DELIVERY_DELAY_REASON: &str = "delivery_delay";

/// Why the open looks automated; none when it looks like a person. `operator` holds the
/// operator's fragments, already lower-case.
pub(super) fn machine_reason(
    user_agent: Option<&str>,
    operator: &[String],
    delivered_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<&'static str> {
    if let Some(agent) = user_agent {
        let agent = agent.to_lowercase();
        let marked = agent
            .split(|c: char| !c.is_alphanumeric())
            .any(|token| GENERIC_MARKERS.iter().any(|marker| token.ends_with(marker)))
            || operator
                .iter()
                .any(|fragment| !fragment.is_empty() && agent.contains(fragment.as_str()));
        if marked {
            return Some(USER_AGENT);
        }
    }
    delivered_at
        .is_some_and(|delivered| now - delivered < DELIVERY_DELAY)
        .then_some(DELIVERY_DELAY_REASON)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    const DELIVERED: OffsetDateTime = datetime!(2031-05-06 07:08:09 UTC);

    fn later(seconds: i64) -> OffsetDateTime {
        DELIVERED + Duration::seconds(seconds)
    }

    #[test]
    fn a_generic_marker_ending_a_user_agent_token_marks_the_open() {
        for agent in [
            "Some-Image-Proxy/2.0",
            "Link PREFETCH agent",
            "mail-preview",
            "Security Scanner 1",
            "web-crawler",
            "examplebot/1",
            "someimageproxy",
        ] {
            assert_eq!(
                machine_reason(Some(agent), &[], Some(DELIVERED), later(3600)),
                Some(USER_AGENT),
                "{agent}"
            );
        }
    }

    #[test]
    fn a_marker_in_the_middle_of_a_word_does_not_mark_the_open() {
        for agent in ["Mozilla/5.0 Robotic", "abottle/1", "Mozilla bots"] {
            assert_eq!(
                machine_reason(Some(agent), &[], None, later(3600)),
                None,
                "{agent}"
            );
        }
        let operator = vec!["tle".to_owned()];
        assert_eq!(
            machine_reason(Some("abottle/1"), &operator, None, later(3600)),
            Some(USER_AGENT)
        );
    }

    #[test]
    fn the_operators_fragments_mark_the_open_case_insensitively() {
        let operator = vec!["acme-fetch".to_owned()];

        assert_eq!(
            machine_reason(Some("Acme-Fetch/9"), &operator, Some(DELIVERED), later(60)),
            Some(USER_AGENT)
        );
        assert_eq!(
            machine_reason(Some("Acme-Fetch/9"), &[], Some(DELIVERED), later(60)),
            None
        );
    }

    #[test]
    fn an_open_within_ten_seconds_of_delivery_is_marked_and_a_later_one_is_not() {
        for (seconds, expected) in [
            (0, Some(DELIVERY_DELAY_REASON)),
            (9, Some(DELIVERY_DELAY_REASON)),
            (-5, Some(DELIVERY_DELAY_REASON)),
            (10, None),
            (3600, None),
        ] {
            assert_eq!(
                machine_reason(None, &[], Some(DELIVERED), later(seconds)),
                expected,
                "{seconds}"
            );
        }
    }

    #[test]
    fn the_user_agent_wins_over_the_delay_and_a_plain_open_has_no_reason() {
        assert_eq!(
            machine_reason(Some("a-bot"), &[], Some(DELIVERED), later(1)),
            Some(USER_AGENT)
        );
        assert_eq!(
            machine_reason(Some("Mozilla/5.0"), &[], Some(DELIVERED), later(600)),
            None
        );
        assert_eq!(machine_reason(None, &[], None, later(1)), None);
    }

    #[test]
    fn an_empty_operator_fragment_marks_nothing() {
        assert_eq!(
            machine_reason(Some("Mozilla/5.0"), &[String::new()], None, later(600)),
            None
        );
    }
}
