use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::{UserId, ValidationError};

/// The id that follows one piece of work through requests, jobs and decisions.
///
/// Rendered as 32 lowercase hex characters, the W3C trace-id shape.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TraceId(Uuid);

impl TraceId {
    /// A fresh random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0.simple())
    }
}

impl FromStr for TraceId {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid = value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !valid {
            return Err(ValidationError::new(
                "trace_id",
                "must be 32 lowercase hex characters",
            ));
        }
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| ValidationError::new("trace_id", "must be 32 lowercase hex characters"))
    }
}

impl Serialize for TraceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TraceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// What set a piece of work going (architecture section 12.4).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CauseKind {
    Event,
    TraitChange,
    Schedule,
    Manual,
    Api,
    DefinitionChange,
}

impl CauseKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::TraitChange => "trait_change",
            Self::Schedule => "schedule",
            Self::Manual => "manual",
            Self::Api => "api",
            Self::DefinitionChange => "definition_change",
        }
    }
}

impl fmt::Display for CauseKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CauseKind {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "event" => Ok(Self::Event),
            "trait_change" => Ok(Self::TraitChange),
            "schedule" => Ok(Self::Schedule),
            "manual" => Ok(Self::Manual),
            "api" => Ok(Self::Api),
            "definition_change" => Ok(Self::DefinitionChange),
            _ => Err(ValidationError::new("cause_kind", "is not a known cause")),
        }
    }
}

/// A cause kind and the durable id of what caused the work.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Cause {
    pub kind: CauseKind,
    pub id: String,
}

impl Cause {
    pub fn api(request_id: impl Into<String>) -> Self {
        Self {
            kind: CauseKind::Api,
            id: request_id.into(),
        }
    }

    pub fn schedule(id: Uuid) -> Self {
        Self::of(CauseKind::Schedule, id)
    }

    pub fn manual(user: UserId) -> Self {
        Self::of(CauseKind::Manual, user.as_uuid())
    }

    pub fn event(id: Uuid) -> Self {
        Self::of(CauseKind::Event, id)
    }

    pub fn trait_change(id: Uuid) -> Self {
        Self::of(CauseKind::TraitChange, id)
    }

    pub fn definition_change(id: Uuid) -> Self {
        Self::of(CauseKind::DefinitionChange, id)
    }

    /// Rebuilds a cause read back from storage, where the id is already text.
    pub fn from_stored(kind: CauseKind, id: String) -> Self {
        Self { kind, id }
    }

    fn of(kind: CauseKind, id: Uuid) -> Self {
        Self {
            kind,
            id: id.to_string(),
        }
    }
}

/// The trace a piece of work belongs to and what caused it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TraceContext {
    pub trace_id: TraceId,
    pub cause: Cause,
}

impl TraceContext {
    /// Starts a new trace with a fresh id.
    pub fn root(cause: Cause) -> Self {
        Self {
            trace_id: TraceId::new(),
            cause,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_round_trips_through_32_hex_characters() {
        let id = TraceId::new();
        let text = id.to_string();

        assert_eq!(text.len(), 32);
        assert!(!text.contains('-'));
        assert_eq!(text.parse::<TraceId>().unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{text}\""));
        assert_eq!(serde_json::from_str::<TraceId>(&json).unwrap(), id);
    }

    #[test]
    fn trace_id_rejects_hyphenated_and_wrong_length_forms() {
        let id = TraceId::new();

        assert!(id.as_uuid().to_string().parse::<TraceId>().is_err());
        assert!("abc".parse::<TraceId>().is_err());
        assert!(format!("{id}0").parse::<TraceId>().is_err());
        assert!(id.to_string().to_uppercase().parse::<TraceId>().is_err());
    }

    #[test]
    fn cause_constructors_set_kind_and_id() {
        let uuid = Uuid::new_v4();
        let user = UserId::new();

        assert_eq!(Cause::api("req-1").kind, CauseKind::Api);
        assert_eq!(Cause::api("req-1").id, "req-1");
        assert_eq!(Cause::schedule(uuid).kind, CauseKind::Schedule);
        assert_eq!(Cause::schedule(uuid).id, uuid.to_string());
        assert_eq!(Cause::manual(user).kind, CauseKind::Manual);
        assert_eq!(Cause::manual(user).id, user.to_string());
        assert_eq!(Cause::event(uuid).kind, CauseKind::Event);
        assert_eq!(Cause::trait_change(uuid).kind, CauseKind::TraitChange);
        assert_eq!(
            Cause::definition_change(uuid).kind,
            CauseKind::DefinitionChange
        );
    }

    #[test]
    fn cause_kinds_round_trip_and_unknown_is_rejected() {
        for kind in [
            CauseKind::Event,
            CauseKind::TraitChange,
            CauseKind::Schedule,
            CauseKind::Manual,
            CauseKind::Api,
            CauseKind::DefinitionChange,
        ] {
            assert_eq!(kind.as_str().parse::<CauseKind>().unwrap(), kind);
        }
        assert!("other".parse::<CauseKind>().is_err());
    }

    #[test]
    fn a_root_context_mints_a_fresh_trace_each_time() {
        let first = TraceContext::root(Cause::api("r"));
        let second = TraceContext::root(Cause::api("r"));

        assert_ne!(first.trace_id, second.trace_id);
    }
}
