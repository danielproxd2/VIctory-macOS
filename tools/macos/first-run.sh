#!/bin/sh
# Checks what a first real run depends on, in order, and stops at the first
# check that fails with what failed and what to do about it. No title code
# runs here. When every check passes it prints the exact command that starts
# the title, and where its log and its crash log will be.
#   tools/macos/first-run.sh <path to the game's executable>
# The checks:
#   1. the build: the Wine of tools/macos/build-crossover-wine.sh with
#      Apple's D3DMetal in it, and VIctory starting under it
#      (`victory --version`);
#   2. `victory inspect` on the executable: VIctory reads it as a Windows
#      program and lists its headers and imports;
#   3. `victory run` without --execute: the executable is prepared, mapped
#      and its imports are bound; the report names every DLL of the package
#      that is missing and every import nothing provides. The title is not
#      started.
# What the checks print is kept in a new folder per run under
# VICTORY_FIRST_RUN_DIR (default: ~/Library/Logs/victory). This script
# writes nowhere else, and refuses to write inside a VIctory source tree:
# what `inspect` and `run` say about a game is derived from the game
# (docs/asset-policy.md). For the same reason it refuses an executable that
# lies inside a source tree.
# WINE, CARGO, VICTORY_CONFIG and the other variables of
# tools/macos/victory.sh mean here what they mean there.
# docs/first-run.md says what to look at when the start itself fails.
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
LAUNCHER="$ROOT/tools/macos/victory.sh"
say() { printf '%s\n' "first-run: $*"; }
# Ends the script: the check that failed, then what to do, one line each.
fail() {
    {
        printf '%s\n' "first-run: FAILED: $1"
        shift
        for line in "$@"; do printf '%s\n' "  $line"; done
    } >&2
    exit 1
}
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
# $1 as one word of a shell command.
quoted() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
# A macOS path as Wine names it, and a Z: path as macOS names it.
windows() { printf 'Z:%s' "$(printf '%s' "$1" | tr / '\\')"; }
macos() { printf '%s' "$1" | sed 's/^[Zz]://' | tr '\\' /; }

if [ $# -ne 1 ] || [ -z "$1" ]; then
    {
        echo "usage: tools/macos/first-run.sh <path to the game's executable>"
        echo "Checks the setup and the executable without starting the title, then says how to start it."
    } >&2
    exit 2
fi
case "$1" in
    /*) EXE="$1" ;;
    *) EXE="$(pwd -P)/$1" ;;
esac
[ -f "$EXE" ] || fail "there is no file at $EXE" \
    "Name the game's executable by its macOS path, for example ~/games/<the game>/<name>.exe."
TITLE_DIR="$(cd "$(dirname "$EXE")" && pwd -P)"
EXE="$TITLE_DIR/$(basename "$EXE")"
if in_source_tree "$TITLE_DIR"; then
    fail "the executable is inside a VIctory source tree ($TITLE_DIR)" \
        "Nothing from a game may be inside the repository (docs/asset-policy.md)." \
        "Move the game's folder somewhere else, for example ~/games, and name it there."
fi
case "${VICTORY_FIRST_RUN_DIR:-}" in
    "") LOGS="${HOME:?}/Library/Logs/victory" ;;
    /*) LOGS="$VICTORY_FIRST_RUN_DIR" ;;
    *) LOGS="$(pwd -P)/$VICTORY_FIRST_RUN_DIR" ;;
esac
if in_source_tree "$(existing "$LOGS")"; then
    fail "refusing to write the reports to $LOGS: it is inside a VIctory source tree" \
        "What VIctory says about a game is derived from the game and may not be in the repository (docs/asset-policy.md)." \
        "Set VICTORY_FIRST_RUN_DIR to a folder outside it, or leave it unset for ~/Library/Logs/victory."
fi
OUT="$LOGS/first-run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT" || fail "cannot make the folder $OUT for the reports" \
    "Set VICTORY_FIRST_RUN_DIR to a folder you can write to."
say "checking $EXE"
say "reports go to $OUT"

# 1. The build.
WINE="${WINE:-$HOME/Library/Caches/victory-macos/target/wine-cx/bin/wine}"
[ -x "$WINE" ] || fail "[1/3] there is no Wine at $WINE" \
    "Run ./setup.sh in the repository; it builds Wine (about 30 minutes) and skips what is done." \
    "A Wine built elsewhere is named with WINE=<path to its bin/wine>."
[ -d "$(dirname "$WINE")/../lib/external/D3DMetal.framework" ] || fail "[1/3] Apple's D3DMetal is not in that Wine ($(dirname "$WINE")/../lib/external/D3DMetal.framework)" \
    "Download the Game Porting Toolkit .dmg from Apple into ~/Downloads and run ./setup.sh again:" \
    "it copies D3DMetal in. Without it there is no D3D12 device and nothing is drawn."
if ! "$LAUNCHER" --version > "$OUT/version.txt" 2> "$OUT/version.log"; then
    sed 's/^/  | /' "$OUT/version.log" >&2
    fail "[1/3] VIctory does not start under Wine (see above, and $OUT/version.log)" \
        "If the lines above are a compiler's, the build of VIctory failed: run ./setup.sh again." \
        "Otherwise run tools/macos/check-setup.sh, which shows the same start on its own."
fi
case "$(cat "$OUT/version.txt")" in
    victory\ *) say "[1/3] ok: Wine and D3DMetal are there, and $(cat "$OUT/version.txt") starts under Wine" ;;
    *) fail "[1/3] VIctory started under Wine and did not say its version (see $OUT/version.txt)" \
        "Run tools/macos/check-setup.sh, which shows the same start on its own." ;;
esac

# 2. The executable, read and not run.
if ! "$LAUNCHER" inspect "$(windows "$EXE")" > "$OUT/inspect.txt" 2> "$OUT/inspect.log"; then
    grep -v ' INFO ' "$OUT/inspect.log" | tail -n 12 | sed 's/^/  | /' >&2
    fail "[2/3] VIctory cannot read $EXE as a Windows program (see above, and $OUT/inspect.log)" \
        "Check that it is the game's executable from your own package and that the copy is whole:" \
        "a file cut short, an archive or a package that has not been unpacked gives this."
fi
say "[2/3] ok: the executable is a Windows program VIctory can read ($OUT/inspect.txt)"

# 3. Prepared, mapped and bound; not started.
if ! "$LAUNCHER" run "$(windows "$EXE")" > "$OUT/bind.txt" 2> "$OUT/bind.log"; then
    grep -v ' INFO ' "$OUT/bind.log" | tail -n 12 | sed 's/^/  | /' >&2
    if grep -q 'inside the VIctory source tree' "$OUT/bind.log"; then
        fail "[3/3] a folder VIctory would write to is inside the source tree (see above)" \
            "Run tools/macos/victory.sh config: it marks each such folder with POLICY VIOLATION." \
            "Name folders outside the repository in victory.toml, or remove the lines for the defaults."
    fi
    if grep -q -e 'parsing ' -e 'VICTORY_CONFIG points at' "$OUT/bind.log"; then
        fail "[3/3] the configuration cannot be read (see above)" \
            "Run tools/macos/victory.sh config and correct victory.toml, or the VICTORY_CONFIG that names another file."
    fi
    fail "[3/3] VIctory could not prepare, map and bind the executable (see above, and $OUT/bind.log)" \
        "docs/first-run.md lists what each message means. The title was not started."
fi
if grep -q '^missing dlls ' "$OUT/bind.txt"; then
    grep '^missing dlls ' "$OUT/bind.txt" | sed 's/^/  | /' >&2
    fail "[3/3] DLLs of the game's own package are not beside the executable (see above)" \
        "Copy every file of the package into $TITLE_DIR, next to the executable." \
        "A start with --execute stops on this before the title runs."
fi
say "[3/3] ok: the executable is mapped and its imports are bound ($OUT/bind.txt)"
if grep -q '^build          unknown ' "$OUT/bind.txt"; then
    say "  note: $(grep '^build ' "$OUT/bind.txt" | tr -s ' ')."
    say "        VIctory does not know this build by its hash. It runs it all the same, and"
    say "        the startup-archive check it makes for a build it knows is not made."
else
    say "  $(grep '^build ' "$OUT/bind.txt" | tr -s ' '): its startup archive is checked when the title starts."
fi
UNBOUND="$(grep -c '^  missing ' "$OUT/bind.txt")"
if [ "$UNBOUND" -gt 0 ]; then
    say "  note: $UNBOUND imports are provided by nothing (the lines '  missing <dll>!<function>' in bind.txt)."
    say "        The title runs until it first calls one; the run then ends and names it:"
    say "        'unimplemented import <dll>!<function> called'."
fi

# Where the title will leave its crash log: beside its storage folder.
STORAGE="$(sed -n 's/^title  *[0-9A-Fa-f]*, storage \(.*\) ([0-9]* MB)$/\1/p' "$OUT/bind.txt")"
case "$STORAGE" in
    [Zz]:*) CRASHES="$(dirname "$(macos "$STORAGE")")/crashes.log" ;;
    "") CRASHES="crashes.log beside the storage folder the report names (no 'title' line was found in bind.txt)" ;;
    *) CRASHES="crashes.log beside $STORAGE (a path inside the Wine prefix)" ;;
esac
START="cd $(quoted "$TITLE_DIR") &&"
END="$(quoted "$LAUNCHER") run --execute $(quoted "$(basename "$EXE")") 2>&1 | tee $(quoted "$OUT/run.log")"
echo
say "Every check passed. Nothing of the title has run yet. To start it, with its log kept:"
echo
printf '  %s %s\n' "$START" "$END"
echo
say "The title runs in its own folder, which is its G: drive."
say "run.log gets everything VIctory prints: its report, its log lines (info and above),"
say "and a crash report if the title faults. The crash report is also added to"
say "  $CRASHES"
say "The same start with the debugging build, which also logs why each dropped draw"
say "was dropped (it builds VIctory again, and again for the next plain start):"
echo
printf '  %s VICTORY_FEATURES=diagnostics %s\n' "$START" "$END"
echo
say "docs/first-run.md says which line to look for at each stage."
