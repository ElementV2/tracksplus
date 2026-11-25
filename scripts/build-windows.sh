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
# Empreinte des sources (.cpp, .h, .c : « chemin:SHA-256 » triés, joints par des retours
# à la ligne), même calcul que Get-SdkTreeHash dans package-windows.ps1. Deux formes du
# même SDK 2.3.4 : l'archive officielle de Steinberg (celle de package-windows.ps1), et
# ce miroir au commit épinglé (fins de ligne du dépôt).
ASIO_TREE_ARCHIVE=A766723D8D7D78BCD85A9F7F0F8ED5FF62A35620D1A58FBDB6864FF335AAE6A6
ASIO_TREE_MIRROR=7D50CEAD9BC096C64253F33D60C3FC6C33531928724187E31B755E086F4BB090

sdk_tree_hash() {
  (
    cd "$1"
    find . -path ./.git -prune -o -type f \( -iname '*.cpp' -o -iname '*.h' -o -iname '*.c' \) -print |
      while IFS= read -r f; do
        f=${f#./}
        printf '%s:%s\n' "$f" "$(sha256sum -- "$f" | cut -d' ' -f1 | tr 'a-f' 'A-F')"
      done |
      LC_ALL=C sort |
      awk 'NR > 1 { printf "\n" } { printf "%s", $0 }' |
      sha256sum | cut -d' ' -f1 | tr 'a-f' 'A-F'
  )
}

if [ ! -f "$SDK/host/asiodrivers.h" ]; then
  echo "== Récupération du SDK ASIO (commit $ASIO_COMMIT)"
  rm -rf "$SDK"
  git init -q "$SDK"
  git -C "$SDK" -c core.autocrlf=false fetch -q --depth 1 "$ASIO_MIRROR" "$ASIO_COMMIT"
  git -C "$SDK" -c core.autocrlf=false -c advice.detachedHead=false checkout -q FETCH_HEAD
fi
# Vérifié à chaque compilation, même avec un SDK déjà présent, venu du miroir, de
# l'archive ou de CPAL_ASIO_DIR : ses sources sont compilées dans l'exécutable.
if [ -d "$SDK/.git" ]; then
  HEAD=$(git -C "$SDK" rev-parse HEAD)
  if [ "$HEAD" != "$ASIO_COMMIT" ] || [ -n "$(git -C "$SDK" status --porcelain)" ]; then
    echo "ERREUR : SDK ASIO différent du commit épinglé $ASIO_COMMIT (supprimez $SDK)" >&2
    exit 1
  fi
fi
TREE=$(sdk_tree_hash "$SDK")
if [ "$TREE" != "$ASIO_TREE_ARCHIVE" ] && [ "$TREE" != "$ASIO_TREE_MIRROR" ]; then
  # Autre version assumée : TRACKSPLUS_ACCEPT_ASIO_SDK doit donner cette empreinte exacte.
  if [ "${TRACKSPLUS_ACCEPT_ASIO_SDK:-}" = "$TREE" ]; then
    echo "ATTENTION : SDK ASIO non épinglé accepté ($TREE)" >&2
  else
    echo "ERREUR : sources du SDK ASIO de $SDK différentes de la version épinglée ($TREE)." >&2
    echo "Supprimez $SDK pour le récupérer, ou posez TRACKSPLUS_ACCEPT_ASIO_SDK=$TREE pour l'accepter." >&2
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
