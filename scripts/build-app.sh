#!/usr/bin/env bash
# Build OpenDownloader as one thing a person downloads and opens.
#
# The web app, the relay and the torrent bridge end up in a single binary serving one
# loopback port, inside a macOS .app bundle. No Node, no Rust, no terminal, no ports to
# remember — double-click and the browser opens on a working app.
#
# The web app is embedded at compile time, so it is built first; a stale dist would be
# baked in silently.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
cd "$REPO"

APP_NAME="OpenDownloader"
OUT="$REPO/dist-app"
BUNDLE="$OUT/$APP_NAME.app"

echo "1/3  Building the web app (it is embedded in the binary)"
npm run build:web --silent >/dev/null

echo "2/3  Building the binary"
cargo build --release -p dl-app -q

echo "3/3  Assembling $APP_NAME.app"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp "$REPO/target/release/opendownloader" "$BUNDLE/Contents/MacOS/$APP_NAME"
[ -f "$REPO/apps/web/public/icon-512.png" ] &&
  cp "$REPO/apps/web/public/icon-512.png" "$BUNDLE/Contents/Resources/icon.png" || true

cat > "$BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>app.opendownloader.desktop</string>
  <key>CFBundleExecutable</key><string>$APP_NAME</string>
  <key>CFBundleVersion</key><string>0.2.0</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <!-- In the Dock, so it can be quit the way every other app is. A background-only
       agent would leave a server running that nobody could find or stop. -->
  <key>LSUIElement</key><false/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo
echo "Built $BUNDLE"
du -sh "$BUNDLE" | awk '{print "  size: " $1}'
echo
echo "It is unsigned, so the first open needs right-click -> Open, once."
echo "Signing and notarising is what removes that, and needs a Developer ID."
