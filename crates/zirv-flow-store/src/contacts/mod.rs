//! Native contacts (architecture section 7.6): the contact definition, contact reads and the
//! synchronous single-contact writes. A contact is a person with a live native source record;
//! the bulk path goes through the ingestion inbox.
//!
//! Lock orders: a single write takes its idempotency lock, then the contact lock, then the
//! source record and the person; the drain takes its inbox rows, then the contact lock of every
//! contact in the batch (sorted, before any row is applied), then the source record and the
//! people. See `write` for why that cannot cycle.

mod definition;
mod records;
mod write;

pub use definition::{
    ContactDefinition, DefinitionError, DefinitionProblem, contact_registry,
    find_contact_definition, put_contact_definition, typed_definition,
};
pub use records::{
    Contact, ContactPage, ContactSearch, ContactSearchError, find_contact, list_contacts,
    search_contacts,
};
pub use write::{
    ContactChange, Idempotency, WriteError, Written, delete_contact, expire_contact_idempotency,
    write_contact,
};
pub(crate) use write::{lock_contacts, replace_stored_fields};

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::contacts::{ContactField, KEY_KIND};
    use zirv_flow_core::people::{Identifier, Precedence, TraitType};
    use zirv_flow_core::{
        Cause, HmacPurpose, MasterKey, OrphanPolicy, Slug, TraceContext, WorkspaceId, WorkspaceName,
    };

    use super::*;
    use crate::{
        IngestItem, IngestPayload, NewTraitDefinition, ProfileRecord, append_inbox,
        apply_profile_batch, begin, commit, create_trait_definition, create_workspace, drain_batch,
        find_person_by_identifier, set_orphan_policy,
    };

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("test"))
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn registry(pool: &PgPool, ws: WorkspaceId, key: &str, trait_type: TraitType) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.to_owned(),
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

    fn field(key: &str, required: bool, identifier: Option<&str>) -> ContactField {
        ContactField {
            key: key.to_owned(),
            required,
            identifier: identifier.map(str::to_owned),
        }
    }

    /// A workspace with the registry fields `email`, `age` and `plan` and a definition that
    /// requires the email, which is also an identifier.
    async fn defined(pool: &PgPool, slug: &str) -> WorkspaceId {
        let ws = workspace(pool, slug).await;
        registry(pool, ws, "email", TraitType::String).await;
        registry(pool, ws, "age", TraitType::Number).await;
        registry(pool, ws, "plan", TraitType::String).await;
        let mut tx = begin(pool).await.unwrap();
        put_contact_definition(
            &mut tx,
            ws,
            &[
                field("email", true, Some("email")),
                field("age", false, None),
                field("plan", false, None),
            ],
            None,
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        ws
    }

    async fn write(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        change: ContactChange,
        idempotency: Option<&Idempotency<'_>>,
    ) -> Result<Written, WriteError> {
        let mut tx = begin(pool).await.unwrap();
        let written = write_contact(&mut tx, ws, key, change, idempotency, &trace()).await?;
        commit(tx).await.unwrap();
        Ok(written)
    }

    async fn remove(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        idempotency: Option<&Idempotency<'_>>,
    ) -> Result<Written, WriteError> {
        let mut tx = begin(pool).await.unwrap();
        let written = delete_contact(&mut tx, ws, key, idempotency, &trace()).await?;
        commit(tx).await.unwrap();
        Ok(written)
    }

    async fn read(pool: &PgPool, ws: WorkspaceId, key: &str) -> Option<Contact> {
        let mut connection = pool.acquire().await.unwrap();
        find_contact(&mut connection, ws, key).await.unwrap()
    }

    async fn person_traits(pool: &PgPool, ws: WorkspaceId, person: Uuid) -> Value {
        sqlx::query_scalar("SELECT traits FROM people WHERE workspace_id = $1 AND id = $2")
            .bind(ws.as_uuid())
            .bind(person)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Every table a contact write may touch, as comparable text.
    async fn dump(pool: &PgPool) -> Vec<Vec<String>> {
        let mut state = Vec::new();
        for table in [
            "people",
            "identities",
            "source_records",
            "person_changes",
            "decision_records",
            "contact_idempotency",
        ] {
            state.push(
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT to_jsonb(t)::text FROM {table} t ORDER BY 1"
                )))
                .fetch_all(pool)
                .await
                .unwrap(),
            );
        }
        state
    }

    fn reasons(error: WriteError) -> Vec<(String, &'static str)> {
        let WriteError::Invalid(problems) = error else {
            panic!("not a validation error: {error:?}");
        };
        problems
            .into_iter()
            .map(|problem| (problem.field, problem.reason))
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn create_patch_replace_and_delete_round_trip_the_typed_fields(pool: PgPool) {
        let ws = defined(&pool, "one").await;

        let created = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "A@Example.com", "age": 30}))),
            None,
        )
        .await
        .unwrap();
        let contact = created.contact.unwrap();
        assert_eq!(created.status, 201);
        assert_eq!(
            contact.fields,
            object(json!({"email": "A@Example.com", "age": 30}))
        );
        let person = contact.person_id;
        assert_eq!(
            person_traits(&pool, ws, person).await,
            json!({"email": "A@Example.com", "age": 30})
        );
        let by_email = find_person_by_identifier(
            &pool,
            ws,
            &Identifier::parse("email", "a@example.com").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(by_email.map(|found| found.id), Some(person));

        let patched = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"age": 31, "plan": "pro"}))),
            None,
        )
        .await
        .unwrap();
        assert_eq!(patched.status, 200);
        assert_eq!(
            patched.contact.unwrap().fields,
            object(json!({"email": "A@Example.com", "age": 31, "plan": "pro"}))
        );

        let unset = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"plan": null}))),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            unset.contact.unwrap().fields,
            object(json!({"email": "A@Example.com", "age": 31}))
        );

        let replaced = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Replace(object(json!({"email": "A@Example.com", "plan": "free"}))),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            replaced.contact.unwrap().fields,
            object(json!({"email": "A@Example.com", "plan": "free"}))
        );
        assert_eq!(
            person_traits(&pool, ws, person).await,
            json!({"email": "A@Example.com", "plan": "free"})
        );

        let deleted = remove(&pool, ws, "c-1", None).await.unwrap();
        assert_eq!((deleted.status, deleted.contact), (204, None));
        assert!(read(&pool, ws, "c-1").await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_contact_that_breaks_the_definition_is_named_and_writes_nothing(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        let before = dump(&pool).await;

        let missing = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"age": 3}))),
            None,
        )
        .await
        .unwrap_err();
        let wrong = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(
                json!({"email": "a@example.com", "age": "secret", "other": 1}),
            )),
            None,
        )
        .await
        .unwrap_err();
        let bad_key = write(
            &pool,
            ws,
            "",
            ContactChange::Create(object(json!({"email": "a@example.com"}))),
            None,
        )
        .await
        .unwrap_err();

        assert_eq!(reasons(missing), [("email".to_owned(), "missing_required")]);
        assert_eq!(
            reasons(wrong),
            [
                ("age".to_owned(), "wrong_type"),
                ("other".to_owned(), "unplanned_property")
            ]
        );
        assert_eq!(reasons(bad_key), [("key".to_owned(), "invalid_key")]);
        assert_eq!(dump(&pool).await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn removing_a_required_field_by_patch_is_refused_and_keeps_the_contact(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "a@example.com"}))),
            None,
        )
        .await
        .unwrap();
        let before = dump(&pool).await;

        let error = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"email": null}))),
            None,
        )
        .await
        .unwrap_err();

        assert_eq!(reasons(error), [("email".to_owned(), "missing_required")]);
        assert_eq!(dump(&pool).await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicate_create_exists_and_a_missing_contact_is_not_found(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        let body = || object(json!({"email": "a@example.com"}));
        write(&pool, ws, "c-1", ContactChange::Create(body()), None)
            .await
            .unwrap();

        let again = write(&pool, ws, "c-1", ContactChange::Create(body()), None).await;
        let replace = write(&pool, ws, "nope", ContactChange::Replace(body()), None).await;
        let patch = write(&pool, ws, "nope", ContactChange::Patch(body()), None).await;
        let delete = remove(&pool, ws, "nope", None).await;

        assert!(matches!(again, Err(WriteError::Exists)));
        assert!(matches!(replace, Err(WriteError::NotFound)));
        assert!(matches!(patch, Err(WriteError::NotFound)));
        assert!(matches!(delete, Err(WriteError::NotFound)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_definition_only_the_key_is_required_and_the_registry_checks_types(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        registry(&pool, ws, "age", TraitType::Number).await;

        let free = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"nickname": "ace", "age": 3}))),
            None,
        )
        .await
        .unwrap();
        let wrong = write(
            &pool,
            ws,
            "c-2",
            ContactChange::Create(object(json!({"age": "three"}))),
            None,
        )
        .await
        .unwrap_err();
        let key_only = write(&pool, ws, "c-3", ContactChange::Create(Map::new()), None)
            .await
            .unwrap();

        assert_eq!(
            free.contact.unwrap().fields,
            object(json!({"nickname": "ace", "age": 3}))
        );
        assert_eq!(reasons(wrong), [("age".to_owned(), "wrong_type")]);
        assert_eq!(key_only.status, 201);
        assert!(read(&pool, ws, "c-2").await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_delete_keeps_another_sources_traits_and_the_orphan_policy_decides_the_person(
        pool: PgPool,
    ) {
        let ws = defined(&pool, "one").await;
        // A CRM record shares the contact's email, so both land on one person.
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[ProfileRecord {
                source: "crm".into(),
                stream: "people".into(),
                record_key: "9".into(),
                identifiers: vec![("email".into(), "a@example.com".into())],
                traits: object(json!({"plan": "crm-plan"})),
                observed_at: time::macros::datetime!(2026-01-01 00:00 UTC),
                trace: trace(),
            }],
        )
        .await
        .unwrap();
        let crate::ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        let created = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "a@example.com", "age": 5}))),
            None,
        )
        .await
        .unwrap();
        assert_eq!(created.contact.unwrap().person_id, person_id);

        remove(&pool, ws, "c-1", None).await.unwrap();

        assert!(read(&pool, ws, "c-1").await.is_none());
        assert_eq!(
            person_traits(&pool, ws, person_id).await,
            json!({"plan": "crm-plan"})
        );

        // Alone, under the delete policy, the person goes with the contact.
        set_orphan_policy(&pool, ws, OrphanPolicy::Delete)
            .await
            .unwrap();
        let alone = write(
            &pool,
            ws,
            "c-2",
            ContactChange::Create(object(json!({"email": "b@example.com"}))),
            None,
        )
        .await
        .unwrap();
        let lone_person = alone.contact.unwrap().person_id;
        remove(&pool, ws, "c-2", None).await.unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM people WHERE id = $1")
            .bind(lone_person)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bulk_upsert_drains_to_the_state_the_single_writes_make(pool: PgPool) {
        let single = defined(&pool, "single").await;
        let bulk = defined(&pool, "bulk").await;
        let bodies = [
            (
                "c-1",
                json!({"email": "a@example.com", "age": 1, "plan": "pro"}),
            ),
            ("c-2", json!({"email": "b@example.com"})),
            // The second write of c-1 replaces its fields.
            ("c-1", json!({"email": "a@example.com", "age": 2})),
        ];
        for (key, body) in &bodies {
            let change = if read(&pool, single, key).await.is_some() {
                ContactChange::Replace(object(body.clone()))
            } else {
                ContactChange::Create(object(body.clone()))
            };
            write(&pool, single, key, change, None).await.unwrap();
        }
        let items: Vec<IngestItem> = bodies
            .iter()
            .map(|(key, body)| {
                let planned =
                    zirv_flow_core::contacts::plan(None, key, &object(body.clone())).unwrap();
                IngestItem {
                    identifiers: vec![
                        (KEY_KIND.to_owned(), (*key).to_owned()),
                        (
                            "email".to_owned(),
                            body["email"].as_str().unwrap().to_owned(),
                        ),
                    ],
                    timestamp: None,
                    message_id: None,
                    payload: IngestPayload::Contact {
                        traits: planned.traits,
                    },
                }
            })
            .collect();
        append_inbox(&pool, bulk, Uuid::now_v7(), &trace(), &items)
            .await
            .unwrap();

        let key = MasterKey::generate().hmac_key(HmacPurpose::Redaction);
        let drained = drain_batch(&pool, bulk, &key, 100).await.unwrap();

        assert_eq!(drained.rows, 3);
        assert_eq!(drained.applied.get(&("contact", "valid")), Some(&3));
        for contact_key in ["c-1", "c-2"] {
            let one = read(&pool, single, contact_key).await.unwrap();
            let other = read(&pool, bulk, contact_key).await.unwrap();
            assert_eq!(one.fields, other.fields, "{contact_key}");
            assert_eq!(
                person_traits(&pool, single, one.person_id).await,
                person_traits(&pool, bulk, other.person_id).await,
                "{contact_key}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_repeated_idempotency_key_replays_the_answer_and_changes_nothing(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        let hash = [1_u8; 32];
        let idem = Idempotency {
            key: "req-1",
            request_hash: &hash,
        };
        let body = || object(json!({"email": "a@example.com", "age": 4}));
        let first = write(&pool, ws, "c-1", ContactChange::Create(body()), Some(&idem))
            .await
            .unwrap();
        let before = dump(&pool).await;

        let second = write(&pool, ws, "c-1", ContactChange::Create(body()), Some(&idem))
            .await
            .unwrap();

        assert_eq!((first.status, first.replayed), (201, false));
        assert_eq!((second.status, second.replayed), (201, true));
        assert_eq!(second.contact, first.contact);
        assert_eq!(dump(&pool).await, before);

        // The replay shows the contact as it is stored now, or nothing once it is gone.
        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"age": 9}))),
            None,
        )
        .await
        .unwrap();
        let later = write(&pool, ws, "c-1", ContactChange::Create(body()), Some(&idem))
            .await
            .unwrap();
        assert_eq!(later.contact.unwrap().fields["age"], json!(9));
        remove(&pool, ws, "c-1", None).await.unwrap();
        let gone = write(&pool, ws, "c-1", ContactChange::Create(body()), Some(&idem))
            .await
            .unwrap();
        assert_eq!(
            (gone.status, gone.contact, gone.replayed),
            (201, None, true)
        );
        assert!(read(&pool, ws, "c-1").await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_reused_for_another_request_is_refused_and_a_delete_replays_204(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        let (hash_a, hash_b) = ([1_u8; 32], [2_u8; 32]);
        let create = Idempotency {
            key: "req-1",
            request_hash: &hash_a,
        };
        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "a@example.com"}))),
            Some(&create),
        )
        .await
        .unwrap();
        let before = dump(&pool).await;

        let reused = Idempotency {
            key: "req-1",
            request_hash: &hash_b,
        };
        let refused = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"age": 1}))),
            Some(&reused),
        )
        .await;
        assert!(matches!(refused, Err(WriteError::KeyReused)));
        assert_eq!(dump(&pool).await, before);

        let delete_key = Idempotency {
            key: "req-2",
            request_hash: &hash_b,
        };
        let first = remove(&pool, ws, "c-1", Some(&delete_key)).await.unwrap();
        let after_delete = dump(&pool).await;
        let again = remove(&pool, ws, "c-1", Some(&delete_key)).await.unwrap();
        assert_eq!((first.status, first.replayed), (204, false));
        assert_eq!(
            (again.status, again.replayed, again.contact),
            (204, true, None)
        );
        assert_eq!(dump(&pool).await, after_delete);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_key_is_forgotten_and_the_upkeep_deletes_old_rows(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        let hash = [1_u8; 32];
        let idem = Idempotency {
            key: "req-1",
            request_hash: &hash,
        };
        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "a@example.com"}))),
            Some(&idem),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE contact_idempotency SET created_at = now() - interval '25 hours'")
            .execute(&pool)
            .await
            .unwrap();

        // Expired, so the key is new again: the create meets the existing contact.
        let reused = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "a@example.com"}))),
            Some(&idem),
        )
        .await;
        assert!(matches!(reused, Err(WriteError::Exists)));

        sqlx::query("UPDATE contact_idempotency SET created_at = now() - interval '25 hours'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(expire_contact_idempotency(&pool).await.unwrap(), 1);
        assert_eq!(count(&pool, "contact_idempotency").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn contacts_are_listed_in_key_order_searched_and_kept_inside_their_workspace(
        pool: PgPool,
    ) {
        let one = defined(&pool, "one").await;
        let two = defined(&pool, "two").await;
        for ws in [one, two] {
            for (key, email) in [
                ("b", "b@example.com"),
                ("a2", "a2@example.com"),
                ("a1", "a1@example.com"),
            ] {
                write(
                    &pool,
                    ws,
                    key,
                    ContactChange::Create(object(json!({"email": email}))),
                    None,
                )
                .await
                .unwrap();
            }
        }
        let mut connection = pool.acquire().await.unwrap();

        let first = list_contacts(&mut connection, one, None, 2).await.unwrap();
        let second = list_contacts(&mut connection, one, first.next_after.as_deref(), 2)
            .await
            .unwrap();
        let prefixed = search_contacts(
            &mut connection,
            one,
            &ContactSearch::KeyPrefix("a".into()),
            10,
        )
        .await
        .unwrap();
        let by_email = search_contacts(
            &mut connection,
            one,
            &ContactSearch::Identifier {
                kind: "email".into(),
                value: "B@Example.com".into(),
            },
            10,
        )
        .await
        .unwrap();
        let invalid = search_contacts(
            &mut connection,
            one,
            &ContactSearch::Identifier {
                kind: "email".into(),
                value: "nope".into(),
            },
            10,
        )
        .await;

        let keys = |contacts: &[Contact]| -> Vec<String> {
            contacts.iter().map(|contact| contact.key.clone()).collect()
        };
        assert_eq!(keys(&first.contacts), ["a1", "a2"]);
        assert_eq!(keys(&second.contacts), ["b"]);
        assert_eq!(second.next_after, None);
        assert_eq!(keys(&prefixed), ["a1", "a2"]);
        assert_eq!(keys(&by_email), ["b"]);
        assert!(matches!(
            invalid,
            Err(ContactSearchError::InvalidIdentifier("invalid_email"))
        ));
        // The same keys in the other workspace belong to other people.
        let other = find_contact(&mut connection, two, "b")
            .await
            .unwrap()
            .unwrap();
        let mine = find_contact(&mut connection, one, "b")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(other.person_id, mine.person_id);
        let theirs = list_contacts(&mut connection, two, None, 10).await.unwrap();
        assert_eq!(keys(&theirs.contacts), ["a1", "a2", "b"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_definition_must_name_live_registry_fields_and_identifiers_must_be_strings(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        registry(&pool, ws, "age", TraitType::Number).await;
        let mut connection = pool.acquire().await.unwrap();

        let unknown =
            put_contact_definition(&mut connection, ws, &[field("nope", false, None)], None)
                .await
                .unwrap_err();
        let numeric = put_contact_definition(
            &mut connection,
            ws,
            &[field("age", false, Some("external:age"))],
            None,
        )
        .await
        .unwrap_err();
        let malformed = put_contact_definition(
            &mut connection,
            ws,
            &[field("age", false, None), field("age", true, None)],
            None,
        )
        .await
        .unwrap_err();

        let DefinitionError::Fields(unknown) = unknown else {
            panic!("expected field problems");
        };
        let DefinitionError::Fields(numeric) = numeric else {
            panic!("expected field problems");
        };
        assert_eq!(
            (unknown[0].index, unknown[0].key.as_str(), unknown[0].reason),
            (0, "nope", "unknown_key")
        );
        assert_eq!(numeric[0].reason, "identifier_needs_string");
        assert!(matches!(malformed, DefinitionError::Invalid(_)));
        assert!(find_contact_definition(&pool, ws).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_idempotency_row_never_holds_the_contact_key_so_a_deletion_leaves_nothing_to_clear(
        pool: PgPool,
    ) {
        let ws = defined(&pool, "one").await;
        let hash = [1_u8; 32];
        // A contact key may be an email address.
        let key = "someone@example.com";
        let create = Idempotency {
            key: "req-create",
            request_hash: &hash,
        };
        let delete = Idempotency {
            key: "req-delete",
            request_hash: &hash,
        };
        write(
            &pool,
            ws,
            key,
            ContactChange::Create(object(json!({"email": "a@example.com"}))),
            Some(&create),
        )
        .await
        .unwrap();
        remove(&pool, ws, key, Some(&delete)).await.unwrap();

        let rows: Vec<String> =
            sqlx::query_scalar("SELECT to_jsonb(t)::text FROM contact_idempotency t")
                .fetch_all(&pool)
                .await
                .unwrap();

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.contains(key)));
        // A replay takes the key from its own request.
        let replay = remove(&pool, ws, key, Some(&delete)).await.unwrap();
        assert!(replay.replayed && replay.status == 204);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_idempotency_table_holds_no_person_id_and_no_field_values(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        let hash = [7_u8; 32];
        let idem = Idempotency {
            key: "req-1",
            request_hash: &hash,
        };
        let written = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "secret@example.com", "age": 4242}))),
            Some(&idem),
        )
        .await
        .unwrap();

        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT column_name::text, data_type::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'contact_idempotency' \
             ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let row: String = sqlx::query_scalar("SELECT to_jsonb(t)::text FROM contact_idempotency t")
            .fetch_one(&pool)
            .await
            .unwrap();

        let names: Vec<&str> = columns.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "created_at",
                "key",
                "request_hash",
                "status",
                "workspace_id"
            ]
        );
        assert!(
            columns
                .iter()
                .all(|(name, kind)| kind != "jsonb" && (kind != "uuid" || name == "workspace_id"))
        );
        assert!(!row.contains("secret@example.com") && !row.contains("4242"));
        assert!(!row.contains(&written.contact.unwrap().person_id.to_string()));
    }

    /// Appends one bulk contact row, as the API would after validating it.
    async fn append_bulk(pool: &PgPool, ws: WorkspaceId, key: &str, body: Value) {
        let mut identifiers = vec![(KEY_KIND.to_owned(), key.to_owned())];
        if let Some(email) = body.get("email").and_then(Value::as_str) {
            identifiers.push(("email".to_owned(), email.to_owned()));
        }
        let item = IngestItem {
            identifiers,
            timestamp: None,
            message_id: None,
            payload: IngestPayload::Contact {
                traits: object(body),
            },
        };
        append_inbox(pool, ws, Uuid::now_v7(), &trace(), &[item])
            .await
            .unwrap();
    }

    /// Starts a drain of the workspace and shows that it has not finished after a pause.
    async fn drain_that_waits(
        pool: &PgPool,
        ws: WorkspaceId,
    ) -> tokio::task::JoinHandle<crate::DrainOutcome> {
        let (pool, key) = (
            pool.clone(),
            MasterKey::generate().hmac_key(HmacPurpose::Redaction),
        );
        let drain = tokio::spawn(async move { drain_batch(&pool, ws, &key, 100).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !drain.is_finished(),
            "the drain did not wait for the contact lock"
        );
        drain
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bulk_replace_waits_for_a_single_write_and_retracts_what_it_added(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "a@example.com", "age": 1}))),
            None,
        )
        .await
        .unwrap();
        // A patch adds `plan`, and has not committed yet.
        let mut tx = begin(&pool).await.unwrap();
        write_contact(
            &mut tx,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"plan": "pro"}))),
            None,
            &trace(),
        )
        .await
        .unwrap();
        append_bulk(
            &pool,
            ws,
            "c-1",
            json!({"email": "a@example.com", "age": 2}),
        )
        .await;

        let drain = drain_that_waits(&pool, ws).await;
        commit(tx).await.unwrap();
        let drained = drain.await.unwrap();

        assert_eq!(drained.applied.get(&("contact", "valid")), Some(&1));
        assert_eq!(
            read(&pool, ws, "c-1").await.unwrap().fields,
            object(json!({"email": "a@example.com", "age": 2}))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_create_after_a_waiting_drain_finds_the_contact_the_drain_made(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        append_bulk(&pool, ws, "c-9", json!({"email": "n@example.com"})).await;
        let mut holder = begin(&pool).await.unwrap();
        lock_contacts(&mut holder, ws, ["c-9"]).await.unwrap();

        let drain = drain_that_waits(&pool, ws).await;
        commit(holder).await.unwrap();
        drain.await.unwrap();
        let again = write(
            &pool,
            ws,
            "c-9",
            ContactChange::Create(object(json!({"email": "n@example.com"}))),
            None,
        )
        .await;

        assert!(matches!(again, Err(WriteError::Exists)));
    }

    async fn person_of_email(pool: &PgPool, ws: WorkspaceId, email: &str) -> Option<Uuid> {
        find_person_by_identifier(pool, ws, &Identifier::parse("email", email).unwrap())
            .await
            .unwrap()
            .map(|found| found.id)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_changed_identifier_field_releases_the_old_identity_by_put_and_by_bulk(pool: PgPool) {
        let ws = defined(&pool, "one").await;
        let created = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "old@example.test"}))),
            None,
        )
        .await
        .unwrap();
        let person = created.contact.unwrap().person_id;

        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Replace(object(json!({"email": "new@example.test"}))),
            None,
        )
        .await
        .unwrap();

        assert_eq!(person_of_email(&pool, ws, "old@example.test").await, None);
        assert_eq!(
            person_of_email(&pool, ws, "new@example.test").await,
            Some(person)
        );

        append_bulk(&pool, ws, "c-1", json!({"email": "third@example.test"})).await;
        let key = MasterKey::generate().hmac_key(HmacPurpose::Redaction);
        drain_batch(&pool, ws, &key, 100).await.unwrap();

        assert_eq!(person_of_email(&pool, ws, "new@example.test").await, None);
        assert_eq!(
            person_of_email(&pool, ws, "third@example.test").await,
            Some(person)
        );
        assert_eq!(
            find_person_by_identifier(&pool, ws, &Identifier::parse(KEY_KIND, "c-1").unwrap())
                .await
                .unwrap()
                .map(|found| found.id),
            Some(person)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_another_source_lists_stays_when_the_contact_changes_its_field(
        pool: PgPool,
    ) {
        let ws = defined(&pool, "one").await;
        let created = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "shared@example.test"}))),
            None,
        )
        .await
        .unwrap();
        let person = created.contact.unwrap().person_id;
        apply_profile_batch(
            &pool,
            ws,
            &[ProfileRecord {
                source: "crm".into(),
                stream: "people".into(),
                record_key: "9".into(),
                identifiers: vec![("email".into(), "shared@example.test".into())],
                traits: Map::new(),
                observed_at: time::macros::datetime!(2026-01-01 00:00 UTC),
                trace: trace(),
            }],
        )
        .await
        .unwrap();

        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"email": "other@example.test"}))),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            person_of_email(&pool, ws, "shared@example.test").await,
            Some(person)
        );
        assert_eq!(
            person_of_email(&pool, ws, "other@example.test").await,
            Some(person)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_field_whose_registry_key_was_archived_keeps_its_value_and_does_not_block_updates(
        pool: PgPool,
    ) {
        let ws = defined(&pool, "one").await;
        write(
            &pool,
            ws,
            "c-1",
            ContactChange::Create(object(json!({"email": "a@example.com", "age": 3}))),
            None,
        )
        .await
        .unwrap();
        crate::update_trait_definition(
            &pool,
            ws,
            "age",
            &crate::TraitDefinitionUpdate {
                archived: Some(true),
                ..crate::TraitDefinitionUpdate::default()
            },
        )
        .await
        .unwrap();

        let patched = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"plan": "pro"}))),
            None,
        )
        .await
        .unwrap();
        let replaced = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Replace(object(json!({"email": "a@example.com", "plan": "free"}))),
            None,
        )
        .await
        .unwrap();
        let explicit = write(
            &pool,
            ws,
            "c-1",
            ContactChange::Patch(object(json!({"age": 4}))),
            None,
        )
        .await
        .unwrap_err();

        assert_eq!(
            patched.contact.unwrap().fields,
            object(json!({"email": "a@example.com", "age": 3, "plan": "pro"}))
        );
        assert_eq!(
            replaced.contact.unwrap().fields,
            object(json!({"email": "a@example.com", "age": 3, "plan": "free"}))
        );
        assert_eq!(
            reasons(explicit),
            [("age".to_owned(), "unplanned_property")]
        );

        // A bulk replacement leaves it alone too.
        append_bulk(&pool, ws, "c-1", json!({"email": "a@example.com"})).await;
        let key = MasterKey::generate().hmac_key(HmacPurpose::Redaction);
        let drained = drain_batch(&pool, ws, &key, 100).await.unwrap();
        assert_eq!(drained.applied.get(&("contact", "valid")), Some(&1));
        assert_eq!(
            read(&pool, ws, "c-1").await.unwrap().fields,
            object(json!({"email": "a@example.com", "age": 3}))
        );
    }
}
