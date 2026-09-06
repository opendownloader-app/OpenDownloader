#!/usr/bin/env bash
# Run the whole thing locally, for trying it by hand.
#
# Starts three things and leaves them running until you press Ctrl-C:
#
#   the web app         http://127.0.0.1:5180   the standalone site
#   the test media      http://127.0.0.1:5181   generated video to download, no internet needed
#   the relay           http://127.0.0.1:8088   optional; only the web app ever needs it
#   the torrent bridge  http://127.0.0.1:8089   optional; joins a swarm, serves it over HTTP
#
# The extension is not a server and cannot be started — it is loaded into the browser.
# The script prints how, and builds it first so the folder is there.
#
# Ports are fixed rather than chosen at random so the addresses above stay true and so a
# second run replaces the first instead of quietly starting a duplicate.
set -euo pipefail

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
: "${CARGO_TARGET_DIR:=/tmp/opendownloader-target}"
export CARGO_TARGET_DIR

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
cd "$REPO"

WEB_PORT=5180
MEDIA_PORT=5181
RELAY_PORT=8088
TORRENT_PORT=8089

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
dim()  { printf '\033[2m%s\033[0m\n' "$*"; }

# A previous run holding a port is the most common reason this appears to do nothing.
for port in $WEB_PORT $MEDIA_PORT $RELAY_PORT $TORRENT_PORT; do
  if lsof -ti tcp:"$port" >/dev/null 2>&1; then
    dim "port $port was in use; stopping what was there"
    lsof -ti tcp:"$port" | xargs kill 2>/dev/null || true
    sleep 1
  fi
done

bold "1/5  Building"
npm run build --silent >/dev/null
cargo build -q -p dl-testserver -p dl-relay -p dl-torrent
echo "     extension, web app, test server and relay built"

bold "2/5  Starting the test media server on $MEDIA_PORT"
"$CARGO_TARGET_DIR/debug/dl-testserver" "$MEDIA_PORT" >/tmp/opendownloader-media.log 2>&1 &
MEDIA_PID=$!

bold "3/5  Starting the relay on $RELAY_PORT"
# `allow_private_hosts` so the relay may reach the test media server on this machine.
# That is exactly the setting the relay's README tells you to read twice before enabling;
# it is right here and wrong on a public box.
cat >/tmp/opendownloader-relay.toml <<TOML
bind = "127.0.0.1:$RELAY_PORT"
allowed_origins = ["*"]
allow_private_hosts = true
TOML
cargo run -q -p dl-relay -- /tmp/opendownloader-relay.toml >/tmp/opendownloader-relay.log 2>&1 &
RELAY_PID=$!

bold "4/5  Starting the torrent bridge on $TORRENT_PORT"
# Loopback only, and it does nothing until a magnet link is handed to it. This is the
# piece a browser tab cannot be: joining a swarm needs TCP and uTP sockets to other
# people's machines, and a page has no way to open one.
DL_TORRENT_PORT=$TORRENT_PORT DL_TORRENT_DIR="${TMPDIR:-/tmp}/opendownloader-torrents" \
  cargo run -q -p dl-torrent >/tmp/opendownloader-torrent.log 2>&1 &

bold "5/5  Serving the web app on $WEB_PORT"
# `npx serve` and friends are a network install away; python3 is already here and its
# handler sends the right type for .wasm, which is what matters.
( cd apps/web/dist && python3 -m http.server "$WEB_PORT" --bind 127.0.0.1 >/tmp/opendownloader-web.log 2>&1 ) &
WEB_PID=$!

cleanup() {
  echo
  dim "stopping"
  kill "$MEDIA_PID" "$RELAY_PID" "$WEB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

sleep 2
echo
bold "Ready."
echo
echo "  Web app        http://127.0.0.1:$WEB_PORT"
echo "  Test media     http://127.0.0.1:$MEDIA_PORT/page.html"
echo "  Relay          http://127.0.0.1:$RELAY_PORT/healthz"
echo "  Torrent bridge http://127.0.0.1:$TORRENT_PORT/healthz"
echo
bold "Load the extension"
echo "  Chrome or Edge   chrome://extensions → Developer mode → Load unpacked"
echo "                   $REPO/apps/extension/dist"
echo "  Firefox          about:debugging → This Firefox → Load Temporary Add-on"
echo "                   $REPO/apps/extension/dist-firefox/manifest.json"
echo
bold "Something to download that needs no internet"
echo "  http://127.0.0.1:$MEDIA_PORT/fixture.mp4?size=2000000    a 2 MB file"
echo "  http://127.0.0.1:$MEDIA_PORT/hls/master.m3u8             an HLS stream"
echo "  http://127.0.0.1:$MEDIA_PORT/media.mp4                   a real MP4, for the audio tool"
echo "  http://127.0.0.1:$MEDIA_PORT/page-alt.html               a page with media to detect"
echo
dim "Logs: /tmp/opendownloader-{web,media,relay,torrent}.log"
dim "Ctrl-C to stop."
wait
