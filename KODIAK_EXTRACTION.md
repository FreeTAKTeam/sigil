# Moving Kodiak to FreeTAKTeam/R3AKT-Kodiak

R3AKT Kodiak is developed inside the Sigil workspace for now. Its four packages are written as an independent library so they can move to their own repository without refactoring. This guide lists the rules that keep that true and the steps for the move.

## What moves

| Path | Package | Purpose |
|---|---|---|
| `crates/kodiak-core` | `kodiak-core` | Result contract (`kodiak.result.v2`), wake-word gate, `Engine`, `Transcriber` and `IntentClassifier` traits |
| `crates/kodiak-asr` | `kodiak-asr` | Offline recognizers: whisper.cpp (feature `whisper`, default) and Parakeet TDT via Sherpa-ONNX (feature `parakeet`) |
| `crates/kodiak-intent` | `kodiak-intent` | Versioned vocabularies, the deterministic classifier, datasets, the text CLI and evaluation examples |
| `crates/kodiak-laya` | `kodiak-laya` | Optional laya typed-decision classifier (`laya/2`), its questions and the `evaluate_laya` benchmark. Depends on `laya-candle` (Apache-2.0): binaries that include it are GPLv3-or-later; see its `NOTICE`. Model files are not part of the package. |

Everything else stays in Sigil. That includes `src/intents.rs`, which converts Kodiak results into RCH LXMF messages, and `tests/kodiak_isolation.rs`.

## Rules that keep the packages movable

These rules hold today, and `tests/kodiak_isolation.rs` enforces them on every `cargo test`:

- No Kodiak manifest uses `workspace = true` inheritance. Each sets its own edition, rust-version, licence and lints.
- Path dependencies point only at sibling Kodiak packages (`../kodiak-*`). Each path dependency also carries a `version`, so the manifests stay valid after the move.
- No Kodiak source or manifest names Sigil, LXMF, Reticulum, RTL-SDR, the SDR DSP crates, Yodel or `r3akt-map-core`.
- `include_str!` reads only files inside the package. The vocabularies and datasets live under `crates/kodiak-intent/`.
- Model paths are always supplied by the caller. Kodiak has no default model location and no absolute paths.
- Tests and examples need no SDR hardware. Only `kodiak-asr`'s `bench` example and `kodiak-laya`'s examples need model files.
- Dependencies point one way: Sigil → Kodiak. `kodiak-asr` and `kodiak-intent` depend on `kodiak-core`. `kodiak-laya` depends on `kodiak-core` and `kodiak-intent`. `kodiak-asr` uses `kodiak-intent` only as a dev-dependency, for its benchmark.

## Rehearsal (21 November 2026)

The `kodiak-core`, `kodiak-asr` and `kodiak-intent` directories were copied, unchanged, into an empty folder; `kodiak-laya` was not part of the rehearsal. The folder also got Sigil's `rust-toolchain.toml` and `Cargo.lock`, plus this root manifest:

```toml
[workspace]
members = ["crates/kodiak-core", "crates/kodiak-asr", "crates/kodiak-intent"]
resolver = "3"
```

`cargo test --offline --release --workspace --features kodiak-asr/parakeet` passed all 15 Kodiak tests. That build compiled both whisper.cpp and Sherpa-ONNX. `cargo tree -p kodiak-intent -e normal` shows that `kodiak-intent` depends only on `kodiak-core`, `serde`, `serde_json` and `toml`.

On Windows, whisper.cpp's CMake build fails if the build directory path is too long (MSBuild `FTK1011`). Keep the checkout or `CARGO_TARGET_DIR` in a short path.

## Steps

1. **Create the repository** `FreeTAKTeam/R3AKT-Kodiak`. Decide its licence before publishing: the packages currently declare Sigil's `GPL-2.0-or-later`. If you change it, change the `license` field in all four manifests. `kodiak-laya` links Apache-2.0 code, which GPL-2.0-only cannot accept.
2. **Carry the history.** In a fresh clone of Sigil, keep only the Kodiak paths and move them to the new repository's root layout:

   ```bash
   git filter-repo --path crates/kodiak-core --path crates/kodiak-asr --path crates/kodiak-intent --path crates/kodiak-laya --path KODIAK_EXTRACTION.md
   ```

   If history is not needed, copy the four directories instead.
3. **Add the root files:** the workspace manifest shown above with `"crates/kodiak-laya"` added to `members`, a copy of `rust-toolchain.toml` (Rust 1.96.0), `LICENSE`, a short `README.md` that links to the package READMEs, and CI that runs `cargo clippy --workspace --all-targets` and `cargo test --workspace`. CI needs CMake and libclang for whisper.cpp; run it with `--features kodiak-asr/parakeet` as well.
4. **Generate a lockfile** with `cargo generate-lockfile`. To keep the versions Sigil was tested with, start from a copy of Sigil's `Cargo.lock` and run `cargo update --workspace`, which prunes unused entries.
5. **Tag a release** such as `v0.1.0`, and record its commit hash.
6. **Switch Sigil to the pinned Git dependency.** Delete `crates/kodiak-*`, remove the `[workspace]` section from Sigil's `Cargo.toml`, and replace the four path dependencies:

   ```toml
   kodiak-asr = { git = "https://github.com/FreeTAKTeam/R3AKT-Kodiak.git", rev = "<commit>" }
   kodiak-core = { git = "https://github.com/FreeTAKTeam/R3AKT-Kodiak.git", rev = "<commit>" }
   kodiak-intent = { git = "https://github.com/FreeTAKTeam/R3AKT-Kodiak.git", rev = "<commit>" }
   kodiak-laya = { git = "https://github.com/FreeTAKTeam/R3AKT-Kodiak.git", rev = "<commit>", optional = true }
   ```

   Keep `parakeet = ["kodiak-asr/parakeet"]` and `laya = ["dep:kodiak-laya"]` in Sigil's features.
7. **Update Sigil's checks.** Delete `tests/kodiak_isolation.rs`; the separate repository now enforces the boundary. Run `cargo test --release --locked` and `scripts/package-windows.ps1`. The packaging script still finds the Sherpa-ONNX DLLs in `target/release` (or `$CARGO_TARGET_DIR/release`).
8. **Move the documentation.** The Kodiak section of `docs/kodiak.md` and the package READMEs go to the new repository. Sigil keeps the integration part (`src/intents.rs`, the `[kodiak]` configuration keys and the RCH message mapping).

## Contract ownership

| Type | Owner | Notes |
|---|---|---|
| `KodiakResult`, `Intent`, `MecpReport`, `ContactReport`, `RchCommand`, `Rejection`, `RejectReason`, `Category`, `Affiliation`, `Direction` | `kodiak-core` | Serialized as `kodiak.result.v2` (reads v1). An incompatible change needs a new schema version. |
| `AudioInput`, `TextInput`, `Engine`, `Transcriber`, `IntentClassifier`, `WakeWord`, `KodiakError` | `kodiak-core` | |
| MECP, contact and RCH vocabularies (`kodiak.vocabulary.*`, version 1) | `kodiak-intent` | MECP codes mirror REM and RCH. RCH intent names are RCH's canonical command names. |
| `Plan`, RCH field-command encoding, contact geodesy, position and heading policy | Sigil (`src/intents.rs`) | Never moves into Kodiak |
| `sdr.gateway.v1` event, including the embedded `kodiak` result | Sigil | |
