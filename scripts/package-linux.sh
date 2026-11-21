#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

case "$(uname -m)" in
    x86_64) arch=x64 ;;
    aarch64) arch=arm64 ;;
    *) printf 'Unsupported Linux architecture\n' >&2; exit 1 ;;
esac

cargo build --release --locked
bundle="dist/r3akt-sigil-linux-$arch"
mkdir -p "$bundle/docs"
cp "${CARGO_TARGET_DIR:-target}/release/r3akt-sigil" gateway.example.toml README.md LICENSE "$bundle/"
cp docs/architecture.md docs/developer-guide.md "$bundle/docs/"
mkdir -p "$bundle/docs/images"
cp docs/images/sigil-logo.webp "$bundle/docs/images/"
if [ "$#" -gt 0 ]; then
    mkdir -p "$bundle/models"
    cp "$1" "$bundle/models/ggml-base.en.bin"
fi
tar -C "$bundle" -czf "$bundle.tar.gz" .
printf '%s\n' "$bundle.tar.gz"
