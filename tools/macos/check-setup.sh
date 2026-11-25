#!/bin/sh
# The last step of setup.sh, and a check to run at any time: does VIctory
# start under Wine, and can its configuration be used as it is?
#   tools/macos/check-setup.sh
# It needs no game. `victory --version` is what proves the start: the
# Windows build of VIctory runs under the Wine that
# tools/macos/build-crossover-wine.sh built and says its name. A failure
# there ends this script with status 1 and with what Wine and VIctory said.
# `victory config` is then shown in full. A configuration with no game_dir
# is complete (victory.example.toml ships that way); one that VIctory
# refuses (a game_dir that is set and is not there, a folder inside the
# repository) is the user's file to correct and not a failed setup, so it is
# reported with what to change and the status stays 0.
# It ends by naming `tools/macos/victory.sh doctor`, the fuller check of the
# Mac itself (the D3D12 device, the folders, the prebuilt shaders), which
# needs no game either and is not run here.
# WINE, CARGO, VICTORY_CONFIG and the other variables of
# tools/macos/victory.sh mean here what they mean there.
set -u
LAUNCHER="$(dirname "$0")/victory.sh"
if ! said="$("$LAUNCHER" --version 2>&1)"; then
    {
        echo "VIctory did not start under Wine. \`tools/macos/victory.sh --version\` said:"
        printf '%s\n' "$said" | sed 's/^/  /'
    } >&2
    exit 1
fi
case "$said" in
    victory\ *) printf '%s\n' "VIctory starts under Wine: $said" ;;
    *)
        {
            echo "VIctory did not start under Wine: \`tools/macos/victory.sh --version\` ended with status 0 and did not say its version. It said:"
            printf '%s\n' "$said" | sed 's/^/  /'
        } >&2
        exit 1
        ;;
esac
if report="$("$LAUNCHER" config 2>&1)"; then
    printf '%s\n' "$report"
else
    printf '%s\n' "$report"
    {
        echo
        echo "VIctory starts, and its configuration cannot be used as it is (see the lines above)."
        echo "Correct victory.toml, or the VICTORY_* variables that override it:"
        echo "  - MISSING: game_dir ...: name the folder your game is in, in the Z:\\ form"
        echo "    (/Users/you/games/x is Z:\\Users\\you\\games\\x), or remove the game_dir line."
        echo "  - POLICY VIOLATION: ...: name a folder outside the VIctory source tree."
        echo "Then run tools/macos/victory.sh config again."
    } >&2
fi
echo
echo "For a check of this Mac itself (the D3D12 device and what VIctory measures on it,"
echo "the folders a run writes to, the prebuilt shaders): tools/macos/victory.sh doctor"
