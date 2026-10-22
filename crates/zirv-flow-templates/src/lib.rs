//! The pure renderer for code-mode email and webhook content (architecture 11, ADR-006).
//!
//! Preview (`api`) and send (`jobs`) call the same functions, so what the marketer previews is
//! what is sent. The crate does no I/O, reads no clock and draws no random numbers: equal inputs
//! give equal bytes. It never signs or hashes anything; the click-tracking token function is
//! passed in by the caller.
//!
//! - [`render_email`] and [`render_webhook`] render a [`TemplateDocument`] against a
//!   [`RenderContext`] inside a sandbox (strict undefined, fuel, recursion and output limits,
//!   HTML autoescape in HTML fields only).
//! - [`validate`] reports what is wrong with a document as [`Issue`]s with fixed codes.
//! - [`RenderRequest`] and [`RenderResponse`] are the wire shape of an isolated render (11.9).
//! - [`rewrite_links`] and [`unsubscribe_headers`] are the pure post-render steps.

mod callables;
mod context;
mod document;
mod error;
mod issue;
mod links;
mod render;
mod sandbox;
mod validate;
mod wire;

pub use context::{MessageContext, RenderContext, SystemValues};
pub use document::{
    Category, Channel, EmailContent, Mode, SCHEMA_VERSION, TemplateDocument, WebhookContent,
};
pub use error::RenderError;
pub use issue::{FieldCatalog, Issue, Severity, code, severity_of};
pub use links::{Hooks, INERT_TOKEN, Tracking, rewrite_links, unsubscribe_headers};
pub use render::{Rendered, RenderedWebhook, apply_tracking, render_email, render_webhook};
pub use sandbox::{
    FUEL, MAX_HEADER_COUNT, MAX_HEADER_VALUE_BYTES, MAX_HEADERS_BYTES, MAX_HTML_BYTES,
    MAX_SOURCE_BYTES, MAX_SUBJECT_BYTES, MAX_TEXT_BYTES, MAX_WEBHOOK_BODY_BYTES, RECURSION_LIMIT,
};
pub use validate::validate;
pub use wire::{RenderRequest, RenderResponse};
