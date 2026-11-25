#!/usr/bin/env bash
# « CI » locale : formatage, lint, tests (dont la charge 64 pistes temps réel),
# build release Linux, et vérification de compilation Windows / macOS.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== Formatage";      cargo fmt --check
echo "== Lint";           cargo clippy --all-targets -- -D warnings
echo "== Tests";          cargo test
if command -v cargo-audit >/dev/null; then
  echo "== Failles connues des dépendances (RustSec)"; cargo audit -q
else
  echo "== cargo audit ignoré (cargo install cargo-audit --locked)"
fi
echo "== Charge 64 pistes temps réel (10 s)"
cargo test --release -- --ignored realtime_64_tracks
echo "== Build release Linux"; cargo build --release
for t in x86_64-pc-windows-gnu aarch64-apple-darwin x86_64-apple-darwin; do
  if [[ "$t" == *apple-darwin && -z "${SDKROOT:-}" ]] && ! command -v xcrun >/dev/null; then
    # Depuis les encodeurs MP3 / Ogg (code C), la vérification macOS demande le SDK Apple.
    echo "== $t ignoré (SDK macOS absent : scripts/build-macos.sh sur un Mac)"
  elif rustup target list --installed | grep -qx "$t"; then
    echo "== Compilation $t"; cargo check --release --target "$t"
  else
    echo "== $t ignoré (rustup target add $t)"
  fi
done
echo "OK : tout est vert."
