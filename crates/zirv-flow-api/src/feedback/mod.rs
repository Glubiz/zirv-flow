//! `POST /api/v1/destinations/{id}/feedback`: the public edge of an email provider's feedback
//! webhook (architecture 10.2, #223). The provider posts a delivery, a bounce or a complaint for a
//! message it was given; the destination's manifest says how to read the event and how it is
//! signed.
//!
//! The URL id only locates the destination; the signature authorizes. The order is fixed:
//!
//! 1. the body cap (declared, then streamed),
//! 2. the signature, with the destination's own signing secret, **before the body is parsed**,
//!    and a bad or missing signature or a stale timestamp is a 401 that writes nothing,
//! 3. only then the JSON, the event type and the message id.
//!
//! A message that does not exist, belongs to another workspace than the destination's, or was sent
//! through another destination is dropped with a 202 and counted. No log line, metric label or
//! problem body holds the payload, a header value, a signature, a secret, an address or an id.

pub(crate) mod receive;

#[cfg(test)]
mod acceptance;
