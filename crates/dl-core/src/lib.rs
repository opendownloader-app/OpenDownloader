//! `dl-core` — the decision-making half of opendownloader.
//!
//! This crate performs **zero I/O**. It never opens a socket, touches a file, reads a
//! clock, or spawns a thread; every entry point is a pure function or a state machine
//! driven entirely by bytes handed in from outside. TypeScript owns the browser APIs and
//! asks this crate what to do next.
//!
//! That constraint is not stylistic. It is what lets the same code compile to
//! `wasm32-unknown-unknown` for the extension and to the host triple for `cargo test`,
//! and it is what makes every decision here assertable against a fixture table instead of
//! a live browser.

pub mod classify;
pub mod ed2k;
pub mod hls;
pub mod integrity;
pub mod links;
pub mod mega;
pub mod plan;
pub mod policy;
pub mod quark;
pub mod session;
pub mod sites;
pub mod subs;

#[cfg(target_arch = "wasm32")]
pub mod wasm;

pub use classify::{classify, MediaCandidate, MediaKind, RequestMeta};
pub use hls::{
    parse_playlist, select_variant, HlsError, MasterPlaylist, MediaStream, Playlist, Rendition,
    Segment, Variant,
};
pub use integrity::Hasher;
pub use plan::{plan_chunks, ByteRange, ResumeState};
pub use policy::{is_restricted, refuse_encrypted};
pub use session::{DownloadSession, SessionState};
pub use subs::{cues_to_srt, cues_to_vtt, merge_vtt_segments, parse_cues, vtt_to_srt, Cue};
