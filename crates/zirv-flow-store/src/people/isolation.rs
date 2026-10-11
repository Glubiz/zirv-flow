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

    use crate::create_workspace;
    use crate::people::{
        NewTraitDefinition, ProfileOutcome, ProfileRecord, TraitDefinitionUpdate,
        apply_profile_batch, create_trait_definition, find_person, find_person_by_identifier,
        find_trait_definition, list_identities, list_identity_conflicts, list_trait_definitions,
        update_trait_definition,
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

    /// Seeds the same shape in a workspace: a person with an email, a second one with a
    /// phone, a record that touches both (a conflict) and a registry definition.
    async fn seed(pool: &PgPool, ws: WorkspaceId) -> Uuid {
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
                record("1", vec![("email", "a@example.com")]),
                record("2", vec![("phone", "+4512345678")]),
                record(
                    "3",
                    vec![("email", "a@example.com"), ("phone", "+4512345678")],
                ),
            ],
        )
        .await
        .unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        person_id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_read_returns_only_its_own_workspaces_rows(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let person_one = seed(&pool, one).await;
        let person_two = seed(&pool, two).await;
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
            1
        );

        // Conflicts.
        for (ws, own, foreign) in [(one, person_one, person_two), (two, person_two, person_one)] {
            let conflicts = list_identity_conflicts(&pool, ws, own).await.unwrap();
            assert_eq!(conflicts.len(), 1);
            assert_ne!(conflicts[0].other_person_id, foreign);
            assert!(
                list_identity_conflicts(&pool, ws, foreign)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
