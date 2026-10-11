//! People search (architecture section 12.3): by identifier, exact on the normalized value, or
//! by one trait with an operator that fits the trait's registry type. Both run under a
//! statement timeout, and neither puts a searched value in an error.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;

use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::{
    Identifier, IdentifierKind, TraitType, TraitValue, coerce, validate_key,
};

use super::bounded::begin_bounded;
use super::registry::find_trait_definition;
use crate::StoreError;

/// How a trait is compared with the searched value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraitOp {
    /// Any type. A `string_list` matches when it holds the value.
    Eq,
    /// Strings.
    Prefix,
    /// Numbers, timestamps and dates.
    Gt,
    Gte,
    Lt,
    Lte,
    /// Any type: the person has a value for the trait.
    Exists,
}

impl TraitOp {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Prefix => "prefix",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Exists => "exists",
        }
    }

    /// Whether the operator applies to `kind`.
    pub const fn allows(&self, kind: TraitType) -> bool {
        match self {
            Self::Eq | Self::Exists => true,
            Self::Prefix => matches!(kind, TraitType::String),
            Self::Gt | Self::Gte | Self::Lt | Self::Lte => matches!(
                kind,
                TraitType::Number | TraitType::Timestamp | TraitType::Date
            ),
        }
    }

    const fn comparison(&self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::Lt => "<",
            Self::Lte => "<=",
            _ => "=",
        }
    }
}

impl std::str::FromStr for TraitOp {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        match text {
            "eq" => Ok(Self::Eq),
            "prefix" => Ok(Self::Prefix),
            "gt" => Ok(Self::Gt),
            "gte" => Ok(Self::Gte),
            "lt" => Ok(Self::Lt),
            "lte" => Ok(Self::Lte),
            "exists" => Ok(Self::Exists),
            _ => Err(()),
        }
    }
}

/// What to search for.
#[derive(Clone)]
pub enum PersonSearch {
    /// `kind` and `value` as a caller wrote them; the value is normalized for the kind.
    Identifier { kind: String, value: String },
    /// `value` is absent for `exists`.
    Trait {
        key: String,
        op: TraitOp,
        value: Option<Value>,
    },
}

// The searched values are personal data: Debug shows the shape only.
impl fmt::Debug for PersonSearch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identifier { kind, .. } => write!(formatter, "Identifier({kind}, ..)"),
            Self::Trait { key, op, .. } => write!(formatter, "Trait({key}, {}, ..)", op.as_str()),
        }
    }
}

/// Why a search was refused or failed. `Invalid` names the field and the rule, never the value.
#[derive(Debug)]
pub enum SearchError {
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    /// The trait is PII-flagged and the caller may not see values, so only `eq` and `exists`
    /// are allowed.
    PiiTraitNotSearchable,
    Store(StoreError),
}

impl From<StoreError> for SearchError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { field, reason } => write!(formatter, "{field}: {reason}"),
            Self::PiiTraitNotSearchable => formatter.write_str(
                "trait.op: only eq and exists apply to a PII trait without access to values",
            ),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SearchError {}

/// A matching person.
#[derive(Clone, PartialEq)]
pub struct SearchHit {
    /// The surviving person, never a merged one.
    pub id: Uuid,
    /// The strongest identifier, as `(kind, value)`; absent for a person with none.
    pub display: Option<(String, String)>,
    pub updated_at: OffsetDateTime,
}

// The display value is personal data.
impl fmt::Debug for SearchHit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchHit")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// A page of hits by person id.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    /// Pass as `after` for the next page; absent on the last.
    pub next_after: Option<Uuid>,
}

/// The value a trait search compares with, checked against the trait's type.
enum Needle {
    None,
    /// What a person's traits must hold under the key, as JSON text.
    Json(String),
    Text(String),
}

fn invalid(field: &'static str, reason: &'static str) -> SearchError {
    SearchError::Invalid { field, reason }
}

fn needle(op: TraitOp, kind: TraitType, value: Option<&Value>) -> Result<Needle, SearchError> {
    if !op.allows(kind) {
        return Err(invalid(
            "trait.op",
            "does not apply to the type of this trait",
        ));
    }
    if op == TraitOp::Exists {
        return Ok(Needle::None);
    }
    let value = value.ok_or_else(|| invalid("trait.value", "is required for this operator"))?;
    let wrong = || invalid("trait.value", "does not fit the type of this trait");
    // A string list holds strings, so it is searched for one.
    let item_type = if kind == TraitType::StringList {
        TraitType::String
    } else {
        kind
    };
    let checked = coerce(item_type, value).map_err(|_| wrong())?;
    Ok(match (op, checked) {
        (TraitOp::Eq, checked) => {
            let json = to_json(checked);
            Needle::Json(if kind == TraitType::StringList {
                Value::Array(vec![json]).to_string()
            } else {
                json.to_string()
            })
        }
        (TraitOp::Prefix, TraitValue::String(text)) => Needle::Text(text),
        (_, TraitValue::Number(number)) => Needle::Text(number.to_string()),
        (_, TraitValue::Timestamp(text) | TraitValue::Date(text)) => Needle::Text(text),
        _ => return Err(wrong()),
    })
}

fn to_json(value: TraitValue) -> Value {
    match value {
        TraitValue::String(text) | TraitValue::Timestamp(text) | TraitValue::Date(text) => {
            Value::String(text)
        }
        TraitValue::Number(number) => Value::Number(number),
        TraitValue::Boolean(flag) => Value::Bool(flag),
        TraitValue::StringList(items) => {
            Value::Array(items.into_iter().map(Value::String).collect())
        }
    }
}

/// The SQL type an ordered comparison reads a stored trait as, and the JSON type it checks.
const fn ordered_as(kind: TraitType) -> (&'static str, &'static str) {
    match kind {
        TraitType::Number => ("numeric", "number"),
        TraitType::Timestamp => ("timestamptz", "string"),
        _ => ("date", "string"),
    }
}

/// Searches `workspace` for people. At most `limit` hits, ordered by person id and after
/// `after`; an identifier search finds at most one person. A caller who may not see values can
/// compare a PII-flagged trait only with `eq` and `exists`: the other operators would let them
/// read the value out of the order or the prefix.
pub async fn search_people(
    pool: &PgPool,
    workspace: WorkspaceId,
    search: &PersonSearch,
    limit: u32,
    after: Option<Uuid>,
    may_see_values: bool,
) -> Result<SearchPage, SearchError> {
    let limit = i64::from(limit.max(1));
    match search {
        PersonSearch::Identifier { kind, value } => {
            let identifier = Identifier::parse(kind, value)
                .map_err(|error| invalid("identifier", error.reason()))?;
            let mut tx = begin_bounded(pool).await?;
            let rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
                "SELECT s.id, s.updated_at FROM identities i \
                 JOIN people m ON m.workspace_id = i.workspace_id AND m.id = i.person_id \
                 JOIN people s ON s.workspace_id = i.workspace_id \
                               AND s.id = COALESCE(m.merged_into, m.id) \
                 WHERE i.workspace_id = $1 AND i.kind = $2 AND i.value = $3",
            )
            .bind(workspace.as_uuid())
            .bind(identifier.kind().as_text())
            .bind(identifier.value())
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::query)?;
            Ok(finish(&mut tx, workspace, rows, None).await?)
        }
        PersonSearch::Trait { key, op, value } => {
            validate_key(key).map_err(|_| invalid("trait.key", "is not a valid trait key"))?;
            let definition = find_trait_definition(pool, workspace, key)
                .await?
                .ok_or_else(|| invalid("trait.key", "is not a registered trait"))?;
            if definition.pii && !may_see_values && !matches!(op, TraitOp::Eq | TraitOp::Exists) {
                return Err(SearchError::PiiTraitNotSearchable);
            }
            let needle = needle(*op, definition.trait_type, value.as_ref())?;
            let condition = match op {
                TraitOp::Exists => "traits ? $2 AND $3::text IS NULL".to_owned(),
                TraitOp::Eq => "traits @> jsonb_build_object($2::text, $3::jsonb)".to_owned(),
                TraitOp::Prefix => "jsonb_typeof(traits -> $2) = 'string' \
                     AND starts_with(traits ->> $2, $3::text)"
                    .to_owned(),
                op => {
                    let (cast, json) = ordered_as(definition.trait_type);
                    format!(
                        "(CASE WHEN jsonb_typeof(traits -> $2) = '{json}' \
                              THEN (traits ->> $2)::{cast} END) {} $3::{cast}",
                        op.comparison(),
                    )
                }
            };
            // The condition is built from this module's constants; every value is bound.
            let sql = format!(
                "SELECT id, updated_at FROM people \
                 WHERE workspace_id = $1 AND merged_into IS NULL AND {condition} \
                   AND ($4::uuid IS NULL OR id > $4) \
                 ORDER BY id LIMIT $5"
            );
            let (json, text) = match needle {
                Needle::None => (None, None),
                Needle::Json(json) => (Some(json), None),
                Needle::Text(text) => (None, Some(text)),
            };
            let mut tx = begin_bounded(pool).await?;
            let rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(workspace.as_uuid())
                .bind(key)
                .bind(json.or(text))
                .bind(after)
                .bind(limit + 1)
                .fetch_all(&mut *tx)
                .await
                .map_err(StoreError::query)?;
            Ok(finish(&mut tx, workspace, rows, Some(limit)).await?)
        }
    }
}

/// Cuts the extra row that says there is more, and adds each person's display identifier.
async fn finish(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    mut rows: Vec<(Uuid, OffsetDateTime)>,
    limit: Option<i64>,
) -> Result<SearchPage, StoreError> {
    let mut next_after = None;
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if rows.len() > limit {
            rows.truncate(limit);
            next_after = rows.last().map(|(id, _)| *id);
        }
    }
    let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
    let mut display = strongest_identifiers(connection, workspace, &ids).await?;
    Ok(SearchPage {
        hits: rows
            .into_iter()
            .map(|(id, updated_at)| SearchHit {
                id,
                display: display.remove(&id),
                updated_at,
            })
            .collect(),
        next_after,
    })
}

/// The strongest identifier of each person in `people`, by kind strength and then by kind and
/// value so the choice is stable.
pub(crate) async fn strongest_identifiers(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<HashMap<Uuid, (String, String)>, StoreError> {
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT person_id, kind, value FROM identities \
         WHERE workspace_id = $1 AND person_id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let strength = |kind: &str| IdentifierKind::parse(kind).map_or(0, |kind| kind.strength());
    let mut best: HashMap<Uuid, (String, String)> = HashMap::new();
    for (person, kind, value) in rows {
        let better = best.get(&person).is_none_or(|(held_kind, held_value)| {
            (strength(&kind), Reverse((&kind, &value)))
                > (strength(held_kind), Reverse((held_kind, held_value)))
        });
        if better {
            best.insert(person, (kind, value));
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::people::Precedence;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};

    use super::*;
    use crate::create_workspace;
    use crate::people::{
        NewTraitDefinition, ProfileOutcome, ProfileRecord, apply_profile_batch,
        create_trait_definition,
    };

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn define(pool: &PgPool, ws: WorkspaceId, key: &str, trait_type: TraitType) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.into(),
                trait_type,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    async fn person(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        identifiers: &[(&str, &str)],
        traits: Value,
    ) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: traits.as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
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

    fn by_trait(key: &str, op: TraitOp, value: Option<Value>) -> PersonSearch {
        PersonSearch::Trait {
            key: key.into(),
            op,
            value,
        }
    }

    async fn ids(pool: &PgPool, ws: WorkspaceId, search: PersonSearch) -> Vec<Uuid> {
        let mut found: Vec<Uuid> = search_people(pool, ws, &search, 50, None, true)
            .await
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        found.sort();
        found
    }

    fn sorted(mut people: Vec<Uuid>) -> Vec<Uuid> {
        people.sort();
        people
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identifier_search_is_exact_on_the_normalized_value_and_follows_merges(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let survivor = person(&pool, ws, "1", &[("email", "ada@example.com")], json!({})).await;
        let anonymous = person(&pool, ws, "2", &[("anonymous_id", "anon-1")], json!({})).await;
        let merged = person(
            &pool,
            ws,
            "3",
            &[("email", "ada@example.com"), ("anonymous_id", "anon-1")],
            json!({}),
        )
        .await;
        assert_eq!(merged, survivor);
        assert_ne!(anonymous, survivor);
        let find = |kind: &str, value: &str| {
            let search = PersonSearch::Identifier {
                kind: kind.into(),
                value: value.into(),
            };
            let pool = pool.clone();
            async move { search_people(&pool, ws, &search, 10, None, true).await }
        };

        let by_email = find("email", " ADA@Example.com ").await.unwrap();
        let by_merged = find("anonymous_id", "anon-1").await.unwrap();
        let missing = find("email", "nobody@example.com").await.unwrap();
        let bad = find("email", "not an email").await.unwrap_err();
        let kind = find("fax", "1").await.unwrap_err();

        assert_eq!(by_email.hits.len(), 1);
        assert_eq!(by_email.hits[0].id, survivor);
        assert_eq!(by_merged.hits[0].id, survivor);
        assert_eq!(
            by_email.hits[0].display,
            Some(("email".into(), "ada@example.com".into()))
        );
        assert!(missing.hits.is_empty() && missing.next_after.is_none());
        assert!(matches!(
            bad,
            SearchError::Invalid {
                field: "identifier",
                ..
            }
        ));
        assert!(!bad.to_string().contains("not an email"));
        assert!(matches!(
            kind,
            SearchError::Invalid {
                field: "identifier",
                ..
            }
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_display_identifier_is_the_strongest_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let one = person(
            &pool,
            ws,
            "1",
            &[
                ("anonymous_id", "anon-1"),
                ("phone", "+4512345678"),
                ("email", "ada@example.com"),
            ],
            json!({}),
        )
        .await;

        let page = search_people(
            &pool,
            ws,
            &PersonSearch::Identifier {
                kind: "phone".into(),
                value: "+4512345678".into(),
            },
            10,
            None,
            true,
        )
        .await
        .unwrap();

        assert_eq!(page.hits[0].id, one);
        assert_eq!(
            page.hits[0].display,
            Some(("email".into(), "ada@example.com".into()))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_trait_operator_works_for_its_types(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        for (key, kind) in [
            ("plan", TraitType::String),
            ("age", TraitType::Number),
            ("joined", TraitType::Date),
            ("seen", TraitType::Timestamp),
            ("tags", TraitType::StringList),
            ("vip", TraitType::Boolean),
        ] {
            define(&pool, ws, key, kind).await;
        }
        let ada = person(
            &pool,
            ws,
            "1",
            &[("email", "ada@example.com")],
            json!({"plan": "pro-annual", "age": 36, "joined": "2024-02-29",
                   "seen": "2026-01-05T10:00:00Z", "tags": ["a", "b"], "vip": true}),
        )
        .await;
        let bo = person(
            &pool,
            ws,
            "2",
            &[("email", "bo@example.com")],
            json!({"plan": "free", "age": 9.5, "joined": "2025-06-01",
                   "seen": "2026-02-01T08:30:00+02:00", "tags": ["b"], "vip": false}),
        )
        .await;
        let cy = person(&pool, ws, "3", &[("email", "cy@example.com")], json!({})).await;

        let eq = |key, value| by_trait(key, TraitOp::Eq, Some(value));
        assert_eq!(ids(&pool, ws, eq("plan", json!("free"))).await, [bo]);
        assert_eq!(ids(&pool, ws, eq("age", json!(36.0))).await, [ada]);
        assert_eq!(ids(&pool, ws, eq("vip", json!(false))).await, [bo]);
        assert_eq!(
            ids(&pool, ws, eq("joined", json!("2024-02-29"))).await,
            [ada]
        );
        assert_eq!(
            ids(&pool, ws, eq("seen", json!("2026-02-01T06:30:00Z"))).await,
            [bo]
        );
        assert_eq!(
            ids(&pool, ws, eq("tags", json!("b"))).await,
            sorted(vec![ada, bo])
        );
        assert_eq!(ids(&pool, ws, eq("tags", json!("a"))).await, [ada]);
        assert_eq!(
            ids(
                &pool,
                ws,
                by_trait("plan", TraitOp::Prefix, Some(json!("pro")))
            )
            .await,
            [ada]
        );
        assert!(
            ids(
                &pool,
                ws,
                by_trait("plan", TraitOp::Prefix, Some(json!("Pro")))
            )
            .await
            .is_empty()
        );
        let range = |key, op, value| by_trait(key, op, Some(value));
        assert_eq!(
            ids(&pool, ws, range("age", TraitOp::Gt, json!(9.5))).await,
            [ada]
        );
        assert_eq!(
            ids(&pool, ws, range("age", TraitOp::Gte, json!(9.5))).await,
            sorted(vec![ada, bo])
        );
        assert_eq!(
            ids(&pool, ws, range("age", TraitOp::Lt, json!(10))).await,
            [bo]
        );
        assert_eq!(
            ids(&pool, ws, range("age", TraitOp::Lte, json!(36)))
                .await
                .len(),
            2
        );
        assert_eq!(
            ids(&pool, ws, range("joined", TraitOp::Lt, json!("2025-01-01"))).await,
            [ada]
        );
        assert_eq!(
            ids(
                &pool,
                ws,
                range("seen", TraitOp::Gte, json!("2026-02-01T00:00:00Z"))
            )
            .await,
            [bo]
        );
        assert_eq!(
            ids(&pool, ws, by_trait("age", TraitOp::Exists, None)).await,
            sorted(vec![ada, bo])
        );
        assert!(
            !ids(&pool, ws, by_trait("age", TraitOp::Exists, None))
                .await
                .contains(&cy)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_operator_that_does_not_fit_the_type_or_a_bad_value_is_refused_without_the_value(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        define(&pool, ws, "plan", TraitType::String).await;
        define(&pool, ws, "age", TraitType::Number).await;
        define(&pool, ws, "vip", TraitType::Boolean).await;
        let refused = |search| {
            let pool = pool.clone();
            async move {
                search_people(&pool, ws, &search, 10, None, true)
                    .await
                    .unwrap_err()
            }
        };

        let cases = [
            (
                by_trait("age", TraitOp::Prefix, Some(json!("3"))),
                "trait.op",
            ),
            (by_trait("plan", TraitOp::Gt, Some(json!("a"))), "trait.op"),
            (by_trait("vip", TraitOp::Lt, Some(json!(true))), "trait.op"),
            (
                by_trait("age", TraitOp::Eq, Some(json!("secret-text"))),
                "trait.value",
            ),
            (by_trait("age", TraitOp::Gt, None), "trait.value"),
            (by_trait("nope", TraitOp::Exists, None), "trait.key"),
            (by_trait("Bad Key", TraitOp::Exists, None), "trait.key"),
        ];
        for (search, field) in cases {
            let error = refused(search).await;
            assert!(
                matches!(&error, SearchError::Invalid { field: found, .. } if *found == field),
                "{error}"
            );
            assert!(!error.to_string().contains("secret-text"));
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_trait_search_pages_by_person_id_and_skips_merged_people(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        define(&pool, ws, "plan", TraitType::String).await;
        let mut everyone = Vec::new();
        for n in 0..5 {
            let email = format!("p{n}@example.com");
            everyone.push(
                person(
                    &pool,
                    ws,
                    &n.to_string(),
                    &[("email", &email)],
                    json!({"plan": "pro"}),
                )
                .await,
            );
        }
        let search = by_trait("plan", TraitOp::Eq, Some(json!("pro")));

        let mut seen = Vec::new();
        let mut after = None;
        let mut pages = 0;
        loop {
            let page = search_people(&pool, ws, &search, 2, after, true)
                .await
                .unwrap();
            assert!(page.hits.len() <= 2);
            seen.extend(page.hits.iter().map(|hit| hit.id));
            pages += 1;
            match page.next_after {
                Some(next) => after = Some(next),
                None => break,
            }
        }

        assert_eq!(pages, 3);
        assert_eq!(seen, sorted(everyone));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_search_finds_only_its_own_workspaces_people(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        for ws in [one, two] {
            define(&pool, ws, "plan", TraitType::String).await;
            person(
                &pool,
                ws,
                "1",
                &[("email", "a@example.com")],
                json!({"plan": "pro"}),
            )
            .await;
        }
        let by_identifier = PersonSearch::Identifier {
            kind: "email".into(),
            value: "a@example.com".into(),
        };

        for ws in [one, two] {
            let hits = search_people(&pool, ws, &by_identifier, 10, None, true)
                .await
                .unwrap()
                .hits;
            let traits = ids(&pool, ws, by_trait("plan", TraitOp::Exists, None)).await;
            assert_eq!(hits.len(), 1);
            assert_eq!(traits, [hits[0].id]);
        }
        assert_ne!(
            ids(&pool, one, by_trait("plan", TraitOp::Exists, None)).await,
            ids(&pool, two, by_trait("plan", TraitOp::Exists, None)).await
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pii_trait_without_access_to_values_allows_only_eq_and_exists(pool: PgPool) {
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
        let aarhus = person(
            &pool,
            ws,
            "1",
            &[("email", "a@example.com")],
            json!({"city": "Aarhus"}),
        )
        .await;
        let run = |op, value: Option<Value>, access| {
            let pool = pool.clone();
            let search = by_trait("city", op, value);
            async move { search_people(&pool, ws, &search, 10, None, access).await }
        };

        for op in [
            TraitOp::Prefix,
            TraitOp::Gt,
            TraitOp::Gte,
            TraitOp::Lt,
            TraitOp::Lte,
        ] {
            let refused = run(op, Some(json!("A")), false).await.unwrap_err();
            assert!(
                matches!(refused, SearchError::PiiTraitNotSearchable),
                "{op:?}"
            );
        }
        let eq = run(TraitOp::Eq, Some(json!("Aarhus")), false)
            .await
            .unwrap();
        let exists = run(TraitOp::Exists, None, false).await.unwrap();
        let prefix = run(TraitOp::Prefix, Some(json!("Aa")), true).await.unwrap();
        assert_eq!(eq.hits[0].id, aarhus);
        assert_eq!(exists.hits[0].id, aarhus);
        assert_eq!(prefix.hits[0].id, aarhus);
        assert!(run(TraitOp::Gt, Some(json!("A")), true).await.is_err());
    }

    #[test]
    fn the_debug_of_a_search_and_a_hit_shows_no_value() {
        let search = PersonSearch::Identifier {
            kind: "email".into(),
            value: "ada@example.com".into(),
        };
        let trait_search = by_trait("plan", TraitOp::Eq, Some(json!("secret-plan")));
        let hit = SearchHit {
            id: Uuid::nil(),
            display: Some(("email".into(), "ada@example.com".into())),
            updated_at: datetime!(2026-01-01 00:00 UTC),
        };

        for text in [
            format!("{search:?}"),
            format!("{trait_search:?}"),
            format!("{hit:?}"),
        ] {
            assert!(
                !text.contains("ada@example.com") && !text.contains("secret-plan"),
                "{text}"
            );
        }
    }

    #[test]
    fn operators_apply_to_the_types_the_spec_names() {
        assert!(TraitOp::Prefix.allows(TraitType::String));
        assert!(!TraitOp::Prefix.allows(TraitType::StringList));
        for kind in [TraitType::Number, TraitType::Timestamp, TraitType::Date] {
            assert!(TraitOp::Gte.allows(kind));
        }
        for kind in [TraitType::String, TraitType::Boolean, TraitType::StringList] {
            assert!(!TraitOp::Lt.allows(kind));
            assert!(TraitOp::Eq.allows(kind) && TraitOp::Exists.allows(kind));
        }
        assert_eq!("gte".parse::<TraitOp>(), Ok(TraitOp::Gte));
        assert!("like".parse::<TraitOp>().is_err());
    }
}
