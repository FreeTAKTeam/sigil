//! Asks a question set written as laya JSON about each state, and prints
//! every answer distribution. For trying question framings without a rebuild.
//!
//! ```text
//! cargo run --release -p kodiak-laya --example probe_json -- \
//!     models/laya/english questions.json states.jsonl
//! ```
//!
//! Each line of the states file is a JSON value (an object whose keys the
//! instructions name in backticks, or a string). A line that is not JSON is
//! used as a plain string.

use serde_json::Value;

fn main() {
    let mut args = std::env::args().skip(1);
    let checkpoint = args.next().expect("checkpoint directory");
    let questions = args.next().expect("questions JSON file");
    let states = args.next().expect("states file");
    let agent = laya::Agent::from_dir(&checkpoint).expect("checkpoint loads");
    let raw = std::fs::read_to_string(&questions).expect("questions file");
    let set = laya::Questions::from_json(&raw).expect("valid questions");
    let raw = std::fs::read_to_string(&states).expect("states file");
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let state: Value =
            serde_json::from_str(line).unwrap_or_else(|_| Value::String(line.to_owned()));
        println!("{state}");
        let out = agent.predict(state, &set).expect("laya runs");
        for (id, question) in &set {
            let answer = out.get(id).expect("answered");
            if let Some(p) = answer.as_noul() {
                println!("    {id:12} yes {p:.2}");
                continue;
            }
            let mut ranked: Vec<(String, f32)> = question
                .labels(id)
                .expect("labels")
                .into_iter()
                .map(|label| {
                    let p = answer.probability(&label).unwrap_or(0.0);
                    (label, p)
                })
                .collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
            let top: Vec<String> = ranked
                .iter()
                .take(4)
                .map(|(label, p)| format!("{label} {p:.2}"))
                .collect();
            println!("    {id:12} {}", top.join(", "));
        }
    }
}
