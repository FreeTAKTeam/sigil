#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

case "$(uname -m)" in
    x86_64) arch=x64 ;;
    aarch64) arch=arm64 ;;
    *) printf 'Unsupported Linux architecture\n' >&2; exit 1 ;;
esac

# The package version is the first `version = "..."` line, in [package].
version=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
if [ -z "$version" ]; then
    printf 'No package version in Cargo.toml\n' >&2
    exit 1
fi

# Parakeet is the default recognizer. WHISPER_ONLY=1 builds without ONNX Runtime.
if [ "${WHISPER_ONLY:-0}" = 1 ]; then
    backend=whisper
    cargo build --release --locked --no-default-features
else
    backend=parakeet
    cargo build --release --locked
fi
release="${CARGO_TARGET_DIR:-target}/release"
name="r3akt-sigil-$version-linux-$arch-$backend"
bundle="dist/$name"
rm -rf "$bundle"
mkdir -p "$bundle/docs"
cp "$release/r3akt-sigil" gateway.example.toml README.md LICENSE THIRD_PARTY_NOTICES.md KODIAK_EXTRACTION.md lxmf.toml send_lxmf.py test_send_lxmf.py "$bundle/"
mkdir -p "$bundle/crates/kodiak-laya"
cp crates/kodiak-laya/NOTICE "$bundle/crates/kodiak-laya/"
if [ "$backend" = parakeet ]; then
    # Sherpa-ONNX and ONNX Runtime shared libraries for Parakeet.
    cp -P "$release/libsherpa-onnx-c-api.so" "$bundle/"
    for library in "$release"/libonnxruntime*.so*; do
        if [ -e "$library" ]; then
            cp -P "$library" "$bundle/"
        fi
    done
fi
cp docs/architecture.md docs/developer-guide.md docs/kodiak.md docs/kodiak-laya-benchmark.md "$bundle/docs/"
mkdir -p "$bundle/docs/images"
cp docs/images/sigil-logo.webp "$bundle/docs/images/"
# Optional model: a Parakeet model directory or a Whisper ggml file, under its own name.
if [ "$#" -gt 0 ]; then
    mkdir -p "$bundle/models"
    cp -R "$1" "$bundle/models/"
fi
tar -C "$bundle" -czf "$bundle.tar.gz" .
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
printf '%s\n' "$bundle.tar.gz"
