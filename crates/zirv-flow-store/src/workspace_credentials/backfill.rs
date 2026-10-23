//! The tests of migration 0047's backfill: a source and a destination that sealed a credential of
//! their own before workspace credentials existed keep working, referenced in place. Nothing is
//! re-sealed: the credential points at the same `encrypted_secrets` row with its old purpose.

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::{Keyring, MasterKey, SecretPurpose};

    use crate::workspace_credentials::testing::{secret, workspace};
    use crate::{credential_users, find_destination, find_source, open_credential};

    /// Applies every migration older than 0047, in order.
    async fn migrate_before_credentials(pool: &PgPool) {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut files: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "sql"))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name < "0047")
            })
            .collect();
        files.sort();
        assert!(files.len() >= 40, "{}", files.len());
        for file in files {
            let sql = std::fs::read_to_string(&file).unwrap();
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(pool)
                .await
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        }
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_existing_source_and_destination_credential_still_opens_referenced_in_place(
        pool: PgPool,
    ) {
        migrate_before_credentials(&pool).await;
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let source_secret = secret(
            &pool,
            &keys,
            ws,
            SecretPurpose::ConnectorCredential,
            br#"{"token":"source-token"}"#,
        )
        .await;
        let destination_secret = secret(
            &pool,
            &keys,
            ws,
            SecretPurpose::DestinationCredential,
            br#"{"token":"destination-token"}"#,
        )
        .await;
        let (source_id, destination_id, bare_id) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, name, secret_id, fields) in [
            (source_id, "crm", Some(source_secret), vec!["token"]),
            (bare_id, "bare", None, vec![]),
        ] {
            sqlx::query(
                "INSERT INTO sources (id, workspace_id, kind, name, config, credential_secret_id, \
                    credential_fields) VALUES ($1, $2, 'fake', $3, '{}', $4, $5)",
            )
            .bind(id)
            .bind(ws.as_uuid())
            .bind(name)
            .bind(secret_id.map(|id| id.as_uuid()))
            .bind(fields.into_iter().map(String::from).collect::<Vec<_>>())
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO destinations (id, workspace_id, kind, name, config, \
                credential_secret_id, credential_fields) \
             VALUES ($1, $2, 'fake', 'mail', '{}', $3, '{token}')",
        )
        .bind(destination_id)
        .bind(ws.as_uuid())
        .bind(destination_secret.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!(
            "../../migrations/0047_workspace_credentials.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let source = find_source(&pool, ws, source_id).await.unwrap().unwrap();
        let destination = find_destination(&pool, ws, destination_id)
            .await
            .unwrap()
            .unwrap();
        let bare = find_source(&pool, ws, bare_id).await.unwrap().unwrap();
        assert_eq!(source.credential_secret_id, Some(source_secret.as_uuid()));
        assert_eq!(
            destination.credential_secret_id,
            Some(destination_secret.as_uuid())
        );
        assert_eq!(source.credential_fields, ["token"]);
        assert_eq!(destination.credential_fields, ["token"]);
        assert!(bare.credential_id.is_none() && bare.credential_fields.is_empty());

        let opened = open_credential(&pool, &keys, ws, source.credential_id.unwrap())
            .await
            .unwrap();
        assert_eq!(opened.secret.expose(), br#"{"token":"source-token"}"#);
        assert_eq!(opened.fields, json!({}));
        let opened = open_credential(&pool, &keys, ws, destination.credential_id.unwrap())
            .await
            .unwrap();
        assert_eq!(opened.secret.expose(), br#"{"token":"destination-token"}"#);

        let rows: Vec<(String, String, Option<String>, serde_json::Value)> = sqlx::query_as(
            "SELECT c.kind, s.purpose, c.fingerprint, c.hints FROM credentials c \
             JOIN encrypted_secrets s ON s.id = c.secret_id ORDER BY s.purpose",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                (
                    "imported".to_owned(),
                    "connector_credential".to_owned(),
                    None,
                    json!({"token": ""})
                ),
                (
                    "imported".to_owned(),
                    "destination_credential".to_owned(),
                    None,
                    json!({"token": ""})
                ),
            ]
        );
        let users = credential_users(&pool, ws, source.credential_id.unwrap())
            .await
            .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!((users[0].kind.as_str(), users[0].id), ("source", source_id));
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn owners_whose_names_differ_only_in_case_or_after_the_hundredth_character_and_whose_ids_share_a_prefix_all_get_a_credential(
        pool: PgPool,
    ) {
        migrate_before_credentials(&pool).await;
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let long = "x".repeat(110);
        // Ids that agree on their first 8 hex characters, as UUIDv7s made together do.
        let ids = [
            "01a166e4-0000-7000-8000-000000000001",
            "01a166e4-0000-7000-8000-000000000002",
            "01a166e4-0000-7000-8000-000000000003",
            "01a166e4-0000-7000-8000-000000000004",
        ];
        let names = ["CRM", "crm", &format!("{long}a"), &format!("{long}b")];
        for (id, name) in ids.iter().zip(names) {
            let secret_id = secret(
                &pool,
                &keys,
                ws,
                SecretPurpose::ConnectorCredential,
                b"{\"token\":\"t\"}",
            )
            .await;
            sqlx::query(
                "INSERT INTO sources (id, workspace_id, kind, name, config,                     credential_secret_id, credential_fields)                  VALUES ($1::uuid, $2, 'fake', $3, '{}', $4, '{token}')",
            )
            .bind(id)
            .bind(ws.as_uuid())
            .bind(name)
            .bind(secret_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        }

        sqlx::raw_sql(include_str!(
            "../../migrations/0047_workspace_credentials.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM credentials")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(names.len(), 4);
        assert!(
            names.iter().all(|name| name.chars().count() <= 128),
            "{names:?}"
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_database_source_credential_is_named_for_username_and_still_opens_with_its_sealed_user(
        pool: PgPool,
    ) {
        migrate_before_credentials(&pool).await;
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        for (kind, name) in [
            ("postgres", "warehouse"),
            ("mysql", "shop"),
            ("fake", "other"),
        ] {
            let secret_id = secret(
                &pool,
                &keys,
                ws,
                SecretPurpose::ConnectorCredential,
                br#"{"user":"reader","password":"pw"}"#,
            )
            .await;
            let fields: Vec<String> = if kind == "fake" {
                vec!["token".to_owned()]
            } else {
                vec!["user".to_owned(), "password".to_owned()]
            };
            sqlx::query(
                "INSERT INTO sources (id, workspace_id, kind, name, config, \
                    credential_secret_id, credential_fields) \
                 VALUES ($1, $2, $3, $4, '{}', $5, $6)",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(kind)
            .bind(name)
            .bind(secret_id.as_uuid())
            .bind(fields)
            .execute(&pool)
            .await
            .unwrap();
        }

        sqlx::raw_sql(include_str!(
            "../../migrations/0047_workspace_credentials.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let rows: Vec<(String, serde_json::Value, Uuid)> = sqlx::query_as(
            "SELECT s.kind, c.hints, c.id FROM sources s JOIN credentials c ON c.id = s.credential_id \
             ORDER BY s.name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let kinds: Vec<_> = rows.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(kinds, ["fake", "mysql", "postgres"]);
        assert_eq!(rows[0].1, json!({"token": ""}));
        for row in &rows[1..] {
            assert_eq!(row.1, json!({"username": "", "password": ""}));
            let opened = open_credential(&pool, &keys, ws, row.2).await.unwrap();
            assert_eq!(
                opened.secret.expose(),
                br#"{"user":"reader","password":"pw"}"#
            );
        }
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_old_credential_columns_are_dropped_and_every_workspace_gets_its_own_credentials(
        pool: PgPool,
    ) {
        migrate_before_credentials(&pool).await;
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        for (ws, name) in [(one, "same"), (two, "same")] {
            let secret_id = secret(
                &pool,
                &keys,
                ws,
                SecretPurpose::ConnectorCredential,
                b"{\"token\":\"t\"}",
            )
            .await;
            sqlx::query(
                "INSERT INTO sources (id, workspace_id, kind, name, config, \
                    credential_secret_id, credential_fields) \
                 VALUES ($1, $2, 'fake', $3, '{}', $4, '{token}')",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(name)
            .bind(secret_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        }

        sqlx::raw_sql(include_str!(
            "../../migrations/0047_workspace_credentials.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let dropped: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('sources', 'destinations') \
               AND column_name IN ('credential_secret_id', 'credential_fields')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let per_workspace: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT workspace_id, count(*) FROM credentials GROUP BY workspace_id \
             ORDER BY workspace_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(per_workspace.len(), 2);
        assert!(per_workspace.iter().all(|(_, count)| *count == 1));
    }
}
