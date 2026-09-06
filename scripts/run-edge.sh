#!/usr/bin/env bash
# Load this build into *your* Microsoft Edge — the one you actually browse with.
#
# It used to open a throwaway profile instead. That was safer in the abstract and wrong
# in practice: the sites this extension exists for are the ones you have to be signed
# into, and a clean profile is signed into nothing. Worse, the two browsers drift — a
# rebuild lands in the dev profile and the everyday one keeps running yesterday's code,
# which looks exactly like a bug in the extension.
#
# An unpacked extension does not pick up new code on its own. Edge reads it once, when
# the extension is loaded, and never looks again. So a rebuild needs the extension
# reloaded, and the only ways to do that are the reload button on edge://extensions or
# a restart of the browser. This restarts it, and asks Edge to restore the tabs.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
DIST="$REPO/apps/extension/dist"
URL="${1:-}"
# Off by default. With it, this script can reload the extension without restarting Edge
# again — but it also lets any program on this machine drive a browser you are signed
# into, so it is opt-in rather than something left switched on.
DEBUG_PORT="${EDGE_DEBUG_PORT:-}"

[ -f "$DIST/manifest.json" ] || { echo "No build at $DIST — run: npm run build:extension" >&2; exit 1; }

if ! pgrep -f "Microsoft Edge.app/Contents/MacOS/Microsoft Edge" >/dev/null; then
  echo "Edge is not running; starting it."
else
  echo "Restarting Edge so it re-reads the extension from disk."
  osascript -e 'tell application "Microsoft Edge" to quit' 2>/dev/null || true
  for _ in $(seq 1 30); do
    pgrep -f "Microsoft Edge.app/Contents/MacOS/Microsoft Edge" >/dev/null || break
    sleep 0.5
  done
  pgrep -f "Microsoft Edge.app/Contents/MacOS/Microsoft Edge" >/dev/null &&
    { echo "Edge would not quit — close it yourself, then rerun." >&2; exit 1; }
fi

ARGS=(--restore-last-session)
[ -n "$DEBUG_PORT" ] && ARGS+=("--remote-debugging-port=$DEBUG_PORT")
[ -n "$URL" ] && ARGS+=("$URL")

open -na "Microsoft Edge" --args "${ARGS[@]}"
echo "Edge restarted with the current build${DEBUG_PORT:+ (debug port $DEBUG_PORT)}."
echo
echo "If OpenDownloader is not listed at edge://extensions, add it once:"
echo "  Developer mode -> Load unpacked -> $DIST"
