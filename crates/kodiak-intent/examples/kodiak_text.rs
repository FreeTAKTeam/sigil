//! Classify text and print each `KodiakResult` as JSON. No SDR or speech model.
//!
//! ```text
//! cargo run -p kodiak-intent --example kodiak_text -- "Kodiak urgent need food"
//! echo "Kodiak list users" | cargo run -p kodiak-intent --example kodiak_text
//! ```

use std::io::BufRead;

use kodiak_core::{Engine, TextInput};
use kodiak_intent::DeterministicClassifier;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Engine::new(DeterministicClassifier::new());
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let inputs: Vec<String> = if arguments.is_empty() {
        std::io::stdin().lock().lines().collect::<Result<_, _>>()?
    } else {
        vec![arguments.join(" ")]
    };
    for text in inputs.iter().filter(|line| !line.trim().is_empty()) {
        let result = engine.process_text(&TextInput::new(text.as_str()));
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}
