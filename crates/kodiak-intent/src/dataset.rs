//! Training and evaluation examples generated from the vocabularies.
//!
//! Every phrase of every vocabulary yields labelled utterances, so a model
//! trained on this set sees exactly the language the deterministic baseline
//! accepts. Labels are `chat`, `no_intent`, `mecp.report/<CODE>`,
//! `contact.report` or an RCH command name.

use serde::Serialize;

use crate::vocabulary::{CONTACT_V1, Contact, MECP_V1, Mecp, RCH_V1, Rch};

/// The hand-written evaluation set: one JSON object per line with `text` and
/// the expected `kodiak.result.v1` fields under `expect`.
pub const EVAL_V1: &str = include_str!("../datasets/eval.v1.jsonl");

/// Live radio transcripts and constructed mishearings: one JSON object per
/// line with `text`, the expected `label` (as [`label`] writes it), the
/// expected MECP `content` where there is one, and its `source`.
pub const LIVE_V1: &str = include_str!("../datasets/live.v1.jsonl");

/// The regression set for `kodiak.result.v2`: one JSON object per line with
/// `suite` (`mecp`, `ddd`, `chat` or `rch`), `source` (`live` for a
/// recorded Parakeet v2 INT8 transcript, `constructed` otherwise), `text`,
/// the expected result fields under `expect`, and an optional `note`.
pub const REGRESSION_V2: &str = include_str!("../datasets/regression.v2.jsonl");

/// One labelled utterance.
#[derive(Clone, Debug, Serialize)]
pub struct Example {
    /// Utterance as a recognizer would transcribe it.
    pub text: String,
    /// Expected label.
    pub label: String,
}

/// The label of a result, in the same form as [`Example::label`].
#[must_use]
pub fn label(result: &kodiak_core::KodiakResult) -> String {
    use kodiak_core::{Intent, KodiakResult, RejectReason};
    match result {
        KodiakResult::Chat { .. } => "chat".into(),
        KodiakResult::Intent {
            intent: Intent::MecpReport(report),
            ..
        } => format!("mecp.report/{}", report.codes.join("+")),
        KodiakResult::Intent { intent, .. } => intent.name().into(),
        KodiakResult::Rejected { rejection, .. } => match rejection.reason {
            RejectReason::NoIntent => "no_intent",
            RejectReason::Ambiguous => "rejected/ambiguous",
            RejectReason::MissingParameter => "rejected/missing_parameter",
            RejectReason::InvalidParameter => "rejected/invalid_parameter",
            RejectReason::UnsupportedCommand => "rejected/unsupported_command",
            RejectReason::Contradictory => "rejected/contradictory",
            RejectReason::Cancelled => "rejected/cancelled",
        }
        .into(),
    }
}

fn example(text: impl Into<String>, label: impl Into<String>) -> Example {
    Example {
        text: text.into(),
        label: label.into(),
    }
}

/// Generates the labelled examples for the embedded version 1 vocabularies.
///
/// # Panics
///
/// Never in practice: the embedded vocabularies are validated by tests.
#[must_use]
pub fn generate() -> Vec<Example> {
    let mecp = Mecp::parse(MECP_V1).expect("valid MECP vocabulary");
    let contact = Contact::parse(CONTACT_V1).expect("valid contact vocabulary");
    let rch = Rch::parse(RCH_V1).expect("valid RCH vocabulary");
    let mut examples = Vec::new();
    for code in &mecp.codes {
        for (index, phrase) in code.phrases.iter().enumerate() {
            // Rotate severities so every level and every phrase appear.
            let severity = &mecp.severities[index % mecp.severities.len()];
            let word = &severity.words[0];
            examples.push(example(
                format!("Kodiak {word} {phrase}"),
                format!("mecp.report/{}", code.code),
            ));
            examples.push(example(
                format!("Kodiak. {phrase}, {word}."),
                format!("mecp.report/{}", code.code),
            ));
        }
    }
    let directions = [
        "three o'clock",
        "three thirty",
        "half past nine",
        "12:00",
        "bearing two seven zero",
        "bearing 45 degrees",
        "northwest",
    ];
    let distances = [
        "two hundred meters",
        "1.5 km",
        "500 m",
        "one kilometer",
        "a hundred metres",
    ];
    let mut variant = 0_usize;
    for words in contact.affiliations.values() {
        for word in words {
            for direction in directions {
                let distance = distances[variant % distances.len()];
                variant += 1;
                examples.push(example(
                    format!("Kodiak {word} {direction} {distance}"),
                    "contact.report",
                ));
            }
        }
    }
    for (word, _) in contact.classifications.iter().step_by(3) {
        examples.push(example(
            format!("Kodiak contact hostile two {word} nine o'clock 300 meters moving north"),
            "contact.report",
        ));
    }
    for command in &rch.commands {
        for phrase in &command.phrases {
            let text = match &command.argument {
                Some(_) => format!("Kodiak {phrase} Bravo Six"),
                None => format!("Kodiak, {phrase}."),
            };
            examples.push(example(text, command.intent.clone()));
        }
    }
    for chat in [
        "We need more food at the camp.",
        "Hostile contact at three o'clock two hundred meters.",
        "Urgent, need water at the north gate.",
        "List users please.",
        "Radio check, how copy?",
        "The Kodiaks are moving to the ridge.",
    ] {
        examples.push(example(chat, "chat"));
    }
    for none in [
        "Kodiak",
        "Kodiak, how are you today?",
        "Kodiak testing one two",
    ] {
        examples.push(example(none, "no_intent"));
    }
    examples
}
