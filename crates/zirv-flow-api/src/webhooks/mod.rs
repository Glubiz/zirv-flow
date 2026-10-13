//! Inbound webhooks (architecture sections 7.1 and 16.1): the public receive endpoint, and the
//! admin operations that create, change, rotate and delete a hook. A hook is a source of kind
//! `webhook`; its mapping, staleness threshold and enabling go through the stream routes.

pub(crate) mod ops;
pub(crate) mod receive;

#[cfg(test)]
mod acceptance;
