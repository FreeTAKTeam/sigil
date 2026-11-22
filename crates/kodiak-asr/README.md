# kodiak-asr

Offline speech recognizers for R3AKT Kodiak. They implement `kodiak_core::Transcriber` and take 16 kHz mono `f32` audio.

| Feature | Backend | Model, passed by the caller | Licence |
|---|---|---|---|
| `whisper` (default) | whisper.cpp via `whisper-rs` 0.16 | a ggml file, for example [`ggml-base.en.bin`](https://huggingface.co/ggerganov/whisper.cpp) | whisper.cpp: MIT; Whisper weights: MIT |
| `parakeet` | Sherpa-ONNX 2.0.2 offline transducer | directory with `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx` and `tokens.txt` ([download](https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2)) | Sherpa-ONNX: Apache-2.0; [Parakeet TDT 0.6B v2](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2): CC-BY-4.0 |

Building whisper.cpp needs CMake and libclang (`LIBCLANG_PATH`). On Windows, keep the build directory path short. The `parakeet` feature ships `onnxruntime.dll`, `onnxruntime_providers_shared.dll` and `sherpa-onnx-c-api.dll`, which must sit next to the executable.

To time recognition plus classification with the model loaded once:

```bash
cargo run --release -p kodiak-asr --features parakeet --example bench -- parakeet <model-dir> clip.wav
```
