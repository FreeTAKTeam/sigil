# kodiak-intent

Versioned vocabularies and intent classifiers for R3AKT Kodiak.

- `vocabularies/mecp/v1.toml`: MECP severities and all 108 event codes shared by REM and RCH, with spoken phrases.
- `vocabularies/contact/v1.toml`: affiliations, clock positions, bearings, units, classifications and activities for contact reports.
- `vocabularies/rch/v1.toml`: spoken phrases for RCH's canonical command names, including `mission.time` and `mission.library.list` (with an optional `search`).
- `DeterministicClassifier`: rule-based classifier over those vocabularies (`deterministic/5`). It first corrects unknown words to the closest-sounding vocabulary word when one is clearly closest, and reports each correction. It then rejects anything it cannot parse completely, and never reports a confidence value. When the activation word is missing, a first word close to "kodiak" ("Kardiac", "Podax") still activates if the rest is a recognized command (`first_word_wake`).
- `dataset::generate()`: about 1,000 labelled utterances generated from the vocabularies, for training a model.
- `datasets/eval.v1.jsonl`: a hand-written evaluation set of 94 utterances.

Examples, none of which need hardware or a speech model:

```bash
cargo run -p kodiak-intent --example kodiak_text -- "Kodiak urgent need food"
cargo run -p kodiak-intent --example generate_dataset > kodiak-train.v1.jsonl
cargo run --release -p kodiak-intent --example evaluate
```

A model-based classifier can replace `DeterministicClassifier` behind `kodiak_core::IntentClassifier`. "Intent-SLM" has no published weights; see `docs/kodiak.md` for the model status and licences. The vocabulary files are data, not code: any change to them must keep `cargo test -p kodiak-intent` passing.
