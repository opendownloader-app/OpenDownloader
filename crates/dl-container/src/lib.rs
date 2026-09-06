//! `dl-container` — the bytes half of opendownloader.
//!
//! Takes MPEG-TS segment bytes — or the `moov` and chunks of one progressive MP4, or of
//! a video-only and an audio-only pair — and returns fragmented-MP4 bytes. Like `dl-core` it does
//! no I/O at all, and additionally guarantees **byte-identical output for identical
//! input**: nothing here embeds a timestamp, a random value, or a hash-map iteration
//! order. That determinism is what allows exact assertions rather than fuzzy tolerances.

pub mod aac;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
pub mod fmerge;
pub mod fmp4;
pub mod h264;
pub mod mp4;
pub mod mux;
pub mod remux;
pub mod ts;

pub use fmerge::{FragmentMerger, FragmentRead};
pub use mp4::{box_header, AudioExtractor, BoxHeader, ChunkPlan, Mp4Error};
pub use mux::{MergeRead, Muxer, Source};
pub use remux::{RemuxError, RemuxState, Remuxer};
pub use ts::{PesPacket, TsDemuxer, TsError};
