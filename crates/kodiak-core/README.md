# kodiak-core

Contracts and the processing engine of R3AKT Kodiak, an offline voice-command engine.

- `Engine::process_audio(&AudioInput)`: transcribes the audio, checks for the activation word, then classifies. Returns `Result<KodiakResult, KodiakError>`.
- `Engine::process_text(&TextInput)`: starts at the activation-word check. Returns `KodiakResult`.
- `KodiakResult` is `Chat`, `Intent` (MECP report, contact report or RCH command) or `Rejected`. Its JSON form is the versioned contract `kodiak.result.v2` (version 1 documents are still read).
- `Transcriber` and `IntentClassifier` are the replaceable parts. `kodiak-asr` and `kodiak-intent` provide implementations. `IntentClassifier::addressed(transcript, wake)` may accept a misheard activation word and return its classification for the engine to reuse.
- `WakeWord` matches "Kodiak" as the first word of the message, case-insensitively, plus the spellings that the bundled recognizers were observed to produce; `WakeWord::with_forms` replaces them. The engine drops repeated activation words at the start of a command (`WakeWord::strip_leading`).
- `Engine::with_transcriber_loader` loads the speech model on the first audio message. A failed load is retried at most every `ASR_RETRY_INTERVAL` (30 s), and the model is reloaded after the recognizer panics.
- Input bounds: audio over `DEFAULT_MAX_AUDIO_SECONDS` (300) is `InvalidAudio`; a command over `DEFAULT_MAX_COMMAND_WORDS` (128) words is rejected as `invalid_parameter`. Both are adjustable on `Engine`.

There are no features. Dependencies are `serde` and `serde_json` only. See `docs/kodiak.md` in the workspace for the full contract.
