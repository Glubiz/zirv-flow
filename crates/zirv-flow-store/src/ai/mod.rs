//! The AI provider layer's storage (architecture sections 13.1 and 13.4): a workspace's assistant
//! settings, the content-free log of model calls with the budget and rate-limit counts read from
//! it, and opt-in transcripts with their retention and erasure.
//!
//! Counting from the database, not from memory, keeps the budget and the rate limit true across
//! instances. The call log has no column that could hold a prompt or a response.

mod calls;
mod drafts;
mod settings;
mod transcripts;

pub use calls::{
    AiCall, AiUsage, CallOutcome, FeatureUsage, FinishedAiCall, NewAiCall, Reservation,
    ReservationLimits, UserUsage, ai_usage, calls_in_last_minute, finish_ai_call, month_start,
    record_ai_call, reserve_ai_call, spent_this_month,
};
pub use drafts::{
    AiDraft, DRAFT_PURGE_BATCH, DraftKind, DraftStatus, NewAiDraft, find_ai_draft, insert_ai_draft,
    mark_ai_draft_accepted, purge_expired_ai_drafts,
};
pub use settings::{
    AiSettings, AiSettingsUpdate, ProbeRecord, find_ai_settings, lock_ai_settings, put_ai_settings,
};
pub use transcripts::{
    TRANSCRIPT_PURGE_BATCH, erase_ai_transcripts, insert_ai_transcript,
    purge_expired_ai_transcripts,
};
