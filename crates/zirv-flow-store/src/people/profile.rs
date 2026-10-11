//! The profile of one person (architecture section 12.3): identities grouped by the sources
//! that listed them, traits with the source that supplied each and when it was observed, and
//! the people merged into this one.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::TraitType;

use super::bounded::begin_bounded;
use super::profiles::{StoredIdentity, find_person};
use crate::StoreError;

/// The identities one source lists for the person.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentityGroup {
    pub source: String,
    pub identities: Vec<StoredIdentity>,
}

/// One trait of a person with its provenance.
#[derive(Clone, PartialEq)]
pub struct ProfileTrait {
    pub key: String,
    pub value: Value,
    /// The source that supplied the value.
    pub source: Option<String>,
    /// When that source observed the value.
    pub observed_at: Option<OffsetDateTime>,
    /// Absent for a key the registry does not know.
    pub trait_type: Option<TraitType>,
    /// A key the registry does not know counts as PII.
    pub pii: bool,
}

// Trait values are personal data.
impl fmt::Debug for ProfileTrait {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileTrait")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// A person merged into this one.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileMerge {
    pub merged_person_id: Uuid,
    pub merged_at: OffsetDateTime,
}

/// What the profile page shows of the person itself. Consent and lists have their own reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// The surviving person.
    pub id: Uuid,
    /// The id that was asked for, when it was merged into `id`.
    pub merged_from: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// By source name.
    pub identity_groups: Vec<IdentityGroup>,
    /// Identities no live source record lists, such as one first seen on an event.
    pub observed: Vec<StoredIdentity>,
    /// By key.
    pub traits: Vec<ProfileTrait>,
    /// Oldest first.
    pub merges: Vec<ProfileMerge>,
}

/// The profile of `person`, or of its survivor when it was merged. `None` when the workspace
/// has no such person.
pub async fn load_profile(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Profile>, StoreError> {
    let Some(found) = find_person(pool, workspace, person).await? else {
        return Ok(None);
    };
    let mut tx = begin_bounded(pool).await?;
    let identities: Vec<StoredIdentity> = sqlx::query_as(
        "SELECT id, person_id, kind, value FROM identities \
         WHERE workspace_id = $1 AND person_id = $2 ORDER BY kind, value",
    )
    .bind(workspace.as_uuid())
    .bind(found.id)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let records: Vec<(String, Value, Value)> = sqlx::query_as(
        "SELECT source, identifiers, traits FROM source_records \
         WHERE workspace_id = $1 AND person_id = $2 AND deleted_at IS NULL \
         ORDER BY source, stream, record_key",
    )
    .bind(workspace.as_uuid())
    .bind(found.id)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let definitions: Vec<(String, String, bool)> =
        sqlx::query_as("SELECT key, type, pii FROM trait_definitions WHERE workspace_id = $1")
            .bind(workspace.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::bounded)?;
    let merges: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT merged_person_id, created_at FROM person_merges \
         WHERE workspace_id = $1 AND survivor_person_id = $2 ORDER BY created_at, id",
    )
    .bind(workspace.as_uuid())
    .bind(found.id)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    drop(tx);

    let mut groups: BTreeMap<String, Vec<StoredIdentity>> = BTreeMap::new();
    let mut listed = vec![false; identities.len()];
    for (source, listed_identifiers, _) in &records {
        let pairs: Vec<(&str, &str)> = listed_identifiers
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| Some((entry.get("kind")?.as_str()?, entry.get("value")?.as_str()?)))
            .collect();
        for (index, identity) in identities.iter().enumerate() {
            if pairs.contains(&(identity.kind.as_str(), identity.value.as_str())) {
                listed[index] = true;
                let group = groups.entry(source.clone()).or_default();
                if !group.contains(identity) {
                    group.push(identity.clone());
                }
            }
        }
    }
    let observed = identities
        .iter()
        .zip(&listed)
        .filter(|(_, listed)| !**listed)
        .map(|(identity, _)| identity.clone())
        .collect();

    let traits = found
        .traits
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| {
            let source = found
                .trait_sources
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned);
            let definition = definitions.iter().find(|(known, _, _)| known == key);
            ProfileTrait {
                key: key.clone(),
                value: value.clone(),
                observed_at: source
                    .as_deref()
                    .and_then(|source| observed_at(&records, source, key, value)),
                source,
                trait_type: definition.and_then(|(_, kind, _)| kind.parse().ok()),
                pii: definition.is_none_or(|(_, _, pii)| *pii),
            }
        })
        .collect::<Vec<_>>();
    let mut traits = traits;
    traits.sort_by(|a, b| a.key.cmp(&b.key));

    Ok(Some(Profile {
        id: found.id,
        merged_from: (found.id != person).then_some(person),
        created_at: found.created_at,
        updated_at: found.updated_at,
        identity_groups: groups
            .into_iter()
            .map(|(source, identities)| IdentityGroup { source, identities })
            .collect(),
        observed,
        traits,
        merges: merges
            .into_iter()
            .map(|(merged_person_id, merged_at)| ProfileMerge {
                merged_person_id,
                merged_at,
            })
            .collect(),
    }))
}

/// When `source` observed the value a trait holds: the latest record of that source that
/// carries exactly the value, or else the latest record of the source that carries the key.
fn observed_at(
    records: &[(String, Value, Value)],
    source: &str,
    key: &str,
    value: &Value,
) -> Option<OffsetDateTime> {
    let entries = records
        .iter()
        .filter(|(record_source, _, _)| record_source == source)
        .filter_map(|(_, _, traits)| traits.get(key))
        .filter_map(|entry| {
            let at = OffsetDateTime::parse(entry.get("at")?.as_str()?, &Rfc3339).ok()?;
            Some((entry.get("v") == Some(value), at))
        });
    let mut exact = None;
    let mut any = None;
    for (same, at) in entries {
        any = any.max(Some(at));
        if same {
            exact = exact.max(Some(at));
        }
    }
    exact.or(any)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::people::Precedence;
    use zirv_flow_core::{Cause, HmacPurpose, MasterKey, Slug, TraceContext, WorkspaceName};

    use super::*;
    use crate::create_workspace;
    use crate::events::{EventOutcome, EventRecord, apply_event_batch};
    use crate::people::{
        NewTraitDefinition, ProfileOutcome, ProfileRecord, apply_profile_batch,
        create_trait_definition,
    };

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn apply(
        pool: &PgPool,
        ws: WorkspaceId,
        source: &str,
        key: &str,
        identifiers: &[(&str, &str)],
        traits: Value,
        observed_at: OffsetDateTime,
    ) -> Uuid {
        let record = ProfileRecord {
            source: source.into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: traits.as_object().cloned().unwrap(),
            observed_at,
            trace: TraceContext::root(Cause::api("test")),
        };
        match apply_profile_batch(pool, ws, &[record])
            .await
            .unwrap()
            .remove(0)
        {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_profile_groups_identities_by_source_and_shows_each_traits_provenance(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_trait_definition(
            &pool,
            ws,
            &NewTraitDefinition {
                key: "city".into(),
                trait_type: TraitType::String,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: true,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
        let t1 = datetime!(2026-01-01 00:00 UTC);
        let t2 = datetime!(2026-02-01 00:00 UTC);
        let person = apply(
            &pool,
            ws,
            "crm",
            "1",
            &[("email", "ada@example.com")],
            json!({"city": "Aarhus", "plan": "free"}),
            t1,
        )
        .await;
        apply(
            &pool,
            ws,
            "shop",
            "9",
            &[("email", "ada@example.com"), ("external:shop", "c-9")],
            json!({"plan": "pro"}),
            t2,
        )
        .await;

        let profile = load_profile(&pool, ws, person).await.unwrap().unwrap();

        assert_eq!(profile.id, person);
        assert_eq!(profile.merged_from, None);
        let sources: Vec<&str> = profile
            .identity_groups
            .iter()
            .map(|group| group.source.as_str())
            .collect();
        assert_eq!(sources, ["crm", "shop"]);
        assert_eq!(profile.identity_groups[0].identities.len(), 1);
        assert_eq!(profile.identity_groups[1].identities.len(), 2);
        assert!(profile.observed.is_empty());
        let city = profile.traits.iter().find(|t| t.key == "city").unwrap();
        assert_eq!(city.value, json!("Aarhus"));
        assert_eq!(city.source.as_deref(), Some("crm"));
        assert_eq!(city.observed_at, Some(t1));
        assert_eq!(city.trait_type, Some(TraitType::String));
        assert!(city.pii);
        let plan = profile.traits.iter().find(|t| t.key == "plan").unwrap();
        assert_eq!(plan.value, json!("pro"));
        assert_eq!(plan.source.as_deref(), Some("shop"));
        assert_eq!(plan.observed_at, Some(t2));
        assert!(!plan.pii, "an inferred trait is registered without PII");
        assert!(!format!("{plan:?}").contains("pro\""));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_no_record_lists_is_observed_and_a_merge_is_listed_on_the_survivor(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let survivor = apply(
            &pool,
            ws,
            "crm",
            "1",
            &[("external:crm", "1")],
            json!({}),
            datetime!(2026-01-01 00:00 UTC),
        )
        .await;
        let anonymous = apply(
            &pool,
            ws,
            "web",
            "2",
            &[("anonymous_id", "anon-1")],
            json!({}),
            datetime!(2026-01-01 00:00 UTC),
        )
        .await;
        // An event names an email the person had no record for: the identity is only observed.
        let event = EventRecord {
            source: "web".into(),
            identifiers: vec![
                ("external:crm".into(), "1".into()),
                ("email".into(), "ada@example.com".into()),
            ],
            name: "visit".into(),
            properties: serde_json::Map::new(),
            timestamp: None,
            received_at: OffsetDateTime::now_utc(),
            message_id: None,
            trace: TraceContext::root(Cause::api("test")),
        };
        let key = MasterKey::generate().hmac_key(HmacPurpose::Redaction);
        let outcome = apply_event_batch(&pool, ws, &key, &[event])
            .await
            .unwrap()
            .remove(0);
        assert!(matches!(outcome, EventOutcome::Accepted { .. }));
        apply(
            &pool,
            ws,
            "app",
            "3",
            &[("external:crm", "1"), ("anonymous_id", "anon-1")],
            json!({}),
            datetime!(2026-01-02 00:00 UTC),
        )
        .await;

        let through_loser = load_profile(&pool, ws, anonymous).await.unwrap().unwrap();

        assert_eq!(through_loser.id, survivor);
        assert_eq!(through_loser.merged_from, Some(anonymous));
        assert_eq!(through_loser.merges.len(), 1);
        assert_eq!(through_loser.merges[0].merged_person_id, anonymous);
        let observed: Vec<&str> = through_loser
            .observed
            .iter()
            .map(|identity| identity.kind.as_str())
            .collect();
        assert_eq!(observed, ["email"]);
        let listed: Vec<&str> = through_loser
            .identity_groups
            .iter()
            .flat_map(|group| group.identities.iter().map(|i| i.kind.as_str()))
            .collect();
        assert!(listed.contains(&"external:crm") && listed.contains(&"anonymous_id"));
        let direct = load_profile(&pool, ws, survivor).await.unwrap().unwrap();
        assert_eq!(direct.merged_from, None);
        assert_eq!(direct.merges, through_loser.merges);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspaces_person_has_no_profile(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let person = apply(
            &pool,
            one,
            "crm",
            "1",
            &[("email", "ada@example.com")],
            json!({"plan": "pro"}),
            datetime!(2026-01-01 00:00 UTC),
        )
        .await;

        assert!(load_profile(&pool, two, person).await.unwrap().is_none());
        assert!(
            load_profile(&pool, one, Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
    }
}
