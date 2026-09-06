# opendownloader Implementation Plan

> **Superseded on 2026-09-05** by
> `docs/superpowers/plans/2026-09-04-all-features-free.md`, which builds on
> everything below. Two facts here are now out of date: the `wasm-bindgen` pin is
> `=0.2.126`, and the extension's TypeScript has moved out of
> `apps/extension/src/{shared,manager}` into `packages/engine` and `packages/ui`,
> which the standalone web app shares. Kept as the record of how the first
> version was built.

**Status (2026-08-15): all 14 tasks implemented.** 94 Rust tests green, clippy
`-D warnings` and `fmt --check` clean, `tsc --noEmit` clean, both `dist/` and
`dist-firefox/` build, and the compiled wasm exercised end to end under Node.
Two deviations from the plan as written, both noted in the tasks below:
`dl-qa` was dropped (its assertions live in the box-walking test helpers), and
`hls::Playlist::Media` carries a `MediaStream` rather than a bare `Vec<Segment>`
so `#EXT-X-MAP` init segments are representable.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an ad-free download-manager browser extension whose entire engine is Rust compiled to WebAssembly — sniffing open HTML5 media, downloading it with true resumability and integrity verification, and remuxing HLS into playable fragmented MP4.

**Architecture:** Rust decides, TypeScript fetches. `dl-core` (zero I/O: policy, classification, HLS, range planning, resume, hashing) and `dl-container` (zero I/O: MPEG-TS demux → fMP4 mux) compile to both the host triple and `wasm32-unknown-unknown`. The MV3 service worker only observes `webRequest`; the download engine lives in a dedicated manager tab, the one runtime that behaves identically on Chrome, Edge, and Firefox.

**Tech Stack:** Rust (stable 1.97), `wasm-bindgen =0.2.100`, `m3u8-rs 6`, `sha2`, TypeScript + Vite, MV3.

**Spec:** `docs/superpowers/specs/2026-08-15-opendownloader-design.md`

## Global Constraints

- `wasm-bindgen` pinned exactly `=0.2.100` (must match installed `wasm-bindgen-cli` 0.2.100).
- Every cargo invocation: `export PATH="$HOME/.cargo/bin:$PATH"` and `CARGO_TARGET_DIR=/tmp/opendownloader-target` with `CARGO_INCREMENTAL=0`. Never target a directory under `/Volumes/My Shared Files/...`. Disk has ~4.5 GB free — `rm -rf` the target dir after final verification.
- `dl-core` and `dl-container` must contain no `std::fs`, `std::net`, `std::thread`, or `std::time` — they must build for `wasm32-unknown-unknown`.
- fMP4 output must be byte-identical for identical input: no timestamps, no RNG, no hash-map iteration order in serialization paths.
- `git status` is unreliable on this mount. Verify commits by comparing `git rev-parse HEAD:<f>` against `git hash-object <f>`.
- Workspace is independent of the platform workspace at the repo root (own `Cargo.toml`, own `rust-toolchain.toml`), matching `opencapture`/`openvidsub`.

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`, `crates/dl-core/Cargo.toml`, `crates/dl-core/src/lib.rs`

**Interfaces:**
- Produces: a buildable workspace with `dl-core` as a library crate.

- [ ] **Step 1: Create the workspace manifest**

```toml
# Cargo.toml
[workspace]
members = ["crates/dl-core", "crates/dl-container", "crates/dl-qa"]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/opentoolkitsg/opendownloader"

[workspace.dependencies]
sha2 = "0.10"
m3u8-rs = "6"
thiserror = "2"
wasm-bindgen = "=0.2.100"
```

- [ ] **Step 2: Create `rust-toolchain.toml`**

```toml
[toolchain]
channel = "stable"
components = ["clippy", "rustfmt"]
targets = ["wasm32-unknown-unknown"]
```

- [ ] **Step 3: Create `crates/dl-core/Cargo.toml` and an empty `lib.rs`, then build**

Run: `CARGO_TARGET_DIR=/tmp/opendownloader-target cargo build`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "opendownloader: workspace scaffold"
```

---

### Task 2: `dl-core::policy` — restricted hosts and encrypted-stream refusal

**Files:**
- Create: `crates/dl-core/src/policy.rs`
- Modify: `crates/dl-core/src/lib.rs`

**Interfaces:**
- Produces: `policy::is_restricted(page_origin: &str, media_url: &str) -> bool`, `policy::refuse_encrypted(playlist_text: &str) -> bool`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn blocks_restricted_hosts_on_either_origin() {
    assert!(is_restricted("https://www.youtube.com", "https://cdn.example.com/a.mp4"));
    assert!(is_restricted("https://blog.example.com", "https://r1---sn-x.googlevideo.com/v"));
    assert!(is_restricted("https://www.netflix.com", "https://x/y.m3u8"));
}

#[test]
fn allows_ordinary_sites() {
    assert!(!is_restricted("https://blog.example.com", "https://cdn.example.com/a.mp4"));
}

#[test]
fn subdomain_match_is_boundary_aware() {
    assert!(is_restricted("https://m.youtube.com", "https://x/y.mp4"));
    // must NOT match a lookalike domain that merely ends with the blocked string
    assert!(!is_restricted("https://notyoutube.com", "https://x/y.mp4"));
}

#[test]
fn refuses_encrypted_playlists() {
    assert!(refuse_encrypted("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n"));
    assert!(refuse_encrypted("#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES\n"));
    assert!(!refuse_encrypted("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n"));
    assert!(!refuse_encrypted("#EXTM3U\n#EXTINF:4,\nseg.ts\n"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `CARGO_TARGET_DIR=/tmp/opendownloader-target cargo test -p dl-core policy`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

Host extraction is a small manual parse (no `url` crate — it pulls `idna` and bloats wasm). Boundary-aware matching: a host matches a blocked entry when it equals it **or** ends with `"." + entry`.

- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-core: compliance policy (restricted hosts, encrypted-stream refusal)"`

---

### Task 3: `dl-core::classify` — request metadata → MediaCandidate

**Files:**
- Create: `crates/dl-core/src/classify.rs`

**Interfaces:**
- Consumes: `policy::is_restricted`
- Produces: `MediaKind`, `MediaCandidate`, `RequestMeta`, `classify(&RequestMeta) -> Option<MediaCandidate>`

```rust
pub struct RequestMeta {
    pub url: String,
    pub page_origin: String,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub content_disposition: Option<String>,
}
```

- [ ] **Step 1: Write the failing tests**

```rust
fn meta(url: &str, ct: Option<&str>) -> RequestMeta {
    RequestMeta { url: url.into(), page_origin: "https://ok.example".into(),
        content_type: ct.map(Into::into), content_length: None, content_disposition: None }
}

#[test]
fn detects_progressive_by_mime() {
    let c = classify(&meta("https://cdn.x/a?q=1", Some("video/mp4"))).unwrap();
    assert!(matches!(c.kind, MediaKind::Progressive));
    assert_eq!(c.filename, "a.mp4");
}

#[test]
fn detects_hls_by_extension_and_mime() {
    assert!(matches!(classify(&meta("https://cdn.x/master.m3u8", None)).unwrap().kind, MediaKind::HlsPlaylist));
    assert!(matches!(classify(&meta("https://cdn.x/p", Some("application/vnd.apple.mpegurl"))).unwrap().kind, MediaKind::HlsPlaylist));
}

#[test]
fn ignores_non_media() {
    assert!(classify(&meta("https://cdn.x/app.js", Some("application/javascript"))).is_none());
    assert!(classify(&meta("https://cdn.x/page", Some("text/html"))).is_none());
}

#[test]
fn content_disposition_wins_over_url_path() {
    let mut m = meta("https://cdn.x/blob?id=9", Some("video/mp4"));
    m.content_disposition = Some("attachment; filename=\"My Clip.mp4\"".into());
    assert_eq!(classify(&m).unwrap().filename, "My Clip.mp4");
}

#[test]
fn restricted_hosts_never_surface() {
    let mut m = meta("https://cdn.x/a.mp4", Some("video/mp4"));
    m.page_origin = "https://www.youtube.com".into();
    assert!(classify(&m).is_none());
}

#[test]
fn hls_segments_are_not_candidates() {
    // .ts segments are fetched as part of a playlist job, never offered on their own
    assert!(classify(&meta("https://cdn.x/seg00001.ts", Some("video/mp2t"))).is_none());
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — MIME table first, extension fallback second; sanitize the filename (strip path separators, control chars, and leading dots).
- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-core: media candidate classification"`

---

### Task 4: `dl-core::plan` — resume state and range planning

**Files:**
- Create: `crates/dl-core/src/plan.rs`

**Interfaces:**
- Produces: `ByteRange`, `ResumeState`, `plan_chunks(&ResumeState, u64, usize) -> Vec<ByteRange>`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn record_merges_adjacent_and_overlapping_ranges() {
    let mut s = ResumeState::new(Some(100), true);
    s.record(ByteRange { start: 0, end: 9 });
    s.record(ByteRange { start: 10, end: 19 });   // adjacent → merges
    s.record(ByteRange { start: 15, end: 24 });   // overlapping → merges
    assert_eq!(s.completed, vec![ByteRange { start: 0, end: 24 }]);
    assert_eq!(s.downloaded(), 25);
}

#[test]
fn missing_is_the_exact_complement() {
    let mut s = ResumeState::new(Some(100), true);
    s.record(ByteRange { start: 10, end: 19 });
    s.record(ByteRange { start: 50, end: 59 });
    assert_eq!(s.missing(), vec![
        ByteRange { start: 0, end: 9 },
        ByteRange { start: 20, end: 49 },
        ByteRange { start: 60, end: 99 },
    ]);
}

#[test]
fn plan_respects_chunk_size_and_parallelism() {
    let s = ResumeState::new(Some(1000), true);
    let chunks = plan_chunks(&s, 256, 3);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0], ByteRange { start: 0, end: 255 });
    assert_eq!(chunks[2], ByteRange { start: 512, end: 767 });
}

#[test]
fn without_range_support_plan_is_one_whole_file_chunk() {
    let s = ResumeState::new(Some(1000), false);
    assert_eq!(plan_chunks(&s, 256, 4), vec![ByteRange { start: 0, end: 999 }]);
}

#[test]
fn unknown_total_plans_a_single_open_ended_chunk() {
    let s = ResumeState::new(None, false);
    assert_eq!(plan_chunks(&s, 256, 4), vec![ByteRange { start: 0, end: u64::MAX }]);
}

#[test]
fn completed_file_plans_nothing() {
    let mut s = ResumeState::new(Some(10), true);
    s.record(ByteRange { start: 0, end: 9 });
    assert!(s.is_complete());
    assert!(plan_chunks(&s, 4, 4).is_empty());
}
```

- [ ] **Step 2: Write the coverage property test**

```rust
#[test]
fn planned_plus_completed_always_covers_the_file_exactly() {
    // deterministic pseudo-random sweep; no external proptest dependency
    let mut seed: u64 = 0x2545F4914F6CDD1D;
    let mut next = move || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
    for _ in 0..2000 {
        let total = 1 + next() % 5000;
        let mut s = ResumeState::new(Some(total), true);
        for _ in 0..(next() % 8) {
            let a = next() % total;
            let b = (a + next() % 500).min(total - 1);
            s.record(ByteRange { start: a, end: b });
        }
        let chunk = 1 + next() % 700;
        let mut covered = vec![false; total as usize];
        for r in s.completed.iter().cloned().chain(plan_chunks(&s, chunk, 64)) {
            for i in r.start..=r.end.min(total - 1) { covered[i as usize] = true; }
        }
        assert!(covered.iter().all(|&c| c), "gap in coverage");
    }
}
```

- [ ] **Step 3: Run to verify failure.**
- [ ] **Step 4: Implement** — `record` inserts then normalizes (sort by start, merge where `next.start <= cur.end + 1`). `missing` walks the normalized list emitting the gaps. `plan_chunks` slices `missing` into `chunk_size` pieces and takes the first `max_parallel`.
- [ ] **Step 5: Run tests** — Expected: PASS (note: `plan_chunks` with `max_parallel: 64` must return enough chunks for the coverage test, so the property test uses a high cap).
- [ ] **Step 6: Commit** — `git commit -m "dl-core: resume state and range planning"`

---

### Task 5: `dl-core::integrity` — streaming SHA-256

**Files:**
- Create: `crates/dl-core/src/integrity.rs`

**Interfaces:**
- Produces: `Hasher::new()`, `Hasher::update(&[u8])`, `Hasher::finish_hex() -> String`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn matches_known_sha256_vectors() {
    let mut h = Hasher::new();
    h.update(b"abc");
    assert_eq!(h.finish_hex(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}

#[test]
fn chunking_does_not_change_the_digest() {
    let mut whole = Hasher::new();
    whole.update(b"the quick brown fox");
    let mut split = Hasher::new();
    split.update(b"the quick ");
    split.update(b"brown fox");
    assert_eq!(whole.finish_hex(), split.finish_hex());
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — thin wrapper over `sha2::Sha256`.
- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-core: streaming SHA-256"`

---

### Task 6: `dl-core::hls` — playlist parsing and variant selection

**Files:**
- Create: `crates/dl-core/src/hls.rs`

**Interfaces:**
- Consumes: `policy::refuse_encrypted`
- Produces:

```rust
pub struct Variant { pub url: String, pub bandwidth: u64, pub resolution: Option<(u64, u64)>, pub codecs: Option<String> }
pub struct Segment { pub url: String, pub byte_range: Option<ByteRange>, pub duration_ms: u64 }
pub enum Playlist { Master(Vec<Variant>), Media(Vec<Segment>) }
pub fn parse_playlist(text: &str, base_url: &str) -> Result<Playlist, HlsError>;
pub fn select_variant(v: &[Variant], prefer_highest: bool) -> Option<&Variant>;
pub fn resolve_url(base: &str, rel: &str) -> String;
```

- [ ] **Step 1: Write the failing tests**

```rust
const MASTER: &str = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\n360p.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\n720p/index.m3u8\n";

const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
#EXTINF:4.000,\nseg0.ts\n#EXTINF:3.500,\nseg1.ts\n#EXT-X-ENDLIST\n";

#[test]
fn parses_master_and_resolves_relative_urls() {
    let Playlist::Master(v) = parse_playlist(MASTER, "https://cdn.x/hls/master.m3u8").unwrap()
        else { panic!("expected master") };
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].url, "https://cdn.x/hls/360p.m3u8");
    assert_eq!(v[1].url, "https://cdn.x/hls/720p/index.m3u8");
    assert_eq!(v[1].bandwidth, 2_400_000);
    assert_eq!(v[0].resolution, Some((640, 360)));
}

#[test]
fn parses_media_segments_with_durations() {
    let Playlist::Media(s) = parse_playlist(MEDIA, "https://cdn.x/hls/360p.m3u8").unwrap()
        else { panic!("expected media") };
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].url, "https://cdn.x/hls/seg0.ts");
    assert_eq!(s[0].duration_ms, 4000);
    assert_eq!(s[1].duration_ms, 3500);
}

#[test]
fn selects_highest_bandwidth_variant() {
    let Playlist::Master(v) = parse_playlist(MASTER, "https://cdn.x/m.m3u8").unwrap() else { panic!() };
    assert_eq!(select_variant(&v, true).unwrap().bandwidth, 2_400_000);
    assert_eq!(select_variant(&v, false).unwrap().bandwidth, 800_000);
}

#[test]
fn encrypted_playlists_are_rejected() {
    let enc = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.key\"\n#EXTINF:4,\nseg0.ts\n";
    assert!(matches!(parse_playlist(enc, "https://cdn.x/a.m3u8"), Err(HlsError::Encrypted)));
}

#[test]
fn resolves_absolute_and_root_relative_urls() {
    assert_eq!(resolve_url("https://cdn.x/a/b.m3u8", "https://other.y/c.ts"), "https://other.y/c.ts");
    assert_eq!(resolve_url("https://cdn.x/a/b.m3u8", "/root.ts"), "https://cdn.x/root.ts");
    assert_eq!(resolve_url("https://cdn.x/a/b.m3u8", "sub/c.ts"), "https://cdn.x/a/sub/c.ts");
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — call `refuse_encrypted` before parsing; then `m3u8_rs::parse_playlist_res`. `resolve_url` is a hand-rolled resolver (scheme-relative, root-relative, path-relative) to avoid the `url` crate's wasm weight.
- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-core: HLS playlist parsing and variant selection"`

---

### Task 7: `dl-container::ts` — MPEG-TS demux

**Files:**
- Create: `crates/dl-container/Cargo.toml`, `crates/dl-container/src/lib.rs`, `crates/dl-container/src/ts.rs`

**Interfaces:**
- Produces:

```rust
pub struct PesPacket { pub pid: u16, pub stream_type: u8, pub pts: Option<u64>, pub dts: Option<u64>, pub data: Vec<u8> }
pub struct TsDemuxer { /* private */ }
impl TsDemuxer {
    pub fn new() -> Self;
    pub fn push(&mut self, ts: &[u8]) -> Result<Vec<PesPacket>, TsError>;
}
```

- [ ] **Step 1: Write the failing tests**

Build synthetic TS in the test itself — 188-byte packets, sync byte `0x47`, a PAT on PID 0 pointing at a PMT, a PMT declaring stream type `0x1B` (H.264) on PID 256 and `0x0F` (ADTS AAC) on PID 257, then PES packets with a known payload and PTS.

```rust
#[test]
fn demuxes_pat_pmt_and_pes_payloads() {
    let ts = build_test_ts();               // helper defined in the test module
    let mut d = TsDemuxer::new();
    let pes = d.push(&ts).unwrap();
    assert_eq!(pes.len(), 2);
    assert_eq!(pes[0].pid, 256);
    assert_eq!(pes[0].stream_type, 0x1B);
    assert_eq!(pes[0].pts, Some(900_000));   // 10s at 90kHz
    assert_eq!(pes[0].data, VIDEO_PAYLOAD);
    assert_eq!(pes[1].stream_type, 0x0F);
}

#[test]
fn rejects_a_stream_with_no_sync_byte() {
    let mut d = TsDemuxer::new();
    assert!(matches!(d.push(&[0u8; 188]), Err(TsError::LostSync)));
}

#[test]
fn tolerates_a_pes_split_across_packets() {
    let ts = build_split_pes_ts();
    let mut d = TsDemuxer::new();
    assert_eq!(d.push(&ts).unwrap()[0].data, LONG_PAYLOAD);
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — iterate 188-byte packets; validate sync; read `payload_unit_start_indicator`, PID, `adaptation_field_control`; skip the adaptation field; accumulate PES per PID, flushing the previous PES when a new `payload_unit_start_indicator` arrives; parse the PES header for the 33-bit PTS/DTS.
- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-container: MPEG-TS demuxer"`

---

### Task 8: `dl-container::h264` and `::aac` — codec configuration

**Files:**
- Create: `crates/dl-container/src/h264.rs`, `crates/dl-container/src/aac.rs`

**Interfaces:**
- Produces:

```rust
// h264
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8>;
pub fn find_parameter_sets(annexb: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>); // (sps, pps)
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8>;
pub fn sps_resolution(sps: &[u8]) -> Option<(u32, u32)>;
pub fn is_keyframe(annexb: &[u8]) -> bool;

// aac
pub struct AdtsFrame { pub payload_range: (usize, usize), pub sample_rate: u32, pub channels: u8, pub object_type: u8 }
pub fn parse_adts(buf: &[u8]) -> Vec<AdtsFrame>;
pub fn audio_specific_config(object_type: u8, sample_rate: u32, channels: u8) -> Vec<u8>;
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn converts_annexb_start_codes_to_length_prefixes() {
    let annexb = [0,0,0,1, 0x65, 0xAA, 0xBB, 0,0,1, 0x41, 0xCC];
    assert_eq!(annexb_to_avcc(&annexb),
        vec![0,0,0,3, 0x65,0xAA,0xBB, 0,0,0,2, 0x41,0xCC]);
}

#[test]
fn extracts_sps_and_pps_by_nal_type() {
    let annexb = [0,0,0,1, 0x67, 0x42, 0x00, 0x1E, 0,0,0,1, 0x68, 0xCE, 0,0,0,1, 0x65, 0x11];
    let (sps, pps) = find_parameter_sets(&annexb);
    assert_eq!(sps, Some(vec![0x67, 0x42, 0x00, 0x1E]));
    assert_eq!(pps, Some(vec![0x68, 0xCE]));
}

#[test]
fn avcc_carries_the_profile_bytes_from_the_sps() {
    let sps = [0x67, 0x42, 0xC0, 0x1E, 0xAA];
    let cfg = build_avcc(&sps, &[0x68, 0xCE]);
    assert_eq!(cfg[0], 1);          // configurationVersion
    assert_eq!(cfg[1], 0x42);       // AVCProfileIndication
    assert_eq!(cfg[2], 0xC0);       // profile_compatibility
    assert_eq!(cfg[3], 0x1E);       // AVCLevelIndication
    assert_eq!(cfg[4], 0xFF);       // lengthSizeMinusOne = 3
}

#[test]
fn detects_idr_keyframes() {
    assert!(is_keyframe(&[0,0,0,1, 0x65, 0x11]));       // nal_unit_type 5 = IDR
    assert!(!is_keyframe(&[0,0,0,1, 0x41, 0x11]));      // nal_unit_type 1 = non-IDR
}

#[test]
fn parses_adts_headers_and_frame_boundaries() {
    // 2 frames: 44100 Hz, stereo, AAC-LC, 7-byte headers
    let buf = build_two_adts_frames();
    let f = parse_adts(&buf);
    assert_eq!(f.len(), 2);
    assert_eq!(f[0].sample_rate, 44100);
    assert_eq!(f[0].channels, 2);
    assert_eq!(f[0].object_type, 2);
}

#[test]
fn builds_a_two_byte_audio_specific_config() {
    // AAC-LC (2), 44100 Hz (index 4), stereo (2)
    assert_eq!(audio_specific_config(2, 44100, 2), vec![0x12, 0x10]);
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — NAL splitting handles both 3-byte and 4-byte start codes. `sps_resolution` needs an exp-Golomb reader over the RBSP with emulation-prevention bytes removed.
- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-container: H.264 and AAC codec configuration"`

---

### Task 9: `dl-container::fmp4` — fragmented MP4 writer

**Files:**
- Create: `crates/dl-container/src/fmp4.rs`

**Interfaces:**
- Produces:

```rust
pub struct TrackConfig { pub track_id: u32, pub timescale: u32, pub kind: TrackKind }
pub enum TrackKind { Video { width: u32, height: u32, avcc: Vec<u8> }, Audio { channels: u8, sample_rate: u32, asc: Vec<u8> } }
pub struct Sample { pub data: Vec<u8>, pub duration: u32, pub is_sync: bool, pub cts_offset: i32 }
pub fn write_init_segment(tracks: &[TrackConfig]) -> Vec<u8>;
pub fn write_fragment(seq: u32, track_id: u32, base_decode_time: u64, samples: &[Sample]) -> Vec<u8>;
```

- [ ] **Step 1: Write the failing tests**

```rust
fn box_types(buf: &[u8]) -> Vec<String> { /* walk top-level boxes: u32 size + 4cc */ }

#[test]
fn init_segment_has_ftyp_then_moov() {
    let init = write_init_segment(&[video_track()]);
    assert_eq!(box_types(&init), vec!["ftyp", "moov"]);
    assert!(contains_box(&init, "mvex"));   // required or no fragment will play
    assert!(contains_box(&init, "avcC"));
}

#[test]
fn fragment_is_moof_then_mdat() {
    let f = write_fragment(1, 1, 0, &[sample(100), sample(100)]);
    assert_eq!(box_types(&f), vec!["moof", "mdat"]);
}

#[test]
fn data_offset_points_at_the_first_byte_of_mdat_payload() {
    // the single most common fMP4 bug: a wrong trun data_offset makes every player fail
    let f = write_fragment(1, 1, 0, &[sample(100)]);
    let moof_size = u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize;
    let data_offset = read_trun_data_offset(&f);
    assert_eq!(data_offset as usize, moof_size + 8);
}

#[test]
fn output_is_byte_identical_across_runs() {
    let a = write_init_segment(&[video_track()]);
    let b = write_init_segment(&[video_track()]);
    assert_eq!(a, b);
}

#[test]
fn all_box_sizes_match_their_actual_extent() {
    let init = write_init_segment(&[video_track(), audio_track()]);
    assert!(walk_and_validate_all_box_sizes(&init), "a box size field disagrees with its content");
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — a small `BoxWriter` that reserves a 4-byte size placeholder, writes children, then backfills. `moov` = `mvhd` + one `trak` per track (`tkhd`/`mdia`/`mdhd`/`hdlr`/`minf`/`stbl` with empty sample tables) + `mvex`/`trex`. Fragment = `moof`(`mfhd`+`traf`(`tfhd`+`tfdt`+`trun`)) + `mdat`. Write the `trun` twice: once to learn the `moof` size, then again with the correct `data_offset`.
- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-container: fragmented MP4 writer"`

---

### Task 10: `dl-container::Remuxer` — the TS → fMP4 pipeline

**Files:**
- Create: `crates/dl-container/src/remux.rs`

**Interfaces:**
- Consumes: `ts::TsDemuxer`, `h264::*`, `aac::*`, `fmp4::*`
- Produces: `Remuxer::new()`, `Remuxer::push_ts_segment(&mut self, &[u8]) -> Result<Vec<u8>, RemuxError>`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn first_segment_emits_init_plus_fragment() {
    let mut r = Remuxer::new();
    let out = r.push_ts_segment(&fixture_ts_segment()).unwrap();
    assert_eq!(&box_types(&out)[..2], &["ftyp", "moov"]);
    assert!(box_types(&out).contains(&"moof".to_string()));
}

#[test]
fn later_segments_emit_only_fragments() {
    let mut r = Remuxer::new();
    r.push_ts_segment(&fixture_ts_segment()).unwrap();
    let out = r.push_ts_segment(&fixture_ts_segment()).unwrap();
    assert_eq!(box_types(&out), vec!["moof", "mdat"]);
}

#[test]
fn sequence_numbers_and_decode_time_advance_monotonically() {
    let mut r = Remuxer::new();
    r.push_ts_segment(&fixture_ts_segment()).unwrap();
    let a = read_mfhd_sequence(&r.push_ts_segment(&fixture_ts_segment()).unwrap());
    let b = read_mfhd_sequence(&r.push_ts_segment(&fixture_ts_segment()).unwrap());
    assert!(b > a);
}

#[test]
fn concatenated_output_is_byte_identical_across_runs() {
    let run = || {
        let mut r = Remuxer::new();
        let mut o = Vec::new();
        for _ in 0..3 { o.extend(r.push_ts_segment(&fixture_ts_segment()).unwrap()); }
        o
    };
    assert_eq!(run(), run());
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — demux → group PES by PID → convert video PES to AVCC samples with durations derived from consecutive PTS deltas (last sample inherits the previous duration) → emit init on first call from the discovered SPS/PPS/ADTS config → emit a fragment per segment.
- [ ] **Step 4: Run tests** — Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -m "dl-container: TS to fMP4 remux pipeline"`

---

### Task 11: `dl-core::wasm` — the JS-facing surface

**Files:**
- Create: `crates/dl-core/src/wasm.rs`
- Modify: `crates/dl-core/Cargo.toml` (add `wasm-bindgen`, `dl-container`, `[lib] crate-type = ["cdylib", "rlib"]`)

**Interfaces:**
- Produces (all `#[wasm_bindgen]`):

```rust
pub fn classify_request(json: &str) -> Option<String>;        // RequestMeta JSON → MediaCandidate JSON
pub fn parse_playlist_js(text: &str, base: &str) -> Result<String, JsValue>;
pub struct DownloadSession;
impl DownloadSession {
    pub fn new(total: Option<u64>, accepts_ranges: bool, remux: bool) -> Self;
    pub fn plan(&self, chunk_size: u64, max_parallel: usize) -> String;  // JSON [{start,end}]
    pub fn record(&mut self, start: u64, end: u64);
    pub fn push_segment(&mut self, bytes: &[u8]) -> Result<Vec<u8>, JsValue>;
    pub fn hash_update(&mut self, bytes: &[u8]);
    pub fn hash_hex(&self) -> String;
    pub fn state_json(&self) -> String;
    pub fn restore(json: &str) -> Result<DownloadSession, JsValue>;
}
```

- [ ] **Step 1: Write the failing native test** (the surface is `cfg`-gated to wasm, so test the underlying serde round-trip natively)

```rust
#[test]
fn session_state_survives_a_json_round_trip() {
    let mut s = SessionState::new(Some(1000), true, false);
    s.resume.record(ByteRange { start: 0, end: 99 });
    let restored = SessionState::from_json(&s.to_json()).unwrap();
    assert_eq!(restored.resume.completed, s.resume.completed);
    assert_eq!(restored.resume.total, Some(1000));
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** — `SessionState` is the portable core; `wasm.rs` is a thin `#[wasm_bindgen]` shell over it, gated on `cfg(target_arch = "wasm32")`.
- [ ] **Step 4: Verify both targets build**

```bash
CARGO_TARGET_DIR=/tmp/opendownloader-target cargo test --workspace
CARGO_TARGET_DIR=/tmp/opendownloader-target cargo build -p dl-core --target wasm32-unknown-unknown
```

- [ ] **Step 5: Commit** — `git commit -m "dl-core: wasm surface"`

---

### Task 12: Extension shell — manifest, service worker sniffer

**Files:**
- Create: `apps/extension/package.json`, `vite.config.ts`, `tsconfig.json`, `src/manifest.chrome.json`, `src/manifest.firefox.json`, `src/background/index.ts`, `src/background/sniffer.ts`, `src/shared/types.ts`

**Interfaces:**
- Consumes: the wasm surface from Task 11.
- Produces: `sniffer.attach()`, per-tab candidate store, `chrome.runtime` message contract `{action: "listCandidates" | "openManager", tabId}`.

- [ ] **Step 1: Write the manifest**

Permissions: `storage`, `downloads`, `webRequest`, `tabs`. `optional_host_permissions: ["<all_urls>"]` — **not** granted at install. `content_security_policy.extension_pages` must include `'wasm-unsafe-eval'` or WebAssembly is blocked outright.

- [ ] **Step 2: Implement the sniffer** — `chrome.webRequest.onHeadersReceived` (non-blocking, `["responseHeaders"]`), extract content-type/length/disposition, call `classify_request`, dedupe by URL, store per tab, update the action badge.

- [ ] **Step 3: Verify the build**

Run: `cd apps/extension && npm run build`
Expected: `dist/` contains `manifest.json`, `background.js`, and the wasm asset.

- [ ] **Step 4: Commit** — `git commit -m "extension: MV3 shell and webRequest sniffer"`

---

### Task 13: Manager tab — the engine

**Files:**
- Create: `src/manager/manager.html`, `src/manager/engine.ts`, `src/manager/sinks.ts`, `src/manager/store.ts`, `src/manager/ui.ts`

**Interfaces:**
- Consumes: `DownloadSession`, `parse_playlist_js`.
- Produces: `Sink { write(pos, bytes), finalize(), readBack() }`, `FsaSink`, `IdbSink`, `startJob(candidate)`, `resumeJob(id)`.

- [ ] **Step 1: Implement the sinks** — `FsaSink` uses `createWritable({keepExistingData: true})` with `{type:"write", position}`; `IdbSink` stores `{jobId, pos, bytes}` records and assembles on finalize.

- [ ] **Step 2: Implement the progressive path** — probe with `Range: bytes=0-0`, read `Content-Range`/`ETag`, construct `DownloadSession`, loop `plan()` → `fetch` with `Range` + `If-Range` → `sink.write` → `record()` → persist `state_json()` to IndexedDB.

- [ ] **Step 3: Implement the HLS path** — fetch playlist → `parse_playlist_js` → if master, `select_variant` and fetch again → for each segment, `fetch` → `push_segment` → `sink.write` at the running offset.

- [ ] **Step 4: Implement the read-back verification** — after finalize, stream the file back through `hash_update` and display `hash_hex()`.

- [ ] **Step 5: Commit** — `git commit -m "extension: manager-tab download engine with resume and verification"`

---

### Task 14: Firefox build variant

**Files:**
- Modify: `vite.config.ts`, `src/manager/sinks.ts`
- Create: `scripts/build-firefox.mjs`

- [ ] **Step 1: Emit `dist-firefox/`** with the Firefox manifest (event page via `background.scripts`, `browser_specific_settings.gecko.id`).
- [ ] **Step 2: Feature-detect the sink** — `"showSaveFilePicker" in window ? FsaSink : IdbSink`; force `max_parallel = 1` when the sink cannot seek.
- [ ] **Step 3: Verify** both `dist/` and `dist-firefox/` build clean.
- [ ] **Step 4: Commit** — `git commit -m "extension: Firefox build variant with IndexedDB sink"`

---

## Self-Review

**Spec coverage:** policy → Task 2; classify → Task 3; plan/resume → Task 4; integrity → Task 5; hls → Task 6; container (ts/h264/aac/fmp4/remux) → Tasks 7–10; wasm surface → Task 11; sniffer + permissions → Task 12; manager tab, sinks, both download paths, read-back verification → Task 13; three-browser targets → Tasks 12 and 14. `dl-qa` is declared in the workspace manifest but has no task — it is a native inspection CLI whose assertions are covered inline by the Task 9/10 box-walking helpers, so it is dropped from the workspace members list rather than left as an empty crate.

**Type consistency:** `ByteRange` is defined in Task 4 and reused by Tasks 6, 11, 13. `MediaCandidate`/`MediaKind` defined in Task 3, consumed in Tasks 12–13. `Sample`/`TrackConfig` defined in Task 9, consumed in Task 10. `push_ts_segment` is named identically in Tasks 10 and 11.
