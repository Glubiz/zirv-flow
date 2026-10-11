//! The profile of one person (architecture section 12.3): identities by source, traits with
//! provenance, consent, lists and the people merged into it.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::people::IdentifierKind;
use zirv_flow_store::{IdentityGroup, Profile, ProfileMerge, ProfileTrait, StoredIdentity};

use super::search::DisplayBody;
use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::consent::person::ConsentStateBody;
use crate::lists::person::PersonListBody;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct ProfileIdentityBody {
    #[schema(example = "email")]
    kind: String,
    /// Null when `masked`: only workspace admins, the owner and `admin` keys see values.
    value: Option<String>,
    masked: bool,
}

impl ProfileIdentityBody {
    fn new(identity: &StoredIdentity, reveal: bool) -> Self {
        Self {
            kind: identity.kind.clone(),
            value: reveal.then(|| identity.value.clone()),
            masked: !reveal,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IdentityGroupBody {
    /// The source whose live records list these identities.
    #[schema(example = "crm")]
    source: String,
    identities: Vec<ProfileIdentityBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProfileTraitBody {
    #[schema(example = "plan")]
    key: String,
    /// `string`, `number`, `boolean`, `timestamp`, `date` or `string_list`; null for a key the
    /// registry does not know.
    trait_type: Option<String>,
    /// Null when `masked`: PII-flagged traits are shown only to workspace admins, the owner
    /// and `admin` keys.
    value: Option<Value>,
    masked: bool,
    pii: bool,
    /// The source that supplied the value.
    source: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    observed_at: Option<OffsetDateTime>,
}

impl ProfileTraitBody {
    fn new(item: ProfileTrait, reveal: bool) -> Self {
        let masked = item.pii && !reveal;
        Self {
            key: item.key,
            trait_type: item.trait_type.map(|kind| kind.to_string()),
            value: (!masked).then_some(item.value),
            masked,
            pii: item.pii,
            source: item.source,
            observed_at: item.observed_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MergeBody {
    merged_person_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    merged_at: OffsetDateTime,
}

impl From<ProfileMerge> for MergeBody {
    fn from(merge: ProfileMerge) -> Self {
        Self {
            merged_person_id: merge.merged_person_id,
            merged_at: merge.merged_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProfileBody {
    /// The surviving person.
    id: Uuid,
    /// The id that was asked for, when it was merged into `id`; null otherwise.
    merged_from: Option<Uuid>,
    /// The strongest identifier; null for a person with none.
    display: Option<DisplayBody>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
    /// By source name.
    identity_groups: Vec<IdentityGroupBody>,
    /// Identities no live source record lists, such as one first seen on an event.
    observed: Vec<ProfileIdentityBody>,
    /// By key.
    traits: Vec<ProfileTraitBody>,
    /// The channel-wide state first, then topics by key.
    consent: Vec<ConsentStateBody>,
    /// By name.
    lists: Vec<PersonListBody>,
    /// The people merged into this one, oldest first.
    merges: Vec<MergeBody>,
}

/// The strongest identifier of the profile, by kind strength and then by kind and value.
fn display_of(profile: &Profile, reveal: bool) -> Option<DisplayBody> {
    let strength = |kind: &str| IdentifierKind::parse(kind).map_or(0, |kind| kind.strength());
    profile
        .identity_groups
        .iter()
        .flat_map(|group| &group.identities)
        .chain(&profile.observed)
        .max_by(|a, b| {
            strength(&a.kind)
                .cmp(&strength(&b.kind))
                .then_with(|| (&b.kind, &b.value).cmp(&(&a.kind, &a.value)))
        })
        .map(|identity| DisplayBody::new(identity.kind.clone(), identity.value.clone(), reveal))
}

fn group_body(group: &IdentityGroup, reveal: bool) -> IdentityGroupBody {
    IdentityGroupBody {
        source: group.source.clone(),
        identities: group
            .identities
            .iter()
            .map(|identity| ProfileIdentityBody::new(identity, reveal))
            .collect(),
    }
}

/// Everything the system holds about one person: identities grouped by the sources that list
/// them, traits with the source and time of each value, consent, lists and the people merged
/// into this one. A person that was merged into another is read as the survivor, with
/// `merged_from` set. Identifier values and PII-flagged trait values are shown to the owner,
/// workspace admins and `admin` keys, and masked for everyone else.
#[utoipa::path(
    get,
    operation_id = "get_person",
    path = "/api/v1/workspaces/{ws}/people/{id}",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id")
    ),
    responses(
        (status = 200, description = "The profile", body = ProfileBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ProfileBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let requested: Uuid = path_id(&id)?;
    let profile = zirv_flow_store::load_profile(&state.pool, workspace.id, requested)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    let consent = zirv_flow_store::consent_for(&state.pool, workspace.id, profile.id)
        .await?
        .unwrap_or_default();
    let lists = zirv_flow_store::lists_for(&state.pool, workspace.id, profile.id).await?;
    let display = display_of(&profile, reveal);
    Ok(Json(ProfileBody {
        id: profile.id,
        merged_from: profile.merged_from,
        display,
        created_at: profile.created_at,
        updated_at: profile.updated_at,
        identity_groups: profile
            .identity_groups
            .iter()
            .map(|group| group_body(group, reveal))
            .collect(),
        observed: profile
            .observed
            .iter()
            .map(|identity| ProfileIdentityBody::new(identity, reveal))
            .collect(),
        traits: profile
            .traits
            .into_iter()
            .map(|item| ProfileTraitBody::new(item, reveal))
            .collect(),
        consent: consent.into_iter().map(ConsentStateBody::from).collect(),
        lists: lists.into_iter().map(PersonListBody::from).collect(),
        merges: profile.merges.into_iter().map(MergeBody::from).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::people::TraitType;

    use super::*;

    fn identity(kind: &str, value: &str) -> StoredIdentity {
        StoredIdentity {
            id: Uuid::nil(),
            person_id: Uuid::nil(),
            kind: kind.into(),
            value: value.into(),
        }
    }

    fn profile(groups: Vec<IdentityGroup>, observed: Vec<StoredIdentity>) -> Profile {
        Profile {
            id: Uuid::nil(),
            merged_from: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            identity_groups: groups,
            observed,
            traits: Vec::new(),
            merges: Vec::new(),
        }
    }

    fn item(pii: bool) -> ProfileTrait {
        ProfileTrait {
            key: "city".into(),
            value: json!("Aarhus"),
            source: Some("crm".into()),
            observed_at: None,
            trait_type: Some(TraitType::String),
            pii,
        }
    }

    #[test]
    fn only_a_pii_trait_is_masked_and_only_for_those_without_access() {
        let hidden = ProfileTraitBody::new(item(true), false);
        let shown = ProfileTraitBody::new(item(true), true);
        let plain = ProfileTraitBody::new(item(false), false);

        assert!(hidden.masked && hidden.value.is_none() && hidden.pii);
        assert_eq!(hidden.key, "city");
        assert_eq!(hidden.source.as_deref(), Some("crm"));
        assert!(!shown.masked);
        assert_eq!(shown.value, Some(json!("Aarhus")));
        assert!(!plain.masked);
        assert_eq!(plain.value, Some(json!("Aarhus")));
        assert_eq!(plain.trait_type.as_deref(), Some("string"));
    }

    #[test]
    fn an_identity_is_masked_without_access_and_keeps_its_kind() {
        let hidden = ProfileIdentityBody::new(&identity("email", "a@example.com"), false);
        let shown = ProfileIdentityBody::new(&identity("email", "a@example.com"), true);

        assert!(hidden.masked && hidden.value.is_none() && hidden.kind == "email");
        assert_eq!(shown.value.as_deref(), Some("a@example.com"));
    }

    #[test]
    fn the_display_identifier_is_the_strongest_one_listed_or_observed() {
        let groups = vec![IdentityGroup {
            source: "crm".into(),
            identities: vec![
                identity("anonymous_id", "anon-1"),
                identity("phone", "+4511"),
            ],
        }];

        let from_groups = display_of(&profile(groups.clone(), Vec::new()), true).unwrap();
        let with_observed = display_of(
            &profile(groups, vec![identity("email", "a@example.com")]),
            false,
        )
        .unwrap();

        assert_eq!(from_groups.kind, "phone");
        assert_eq!(with_observed.kind, "email");
        assert!(with_observed.value.is_none() && with_observed.masked);
        assert!(display_of(&profile(Vec::new(), Vec::new()), true).is_none());
    }
}
