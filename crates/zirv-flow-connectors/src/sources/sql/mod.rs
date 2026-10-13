//! What the PostgreSQL and MySQL sources share: the settings, the egress and TLS rules at the
//! connect point, the keyset checkpoint, error mapping and metrics. Not a registry: each source
//! type is still one module and one line in [`super::ALL`].

pub(crate) mod connect;
pub(crate) mod error;
pub(crate) mod keyset;
pub(crate) mod metrics;
pub(crate) mod settings;
pub(crate) mod tls;
