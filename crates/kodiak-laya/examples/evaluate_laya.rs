//! Compares the deterministic baseline with laya, alone and in hybrid mode,
//! on the bundled evaluation set and the live radio transcripts.
//!
//! ```text
//! cargo run --release -p kodiak-laya --example evaluate_laya -- \
//!     models/laya/english models/laya/multilingual [--threshold 0.85] [--verbose] [--hybrid-only]
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kodiak_core::{Engine, IntentClassifier, KodiakResult, TextInput};
use kodiak_intent::DeterministicClassifier;
use kodiak_intent::dataset::label;
use kodiak_laya::{LayaClassifier, LayaOracle, Mode, Policy};
use serde_json::Value;

struct Case {
    text: String,
    /// `eval.v1`: fields the result JSON must contain.
    expect: Option<Value>,
    /// `live.v1`: the expected label and MECP content.
    label: String,
    content: Option<String>,
    source: String,
}

impl Case {
    fn is_command(&self) -> bool {
        match &self.expect {
            Some(expect) => expect["type"] == "intent",
            None => is_command(&self.label),
        }
    }

    fn expected(&self) -> String {
        match &self.expect {
            Some(expect) => expect.to_string(),
            None => format!(
                "{}{}",
                self.label,
                self.content
                    .as_ref()
                    .map(|c| format!(" {c}"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// Whether every field in `expected` appears in `actual` with the same value.
fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|a| contains(a, value))),
        _ => actual == expected,
    }
}

fn cases() -> Vec<Case> {
    let lines = |text: &'static str| text.lines().filter(|l| !l.trim().is_empty());
    let mut cases: Vec<Case> = lines(kodiak_intent::dataset::EVAL_V1)
        .map(|line| {
            let row: Value = serde_json::from_str(line).expect("valid JSON line");
            Case {
                text: row["text"].as_str().unwrap_or_default().into(),
                expect: Some(row["expect"].clone()),
                label: String::new(),
                content: None,
                source: "eval.v1".into(),
            }
        })
        .collect();
    cases.extend(lines(kodiak_intent::dataset::LIVE_V1).map(|line| {
        let row: Value = serde_json::from_str(line).expect("valid JSON line");
        Case {
            text: row["text"].as_str().unwrap_or_default().into(),
            expect: None,
            label: row["label"].as_str().unwrap_or_default().into(),
            content: row["content"].as_str().map(Into::into),
            source: row["source"].as_str().unwrap_or_default().into(),
        }
    }));
    cases
}

fn correct(case: &Case, result: &KodiakResult) -> bool {
    if let Some(expect) = &case.expect {
        let actual = serde_json::to_value(result).unwrap_or_default();
        return contains(&actual, expect);
    }
    if label(result) != case.label {
        return false;
    }
    match (&case.content, result) {
        (
            Some(content),
            KodiakResult::Intent {
                intent: kodiak_core::Intent::MecpReport(report),
                ..
            },
        ) => report.content == *content,
        _ => true,
    }
}

fn is_command(label: &str) -> bool {
    label.contains('.') || label == "help"
}

fn run(name: &str, classifier: impl IntentClassifier + 'static, cases: &[Case], verbose: bool) {
    let engine = Engine::new(classifier);
    let mut by_source: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut times = Vec::new();
    let (mut false_commands, mut missed) = (0, 0);
    let mut bins = [(0usize, 0usize); 5];
    for case in cases {
        let started = Instant::now();
        let result = engine.process_text(&TextInput::new(case.text.as_str()));
        times.push(started.elapsed());
        let ok = correct(case, &result);
        let entry = by_source.entry(case.source.as_str()).or_default();
        entry.0 += usize::from(ok);
        entry.1 += 1;
        let got = label(&result);
        if !case.is_command() && is_command(&got) {
            false_commands += 1;
        }
        if case.is_command() && !ok {
            missed += 1;
        }
        if let KodiakResult::Intent {
            confidence: Some(p),
            ..
        } = &result
        {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "0..=1"
            )]
            let bin = ((p * 5.0) as usize).min(4);
            bins[bin].0 += usize::from(ok);
            bins[bin].1 += 1;
        }
        if verbose && !ok {
            let detail = match &result {
                KodiakResult::Intent {
                    intent: kodiak_core::Intent::MecpReport(r),
                    confidence,
                    ..
                } => format!("{} p={confidence:?}", r.content),
                KodiakResult::Rejected { rejection, .. } => {
                    format!("{got} ({})", rejection.detail)
                }
                _ => got.clone(),
            };
            println!(
                "    MISS [{}] {:?}: expected {} got {detail}",
                case.source,
                case.text,
                case.expected(),
            );
        }
    }
    times.sort_unstable();
    let at = |q: usize| times[(times.len() * q / 100).min(times.len() - 1)];
    let total: Duration = times.iter().sum();
    let sources: Vec<String> = by_source
        .iter()
        .map(|(source, (ok, n))| format!("{source} {ok}/{n}"))
        .collect();
    println!(
        "  {name}: {} | chat->command {false_commands} | missed commands {missed} | mean {:?} p50 {:?} p95 {:?} max {:?}",
        sources.join(", "),
        total / u32::try_from(times.len()).unwrap_or(1),
        at(50),
        at(95),
        times[times.len() - 1],
    );
    let calibration: Vec<String> = bins
        .iter()
        .enumerate()
        .filter(|(_, (_, n))| *n > 0)
        .map(|(i, (ok, n))| {
            format!(
                "0.{}-{}: {ok}/{n}",
                i * 2,
                if i == 4 {
                    "1.0".to_owned()
                } else {
                    format!("0.{}", i * 2 + 2)
                }
            )
        })
        .collect();
    if !calibration.is_empty() {
        println!(
            "    reported confidence vs correct: {}",
            calibration.join(", ")
        );
    }
}

fn main() {
    let mut threshold = 0.85;
    let mut verbose = false;
    let mut hybrid_only = false;
    let mut checkpoints = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threshold" => {
                threshold = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--threshold takes a number");
            }
            "--verbose" => verbose = true,
            "--hybrid-only" => hybrid_only = true,
            _ => checkpoints.push(arg),
        }
    }
    let cases = cases();
    println!("{} cases, threshold {threshold}", cases.len());
    run(
        kodiak_intent::DETERMINISTIC_ID,
        DeterministicClassifier::new(),
        &cases,
        verbose,
    );
    for checkpoint in checkpoints {
        let started = Instant::now();
        let oracle = match LayaOracle::from_dir(&checkpoint) {
            Ok(oracle) => Arc::new(oracle),
            Err(error) => {
                eprintln!("{checkpoint}: {error}");
                continue;
            }
        };
        println!(
            "{checkpoint}: loaded in {:?}; clamped buckets {:?}",
            started.elapsed(),
            oracle.clamped()
        );
        let modes: &[Mode] = if hybrid_only {
            &[Mode::Hybrid]
        } else {
            &[Mode::Hybrid, Mode::LayaOnly]
        };
        for &mode in modes {
            let classifier = LayaClassifier::new(Arc::clone(&oracle), Policy { threshold, mode });
            let name = classifier.id().to_owned();
            run(&name, classifier, &cases, verbose);
        }
    }
}
