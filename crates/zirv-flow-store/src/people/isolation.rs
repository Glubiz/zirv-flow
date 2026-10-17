//! Workspace isolation for the profile store: two workspaces hold the same data, and every
//! read function must return only its own workspace's rows. Later issues add their reads here.

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::people::{Identifier, Precedence, TraitType};
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};

    use crate::people::{
        ConflictResolution, NewTraitDefinition, ProfileOutcome, ProfileRecord, ResolveError,
        TraitDefinitionUpdate, apply_profile_batch, create_trait_definition, current_person,
        find_conflict, find_person, find_person_by_identifier, find_trait_definition,
        list_conflicts, list_identities, list_identity_conflicts, list_trait_definitions,
        resolve_conflict, update_trait_definition,
    };
    use crate::{
        ListSource, add_members, begin, commit, create_list, create_workspace, delete_list,
        find_list, list_lists, lists_for, members, membership_history, remove_members, update_list,
    };

    use crate::{
        ConsentChange, ConsentOutcome, NewSuppression, NewTopic, add_suppression, consent_for,
        consent_history, create_topic, find_suppression, find_topic, is_suppressed,
        list_suppressions, list_topics, set_consent,
    };
    use zirv_flow_core::consent::{
        ChangeSource, Channel, ConsentStatus, SuppressionReason, TopicPolicy,
    };

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn record(key: &str, identifiers: Vec<(&str, &str)>) -> ProfileRecord {
        ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: key.into(),
            identifiers: identifiers
                .into_iter()
                .map(|(kind, value)| (kind.to_owned(), value.to_owned()))
                .collect(),
            traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        }
    }

    /// Seeds the same shape in a workspace: a person with an email and an external id, a
    /// second one with another external id, a record that touches both (a conflict, since a
    /// person holds one external id) and a registry definition. Returns the first person, who
    /// owns the email, and the second, who holds the conflict.
    async fn seed(pool: &PgPool, ws: WorkspaceId) -> (Uuid, Uuid) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: "tier".into(),
                trait_type: TraitType::String,
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
        let outcomes = apply_profile_batch(
            pool,
            ws,
            &[
                record("1", vec![("email", "a@example.com"), ("external:crm", "1")]),
                record("2", vec![("external:crm", "2")]),
                record("3", vec![("email", "a@example.com"), ("external:crm", "2")]),
            ],
        )
        .await
        .unwrap();
        let (
            ProfileOutcome::Applied {
                person_id: first, ..
            },
            ProfileOutcome::Applied {
                person_id: second, ..
            },
        ) = (&outcomes[0], &outcomes[1])
        else {
            panic!("not applied");
        };
        (*first, *second)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_read_returns_only_its_own_workspaces_rows(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let (person_one, holder_one) = seed(&pool, one).await;
        let (person_two, holder_two) = seed(&pool, two).await;
        assert_ne!(person_one, person_two);
        let email = Identifier::parse("email", "a@example.com").unwrap();

        // Registry: each workspace sees its own definitions (`tier` and the inferred `plan`).
        for ws in [one, two] {
            let definitions = list_trait_definitions(&pool, ws).await.unwrap();
            assert!(definitions.iter().all(|d| d.workspace_id == ws));
            let keys: Vec<&str> = definitions.iter().map(|d| d.key.as_str()).collect();
            assert_eq!(keys, ["plan", "tier"]);
        }
        update_trait_definition(
            &pool,
            one,
            "tier",
            &TraitDefinitionUpdate {
                description: Some("only in one".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let in_two = find_trait_definition(&pool, two, "tier")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(in_two.description, "");

        // People and identities.
        assert!(find_person(&pool, one, person_two).await.unwrap().is_none());
        assert!(find_person(&pool, two, person_one).await.unwrap().is_none());
        let found = find_person_by_identifier(&pool, one, &email)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, person_one);
        let found = find_person_by_identifier(&pool, two, &email)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, person_two);
        assert!(
            list_identities(&pool, one, person_two)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            list_identities(&pool, one, person_one).await.unwrap().len(),
            2
        );

        // Conflicts: each workspace lists its own, and a foreign person reads as empty.
        for (ws, holder, foreign) in [(one, holder_one, holder_two), (two, holder_two, holder_one)]
        {
            let conflicts = list_identity_conflicts(&pool, ws, holder).await.unwrap();
            assert_eq!(conflicts.len(), 1);
            assert_ne!(conflicts[0].other_person_id, foreign);
            assert!(
                list_identity_conflicts(&pool, ws, foreign)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (open, more) = list_conflicts(&pool, ws, false, 10, None).await.unwrap();
            assert!(!more);
            assert_eq!(open, conflicts);
            assert_eq!(
                find_conflict(&pool, ws, conflicts[0].id).await.unwrap(),
                Some(conflicts[0].clone())
            );
        }
        let foreign = list_identity_conflicts(&pool, one, holder_one)
            .await
            .unwrap();
        assert!(
            find_conflict(&pool, two, foreign[0].id)
                .await
                .unwrap()
                .is_none()
        );
        let mut tx = begin(&pool).await.unwrap();
        let refused = resolve_conflict(
            &mut tx,
            two,
            foreign[0].id,
            ConflictResolution::KeepSeparate,
            &TraceContext::root(Cause::api("t")),
        )
        .await;
        assert!(matches!(refused, Err(ResolveError::NotFound)));

        // Merges: the current person of another workspace's id is nothing.
        assert_eq!(current_person(&pool, one, holder_two).await.unwrap(), None);
        assert_eq!(
            current_person(&pool, one, holder_one).await.unwrap(),
            Some(holder_one)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_event_read_returns_only_its_own_workspaces_rows(pool: PgPool) {
        use zirv_flow_core::events::{EventMode, PropertyType, SchemaProperty};
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};

        use crate::{
            EventPolicy, EventRecord, NewEventSchema, RejectFilter, apply_event_batch, begin,
            commit, create_event_schema, find_event_schema, get_event_policy, list_event_schemas,
            list_ingest_rejects, list_schema_versions, publish_schema_draft, set_event_policy,
        };

        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        for (ws, name) in [(one, "order"), (two, "order")] {
            create_event_schema(
                &pool,
                ws,
                &NewEventSchema {
                    name: name.into(),
                    mode: EventMode::Block,
                    description: String::new(),
                    properties: vec![SchemaProperty {
                        name: "total".into(),
                        property_type: PropertyType::Number,
                        required: true,
                        enum_values: None,
                        description: String::new(),
                        pii: false,
                    }],
                },
            )
            .await
            .unwrap();
            let mut tx = begin(&pool).await.unwrap();
            publish_schema_draft(&mut tx, ws, name, None).await.unwrap();
            commit(tx).await.unwrap();
            let rejected = EventRecord {
                source_record_id: None,
                source: "web".into(),
                identifiers: vec![("email".into(), "a@example.com".into())],
                name: name.into(),
                properties: json!({"total": "x"}).as_object().cloned().unwrap(),
                timestamp: None,
                received_at: datetime!(2026-01-01 00:00 UTC),
                message_id: None,
                trace: TraceContext::root(Cause::api("t")),
            };
            apply_event_batch(&pool, ws, &key, &[rejected])
                .await
                .unwrap();
        }
        set_event_policy(
            &pool,
            one,
            &EventPolicy {
                future_skew_seconds: 1,
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();

        for ws in [one, two] {
            let schemas = list_event_schemas(&pool, ws).await.unwrap();
            assert_eq!(schemas.len(), 1);
            assert_eq!(schemas[0].workspace_id, ws);
            assert_eq!(
                list_schema_versions(&pool, ws, "order")
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let (rejects, _) = list_ingest_rejects(&pool, ws, RejectFilter::default(), None, 10)
                .await
                .unwrap();
            assert_eq!(rejects.len(), 1);
        }
        assert!(
            find_event_schema(&pool, one, "other")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            get_event_policy(&pool, one)
                .await
                .unwrap()
                .future_skew_seconds,
            1
        );
        assert_eq!(
            get_event_policy(&pool, two)
                .await
                .unwrap()
                .future_skew_seconds,
            300
        );
        let (ids_one, _) = list_ingest_rejects(&pool, one, RejectFilter::default(), None, 10)
            .await
            .unwrap();
        let (ids_two, _) = list_ingest_rejects(&pool, two, RejectFilter::default(), None, 10)
            .await
            .unwrap();
        assert_ne!(ids_one[0].id, ids_two[0].id);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_list_read_and_write_stays_inside_its_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let (person_one, _) = seed(&pool, one).await;
        let (person_two, _) = seed(&pool, two).await;
        let list_one = create_list(&pool, one, "VIPs", "", None).await.unwrap();
        let list_two = create_list(&pool, two, "VIPs", "", None).await.unwrap();
        let trace = TraceContext::root(Cause::api("t"));
        let mut tx = begin(&pool).await.unwrap();
        add_members(
            &mut tx,
            one,
            list_one.id,
            &[person_one],
            ListSource::Api,
            None,
            &trace,
        )
        .await
        .unwrap();
        add_members(
            &mut tx,
            two,
            list_two.id,
            &[person_two],
            ListSource::Api,
            None,
            &trace,
        )
        .await
        .unwrap();
        // A person of the other workspace is missing, and a list of it does not exist.
        let foreign_person = add_members(
            &mut tx,
            one,
            list_one.id,
            &[person_two],
            ListSource::Api,
            None,
            &trace,
        )
        .await
        .unwrap();
        let foreign_list = add_members(
            &mut tx,
            one,
            list_two.id,
            &[person_one],
            ListSource::Api,
            None,
            &trace,
        )
        .await;
        let foreign_remove = remove_members(
            &mut tx,
            one,
            list_two.id,
            &[person_two],
            ListSource::Api,
            None,
            &trace,
        )
        .await;
        commit(tx).await.unwrap();

        assert_eq!(foreign_person.missing, [0]);
        assert!(foreign_list.is_err());
        assert!(foreign_remove.is_err());
        for (ws, own, person, foreign) in [
            (one, &list_one, person_one, &list_two),
            (two, &list_two, person_two, &list_one),
        ] {
            let lists = list_lists(&pool, ws).await.unwrap();
            assert_eq!(
                lists.iter().map(|list| list.id).collect::<Vec<_>>(),
                [own.id]
            );
            assert!(find_list(&pool, ws, foreign.id).await.unwrap().is_none());
            assert!(
                update_list(&pool, ws, foreign.id, Some("x"), None)
                    .await
                    .unwrap()
                    .is_none()
            );
            let (page, _) = members(&pool, ws, own.id, 10, None).await.unwrap();
            assert_eq!(
                page.iter().map(|m| m.person_id).collect::<Vec<_>>(),
                [person]
            );
            assert!(
                members(&pool, ws, foreign.id, 10, None)
                    .await
                    .unwrap()
                    .0
                    .is_empty()
            );
            assert_eq!(lists_for(&pool, ws, person).await.unwrap().len(), 1);
            let (history, _) = membership_history(&pool, ws, person, 10, None)
                .await
                .unwrap();
            assert_eq!(history.len(), 1);
            let mut connection = pool.acquire().await.unwrap();
            assert!(
                delete_list(&mut connection, ws, foreign.id)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(lists_for(&pool, one, person_two).await.unwrap().is_empty());
        assert_eq!(
            find_list(&pool, one, list_one.id)
                .await
                .unwrap()
                .unwrap()
                .member_count,
            1
        );
    }

    /// Seeds a person with a channel-wide and a topic consent, a topic and a suppression, and
    /// returns the person, the topic and the suppressed hash.
    async fn seed_consent(pool: &PgPool, ws: WorkspaceId) -> (Uuid, Uuid, [u8; 32]) {
        let (person, _) = seed(pool, ws).await;
        let topic = create_topic(
            pool,
            ws,
            &NewTopic {
                key: "news".into(),
                name: "News".into(),
                description: String::new(),
                channel: Channel::Email,
                policy: TopicPolicy::OptOut,
            },
        )
        .await
        .unwrap()
        .id;
        let mut tx = begin(pool).await.unwrap();
        for (topic, status) in [
            (None, ConsentStatus::Unsubscribed),
            (Some(topic), ConsentStatus::Subscribed),
        ] {
            let outcome = set_consent(
                &mut tx,
                ws,
                &consent_change(person, topic, status),
                &TraceContext::root(Cause::api("t")),
            )
            .await
            .unwrap();
            assert!(matches!(outcome, ConsentOutcome::Changed { .. }));
        }
        commit(tx).await.unwrap();
        let hash = [9_u8; 32];
        add_suppression(
            pool,
            ws,
            &NewSuppression {
                identifier_hash: hash,
                kind: "email",
                reason: SuppressionReason::Manual,
                source: "api",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        (person, topic, hash)
    }

    fn consent_change(person: Uuid, topic: Option<Uuid>, status: ConsentStatus) -> ConsentChange {
        ConsentChange {
            person,
            channel: Channel::Email,
            topic,
            status,
            source: ChangeSource::Api,
            evidence: json!({}),
            occurred_at: datetime!(2026-01-01 00:00 UTC),
            actor: None,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn consent_topics_and_suppressions_are_private_to_their_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let (person_one, topic_one, hash) = seed_consent(&pool, one).await;
        let (person_two, topic_two, _) = seed_consent(&pool, two).await;
        assert_ne!(topic_one, topic_two);

        for (ws, own, foreign) in [(one, topic_one, topic_two), (two, topic_two, topic_one)] {
            let topics = list_topics(&pool, ws, true).await.unwrap();
            assert_eq!(topics.len(), 1);
            assert_eq!(topics[0].id, own);
            assert!(find_topic(&pool, ws, foreign).await.unwrap().is_none());
        }
        assert!(consent_for(&pool, one, person_two).await.unwrap().is_none());
        assert!(
            consent_history(&pool, two, person_one, 10, None)
                .await
                .unwrap()
                .is_none()
        );
        for (ws, person) in [(one, person_one), (two, person_two)] {
            let states = consent_for(&pool, ws, person).await.unwrap().unwrap();
            assert_eq!(states.len(), 2);
            let (history, _) = consent_history(&pool, ws, person, 10, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(history.len(), 2);
        }

        let mut tx = begin(&pool).await.unwrap();
        let trace = TraceContext::root(Cause::api("t"));
        let foreign_topic = consent_change(person_one, Some(topic_two), ConsentStatus::Subscribed);
        let foreign_person = consent_change(person_two, None, ConsentStatus::Subscribed);
        assert_eq!(
            set_consent(&mut tx, one, &foreign_topic, &trace)
                .await
                .unwrap(),
            ConsentOutcome::NoSuchTopic
        );
        assert_eq!(
            set_consent(&mut tx, one, &foreign_person, &trace)
                .await
                .unwrap(),
            ConsentOutcome::NoSuchPerson
        );
        drop(tx);

        assert!(is_suppressed(&pool, one, &hash).await.unwrap());
        assert!(find_suppression(&pool, one, &hash).await.unwrap().is_some());
        assert!(!is_suppressed(&pool, two, &[1; 32]).await.unwrap());
        let (listed, _) = list_suppressions(&pool, one, 10, None, None).await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn draining_the_inbox_touches_only_its_own_workspaces_rows(pool: PgPool) {
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};

        use crate::{IngestItem, IngestPayload, Shard, append_inbox, drain_batch, inbox_heads};

        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let trace = TraceContext::root(Cause::api("t"));
        let item = IngestItem {
            identifiers: vec![("email".into(), "a@example.com".into())],
            timestamp: None,
            message_id: Some("m-1".into()),
            payload: IngestPayload::Track {
                event: "seen".into(),
                properties: serde_json::Map::new(),
            },
        };
        for ws in [one, two] {
            append_inbox(
                &pool,
                ws,
                Uuid::now_v7(),
                &trace,
                std::slice::from_ref(&item),
            )
            .await
            .unwrap();
        }
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);

        let outcome = drain_batch(&pool, one, Shard::ALL, &key, 100)
            .await
            .unwrap();

        assert_eq!(outcome.rows, 1);
        let heads = inbox_heads(&pool).await.unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].workspace_id, two);
        let events: Vec<Uuid> = sqlx::query_scalar("SELECT workspace_id FROM events")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(events, [one.as_uuid()]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn search_profile_and_timeline_reads_stay_inside_their_workspace(pool: PgPool) {
        use crate::people::{
            PersonSearch, TimelineFilter, TraitOp, load_profile, load_timeline, search_people,
        };

        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let (person_one, _) = seed(&pool, one).await;
        let (person_two, _) = seed(&pool, two).await;
        let email = PersonSearch::Identifier {
            kind: "email".into(),
            value: "a@example.com".into(),
        };
        let plan = PersonSearch::Trait {
            key: "plan".into(),
            op: TraitOp::Exists,
            value: None,
        };

        for (ws, own, foreign) in [(one, person_one, person_two), (two, person_two, person_one)] {
            let found = search_people(&pool, ws, &email, 10, None, true)
                .await
                .unwrap();
            assert_eq!(found.hits.len(), 1);
            assert_eq!(found.hits[0].id, own);
            let by_trait = search_people(&pool, ws, &plan, 50, None, true)
                .await
                .unwrap();
            assert!(by_trait.hits.iter().all(|hit| hit.id != foreign));
            assert!(load_profile(&pool, ws, foreign).await.unwrap().is_none());
            assert!(load_profile(&pool, ws, own).await.unwrap().is_some());
            let filter = TimelineFilter::default();
            assert!(
                load_timeline(&pool, ws, foreign, filter, 10, None)
                    .await
                    .unwrap()
                    .is_none()
            );
            let timeline = load_timeline(&pool, ws, own, filter, 100, None)
                .await
                .unwrap()
                .unwrap();
            assert!(!timeline.entries.is_empty());
            for entry in timeline.entries {
                if let crate::people::TimelineEntry::Decision(decision) = entry {
                    assert_eq!(decision.subject_ids["person"], own.to_string());
                }
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sources_and_streams_are_read_and_changed_only_in_their_own_workspace(pool: PgPool) {
        use crate::{
            DiscoveredStream, NewSource, SourceUpdate, StreamSettings, configure_stream,
            create_source, delete_source, find_source, find_stream, list_sources, list_streams,
            update_source, upsert_discovered_streams,
        };

        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let config = json!({});
        let descriptor = json!({});
        let mut sources = Vec::new();
        for ws in [one, two] {
            let source = create_source(
                &pool,
                ws,
                &NewSource {
                    id: Uuid::now_v7(),
                    kind: "fake",
                    name: "crm",
                    config: &config,
                    credential_secret_id: None,
                    credential_fields: &[],
                    schedule: None,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            let mut tx = begin(&pool).await.unwrap();
            upsert_discovered_streams(
                &mut tx,
                ws,
                source.id,
                &[DiscoveredStream {
                    name: "contacts",
                    sync_mode: "full_refresh",
                    cursor_field: None,
                    primary_key: &[],
                    descriptor: &descriptor,
                    suggested_mapping: None,
                }],
            )
            .await
            .unwrap();
            commit(tx).await.unwrap();
            sources.push(source.id);
        }

        for (ws, own, foreign) in [(one, sources[0], sources[1]), (two, sources[1], sources[0])] {
            let listed = list_sources(&pool, ws).await.unwrap();
            assert_eq!(listed.iter().map(|s| s.id).collect::<Vec<_>>(), [own]);
            assert!(find_source(&pool, ws, foreign).await.unwrap().is_none());
            assert!(
                update_source(
                    &mut pool.acquire().await.unwrap(),
                    ws,
                    foreign,
                    &SourceUpdate::default()
                )
                .await
                .unwrap()
                .is_none()
            );
            assert_eq!(list_streams(&pool, ws, own).await.unwrap().len(), 1);
            assert!(list_streams(&pool, ws, foreign).await.unwrap().is_empty());
            assert!(
                find_stream(&pool, ws, foreign, "contacts")
                    .await
                    .unwrap()
                    .is_none()
            );
            let settings = StreamSettings {
                reconcile_schedule: None,
                sweep_limit: 0.5,
                enabled: true,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                staleness_threshold_seconds: None,
            };
            assert!(
                configure_stream(
                    &mut pool.acquire().await.unwrap(),
                    ws,
                    foreign,
                    "contacts",
                    &settings
                )
                .await
                .unwrap()
                .is_none()
            );
            let mut connection = pool.acquire().await.unwrap();
            assert!(
                delete_source(&mut connection, ws, foreign)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            !find_stream(&pool, one, sources[0], "contacts")
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sync_runs_are_read_and_changed_only_in_their_own_workspace(pool: PgPool) {
        use crate::sync_runs::testing::{batch, source_with_stream, started_run, upsert};
        use crate::{
            CancelOutcome, RunKind, RunTrigger, cancel_run, commit_sync_batch, fail_run, find_run,
            finish_run, list_run_rejects, list_runs, queue_run, queue_runs_for_source, start_run,
            sync_schedule,
        };

        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let (source, stream) = source_with_stream(&pool, one, "crm", "full_refresh").await;
        let fx = started_run(&pool, one, source, stream).await;
        let run = fx.run.id;
        let mut connection = pool.acquire().await.unwrap();

        assert!(find_run(&pool, two, source, run).await.unwrap().is_none());
        let (listed, _) = list_runs(&pool, two, source, None, None, 10).await.unwrap();
        assert!(listed.is_empty());
        assert!(
            list_run_rejects(&pool, two, run, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cancel_run(&mut connection, two, source, run).await.unwrap(),
            CancelOutcome::NotFound
        );
        assert!(start_run(&pool, two, run).await.unwrap().is_none());
        let lease = crate::SyncLease {
            job_id: fx.job.id,
            token: fx.job.lease_token,
        };
        assert_eq!(
            fail_run(&pool, two, run, lease, "x").await.unwrap(),
            crate::RunEnd::Over
        );
        assert_eq!(
            finish_run(&pool, two, run, lease).await.unwrap(),
            crate::RunEnd::Over
        );
        assert!(
            queue_run(
                &mut connection,
                two,
                source,
                stream,
                RunKind::Sync,
                RunTrigger::Manual,
                None
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            queue_runs_for_source(
                &mut connection,
                two,
                source,
                RunTrigger::Manual,
                None,
                false
            )
            .await
            .unwrap()
            .is_none()
        );
        // A batch aimed at the run through the other workspace writes nothing.
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        let cp = json!({"offset": 1});
        let foreign = commit_sync_batch(
            &pool,
            &crate::SyncBatch {
                workspace: two,
                ..batch(&fx, &source.to_string(), &upserts, &cp)
            },
        )
        .await
        .unwrap();
        assert!(matches!(foreign, crate::SyncBatchOutcome::Cancelled));
        // Nor can the other workspace mirror or clear this source's schedule.
        sync_schedule(&mut connection, one, source, Some("0 * * * *"), true)
            .await
            .unwrap();
        sync_schedule(&mut connection, two, source, None, true)
            .await
            .unwrap();
        let schedules: i64 =
            sqlx::query_scalar("SELECT count(*) FROM schedules WHERE source_id = $1")
                .bind(source)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(schedules, 1);

        let own = find_run(&pool, one, source, run).await.unwrap().unwrap();
        assert_eq!((own.state.as_str(), own.records_read), ("running", 0));
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM source_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sync_events_event_rejects_and_list_members_stay_inside_their_workspace(pool: PgPool) {
        use crate::sync_runs::testing::{batch, fixture, upsert};
        use crate::{
            RejectFilter, SyncBatchOutcome, SyncEvent, SyncUpsert, commit_sync_batch,
            list_ingest_rejects, list_run_rejects,
        };
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};

        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let one = fixture(&pool, "one", "full_refresh").await;
        let two = fixture(&pool, "two", "full_refresh").await;
        let news = create_list(&pool, one.ws, "news", "", None)
            .await
            .unwrap()
            .id;
        // A block-mode schema in the first workspace only: its event is refused there.
        crate::create_event_schema(
            &pool,
            one.ws,
            &crate::NewEventSchema {
                name: "order_placed".into(),
                mode: zirv_flow_core::events::EventMode::Block,
                description: String::new(),
                properties: vec![zirv_flow_core::events::SchemaProperty {
                    name: "total".into(),
                    property_type: zirv_flow_core::events::PropertyType::Number,
                    required: true,
                    enum_values: None,
                    description: String::new(),
                    pii: false,
                }],
            },
        )
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        crate::publish_schema_draft(&mut tx, one.ws, "order_placed", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        let with = |fx: &crate::sync_runs::testing::Fixture, record: &str, total, lists| {
            let mut item: SyncUpsert = upsert(fx, record, &format!("{record}@example.com"));
            item.lists = lists;
            item.event = Some(SyncEvent {
                name: "order_placed".into(),
                properties: json!({ "total": total }).as_object().cloned().unwrap(),
                timestamp: None,
            });
            item
        };
        let upserts = [
            with(&one, "ok", json!(1), vec![news]),
            with(&one, "bad", json!("x"), vec![news]),
        ];
        let cp = json!({"offset": 2});
        let source = one.source_id.to_string();
        let done = commit_sync_batch(
            &pool,
            &crate::SyncBatch {
                redaction_key: Some(&key),
                ..batch(&one, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();
        assert!(matches!(done, SyncBatchOutcome::Committed(_)));

        // The second workspace sees none of it, however it asks.
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE workspace_id = $1")
            .bind(two.ws.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 0);
        let (rejects, _) = list_ingest_rejects(&pool, two.ws, RejectFilter::default(), None, 10)
            .await
            .unwrap();
        assert!(rejects.is_empty());
        assert!(
            list_run_rejects(&pool, two.ws, one.run.id, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let (other_members, _) = members(&pool, two.ws, news, 10, None).await.unwrap();
        assert!(other_members.is_empty());
        let (own_rejects, _) =
            list_ingest_rejects(&pool, one.ws, RejectFilter::default(), None, 10)
                .await
                .unwrap();
        assert_eq!(own_rejects.len(), 1);

        // A batch of the second workspace naming the first one's list finds no such list, and
        // writes nothing.
        let foreign = [with(&two, "ok", json!(1), vec![news])];
        let other_source = two.source_id.to_string();
        let refused = commit_sync_batch(
            &pool,
            &crate::SyncBatch {
                redaction_key: Some(&key),
                ..batch(&two, &other_source, &foreign, &cp)
            },
        )
        .await
        .unwrap();
        assert!(matches!(refused, SyncBatchOutcome::ListMissing));
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people WHERE workspace_id = $1")
            .bind(two.ws.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 0);
        let (still, _) = members(&pool, one.ws, news, 10, None).await.unwrap();
        assert_eq!(still.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stream_freshness_is_read_and_recorded_only_in_its_own_workspace(pool: PgPool) {
        use crate::sync_runs::testing::source_with_stream;
        use crate::{StreamOutcome, record_stream_outcome, stream_freshness};

        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let (source, stream) = source_with_stream(&pool, one, "crm", "full_refresh").await;
        let now = crate::database_now(&pool).await.unwrap();

        let foreign = stream_freshness(&pool, Some(two), None, now).await.unwrap();
        let foreign_source = stream_freshness(&pool, Some(two), Some(source), now)
            .await
            .unwrap();
        let recorded = record_stream_outcome(
            &mut pool.acquire().await.unwrap(),
            two,
            stream,
            StreamOutcome::Failed,
            now,
        )
        .await
        .unwrap();

        assert!(foreign.is_empty());
        assert!(foreign_source.is_empty());
        assert!(!recorded);
        let own = stream_freshness(&pool, Some(one), None, now).await.unwrap();
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].state, zirv_flow_core::Freshness::Fresh);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sweep_in_one_workspace_leaves_the_records_and_events_of_another(pool: PgPool) {
        use crate::sync_runs::testing::{commit_records, fixture, next_run, upsert};
        use crate::{
            ReconcileQueued, RunTrigger, SweepOutcome, SweepUnit, queue_reconcile_run,
            sweep_exceeds_limit, sweep_unreached,
        };

        let first = fixture(&pool, "one", "incremental").await;
        commit_records(&pool, &first, 3).await;
        let fx = next_run(&pool, &first).await;
        let two = workspace(&pool, "two").await;
        let source = fx.source_id.to_string();
        // Workspace two holds records of the same source and stream names, each with an event.
        for n in 0..3 {
            let mut record = upsert(&fx, &format!("k{n}"), &format!("b{n}@example.com")).record;
            record.source = source.clone();
            let outcome = apply_profile_batch(&pool, two, &[record]).await.unwrap();
            let person = match &outcome[0] {
                crate::ProfileOutcome::Applied { person_id, .. } => *person_id,
                other => panic!("{other:?}"),
            };
            let record_id: Uuid = sqlx::query_scalar(
                "SELECT id FROM source_records WHERE workspace_id = $1 AND record_key = $2",
            )
            .bind(two.as_uuid())
            .bind(format!("k{n}"))
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                    source, trace_id, validation_state, source_record_id) \
                 VALUES ($1, $2, $3, 'seen', now(), now(), 'web', $4, 'valid', $5)",
            )
            .bind(Uuid::now_v7())
            .bind(two.as_uuid())
            .bind(person)
            .bind(Uuid::now_v7())
            .bind(record_id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let unit = |workspace| SweepUnit {
            workspace,
            run_id: fx.run.id,
            source: &source,
            stream: "contacts",
            job_id: fx.job.id,
            lease_token: fx.job.lease_token,
            attempt: fx.job.attempts,
            trace: &fx.trace,
            after: None,
        };

        // Aimed at the run through the other workspace, the sweep finds no run and removes
        // nothing; the guard counts only that workspace's own rows.
        assert_eq!(
            sweep_unreached(&pool, &unit(two)).await.unwrap(),
            SweepOutcome::Cancelled
        );
        assert!(
            !sweep_exceeds_limit(&pool, two, Uuid::now_v7(), "nobody", "contacts", 0.1)
                .await
                .unwrap()
        );
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(
            queue_reconcile_run(
                &mut connection,
                two,
                fx.source_id,
                fx.stream_id,
                RunTrigger::Schedule,
                None,
                true
            )
            .await
            .unwrap(),
            ReconcileQueued::Ineligible
        );
        drop(connection);

        // The real sweep removes workspace one's three records and none of workspace two's.
        let outcome = sweep_unreached(&pool, &unit(fx.ws)).await.unwrap();
        assert!(matches!(outcome, SweepOutcome::Swept { removed: 3, .. }));
        let per_workspace: Vec<(Uuid, i64, i64)> = sqlx::query_as(
            "SELECT r.workspace_id, count(*) FILTER (WHERE r.deleted_at IS NULL), \
                    (SELECT count(*) FROM events e WHERE e.workspace_id = r.workspace_id) \
             FROM source_records r GROUP BY r.workspace_id ORDER BY 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            per_workspace,
            [(fx.ws.as_uuid(), 0, 0), (two.as_uuid(), 3, 3)]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stream_offsets_are_read_written_and_seen_only_in_their_own_workspace_and_go_with_the_stream(
        pool: PgPool,
    ) {
        use crate::sync_runs::testing::{Fixture, batch, fixture};
        use crate::{
            PartitionOffset, SyncBatch, SyncLease, commit_sync_batch, delete_source, load_offsets,
            report_stream_lag, stream_freshness, stream_lag,
        };

        let fx: Fixture = fixture(&pool, "one", "stream").await;
        let two = workspace(&pool, "two").await;
        let source = fx.source_id.to_string();
        let own = [PartitionOffset {
            partition: 0,
            next: 4,
            end: Some(9),
        }];
        let cp = serde_json::json!({});
        commit_sync_batch(
            &pool,
            &SyncBatch {
                offsets: &own,
                ..batch(&fx, &source, &[], &cp)
            },
        )
        .await
        .unwrap();

        // Workspace two reads none of it, sees no lag, and its writes against the stream's
        // run find nothing to write to.
        assert!(
            load_offsets(&pool, two, fx.stream_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            stream_lag(&pool, two, fx.source_id)
                .await
                .unwrap()
                .is_empty()
        );
        let foreign_write = commit_sync_batch(
            &pool,
            &SyncBatch {
                workspace: two,
                offsets: &[PartitionOffset {
                    partition: 0,
                    next: 999,
                    end: Some(999),
                }],
                ..batch(&fx, &source, &[], &cp)
            },
        )
        .await
        .unwrap();
        assert!(matches!(foreign_write, crate::SyncBatchOutcome::Cancelled));
        let foreign_report = report_stream_lag(
            &pool,
            two,
            fx.stream_id,
            fx.run.id,
            SyncLease {
                job_id: fx.job.id,
                token: fx.job.lease_token,
            },
            &[PartitionOffset {
                partition: 0,
                next: 999,
                end: Some(999),
            }],
            0,
        )
        .await
        .unwrap();
        assert_eq!(foreign_report, crate::LagReport::Cancelled);
        let now = crate::database_now(&pool).await.unwrap();
        assert!(
            stream_freshness(&pool, Some(two), None, now)
                .await
                .unwrap()
                .is_empty()
        );
        let mine = load_offsets(&pool, fx.ws, fx.stream_id).await.unwrap();
        assert_eq!(
            mine,
            [PartitionOffset {
                partition: 0,
                next: 4,
                end: Some(9)
            }],
            "the other workspace changed nothing"
        );
        let lag = stream_freshness(&pool, Some(fx.ws), None, now)
            .await
            .unwrap();
        assert_eq!(lag[0].lag, Some(5));

        // Deleting the source deletes its streams and, with them, the offsets.
        assert!(
            delete_source(&mut pool.acquire().await.unwrap(), fx.ws, fx.source_id)
                .await
                .unwrap()
                .is_some()
        );
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM stream_offsets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn webhooks_are_read_changed_and_received_only_in_their_own_workspace(pool: PgPool) {
        use crate::webhooks::fixtures::{hook, mapping};
        use crate::{
            SecretId, WebhookUpdate, delete_source, find_for_receive, find_webhook, list_webhooks,
            replace_webhook_secret, update_webhook,
        };

        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let mine = hook(&pool, one, "orders", Some(&mapping(&[]))).await;
        let theirs = hook(&pool, two, "orders", Some(&mapping(&[]))).await;
        let mut connection = pool.acquire().await.unwrap();

        let listed = list_webhooks(&pool, one).await.unwrap();
        assert_eq!(listed.iter().map(|h| h.id).collect::<Vec<_>>(), [mine.id]);
        assert!(find_webhook(&pool, one, theirs.id).await.unwrap().is_none());
        let update = WebhookUpdate {
            dedupe_field: Some("x"),
            ..WebhookUpdate::default()
        };
        assert!(
            update_webhook(&mut connection, one, theirs.id, &update)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            replace_webhook_secret(&mut connection, one, theirs.id, SecretId::new())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            delete_source(&mut connection, one, theirs.source_id)
                .await
                .unwrap()
                .is_none()
        );
        // The receive path locates a hook by id and carries its own workspace, so a payload
        // for the other workspace's hook is queued there and nowhere else.
        let received = find_for_receive(&pool, theirs.id).await.unwrap().unwrap();
        assert_eq!(received.workspace(), two);
        let untouched = find_webhook(&pool, two, theirs.id).await.unwrap().unwrap();
        assert_eq!(untouched.dedupe_field, "id");
        assert_eq!(untouched.secret_id, theirs.secret_id);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn oauth_states_and_tokens_are_taken_and_read_only_in_their_own_workspace(pool: PgPool) {
        use zirv_flow_core::{Email, Keyring, MasterKey};

        use crate::{
            NewOAuthState, NewSource, create_oauth_state, create_source, create_user, load_tokens,
            take_oauth_state,
        };

        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let source = create_source(
            &pool,
            one,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "http",
                name: "api",
                config: &json!({}),
                credential_secret_id: None,
                credential_fields: &[],
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let user = create_user(&pool, &Email::parse("a@example.com").unwrap(), None, false)
            .await
            .unwrap()
            .id
            .as_uuid();

        // Workspace two cannot start a consent for workspace one's source.
        assert!(
            create_oauth_state(
                &pool,
                &NewOAuthState {
                    workspace: two,
                    source: source.id,
                    user,
                    state_hash: b"foreign",
                    pkce_verifier: "v",
                    binding: "b",
                    ttl_seconds: 600,
                },
            )
            .await
            .is_err()
        );
        assert!(take_oauth_state(&pool, b"foreign").await.unwrap().is_none());

        // The consent that workspace one started names workspace one, and only it.
        create_oauth_state(
            &pool,
            &NewOAuthState {
                workspace: one,
                source: source.id,
                user,
                state_hash: b"own",
                pkce_verifier: "v",
                binding: "b",
                ttl_seconds: 600,
            },
        )
        .await
        .unwrap();
        let taken = take_oauth_state(&pool, b"own").await.unwrap().unwrap();
        assert_eq!(taken.workspace_id, one);
        assert_eq!(taken.binding, "b");

        // Tokens are read through the workspace.
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        assert!(
            load_tokens(&pool, &keys, two, source.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn ai_drafts_are_found_and_accepted_only_in_their_own_workspace(pool: PgPool) {
        use crate::{
            DraftKind, DraftStatus, NewAiDraft, NewSource, create_source, create_user,
            find_ai_draft, insert_ai_draft, mark_ai_draft_accepted, purge_expired_ai_drafts,
        };
        use zirv_flow_core::Email;

        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let user = create_user(
            &pool,
            &Email::parse("ada@example.com").unwrap(),
            None,
            false,
        )
        .await
        .unwrap()
        .id;
        let config = json!({});
        let mut sources = Vec::new();
        for ws in [one, two] {
            let source = create_source(
                &pool,
                ws,
                &NewSource {
                    id: Uuid::now_v7(),
                    kind: "fake",
                    name: "crm",
                    config: &config,
                    credential_secret_id: None,
                    credential_fields: &[],
                    schedule: None,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            sources.push(source.id);
        }
        let now = datetime!(2026-05-15 12:00 UTC);
        let content = json!({"columns": []});
        let draft = |ws: WorkspaceId, source: Uuid| NewAiDraft {
            id: Uuid::now_v7(),
            workspace_id: ws,
            user_id: user,
            kind: DraftKind::Mapping,
            source_id: Some(source),
            stream_name: Some("contacts"),
            segment_id: None,
            status: DraftStatus::Valid,
            content: Some(&content),
            base_digest: None,
            expires_at: now + time::Duration::days(7),
        };
        let mut connection = pool.acquire().await.unwrap();
        let mine = draft(one, sources[0]);
        insert_ai_draft(&mut connection, &mine).await.unwrap();
        // A draft cannot point at another workspace's source.
        assert!(
            insert_ai_draft(&mut connection, &draft(one, sources[1]))
                .await
                .is_err()
        );

        assert!(
            find_ai_draft(&pool, two, user, mine.id, now)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !mark_ai_draft_accepted(&mut connection, two, user, mine.id, now)
                .await
                .unwrap()
        );
        let untouched = find_ai_draft(&pool, one, user, mine.id, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.status, DraftStatus::Valid);
        // The purge is instance-wide by design, so it takes nothing that has not expired.
        assert_eq!(
            purge_expired_ai_drafts(&mut connection, now, 10)
                .await
                .unwrap(),
            0
        );
    }
}
