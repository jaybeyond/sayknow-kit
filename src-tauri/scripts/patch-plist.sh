#!/bin/sh
# Append macOS-specific Info.plist keys that Tauri's generator doesn't
# expose via tauri.conf.json. Runs after the macOS .app bundle is built.

set -e

APP="$(dirname "$0")/../target/release/bundle/macos/SayKnow Kit.app"
PLIST="$APP/Contents/Info.plist"
[ -f "$PLIST" ] || exit 0  # nothing to patch (different target)

# Stop macOS from offering "Reopen windows from last time" — irrelevant for
# a menubar utility and the prompt shows whenever a CI / dev-loop kills the
# process before macOS can flush window state.
plutil -replace NSQuitAlwaysKeepsWindows -bool false "$PLIST"

# Explicitly mark this as an accessory app at the plist level (we also set
# the activation policy at runtime, but pre-declaring avoids a Dock flash
# during launch).
plutil -replace LSUIElement -bool true "$PLIST"

# Patching Info.plist invalidates Tauri's linker/ad-hoc signature. An ad-hoc
# build also gets a new CDHash every time, so macOS treats every rebuild as a
# different Accessibility client and repeatedly asks for permission. Re-sign
# with an explicit or locally available Apple Development identity so the
# designated requirement remains stable across builds.
SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-}"
if [ -z "$SIGNING_IDENTITY" ] && command -v security >/dev/null 2>&1; then
  SIGNING_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Apple Development:.*\)"/\1/p' \
    | sed -n '1p')"
fi

if [ -n "$SIGNING_IDENTITY" ]; then
  codesign --force --deep --options runtime --timestamp=none \
    --sign "$SIGNING_IDENTITY" "$APP"
  codesign --verify --deep --strict "$APP"
  echo "[patch-plist] signed with $SIGNING_IDENTITY"
else
  echo "[patch-plist] no signing identity; Accessibility permission will not persist" >&2
fi

echo "[patch-plist] applied to $PLIST"
