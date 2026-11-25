#!/bin/sh
# Builds VIctory for Windows (gnullvm) on macOS and runs its tests under Wine.
# Usage: tools/macos/test-wine.sh [cargo test arguments]
# WINE names the wine binary; it defaults to the Wine that
# tools/macos/build-crossover-wine.sh builds, and to a wine on PATH where
# that one is missing. A Wine with Apple's D3DMetal beside it gets the DLL
# overrides that make it the D3D12 device, unless WINEDLLOVERRIDES already
# says something about d3d12.
# The Wine prefix (WINEPREFIX) defaults to a folder outside the repository:
# the tests' temporary folders and %LOCALAPPDATA% lie inside the prefix, and
# victory_core::ensure_outside_repo refuses them inside the source tree.
# VICTORY_REQUIRE_GPU=1 makes the tests that need a host D3D12 device fail
# where there is none; otherwise they pass, each printing
# "skipped: no host D3D12 device".
# A run that is meant to prove something sets VICTORY_REQUIRE_GPU=1: without
# it a machine with no device passes these tests without running them.
# The stand-in title and its DLLs (tests/fixtures) are built here, once, in
# the profile the tests are built in, and VICTORY_FIXTURE_DIR tells the tests
# where they are: cargo cannot be started inside Wine.
# title-unwind, the fixture that unwinds, takes a second build with a profile
# of its own (unwind, or unwind-release) and lies in the folder of that name
# beside the others.
# VICTORY_DXIL_UNIX names a dxil.dll (Microsoft's DXIL validator, kept
# outside the repository) by its macOS path; the tests get it as
# VICTORY_DXIL, the Z: path Wine knows it by. VICTORY_REQUIRE_DXIL=1 makes
# the shader tests fail where the validator cannot be loaded, instead of
# printing "skipped: DXIL validation: ...". VICTORY_REQUIRE_WARP=1 and
# VICTORY_REQUIRE_DIFF=1 do the same for WARP and for the compilers of the
# differential shader tests.
# Cargo splits the runner on spaces, so the runner is a wrapper script in a
# temporary folder without spaces, removed when this script ends.
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
# Without the target's standard library the build fails on "can't find crate
# for `core`", which does not say what to do.
[ -d "$(rustc --print target-libdir --target "$TARGET" 2> /dev/null)" ] || {
    echo "test-wine.sh: the Rust target $TARGET is not installed; run: rustup target add $TARGET" >&2
    exit 1
}
# The fixtures follow the tests' profile.
PROFILE=debug
FIXTURE_PROFILE=
UNWIND_PROFILE=unwind
for a in "$@"; do
    case "$a" in
        --) break ;;
        --release | -r) PROFILE=release FIXTURE_PROFILE=--release UNWIND_PROFILE=unwind-release ;;
    esac
done
# A wrapper rather than a symlink: some Wine builds find their loader next
# to the path they were started from.
WRAPPER_DIR="$(mktemp -d /tmp/victory-wine.XXXXXX)"
trap 'rm -rf "$WRAPPER_DIR"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
LINK="$WRAPPER_DIR/wine"
printf '#!/bin/sh\nexec "%s" "$@"\n' "$WINE" > "$LINK"
chmod +x "$LINK"
export WINEPREFIX="${WINEPREFIX:-$HOME/Library/Caches/victory-macos/prefix-test}"
export WINEDEBUG="${WINEDEBUG:--all}"
# A new prefix would wait on hidden Mono and Gecko install prompts, and the
# tests need neither. Wine's crash dialog is turned off for the same reason.
export WINEDLLOVERRIDES="mscoree,mshtml=;winedbg.exe=d$D3DMETAL${WINEDLLOVERRIDES:+;$WINEDLLOVERRIDES}"
# Wine hands its environment to the tests; said here so that it stays so.
[ -z "${VICTORY_REQUIRE_GPU:-}" ] || export VICTORY_REQUIRE_GPU
[ -z "${VICTORY_REQUIRE_WARP:-}" ] || export VICTORY_REQUIRE_WARP
[ -z "${VICTORY_REQUIRE_DXIL:-}" ] || export VICTORY_REQUIRE_DXIL
[ -z "${VICTORY_REQUIRE_DIFF:-}" ] || export VICTORY_REQUIRE_DIFF
if [ -n "${VICTORY_DXIL_UNIX:-}" ]; then
    [ -f "$VICTORY_DXIL_UNIX" ] || { echo "VICTORY_DXIL_UNIX: no file at $VICTORY_DXIL_UNIX" >&2; exit 1; }
    case "$VICTORY_DXIL_UNIX" in
        /*) ;;
        *) echo "VICTORY_DXIL_UNIX must be an absolute path" >&2; exit 1 ;;
    esac
    VICTORY_DXIL="Z:$(printf '%s' "$VICTORY_DXIL_UNIX" | tr / '\\')"
    export VICTORY_DXIL
fi
export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER="$LINK"
cargo build --quiet --locked --manifest-path tests/fixtures/Cargo.toml \
    --target-dir target/fixtures --target "$TARGET" $FIXTURE_PROFILE
cargo build --quiet --locked --manifest-path tests/fixtures/Cargo.toml \
    --target-dir target/fixtures --target "$TARGET" -p title-unwind --profile "$UNWIND_PROFILE"
VICTORY_FIXTURE_DIR="Z:$(printf '%s' "$ROOT" | tr / '\\')\\target\\fixtures\\$TARGET\\$PROFILE"
export VICTORY_FIXTURE_DIR
cargo test --target "$TARGET" "$@"
