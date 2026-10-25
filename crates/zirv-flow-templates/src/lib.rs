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
//! - [`blocks_to_mjml`] turns a block document into MJML code, merge tags kept.
//! - [`validate`] reports what is wrong with a document as [`Issue`]s with fixed codes.
//! - [`RenderRequest`] and [`RenderResponse`] are the wire shape of an isolated render (11.9).
//! - [`rewrite_links`] and [`unsubscribe_headers`] are the pure post-render steps.

mod callables;
mod content;
mod context;
mod document;
mod error;
mod issue;
mod links;
mod render;
mod sandbox;
mod utm;
mod validate;
mod wire;

pub use content::blocks::{
    Align, Block, BlocksContent, BlocksError, Button, ConvertError, Converted, Divider,
    FONT_STACKS, Heading, Image, MAX_BLOCKS, MAX_COLUMNS, MAX_CONTENT_WIDTH, MAX_DOCUMENT_BYTES,
    MAX_ID_BYTES, MAX_IMAGE_WIDTH, MAX_LABEL_BYTES, MAX_RAW_HTML_BYTES, MAX_RICH_TEXT_BYTES,
    MAX_SECTIONS, MAX_SPACER_HEIGHT, MAX_SUBJECT_SOURCE_BYTES, MAX_URL_BYTES, MIN_CONTENT_WIDTH,
    PREHEADER_DISPLAY_CHARS, RawHtml, RichText, SUBJECT_DISPLAY_CHARS, Section, Settings, Spacer,
    blocks_to_mjml, parse_template_document,
};
pub use content::{
    ContentError, RenderKind, TemplateContent, content_type, content_type_ids, content_type_of,
    validate_document,
};
pub use context::{
    MessageContext, RESERVED_EMAIL, RenderContext, SAMPLE_EMAIL, SAMPLE_PREFERENCES_URL,
    SAMPLE_UNSUBSCRIBE_URL, SystemValues,
};
pub use document::{
    Category, Channel, EmailContent, Mode, SCHEMA_VERSION, Syntax, TemplateDocument, WebhookContent,
};
pub use error::RenderError;
pub use issue::{FieldCatalog, Issue, Severity, code, severity_of};
pub use links::{Hooks, INERT_TOKEN, Tracking, rewrite_links, unsubscribe_headers};
pub use render::{Rendered, RenderedWebhook, apply_tracking, render_email, render_webhook};
pub use sandbox::{
    FUEL, MAX_HEADER_COUNT, MAX_HEADER_VALUE_BYTES, MAX_HEADERS_BYTES, MAX_HTML_BYTES,
    MAX_SOURCE_BYTES, MAX_SUBJECT_BYTES, MAX_TEXT_BYTES, MAX_WEBHOOK_BODY_BYTES, RECURSION_LIMIT,
};
pub use utm::{
    MAX_DOMAINS, MAX_PATTERN_BYTES, PLACEHOLDERS, PatternError, UtmPlan, UtmTags, UtmValues,
    expand as expand_utm_pattern, normalize_domain, slug as utm_slug, tag_url,
    validate_pattern as validate_utm_pattern,
};
pub use validate::validate;
pub use wire::{RenderRequest, RenderResponse};
