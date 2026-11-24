#!/bin/sh
# Builds VIctory for Windows (gnullvm) on macOS and runs its tests under Wine.
# Usage: tools/macos/test-wine.sh [cargo test arguments]
# WINE names the wine binary; it defaults to the Wine that
# tools/macos/build-crossover-wine.sh builds, and to a wine on PATH where
# that one is missing. A Wine with Apple's D3DMetal beside it gets the DLL
# overrides that make it the D3D12 device, unless WINEDLLOVERRIDES already
# says something about d3d12.
# VICTORY_REQUIRE_GPU=1 makes the tests that need a host D3D12 device fail
# where there is none; otherwise they pass, each printing
# "skipped: no host D3D12 device".
# Cargo splits the runner on spaces, so the runner is a wrapper script in a
# temporary folder without spaces.
set -e
cd "$(dirname "$0")/../.."
ROOT="$PWD"
BUILT_WINE="$HOME/Library/Caches/victory-macos/target/wine-cx/bin/wine"
if [ -z "${WINE:-}" ]; then
    if [ -x "$BUILT_WINE" ]; then
        WINE="$BUILT_WINE"
    else
        WINE="$(command -v wine || true)"
        [ -z "$WINE" ] || echo "test-wine.sh: no Wine at $BUILT_WINE, using $WINE" >&2
    fi
fi
[ -x "$WINE" ] || { echo "set WINE to a wine binary, or run tools/macos/build-crossover-wine.sh" >&2; exit 1; }
D3DMETAL=
case "${WINEDLLOVERRIDES:-}" in
    *d3d12*) ;;
    *) [ ! -d "$(dirname "$WINE")/../lib/external/D3DMetal.framework" ] || D3DMETAL=";d3d12,d3d12core,dxgi=b" ;;
esac
TARGET=x86_64-pc-windows-gnullvm
mkdir -p target/macos
# A wrapper rather than a symlink: some Wine builds find their loader next
# to the path they were started from.
LINK="$(mktemp -d /tmp/victory-wine.XXXXXX)/wine"
printf '#!/bin/sh\nexec "%s" "$@"\n' "$WINE" > "$LINK"
chmod +x "$LINK"
export WINEPREFIX="${WINEPREFIX:-$ROOT/target/wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"
# A new prefix would wait on hidden Mono and Gecko install prompts, and the
# tests need neither. Wine's crash dialog is turned off for the same reason.
export WINEDLLOVERRIDES="mscoree,mshtml=;winedbg.exe=d$D3DMETAL${WINEDLLOVERRIDES:+;$WINEDLLOVERRIDES}"
# Wine hands its environment to the tests; said here so that it stays so.
[ -z "${VICTORY_REQUIRE_GPU:-}" ] || export VICTORY_REQUIRE_GPU
export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER="$LINK"
cargo build --quiet --manifest-path tests/fixtures/hle-probe/Cargo.toml \
    --target-dir target/fixtures --target "$TARGET"
VICTORY_HLE_PROBE="Z:$(printf '%s' "$ROOT" | tr / '\\')\\target\\fixtures\\$TARGET\\debug\\hle_probe.dll"
export VICTORY_HLE_PROBE
cargo test --target "$TARGET" "$@"
