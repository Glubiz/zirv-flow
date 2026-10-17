//! The compiled SQL checked against the reference evaluator on PostgreSQL (architecture
//! sections 8.2 and 18; rulings 3724f651, 887b9eef, e6659f67 and 0c6404db).
//!
//! The generators and the golden SQL live in `zirv-flow-segments`; the comparison lives here
//! because only this crate sees both the compiler and the store. Every test needs PostgreSQL and
//! is ignored without it. CI runs a fixed seed and 64 cases; the nightly run sets
//! `ZIRV_FLOW_PROPTEST_SEED` at random and `ZIRV_FLOW_PROPTEST_CASES` to 1,000 or more.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use sqlx::{PgConnection, PgPool};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use uuid::Uuid;
    use zirv_flow_core::events::{EventMode, SchemaProperty};
    use zirv_flow_core::people::Precedence;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};
    use zirv_flow_segments::generate::{self, NOW};
    use zirv_flow_segments::{
        Catalog, Definition, Outcome, PersonSnapshot, SnapshotEvent, TraitStorage, compile,
        evaluate, normalize,
    };
    use zirv_flow_store::{
        NewEventSchema, NewSegment, NewSegmentVersion, NewTraitDefinition, SqlParam, SqlQuery,
        begin, commit, create_event_schema, create_segment, create_trait_definition,
        create_workspace, lock_segments, migrate, publish_schema_draft, run_segment_ids,
        segment_catalog,
    };

    use crate::kinds::segment_refresh::sql_query;

    /// A migrated workspace that defines exactly what [`generate::catalog`] names, with the
    /// same ids for its lists, segments and topics. The traits and the event schemas go in
    /// through the store's own write paths, and the catalog the API would read back is checked
    /// against the generator's.
    async fn world(pool: &PgPool) -> (WorkspaceId, Catalog) {
        migrate(pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = create_workspace(pool, &name, &slug).await.unwrap().id;
        let catalog = generate::catalog();
        for (key, kind) in &catalog.traits {
            create_trait_definition(
                pool,
                ws,
                &NewTraitDefinition {
                    key: key.clone(),
                    trait_type: *kind,
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
        for (name, schema) in &catalog.events {
            let properties: Vec<SchemaProperty> = serde_json::from_value(Value::Array(
                schema
                    .iter()
                    .map(|(key, kind)| json!({"name": key, "type": kind.as_str()}))
                    .collect(),
            ))
            .unwrap();
            create_event_schema(
                pool,
                ws,
                &NewEventSchema {
                    name: name.clone(),
                    mode: EventMode::Warn,
                    description: String::new(),
                    properties,
                },
            )
            .await
            .unwrap();
            let mut connection = pool.acquire().await.unwrap();
            publish_schema_draft(&mut connection, ws, name, None)
                .await
                .unwrap();
        }
        // The ids are the generator's, which the write paths would not let a caller choose.
        for id in &catalog.lists {
            sqlx::query("INSERT INTO lists (id, workspace_id, name) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(ws.as_uuid())
                .bind(format!("list {id}"))
                .execute(pool)
                .await
                .unwrap();
        }
        for id in &catalog.segments {
            insert_segment(pool, ws, *id).await;
        }
        for (key, id) in &catalog.topics[&zirv_flow_core::consent::Channel::Email] {
            sqlx::query(
                "INSERT INTO topics (id, workspace_id, key, name, channel, policy) \
                 VALUES ($1, $2, $3, $3, 'email', 'opt_in')",
            )
            .bind(id)
            .bind(ws.as_uuid())
            .bind(key)
            .execute(pool)
            .await
            .unwrap();
        }
        let mut connection = pool.acquire().await.unwrap();
        let stored = segment_catalog(&mut connection, ws).await.unwrap();
        assert_eq!(stored.traits, catalog.traits);
        assert_eq!(stored.events, catalog.events);
        assert_eq!(stored.lists, catalog.lists);
        assert_eq!(stored.segments, catalog.segments);
        assert_eq!(stored.topics, catalog.topics);
        (ws, catalog)
    }

    /// A segment with a chosen id, through the store's own create path.
    async fn insert_segment(pool: &PgPool, ws: WorkspaceId, id: Uuid) {
        let definition = json!({"all": [], "v": 1});
        let parsed = Definition::from_value(&definition).unwrap();
        let compiled =
            compile(&parsed, &Catalog::default(), &TraitStorage::default(), NOW).unwrap();
        let content = NewSegmentVersion {
            definition,
            normalized: normalize(parsed).to_value(),
            deps: compiled.deps.to_value(),
            time_sensitive: compiled.time_sensitive,
        };
        let mut tx = begin(pool).await.unwrap();
        lock_segments(&mut tx, ws).await.unwrap();
        let new = NewSegment {
            id,
            name: format!("segment {id}"),
            description: String::new(),
            cadence_minutes: 60,
        };
        let trace = TraceContext::root(Cause::api("test"));
        create_segment(&mut tx, ws, &new, &content, None, &trace)
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    fn at(instant: OffsetDateTime) -> String {
        instant.format(&Rfc3339).unwrap()
    }

    /// Writes the people with their traits, events, lists, consent, identifiers and open segment
    /// memberships. Raw SQL into the store's tables, in one statement per table: the write
    /// paths refuse or rewrite exactly the values the comparison needs, such as a time string in
    /// the wrong shape or a fractional integer, and the SQL must read them as stored.
    async fn seed(
        connection: &mut PgConnection,
        ws: WorkspaceId,
        people: &[PersonSnapshot],
    ) -> Vec<Uuid> {
        let ids: Vec<Uuid> = people.iter().map(|_| Uuid::now_v7()).collect();
        let mut rows = [const { Vec::<Value>::new() }; 6];
        for (id, person) in ids.iter().zip(people) {
            rows[0].push(json!({"id": id, "traits": person.traits}));
            for event in &person.events {
                rows[1].push(json!({
                    "person": id,
                    "name": event.name,
                    "at": at(event.occurred_at),
                    "properties": event.properties,
                }));
            }
            for list in &person.lists {
                rows[2].push(json!({"person": id, "ref": list}));
            }
            for row in &person.consent {
                rows[3].push(json!({
                    "person": id,
                    "topic": row.topic,
                    "status": row.status.as_str(),
                    "explicit": row.status.is_explicit(),
                }));
            }
            for kind in &person.identifier_kinds {
                rows[4].push(json!({"person": id, "kind": kind}));
            }
            for segment in &person.segments {
                rows[5].push(json!({"person": id, "ref": segment}));
            }
        }
        let statements = [
            "INSERT INTO people (id, workspace_id, traits) \
             SELECT (r->>'id')::uuid, $1, r->'traits' FROM jsonb_array_elements($2::jsonb) r",
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                                 properties, source, trace_id, validation_state) \
             SELECT gen_random_uuid(), $1, (r->>'person')::uuid, r->>'name', \
                    (r->>'at')::timestamptz, now(), r->'properties', 'test', \
                    gen_random_uuid(), 'valid' \
             FROM jsonb_array_elements($2::jsonb) r",
            "INSERT INTO list_members (workspace_id, list_id, person_id, source) \
             SELECT $1, (r->>'ref')::uuid, (r->>'person')::uuid, 'api' \
             FROM jsonb_array_elements($2::jsonb) r",
            "INSERT INTO consent (workspace_id, person_id, channel, topic_id, status, explicit, \
                                  changed_at) \
             SELECT $1, (r->>'person')::uuid, 'email', (r->>'topic')::uuid, r->>'status', \
                    (r->>'explicit')::boolean, now() \
             FROM jsonb_array_elements($2::jsonb) r",
            "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
             SELECT gen_random_uuid(), $1, (r->>'person')::uuid, r->>'kind', r->>'person' \
             FROM jsonb_array_elements($2::jsonb) r",
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                                          entered_at, entered_trace_id, entered_cause) \
             SELECT $1, (r->>'ref')::uuid, (r->>'person')::uuid, 1, now(), gen_random_uuid(), \
                    'initial' \
             FROM jsonb_array_elements($2::jsonb) r",
        ];
        for (statement, rows) in statements.into_iter().zip(rows) {
            if rows.is_empty() {
                continue;
            }
            sqlx::query(statement)
                .bind(ws.as_uuid())
                .bind(Value::Array(rows))
                .execute(&mut *connection)
                .await
                .unwrap();
        }
        ids
    }

    fn person_json(person: &PersonSnapshot) -> Value {
        let events: Vec<Value> = person
            .events
            .iter()
            .map(
                |SnapshotEvent {
                     name,
                     occurred_at,
                     properties,
                 }| {
                    json!({"name": name, "at": at(*occurred_at), "properties": properties})
                },
            )
            .collect();
        json!({
            "traits": person.traits,
            "events": events,
            "lists": person.lists,
            "consent": person.consent.iter().map(|row| {
                json!({"topic": row.topic, "status": row.status.as_str()})
            }).collect::<Vec<_>>(),
            "identifier_kinds": person.identifier_kinds,
            "segments": person.segments,
        })
    }

    /// Compiles the definition at [`NOW`], runs `members_sql` and `batch_sql` over the seeded
    /// people, and checks both against the evaluator. A mismatch is described in full.
    async fn compare(
        connection: &mut PgConnection,
        ws: WorkspaceId,
        catalog: &Catalog,
        definition: &Definition,
        people: &[PersonSnapshot],
        ids: &[Uuid],
    ) -> Result<(usize, usize), String> {
        let compiled = compile(definition, catalog, &TraitStorage::default(), NOW)
            .map_err(|issues| format!("does not compile: {issues:?}"))?;
        let passes: Vec<bool> = people
            .iter()
            .map(
                |person| match evaluate(definition, catalog, person, NOW).outcome {
                    Outcome::Pass => true,
                    Outcome::Fail => false,
                    Outcome::Unknown => panic!("nothing is capped, so nothing can be unknown"),
                },
            )
            .collect();
        let expected: BTreeSet<Uuid> = ids
            .iter()
            .zip(&passes)
            .filter_map(|(id, pass)| pass.then_some(*id))
            .collect();

        let members_query = sql_query(compiled.members_sql());
        let members: BTreeSet<Uuid> = run_segment_ids(connection, ws, &members_query)
            .await
            .map_err(|error| format!("members_sql failed: {error:?}\n{}", members_query.sql))?
            .into_iter()
            .collect();

        // A batch of every other person, and a person who does not exist.
        let batch: Vec<Uuid> = ids
            .iter()
            .step_by(2)
            .copied()
            .chain([Uuid::nil()])
            .collect();
        let mut batch_query: SqlQuery = sql_query(compiled.batch_sql());
        batch_query.params.push(SqlParam::UuidArray(batch.clone()));
        let in_batch: BTreeSet<Uuid> = run_segment_ids(connection, ws, &batch_query)
            .await
            .map_err(|error| format!("batch_sql failed: {error:?}\n{}", batch_query.sql))?
            .into_iter()
            .collect();
        let batch_expected: BTreeSet<Uuid> = batch
            .iter()
            .copied()
            .filter(|id| expected.contains(id))
            .collect();

        if members == expected && in_batch == batch_expected {
            return Ok((expected.len(), people.len() - expected.len()));
        }
        let mut report = format!(
            "definition: {}\nsql: {}\n",
            definition.to_value(),
            members_query.sql
        );
        for (position, (id, person)) in ids.iter().zip(people).enumerate() {
            let (sql, reference) = (members.contains(id), expected.contains(id));
            let in_sql_batch = in_batch.contains(id);
            let batched = batch.contains(id);
            if sql != reference || (batched && in_sql_batch != reference) {
                report.push_str(&format!(
                    "person {position} ({id}): members_sql {sql}, batch_sql {in_sql_batch} \
                     (in the batch: {batched}), evaluator {reference}\n  {}\n  {:?}\n",
                    person_json(person),
                    evaluate(definition, catalog, person, NOW).tree,
                ));
            }
        }
        Err(report)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sql_and_evaluator_comparison_agrees_on_generated_cases(pool: PgPool) {
        let (ws, catalog) = world(&pool).await;
        let (rng_seed, cases) = (generate::seed(), generate::cases());
        println!("comparison: seed {rng_seed}, {cases} cases");
        let generated = generate::sample(&generate::case(), &mut generate::runner(), cases);
        let (mut members, mut others, mut failures) = (0, 0, Vec::new());
        for (number, case) in generated.iter().enumerate() {
            let mut tx = pool.begin().await.unwrap();
            let ids = seed(&mut tx, ws, &case.people).await;
            match compare(&mut tx, ws, &catalog, &case.definition, &case.people, &ids).await {
                Ok((passed, failed)) => {
                    members += passed;
                    others += failed;
                }
                Err(report) => failures.push(format!("case {number}: {report}")),
            }
            tx.rollback().await.unwrap();
            if failures.len() >= 3 {
                break;
            }
        }
        assert!(
            failures.is_empty(),
            "SQL and evaluator disagree (seed {rng_seed}, {cases} cases; rerun with \
             {}={rng_seed} {}={cases}):\n{}",
            generate::SEED_VAR,
            generate::CASES_VAR,
            failures.join("\n")
        );
        println!("comparison: {members} members and {others} non-members agreed");
        assert!(members > 0 && others > 0, "the cases were one-sided");
    }

    fn definition(value: Value) -> Definition {
        let mut value = value;
        value["v"] = json!(1);
        Definition::from_value(&value).unwrap()
    }

    /// One person holding `value` for each of the time-valued traits and event properties.
    fn holding(value: &Value) -> PersonSnapshot {
        let mut person = PersonSnapshot::default();
        for key in ["signed_up", "renewed", "birthday"] {
            person.traits.insert(key.to_owned(), value.clone());
        }
        let properties = json!({"shipped_at": value, "due": value})
            .as_object()
            .cloned();
        person.events.push(SnapshotEvent {
            name: "order".into(),
            occurred_at: NOW,
            properties: properties.unwrap_or_default(),
        });
        person.events.push(SnapshotEvent {
            name: "visit".into(),
            occurred_at: NOW,
            properties: json!({"seen_at": value})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        });
        person
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn comparison_of_each_time_disagreement_case_in_trait_and_property_position(
        pool: PgPool,
    ) {
        let (ws, catalog) = world(&pool).await;
        let mut values: Vec<Value> = generate::TIMESTAMP_ODDITIES
            .iter()
            .chain(generate::DATE_ODDITIES.iter())
            .map(|text| json!(text))
            .collect();
        values.extend([
            json!("2025-06-15T12:00:00Z"),
            json!("2025-06-15t12:00:00z"),
            json!("2025-06-15 12:00:00+02:00"),
            json!("2025-06-15T12:00:00.000Z"),
            json!("1999-12-31T23:59:59Z"),
            json!("1969-12-31T23:59:59Z"),
            json!("2025-06-15T12:00:00-15:00"),
            json!("2025-06-15T12:00:00+15:00"),
            json!("2025-06-15T00:00:00+15:59"),
            json!("2025-06-15"),
            json!("2024-02-29"),
            json!("0001-01-01"),
            json!("9999-12-31"),
            json!("0001-01-01T00:00:00Z"),
            json!("9999-12-31T23:59:59Z"),
            json!("9999-12-31T23:59:59-15:00"),
            json!(5),
            json!(null),
            json!([]),
            json!({}),
        ]);
        let people: Vec<PersonSnapshot> = values.iter().map(holding).collect();
        let late = "9999-12-31T23:59:59Z";
        let early = "0001-01-01T00:00:00Z";
        let mut definitions = Vec::new();
        for (kind, trait_key, prop_event, prop) in [
            ("timestamp", "signed_up", "order", "shipped_at"),
            ("timestamp", "renewed", "visit", "seen_at"),
            ("date", "birthday", "order", "due"),
        ] {
            let (low, high) = if kind == "date" {
                ("0001-01-01", "9999-12-31")
            } else {
                (early, late)
            };
            let day = if kind == "date" {
                "2025-06-15"
            } else {
                "2025-06-15T12:00:00Z"
            };
            let next = if kind == "date" {
                "2025-06-16"
            } else {
                "2025-06-15T12:00:01Z"
            };
            for (op, value) in [
                ("before", json!(high)),
                ("after", json!(low)),
                ("before", json!(day)),
                ("after", json!(day)),
                ("between", json!([low, high])),
                ("between", json!([day, next])),
                ("within_last", json!("P30D")),
                ("more_than_ago", json!("PT1H")),
                ("within_next", json!("P7D")),
                ("exists", Value::Null),
                ("not_exists", Value::Null),
            ] {
                let mut leaf = json!({"op": op});
                if !value.is_null() {
                    leaf["value"] = value.clone();
                }
                let mut on_trait = leaf.clone();
                on_trait["trait"] = json!(trait_key);
                definitions.push(on_trait);
                let mut filter = leaf;
                filter["prop"] = json!(prop);
                definitions.push(json!({"event": prop_event, "where": [filter]}));
            }
            // The bound of a window is a stored trait, read the same way.
            definitions.push(json!({"event": "order", "window": {"since_trait": trait_key}}));
            definitions.push(
                json!({"event": "order", "window": {"since_trait": trait_key},
                "agg": {"fn": "count", "op": "eq", "value": 0}}),
            );
            definitions
                .push(json!({"not": {"event": "visit", "window": {"since_trait": trait_key}}}));
        }
        // Bounds finer than a microsecond: the driver rounds them toward zero, which is up
        // before 2000, so the lowering binds the floored instant and the evaluator reads it.
        for (stored_at, bound) in [
            ("signed_up", "1999-12-31T23:59:59.0000005Z"),
            ("signed_up", "1969-12-31T23:59:59.9999995Z"),
            ("signed_up", "2025-06-15T12:00:00.0000005Z"),
        ] {
            for op in ["before", "after"] {
                definitions.push(json!({"trait": stored_at, "op": op, "value": bound}));
            }
            definitions.push(json!({"trait": stored_at, "op": "between", "value": [bound, "2100-01-01T00:00:00Z"]}));
        }
        let mut tx = pool.begin().await.unwrap();
        let ids = seed(&mut tx, ws, &people).await;
        let mut failures = Vec::new();
        let mut checked = 0;
        for value in definitions {
            let definition = definition(value);
            checked += 1;
            if let Err(report) = compare(&mut tx, ws, &catalog, &definition, &people, &ids).await {
                failures.push(report);
            }
        }
        tx.rollback().await.unwrap();
        assert!(
            failures.is_empty(),
            "{} of {checked} definitions disagree:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    fn param(tagged: &Value) -> SqlParam {
        let (tag, value) = tagged
            .as_object()
            .and_then(|map| map.iter().next())
            .expect("a param is a tagged value");
        let text = |value: &Value| value.as_str().expect("a string").to_owned();
        let uuid = |value: &Value| Uuid::parse_str(value.as_str().expect("a string")).unwrap();
        let items = || value.as_array().expect("an array");
        match tag.as_str() {
            "text" => SqlParam::Text(text(value)),
            "text_array" => SqlParam::TextArray(items().iter().map(text).collect()),
            "float" => SqlParam::Float(value.as_f64().expect("a number")),
            "float_array" => SqlParam::FloatArray(
                items()
                    .iter()
                    .map(|item| item.as_f64().expect("a number"))
                    .collect(),
            ),
            "int" => SqlParam::Int(value.as_i64().expect("an integer")),
            "timestamp" => SqlParam::Timestamp(
                OffsetDateTime::parse(value.as_str().expect("a string"), &Rfc3339).unwrap(),
            ),
            "uuid" => SqlParam::Uuid(uuid(value)),
            "uuid_array" => SqlParam::UuidArray(items().iter().map(uuid).collect()),
            "bool" => SqlParam::Bool(value.as_bool().expect("a boolean")),
            "jsonb" => SqlParam::Jsonb(value.clone()),
            other => panic!("unknown param tag {other}"),
        }
    }

    fn fixtures() -> Vec<(String, Value)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../zirv-flow-segments/tests/fixtures/golden");
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .expect("the golden directory should exist")
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (
                    name,
                    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap(),
                )
            })
            .collect()
    }

    fn golden_query(fixture: &Value) -> SqlQuery {
        SqlQuery {
            sql: fixture["sql"]
                .as_str()
                .expect("the fixture has its SQL")
                .to_owned(),
            params: fixture["params"]
                .as_array()
                .expect("the fixture has its params")
                .iter()
                .map(param)
                .collect(),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn golden_fixtures_execute_on_postgres_and_constants_select_everyone_or_no_one(
        pool: PgPool,
    ) {
        let (ws, _) = world(&pool).await;
        let people = vec![
            holding(&json!("2025-06-15T12:00:00Z")),
            holding(&json!(null)),
            PersonSnapshot::default(),
        ];
        let mut connection = pool.acquire().await.unwrap();
        let ids: BTreeSet<Uuid> = seed(&mut connection, ws, &people)
            .await
            .into_iter()
            .collect();
        let mut executed = 0;
        for (name, fixture) in fixtures() {
            let query = golden_query(&fixture);
            let found = run_segment_ids(&mut connection, ws, &query)
                .await
                .unwrap_or_else(|error| panic!("{name} does not run: {error:?}\n{}", query.sql));
            let found: BTreeSet<Uuid> = found.into_iter().collect();
            assert!(found.is_subset(&ids), "{name} returned someone unknown");
            if name.contains("constant-true") {
                assert_eq!(found, ids, "{name}");
            }
            if name.contains("constant-false") {
                assert!(found.is_empty(), "{name}");
            }
            executed += 1;
        }
        assert!(executed >= 40, "only {executed} fixtures ran");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn golden_segment_fixtures_read_the_open_rows_of_segment_members(pool: PgPool) {
        let (ws, _) = world(&pool).await;
        let fixture = fixtures()
            .into_iter()
            .find(|(name, _)| name.starts_with("051-"))
            .expect("the segment fixture exists")
            .1;
        let query = golden_query(&fixture);
        let SqlParam::Uuid(segment) = query.params[0] else {
            panic!("the segment fixture binds the segment id");
        };
        insert_segment(&pool, ws, segment).await;
        let people = vec![
            PersonSnapshot::default(),
            PersonSnapshot::default(),
            PersonSnapshot::default(),
        ];
        let mut connection = pool.acquire().await.unwrap();
        let ids = seed(&mut connection, ws, &people).await;
        for (person, closed) in [(ids[0], false), (ids[1], true)] {
            sqlx::query(
                "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                                              entered_at, entered_trace_id, entered_cause, \
                                              exited_version, exited_at, exited_trace_id, \
                                              exited_cause) \
                 VALUES ($1, $2, $3, 1, now(), gen_random_uuid(), 'initial', \
                         CASE WHEN $4 THEN 1 END, CASE WHEN $4 THEN now() END, \
                         CASE WHEN $4 THEN gen_random_uuid() END, CASE WHEN $4 THEN 'data_change' END)",
            )
            .bind(ws.as_uuid())
            .bind(segment)
            .bind(person)
            .bind(closed)
            .execute(&mut *connection)
            .await
            .unwrap();
        }
        let members = run_segment_ids(&mut connection, ws, &query).await.unwrap();
        assert_eq!(members, [ids[0]]);
    }
}
