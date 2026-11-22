//! Accuracy and latency of text classification on the bundled datasets.
//!
//! Latency is parsing only: the input is text, so no speech recognition time
//! is included. The regression set is reported separately for MECP, DDD
//! contact reports, ordinary radio speech (chat) and RCH commands.
//!
//! ```text
//! cargo run --release -p kodiak-intent --example evaluate
//! ```

use std::time::{Duration, Instant};

use kodiak_core::{Engine, TextInput};
use kodiak_intent::DeterministicClassifier;

fn summary(name: &str, mut times: Vec<Duration>, correct: Option<usize>) {
    times.sort_unstable();
    let total: Duration = times.iter().sum();
    let count = u32::try_from(times.len()).unwrap_or(u32::MAX);
    let at = |q: usize| times[(times.len() * q / 100).min(times.len() - 1)];
    println!(
        "{name}: {} utterances{}, mean {:?}, p50 {:?}, p95 {:?}, max {:?}",
        times.len(),
        correct
            .map(|n| format!(", {n} labelled correctly"))
            .unwrap_or_default(),
        total / count,
        at(50),
        at(95),
        times[times.len() - 1]
    );
}

fn main() {
    let started = Instant::now();
    let engine = Engine::new(DeterministicClassifier::new());
    println!(
        "Engine construction (parse and validate vocabularies): {:?}",
        started.elapsed()
    );

    let examples = kodiak_intent::dataset::generate();
    let mut times = Vec::new();
    let mut correct = 0;
    for example in &examples {
        let input = TextInput::new(example.text.as_str());
        let started = Instant::now();
        let result = engine.process_text(&input);
        times.push(started.elapsed());
        correct += usize::from(kodiak_intent::dataset::label(&result) == example.label);
    }
    summary("Generated set", times, Some(correct));

    let mut times = Vec::new();
    for line in include_str!("../datasets/eval.v1.jsonl")
        .lines()
        .filter(|l| !l.is_empty())
    {
        let case: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        let input = TextInput::new(case["text"].as_str().unwrap_or_default());
        let started = Instant::now();
        let _ = engine.process_text(&input);
        times.push(started.elapsed());
    }
    summary(
        "Evaluation set (correctness is checked by the acceptance test)",
        times,
        None,
    );

    let mut suites: std::collections::BTreeMap<String, (Vec<Duration>, usize)> =
        std::collections::BTreeMap::new();
    for line in kodiak_intent::dataset::REGRESSION_V2
        .lines()
        .filter(|l| !l.is_empty())
    {
        let case: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        let input = TextInput::new(case["text"].as_str().unwrap_or_default());
        let started = Instant::now();
        let result = engine.process_text(&input);
        let elapsed = started.elapsed();
        let actual = serde_json::to_value(&result).expect("serializable result");
        let entry = suites
            .entry(case["suite"].as_str().unwrap_or("?").to_owned())
            .or_default();
        entry.0.push(elapsed);
        entry.1 += usize::from(contains(&actual, &case["expect"]));
    }
    for (suite, (times, correct)) in suites {
        summary(&format!("Regression set, {suite}"), times, Some(correct));
    }
}

/// Whether every field in `expected` appears in `actual` with the same value.
fn contains(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (actual, expected) {
        (serde_json::Value::Object(actual), serde_json::Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|found| contains(found, value))),
        _ => actual == expected,
    }
}
