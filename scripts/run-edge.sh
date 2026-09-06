#!/usr/bin/env bash
# Open Microsoft Edge with this extension already loaded, for testing.
#
# Edge keeps a separate profile for this, under ~/.opendownloader-edge-profile, so
# nothing here touches the Edge profile you browse with — no history, no logins, no
# risk of leaving a development extension behind in your everyday browser.
#
# To put it in your *normal* Edge profile instead, see the three steps this prints.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
DIST="$REPO/apps/extension/dist"
PROFILE="$HOME/.opendownloader-edge-profile"
EDGE="/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"

if [ ! -x "$EDGE" ]; then
  echo "Microsoft Edge is not installed at $EDGE" >&2
  exit 1
fi

# Build only if the folder is missing; a stale dist is the caller's to refresh with
# `npm run build:extension`, and rebuilding here would silently discard an E2E build.
if [ ! -f "$DIST/manifest.json" ]; then
  echo "No build found — running npm run build:extension"
  (cd "$REPO" && npm run build:extension)
fi

mkdir -p "$PROFILE"

cat <<TXT

Opening Edge with OpenDownloader loaded.
  extension   $DIST
  profile     $PROFILE   (separate from your everyday Edge profile)

To add it to your everyday Edge profile instead:
  1. Go to  edge://extensions
  2. Turn on "Developer mode" (bottom-left)
  3. Click "Load unpacked" and choose:
     $DIST

TXT

exec "$EDGE" \
  --user-data-dir="$PROFILE" \
  --disable-extensions-except="$DIST" \
  --load-extension="$DIST" \
  --no-first-run \
  --no-default-browser-check \
  "http://127.0.0.1:5181/page.html"
