//! Acceptance tests: the evaluation set, the generated training set, the
//! regression set (recorded Parakeet transcripts and the REQ-07 cases), every
//! MECP code with every severity, and DDD permutations.

use kodiak_core::{Affiliation, ContactReport, Direction, Engine, Intent, KodiakResult, TextInput};
use kodiak_intent::DeterministicClassifier;
use serde_json::Value;

const EVAL: &str = include_str!("../datasets/eval.v1.jsonl");

/// Whether every field in `expected` appears in `actual` with the same value.
fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|found| contains(found, value))),
        _ => actual == expected,
    }
}

#[test]
fn evaluation_set_matches_expected_results() {
    let engine = Engine::new(DeterministicClassifier::new());
    let mut failures = Vec::new();
    let mut count = 0;
    for line in EVAL.lines().filter(|line| !line.trim().is_empty()) {
        let case: Value = serde_json::from_str(line).unwrap();
        let text = case["text"].as_str().unwrap();
        let result = engine.process_text(&TextInput::new(text));
        let actual = serde_json::to_value(&result).unwrap();
        if !contains(&actual, &case["expect"]) {
            failures.push(format!(
                "{text}\n  expected {}\n  actual   {actual}",
                case["expect"]
            ));
        }
        // Every result must round-trip through the versioned contract.
        assert_eq!(
            serde_json::from_value::<KodiakResult>(actual).unwrap(),
            result
        );
        count += 1;
    }
    assert!(count >= 70, "evaluation set has {count} cases");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn generated_training_set_is_consistent_with_the_baseline() {
    let engine = Engine::new(DeterministicClassifier::new());
    let examples = kodiak_intent::dataset::generate();
    assert!(examples.len() > 500, "{} examples", examples.len());
    let failures: Vec<String> = examples
        .iter()
        .filter_map(|example| {
            let result = engine.process_text(&TextInput::new(example.text.as_str()));
            let label = kodiak_intent::dataset::label(&result);
            (label != example.label)
                .then(|| format!("{:?}: expected {} got {label}", example.text, example.label))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn every_mecp_code_is_reachable_by_voice() {
    let mecp = kodiak_intent::vocabulary::Mecp::parse(kodiak_intent::vocabulary::MECP_V1).unwrap();
    let labels: std::collections::BTreeSet<String> = kodiak_intent::dataset::generate()
        .into_iter()
        .map(|example| example.label)
        .collect();
    for code in &mecp.codes {
        assert!(
            labels.contains(&format!("mecp.report/{}", code.code)),
            "{}",
            code.code
        );
    }
}

/// Per-suite results of the regression set: (suite, passed, failures).
fn regression() -> Vec<(String, usize, Vec<String>)> {
    let engine = Engine::new(DeterministicClassifier::new());
    let mut suites: std::collections::BTreeMap<String, (usize, Vec<String>)> =
        std::collections::BTreeMap::new();
    for line in kodiak_intent::dataset::REGRESSION_V2
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let case: Value = serde_json::from_str(line).unwrap();
        let text = case["text"].as_str().unwrap();
        let expect = &case["expect"];
        let result = engine.process_text(&TextInput::new(text));
        let actual = serde_json::to_value(&result).unwrap();
        let entry = suites
            .entry(case["suite"].as_str().unwrap().to_owned())
            .or_default();
        // A case that must not execute may never come back as an intent.
        let must_not_execute = expect["type"] != "intent";
        if contains(&actual, expect) && !(must_not_execute && actual["type"] == "intent") {
            entry.0 += 1;
        } else {
            entry.1.push(format!(
                "[{}] {text}\n  expected {expect}\n  actual   {actual}",
                case["source"].as_str().unwrap_or("?")
            ));
        }
        assert_eq!(
            serde_json::from_value::<KodiakResult>(actual).unwrap(),
            result
        );
    }
    suites
        .into_iter()
        .map(|(suite, (passed, failures))| (suite, passed, failures))
        .collect()
}

#[test]
fn regression_suite_passes_for_mecp_ddd_chat_and_rch() {
    let results = regression();
    let mut failures = Vec::new();
    for (suite, passed, failed) in &results {
        println!("{suite}: {passed} passed, {} failed", failed.len());
        failures.extend(failed.iter().cloned());
    }
    let suites: Vec<&str> = results.iter().map(|(s, _, _)| s.as_str()).collect();
    assert_eq!(suites, ["chat", "ddd", "mecp", "rch"]);
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn contact_of(result: &KodiakResult) -> Option<&ContactReport> {
    match result {
        KodiakResult::Intent {
            intent: Intent::ContactReport(report),
            ..
        } => Some(report),
        _ => None,
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "the test tables are inline")]
fn ddd_components_in_any_order_and_with_any_punctuation() {
    let engine = Engine::new(DeterministicClassifier::new());
    let descriptions = [
        ("hostile", Affiliation::Hostile, None, None),
        ("friendly", Affiliation::Friendly, None, None),
        ("neutral", Affiliation::Neutral, None, None),
        ("unknown", Affiliation::Unknown, None, None),
        ("suspect", Affiliation::Suspect, None, None),
        (
            "hostile two vehicles",
            Affiliation::Hostile,
            Some("vehicle"),
            Some(2),
        ),
        (
            "enemy three tanks",
            Affiliation::Hostile,
            Some("armor"),
            Some(3),
        ),
    ];
    let distances = [
        ("three hundred meters", 300),
        ("300 meters", 300),
        ("300m", 300),
        ("half a kilometer", 500),
        ("one kilometer", 1000),
        ("1.5 km", 1500),
    ];
    let directions = [
        (
            "6 o'clock",
            Direction::RelativeClock {
                relative_bearing_deg: 180,
            },
        ),
        (
            "twelve o'clock",
            Direction::RelativeClock {
                relative_bearing_deg: 0,
            },
        ),
        (
            "3:30",
            Direction::RelativeClock {
                relative_bearing_deg: 105,
            },
        ),
        (
            "nine o'clock",
            Direction::RelativeClock {
                relative_bearing_deg: 270,
            },
        ),
        ("bearing 045", Direction::CompassBearing { bearing_deg: 45 }),
        (
            "270 degrees",
            Direction::CompassBearing { bearing_deg: 270 },
        ),
        ("south west", Direction::CompassBearing { bearing_deg: 225 }),
    ];
    let separators = [" ", ", ", ". ", "; ", " - ", "... "];
    let orders: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut checked = 0;
    let mut failures = Vec::new();
    for (description, affiliation, classification, quantity) in descriptions {
        for (distance, metres) in distances {
            for (direction, expected) in directions {
                for order in orders {
                    // A clock hour right before a spoken number reads as the
                    // half hour ("six o'clock three hundred" is fine, "3:30
                    // three hundred" too), so every order must parse.
                    let parts = [description, distance, direction];
                    let separator = separators[checked % separators.len()];
                    let text = format!(
                        "Kodiak{separator}{}{separator}{}{separator}{}.",
                        parts[order[0]], parts[order[1]], parts[order[2]]
                    );
                    checked += 1;
                    let result = engine.process_text(&TextInput::new(text.as_str()));
                    let ok = contact_of(&result).is_some_and(|report| {
                        report.affiliation == affiliation
                            && report.distance_m == metres
                            && report.direction == expected
                            && report.classification.as_deref() == classification
                            && report.quantity == quantity
                    });
                    if !ok {
                        failures.push(format!(
                            "{text}: {}",
                            serde_json::to_string(&result).unwrap()
                        ));
                    }
                }
            }
        }
    }
    assert!(checked > 1500, "{checked}");
    assert!(
        failures.is_empty(),
        "{} of {checked} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn every_mecp_code_and_phrase_with_every_severity() {
    let engine = Engine::new(DeterministicClassifier::new());
    let mecp = kodiak_intent::vocabulary::Mecp::parse(kodiak_intent::vocabulary::MECP_V1).unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for severity in &mecp.severities {
        for word in &severity.words {
            for code in &mecp.codes {
                for phrase in &code.phrases {
                    let text = format!("Kodiak {word} {phrase}");
                    checked += 1;
                    let expected = format!("MECP/{}/{}", severity.level, code.code);
                    match engine.process_text(&TextInput::new(text.as_str())) {
                        KodiakResult::Intent {
                            intent: Intent::MecpReport(report),
                            ..
                        } if report.content == expected => {}
                        other => failures.push(format!(
                            "{text}: expected {expected}, got {}",
                            serde_json::to_string(&other).unwrap()
                        )),
                    }
                }
            }
        }
    }
    assert!(checked > 3000, "{checked}");
    assert!(
        failures.is_empty(),
        "{} of {checked} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
