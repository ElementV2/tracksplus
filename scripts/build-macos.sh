#!/usr/bin/env bash
# Build macOS (à lancer sur un Mac) : application universelle Apple Silicon + Intel.
# Prérequis : Xcode Command Line Tools (xcode-select --install) et Rust (rustup).
set -euo pipefail
cd "$(dirname "$0")/.."
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
mkdir -p dist
APP=dist/TracksPlus.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
lipo -create -output "$APP/Contents/MacOS/TracksPlus" \
  target/aarch64-apple-darwin/release/tracksplus \
  target/x86_64-apple-darwin/release/tracksplus
cp packaging/macos/Info.plist "$APP/Contents/Info.plist"
codesign --force --deep -s - "$APP"
echo "OK : $APP (premier lancement : clic droit → Ouvrir)"
