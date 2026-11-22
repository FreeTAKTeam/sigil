//! Prints laya's full answer distributions for each command (the words
//! after Kodiak), question by question as the classifier asks them: the
//! severity of the first word, the MECP event in the rest, the hub command.
//!
//! ```text
//! cargo run --release -p kodiak-laya --example probe -- models/laya/english "normal need water"
//! ```

use kodiak_intent::vocabulary::{MECP_V1, Mecp, RCH_V1, Rch};
use kodiak_laya::{Ask, COMMAND, LayaOracle, Oracle, QuestionSet, REPORT, Reply, WORD};
use serde_json::json;

fn show(asks: &[(String, Ask)], replies: &[Reply]) {
    for ((id, _), reply) in asks.iter().zip(replies) {
        let mut ranked: Vec<_> = reply.probabilities.iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(a.1));
        let top: Vec<String> = ranked
            .iter()
            .take(4)
            .map(|(label, p)| format!("{label} {p:.2}"))
            .collect();
        println!("    {id:10} {}", top.join(", "));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let checkpoint = args.next().expect("checkpoint directory");
    let oracle = LayaOracle::from_dir(&checkpoint).expect("checkpoint loads");
    let questions = QuestionSet::new(
        &Mecp::parse(MECP_V1).expect("valid"),
        &Rch::parse(RCH_V1).expect("valid"),
    );
    for text in args {
        println!("{text:?}");
        let words: Vec<&str> = text
            .split(|c: char| !(c.is_alphanumeric() || c == '\''))
            .filter(|w| !w.is_empty())
            .collect();
        let first = words.first().copied().unwrap_or_default();
        let rest = words.get(1..).unwrap_or_default().join(" ");
        for (state, asks) in [
            (json!({ WORD: first }), questions.severity()),
            (json!({ REPORT: rest }), questions.codes()),
            (json!({ COMMAND: text }), questions.commands()),
        ] {
            let replies = oracle.ask(&state, &asks).expect("laya runs");
            show(&asks, &replies);
        }
    }
}
