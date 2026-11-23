//! Policy tests with a scripted oracle, so they run without model files.

use std::sync::atomic::{AtomicUsize, Ordering};

use kodiak_core::{Engine, KodiakResult, TextInput};

use super::*;

/// Answers each choice by giving `p` to the first scripted label among its
/// options and spreading the rest; unscripted questions go to
/// [`NONE_OF_THESE`] or [`NOT_A_SEVERITY`].
struct Scripted {
    picks: Vec<(&'static str, f32)>,
    calibrated: bool,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(picks: &[(&'static str, f32)]) -> Self {
        Self {
            picks: picks.to_vec(),
            calibrated: true,
            calls: AtomicUsize::new(0),
        }
    }
}

impl Oracle for Scripted {
    fn ask(&self, _: &Value, questions: &[(String, Ask)]) -> Result<Vec<Reply>, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(questions
            .iter()
            .map(|(_, ask)| {
                let probabilities = match ask {
                    Ask::Noul { .. } => BTreeMap::from([("true".into(), 0.05)]),
                    Ask::Choice { options, .. } => {
                        let has = |label: &str| options.iter().any(|(option, _)| option == label);
                        let fallback = if has(NOT_A_SEVERITY) {
                            NOT_A_SEVERITY
                        } else {
                            NONE_OF_THESE
                        };
                        let (label, p) = self
                            .picks
                            .iter()
                            .find(|(label, _)| has(label))
                            .copied()
                            .unwrap_or((fallback, 0.9));
                        #[expect(clippy::cast_precision_loss, reason = "a handful of options")]
                        let rest = (1.0 - p) / options.len().saturating_sub(1).max(1) as f32;
                        options
                            .iter()
                            .map(|(option, _)| {
                                (option.clone(), if option == label { p } else { rest })
                            })
                            .collect()
                    }
                };
                Reply {
                    probabilities,
                    calibrated: self.calibrated,
                }
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "scripted"
    }
}

fn classifier(oracle: Scripted, mode: Mode) -> LayaClassifier<Scripted> {
    LayaClassifier::new(
        oracle,
        Policy {
            threshold: 0.85,
            mode,
        },
    )
}

fn classify(oracle: Scripted, mode: Mode, text: &str) -> Classification {
    classifier(oracle, mode).classify(text)
}

fn content(classification: &Classification) -> String {
    match &classification.result {
        Ok(Intent::MecpReport(report)) => report.content.clone(),
        Ok(other) => other.name().to_owned(),
        Err(rejection) => format!("{:?}", rejection.reason),
    }
}

fn result(engine_result: &KodiakResult) -> String {
    match engine_result {
        KodiakResult::Intent {
            intent: Intent::MecpReport(report),
            ..
        } => report.content.clone(),
        KodiakResult::Intent { intent, .. } => intent.name().to_owned(),
        KodiakResult::Rejected { rejection, .. } => format!("{:?}", rejection.reason),
        KodiakResult::Chat { .. } => "chat".into(),
    }
}

#[test]
fn hybrid_does_not_ask_laya_when_the_rules_succeed() {
    let laya = classifier(Scripted::new(&[("S02", 0.99)]), Mode::Hybrid);
    let out = laya.classify("urgent need water");
    assert_eq!(content(&out), "MECP/1/S01");
    assert_eq!(out.confidence, None);
    assert_eq!(laya.oracle.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn a_severity_synonym_is_read_by_laya_and_the_event_by_the_rules() {
    // "critical" is not a severity word in the rules.
    let out = classify(
        Scripted::new(&[("routine", 0.95)]),
        Mode::Hybrid,
        "critical need water",
    );
    assert_eq!(content(&out), "MECP/3/S01");
    // The scripted spread gives the other routine form a little too.
    assert!(out.confidence.unwrap() >= 0.95);
}

#[test]
fn spoken_forms_of_one_level_are_summed() {
    // 0.5 + 0.4 for mayday: sure only together.
    struct Fixed;
    impl Oracle for Fixed {
        fn ask(&self, _: &Value, questions: &[(String, Ask)]) -> Result<Vec<Reply>, String> {
            let reply = Reply {
                probabilities: BTreeMap::from([
                    ("mayday".into(), 0.5),
                    ("may".into(), 0.4),
                    ("urgent".into(), 0.1),
                ]),
                calibrated: true,
            };
            Ok(vec![reply; questions.len()])
        }

        fn name(&self) -> &'static str {
            "fixed"
        }
    }
    let laya = LayaClassifier::new(Fixed, Policy::default());
    let read = laya.severity("Mayday").unwrap().unwrap();
    assert_eq!((read.level, read.words), (0, 1));
    assert!((read.p - 0.9).abs() < 1e-6);
}

#[test]
fn unsure_or_uncalibrated_laya_keeps_the_rules() {
    let out = classify(
        Scripted::new(&[("routine", 0.6)]),
        Mode::Hybrid,
        "critical need water",
    );
    assert_eq!(content(&out), "MissingParameter");
    let mut oracle = Scripted::new(&[("routine", 0.99)]);
    oracle.calibrated = false;
    let out = classify(oracle, Mode::Hybrid, "critical need water");
    assert_eq!(content(&out), "MissingParameter");
}

#[test]
fn laya_reads_the_event_only_when_alone() {
    let picks = &[("urgent", 0.97), ("M07", 0.95)];
    let text = "urgent someone fell and broke a leg";
    let out = classify(Scripted::new(picks), Mode::Hybrid, text);
    assert_eq!(content(&out), "MissingParameter");
    // Nor does a hub reading move an MECP report to another family.
    let help = &[("help", 0.99)];
    let out = classify(Scripted::new(help), Mode::Hybrid, text);
    assert_eq!(content(&out), "MissingParameter");
    let out = classify(Scripted::new(picks), Mode::LayaOnly, text);
    assert_eq!(content(&out), "MECP/1/M07");
    let p = out.confidence.unwrap();
    assert!((0.97 * 0.95..0.95).contains(&p), "{p}");
}

#[test]
fn hybrid_never_reads_a_known_word_as_a_severity() {
    // "need" is a filler the rules know; laya once read it as urgent.
    let out = classify(
        Scripted::new(&[("urgent", 0.99)]),
        Mode::Hybrid,
        "need food",
    );
    assert_eq!(content(&out), "MissingParameter");
}

#[test]
fn ambiguous_and_invalid_rules_stay_rejected() {
    let out = classify(
        Scripted::new(&[("urgent", 0.99), ("S01", 0.99)]),
        Mode::Hybrid,
        "urgent need water rescue",
    );
    assert_eq!(content(&out), "Ambiguous");
    let out = classify(
        Scripted::new(&[("mayday", 0.99), ("S01", 0.99)]),
        Mode::Hybrid,
        "mayday we don't need water anymore",
    );
    assert_eq!(content(&out), "Cancelled");
    let out = classify(
        Scripted::new(&[("urgent", 0.99), ("S01", 0.99)]),
        Mode::Hybrid,
        "urgent need water we have water",
    );
    assert_eq!(content(&out), "Contradictory");
}

#[test]
fn values_still_come_from_the_parsers() {
    let out = classify(
        Scripted::new(&[]),
        Mode::LayaOnly,
        "hostile three o'clock two hundred meters",
    );
    assert_eq!(content(&out), "contact.report");
    let nick = &[("mission.nick", 0.95)];
    let out = classify(Scripted::new(nick), Mode::LayaOnly, "nickname");
    assert_eq!(content(&out), "MissingParameter");
    let users = &[("mission.users", 0.95)];
    let out = classify(
        Scripted::new(users),
        Mode::LayaOnly,
        "show me who is around",
    );
    assert_eq!(content(&out), "mission.users");
    let pax = &[("safety", 0.95), ("H07", 0.95)];
    let out = classify(
        Scripted::new(pax),
        Mode::LayaOnly,
        "safety have shelter for 12 people",
    );
    assert_eq!(content(&out), "MECP/2/H07 12pax");
    let out = classify(Scripted::new(pax), Mode::LayaOnly, "safety shelter");
    assert_eq!(content(&out), "MissingParameter");
}

#[test]
fn may_day_consumes_two_words() {
    let out = classify(
        Scripted::new(&[("may", 0.95), ("S01", 0.95)]),
        Mode::LayaOnly,
        "May they need water",
    );
    assert_eq!(content(&out), "MECP/0/S01");
    // "may" without a word that completes "may day" does not swallow the
    // next word: "injured" would otherwise be lost.
    let out = classify(
        Scripted::new(&[("may", 0.95), ("S01", 0.95)]),
        Mode::LayaOnly,
        "May injured need water",
    );
    assert_eq!(content(&out), "MissingParameter");
}

#[test]
fn an_unsure_hub_reading_is_not_a_command() {
    let out = classify(
        Scripted::new(&[("mission.marker.list", 0.5)]),
        Mode::LayaOnly,
        "delete all markers",
    );
    assert_eq!(content(&out), "NoIntent");
}

#[test]
fn a_misheard_first_word_is_kodiak_only_before_a_command() {
    // An unsure laya leaves the rules' first-word check in charge.
    let engine = Engine::new(classifier(Scripted::new(&[("help", 0.5)]), Mode::Hybrid));
    let out = engine.process_text(&TextInput::new("Kardiac urgent need food."));
    assert_eq!(result(&out), "MECP/1/S02");
    let KodiakResult::Intent { corrections, .. } = out else {
        unreachable!()
    };
    assert_eq!(corrections[0].heard, "Kardiac");
    assert_eq!(
        result(&engine.process_text(&TextInput::new("Podax, mayday, meet water."))),
        "MECP/0/S01"
    );
    for chat in [
        "I think we need water.",
        "Cardiac arrest is a medical term",
        "Koala bears sleep a lot",
        "Mayday was a great film",
    ] {
        assert_eq!(
            result(&engine.process_text(&TextInput::new(chat))),
            "chat",
            "{chat}"
        );
    }
    // laya can supply the missing piece of a misheard command.
    let engine = Engine::new(classifier(
        Scripted::new(&[("routine", 0.95)]),
        Mode::Hybrid,
    ));
    assert_eq!(
        result(&engine.process_text(&TextInput::new("Kardiac critical need water."))),
        "MECP/3/S01"
    );
}
