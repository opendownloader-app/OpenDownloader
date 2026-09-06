# opendownloader

An open-source download manager for the browser. Resumable downloads with real
integrity verification, HLS streams remuxed into playable MP4, subtitles,
lossless audio extraction and format conversion — all locally, with the engine
written in Rust and compiled to WebAssembly.

**Every feature is free.** No account, no sign-in, no trial, no credits, no
usage limit, and no paid tier holding anything back. No ads, no tracking, and no
standing access to any site you have not explicitly enabled. There is nothing to
upgrade to, and no companion application to install.

Three ways to use it, from one source tree:

| | What it is | Where |
|---|---|---|
| **Extension** | Watches the page you are on and offers what it finds | Chrome, Edge, Firefox |
| **Web app** | Paste a link; no install | `apps/web` |
| **Relay** | Optional, self-hosted, for servers that refuse a web page | `crates/dl-relay` |

## What it does

- **Detects** media on pages you enable it for — progressive files
  (mp4/webm/mp3/pdf/zip/…) and HLS playlists. Select several and queue them at
  once.
- **Reads the big platforms directly** — YouTube, Bilibili, TikTok, Douyin,
  Instagram, Facebook, WeChat articles, X, Vimeo, Twitch, Reddit and others. On
  these it offers the site's own list of qualities rather than whatever the
  network happened to reveal, and merges separate video and audio into one file
  where the platform no longer ships a combined one.
- **Downloads** with byte-range resumption: interrupt it, close the tab, restart
  the browser, and it picks up from where it stopped rather than starting over.
- **Queues** as many as you like, running a configurable number at a time over a
  configurable number of connections each, with speed and time remaining shown.
- **Verifies** every finished file by reading it back off disk and hashing it,
  so the SHA-256 you see describes the file that exists. Supply a digest you
  expect and the result is reported as verified or as a mismatch.
- **Remuxes** HLS segment streams (MPEG-TS) into fragmented MP4 without
  re-encoding, so nothing is re-compressed and nothing is lost.
- **Extracts audio** — from a stream's alternate audio rendition, from a
  transport stream by dropping the video track, or from a progressive MP4 by
  copying its AAC samples out unchanged. None of these re-encode anything.
- **Saves subtitles** from a stream's subtitle renditions as WebVTT or SRT,
  merged correctly across segments, and translates them with the browser's
  on-device translator where one exists.
- **Converts** between MP4, WebM, M4A, MP3, WAV and OGG using the browser's own
  codecs — no ffmpeg build is shipped.
- **Repairs** a stalled download by remuxing leftover `.ts` segments from disk
  into one playable file.

The web app adds local transcription: subtitles generated from a file's own
speech by a Whisper model that runs in the browser.

## What it deliberately does not do

There is one principle, enforced in code rather than in a policy document:
**we do not break encryption.**

- `dl-core::policy` refuses the services whose catalogue is DRM-protected —
  Netflix, Disney+, Prime Video, Max, Hulu, Apple TV+, Spotify, Apple Music,
  Audible, Tidal, Deezer, Crunchyroll and their CDNs — checked against both the
  page origin and the media origin. There is no override and no advanced mode.
  The relay enforces the same list server-side, so running one is not a way
  around it.
- **Any encrypted stream, on any host**, is refused: HLS `#EXT-X-KEY` /
  `#EXT-X-SESSION-KEY`, and DASH `ContentProtection`. No key is ever fetched and
  nothing is ever decrypted. This is the check that does the real work, and it is
  why the host list above is short: a site with a free catalogue and a paid,
  protected one refuses its protected content by itself, on the evidence of its
  own manifest.
- Live streams are refused while they are live — they have no end to download
  to. A finished recording of one is an ordinary video.

Both rules are pure functions over their inputs, so they can be audited by
reading them.

Also out of scope, stated rather than silently missing: DASH manifests (`.mpd`)
as a download source, Safari, and a mobile app.

> **On distribution.** The Chrome Web Store's developer policy prohibits
> extensions that download from YouTube, and Edge mirrors it. A build with the
> platform extractors enabled will not survive on those stores. The web app, the
> relay, and a self-distributed extension are unaffected. See
> `docs/superpowers/plans/2026-09-05-major-platforms.md`.

## Architecture

**Rust decides, TypeScript fetches.** `dl-core` performs zero I/O: it is a set of
pure functions and state machines that answer "what should be fetched next", "is
this resume still valid", "what do these segments become" and "what does this
hash to". TypeScript owns every `fetch()` and every disk write, and asks Rust
what to do. That split is what lets the same code compile natively for
`cargo test` and to `wasm32-unknown-unknown` for the browser.

```
crates/
  dl-core/        zero I/O — policy, classification, HLS (with alternate
                  renditions), subtitles, range planning, resume, integrity,
                  per-site extractors, and the wasm surface
  dl-container/   zero I/O — MPEG-TS demux → fragmented MP4 mux, progressive
                  MP4 demux → audio-only MP4, video+audio merge; byte-identical
                  output for identical input
  dl-testserver/  local media server generating the same fixtures the unit
                  tests assert against
  dl-relay/       optional self-hosted CORS relay; the only crate that does I/O
packages/
  engine/         shared TypeScript: job store, settings, sinks, queue, engine,
                  subtitles, conversion, local tools
  ui/             shared DOM UI: the manager and the tools panels
apps/
  extension/      MV3 shell (TypeScript + Vite) → dist/ and dist-firefox/
  web/            the standalone site → dist/
```

Both front ends import the same engine and mount the same manager. The only
difference between them is what a host can do: the extension can watch a page
and save without a gesture, a web page can do neither, so those two things are
the entire `Platform` interface.

The MV3 service worker only observes `webRequest`. The download engine runs in a
dedicated **manager tab**, because an MV3 worker is terminated after ~30s idle,
after 5 minutes on a request, and if a `fetch()` response takes over 30s — all
of which a real download violates routinely. A normal document has none of those
limits and behaves identically on Chrome, Edge and Firefox.

`dl-container` is written in-house rather than taken from a crate: `transmux` has
~1.6k downloads and `mp4` has no fMP4 writer and no release since 2023, and this
is the piece that most needs byte-level determinism.

## Building

Requires a Rust toolchain with the `wasm32-unknown-unknown` target, and
`wasm-bindgen-cli` at **exactly** the version the workspace pins (`0.2.126`) —
a mismatch fails at glue generation with a schema error.

```bash
cargo install wasm-bindgen-cli --version 0.2.126 --locked
rustup target add wasm32-unknown-unknown

npm install                      # one install for the whole workspace
npm run build:extension          # → apps/extension/dist/          (Chrome, Edge)
npm run build:extension:firefox  # → apps/extension/dist-firefox/  (Firefox)
npm run build:web                # → apps/web/dist/
```

Load `apps/extension/dist/` via `chrome://extensions` → Developer mode → Load
unpacked, or `dist-firefox/` via `about:debugging` → Load Temporary Add-on.

### Checks

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
npm run typecheck                # every package and app
npm run e2e                      # extension, in a real Chrome
npm run smoke                    # web app, in a real Chrome
```

The end-to-end suites drive a real browser against `dl-testserver`, which
generates its media from the same Rust fixtures the unit tests use — so a
browser run and a `cargo test` run assert against byte-identical input and
neither can drift from the other.

> The repository lives on a shared VM mount that produces spurious archive/mmap
> failures when used as the cargo target directory. Set `CARGO_TARGET_DIR` to a
> path outside the mount (e.g. `/tmp/opendownloader-target`) for any build.

## Browser differences

Two capabilities genuinely differ, and both are feature-detected rather than
inferred from a user agent.

| | Chrome / Edge | Firefox |
|---|---|---|
| Sink | File System Access — a real seekable file handle | IndexedDB accumulation → Blob → downloads API |
| Connections | Up to 8 parallel ranges | 1 (a non-seeking sink must receive bytes in order) |
| Size ceiling | Disk | Practical limit at Blob materialisation |
| Unattended queue | After choosing a download folder, once | Always |

Choosing a download folder on Chrome or Edge is what turns one save dialog per
file into one permission for the session, and it is what lets a queue run while
you do something else. Firefox has no directory picker, so its downloads go
through the browser's own download manager and need no dialog to begin with.

## The relay

A web page cannot fetch a file from a server that sends no CORS headers; the
browser blocks it before the response is readable. The extension has host
permissions and never has this problem. `crates/dl-relay` is a small streaming
proxy that gets around it for the web app, and it is meant to be **run by you**:
there is no hosted one, and no paid tier. It refuses restricted hosts, private
network addresses and oversized responses. See `crates/dl-relay/README.md`.

## Licence

MIT OR Apache-2.0.
