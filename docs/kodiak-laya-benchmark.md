# Kodiak laya benchmark (21 November 2026)

This page records whether [laya-candle](https://github.com/Trystan-SA/laya-candle) 0.1.2 can improve Kodiak's command recognition.

**Verdict for `laya/2`:** hybrid mode passes the gates (as good as the rules everywhere, no chat sent as a command). But nearly all of its gain came from a deterministic first-word check and the word "normal". Both are now in the rules (`deterministic/3`, see [Follow-up](#follow-up-deterministic3)), which reach 34/35 live on their own. laya adds one paraphrased hub command on top. It stays optional (`--features laya`, `[kodiak] classifier = "laya"`) and off by default. The results below were measured against `deterministic/2`; the first `laya/1` attempt is kept as history.

## laya/2: the operator's flow

### What changed

The `laya/1` questions asked laya abstract judgements about the whole transcript. Examples: "is this addressed to Kodiak?", "what kind of message is this?" and "which topic?". Probing showed that laya is weak at these, but very good at **literal word identity**. So `laya/2` follows the operator's flow and asks laya only that kind of question, each about the part of the message it concerns.

1. **Is the first word Kodiak?**
   - This step is **deterministic**: edit distance ≤ 3 to "kodiak", for words of 4 letters or more.
   - laya cannot do this. Its state was just the word, and questions asked whether it is "the name Kodiak or a word that sounds like it". On english, almost every word scored about 0.8 "similar" to Kodiak: "Pass", "Hey" and "Bronto" as well as "Kardiac". Multilingual put "Koala" (0.99) and "Radiac" (0.92) above "Podax" (0.46). Only the exact spelling was reliable.
   - A misheard first word only counts when the rest of the message is a recognized command. Otherwise the transcript stays chat.
2. **Not Kodiak:** chat.
3. **Kodiak:** read the rest of the message.
   - **MECP** when the first word is a severity:
     - laya is asked "Which word is `word`?", with `word` set to that word alone.
     - There is one option per spoken form: mayday, may (of "may day" or "may they"), urgent, urgently, safety, routine, normal, other. Since MECP revision 9 there are ten spoken forms (made, medei and urging added), asked as two questions of five.
     - Probabilities of forms with the same level are summed.
     - The event is then read from the text after the severity word.
   - **Contact report** (affiliation, direction, distance): these are values, so the deterministic parser decides. Yes/no questions to laya ("does it give a distance?") reached only 0.3–0.8 on real contact reports.
   - **Hub command:** laya is asked "Which hub command does `command` ask for?". The options are the commands' own phrases, in two chunks with a none option.

**Hybrid mode (the shipping candidate):**
- The rules run first. laya is asked only when they recognize nothing or miss the MECP severity.
- laya may then:
  - read a first word the vocabularies **do not know** as a severity ("normal"), after which the rules read the event from `routine <rest>`;
  - pick a plain hub command when the rules found no family at all.
- Three restrictions came from the first `laya/2` run:
  - laya may not reinterpret a known word. It read the filler "need" in "Kodiak need food" as urgent at 0.96.
  - laya does not guess MECP events. "Broke a leg" became M01 injury at 0.92, "need fod" became S01 at 1.00, and multilingual read "fell" as P01 stranded.
  - laya may not move a message the rules placed in a family. Multilingual read "urgent someone fell and broke a leg" as the hub command `help`.

**laya-only mode** lets laya pick the severity, MECP code (all 108 codes in 12 questions of ≤ 10 options, with need and have for the same supply in the same question) and hub command. Values still come from the parsers. **Rules mode** runs the same flow without laya, to show what laya itself adds.

Tools:
- `examples/probe.rs` prints every distribution question by question.
- `examples/probe_json.rs` asks any laya JSON question set about any JSON states, so framings can be tried without a rebuild.

### Results, threshold 0.85

| Classifier | eval.v1 | live | constructed | Chat sent as a command | Mean / p95 / max per message |
|---|---|---|---|---|---|
| deterministic/2 | 94/94 | 31/35 | 10/18 | 0 | 0.2 ms / 1.6 ms / 3 ms |
| laya/2 rules mode (first-word check, no laya; now `deterministic/3`) | 94/94 | 33/35 | 11/18 | 0 | 0.5 ms / 4 ms / 8 ms |
| **laya/2 english, hybrid** | **94/94** | **34/35** | **12/18** | **0** | 60 ms / 2.8 ms / 2.3 s |
| laya/2 multilingual, hybrid | 94/94 | 34/35 | 11/18 | 0 | 25 ms / 2.5 ms / 0.98 s |
| laya/2 english, alone | 72/94 | 29/35 | 13/18 | 4 | 5.2 s / 11.4 s / 12.5 s |
| laya/2 multilingual, alone | 56/94 | 31/35 | 8/18 | 6 | 2.1 s / 5.0 s / 8.4 s |

- **Latency:** hybrid latency is near zero for every command the rules read. The p95 is milliseconds because laya runs only on the few failures, which take 0.3–2.3 s.
- **"Chat sent as a command"** counts any non-command case answered with a command. The 4 and 6 in laya-only mode are eval.v1 rejections that laya turned into reports. Examples: "Kodiak need food" as MECP/1/L03, "Kodiak mayday deed water" as T04 or S01, and "we don't need water anymore" as S01 (multilingual).

### The live failures

| Transcript | deterministic/2 | laya/2 hybrid | Handled by |
|---|---|---|---|
| `Kardiac urgent need food.` | chat | MECP/1/S02 | the first-word check (deterministic) |
| `Podax, mayday, meet water.` | chat | MECP/0/S01 | the first-word check; the rules correct "meet" |
| `Kodiak normal need water.` | missing severity | MECP/3/S01 | **laya**: "normal" read as routine by both checkpoints; english gave normal 0.92–0.99 in the probes. Multilingual gave only 0.78–0.81 in probes with a slightly different option set, but passed the threshold with the shipped options |
| `God yeah. May they need water.` | chat | chat | not handled: the first word "God" is too far from "kodiak" |

Constructed cases laya adds in hybrid mode:
- "Kodiak show me the list of users" → `mission.users` (english only).

Still missed by every mode except laya-only english:
- "run out of food", "my friend is bleeding badly", "fell and broke a leg", "the road is blocked", "coffee is ready".

### What the probes showed

**laya is excellent at literal word identity:**
- "Which word is `word`?" scored 0.94–1.00 for urgent, mayday, safety and routine on english.
- "May" → may 0.98, "normal" → normal 0.99.
- Non-severity words did not reach 0.85: "list" → routine 0.69 at most, "set" 0.39.

**laya is excellent at matching events to the phrases in their option text (english):**
- "need water" S01 1.00, "run out of food" S02 1.00, "bleeding badly" M06 0.99, "road is blocked" T01 1.00.
- Hub commands: "list users" 0.99, "how much time is left" 1.00, "join the mission" 1.00.

**laya is confidently wrong often enough to matter:**
- "meet water" → T02 bridge out 0.97 (english).
- "we have water available" → S01 1.00 although H01 is in the same question.
- "need" → urgent 0.96.

**laya does not map synonyms or sounds:**
- With an option "routine or normal", "normal" scored only 0.53.
- Spelling and sound similarity are not modelled at all.

**The multilingual checkpoint is much weaker at event and command matching:**
- "list users", "leave" and "pause" stayed below 0.85.
- "severe bleeding" became P01, "road blocked" became R04.

### Decision

**Gates:**
- hybrid at least as good as the rules in every category: met;
- no chat sent as a command: met;
- p95 of 1.5 s or less: met, 2.8 ms; the slowest message took 2.3 s.

**Recommendation: keep `deterministic/2` as the default.** laya's own contribution on this data is two cases: "normal" and one paraphrased hub command. The cost is a 1.3 GiB checkpoint and up to 2 s on unrecognized messages. Ranked by value:

1. **Move the first-word check into the rules** (`DeterministicClassifier::addressed`). It is what recovers Kardiac, Podax and Codeac, and it needs no model.
2. **Add "normal" to the routine severity words.**
3. **Keep laya hybrid as the opt-in** for operators who want unknown severity words and paraphrased hub commands read. Use the english checkpoint: it was better than multilingual on every question type tested.

laya should not pick MECP events or replace the rules (laya-only mode). It is right on most clear reports but confidently wrong on near-misses and negations.

**Command:**

```text
cargo run --release -p kodiak-laya --example evaluate_laya -- models/laya/english models/laya/multilingual --verbose [--hybrid-only]
cargo run --release -p kodiak-laya --example probe -- models/laya/english "normal need water"
```

## Follow-up: deterministic/3

Both recommendations above are done:
- `DeterministicClassifier::addressed` now applies the first-word check (`kodiak_intent::first_word_wake`, shared with `kodiak-laya`). It now measures against the engine's primary activation word rather than a fixed "kodiak".
- "normal" is a routine severity word in `mecp/v1.toml`.

The rules are versioned `deterministic/3`. `laya/2` hybrid now only asks laya for first words the vocabularies don't know, so "normal" no longer reaches laya. The no-laya `rules` mode is removed, because `deterministic/3` is that flow.

| Classifier | eval.v1 | live | constructed | Chat sent as a command | Mean / p95 / max per message |
|---|---|---|---|---|---|
| **deterministic/3** | **94/94** | **34/35** | **11/18** | **0** | 0.2 ms / 1.4 ms / 2.6 ms |
| laya/2 english, hybrid | 94/94 | 34/35 | 12/18 | 0 | 44 ms / 1.8 ms / 1.7 s |
| laya/2 multilingual, hybrid | 94/94 | 34/35 | 11/18 | 0 | 19 ms / 2.1 ms / 0.8 s |

All four live failures except `God yeah. May they need water.` are now handled by the rules alone, without a model. The english hybrid's only extra recognition is "Kodiak show me the list of users" → `mission.users`. laya-only mode was not re-run: these changes don't affect it.

## History: laya/1

### Setup

**Classifier:** `crates/kodiak-laya`, `laya/1`.
- It asks typed questions in stages.
- Stage A asks three questions:
  - "Addressed to Kodiak?" (yes/no)
  - Intent family (5 options)
  - Severity (5 options)
- Then the MECP category, as two questions of 7 options.
- Then the MECP code or hub command, in chunks of at most 10 options.

**Hybrid mode, the shipping candidate:**
- It runs the deterministic rules as well.
- When laya disagrees with confidence, the command is rejected as ambiguous.
- laya can add an intent only when the rules found none, and only when every answer on its path is calibrated and at or above the threshold.

**Data:** 147 cases.
- `eval.v1`: 94 cases, the existing acceptance set.
- `live`: 35 transcripts from the Sigil radio runs of 20–21 November.
- `constructed`: 18 paraphrases and mishearings.

**Machine:** this Windows PC, CPU only, f32, no BLAS.

**Command:**

```text
cargo run --release -p kodiak-laya --example evaluate_laya -- models/laya/english models/laya/multilingual --verbose
cargo run --release -p kodiak-laya --example probe -- models/laya/multilingual "Kardiac urgent need food."
```

### Results, threshold 0.85

| Classifier | eval.v1 | live | constructed | Chat sent as a command | Mean / p95 per message |
|---|---|---|---|---|---|
| deterministic/2 | 94/94 | 31/35 | 10/18 | 0 | 0.2 ms / 1.3 ms |
| laya english, hybrid | 94/94 | 31/35 | 10/18 | 0 | 1.67 s / 1.85 s |
| laya english, alone | 9/94 | 21/35 | 5/18 | 0 | 1.68 s / 1.86 s |
| laya multilingual, hybrid | 93/94 | 31/35 | 10/18 | 0 | 0.72 s / 1.30 s |
| laya multilingual, alone | 13/94 | 21/35 | 6/18 | 1 | 0.70 s / 1.25 s |

- **Load time:** English 0.7 s, multilingual 1.9 s.
- **Calibration:** the English checkpoint ships a `choice:11+` temperature of 0.10, which laya-candle clamps. No Kodiak question has more than 10 options, so this bucket is never used.

At threshold 0.7, multilingual hybrid gets worse:
- 91/94 on eval.v1, with 4 more misses in total;
- `"Kodiak delete all markers"` became a `mission.marker.list` command;
- "need medication" was flagged as ambiguous because laya read it as M06 severe bleeding.

### Why

The probe example prints every answer distribution.

**laya is excellent at narrow lexical matches.**
- "need water" → S01 at 1.00.
- "need food" → S02 at 0.99.

**It is unreliable at the questions Kodiak most needs it for.**
- **Addressed to Kodiak?** 0.10–0.60 on every transcript, true commands included. It cannot recover a misheard wake word.
- **Intent family:**
  - The English checkpoint reads "Kodiak Mayday need water." as chat (0.44) and mecp_report at only 0.21.
  - The multilingual checkpoint reads "Kodiak normal need water." as other (0.95).
- **Severity:** English reads "Mayday" as urgent (0.54).
- **Split category questions compete badly.** English picks H "have water" (0.96) over S "need water" (0.56).

The live failures (Kardiac, Podax and "normal" as a severity) are still missed. In those cases laya is either not confident or confidently wrong.

### Decision

The gates in the plan were:
- hybrid at least as good as the rules in every category;
- no chat sent as a command;
- p95 of 1.5 s or less.

They are not met in a way that helps. At 0.85, hybrid equals the rules and adds 0.7–1.7 s per message. At 0.7, it is worse and unsafe.

Keep `deterministic/2` live. Handle the observed mishearings with vocabulary entries ("kardiac", "podax", "normal").

laya could be revisited with a checkpoint fine-tuned on Kodiak's own dataset. The five-file layout loads any fine-tune, and `kodiak_intent::dataset::generate()` already produces labelled examples.
