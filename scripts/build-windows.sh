#!/usr/bin/env bash
# Build Windows (64 bits) avec ASIO, depuis Linux, sans Windows ni Visual Studio.
# Prérequis (Debian/Ubuntu) : apt install mingw-w64 clang ; rustup target add x86_64-pc-windows-gnu
# Le SDK ASIO (Steinberg, double licence GPLv3 / propriétaire) est récupéré automatiquement.
set -euo pipefail
cd "$(dirname "$0")/.."

SDK="${CPAL_ASIO_DIR:-$PWD/target/asio-sdk}"
# Miroir du SDK épinglé sur un commit vérifié : ses sources sont compilées dans
# l'exécutable distribué, une modification du dépôt ne doit jamais s'y retrouver.
ASIO_MIRROR=https://github.com/audiosdk/asio.git
ASIO_COMMIT=496a0765b8bb9c26f764f22f9a9712a937177db2
if [ ! -f "$SDK/host/asiodrivers.h" ]; then
  echo "== Récupération du SDK ASIO (commit $ASIO_COMMIT)"
  rm -rf "$SDK"
  git init -q "$SDK"
  git -C "$SDK" fetch -q --depth 1 "$ASIO_MIRROR" "$ASIO_COMMIT"
  git -C "$SDK" -c advice.detachedHead=false checkout -q FETCH_HEAD
fi
# Vérifié à chaque compilation, même avec un SDK déjà présent.
if [ -d "$SDK/.git" ]; then
  HEAD=$(git -C "$SDK" rev-parse HEAD)
  if [ "$HEAD" != "$ASIO_COMMIT" ] || [ -n "$(git -C "$SDK" status --porcelain)" ]; then
    echo "ERREUR : SDK ASIO différent du commit épinglé $ASIO_COMMIT (supprimez $SDK)" >&2
    exit 1
  fi
fi
export CPAL_ASIO_DIR="$SDK"
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="--target=x86_64-pc-windows-gnu --sysroot=/usr/x86_64-w64-mingw32 -I/usr/x86_64-w64-mingw32/include"

echo "== Compilation (ASIO + WASAPI)"
cargo build --release --target x86_64-pc-windows-gnu --features asio

EXE=target/x86_64-pc-windows-gnu/release/tracksplus.exe
# L'exécutable doit être autonome : aucune DLL MinGW ne doit être requise.
if x86_64-w64-mingw32-objdump -p "$EXE" | grep -iE "DLL Name: (libstdc|libgcc|libwinpthread)"; then
  echo "ERREUR : l'exécutable dépend de DLL MinGW" >&2
  exit 1
fi
mkdir -p dist
cp "$EXE" dist/TracksPlus.exe
echo "OK : dist/TracksPlus.exe"
