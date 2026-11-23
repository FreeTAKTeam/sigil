# kodiak-laya

An optional intent classifier for R3AKT Kodiak. `LayaClassifier` (`laya/2`) implements `kodiak_core::IntentClassifier` by asking a laya typed-decision model ([laya-candle](https://github.com/Trystan-SA/laya-candle) 0.1.2) which command a transcript carries.

- laya only picks labels from closed sets: an MECP severity word, an MECP code or a hub command. Distances, clock positions, nicknames, ETAs and search terms come from the deterministic parsers in `kodiak-intent`, so laya cannot produce a code or command the vocabularies do not know.
- The first-word check for a misheard activation word is the deterministic `kodiak_intent::first_word_wake`, against the engine's primary activation word, not laya.
- `Mode::Hybrid` (the default) runs the deterministic rules first and asks laya only when they do not recognize a command. An answer is used only when it reaches the threshold with a calibrated probability. `Mode::LayaOnly` lets laya pick every label and is for benchmarking.
- An intent that laya picked carries laya's probability as `confidence`; an intent the rules recognized on their own carries none.

In Sigil the crate is built only with `--features laya`, and used only when `[kodiak] classifier = "laya"`. It did not beat the rules in testing, so the rules stay the default; see `docs/kodiak-laya-benchmark.md` in the workspace.

Model files are not included. `LayaClassifier::from_dir` loads a checkpoint directory supplied by the caller (Sigil's default is `models/laya/english`, relative to `gateway.toml`).

Examples, each taking a checkpoint directory:

```bash
cargo run --release -p kodiak-laya --example evaluate_laya -- models/laya/english models/laya/multilingual --verbose [--hybrid-only]
cargo run --release -p kodiak-laya --example probe -- models/laya/english "normal need water"
cargo run --release -p kodiak-laya --example probe_json -- models/laya/english questions.json states.jsonl
```

Licence: the crate is EPL-2.0 and depends on laya-candle, which is Apache-2.0. Read `NOTICE` before distributing a binary that includes it.
