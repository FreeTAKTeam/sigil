#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

case "$(uname -m)" in
    x86_64) arch=x64 ;;
    aarch64) arch=arm64 ;;
    *) printf 'Unsupported Linux architecture\n' >&2; exit 1 ;;
esac

# Parakeet is the default recognizer. WHISPER_ONLY=1 builds without ONNX Runtime.
if [ "${WHISPER_ONLY:-0}" = 1 ]; then
    cargo build --release --locked --no-default-features
else
    cargo build --release --locked
fi
release="${CARGO_TARGET_DIR:-target}/release"
bundle="dist/r3akt-sigil-linux-$arch"
mkdir -p "$bundle/docs"
cp "$release/r3akt-sigil" gateway.example.toml README.md LICENSE "$bundle/"
# Sherpa-ONNX and ONNX Runtime shared libraries for Parakeet.
for library in "$release"/libsherpa-onnx-c-api.so "$release"/libonnxruntime*.so*; do
    [ -e "$library" ] && cp -P "$library" "$bundle/"
done
cp docs/architecture.md docs/developer-guide.md docs/kodiak.md "$bundle/docs/"
mkdir -p "$bundle/docs/images"
cp docs/images/sigil-logo.webp "$bundle/docs/images/"
# Optional model: a Parakeet model directory or a Whisper ggml file.
if [ "$#" -gt 0 ]; then
    mkdir -p "$bundle/models"
    if [ -d "$1" ]; then
        cp -R "$1" "$bundle/models/"
    else
        cp "$1" "$bundle/models/ggml-base.en.bin"
    fi
fi
tar -C "$bundle" -czf "$bundle.tar.gz" .
printf '%s\n' "$bundle.tar.gz"
