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
//! 2. An MECP severity word (mayday, urgent, safety, routine) → `mecp.report`,
//!    with one or more event codes and any spoken structured values.
//! 3. A contact affiliation (friendly, hostile, …) → `contact.report`.
//!
//! A severity together with an affiliation is ambiguous. Otherwise a negation
//! ("Kodiak don't create a marker") is `cancelled`, an MECP event without a
//! severity is `missing_parameter`, and anything else is `no_intent`.
//!
//! When the engine finds no activation word, a first word that sounds like it
//! ("Kardiac", "Cardiac", "Podax") still activates, but only when the rest of
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
    WakeWord,
};

pub use text::tokenize;
use vocabulary::VocabularyError;

/// Identifier recorded in results produced by [`DeterministicClassifier`].
pub const DETERMINISTIC_ID: &str = "deterministic/6";

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
        let known = known_words(&mecp, &contact, &rch);
        let mecp = mecp::Parser::new(mecp);
        let contact = contact::Parser::new(contact);
        // Grammar words are known but never a correction target: a misheard
        // word must not become a keyword, a language or a negation.
        let grammar: BTreeSet<String> = mecp
            .grammar_words()
            .chain(contact.grammar_words().cloned())
            .collect();
        let mut known = known;
        known.extend(grammar.iter().cloned());
        let corrector = fuzzy::Corrector::new(known, &grammar);
        Ok(Self {
            mecp,
            contact,
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
            // An argument takes every word after the command phrase, so a
            // spoken correction would become part of it ("call me bravo, no,
            // cancel that" as the nickname "bravo no cancel that").
            if let Ok(Intent::RchCommand(command)) = &result
                && !command.arguments.is_empty()
                && let Some(word) = tokens
                    .iter()
                    .map(|t| t.word.as_str())
                    .find(|word| self.contact.is_negation(word))
            {
                return Err(Rejection::new(
                    RejectReason::Cancelled,
                    Some(Category::Rch),
                    format!("the command is negated or cancelled by {word:?}"),
                ));
            }
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
                if let Some(word) = self.mecp.uncovered_negation(&words).or_else(|| {
                    words
                        .iter()
                        .copied()
                        .find(|word| self.contact.is_negation(word))
                        .filter(|_| !self.mecp.mentions_event(&words))
                }) {
                    Err(Rejection::new(
                        RejectReason::Cancelled,
                        None,
                        format!("the command is negated or cancelled by {word:?}"),
                    ))
                } else if self.mecp.mentions_event(&words) {
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

/// Largest sound-aware distance, in half edits, at which a first word still
/// counts as the activation word: three edits, where letters that sound
/// alike over radio (c/k, a/o, …) cost half an edit. Words shorter than four
/// letters never count.
const WAKE_DISTANCE: u32 = 6;

/// When the first word of `transcript` sounds like the activation word
/// (at most three edits from `wake.word()`, counting sound-alike letters as
/// half an edit, and four letters or more), returns that word as heard and
/// the command after it. With the default word "Cardiac" qualifies: c/k and
/// a/o sound alike.
///
/// This only finds a candidate: callers must still require the command to be
/// recognized before treating the transcript as addressed to Kodiak.
#[must_use]
pub fn first_word_wake(transcript: &str, wake: &WakeWord) -> Option<(String, String)> {
    let start = transcript.find(char::is_alphanumeric)?;
    let rest = &transcript[start..];
    let end = rest
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(rest.len());
    let heard = &rest[..end];
    let bare = heard.to_lowercase();
    (bare.chars().count() >= 4 && fuzzy::distance(&bare, wake.word()) <= WAKE_DISTANCE)
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

    fn vocabularies(&self) -> std::collections::BTreeMap<String, String> {
        [
            ("mecp".to_owned(), self.mecp.release()),
            ("contact".to_owned(), self.contact.release()),
            ("rch".to_owned(), self.rch.release()),
        ]
        .into()
    }

    fn classify(&self, command: &str) -> Classification {
        let tokens = self.mecp.join_severities(&tokenize(command));
        let (tokens, mut corrections) = self.corrector.drop_false_starts(&tokens);
        let words: Vec<&str> = tokens.iter().map(|t| t.word.as_str()).collect();
        let verbatim = self.mecp.verbatim(&words);
        let (tokens, fixed) = self.corrector.correct(tokens, &verbatim);
        corrections.extend(fixed);
        let result = self.classify_tokens(&tokens, &mut corrections);
        Classification {
            result,
            corrections,
            confidence: None,
        }
    }

    /// A misheard activation word as the first word, when the rest is a
    /// recognized command.
    fn addressed(&self, transcript: &str, wake: &WakeWord) -> Option<Addressed> {
        let (heard, command) = first_word_wake(transcript, wake)?;
        let command = wake.strip_leading(&command).to_owned();
        let classification = self.classify(&command);
        classification.result.as_ref().ok()?;
        Some(Addressed {
            command,
            heard: Some(heard),
            classification: Some(classification),
        })
    }
}

#[cfg(test)]
mod tests {
    use kodiak_core::{
        Affiliation, ContactReport, Correction, Direction, Engine, KodiakResult, MecpParameters,
        MecpReport, TextInput,
    };

    use super::*;

    fn classify(text: &str) -> Result<Intent, Rejection> {
        DeterministicClassifier::new().classify(text).result
    }

    /// What the engine makes of `text`: an MECP content, a rejection
    /// reason, another intent's name or "chat".
    fn engine_mecp(engine: &Engine, text: &str) -> String {
        match engine.process_text(&TextInput::new(text)) {
            KodiakResult::Intent {
                intent: Intent::MecpReport(report),
                ..
            } => report.content,
            KodiakResult::Intent { intent, .. } => intent.name().to_owned(),
            KodiakResult::Rejected { rejection, .. } => format!("{:?}", rejection.reason),
            KodiakResult::Chat { .. } => "chat".into(),
        }
    }

    #[test]
    fn a_configured_wake_word_replaces_kodiak() {
        let engine = Engine::new(DeterministicClassifier::new())
            .with_wake_word(WakeWord::with_forms(["falcon"]));
        // Exact word, first.
        assert_eq!(
            engine_mecp(&engine, "Falcon, urgent need food."),
            "MECP/1/S02"
        );
        assert_eq!(engine_mecp(&engine, "hey falcon list users"), "chat");
        // Repeated.
        assert_eq!(
            engine_mecp(&engine, "Falcon falcon routine heading home"),
            "MECP/3/L14"
        );
        // Misheard first word, within the sound-aware distance.
        let KodiakResult::Intent { corrections, .. } =
            engine.process_text(&TextInput::new("Valcon urgent need food."))
        else {
            panic!("expected intent");
        };
        assert_eq!(
            corrections[0],
            Correction {
                heard: "Valcon".into(),
                read_as: "falcon".into()
            }
        );
        // "kodiak" is not special any more.
        for chat in [
            "Kodiak urgent need food.",
            "Kardiac urgent need food.",
            "Cardiac, May Day Severe Bleeding.",
        ] {
            assert_eq!(engine_mecp(&engine, chat), "chat", "{chat}");
        }
        assert_ne!(
            engine_mecp(&engine, "Falcon kodiak routine heading home"),
            "MECP/3/L14"
        );
        let falcon = WakeWord::with_forms(["falcon"]);
        assert_eq!(
            first_word_wake("Falkon list users", &falcon),
            Some(("Falkon".into(), " list users".into()))
        );
        assert_eq!(first_word_wake("Kodiak list users", &falcon), None);
        assert_eq!(first_word_wake("Fal list users", &falcon), None);
    }

    #[test]
    fn every_default_spelling_is_dropped_when_repeated() {
        let engine = Engine::new(DeterministicClassifier::new());
        for text in [
            "Kodiak kodiac routine heading home",
            "Kodiak, Codiak. routine heading home",
            "codeiac kodiak routine heading home",
        ] {
            assert_eq!(engine_mecp(&engine, text), "MECP/3/L14", "{text}");
        }
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
            RejectReason::Contradictory
        );
        assert_eq!(
            reason("hostile friendly three o'clock 200 meters"),
            RejectReason::Contradictory
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

    fn contact(text: &str) -> ContactReport {
        match classify(text) {
            Ok(Intent::ContactReport(report)) => report,
            other => panic!("{text}: {other:?}"),
        }
    }

    fn parameters(text: &str) -> MecpParameters {
        match classify(text) {
            Ok(Intent::MecpReport(report)) => report.parameters,
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn several_events_keep_their_spoken_order() {
        assert_eq!(
            mecp("mayday injured, broken leg, need water."),
            "MECP/0/M01 M07 S01"
        );
        assert_eq!(mecp("urgent road blocked bridge out"), "MECP/1/T01 T02");
        assert_eq!(mecp("urgent food and water"), "MECP/1/S02 S01");
        // A repeated event is reported once.
        assert_eq!(mecp("mayday need water, water!"), "MECP/0/S01");
        // Phrases that cross: "need water" (S01) or "water rescue" (T11)?
        assert_eq!(reason("mayday need water rescue"), RejectReason::Ambiguous);
        // Events that exclude each other.
        assert_eq!(
            reason("urgent need water we have water"),
            RejectReason::Contradictory
        );
        assert_eq!(
            reason("urgent mayday bleeding"),
            RejectReason::Contradictory
        );
    }

    #[test]
    fn spoken_parameters_are_encoded_in_canonical_order() {
        assert_eq!(mecp("mayday injured three people"), "MECP/0/M01 3pax");
        assert_eq!(mecp("routine have space for five"), "MECP/3/H07 5pax");
        assert_eq!(mecp("routine eta twenty minutes"), "MECP/3/R03 20");
        assert_eq!(mecp("routine eta one hour"), "MECP/3/R03 60");
        assert_eq!(
            mecp("urgent stranded 45.5017, -73.5673"),
            "MECP/1/P01 45.5017,-73.5673"
        );
        assert_eq!(
            mecp("urgent stranded forty five point five minus seventy three point two five"),
            "MECP/1/P01 45.5,-73.25"
        );
        assert_eq!(
            mecp("mayday injured callsign bravo six time fourteen thirty language french"),
            "MECP/0/M01 @fr @1430 ~BRAVO-6"
        );
        assert_eq!(
            mecp("safety road blocked reference alpha one two details north gate by the Barn"),
            "MECP/2/T01 #ALPHA-12 north gate by the Barn"
        );
        assert_eq!(mecp("urgent fire 0930 zulu"), "MECP/1/T07 @0930");
        assert_eq!(
            mecp("routine eta estimated time of arrival 15"),
            "MECP/3/R03 15"
        );
        let spoken = parameters("mayday injured four people");
        assert_eq!(spoken.pax, Some(4));
        assert_eq!(spoken.coordinates, None, "never filled in from metadata");
    }

    #[test]
    fn invalid_or_incomplete_parameters_are_rejected() {
        assert_eq!(
            reason("mayday injured callsign"),
            RejectReason::MissingParameter
        );
        assert_eq!(
            reason("mayday injured language klingon"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("mayday injured time 2575"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("mayday injured 0 people"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("urgent stranded 95.5 10.5"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("mayday injured three people four people"),
            RejectReason::Contradictory
        );
        // R03 and H07 both take a bare number.
        assert_eq!(
            reason("routine eta have space for 5"),
            RejectReason::Ambiguous
        );
        // More than 200 bytes on the wire.
        let long = format!("mayday injured details {}", "very long words ".repeat(14));
        assert_eq!(reason(&long), RejectReason::InvalidParameter);
    }

    #[test]
    fn review_findings_of_23_november_2026() {
        // A spoken correction inside an argument cancels the command.
        assert_eq!(
            reason("call me bravo, no, cancel that"),
            RejectReason::Cancelled
        );
        assert!(matches!(
            classify("set nickname to Bravo"),
            Ok(Intent::RchCommand(c)) if c.arguments["nickname"] == "Bravo"
        ));
        // A huge spoken number is not a number, and never overflows.
        let huge = "nineteen-hundred-ninety-nine-thousand-".repeat(30);
        let _ = classify(&format!("urgent injured {huge}"));
        let engine = Engine::new(DeterministicClassifier::new());
        // The engine's word limit counts hyphenated words.
        let long = "nineteen-hundred-ninety-nine-thousand-".repeat(3000);
        assert!(matches!(
            engine.process_text(&kodiak_core::TextInput::new(format!(
                "Kodiak urgent injured {long}"
            ))),
            KodiakResult::Rejected { .. }
        ));
        // A misheard repeated activation word adds no event.
        let out = engine.process_text(&kodiak_core::TextInput::new(
            "Kodiak, Cardiac, mayday severe bleeding",
        ));
        let KodiakResult::Intent {
            intent: Intent::MecpReport(report),
            corrections,
            ..
        } = out
        else {
            panic!("expected a report: {out:?}");
        };
        assert_eq!(report.content, "MECP/0/M06");
        assert_eq!(corrections[0].heard, "Cardiac");
        // Only the first word activates Kodiak.
        for chat in ["I saw a kodiak bear yesterday", "Hey Kodiak, list users"] {
            assert!(matches!(
                engine.process_text(&kodiak_core::TextInput::new(chat)),
                KodiakResult::Chat { .. }
            ));
        }
    }

    #[test]
    fn negations_cancel_commands() {
        assert_eq!(reason("don't create a marker"), RejectReason::Cancelled);
        assert_eq!(reason("mayday don't need water"), RejectReason::Cancelled);
        assert_eq!(
            reason("hostile 300 meters 6 o'clock no"),
            RejectReason::Cancelled
        );
        assert_eq!(reason("cancel"), RejectReason::Cancelled);
        // Negations inside event phrases are events.
        assert_eq!(mecp("mayday not breathing"), "MECP/0/M03");
        assert_eq!(mecp("urgent no water"), "MECP/1/S01");
        assert_eq!(mecp("routine cancel beacon"), "MECP/3/B03");
        assert_eq!(reason("cancel beacon"), RejectReason::MissingParameter);
    }

    #[test]
    fn ddd_contact_reports() {
        let report = contact("hostile, 300 meters, 6 o'clock.");
        assert_eq!(report.affiliation, Affiliation::Hostile);
        assert_eq!(report.distance_m, 300);
        assert_eq!(
            report.direction,
            Direction::RelativeClock {
                relative_bearing_deg: 180
            }
        );
        let report = contact("hostile, two vehicles, 500 meters, 3 o'clock, moving north.");
        assert_eq!(report.classification.as_deref(), Some("vehicle"));
        assert_eq!(report.quantity, Some(2));
        assert_eq!(report.activity.as_deref(), Some("moving north"));
        assert_eq!(
            report.direction,
            Direction::RelativeClock {
                relative_bearing_deg: 90
            }
        );
        for (text, metres) in [
            ("friendly three hundred meters 12 o'clock", 300),
            ("friendly 300 meters 12 o'clock", 300),
            ("friendly half a kilometer 12 o'clock", 500),
            ("friendly one kilometer 12 o'clock", 1000),
            ("friendly a kilometer 12 o'clock", 1000),
            ("friendly quarter of a klick 12 o'clock", 250),
        ] {
            assert_eq!(contact(text).distance_m, metres, "{text}");
        }
        for (clock, degrees) in [
            ("12 o'clock", 0),
            ("3 o'clock", 90),
            ("6 o'clock", 180),
            ("9 o'clock", 270),
            ("3:30", 105),
            ("half past ten", 315),
        ] {
            assert_eq!(
                contact(&format!("unknown 100 m {clock}")).direction,
                Direction::RelativeClock {
                    relative_bearing_deg: degrees
                },
                "{clock}"
            );
        }
        for (bearing, degrees) in [
            ("bearing 270", 270),
            ("270 degrees", 270),
            ("northeast", 45),
            ("north east", 45),
        ] {
            assert_eq!(
                contact(&format!("suspect 100 m {bearing}")).direction,
                Direction::CompassBearing {
                    bearing_deg: degrees
                },
                "{bearing}"
            );
        }
        assert_eq!(
            reason("hostile 0 meters 6 o'clock"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("hostile 200 kilometers 6 o'clock"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("hostile 300 meters 6 o'clock 9 o'clock"),
            RejectReason::Contradictory
        );
        assert_eq!(
            reason("hostile 300 meters 500 meters 6 o'clock"),
            RejectReason::Contradictory
        );
        assert_eq!(
            reason("hostile 13 o'clock 300 meters"),
            RejectReason::InvalidParameter
        );
    }

    #[test]
    fn vocabulary_versions_are_recorded() {
        let versions = DeterministicClassifier::new().vocabularies();
        assert_eq!(versions["mecp"], "1.9");
        assert_eq!(versions["contact"], "1.3");
        assert_eq!(versions["rch"], "1.1");
    }

    #[test]
    fn normal_is_routine() {
        assert_eq!(mecp("normal need water."), "MECP/3/S01");
    }

    #[test]
    fn a_misheard_first_word_activates_only_before_a_command() {
        let rules = DeterministicClassifier::new();
        let wake = WakeWord::default();
        let addressed = rules.addressed("Kardiac urgent need food.", &wake).unwrap();
        assert_eq!(addressed.heard.as_deref(), Some("Kardiac"));
        assert_eq!(addressed.command, " urgent need food.");
        let addressed = rules
            .addressed("Podax, mayday, meet water.", &wake)
            .unwrap();
        assert_eq!(addressed.heard.as_deref(), Some("Podax"));
        // Heard on air on 22 November 2026; c/k and a/o sound alike.
        let addressed = rules
            .addressed("Cardiac, May Day Severe Bleeding.", &wake)
            .unwrap();
        assert_eq!(addressed.heard.as_deref(), Some("Cardiac"));
        let addressed = rules.addressed("Kodiaks mayday meet water", &wake).unwrap();
        assert_eq!(addressed.heard.as_deref(), Some("Kodiaks"));
        // "Kodiaks" before an incomplete command stays chat.
        assert_eq!(rules.addressed("Kodiaks hostile 300 meters", &wake), None);
        assert_eq!(rules.addressed("The Kodiaks need water", &wake), None);
        for chat in [
            "I think we need water.",
            "Cardiac arrest is a medical term",
            "Koala bears sleep a lot",
            "Codeac, are you there?",
            "God yeah. May they need water.",
        ] {
            assert_eq!(rules.addressed(chat, &wake), None, "{chat}");
        }
    }

    /// Rules added after the 22 November 2026 voice samples.
    #[test]
    fn live_voice_sample_fixes() {
        // A repeated activation word is dropped (by the engine).
        let engine = Engine::new(DeterministicClassifier::new());
        assert_eq!(
            engine_mecp(&engine, "kodiak routine heading home"),
            "MECP/3/L14"
        );
        assert_eq!(
            engine_mecp(&engine, "kodiak kodiak routine heading home"),
            "MECP/3/L14"
        );
        // A plural unit after "one": a number word was most likely dropped.
        assert_eq!(
            reason("friendly one meters twelve o'clock"),
            RejectReason::Ambiguous
        );
        assert_eq!(
            reason("hostile three o'clock one kilometres"),
            RejectReason::Ambiguous
        );
        assert!(classify("hostile one kilometer three o'clock").is_ok());
        assert!(classify("hostile 1 meter three o'clock").is_ok());
        // Bearings as hundreds and a two-digit number.
        for (text, expected) in [
            ("neutral one kilometre bearing two seventy", 270),
            ("neutral 500 meters bearings one twenty five", 125),
            ("neutral 500 meters three fifteen degrees", 315),
            ("neutral 500 meters bearing two seven zero", 270),
        ] {
            match classify(text) {
                Ok(Intent::ContactReport(ContactReport {
                    direction: Direction::CompassBearing { bearing_deg },
                    ..
                })) => assert_eq!(bearing_deg, expected, "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
        assert_eq!(
            reason("neutral 500 meters bearing four seventy"),
            RejectReason::InvalidParameter
        );
        // Recorded spellings of words and keywords.
        assert_eq!(
            mecp("mayday injure broken leg need water"),
            "MECP/0/M01 M07 S01"
        );
        assert_eq!(mecp("urgent roadblocked bridge out"), "MECP/1/T01 T02");
        assert_eq!(
            mecp("mayday injure cosign bravo sixth language french"),
            "MECP/0/M01 @fr ~BRAVO-6"
        );
        assert_eq!(
            mecp("mayday injured cold sign bravo twelve"),
            "MECP/0/M01 ~BRAVO-12"
        );
        // A word outside the phonetic alphabet is never sent in a callsign.
        assert_eq!(
            reason("mayday injured cold sign revo six"),
            RejectReason::InvalidParameter
        );
        assert_eq!(
            reason("safety road blocked reference offer one"),
            RejectReason::InvalidParameter
        );
        // A false start before the word it begins is dropped and reported.
        let result = DeterministicClassifier::new().classify("mayday stranded st stuck");
        assert_eq!(result.corrections.len(), 1);
        assert_eq!(result.corrections[0].read_as, "stuck");
        assert!(matches!(
            result.result,
            Ok(Intent::MecpReport(MecpReport { ref content, .. })) if content == "MECP/0/P01"
        ));
        assert_eq!(mecp("routine un route two"), "MECP/3/P04");
        assert_eq!(mecp("routine en route to"), "MECP/3/P04");
        // Free text and identifiers keep their spelling: no corrections.
        let rules = DeterministicClassifier::new();
        let result = rules.classify("safety road blocked reference alpha one details north gate");
        assert!(result.corrections.is_empty(), "{:?}", result.corrections);
        match result.result {
            Ok(Intent::MecpReport(MecpReport { content, .. })) => {
                assert_eq!(content, "MECP/2/T01 #ALPHA-1 north gate");
            }
            other => panic!("{other:?}"),
        }
    }
}
