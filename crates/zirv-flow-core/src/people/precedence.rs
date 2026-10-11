//! Which source's value wins a trait (architecture section 7.1).

use time::OffsetDateTime;
use uuid::Uuid;

/// How competing sources are ranked for one trait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Precedence {
    Latest,
    SourcePriority,
    First,
}

impl Precedence {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::SourcePriority => "source_priority",
            Self::First => "first",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "latest" => Some(Self::Latest),
            "source_priority" => Some(Self::SourcePriority),
            "first" => Some(Self::First),
            _ => None,
        }
    }
}

/// One source record's value for a key.
#[derive(Clone, Debug, PartialEq)]
pub struct Contribution<V> {
    pub source: String,
    pub at: OffsetDateTime,
    pub record_id: Uuid,
    pub value: V,
}

fn newest<'a, V>(a: &&'a Contribution<V>, b: &&'a Contribution<V>) -> std::cmp::Ordering {
    (a.at, a.record_id).cmp(&(b.at, b.record_id))
}

/// The winning contribution among a person's live ones, or `None` when there are none.
pub fn resolve<'a, V>(
    precedence: Precedence,
    source_order: &[String],
    contributions: &'a [Contribution<V>],
) -> Option<&'a Contribution<V>> {
    match precedence {
        Precedence::Latest => contributions.iter().max_by(newest),
        Precedence::First => contributions.iter().min_by(newest),
        Precedence::SourcePriority => {
            let rank = |contribution: &Contribution<V>| {
                source_order
                    .iter()
                    .position(|source| *source == contribution.source)
                    .unwrap_or(usize::MAX)
            };
            let best = contributions.iter().map(rank).min()?;
            contributions
                .iter()
                .filter(|contribution| rank(contribution) == best)
                .max_by(newest)
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn contribution(
        source: &str,
        at: OffsetDateTime,
        id: u128,
        value: &'static str,
    ) -> Contribution<&'static str> {
        Contribution {
            source: source.to_owned(),
            at,
            record_id: Uuid::from_u128(id),
            value,
        }
    }

    fn sample() -> Vec<Contribution<&'static str>> {
        vec![
            contribution("crm", datetime!(2026-01-01 00:00 UTC), 1, "crm-old"),
            contribution("shop", datetime!(2026-03-01 00:00 UTC), 2, "shop-new"),
            contribution("web", datetime!(2026-02-01 00:00 UTC), 3, "web-mid"),
        ]
    }

    fn winner(
        precedence: Precedence,
        order: &[&str],
        list: &[Contribution<&'static str>],
    ) -> Option<&'static str> {
        let order: Vec<String> = order.iter().map(|s| (*s).to_owned()).collect();
        resolve(precedence, &order, list).map(|c| c.value)
    }

    #[test]
    fn latest_takes_the_greatest_time() {
        assert_eq!(winner(Precedence::Latest, &[], &sample()), Some("shop-new"));
    }

    #[test]
    fn first_takes_the_smallest_time() {
        assert_eq!(winner(Precedence::First, &[], &sample()), Some("crm-old"));
    }

    #[test]
    fn ties_go_to_the_greater_id_for_latest_and_the_smaller_for_first() {
        let at = datetime!(2026-01-01 00:00 UTC);
        let tied = vec![
            contribution("a", at, 5, "five"),
            contribution("b", at, 9, "nine"),
        ];
        assert_eq!(winner(Precedence::Latest, &[], &tied), Some("nine"));
        assert_eq!(winner(Precedence::First, &[], &tied), Some("five"));
    }

    #[test]
    fn source_priority_takes_the_first_listed_source_with_a_value() {
        assert_eq!(
            winner(Precedence::SourcePriority, &["web", "crm"], &sample()),
            Some("web-mid")
        );
        let without_web: Vec<_> = sample().into_iter().filter(|c| c.source != "web").collect();
        assert_eq!(
            winner(Precedence::SourcePriority, &["web", "crm"], &without_web),
            Some("crm-old")
        );
    }

    #[test]
    fn unlisted_sources_rank_after_the_list_by_latest() {
        assert_eq!(
            winner(Precedence::SourcePriority, &["nobody"], &sample()),
            Some("shop-new")
        );
        let unlisted: Vec<_> = sample().into_iter().filter(|c| c.source != "crm").collect();
        assert_eq!(
            winner(Precedence::SourcePriority, &["crm"], &unlisted),
            Some("shop-new")
        );
    }

    #[test]
    fn nothing_to_resolve_yields_none() {
        assert_eq!(winner(Precedence::Latest, &[], &[]), None);
        assert_eq!(winner(Precedence::SourcePriority, &["a"], &[]), None);
    }

    #[test]
    fn precedence_round_trips_through_text() {
        for precedence in [
            Precedence::Latest,
            Precedence::SourcePriority,
            Precedence::First,
        ] {
            assert_eq!(Precedence::parse(precedence.as_str()), Some(precedence));
        }
        assert_eq!(Precedence::parse("newest"), None);
    }
}
