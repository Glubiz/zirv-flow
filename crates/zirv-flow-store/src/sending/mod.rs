//! Sending domains and sender identities (architecture 10.2 and 16.2): the domains a workspace
//! sends from, the DNS records each must publish and what the last check found, and the sender
//! identities (from name and address) bound to a domain.
//!
//! A domain's DKIM private key is a sealed secret in `encrypted_secrets`, referenced by id; this
//! module stores the reference and the public half, and never sees a plaintext. A domain is
//! `verified` only while its DKIM, SPF and DMARC records are all `ok`; [`record_domain_check`]
//! is the one place that changes that, and it appends a decision when the status changes.
//! [`require_verified_sender`] is the shared refusal template publish and sends use.
//!
//! None of these tables is person-scoped: `sender_identities.from_address` is the address of a
//! sender the workspace owns, never a contact's, so the erasure scan has nothing to look for in
//! them.

mod domains;
mod identities;
mod sender;
mod verify;

pub use domains::{
    DomainDelete, DomainRecord, NewSendingDomain, SendingDomain, create_sending_domain,
    delete_sending_domain, due_sending_domains, find_sending_domain, list_domain_records,
    list_sending_domains, list_workspace_domain_records,
};
pub use identities::{
    IdentityError, NewSenderIdentity, SenderIdentity, SenderIdentityPatch, archive_sender_identity,
    create_sender_identity, create_sender_identity_in, find_sender_identity,
    list_sender_identities, update_sender_identity,
};
pub use sender::{SenderError, VerifiedSender, require_verified_sender};
pub use verify::{
    DomainCheck, RecordFinding, SENDING_DOMAIN_VERIFY_KIND, enqueue_domain_verify,
    record_domain_check,
};

/// Fixtures for the tests of this module and, behind `test-support`, of the crates above it.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::sending::{RecordKind, RecordStatus};
    use zirv_flow_core::{
        Cause, Email, Sealed, SecretId, SecretPurpose, TraceContext, WorkspaceId,
    };

    use super::{
        NewSenderIdentity, NewSendingDomain, RecordFinding, create_sender_identity,
        create_sending_domain, record_domain_check,
    };
    use crate::{StoreError, secrets};

    /// A stand-in public key: a fixture domain never signs, so the key need not be real.
    pub const FIXTURE_PUBLIC_KEY: &str =
        "TUlJQmlqQU5CZ2txaGtpRzl3MEJBUUVGQUFPQ0FROEFNSUlCQ2dLQ0FRRUE=";

    /// Plants a sending domain on `domain` with a placeholder sealed key. It starts unverified.
    pub async fn plant_domain(
        pool: &PgPool,
        workspace: WorkspaceId,
        domain: &str,
    ) -> Result<Uuid, StoreError> {
        let secret = SecretId::new();
        let sealed = Sealed {
            key_id: "fixture".to_owned(),
            wrapped_key: vec![0],
            nonce: [0; 24],
            ciphertext: vec![0],
        };
        secrets::insert_unchecked(pool, workspace, SecretPurpose::DkimKey, secret, &sealed).await?;
        let id = Uuid::now_v7();
        let mut connection = pool.acquire().await.map_err(StoreError::query)?;
        create_sending_domain(
            &mut connection,
            workspace,
            &NewSendingDomain {
                id,
                domain,
                selector: "zirv",
                dkim_secret_id: secret,
                dkim_public_key: FIXTURE_PUBLIC_KEY,
                spf_mechanism: None,
                created_by: None,
            },
        )
        .await?;
        Ok(id)
    }

    /// Marks every record of the domain `ok`, which verifies it.
    pub async fn verify_domain(
        pool: &PgPool,
        workspace: WorkspaceId,
        domain: Uuid,
    ) -> Result<(), StoreError> {
        let findings: Vec<RecordFinding> = RecordKind::ALL
            .into_iter()
            .map(|kind| RecordFinding {
                kind,
                status: RecordStatus::Ok,
                observed: Vec::new(),
            })
            .collect();
        let mut connection = pool.acquire().await.map_err(StoreError::query)?;
        record_domain_check(
            &mut connection,
            workspace,
            domain,
            &findings,
            &TraceContext::root(Cause::definition_change(domain)),
            None,
        )
        .await?;
        Ok(())
    }

    /// Plants a sender identity `name@domain` on a domain; returns its id.
    pub async fn plant_identity(
        pool: &PgPool,
        workspace: WorkspaceId,
        domain: Uuid,
        address: &str,
    ) -> Result<Uuid, StoreError> {
        plant_identity_as(pool, workspace, domain, Uuid::now_v7(), address).await
    }

    /// As [`plant_identity`], with the id given, so a test can name it in a document.
    pub async fn plant_identity_as(
        pool: &PgPool,
        workspace: WorkspaceId,
        domain: Uuid,
        id: Uuid,
        address: &str,
    ) -> Result<Uuid, StoreError> {
        let address = Email::parse(address).map_err(|_| StoreError::poison())?;
        create_sender_identity(
            pool,
            workspace,
            &NewSenderIdentity {
                id,
                domain_id: domain,
                from_name: "Sender",
                from_address: &address,
                reply_to: None,
                created_by: None,
            },
        )
        .await
        .map_err(|_| StoreError::poison())?;
        Ok(id)
    }

    /// Plants a verified domain and a sender identity on it; returns the identity id.
    pub async fn plant_verified_sender(
        pool: &PgPool,
        workspace: WorkspaceId,
        domain: &str,
    ) -> Result<Uuid, StoreError> {
        let domain_id = plant_domain(pool, workspace, domain).await?;
        verify_domain(pool, workspace, domain_id).await?;
        plant_identity(pool, workspace, domain_id, &format!("news@{domain}")).await
    }

    /// Plants a verified domain and a sender identity with the id given on it.
    pub async fn plant_verified_sender_as(
        pool: &PgPool,
        workspace: WorkspaceId,
        domain: &str,
        id: Uuid,
    ) -> Result<Uuid, StoreError> {
        let domain_id = plant_domain(pool, workspace, domain).await?;
        verify_domain(pool, workspace, domain_id).await?;
        plant_identity_as(pool, workspace, domain_id, id, &format!("news@{domain}")).await
    }
}
