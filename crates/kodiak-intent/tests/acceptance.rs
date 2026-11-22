//! Acceptance tests over the evaluation set and the generated training set.

use kodiak_core::{Engine, KodiakResult, TextInput};
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
