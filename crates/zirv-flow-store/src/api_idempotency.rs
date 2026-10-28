//! Idempotency keys of the creating requests that nothing else guards (architecture section 15).
//!
//! A request reserves its key in a short transaction of its own and then holds nothing: the row is
//! `pending` with a lease. The create marks the row `done` inside its own transaction, with
//! `UPDATE ... WHERE reservation = mine`; when no row matches, the reservation was taken over and
//! the create rolls back. A pending row whose lease ran out may be taken over by a retry. The row
//! is identified by workspace, route and the hash of the caller and the key.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// Keys are remembered for 24 hours, like a contact's.
const KEY_HOURS: i32 = 24;

/// The identity of a remembered key.
#[derive(Clone, Copy)]
pub struct ApiIdempotencyKey<'a> {
    pub workspace: WorkspaceId,
    pub route: &'a str,
    pub key_hash: &'a [u8],
}

/// What reserving a key found.
#[derive(Debug, PartialEq)]
pub enum Reserved {
    /// The key is free, or its lease had run out: the request owns this reservation.
    Fresh(Uuid),
    /// An earlier request created the resource.
    Done {
        /// The earlier request's hash.
        request_hash: Vec<u8>,
        resource_id: Uuid,
    },
    /// Another request holds the key and its lease has not run out.
    InFlight {
        /// Whole seconds until the lease runs out, at least 1.
        retry_after: u64,
    },
    /// The key was first used for another request.
    Reused,
}

#[derive(sqlx::FromRow)]
struct Row {
    state: String,
    request_hash: Option<Vec<u8>>,
    resource_id: Option<Uuid>,
    expired: bool,
    seconds_left: i64,
}

/// Reserves the key for a request, in a transaction of its own that ends before this returns.
/// `request_hash` is `None` for an upload that is hashed while it streams; the caller then binds
/// the hash with [`bind_api_idempotency_hash`] and compares a [`Reserved::Done`] itself.
pub async fn reserve_api_idempotency(
    pool: &PgPool,
    key: ApiIdempotencyKey<'_>,
    request_hash: Option<&[u8]>,
    lease_seconds: i64,
) -> Result<Reserved, StoreError> {
    let mut tx = crate::begin(pool).await?;
    sqlx::query(
        "DELETE FROM api_idempotency WHERE workspace_id = $1 AND route = $2 AND key_hash = $3 \
           AND created_at <= now() - make_interval(hours => $4)",
    )
    .bind(key.workspace.as_uuid())
    .bind(key.route)
    .bind(key.key_hash)
    .bind(KEY_HOURS)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let reservation = Uuid::now_v7();
    for _ in 0..3 {
        let inserted = sqlx::query(
            "INSERT INTO api_idempotency \
           (workspace_id, route, key_hash, state, reservation, lease_until, request_hash) \
         VALUES ($1, $2, $3, 'pending', $4, now() + make_interval(secs => $5), $6) \
         ON CONFLICT DO NOTHING",
        )
        .bind(key.workspace.as_uuid())
        .bind(key.route)
        .bind(key.key_hash)
        .bind(reservation)
        .bind(lease_seconds as f64)
        .bind(request_hash)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        if inserted == 1 {
            crate::commit(tx).await?;
            return Ok(Reserved::Fresh(reservation));
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT state, request_hash, resource_id, lease_until <= now() AS expired, \
                ceil(extract(epoch FROM lease_until - now()))::bigint AS seconds_left \
         FROM api_idempotency WHERE workspace_id = $1 AND route = $2 AND key_hash = $3 FOR UPDATE",
        )
        .bind(key.workspace.as_uuid())
        .bind(key.route)
        .bind(key.key_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        // A release or expiry sweep can delete the conflict before this statement sees it.
        let Some(row) = row else {
            continue;
        };
        let pending = row.state == "pending";
        // A pending row whose lease ran out never created anything, so a retry may differ from it.
        let takeover = pending && row.expired;
        if !takeover
            && let (Some(first), Some(now)) = (&row.request_hash, request_hash)
            && first.as_slice() != now
        {
            return Ok(Reserved::Reused);
        }
        if takeover {
            sqlx::query(
            "UPDATE api_idempotency SET reservation = $4, request_hash = $5, created_at = now(), \
               lease_until = now() + make_interval(secs => $6) \
             WHERE workspace_id = $1 AND route = $2 AND key_hash = $3",
        )
        .bind(key.workspace.as_uuid())
        .bind(key.route)
        .bind(key.key_hash)
        .bind(reservation)
        .bind(request_hash)
        .bind(lease_seconds as f64)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
            crate::commit(tx).await?;
            return Ok(Reserved::Fresh(reservation));
        }
        if pending {
            return Ok(Reserved::InFlight {
                retry_after: u64::try_from(row.seconds_left).unwrap_or(1).max(1),
            });
        }
        return match (row.request_hash, row.resource_id) {
            (Some(request_hash), Some(resource_id)) => Ok(Reserved::Done {
                request_hash,
                resource_id,
            }),
            _ => Err(StoreError::poison()),
        };
    }
    Ok(Reserved::InFlight { retry_after: 1 })
}

/// Sets the request hash of a reservation made without one. `false` when the reservation is no
/// longer the request's.
pub async fn bind_api_idempotency_hash(
    pool: &PgPool,
    key: ApiIdempotencyKey<'_>,
    reservation: Uuid,
    request_hash: &[u8],
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE api_idempotency SET request_hash = $5 \
         WHERE workspace_id = $1 AND route = $2 AND key_hash = $3 AND reservation = $4 \
           AND state = 'pending'",
    )
    .bind(key.workspace.as_uuid())
    .bind(key.route)
    .bind(key.key_hash)
    .bind(reservation)
    .bind(request_hash)
    .execute(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// Marks the reservation done, inside the create's own transaction. `false` when the reservation
/// is no longer the request's: the caller rolls the create back.
pub async fn complete_api_idempotency(
    connection: &mut PgConnection,
    key: ApiIdempotencyKey<'_>,
    reservation: Uuid,
    status: u16,
    resource_id: Uuid,
    response_hash: &[u8],
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE api_idempotency SET state = 'done', status = $5, resource_id = $6, response_hash = $7, \
           created_at = now() \
         WHERE workspace_id = $1 AND route = $2 AND key_hash = $3 AND reservation = $4 \
           AND state = 'pending' AND request_hash IS NOT NULL",
    )
    .bind(key.workspace.as_uuid())
    .bind(key.route)
    .bind(key.key_hash)
    .bind(reservation)
    .bind(i16::try_from(status).unwrap_or(200))
    .bind(resource_id)
    .bind(response_hash)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// Deletes a reservation that did not end in a create. Another request's reservation, and a done
/// row, are left alone.
pub async fn release_api_idempotency(
    pool: &PgPool,
    key: ApiIdempotencyKey<'_>,
    reservation: Uuid,
) -> Result<(), StoreError> {
    sqlx::query(
        "DELETE FROM api_idempotency \
         WHERE workspace_id = $1 AND route = $2 AND key_hash = $3 AND reservation = $4 \
           AND state = 'pending'",
    )
    .bind(key.workspace.as_uuid())
    .bind(key.route)
    .bind(key.key_hash)
    .bind(reservation)
    .execute(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Forgets idempotency keys older than 24 hours. Returns how many.
pub async fn expire_api_idempotency(pool: &PgPool) -> Result<u64, StoreError> {
    let done = sqlx::query(
        "DELETE FROM api_idempotency WHERE created_at <= now() - make_interval(hours => $1)",
    )
    .bind(KEY_HOURS)
    .execute(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, name: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(name).unwrap();
        create_workspace(pool, &name, &Slug::from_name(&name).unwrap())
            .await
            .unwrap()
            .id
    }

    fn key(workspace: WorkspaceId, hash: &[u8]) -> ApiIdempotencyKey<'_> {
        ApiIdempotencyKey {
            workspace,
            route: "create_x",
            key_hash: hash,
        }
    }

    async fn fresh(pool: &PgPool, key: ApiIdempotencyKey<'_>, hash: &[u8]) -> Uuid {
        match reserve_api_idempotency(pool, key, Some(hash), 60)
            .await
            .unwrap()
        {
            Reserved::Fresh(reservation) => reservation,
            other => panic!("expected a fresh reservation, got {other:?}"),
        }
    }

    async fn complete(pool: &PgPool, key: ApiIdempotencyKey<'_>, reservation: Uuid) -> Uuid {
        let resource = Uuid::now_v7();
        let mut tx = begin(pool).await.unwrap();
        assert!(
            complete_api_idempotency(&mut tx, key, reservation, 201, resource, b"response")
                .await
                .unwrap()
        );
        commit(tx).await.unwrap();
        resource
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_done_key_answers_with_its_resource_and_another_route_is_another_key(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let reservation = fresh(&pool, key(ws, b"k"), b"request").await;
        let resource = complete(&pool, key(ws, b"k"), reservation).await;

        let again = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"request"), 60)
            .await
            .unwrap();
        let other_route = reserve_api_idempotency(
            &pool,
            ApiIdempotencyKey {
                route: "create_y",
                ..key(ws, b"k")
            },
            Some(b"request"),
            60,
        )
        .await
        .unwrap();

        assert_eq!(
            again,
            Reserved::Done {
                request_hash: b"request".to_vec(),
                resource_id: resource
            }
        );
        assert!(matches!(other_route, Reserved::Fresh(_)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_request_under_a_pending_or_done_key_is_reused(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let reservation = fresh(&pool, key(ws, b"k"), b"one").await;

        let pending = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"two"), 60)
            .await
            .unwrap();
        complete(&pool, key(ws, b"k"), reservation).await;
        let done = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"two"), 60)
            .await
            .unwrap();

        assert_eq!(pending, Reserved::Reused);
        assert_eq!(done, Reserved::Reused);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_pending_key_is_in_flight_with_the_seconds_left(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        fresh(&pool, key(ws, b"k"), b"one").await;

        let overlap = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"one"), 60)
            .await
            .unwrap();

        match overlap {
            Reserved::InFlight { retry_after } => assert!((1..=60).contains(&retry_after)),
            other => panic!("expected in flight, got {other:?}"),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_conflicting_row_deleted_before_the_read_is_reserved_again(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let first = fresh(&pool, key(ws, b"k"), b"one").await;
        // A statement trigger runs even when ON CONFLICT inserts nothing: delete the first
        // reservation after that conflict and before the read, without scheduling a race.
        sqlx::query(
            "CREATE FUNCTION release_conflict() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
             DELETE FROM api_idempotency WHERE request_hash = convert_to('one', 'UTF8'); \
             RETURN NULL; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER release_conflict AFTER INSERT ON api_idempotency \
             FOR EACH STATEMENT EXECUTE FUNCTION release_conflict()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let second = fresh(&pool, key(ws, b"k"), b"two").await;
        let overlap = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"two"), 60)
            .await
            .unwrap();

        assert_ne!(first, second);
        assert!(matches!(overlap, Reserved::InFlight { .. }));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn three_missing_rows_answer_in_flight(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        sqlx::query("CREATE SEQUENCE reservation_attempts")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE FUNCTION skip_reservation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
             PERFORM nextval('reservation_attempts'); RETURN NULL; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER skip_reservation BEFORE INSERT ON api_idempotency \
             FOR EACH ROW EXECUTE FUNCTION skip_reservation()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let reserved = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"request"), 60)
            .await
            .unwrap();
        let attempts: i64 = sqlx::query_scalar("SELECT last_value FROM reservation_attempts")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(reserved, Reserved::InFlight { retry_after: 1 });
        assert_eq!(attempts, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_pending_key_is_taken_over_and_the_old_owner_cannot_complete(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let first = fresh(&pool, key(ws, b"k"), b"one").await;
        sqlx::query("UPDATE api_idempotency SET lease_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();

        let second = fresh(&pool, key(ws, b"k"), b"one").await;
        let mut tx = begin(&pool).await.unwrap();
        let stale = complete_api_idempotency(
            &mut tx,
            key(ws, b"k"),
            first,
            201,
            Uuid::now_v7(),
            b"response",
        )
        .await
        .unwrap();
        let current = complete_api_idempotency(
            &mut tx,
            key(ws, b"k"),
            second,
            201,
            Uuid::now_v7(),
            b"response",
        )
        .await
        .unwrap();

        assert_ne!(first, second);
        assert!(!stale);
        assert!(current);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_released_key_is_free_and_release_leaves_done_rows_and_other_owners_alone(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "Acme").await;
        let first = fresh(&pool, key(ws, b"k"), b"one").await;
        release_api_idempotency(&pool, key(ws, b"k"), Uuid::now_v7())
            .await
            .unwrap();
        let still_held = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"one"), 60)
            .await
            .unwrap();
        release_api_idempotency(&pool, key(ws, b"k"), first)
            .await
            .unwrap();
        let second = fresh(&pool, key(ws, b"k"), b"one").await;
        complete(&pool, key(ws, b"k"), second).await;
        release_api_idempotency(&pool, key(ws, b"k"), second)
            .await
            .unwrap();

        let done = reserve_api_idempotency(&pool, key(ws, b"k"), Some(b"one"), 60)
            .await
            .unwrap();

        assert!(matches!(still_held, Reserved::InFlight { .. }));
        assert!(matches!(done, Reserved::Done { .. }));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reservation_without_a_hash_binds_it_before_it_can_complete(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let Reserved::Fresh(reservation) = reserve_api_idempotency(&pool, key(ws, b"k"), None, 60)
            .await
            .unwrap()
        else {
            panic!("expected a fresh reservation");
        };
        let mut tx = begin(&pool).await.unwrap();
        let early = complete_api_idempotency(
            &mut tx,
            key(ws, b"k"),
            reservation,
            201,
            Uuid::now_v7(),
            b"response",
        )
        .await
        .unwrap();
        drop(tx);

        let bound = bind_api_idempotency_hash(&pool, key(ws, b"k"), reservation, b"file")
            .await
            .unwrap();
        let stranger = bind_api_idempotency_hash(&pool, key(ws, b"k"), Uuid::now_v7(), b"x")
            .await
            .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        let late = complete_api_idempotency(
            &mut tx,
            key(ws, b"k"),
            reservation,
            201,
            Uuid::now_v7(),
            b"response",
        )
        .await
        .unwrap();

        assert!(!early);
        assert!(bound);
        assert!(!stranger);
        assert!(late);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_forgets_keys_older_than_a_day(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let old = fresh(&pool, key(ws, b"old"), b"one").await;
        complete(&pool, key(ws, b"old"), old).await;
        let new = fresh(&pool, key(ws, b"new"), b"one").await;
        complete(&pool, key(ws, b"new"), new).await;
        sqlx::query(
            "UPDATE api_idempotency SET created_at = now() - interval '25 hours' \
             WHERE key_hash = $1",
        )
        .bind(b"old".as_slice())
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(expire_api_idempotency(&pool).await.unwrap(), 1);

        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM api_idempotency")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1);
    }
}
