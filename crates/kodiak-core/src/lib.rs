//! # kodiak-core
//!
//! Core contracts of R3AKT Kodiak, a reusable offline voice-command engine.
//!
//! Kodiak turns a voice message or a text message into exactly one of:
//!
//! - **chat**: the activation word "Kodiak" is absent;
//! - **intent**: the activation word is present and one validated intent
//!   (MECP report, contact report or RCH command) was extracted;
//! - **rejected**: the activation word is present but the command is unknown,
//!   ambiguous or incomplete.
//!
//! Kodiak never executes commands, publishes messages, or invents positions,
//! headings or confidence values. The calling application decides what to do
//! with a result.
//!
//! This crate has no speech or model dependencies. `kodiak-asr` provides
//! speech recognizers and `kodiak-intent` provides vocabularies and
//! classifiers.

mod engine;
pub mod mecp;
mod result;
mod wake;

pub use engine::{
    ASR_RETRY_INTERVAL, AUDIO_SAMPLE_RATE_HZ, Addressed, AudioInput, Classification,
    DEFAULT_MAX_AUDIO_SECONDS, DEFAULT_MAX_COMMAND_WORDS, Engine, IntentClassifier, KodiakError,
    TextInput, Transcriber,
};
pub use result::{
    Affiliation, CONTACT_REPORT, Category, ContactReport, ContractError, Coordinates, Correction,
    Direction, Intent, KodiakResult, MECP_REPORT, MecpParameters, MecpReport, RESULT_SCHEMA,
    RESULT_SCHEMA_V1, RchCommand, RejectReason, Rejection,
};
pub use wake::{DEFAULT_WAKE_FORMS, DEFAULT_WAKE_WORD, WakeWord};
