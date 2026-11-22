//! Write the vocabulary-generated training set as JSON Lines to stdout.
//!
//! ```text
//! cargo run -p kodiak-intent --example generate_dataset > kodiak-train.v1.jsonl
//! ```

fn main() -> Result<(), serde_json::Error> {
    for example in kodiak_intent::dataset::generate() {
        println!("{}", serde_json::to_string(&example)?);
    }
    Ok(())
}
