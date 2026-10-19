//! The pure rules of the governed event store (architecture section 12.1 and 12.2): schema
//! properties, validation and inference, time rules, redaction and masking.

mod mask;
mod property;
mod redact;
mod time_rules;
mod validate;

pub use mask::{Masked, mask, mask_event, mask_sample};
pub use property::{
    EventMode, PropertyType, RESERVED_EVENT_CODE, RESERVED_EVENT_PREFIX, SchemaProperty,
    UnplannedPolicy, WEBHOOK_FAILED_EVENT, WEBHOOK_SUCCEEDED_EVENT, is_reserved_event_name,
    validate_client_event_name, validate_event_name, validate_properties, validate_property_name,
};
pub use redact::{RedactionRules, redact, validate_rules};
pub use time_rules::{EventFlag, Timing, apply_time_rules};
pub use validate::{
    Reason, ReasonCode, ValidationState, check_properties, evaluate, infer_properties,
};
