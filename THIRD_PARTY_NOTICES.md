# Third-party notices

R3AKT Sigil, including the Kodiak packages under `crates/`, is licensed under EPL-2.0 (see `LICENSE`). This file lists the third-party native libraries and model files that the release bundles ship or need, with the licence each one declares and where that licence was read. Rust crate dependencies are recorded in `Cargo.lock`; each declares its licence in its own manifest.

## Speech recognition

| Component | Where it comes from | Licence | Read from |
|---|---|---|---|
| whisper.cpp 1.8.3 | Compiled into the executable from the copy bundled in `whisper-rs-sys` 0.15.0 | MIT | `whisper-rs-sys-0.15.0/whisper.cpp/LICENSE` and `CMakeLists.txt` |
| whisper-rs 0.16.0 and whisper-rs-sys 0.15.0 | Rust bindings to whisper.cpp ([codeberg.org/tazz4843/whisper-rs](https://codeberg.org/tazz4843/whisper-rs)) | Unlicense | `whisper-rs-0.16.0/Cargo.toml` and `LICENSE`; `whisper-rs-sys-0.15.0/Cargo.toml` |
| Sherpa-ONNX 2.0.2 | `sherpa-onnx` and `sherpa-onnx-sys` crates; Parakeet builds download the prebuilt `sherpa-onnx-c-api` library from the [Sherpa-ONNX releases](https://github.com/k2-fsa/sherpa-onnx/releases) and ship it next to the executable | Apache-2.0 | `sherpa-onnx-2.0.2/Cargo.toml` and `LICENSE`; `sherpa-onnx-sys-2.0.2/Cargo.toml` and `LICENSE` |
| ONNX Runtime | Shipped inside the same Sherpa-ONNX prebuilt archive (`onnxruntime.dll` and `onnxruntime_providers_shared.dll` on Windows, `libonnxruntime*.so*` on Linux), in Parakeet builds only | Not recorded here; see the ONNX Runtime project by Microsoft | — |

## Models (not part of the bundles unless added with `-ModelPath` or a model argument)

| Model | Source | Licence | Read from |
|---|---|---|---|
| NVIDIA Parakeet TDT 0.6B v2, INT8 conversion | [nvidia/parakeet-tdt-0.6b-v2](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2); INT8 files from the [Sherpa-ONNX model release](https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2) | CC-BY-4.0 | `crates/kodiak-asr/README.md` |
| Whisper `ggml-base.en.bin` | [ggerganov/whisper.cpp on Hugging Face](https://huggingface.co/ggerganov/whisper.cpp) | MIT | `crates/kodiak-asr/README.md` |

## laya (only in builds with `--features laya`)

| Component | Where it comes from | Licence | Read from |
|---|---|---|---|
| laya-candle 0.1.2 | [Trystan-SA/laya-candle](https://github.com/Trystan-SA/laya-candle), a Rust port of [Laya](https://github.com/NandhaKishorM/laya) | Apache-2.0 | `laya-candle-0.1.2/Cargo.toml`, `LICENSE` and `NOTICE` |
| laya checkpoints | [convaiinnovations/laya](https://huggingface.co/convaiinnovations/laya); not distributed with Sigil | Same licence as Laya, per the laya-candle `NOTICE` | `laya-candle-0.1.2/NOTICE` |

The release scripts do not build the `laya` feature. See [crates/kodiak-laya/NOTICE](crates/kodiak-laya/NOTICE).

## Compiled into the executable

| Component | Where it comes from | Licence | Read from |
|---|---|---|---|
| rtl-sdr-rs 0.4.3 | The RTL-SDR USB driver, from crates.io ([ccostes/rtl-sdr-rs](https://github.com/ccostes/rtl-sdr-rs)), used unmodified | MPL-2.0 | `rtl-sdr-rs-0.4.3/Cargo.toml` and `LICENSE` |
| SDR-RS DSP crates `sdr-dsp` and `sdr-types` | [jasonherald/rtl-sdr](https://github.com/jasonherald/rtl-sdr) at commit `4380abf` | MIT, as declared at that commit. The commit has no licence file, its source describes the DSP code as a port of SDR++ (GPL-3.0), and the repository has since declared GPL-2.0-or-later. Not resolved. | Workspace `Cargo.toml` at commit `4380abf` and today; `crates/sdr-dsp/src` |
| SQLite | The amalgamation bundled in `libsqlite3-sys` 0.39.1, built through `rusqlite`'s `bundled` feature | Public-domain dedication ("The author disclaims copyright to this source code") | `libsqlite3-sys-0.39.1/sqlite3/sqlite3.c`; `libsqlite3-sys-0.39.1/Cargo.toml` declares MIT for the crate |

## librtlsdr driver (only in builds with `--features librtlsdr`)

| Component | Where it comes from | Licence | Read from |
|---|---|---|---|
| librtlsdr-rs 0.3.0 | The alternative RTL-SDR USB driver, from crates.io ([jasonherald/librtlsdr-rs](https://github.com/jasonherald/librtlsdr-rs)) | GPL-2.0-or-later | `librtlsdr-rs-0.3.0/Cargo.toml` |
| libusb | The copy bundled in `libusb1-sys` 0.7.0, built through `rusb`'s `vendored` feature | LGPL-2.1 (`libusb1-sys` itself declares MIT) | `libusb1-sys-0.7.0/libusb/COPYING`; `libusb1-sys-0.7.0/Cargo.toml` |

A binary built with this feature carries GPL-2.0-or-later. The release scripts do not build it.

The files named under "Read from" are in the Cargo registry source of each crate, except `crates/kodiak-asr/README.md`, which is in this repository.
