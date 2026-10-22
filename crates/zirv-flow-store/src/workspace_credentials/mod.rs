//! Workspace credentials (architecture 10.1 and 16.2): named, typed credentials that sources and
//! destinations share. A credential holds its non-secret `fields` as JSON and refers to one
//! sealed secret that holds every secret field. This module stores and returns rows and sealed
//! values; it never decrypts except in [`open_credential`], which `api` and `jobs` call right
//! before use.
//!
//! Nothing here is person-scoped: no column holds a person, an address or a secret value, so the
//! erasure scan has nothing to look for. Every index leads with `workspace_id` (migration 0006).

mod crud;
mod open;
mod usage;

#[cfg(test)]
mod backfill;
#[cfg(test)]
pub(crate) use crud::testing;

pub use crud::{
    ArchiveOutcome, CredentialUpdate, NewWorkspaceCredential, WorkspaceCredential,
    archive_credential, create_credential, find_credential, find_credential_for_update,
    list_credentials, lock_credential_for_use, record_credential_test, swap_credential_secret,
    update_credential,
};
pub use open::{OpenedCredential, open_credential};
pub use usage::{CredentialUser, credential_use_counts, credential_users};
