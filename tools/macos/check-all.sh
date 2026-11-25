#!/bin/bash
# Runs everything this Mac can check, one step after the other whatever the
# one before did, and prints one table at the end.
#   tools/macos/check-all.sh [--quick] [--no-wine] [--repeat N]
# The steps, each a row of the table:
#   policy                  cargo xtask policy
#   fmt-check               cargo xtask fmt-check
#   native tests (dev)      cargo test --workspace --locked --no-fail-fast
#   native tests (release)  the same with --release
#   wine tests (dev)        tools/macos/test-wine.sh --workspace --no-fail-fast
#   wine tests (release)    the same with --release
#   doctor (wine)           tools/macos/victory.sh doctor
# Options:
#   --quick      policy, fmt-check and the native dev tests only.
#   --no-wine    everything native; no Wine run and no doctor.
#   --repeat N   the two Wine test runs N times each (default 1): device
#                tests that pass once and fail once show up that way.
# Per row the table has the command's exit status, the tests passed, failed
# and ignored (summed over cargo's "test result:" lines; "-" where the step
# runs no tests) and the number of "skipped: <reason> (<test>)" lines, which
# a test prints when it passes without having run (docs/macos.md, "What a
# test run did not check"). Below the table those lines are counted per
# reason for each row that has any.
# The script ends with status 1 if any row's status is not 0, else with 0.
#
# The Wine test runs set every VICTORY_REQUIRE_* switch this Mac can meet,
# so that a test which lacks what it needs fails instead of skipping:
#   with Apple's D3DMetal in the Wine build:
#     VICTORY_REQUIRE_GPU=1 VICTORY_REQUIRE_CLAMP=1 VICTORY_REQUIRE_DEPTH=1
#   with a dxil.dll (the DXIL validator) found:   VICTORY_REQUIRE_DXIL=1
#   with a dxc.exe (the shader compiler) found:   VICTORY_REQUIRE_HLSL=1
# The two files are looked for in the newest folder under
# ~/Library/Caches/victory-macos/dxc/*/ that has a dxil.dll (docs/macos.md,
# "The DXIL validator"), unless VICTORY_DXIL_UNIX and VICTORY_PC_DXC_UNIX
# already name them. Where one is missing the script says which switches it
# therefore left off, before the runs and again under the table.
# These are never set here, because they name something a Mac does not have
# or cannot promise (test-wine.sh says what each is):
#   VICTORY_REQUIRE_WARP VICTORY_REQUIRE_DIFF VICTORY_REQUIRE_PC_GPU
#   VICTORY_REQUIRE_NTFS VICTORY_REQUIRE_IDLE VICTORY_REQUIRE_FIDELITYFX
#   VICTORY_REQUIRE_POLICY VICTORY_REQUIRE_RECONNECT
# One of them set in the environment of this script reaches the Wine runs
# all the same. No switch is set for the native runs: they have no device,
# no validator and no Wine, and print their skip lines.
#
# Each step's output goes to a file of its own in a new folder per run under
# VICTORY_CHECK_DIR (default: ~/Library/Caches/victory-macos/check), named
# <year><month><day>-<hour><minute><second>. The script refuses to write
# inside a VIctory source tree. The last lines of a failed step are shown
# when it ends.
# CARGO names the cargo to use (default: cargo on PATH); WINE and the other
# variables of tools/macos/test-wine.sh and tools/macos/victory.sh mean here
# what they mean there.
#
#   tools/macos/check-all.sh --parse-log <file>
# prints what the table would say of one log: "tests: <passed> passed,
# <failed> failed, <ignored> ignored" (or "tests: none"), then a line
# "<count> <reason>" per reason of its skip lines, most frequent first.
#
# No `set -e`: a failed step must not end the script, and every status is
# read where the step ran. Written for the bash 3.2 of macOS.
set -u

ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"

# The tests of a log as "<passed> <failed> <ignored>", summed over cargo's
# "test result:" lines; "- - -" for a log without one.
tally() {
    awk '
        /test result: / {
            for (i = 1; i < NF; i++) {
                if ($(i + 1) ~ /^passed/) passed += $i
                if ($(i + 1) ~ /^failed/) failed += $i
                if ($(i + 1) ~ /^ignored/) ignored += $i
            }
            results++
        }
        END {
            if (results) printf "%d %d %d\n", passed, failed, ignored
            else print "- - -"
        }
    ' "$1"
}

# The skip lines of a log, counted per reason, most frequent first:
# "<count> <reason>". A line is "skipped: <reason> (<test>)", after whatever
# the test harness had already written on that line; the test's name at the
# end is dropped so that tests skipped for one reason count together.
skips() {
    sed -n 's/.*skipped: //p' "$1" | sed 's/ ([a-z0-9_:]*)$//' | sort | uniq -c \
        | sort -k1,1nr -k2 | sed 's/^ *\([0-9][0-9]*\) /\1 /'
}

# The number of skip lines in a log.
skip_count() {
    grep -c 'skipped: ' "$1"
}

if [ "${1:-}" = --parse-log ]; then
    if [ $# -ne 2 ] || [ ! -f "$2" ]; then
        echo "usage: tools/macos/check-all.sh --parse-log <file>" >&2
        exit 2
    fi
    set -- $(tally "$2") "$2"
    if [ "$1" = - ]; then
        echo "tests: none"
    else
        echo "tests: $1 passed, $2 failed, $3 ignored"
    fi
    skips "$4"
    exit 0
fi

usage() {
    echo "usage: tools/macos/check-all.sh [--quick] [--no-wine] [--repeat N]" >&2
    echo "       tools/macos/check-all.sh --parse-log <file>" >&2
    exit 2
}

QUICK=0
WINE_RUNS=1
REPEAT=1
while [ $# -gt 0 ]; do
    case "$1" in
        --quick) QUICK=1 ;;
        --no-wine) WINE_RUNS=0 ;;
        --repeat)
            [ $# -ge 2 ] || usage
            REPEAT="$2"
            shift
            ;;
        *) usage ;;
    esac
    shift
done
case "$REPEAT" in
    "" | *[!0-9]* | 0*)
        echo "check-all.sh: --repeat takes a number from 1 up, not '$REPEAT'" >&2
        exit 2
        ;;
esac
[ "$QUICK" = 0 ] || WINE_RUNS=0

# Whether the directory $1 is inside a VIctory source tree: this one, or any
# folder with the marker file victory_core::ensure_outside_repo looks for.
in_source_tree() {
    dir="$1"
    case "$dir/" in "$ROOT"/*) return 0 ;; esac
    while :; do
        [ ! -f "$dir/.victory-root" ] || return 0
        case "$dir" in
            / | "") return 1 ;;
        esac
        dir="$(dirname "$dir")"
    done
}
# The nearest folder that exists at or above the path $1, links resolved.
existing() {
    dir="$1"
    until [ -d "$dir" ]; do dir="$(dirname "$dir")"; done
    (cd "$dir" && pwd -P)
}

case "${VICTORY_CHECK_DIR:-}" in
    "") LOGS="${HOME:?}/Library/Caches/victory-macos/check" ;;
    /*) LOGS="$VICTORY_CHECK_DIR" ;;
    *) LOGS="$(pwd -P)/$VICTORY_CHECK_DIR" ;;
esac
if in_source_tree "$(existing "$LOGS")"; then
    echo "check-all.sh: refusing to write the logs to $LOGS: it is inside a VIctory source tree. Set VICTORY_CHECK_DIR to a folder outside it, or leave it unset." >&2
    exit 2
fi
OUT="$LOGS/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT" || {
    echo "check-all.sh: cannot make the folder $OUT for the logs" >&2
    exit 2
}
OUT="$(cd "$OUT" && pwd -P)"

CARGO="${CARGO:-cargo}"

# What the Wine runs require, and what they leave off and why.
WINE_ENV=(env)
LEFT_OFF=""
left_off() { LEFT_OFF="$LEFT_OFF$1
"; }
DXIL_UNIX="${VICTORY_DXIL_UNIX:-}"
DXC_UNIX="${VICTORY_PC_DXC_UNIX:-}"
if [ "$WINE_RUNS" = 1 ]; then
    WINE_BIN="${WINE:-$HOME/Library/Caches/victory-macos/target/wine-cx/bin/wine}"
    if [ -d "$(dirname "$WINE_BIN")/../lib/external/D3DMetal.framework" ]; then
        WINE_ENV+=(VICTORY_REQUIRE_GPU=1 VICTORY_REQUIRE_CLAMP=1 VICTORY_REQUIRE_DEPTH=1)
    else
        left_off "no D3DMetal beside the Wine at $WINE_BIN: VICTORY_REQUIRE_GPU, VICTORY_REQUIRE_CLAMP and VICTORY_REQUIRE_DEPTH are left off, and the device tests pass without running"
    fi
    # The newest DXC folder that has the validator.
    DXC_DIR=""
    for dir in "$HOME"/Library/Caches/victory-macos/dxc/*/; do
        [ ! -f "${dir}dxil.dll" ] || DXC_DIR="${dir%/}"
    done
    if [ -z "$DXIL_UNIX" ] && [ -n "$DXC_DIR" ]; then
        DXIL_UNIX="$DXC_DIR/dxil.dll"
    fi
    if [ -z "$DXC_UNIX" ] && [ -n "$DXC_DIR" ] && [ -f "$DXC_DIR/dxc.exe" ]; then
        DXC_UNIX="$DXC_DIR/dxc.exe"
    fi
    if [ -n "$DXIL_UNIX" ]; then
        WINE_ENV+=("VICTORY_DXIL_UNIX=$DXIL_UNIX" VICTORY_REQUIRE_DXIL=1)
    else
        left_off "no dxil.dll in a folder under ~/Library/Caches/victory-macos/dxc/*/ (and VICTORY_DXIL_UNIX is not set): VICTORY_REQUIRE_DXIL is left off, and the tests that validate DXIL pass without validating"
    fi
    if [ -n "$DXC_UNIX" ]; then
        WINE_ENV+=("VICTORY_PC_DXC_UNIX=$DXC_UNIX" VICTORY_REQUIRE_HLSL=1)
    else
        left_off "no dxc.exe in a folder under ~/Library/Caches/victory-macos/dxc/*/ (and VICTORY_PC_DXC_UNIX is not set): VICTORY_REQUIRE_HLSL is left off, and the device tests whose shaders Wine cannot compile pass without running"
    fi
    left_off "never set here, a Mac does not have or cannot promise what they name: VICTORY_REQUIRE_WARP VICTORY_REQUIRE_DIFF VICTORY_REQUIRE_PC_GPU VICTORY_REQUIRE_NTFS VICTORY_REQUIRE_IDLE VICTORY_REQUIRE_FIDELITYFX VICTORY_REQUIRE_POLICY VICTORY_REQUIRE_RECONNECT"
    # Without the "env" in front: the switches themselves.
    REQUIRED=""
    for word in "${WINE_ENV[@]}"; do
        case "$word" in VICTORY_REQUIRE_*) REQUIRED="$REQUIRED $word" ;; esac
    done
    echo "check-all: the Wine test runs set:${REQUIRED:- no switch}"
    printf '%s' "$LEFT_OFF" | sed 's/^/check-all: /'
fi
echo "check-all: logs go to $OUT"

# The rows: a label, the exit status and the log of each step that ran.
ROWS=0
ROW_LABEL=()
ROW_CODE=()
ROW_LOG=()
FAILED=0

# Runs one step in the source tree: $1 is the row's label, $2 the name of
# its log, the rest the command. Whatever its status, the script goes on.
step() {
    label="$1"
    log="$OUT/$2"
    shift 2
    echo "check-all: $label ..."
    (cd "$ROOT" && "$@") > "$log" 2>&1
    code=$?
    ROW_LABEL[$ROWS]="$label"
    ROW_CODE[$ROWS]="$code"
    ROW_LOG[$ROWS]="$log"
    ROWS=$((ROWS + 1))
    if [ "$code" -eq 0 ]; then
        echo "check-all: $label: ok"
    else
        FAILED=$((FAILED + 1))
        echo "check-all: $label: FAILED with status $code; the end of $log:"
        tail -n 15 "$log" | sed 's/^/  | /'
    fi
}

step "policy" policy.log "$CARGO" xtask policy
step "fmt-check" fmt-check.log "$CARGO" xtask fmt-check
step "native tests (dev)" native-dev.log "$CARGO" test --workspace --locked --no-fail-fast
if [ "$QUICK" = 0 ]; then
    step "native tests (release)" native-release.log \
        "$CARGO" test --workspace --locked --no-fail-fast --release
fi
if [ "$WINE_RUNS" = 1 ]; then
    for profile in dev release; do
        run=1
        while [ "$run" -le "$REPEAT" ]; do
            label="wine tests ($profile)"
            [ "$REPEAT" = 1 ] || label="$label $run/$REPEAT"
            if [ "$profile" = dev ]; then
                step "$label" "wine-dev-$run.log" \
                    "${WINE_ENV[@]}" tools/macos/test-wine.sh --workspace --no-fail-fast
            else
                step "$label" "wine-release-$run.log" \
                    "${WINE_ENV[@]}" tools/macos/test-wine.sh --workspace --no-fail-fast --release
            fi
            run=$((run + 1))
        done
    done
    # The doctor gets the validator by the name Wine knows it by.
    DOCTOR_ENV=(env)
    [ -z "$DXIL_UNIX" ] || DOCTOR_ENV+=("VICTORY_DXIL=Z:$(printf '%s' "$DXIL_UNIX" | tr / '\\')")
    step "doctor (wine)" doctor.log "${DOCTOR_ENV[@]}" tools/macos/victory.sh doctor
fi

echo
echo "check-all: results (logs in $OUT)"
printf '%-28s %4s %7s %7s %7s %7s\n' "step" "exit" "passed" "failed" "ignored" "skipped"
i=0
while [ "$i" -lt "$ROWS" ]; do
    set -- $(tally "${ROW_LOG[$i]}")
    skipped="$(skip_count "${ROW_LOG[$i]}")"
    [ "$1" != - ] || [ "$skipped" != 0 ] || skipped=-
    printf '%-28s %4s %7s %7s %7s %7s\n' "${ROW_LABEL[$i]}" "${ROW_CODE[$i]}" "$1" "$2" "$3" "$skipped"
    i=$((i + 1))
done
i=0
while [ "$i" -lt "$ROWS" ]; do
    if [ "$(skip_count "${ROW_LOG[$i]}")" != 0 ]; then
        echo
        echo "skipped in ${ROW_LABEL[$i]}, by reason:"
        skips "${ROW_LOG[$i]}" | sed 's/^\([0-9]*\) /\1 x  /' | sed 's/^/  /'
    fi
    i=$((i + 1))
done
if [ -n "$LEFT_OFF" ]; then
    echo
    echo "left off in the Wine test runs:"
    printf '%s' "$LEFT_OFF" | sed 's/^/  /'
fi
echo
if [ "$FAILED" -gt 0 ]; then
    echo "check-all: $FAILED of $ROWS steps FAILED"
    exit 1
fi
echo "check-all: all $ROWS steps passed"
