//! # kodiak-intent
//!
//! Versioned vocabularies and intent classifiers for R3AKT Kodiak.
//!
//! [`DeterministicClassifier`] is the baseline [`IntentClassifier`]: exact,
//! rule-based parsing of the embedded vocabularies with no model and no
//! confidence scores. A model-based classifier (for example a fine-tuned
//! `MiniLM` in ONNX) can replace it behind the same trait; see the package
//! README.
//!
//! The words after the activation word are classified in this order:
//!
//! 1. A complete RCH command phrase, e.g. "list users" → `mission.users`.
//! 2. An MECP severity word (mayday, urgent, safety, routine) → `mecp.report`.
//! 3. A contact affiliation (friendly, hostile, …) → `contact.report`.
//!
//! A severity together with an affiliation is ambiguous. Anything else is
//! `no_intent`, or `missing_parameter` when an MECP event was named without
//! a severity.
//!
//! When the engine finds no activation word, a first word that sounds like it
//! ("Kardiac", "Podax", "Codeac") still activates, but only when the rest of
//! the transcript is a recognized command, so conversation stays chat
//! ([`first_word_wake`]).

mod contact;
pub mod dataset;
mod fuzzy;
mod mecp;
mod rch;
mod text;
pub mod vocabulary;

use std::collections::BTreeSet;

use kodiak_core::{
    Addressed, Category, Classification, Intent, IntentClassifier, RejectReason, Rejection,
};

pub use text::tokenize;
use vocabulary::VocabularyError;

/// Identifier recorded in results produced by [`DeterministicClassifier`].
pub const DETERMINISTIC_ID: &str = "deterministic/3";

/// Rule-based classifier over the embedded vocabularies.
pub struct DeterministicClassifier {
    mecp: mecp::Parser,
    contact: contact::Parser,
    rch: rch::Parser,
    corrector: fuzzy::Corrector,
}

impl DeterministicClassifier {
    /// Loads the embedded version 1 vocabularies.
    ///
    /// # Panics
    ///
    /// Never in practice: the embedded vocabularies are validated by tests.
    #[must_use]
    pub fn new() -> Self {
        Self::from_sources(
            vocabulary::MECP_V1,
            vocabulary::CONTACT_V1,
            vocabulary::RCH_V1,
        )
        .expect("embedded Kodiak vocabularies are valid")
    }

    /// Loads vocabularies from TOML sources, for example newer versions.
    ///
    /// # Errors
    ///
    /// Fails if any vocabulary is malformed or inconsistent.
    pub fn from_sources(mecp: &str, contact: &str, rch: &str) -> Result<Self, VocabularyError> {
        let mecp = vocabulary::Mecp::parse(mecp)?;
        let contact = vocabulary::Contact::parse(contact)?;
        let rch = vocabulary::Rch::parse(rch)?;
        let corrector = fuzzy::Corrector::new(known_words(&mecp, &contact, &rch));
        Ok(Self {
            mecp: mecp::Parser::new(mecp),
            contact: contact::Parser::new(contact),
            rch: rch::Parser::new(rch),
            corrector,
        })
    }

    /// The strict grammar over already corrected words.
    fn classify_tokens(
        &self,
        tokens: &[text::Token],
        corrections: &mut Vec<kodiak_core::Correction>,
    ) -> Result<Intent, Rejection> {
        if tokens.is_empty() {
            return Err(Rejection::new(
                RejectReason::NoIntent,
                None,
                "nothing follows the activation word",
            ));
        }
        if let Some(result) = self.rch.parse(tokens) {
            return result;
        }
        let severity = tokens.iter().any(|t| self.mecp.severity(&t.word).is_some());
        let affiliation = tokens
            .iter()
            .any(|t| self.contact.affiliation(&t.word).is_some());
        match (severity, affiliation) {
            (true, true) => Err(Rejection::new(
                RejectReason::Ambiguous,
                None,
                "both an MECP severity and a contact affiliation",
            )),
            (true, false) => self.mecp.parse(tokens, corrections),
            (false, true) => self.contact.parse(tokens),
            (false, false) => {
                let words: Vec<&str> = tokens.iter().map(|t| t.word.as_str()).collect();
                if self.mecp.mentions_event(&words) {
                    Err(Rejection::new(
                        RejectReason::MissingParameter,
                        Some(Category::Mecp),
                        "MECP severity is missing (mayday, urgent, safety or routine)",
                    ))
                } else {
                    Err(Rejection::new(
                        RejectReason::NoIntent,
                        None,
                        "no intent recognized",
                    ))
                }
            }
        }
    }
}

/// The activation word a misheard first word is compared with.
const WAKE: &str = "kodiak";

/// Largest edit distance at which a first word still counts as the
/// activation word. Words shorter than four letters never count.
const WAKE_DISTANCE: usize = 3;

/// Edit distance between two short words.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(current + 1);
            previous = current;
        }
    }
    row[b.len()]
}

/// When the first word of `transcript` sounds like the activation word
/// (edit distance at most 3 to "kodiak", four letters or more), returns that
/// word as heard and the command after it.
///
/// This only finds a candidate: callers must still require the command to be
/// recognized before treating the transcript as addressed to Kodiak.
#[must_use]
pub fn first_word_wake(transcript: &str) -> Option<(String, String)> {
    let start = transcript.find(char::is_alphanumeric)?;
    let rest = &transcript[start..];
    let end = rest
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(rest.len());
    let heard = &rest[..end];
    let bare = heard.to_lowercase();
    (bare.chars().count() >= 4 && edit_distance(&bare, WAKE) <= WAKE_DISTANCE)
        .then(|| (heard.to_owned(), rest[end..].to_owned()))
}

/// Every word any vocabulary uses. Only words outside this set are corrected.
fn known_words(
    mecp: &vocabulary::Mecp,
    contact: &vocabulary::Contact,
    rch: &vocabulary::Rch,
) -> BTreeSet<String> {
    let phrases = mecp
        .severities
        .iter()
        .flat_map(|severity| &severity.words)
        .chain(mecp.codes.iter().flat_map(|code| &code.phrases))
        .chain(contact.affiliations.values().flatten())
        .chain(rch.commands.iter().flat_map(|command| &command.phrases))
        .chain(rch.unsupported.iter().flat_map(|entry| &entry.phrases));
    let mut known: BTreeSet<String> = phrases
        .flat_map(|phrase| vocabulary::phrase_words(phrase))
        .collect();
    known.extend(mecp.fillers.iter().cloned());
    known.extend(contact.fillers.iter().cloned());
    known.extend(rch.fillers.iter().cloned());
    known.extend(contact.classifications.keys().cloned());
    known.extend(contact.activities.keys().cloned());
    known.extend(contact.units.keys().cloned());
    known.extend(contact.bearing_words.iter().cloned());
    known.extend(contact.compass_points.keys().cloned());
    known.extend(
        rch.commands
            .iter()
            .flat_map(|command| command.argument_lead.iter().cloned()),
    );
    known.extend(text::NUMBER_WORDS.iter().map(|word| (*word).to_owned()));
    known
}

impl Default for DeterministicClassifier {
    fn default() -> Self {
        Self::new()
    }
}

impl IntentClassifier for DeterministicClassifier {
    fn id(&self) -> &'static str {
        DETERMINISTIC_ID
    }

    fn classify(&self, command: &str) -> Classification {
        let tokens = self.mecp.join_severities(&tokenize(command));
        let (tokens, mut corrections) = self.corrector.correct(tokens);
        let result = self.classify_tokens(&tokens, &mut corrections);
        Classification {
            result,
            corrections,
            confidence: None,
        }
    }

    /// A misheard activation word as the first word, when the rest is a
    /// recognized command.
    fn addressed(&self, transcript: &str) -> Option<Addressed> {
        let (heard, command) = first_word_wake(transcript)?;
        self.classify(&command).result.ok()?;
        Some(Addressed {
            command,
            heard: Some(heard),
        })
    }
}

#[cfg(test)]
mod tests {
    use kodiak_core::{Affiliation, ContactReport, Direction, MecpReport};

    use super::*;

    fn classify(text: &str) -> Result<Intent, Rejection> {
        DeterministicClassifier::new().classify(text).result
    }

    fn mecp(text: &str) -> String {
        match classify(text) {
            Ok(Intent::MecpReport(MecpReport { content, .. })) => content,
            other => panic!("{text}: {other:?}"),
        }
    }

    fn reason(text: &str) -> RejectReason {
        classify(text).expect_err(text).reason
    }

    #[test]
    fn spec_examples() {
        assert_eq!(mecp(". Urgent. Need Food."), "MECP/1/S02");
        assert_eq!(mecp("urgent need water"), "MECP/1/S01");
        assert_eq!(mecp("mayday severe bleeding"), "MECP/0/M06");
        assert_eq!(mecp("safety all clear"), "MECP/2/R07");
        assert_eq!(
            classify("hostile three thirty two hundred meters.").unwrap(),
            Intent::ContactReport(ContactReport {
                affiliation: Affiliation::Hostile,
                direction: Direction::RelativeClock {
                    relative_bearing_deg: 105
                },
                distance_m: 200,
                classification: None,
                quantity: None,
                activity: None,
            })
        );
        assert!(
            matches!(classify("list users."), Ok(Intent::RchCommand(c)) if c.command == "mission.users")
        );
    }

    #[test]
    fn rejections_are_explicit() {
        assert_eq!(reason(""), RejectReason::NoIntent);
        assert_eq!(reason("what a lovely day"), RejectReason::NoIntent);
        assert_eq!(reason("need food"), RejectReason::MissingParameter);
        assert_eq!(reason("urgent"), RejectReason::MissingParameter);
        assert_eq!(reason("urgent food and water"), RejectReason::Ambiguous);
        assert_eq!(reason("urgent hostile"), RejectReason::Ambiguous);
        assert_eq!(
            reason("hostile two hundred meters"),
            RejectReason::MissingParameter
        );
        assert_eq!(
            reason("hostile three o'clock"),
            RejectReason::MissingParameter
        );
        assert_eq!(
            reason("hostile 3:15 200 meters"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("hostile bearing 400 200 meters"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("hostile three o'clock bearing 270 200 meters"),
            RejectReason::Ambiguous
        );
        assert_eq!(
            reason("hostile friendly three o'clock 200 meters"),
            RejectReason::Ambiguous
        );
        assert_eq!(
            reason("hostile three o'clock 200 meters banana"),
            RejectReason::InvalidParameter
        );
        assert_eq!(reason("nickname"), RejectReason::MissingParameter);
        assert_eq!(reason("search library for"), RejectReason::MissingParameter);
        assert_eq!(reason("time remaining now"), RejectReason::NoIntent);
        assert_eq!(reason("list users now"), RejectReason::NoIntent);
    }

    #[test]
    fn normal_is_routine() {
        assert_eq!(mecp("normal need water."), "MECP/3/S01");
    }

    #[test]
    fn a_misheard_first_word_activates_only_before_a_command() {
        let rules = DeterministicClassifier::new();
        let addressed = rules.addressed("Kardiac urgent need food.").unwrap();
        assert_eq!(addressed.heard.as_deref(), Some("Kardiac"));
        assert_eq!(addressed.command, " urgent need food.");
        let addressed = rules.addressed("Podax, mayday, meet water.").unwrap();
        assert_eq!(addressed.heard.as_deref(), Some("Podax"));
        for chat in [
            "I think we need water.",
            "Cardiac arrest is a medical term",
            "Koala bears sleep a lot",
            "Codeac, are you there?",
            "God yeah. May they need water.",
        ] {
            assert_eq!(rules.addressed(chat), None, "{chat}");
        }
    }
}
