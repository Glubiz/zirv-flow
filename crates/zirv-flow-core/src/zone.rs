//! IANA time zones from the tz database bundled into the binary, so every platform and the
//! static image agree and nothing reads `/usr/share/zoneinfo`.
//!
//! [`Zone`] is a chrono [`TimeZone`], so the cron math and the `DateTime<Utc>` boundaries
//! elsewhere keep working. It accepts the names the tz database spells, case-sensitively:
//! `europe/copenhagen` is not a zone, as it never was.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use chrono::{
    DateTime, Datelike, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset,
    TimeZone, Timelike, Utc,
};
use jiff::civil;
use jiff::tz::{AmbiguousOffset, TimeZoneDatabase};

/// The bundled database, named explicitly so the result never depends on feature unification
/// or on the zone files of the machine.
static DATABASE: LazyLock<TimeZoneDatabase> = LazyLock::new(TimeZoneDatabase::bundled);

/// A zone the bundled database has, but whose name is not a zone: it was never accepted.
const NOT_A_ZONE: &str = "Factory";

/// An IANA zone from the bundled database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Zone(jiff::tz::TimeZone);

/// The text is not the exact name of a zone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownZone;

impl fmt::Display for UnknownZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not an IANA time zone name")
    }
}

impl std::error::Error for UnknownZone {}

impl Zone {
    /// Coordinated Universal Time.
    pub const UTC: Zone = Zone(jiff::tz::TimeZone::UTC);

    /// A local time in this zone as an instant, or `None` when it is out of range.
    ///
    /// A time inside a gap (the clocks jump forward over it) is read with the offset in force
    /// before the jump, and a time that occurs twice (the clocks fall back) is its first
    /// occurrence.
    pub fn resolve_local(&self, local: NaiveDateTime) -> Option<DateTime<Utc>> {
        let seconds = self
            .0
            .to_ambiguous_timestamp(civil_of(&local)?)
            .compatible()
            .ok()?
            .as_second();
        // Subtract the offset from `local` itself, as chrono does, so a leap second
        // (nanosecond >= 1e9) keeps its representation.
        let offset = i32::try_from(local.and_utc().timestamp() - seconds).ok()?;
        local
            .checked_sub_offset(FixedOffset::east_opt(offset)?)
            .map(|utc| utc.and_utc())
    }

    fn offset(&self, seconds: i32) -> ZoneOffset {
        ZoneOffset {
            zone: self.clone(),
            fixed: FixedOffset::east_opt(seconds).unwrap_or_else(|| Utc.fix()),
        }
    }
}

/// The wall-clock fields of `local`, to the second.
fn civil_of(local: &NaiveDateTime) -> Option<civil::DateTime> {
    civil::DateTime::new(
        i16::try_from(local.year()).ok()?,
        local.month() as i8,
        local.day() as i8,
        local.hour() as i8,
        local.minute() as i8,
        local.second().min(59) as i8,
        0,
    )
    .ok()
}

impl FromStr for Zone {
    type Err = UnknownZone;

    fn from_str(name: &str) -> Result<Self, UnknownZone> {
        if name == NOT_A_ZONE {
            return Err(UnknownZone);
        }
        // The database ignores ASCII case; a zone's own spelling is the only one accepted.
        let zone = DATABASE.get(name).map_err(|_| UnknownZone)?;
        if zone.iana_name() == Some(name) {
            Ok(Zone(zone))
        } else {
            Err(UnknownZone)
        }
    }
}

/// The offset of a [`Zone`] at one instant.
#[derive(Clone, Debug)]
pub struct ZoneOffset {
    zone: Zone,
    fixed: FixedOffset,
}

impl Offset for ZoneOffset {
    fn fix(&self) -> FixedOffset {
        self.fixed
    }
}

impl fmt::Display for ZoneOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.fixed, f)
    }
}

impl TimeZone for Zone {
    type Offset = ZoneOffset;

    fn from_offset(offset: &ZoneOffset) -> Zone {
        offset.zone.clone()
    }

    fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<ZoneOffset> {
        // Any offset that occurs that day will do (as chrono-tz): midnight, else 23:59:59.
        let earliest = self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN));
        let latest = self.offset_from_local_datetime(
            &local.and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap()),
        );
        match (earliest, latest) {
            (result @ LocalResult::Single(_), _) | (_, result @ LocalResult::Single(_)) => result,
            (LocalResult::Ambiguous(offset, _), _) | (_, LocalResult::Ambiguous(offset, _)) => {
                LocalResult::Single(offset)
            }
            (LocalResult::None, LocalResult::None) => LocalResult::None,
        }
    }

    fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<ZoneOffset> {
        let Some(civil) = civil_of(local) else {
            return LocalResult::Single(self.offset(0));
        };
        match self.0.to_ambiguous_timestamp(civil).offset() {
            AmbiguousOffset::Unambiguous { offset } => {
                LocalResult::Single(self.offset(offset.seconds()))
            }
            AmbiguousOffset::Gap { .. } => LocalResult::None,
            AmbiguousOffset::Fold { before, after } => {
                LocalResult::Ambiguous(self.offset(before.seconds()), self.offset(after.seconds()))
            }
        }
    }

    fn offset_from_utc_date(&self, utc: &NaiveDate) -> ZoneOffset {
        self.offset_from_utc_datetime(&utc.and_time(NaiveTime::MIN))
    }

    fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> ZoneOffset {
        match jiff::Timestamp::from_second(utc.and_utc().timestamp()) {
            Ok(instant) => self.offset(self.0.to_offset(instant).seconds()),
            Err(_) => self.offset(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> Zone {
        name.parse().unwrap()
    }

    fn local(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn offset_at(zone: &Zone, utc: &str) -> i32 {
        zone.offset_from_utc_datetime(&local(utc))
            .fix()
            .local_minus_utc()
    }

    #[test]
    fn only_the_exact_spelling_of_a_zone_name_parses() {
        for name in [
            "UTC",
            "Etc/UTC",
            "GMT",
            "Etc/GMT+5",
            "US/Eastern",
            "Asia/Kolkata",
            "Zulu",
            "Europe/Copenhagen",
            "America/Argentina/Buenos_Aires",
        ] {
            assert_eq!(zone(name).0.iana_name(), Some(name));
        }
        for name in [
            "utc",
            "Utc",
            "europe/copenhagen",
            "Europe/copenhagen",
            "EUROPE/COPENHAGEN",
            "US/EASTERN",
            "us/eastern",
            "",
            " UTC",
            "UTC ",
            "etc/unknown",
            "+02:00",
            "Europe/Copenhagen\0",
            "Etc/Unknown",
            "Factory",
            "Not/AZone",
        ] {
            assert_eq!(name.parse::<Zone>(), Err(UnknownZone), "{name:?}");
        }
    }

    #[test]
    fn offsets_follow_the_zone_rules_at_an_instant() {
        let copenhagen = zone("Europe/Copenhagen");
        assert_eq!(offset_at(&copenhagen, "2030-01-15T08:00:00"), 3600);
        assert_eq!(offset_at(&copenhagen, "2030-07-15T08:00:00"), 7200);
        assert_eq!(
            offset_at(&zone("Etc/GMT+5"), "2030-07-15T08:00:00"),
            -18_000
        );
        assert_eq!(offset_at(&Zone::UTC, "2030-07-15T08:00:00"), 0);
    }

    #[test]
    fn a_gap_has_no_local_offset_and_a_fold_has_two_earliest_first() {
        let copenhagen = zone("Europe/Copenhagen");
        assert!(matches!(
            copenhagen.offset_from_local_datetime(&local("2030-03-31T02:30:00")),
            LocalResult::None
        ));
        let LocalResult::Ambiguous(first, second) =
            copenhagen.offset_from_local_datetime(&local("2030-10-27T02:30:00"))
        else {
            panic!("the autumn overlap is ambiguous");
        };
        assert_eq!(first.fix().local_minus_utc(), 7200);
        assert_eq!(second.fix().local_minus_utc(), 3600);
    }

    #[test]
    fn a_local_time_resolves_by_the_gap_and_overlap_rules() {
        let copenhagen = zone("Europe/Copenhagen");
        let at = |text| copenhagen.resolve_local(local(text)).unwrap().to_rfc3339();
        assert_eq!(at("2030-01-15T09:00:00"), "2030-01-15T08:00:00+00:00");
        assert_eq!(at("2030-03-31T02:30:00"), "2030-03-31T01:30:00+00:00");
        assert_eq!(at("2030-10-27T02:30:00"), "2030-10-27T00:30:00+00:00");
        assert_eq!(
            Zone::UTC
                .resolve_local(local("2030-03-31T02:30:00"))
                .unwrap()
                .to_rfc3339(),
            "2030-03-31T02:30:00+00:00"
        );
    }

    #[test]
    fn a_leap_second_keeps_its_representation_like_chrono() {
        let leap = local("2016-12-31T23:59:60");
        assert!(leap.nanosecond() >= 1_000_000_000);
        assert_eq!(Zone::UTC.resolve_local(leap), Some(leap.and_utc()));
        assert_eq!(
            zone("Europe/Copenhagen").resolve_local(leap),
            Some(local("2016-12-31T22:59:60").and_utc())
        );
    }

    #[test]
    fn a_local_date_takes_any_offset_that_occurs_that_day() {
        let sao_paulo = zone("America/Sao_Paulo");
        let date = NaiveDate::from_ymd_opt(2018, 11, 4).unwrap();
        let LocalResult::Single(offset) = sao_paulo.offset_from_local_date(&date) else {
            panic!("midnight is in a gap, 23:59:59 is not");
        };
        assert_eq!(offset.fix().local_minus_utc(), -7200);
        let ordinary = NaiveDate::from_ymd_opt(2030, 1, 15).unwrap();
        let LocalResult::Single(offset) =
            zone("Europe/Copenhagen").offset_from_local_date(&ordinary)
        else {
            panic!("an ordinary date has one offset");
        };
        assert_eq!(offset.fix().local_minus_utc(), 3600);
    }
}
