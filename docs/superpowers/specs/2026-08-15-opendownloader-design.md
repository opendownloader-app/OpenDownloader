# opendownloader — Design Spec

**Status: design approved 2026-08-15. Superseded in part on 2026-09-05** by
`docs/superpowers/plans/2026-09-04-all-features-free.md`, which keeps every
decision below and adds the queue, alternate renditions, subtitles, MP4 audio
extraction, conversion, the standalone web app and the optional relay. The one
entry below that changed is the wasm-bindgen pin.

An open-source, ad-free download manager browser extension. Sniffs open HTML5 media,
downloads it with real resumability and integrity verification, and remuxes HLS segment
streams into a playable MP4 — all locally, with the entire engine written in Rust and
compiled to WebAssembly.

Scope of this document is technical only: architecture, interfaces, and the decisions
behind them. No roadmap, no positioning.

---

## Decision log

| Decision | Rationale | Status |
|---|---|---|
| Rust compiles to `wasm32-unknown-unknown` and runs **inside the extension**; no native host | Single store install, zero binary-install friction | ✅ Decided (user) |
| Engine runs in a **dedicated manager tab**, not the service worker or an offscreen document | MV3 SW dies at 30s idle / 5min per request / 30s per fetch response; Firefox has no `offscreen` API. A normal document is the only runtime identical on all three browsers | ✅ Decided (user) |
| v1 handles **progressive HTTP + HLS**; DASH deferred | Covers the overwhelming majority of open HTML5 media | ✅ Decided (user) |
| **Encrypted HLS is refused outright** (`#EXT-X-KEY` / `#EXT-X-SESSION-KEY`) | Removes any argument that the codebase performs access-control circumvention | ✅ Decided (user) |
| Targets **Chrome, Edge, Firefox** from one source tree | `opencapture` precedent (`dist/` + `dist-firefox/`) | ✅ Decided (user) |
| **Fully free, no gating.** OpenApps client scaffolded but never gates anything | `openvidsub` precedent | ✅ Decided (user) |
| Output container is **fragmented MP4**, never progressive MP4 | fMP4 is append-only; progressive MP4 needs a backwards seek to patch `moov`, which the Firefox sink cannot do | ✅ Decided |
| `dl-container` (TS demux + fMP4 mux) is **written in-house**, not taken from a crate | `transmux` has ~1.6k downloads; `mp4` has no fMP4 writer and no release since 2023. This is the core differentiator and needs byte-level determinism | ✅ Decided |
| `m3u8-rs` **is** taken as a dependency | 6.0.1, actively maintained, 1.45M downloads, pure parser with no I/O — safe on wasm | ✅ Decided |
| Detection uses **`optional_host_permissions`, per-site opt-in** | Not `<all_urls>` at install; same trust posture `opencapture` adopted when it dropped its static content script | ✅ Decided |
| `wasm-bindgen` pinned `=0.2.126` | Must match the installed `wasm-bindgen-cli` exactly or glue-gen fails on a schema mismatch (was `0.2.100`; raised 2026-09-05 to match the installed CLI) | ✅ Decided |

---

## 1. Architecture

**The seam: Rust decides, TypeScript fetches.** `dl-core` performs zero I/O. It is a set of
pure functions and state machines. TypeScript owns every `fetch()`, every disk write, and
every browser API call; it asks Rust what to do next and feeds the results back. This is the
same split that makes `opencapture`'s `shot-core` testable natively, and it is the reason
every decision in this system can be asserted against a fixture table instead of a live browser.

```
opendownloader/
├── Cargo.toml                  # workspace (independent of the platform workspace)
├── crates/
│   ├── dl-core/                # ZERO I/O
│   │   ├── policy.rs           # restricted-host + encrypted-stream refusal
│   │   ├── classify.rs         # request metadata → MediaCandidate
│   │   ├── hls.rs              # playlist model, variant selection, segment expansion
│   │   ├── plan.rs             # range planning, resume state machine
│   │   ├── integrity.rs        # streaming SHA-256
│   │   └── wasm.rs             # cfg(target_arch = "wasm32") — the entire JS-facing surface
│   ├── dl-container/           # pure bytes-in/bytes-out
│   │   ├── ts.rs               # MPEG-TS demux: PAT/PMT/PES → elementary streams
│   │   ├── h264.rs             # AnnexB → AVCC, SPS parse → avcC decoder config
│   │   ├── aac.rs              # ADTS → raw AAC frames + AudioSpecificConfig
│   │   └── fmp4.rs             # ftyp/moov/moof/mdat writer
│   └── dl-qa/                  # native CLI: mp4-info, ts-info, hash
├── apps/extension/             # TypeScript + Vite → dist/ and dist-firefox/
└── testdata/fixtures/          # generated, gitignored
```

### Component responsibilities

**`dl-core::policy`** — a pure predicate over `(page_origin, media_url)`. A compiled-in
blocklist of DRM/ToS-restricted hosts. Blocked candidates never reach the UI. Separately,
`refuse_encrypted()` rejects any playlist carrying key tags. Being a pure function over a
list is what makes this auditable.

**`dl-core::classify`** — `(url, content_type, content_length, content_disposition)` →
`Option<MediaCandidate>`. Distinguishes progressive media, HLS playlists, and non-media.
Also derives a filename (Content-Disposition first, URL path second, extension from MIME
third).

**`dl-core::hls`** — wraps `m3u8-rs`. Master playlist → variant list sorted by bandwidth;
media playlist → absolute segment URL list with byte-range support (`#EXT-X-BYTERANGE`) and
init-segment handling (`#EXT-X-MAP`). Relative URLs are resolved against the playlist URL.

**`dl-core::plan`** — the resume brain. Given total size, range support, and the set of
already-completed byte ranges, produces the next chunks to fetch. Coverage-complete by
construction: the union of completed ranges plus planned ranges is always exactly `0..total`,
with no gaps and no overlaps.

**`dl-core::integrity`** — incremental SHA-256 over an ordered byte stream, used for the
read-back verification pass.

**`dl-container`** — takes MPEG-TS segment bytes, returns fMP4 bytes. Stateful across
segments (the `moov` is emitted once from the first segment's codec configuration; each
subsequent segment becomes a `moof`+`mdat` fragment).

### Data flow

1. Service worker observes `webRequest.onBeforeRequest` / `onHeadersReceived` (non-blocking;
   fully available to normal extensions in MV3 — only `webRequestBlocking` is policy-only).
2. Metadata goes to `classify` + `policy`. Surviving candidates are stored per-tab and shown
   as a badge count.
3. User opens the manager tab and starts a download. The manager tab owns the engine loop.
4. For progressive: probe → plan ranges → `fetch()` each range → write to sink.
   For HLS: fetch playlist → parse → select variant → fetch segments → each segment through
   `dl-container` → write returned fMP4 bytes to sink.
5. Resume state is persisted to IndexedDB after every completed chunk.
6. On completion, the file is read back off disk and hashed; the digest is shown and stored.

### The sink abstraction

Forced by Firefox having no File System Access API.

- `FsaSink` (Chrome/Edge): a real `FileSystemWritableFileStream`. Supports positioned writes,
  so parallel range downloads land out of order safely.
- `IdbSink` (Firefox): chunks accumulate in IndexedDB (disk-backed, so memory-safe), then are
  assembled into a Blob handed to `downloads.download`. Sequential single-connection only, and
  subject to a practical size ceiling at Blob-materialization time.

Both implement `Sink { write(pos, bytes), finalize(), readBack() }`. `dl-core` never knows
which one is in use.

---

## 2. Key interfaces

```rust
// dl-core::policy
pub fn is_restricted(page_origin: &str, media_url: &str) -> bool;
pub fn refuse_encrypted(playlist_text: &str) -> bool;

// dl-core::classify
pub enum MediaKind { Progressive, HlsPlaylist }
pub struct MediaCandidate {
    pub url: String,
    pub kind: MediaKind,
    pub filename: String,
    pub mime: Option<String>,
    pub size: Option<u64>,
}
pub fn classify(meta: &RequestMeta) -> Option<MediaCandidate>;

// dl-core::plan
pub struct ByteRange { pub start: u64, pub end: u64 } // inclusive end
pub struct ResumeState {
    pub total: Option<u64>,
    pub validator: Option<String>,   // ETag or Last-Modified
    pub accepts_ranges: bool,
    pub completed: Vec<ByteRange>,   // normalized: sorted, non-overlapping, merged
}
impl ResumeState {
    pub fn record(&mut self, r: ByteRange);
    pub fn missing(&self) -> Vec<ByteRange>;
    pub fn downloaded(&self) -> u64;
    pub fn is_complete(&self) -> bool;
}
pub fn plan_chunks(state: &ResumeState, chunk_size: u64, max_parallel: usize) -> Vec<ByteRange>;

// dl-core::integrity
pub struct Hasher(/* private */);
impl Hasher { pub fn new() -> Self; pub fn update(&mut self, b: &[u8]); pub fn finish_hex(self) -> String; }

// dl-container
pub struct Remuxer(/* private */);
impl Remuxer {
    pub fn new() -> Self;
    /// Returns the fMP4 bytes to append for this segment (init segment on first call).
    pub fn push_ts_segment(&mut self, ts: &[u8]) -> Result<Vec<u8>, RemuxError>;
}
```

The wasm surface (`dl-core::wasm`) exposes exactly these as `wasm-bindgen` types, plus a
`DownloadSession` façade that holds the `ResumeState`, the `Hasher`, and the optional
`Remuxer` for one job.

---

## 3. Constraints

- `wasm-bindgen` must be pinned `=0.2.126` to match the installed CLI.
- All builds set `CARGO_TARGET_DIR` to a path outside `/Volumes/My Shared Files/...` — the
  shared mount produces spurious archive/mmap failures on larger builds.
- `dl-core` and `dl-container` must build for both the host triple and
  `wasm32-unknown-unknown`, so neither may use `std::fs`, `std::net`, threads, or time.
- No dependency may pull in `getrandom` without a wasm-compatible backend.
- fMP4 output must be byte-identical for identical input, so nothing may embed a timestamp,
  a random number, or a hash-map iteration order.
