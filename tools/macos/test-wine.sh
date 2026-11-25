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
# A test that lacks something it needs passes after printing one line
# "skipped: <reason> (<test>)" past the test harness's capture, so
#   tools/macos/test-wine.sh ... 2>&1 | grep 'skipped: '
# lists what a run left out. One switch per need makes the skip a failure.
# Every switch the tree reads is listed here and handed on to the tests
# (xtask/tests/shell_scripts.rs checks both against the sources);
# docs/macos.md, "What a test run did not check", says more about each.
#   VICTORY_REQUIRE_GPU=1     the host D3D12 device; without it the device
#                             tests print "skipped: no host D3D12 device".
#   VICTORY_REQUIRE_DXIL=1    the DXIL validator (VICTORY_DXIL_UNIX, below);
#                             "skipped: DXIL validation: ...".
#   VICTORY_REQUIRE_CLAMP=1   an argument clamp for indirect draws: the
#                             prebuilt bytecode is current, or the host can
#                             compile clamp.hlsl; "skipped: the prebuilt
#                             clamp bytecode is older than clamp.hlsl ...".
#   VICTORY_REQUIRE_HLSL=1    a compiler that builds the shaders some
#                             device tests write themselves; Wine's
#                             (vkd3d-shader) does not, so on a Mac those
#                             tests need the DXC that VICTORY_PC_DXC_UNIX
#                             names (below); without one: "skipped: the
#                             host HLSL compiler cannot build this test
#                             shader", and this switch fails them.
#   VICTORY_REQUIRE_WARP=1    the WARP software device, which D3DMetal does
#                             not have; "skipped: no SM 6.6 WARP device".
#   VICTORY_REQUIRE_DIFF=1    the console and PC shader compilers of the
#                             differential tests (VICTORY_CONSOLE_DXC,
#                             VICTORY_PC_DXC); "skipped: no console
#                             compiler ...".
#   VICTORY_REQUIRE_PC_GPU=1  the PC hardware's results where D3DMetal is
#                             known to give others (docs/macos.md, "Open
#                             differences under D3DMetal"); "skipped:
#                             D3DMetal ...". Fails four shader tests and
#                             one of victory-gpu on a Mac today.
#   VICTORY_REQUIRE_NTFS=1    a temporary folder on NTFS, for its limit of
#                             hard links; "skipped: no NTFS link limit".
#   VICTORY_REQUIRE_IDLE=1    a machine idle enough to start a thread
#                             within 8 ms; "skipped: the host ran the
#                             thread that fills the page list only after".
#   VICTORY_REQUIRE_DEPTH=1   the prebuilt depth conversion (16-bit depth
#                             written through unordered views); "skipped:
#                             no prebuilt depth conversion".
#   VICTORY_REQUIRE_FIDELITYFX=1  the PC FidelityFX DLLs named by
#                             VICTORY_FIDELITYFX_DIR, for the upscaler test.
#   VICTORY_REQUIRE_POLICY=1  the repository's asset policy test, which
#                             needs a git that Wine cannot start;
#                             "skipped: Wine starts no git".
#   VICTORY_REQUIRE_RECONNECT=1  a host that connects a socket again after
#                             a refused attempt, which this Wine does not.
# A run that is meant to prove something on a Mac sets these five:
#   VICTORY_DXIL_UNIX="$HOME/Library/Caches/victory-macos/dxc/v1.9.2609/dxil.dll" \
#   VICTORY_REQUIRE_DXIL=1 VICTORY_REQUIRE_GPU=1 VICTORY_REQUIRE_CLAMP=1 \
#   VICTORY_REQUIRE_DEPTH=1 \
#   VICTORY_PC_DXC_UNIX="$HOME/Library/Caches/victory-macos/dxc/v1.9.2609/dxc.exe" \
#   VICTORY_REQUIRE_HLSL=1 \
#   tools/macos/test-wine.sh --workspace --no-fail-fast
# and again with --release. Without them a machine with no device, no
# validator, a stale clamp or no compiler for the tests' own shaders passes
# those tests without running them. The others name things a Mac does not
# have (WARP, DIFF, PC_GPU, NTFS, FIDELITYFX, POLICY, RECONNECT) or cannot
# promise (IDLE).
# The stand-in title and its DLLs (tests/fixtures) are built here, once, in
# the profile the tests are built in, and VICTORY_FIXTURE_DIR tells the tests
# where they are: cargo cannot be started inside Wine.
# title-unwind, the fixture that unwinds, takes a second build with a profile
# of its own (unwind, or unwind-release) and lies in the folder of that name
# beside the others.
# VICTORY_DXIL_UNIX names a dxil.dll (Microsoft's DXIL validator, kept
# outside the repository) by its macOS path; the tests get it as
# VICTORY_DXIL, the Z: path Wine knows it by.
# VICTORY_PC_DXC_UNIX names a dxc.exe (Microsoft's DirectX Shader Compiler,
# from the same release, with its dxcompiler.dll and dxil.dll beside it and
# kept outside the repository as well) by its macOS path; the tests get it
# as VICTORY_PC_DXC. victory-gpu's device tests that compile shaders of
# their own have it build the ones Wine's D3DCompile refuses, as shader
# model 6.0, and say so in one line that starts with "note: "; a host whose
# D3DCompile builds them never starts it.
# ROSETTA_ADVERTISE_AVX=1 is set for the tests unless the caller set the
# variable: `victory run --execute` refuses a CPU without AVX2, and Rosetta 2
# says it has AVX and AVX2 only to a process started with this variable.
# tools/macos/victory.sh sets it for the same reason, so the tests start
# titles the way the launcher does, through the check and not around it.
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
# Rosetta reads this when a process starts: Wine, the tests and the
# `victory.exe` they start all get it from here.
export ROSETTA_ADVERTISE_AVX="${ROSETTA_ADVERTISE_AVX:-1}"
# Wine hands its environment to the tests; said here so that it stays so.
# One line per switch the tree reads, as in the list at the top.
[ -z "${VICTORY_REQUIRE_GPU:-}" ] || export VICTORY_REQUIRE_GPU
[ -z "${VICTORY_REQUIRE_DXIL:-}" ] || export VICTORY_REQUIRE_DXIL
[ -z "${VICTORY_REQUIRE_CLAMP:-}" ] || export VICTORY_REQUIRE_CLAMP
[ -z "${VICTORY_REQUIRE_HLSL:-}" ] || export VICTORY_REQUIRE_HLSL
[ -z "${VICTORY_REQUIRE_WARP:-}" ] || export VICTORY_REQUIRE_WARP
[ -z "${VICTORY_REQUIRE_DIFF:-}" ] || export VICTORY_REQUIRE_DIFF
[ -z "${VICTORY_REQUIRE_PC_GPU:-}" ] || export VICTORY_REQUIRE_PC_GPU
[ -z "${VICTORY_REQUIRE_NTFS:-}" ] || export VICTORY_REQUIRE_NTFS
[ -z "${VICTORY_REQUIRE_IDLE:-}" ] || export VICTORY_REQUIRE_IDLE
[ -z "${VICTORY_REQUIRE_DEPTH:-}" ] || export VICTORY_REQUIRE_DEPTH
[ -z "${VICTORY_REQUIRE_FIDELITYFX:-}" ] || export VICTORY_REQUIRE_FIDELITYFX
[ -z "${VICTORY_REQUIRE_POLICY:-}" ] || export VICTORY_REQUIRE_POLICY
[ -z "${VICTORY_REQUIRE_RECONNECT:-}" ] || export VICTORY_REQUIRE_RECONNECT
if [ -n "${VICTORY_DXIL_UNIX:-}" ]; then
    [ -f "$VICTORY_DXIL_UNIX" ] || { echo "VICTORY_DXIL_UNIX: no file at $VICTORY_DXIL_UNIX" >&2; exit 1; }
    case "$VICTORY_DXIL_UNIX" in
        /*) ;;
        *) echo "VICTORY_DXIL_UNIX must be an absolute path" >&2; exit 1 ;;
    esac
    VICTORY_DXIL="Z:$(printf '%s' "$VICTORY_DXIL_UNIX" | tr / '\\')"
    export VICTORY_DXIL
fi
if [ -n "${VICTORY_PC_DXC_UNIX:-}" ]; then
    [ -f "$VICTORY_PC_DXC_UNIX" ] || { echo "VICTORY_PC_DXC_UNIX: no file at $VICTORY_PC_DXC_UNIX" >&2; exit 1; }
    case "$VICTORY_PC_DXC_UNIX" in
        /*) ;;
        *) echo "VICTORY_PC_DXC_UNIX must be an absolute path" >&2; exit 1 ;;
    esac
    VICTORY_PC_DXC="Z:$(printf '%s' "$VICTORY_PC_DXC_UNIX" | tr / '\\')"
    export VICTORY_PC_DXC
fi
export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER="$LINK"
cargo build --quiet --locked --manifest-path tests/fixtures/Cargo.toml \
    --target-dir target/fixtures --target "$TARGET" $FIXTURE_PROFILE
cargo build --quiet --locked --manifest-path tests/fixtures/Cargo.toml \
    --target-dir target/fixtures --target "$TARGET" -p title-unwind --profile "$UNWIND_PROFILE"
VICTORY_FIXTURE_DIR="Z:$(printf '%s' "$ROOT" | tr / '\\')\\target\\fixtures\\$TARGET\\$PROFILE"
export VICTORY_FIXTURE_DIR
cargo test --target "$TARGET" "$@"
