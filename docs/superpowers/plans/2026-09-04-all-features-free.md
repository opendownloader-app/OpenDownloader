# opendownloader — every feature, free, local

**Status (2026-09-05): implemented.** 200 Rust tests, 17 extension end-to-end
tests in a real Chrome, and the web app's smoke test all pass; clippy
`-D warnings`, `cargo fmt --check` and `tsc --noEmit` are clean across every
crate, package and app; the Chrome, Firefox and web builds all produce output.

Two deviations from the plan as written, both deliberate and both noted in the
tasks below: the queue auto-starts only where a sink can be opened without a
gesture (so the old "click Start" e2e assertions were rewritten rather than
kept), and `candidateForUrl` probes the server with a one-byte range request
when a pasted URL has no recognisable extension — without it, a link like
`https://cdn.example/asset?id=9` was refused as "not a media file".

**Source:** the competitive study
`市场洞察与竞品研究 - 竞品分析 - 产品竞争格局调研-191.md` (8 competitors: Video
DownloadHelper, IDM, FDM, JDownloader, yt-dlp, cobalt, Video Downloader
Professional, SnapTube). Its recommended feature list (§5.2), open-core split
(§6), form-factor judgement (§8), entry points (§9) and pricing advice (§10) are
the requirements; this document turns them into code.

**Decision that overrides the study:** the study proposed two paid features
(cloud relay acceleration, AI subtitle extraction + translation). Both are built
here as **free, local or self-hosted** capabilities instead. Nothing in the
product is gated, metered or credit-based, and there is no OpenApps account
integration. The one row the study marks "do not do" — downloading from
YouTube/Instagram and other restricted platforms — stays not done: it is refused
in code (`dl-core::policy`), it contradicts the product's compliance positioning,
and it would get the extension removed from every store.

---

## 1. Feature matrix

| # | Feature (study §5.2) | Study says | Before this plan | After this plan | Where it runs |
|---|---|---|---|---|---|
| 1 | HTML5 media sniffing | P0, free | ✓ `webRequest` observer, per-site opt-in | ✓ + multi-select, "download all", manual "add URL" | extension SW; webapp (URL input) |
| 2 | Resumable downloads | P0, free | ✓ range planning + `If-Range` validator | ✓ unchanged, now shared with webapp | engine |
| 3 | Integrity verification | P0, free | ✓ SHA-256 read-back | ✓ + user-supplied expected hash → *verified / mismatch* | engine |
| 4 | Container remux | P0, free | ✓ HLS TS → fMP4 (Rust) | ✓ + local `.ts`/`.m3u8` files remuxed offline; + general convert (MP4/WebM/MP3/WAV/M4A) via `mediabunny` | engine, webapp, manager |
| 5 | Batch download / queue | P0, free | ✗ one click per file, no scheduling | ✓ queue with concurrency limit, folder sink (one prompt per session), auto-start, reorder, pause/resume all, retry failed, clear done | engine `queue.ts` |
| 6 | Multi-connection acceleration | P1, free | ✓ fixed 4 ranges (Chrome/Edge) | ✓ configurable 1–8, speed + ETA shown | engine + settings |
| 7 | Audio extraction | P1, free | ✓ HLS audio-only (TS) | ✓ + HLS alternate-audio renditions; + progressive MP4 → M4A (Rust MP4 demux, lossless); + MP3 via convert | Rust `dl-container::mp4`, engine |
| 8 | Cloud relay acceleration | paid (proposed) | ✗ | ✓ **free, self-hosted** `dl-relay` (Rust) that the webapp can point at for CORS-blocked hosts; policy enforced server-side; no hosted paid tier | `crates/dl-relay` |
| 9 | AI subtitles + translation | paid (proposed) | ✗ | ✓ **free, local**: HLS subtitle renditions → `.vtt`/`.srt` (Rust merge); translation via the browser's on-device Translator API (Chrome 138+), feature-detected; transcription from audio via local Whisper (transformers.js) in the webapp | Rust `dl-core::subs`, webapp |
| 10 | Restricted platforms (YouTube…) | opportunity, "don't" | refused in code | **still refused** | `dl-core::policy` |

Form factors (study §8): **Chrome and Edge** ship from one build (Edge is an
unclaimed official-listing slot — entry point 2); **Firefox** from the second
manifest; **webapp** (cobalt-style, paste a link, same wasm engine) is the
"lightweight standalone site" the study suggests for people who cannot install
an extension. No mobile app (study: wrong form factor for this product).

---

## 2. Architecture after this plan

```
opendownloader/
├── Cargo.toml                       workspace: dl-core, dl-container, dl-testserver, dl-relay
├── package.json                     npm workspaces: packages/*, apps/*
├── crates/
│   ├── dl-core/                     zero I/O — policy, classify, hls (+renditions), plan,
│   │                                integrity, subs (WebVTT merge/SRT), session, wasm surface
│   ├── dl-container/                zero I/O — ts, h264, aac, fmp4, remux, mp4 (progressive demux
│   │                                → audio-only fMP4)
│   ├── dl-testserver/               local fixtures server for e2e (now also serves subtitles,
│   │                                alternate audio, and a progressive MP4)
│   └── dl-relay/                    optional self-hosted CORS relay (axum); policy enforced
├── packages/
│   ├── engine/                      shared TypeScript: types, idb, jobs, settings, wasm loader,
│   │                                fetch-retry, engine (progressive/HLS), sinks (file, folder,
│   │                                idb→blob), queue scheduler, subtitles, convert, translate,
│   │                                remux-local, audio-extract
│   │   └── src/wasm-gen/            generated (gitignored) — built by scripts/build-wasm.sh
│   └── ui/                          shared DOM UI: the manager (job list, queue bar, settings,
│                                    tools panels) mounted by both the extension and the webapp
├── apps/
│   ├── extension/                   MV3 shell: background sniffer, popup (multi-select),
│   │                                manager.html → mounts packages/ui
│   └── web/                         standalone site: URL input + local tools + the same manager
└── docs/store-listing/              Chrome / Edge / Firefox listing copy
```

**The seam does not move.** Rust still decides (what to fetch, whether a resume
is valid, how bytes become MP4/M4A/SRT, what a file hashes to). TypeScript still
performs every `fetch()` and every disk write. New Rust modules follow the same
rule: no `std::fs`, `std::net`, threads or clocks; deterministic output.

**Platform abstraction.** `packages/engine` takes a `Platform` object
(`saveBlob(blob, name)`, `openManager()`, `storage`) so the same sinks and queue
run in an extension page and in a plain web page. The extension's `Platform`
uses `downloads.download`; the webapp's uses an `<a download>`.

**Sinks.**

| Sink | Browser | Seekable | Prompts | Used for |
|---|---|---|---|---|
| `FileSink` (showSaveFilePicker) | Chrome/Edge | yes | one per file | single download |
| `FolderSink` (showDirectoryPicker, handle persisted in IDB) | Chrome/Edge | yes | **one per session** | batch queue, auto-start |
| `IdbSink` → Blob → `saveBlob` | all (Firefox, webapp fallback) | no | none | everything else |

Auto-start is possible on Chrome/Edge only once a folder is chosen (the file
picker needs a gesture per file); on Firefox and in the webapp fallback it is
always possible. The queue scheduler asks the sink factory whether it can open a
sink without a gesture and only auto-starts when it can.

---

## 3. Tasks

Legend: ☐ pending · ☑ done.

### Phase 0 — toolchain and plan
- ☑ Write this plan.
- ☑ Bump `wasm-bindgen` pin `=0.2.100` → `=0.2.126` to match the installed CLI (`cargo update -p wasm-bindgen --precise 0.2.126`); update README/spec constraint.

### Phase 1 — shared engine package (extension keeps working)
- ☑ Root `package.json` with npm workspaces; `packages/engine`, `packages/ui`.
- ☑ Move `apps/extension/src/{shared,manager/engine.ts,manager/sinks.ts,manager/fetch-retry.ts}` into `packages/engine/src/`; introduce `Platform`; wasm glue generated into `packages/engine/src/wasm-gen/`.
- ☑ Extension imports from `@opendownloader/engine`; `npm run build` / `build:firefox` / `typecheck` green; existing e2e green.

### Phase 2 — queue, folder sink, settings, speed/ETA, expected hash
- ☑ `settings.ts`: `maxConcurrentJobs` (default 3), `connectionsPerJob` (1–8, default 4), `autoStart`, `folderHandle` (IDB), `preferAudioOnly`.
- ☑ `FolderSink`: `getFileHandle(name,{create:true})`; dedupe filenames; `keepExistingData` only when resuming; permission re-request UI when `queryPermission` says `prompt`.
- ☑ `queue.ts`: scheduler that starts queued jobs up to the concurrency limit whenever a sink can be opened without a gesture; pause all / resume all / retry failed / clear done / move up / move down (`order` field on `Job`).
- ☑ Engine: throughput sampling → `bytesPerSecond`, `etaSeconds` in progress events; `connectionsPerJob` honoured.
- ☑ `expectedSha256` on `Job`; verification reports `verified` / `mismatch`; UI badge.
- ☑ Popup: checkboxes, "Download all", "Download selected", audio-only toggle for HLS; "Add URL" in manager.
- ☑ `packages/ui` manager: queue bar, settings panel, per-job speed/ETA, reorder, hash input.
- ☑ e2e: batch of three progressive jobs auto-runs with concurrency 2; expected-hash mismatch marks the job; pause-all/resume-all.

### Phase 3 — Rust: renditions, subtitles, MP4 audio extraction
- ☑ `hls.rs`: `Playlist::Master` becomes `MasterPlaylist { variants, audio: Vec<Rendition>, subtitles: Vec<Rendition> }` parsed from `#EXT-X-MEDIA`; tests.
- ☑ `subs.rs`: merge WebVTT segments (`X-TIMESTAMP-MAP` offset, header stripping, duplicate-cue removal) → one VTT; VTT → SRT; tests.
- ☑ `dl-container::mp4`: progressive MP4 box walker (`ftyp/moov/mdat`, `co64`, 64-bit sizes), sample table for the first AAC track (`stsd/mp4a/esds`, `stts`, `stsc`, `stsz`, `stco/co64`), chunk plan → audio-only fMP4 via existing `fmp4` writer; a builder for tests + testserver fixture; tests incl. byte-identity with the TS-path audio extraction.
- ☑ `wasm.rs`: `parse_playlist_js` returns the new shape; `merge_vtt_segments`, `vtt_to_srt`, `Mp4AudioExtractor` (`fromMoov`, `plan`, `pushChunk`).
- ☑ Engine: audio-only for a master with alternate audio downloads the audio rendition playlist; `subtitles.ts` fetches a rendition's segments and saves `.vtt`/`.srt`; `audio-extract.ts` drives `Mp4AudioExtractor` over a `File`/handle.
- ☑ Testserver: master with `#EXT-X-MEDIA` subtitles + audio groups, WebVTT segments, a progressive MP4 fixture and its expected M4A digest.
- ☑ e2e: subtitle export produces the expected SRT; MP4 → M4A digest matches the server's.

### Phase 4 — webapp
- ☑ `apps/web`: Vite + vanilla TS; sections: paste URL → queue; local tools (remux TS/M3U8, extract audio, convert, subtitles); the shared manager; task-oriented landing copy (study §9 entry point 3); links to stores and the source.
- ☑ CORS handling: probe failure explains the cause and offers the relay setting.
- ☑ Smoke test (Playwright borrowed from the extension) against the testserver: URL download verifies; local remux produces a playable fMP4.

### Phase 5 — convert, translate, transcribe
- ☑ `convert.ts`: `mediabunny` conversion (MP4/WebM/M4A/MP3/WAV; MP3 via `@mediabunny/mp3-encoder`), progress, cancel; used by manager "Convert" panel and the webapp.
- ☑ `translate.ts`: on-device Translator API (feature-detected, availability states surfaced); translates a VTT/SRT cue list; falls back with a clear message on browsers without it.
- ☑ `transcribe.ts` (webapp only): Whisper via `@huggingface/transformers` (WebGPU → wasm), 16 kHz decode via `mediabunny`, emits VTT/SRT; model cached by the browser.

### Phase 6 — relay, listings, docs
- ☑ `crates/dl-relay`: `GET /fetch?url=` streaming proxy with Range/If-Range passthrough, CORS headers, `dl-core::policy` refusal, size/host allow-list config, `/healthz`; README with `docker`/systemd notes.
- ☑ `docs/store-listing/{en,zh_CN,de,fr,es}.txt` (Chrome/Edge/Firefox copy, search terms), `apps/extension/public/_locales` name/description.
- ☑ README rewrite: features, three form factors, build, checks, browser differences, relay.
- ☑ Final checks: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `tsc --noEmit` for all packages, Chrome/Firefox/web builds, extension e2e, webapp smoke.

---

## 4. Interfaces (new or changed)

```rust
// dl-core::hls
pub struct Rendition { pub group_id: String, pub name: String, pub language: Option<String>,
                       pub url: Option<String>, pub default: bool, pub autoselect: bool, pub forced: bool }
pub struct MasterPlaylist { pub variants: Vec<Variant>, pub audio: Vec<Rendition>, pub subtitles: Vec<Rendition> }
pub enum Playlist { Master(MasterPlaylist), Media(MediaStream) }

// dl-core::subs
pub fn merge_vtt_segments(segments: &[&str]) -> String;   // one WebVTT document
pub fn vtt_to_srt(vtt: &str) -> String;
pub fn parse_cues(vtt_or_srt: &str) -> Vec<Cue>;           // for translation
pub fn cues_to_vtt(cues: &[Cue]) -> String; pub fn cues_to_srt(cues: &[Cue]) -> String;

// dl-container::mp4
pub struct BoxHeader { pub kind: [u8;4], pub size: u64, pub header_len: u8 }
pub fn box_header(bytes: &[u8]) -> Option<BoxHeader>;
pub struct AudioExtractor { .. }
impl AudioExtractor {
    pub fn from_moov(moov: &[u8]) -> Result<Self, Mp4Error>;
    pub fn chunks(&self) -> &[ChunkPlan];                   // absolute file ranges, in order
    pub fn push_chunk(&mut self, index: usize, bytes: &[u8]) -> Result<Vec<u8>, Mp4Error>; // fMP4 bytes
}
```

```ts
// packages/engine
export interface Platform { saveBlob(blob: Blob, filename: string): Promise<void>; canSaveSilently: boolean }
export interface Settings { maxConcurrentJobs: number; connectionsPerJob: number; autoStart: boolean }
export class Queue { start(job), pause(job), pauseAll(), resumeAll(), retryFailed(), clearDone(), move(job, delta), tick() }
export interface Progress { received; total; status; message?; bytesPerSecond?; etaSeconds? }
```

---

## 5. Out of scope (stated, not silently dropped)

- **DASH** (`.mpd`) — the study never mentions it; the v1 spec deferred it. The
  fMP4 concatenation path would serve it, but MPD parsing (SegmentTemplate,
  SegmentTimeline) is its own project.
- **Muxing separate video and audio renditions into one file** — CMAF streams
  with video-only variants plus an alternate audio group download as video-only
  today; the UI says so and offers the audio rendition as its own file.
- **Safari** — needs an Xcode wrapper; the study rates it lowest value.
- **Mobile app** — wrong form factor for this product (study §8).
- **A hosted relay or any paid tier** — the relay is shipped as source for
  self-hosting; running one publicly is an operational decision, not code.
