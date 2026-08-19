#!/bin/sh
# Append macOS-specific Info.plist keys that Tauri's generator doesn't
# expose via tauri.conf.json. Runs after the macOS .app bundle is built.

set -e

PLIST="$(dirname "$0")/../target/release/bundle/macos/SayKnow Kit.app/Contents/Info.plist"
[ -f "$PLIST" ] || exit 0  # nothing to patch (different target)

# Stop macOS from offering "Reopen windows from last time" — irrelevant for
# a menubar utility and the prompt shows whenever a CI / dev-loop kills the
# process before macOS can flush window state.
plutil -replace NSQuitAlwaysKeepsWindows -bool false "$PLIST"

# Explicitly mark this as an accessory app at the plist level (we also set
# the activation policy at runtime, but pre-declaring avoids a Dock flash
# during launch).
plutil -replace LSUIElement -bool true "$PLIST"

echo "[patch-plist] applied to $PLIST"
