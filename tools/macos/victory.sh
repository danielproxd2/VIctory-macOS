#!/bin/sh
# Builds VIctory for Windows and runs it on macOS under CrossOver Wine with
# Apple's D3DMetal. Arguments go to victory.exe, for example:
#   tools/macos/victory.sh config
#   tools/macos/victory.sh run --execute <path to the game's executable>
# WINE defaults to the Wine that tools/macos/build-crossover-wine.sh builds.
# The Wine prefix (WINEPREFIX) defaults to a folder outside the repository:
# %LOCALAPPDATA% lies inside the prefix, and the default cache_dir lies
# under it, where nothing derived from a game may sit inside the source
# tree (docs/asset-policy.md).
# What titles save does not belong in that prefix, which is under
# ~/Library/Caches and may be cleared: unless VICTORY_DATA_DIR or the config
# file names a data_dir, VICTORY_DATA_DIR is set to
# ~/Library/Application Support/victory/data, as Wine's Z: drive shows it.
# CARGO names the cargo to build with (default: cargo on PATH).
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
"${CARGO:-cargo}" build --quiet --release -p victory --target x86_64-pc-windows-gnullvm
export WINEPREFIX="${WINEPREFIX:-$HOME/Library/Caches/victory-macos/prefix-play}"
export WINEDEBUG="${WINEDEBUG:--all}"
export WINEDLLOVERRIDES="mscoree,mshtml=;winedbg.exe=d;d3d12,d3d12core,dxgi=b${WINEDLLOVERRIDES:+;$WINEDLLOVERRIDES}"
# The config file victory.exe will read from here, by its macOS name: Wine
# shows / as Z:, and a relative name is taken from the repository's root.
CONFIG="$(printf '%s' "${VICTORY_CONFIG:-victory.toml}" | tr '\\' /)"
case "$CONFIG" in [Zz]:/*) CONFIG="${CONFIG#??}" ;; esac
if [ -n "${VICTORY_DATA_DIR:-}" ]; then
    : # The user's choice.
elif [ -n "${VICTORY_CONFIG:-}" ] && [ ! -r "$CONFIG" ]; then
    # It may name a data_dir, which a variable set here would override.
    printf '%s\n' "victory.sh: cannot read the config file $VICTORY_CONFIG from macOS; unless it names a data_dir, what titles save goes to the Wine prefix under ~/Library/Caches" >&2
elif grep -Eq '^[^#]*data_dir[[:space:]]*=' "$CONFIG" 2> /dev/null; then
    : # The config file's choice.
else
    case "$HOME" in
        /*) export VICTORY_DATA_DIR="Z:$(printf '%s' "$HOME/Library/Application Support/victory/data" | tr / '\\')" ;;
    esac
fi
# Rosetta tells the game about AVX and AVX2, which it may check for.
export ROSETTA_ADVERTISE_AVX=1
exec "$WINE" "$ROOT/target/x86_64-pc-windows-gnullvm/release/victory.exe" "$@"
