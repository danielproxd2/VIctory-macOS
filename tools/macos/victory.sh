#!/bin/sh
# Builds VIctory for Windows and runs it on macOS under CrossOver Wine with
# Apple's D3DMetal. Arguments go to victory.exe, for example:
#   tools/macos/victory.sh config
#   tools/macos/victory.sh run --execute <path to the game's executable>
# WINE defaults to the Wine that tools/macos/build-crossover-wine.sh builds.
# The Wine prefix (WINEPREFIX) defaults to a folder outside the repository:
# %LOCALAPPDATA% lies inside the prefix, and the default cache_dir and
# data_dir lie under it, where nothing derived from a game may sit inside
# the source tree (docs/asset-policy.md).
set -e
cd "$(dirname "$0")/../.."
ROOT="$PWD"
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
WINE="${WINE:-$HOME/Library/Caches/victory-macos/target/wine-cx/bin/wine}"
[ -x "$WINE" ] || { echo "no Wine at $WINE: run tools/macos/build-crossover-wine.sh first" >&2; exit 1; }
[ -d "$(dirname "$WINE")/../lib/external/D3DMetal.framework" ] || {
    echo "D3DMetal is missing: copy the redist/lib folder of Apple's evaluation environment over $(dirname "$WINE")/../lib" >&2
    exit 1
}
cargo build --quiet --release -p victory --target x86_64-pc-windows-gnullvm
export WINEPREFIX="${WINEPREFIX:-$HOME/Library/Caches/victory-macos/prefix-play}"
export WINEDEBUG="${WINEDEBUG:--all}"
export WINEDLLOVERRIDES="mscoree,mshtml=;winedbg.exe=d;d3d12,d3d12core,dxgi=b${WINEDLLOVERRIDES:+;$WINEDLLOVERRIDES}"
# Rosetta tells the game about AVX and AVX2, which it may check for.
export ROSETTA_ADVERTISE_AVX=1
exec "$WINE" "$ROOT/target/x86_64-pc-windows-gnullvm/release/victory.exe" "$@"
