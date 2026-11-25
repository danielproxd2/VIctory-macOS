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
# VICTORY_FEATURES names cargo features to build victory.exe with. The one
# of use is `diagnostics`: debug and trace log lines (VICTORY_LOG), the
# reason of each dropped draw beside the frame-rate lines, and the debugging
# switches of that feature (apps/victory/Cargo.toml). A plain build logs info
# and above.
# No game is configured to start by default: without arguments the launcher
# says what to pass and fails, before it builds or starts anything. (`run`
# without an executable is refused by victory.exe, which names it.)
#
# The directory VIctory runs in is the one the launcher was started in, as
# on Windows, where `victory` is started in the user's directory: a path
# given on the command line is taken from there, and so is every relative
# path the title opens. A title started by its file name alone, from its own
# folder (`cd <the game's folder>; .../victory.sh run --execute game.exe`),
# has that folder as its current directory and as its G: drive.
# The one directory a title must never run in is the VIctory source tree
# (this repository, or another copy of it): what a title writes by a
# relative path would land among the sources, where nothing derived from a
# game may be (docs/asset-policy.md). Started from inside a source tree,
# `run` therefore starts the title in the title's own folder, which is the
# case above, and says so; the executable is still the one the path named
# from where the launcher was started. If the title's folder is inside a
# source tree too, nothing is started.
# The config file is VICTORY_CONFIG (a relative name is taken from the
# directory the launcher was started in), else victory.toml in that
# directory, else the repository's victory.toml, which is where setup.sh
# puts it.
set -e
# Where the user is, and where the sources are, links resolved.
HERE="$(pwd -P)"
ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
if [ $# -eq 0 ]; then
    {
        echo "victory.sh: no command given, and no game is started by default. Name the game's executable:"
        echo "  tools/macos/victory.sh run --execute <path to the game's executable>"
        echo "To check the setup instead: tools/macos/victory.sh config"
    } >&2
    exit 2
fi
# A macOS path as Wine names it: Wine shows / as Z:.
windows() { printf 'Z:%s' "$(printf '%s' "$1" | tr / '\\')"; }
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
# Where the title runs, settled before anything is built or started.
# The executable `run` was given: the first argument after `run` that is
# neither an option nor an option's value, before any `--` (what follows
# that is the title's).
exe_at=0
RUN_DIR=
if [ "$1" = run ]; then
    n=0
    value=0
    for a in "$@"; do
        n=$((n + 1))
        [ "$n" -gt 1 ] || continue
        if [ "$value" = 1 ]; then
            value=0
            continue
        fi
        case "$a" in
            --) break ;;
            --unimplemented | --console) value=1 ;;
            -*) ;;
            *)
                exe_at=$n
                EXE="$a"
                break
                ;;
        esac
    done
fi
if [ "$exe_at" -gt 0 ] && in_source_tree "$HERE"; then
    # The title's folder, from the executable's macOS path.
    case "$EXE" in
        [Zz]:[/\\]*) EXE="$(printf '%s' "${EXE#??}" | tr '\\' /)" ;;
        [A-Za-z]:*)
            printf '%s\n' "victory.sh: started inside the VIctory source tree ($HERE), where a title must not run, and $EXE is not a path macOS can follow to the title's folder. Name the executable by its macOS path, or start the launcher from a folder outside the source tree." >&2
            exit 1
            ;;
        /*) ;;
        *) EXE="$HERE/$EXE" ;;
    esac
    TITLE_DIR="$(cd "$(dirname "$EXE")" 2> /dev/null && pwd -P)" || {
        printf '%s\n' "victory.sh: no folder at $(dirname "$EXE"), where $EXE would be" >&2
        exit 1
    }
    if in_source_tree "$TITLE_DIR"; then
        printf '%s\n' "victory.sh: refusing to run a title in $TITLE_DIR: it is inside a VIctory source tree, and so is the directory the launcher was started in ($HERE). What a title writes by a relative path would land among the sources (docs/asset-policy.md). Keep the game in a folder outside the repository." >&2
        exit 1
    fi
    printf '%s\n' "victory.sh: started inside the VIctory source tree; the title runs in its own folder, $TITLE_DIR" >&2
    EXE="$(windows "$TITLE_DIR/$(basename "$EXE")")"
    n=0
    for a in "$@"; do
        n=$((n + 1))
        [ "$n" -ne "$exe_at" ] || a="$EXE"
        set -- "$@" "$a"
        shift
    done
    RUN_DIR="$TITLE_DIR"
fi
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
WINE="${WINE:-$HOME/Library/Caches/victory-macos/target/wine-cx/bin/wine}"
[ -x "$WINE" ] || { echo "no Wine at $WINE: run tools/macos/build-crossover-wine.sh first" >&2; exit 1; }
[ -d "$(dirname "$WINE")/../lib/external/D3DMetal.framework" ] || {
    echo "D3DMetal is missing: copy the redist/lib folder of Apple's evaluation environment over $(dirname "$WINE")/../lib" >&2
    exit 1
}
# The build is the one thing done in the source tree: cargo finds the
# workspace and its toolchain file from there.
(cd "$ROOT" && "${CARGO:-cargo}" build --quiet --release -p victory --target x86_64-pc-windows-gnullvm \
    ${VICTORY_FEATURES:+--features "$VICTORY_FEATURES"})
export WINEPREFIX="${WINEPREFIX:-$HOME/Library/Caches/victory-macos/prefix-play}"
export WINEDEBUG="${WINEDEBUG:--all}"
export WINEDLLOVERRIDES="mscoree,mshtml=;winedbg.exe=d;d3d12,d3d12core,dxgi=b${WINEDLLOVERRIDES:+;$WINEDLLOVERRIDES}"
# The config file victory.exe will read, by its macOS name (empty: none).
# One the launcher found, and a relative name, are handed on as full paths:
# victory.exe may be started in another directory than this one.
CONFIG="$(printf '%s' "${VICTORY_CONFIG:-}" | tr '\\' /)"
case "$CONFIG" in
    "")
        if [ -f "$HERE/victory.toml" ]; then
            CONFIG="$HERE/victory.toml"
        elif [ -f "$ROOT/victory.toml" ]; then
            CONFIG="$ROOT/victory.toml"
        fi
        [ -z "$CONFIG" ] || export VICTORY_CONFIG="$(windows "$CONFIG")"
        ;;
    [Zz]:/*) CONFIG="${CONFIG#??}" ;;
    /* | [A-Za-z]:*) ;;
    *)
        CONFIG="$HERE/$CONFIG"
        export VICTORY_CONFIG="$(windows "$CONFIG")"
        ;;
esac
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
# Wine starts victory.exe in the directory it is started in itself.
[ -z "$RUN_DIR" ] || cd "$RUN_DIR"
exec "$WINE" "$ROOT/target/x86_64-pc-windows-gnullvm/release/victory.exe" "$@"
