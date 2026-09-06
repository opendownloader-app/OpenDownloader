//! Fragmented video-only MP4 + fragmented audio-only MP4 → one fragmented MP4.
//!
//! [`crate::mux`] does the same job for *progressive* inputs: it reads their sample
//! tables and rewrites every sample into fresh fragments. YouTube's adaptive formats are
//! not progressive. They arrive already fragmented —
//!
//! ```text
//! videoonly  ftyp(28) moov(711) sidx(1508) moof(1544) mdat(6786) …
//! audioonly  ftyp(24) moov(699) sidx(800)  moof(1816) mdat(160156) …
//! ```
//!
//! — and their sample tables are deliberately empty, because every sample is described
//! by its own `moof` instead. The progressive muxer therefore fails on them at the first
//! box it reads, with "stsc is empty". Bilibili's DASH has the same shape, so merging a
//! video and an audio rendition into one playable file needs this module rather than
//! that one.
//!
//! The saving grace is that a fragment is self-contained, so **no sample is ever
//! touched**. Merging is two steps:
//!
//! 1. parse both `moov`s and build one that describes both tracks;
//! 2. stream each input's `moof`+`mdat` pairs through unchanged except for two 4-byte
//!    fields, interleaved by decode time.
//!
//! The two fields are `tfhd.track_ID` — YouTube's two renditions both call themselves
//! track 1, so the audio one has to be renumbered — and `mfhd.sequence_number`, which
//! becomes one counter shared by both tracks. Both rewrites are *size-preserving*, and
//! that is the whole trick: `trun.data_offset` is measured from the start of its `moof`,
//! so as long as the `moof` keeps its size and its `mdat` stays immediately behind it,
//! every offset inside the fragment stays valid and nothing else needs recomputing.
//!
//! Like the rest of the crate this is I/O-free and deterministic: the caller fetches the
//! byte ranges [`FragmentMerger::reads`] names and feeds them back in, and identical
//! input produces identical output.

use serde::{Deserialize, Serialize};

use crate::fmp4::{bx, full_bx};
use crate::mp4::{be_u16, be_u32, be_u64, box_header, child, handler_type};

pub use crate::mp4::Mp4Error;
// Reused rather than redefined: a caller that already matches on a progressive merge's
// source can drive this one through the same match arms.
pub use crate::mux::Source;

/// Output track ids, the same two [`crate::mux`] and [`crate::remux`] use.
const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;

/// The common timebase the two tracks' fragment start times are compared in.
const INTERLEAVE_TIMESCALE: u128 = 1000;

/// Movie timescale for the combined `mvhd` and `mehd`. Track timescales are per-track
/// and are carried through from the inputs untouched.
const MOVIE_TIMESCALE: u32 = 1000;

/// The 3x3 unity transformation matrix, in 16.16 / 2.30 fixed point.
const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// `tfhd` flag `base-data-offset-present`.
const TFHD_BASE_DATA_OFFSET: u32 = 0x00_0001;

/// One `moof`+`mdat` pair of one input that the caller must fetch and feed back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentRead {
    pub source: Source,
    /// Absolute byte offset in that input's file.
    pub offset: u64,
    pub len: u64,
}

/// Merges a fragmented video-only and a fragmented audio-only MP4, a fragment at a time.
pub struct FragmentMerger {
    /// `ftyp` + the combined `moov`, built up front because both heads already carry
    /// everything it needs.
    init: Vec<u8>,
    reads: Vec<FragmentRead>,
    /// Index into `reads` of the next read expected.
    next_read: usize,
    init_emitted: bool,
    /// The next `mfhd.sequence_number`, shared by both tracks.
    sequence: u32,
    duration_ms: u64,
}

impl FragmentMerger {
    /// Index both inputs from their heads.
    ///
    /// `video_head` and `audio_head` are each input's bytes from 0 up to and including
    /// its `sidx` — in practice the first 64 KiB, which the caller has already fetched.
    /// Trailing bytes are harmless: everything past the `sidx` is ignored, and a
    /// half-read box at the end of the head is simply not walked into.
    pub fn from_heads(video_head: &[u8], audio_head: &[u8]) -> Result<Self, Mp4Error> {
        let video = Input::parse(
            video_head,
            b"vide",
            VIDEO_TRACK_ID,
            Mp4Error::Malformed("video input has no video track"),
        )?;
        let audio = Input::parse(audio_head, b"soun", AUDIO_TRACK_ID, Mp4Error::NoAudioTrack)?;

        let duration_ms =
            u64::try_from(video.duration_ms().max(audio.duration_ms())).unwrap_or(u64::MAX);
        let reads = interleave(&video, &audio);
        let init = init_segment(&video, &audio, duration_ms);

        Ok(Self {
            init,
            reads,
            next_read: 0,
            init_emitted: false,
            sequence: 1,
            duration_ms,
        })
    }

    /// Index inputs that arrive as several pieces each.
    ///
    /// `video` and `audio` are lists of `(bytes, offset)`: the init segment, then each
    /// media segment's head, with the offset each begins at in its own stream. See
    /// [`Input::parse_spans`] for why one `sidx` is not always enough.
    pub fn from_segment_heads(
        video: &[(&[u8], u64)],
        audio: &[(&[u8], u64)],
    ) -> Result<Self, Mp4Error> {
        let video = Input::parse_spans(
            video,
            b"vide",
            VIDEO_TRACK_ID,
            Mp4Error::Malformed("video input has no video track"),
        )?;
        let audio = Input::parse_spans(audio, b"soun", AUDIO_TRACK_ID, Mp4Error::NoAudioTrack)?;

        let duration_ms =
            u64::try_from(video.duration_ms().max(audio.duration_ms())).unwrap_or(u64::MAX);
        let reads = interleave(&video, &audio);
        let init = init_segment(&video, &audio, duration_ms);

        Ok(Self {
            init,
            reads,
            next_read: 0,
            init_emitted: false,
            sequence: 1,
            duration_ms,
        })
    }

    /// The `moof`+`mdat` byte ranges to fetch, interleaved by decode time.
    pub fn reads(&self) -> &[FragmentRead] {
        &self.reads
    }

    /// Feed read `index`, which must be the next one in order and exactly
    /// `reads()[index].len` bytes long.
    ///
    /// Returns the bytes to append to the output: the combined init segment (`ftyp` plus
    /// a `moov` describing both tracks) followed by the first fragment on the first call,
    /// one fragment per call after that. A rejected call leaves the merger untouched, so
    /// the caller can re-fetch and retry.
    pub fn push(&mut self, index: usize, bytes: &[u8]) -> Result<Vec<u8>, Mp4Error> {
        // A complete merger has no next read; reporting `expected == reads.len()` tells
        // the caller exactly that.
        let read = match self.reads.get(index) {
            Some(&read) if index == self.next_read => read,
            _ => {
                return Err(Mp4Error::OutOfOrder {
                    expected: self.next_read,
                    got: index,
                })
            }
        };
        if bytes.len() as u64 != read.len {
            return Err(Mp4Error::ChunkLength {
                expected: read.len,
                got: bytes.len() as u64,
            });
        }

        let track_id = match read.source {
            Source::Video => VIDEO_TRACK_ID,
            Source::Audio => AUDIO_TRACK_ID,
        };
        // Built into a local first: a malformed fragment must not leave the sequence
        // counter advanced, or a retry of the same read would skip a number.
        let (fragment, sequence) = rewrite_fragments(bytes, track_id, self.sequence)?;

        let mut out = Vec::with_capacity(fragment.len() + self.pending_init_len());
        if !self.init_emitted {
            out.extend_from_slice(&self.init);
            self.init_emitted = true;
        }
        out.extend_from_slice(&fragment);
        self.sequence = sequence;
        self.next_read += 1;
        Ok(out)
    }

    /// Bytes the next [`Self::push`] will prepend, which is the init segment exactly
    /// once.
    fn pending_init_len(&self) -> usize {
        if self.init_emitted {
            0
        } else {
            self.init.len()
        }
    }

    /// True once every planned read has been fed.
    pub fn is_complete(&self) -> bool {
        self.next_read == self.reads.len()
    }

    /// Total duration in milliseconds: the longer of the two tracks, summed from the
    /// `sidx` subsegment durations.
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
}

/// One indexed input: the boxes its half of the combined `moov` contributes, and where
/// its fragments are.
struct Input {
    /// The whole `trak` box, header included, with its `tkhd.track_ID` already rewritten.
    trak: Vec<u8>,
    /// The whole `trex` box, likewise.
    trex: Vec<u8>,
    fragments: Vec<Fragment>,
    /// Every fragment's duration summed, in `timescale` units.
    total_ticks: u64,
    /// The `sidx` timescale, which is the media timescale of the track it indexes.
    timescale: u32,
}

impl Input {
    /// Read one input's head: its track configuration and its fragment index.
    ///
    /// `missing` is the error for a head that does not carry the track it was handed in
    /// as — an audio-only file passed as the video input is a caller mistake worth
    /// reporting, not a file to silently produce a soundtrack-shaped video from.
    fn parse(
        head: &[u8],
        handler: &[u8; 4],
        track_id: u32,
        missing: Mp4Error,
    ) -> Result<Self, Mp4Error> {
        Self::parse_spans(&[(head, 0)], handler, track_id, missing)
    }

    /// Index an input that arrives as several pieces rather than one.
    ///
    /// Each span is `(bytes, offset)` — a piece of the input and where it begins in the
    /// whole stream. One `sidx` per span is read, and its fragment offsets are placed
    /// relative to that span, so the fragments of every piece land at their true
    /// positions in the concatenation.
    ///
    /// This exists because a rendition is not always one file. YouTube and Bilibili ship
    /// one, with a single `sidx` at the head indexing every fragment in it, which is what
    /// `parse` above assumes. Vimeo's adaptive format ships an init segment followed by
    /// twenty-odd `.m4s` files, each carrying its own `sidx` describing only itself.
    /// Reading just the first indexed one segment of twenty-one and produced 2.6 MB of an
    /// 88 MB video: a file that opens in nothing, and looks like a completed download.
    ///
    /// The `moov` comes from whichever span holds it — the init segment, for a stream
    /// shaped that way — and spans with no `sidx` are skipped rather than refused, since
    /// an init segment has none.
    fn parse_spans(
        spans: &[(&[u8], u64)],
        handler: &[u8; 4],
        track_id: u32,
        missing: Mp4Error,
    ) -> Result<Self, Mp4Error> {
        let mut trak_and_id = None;
        let mut fragments = Vec::new();
        let mut total_ticks: u64 = 0;
        let mut timescale = 0u32;

        for (bytes, base) in spans {
            let top = head_spans(bytes);
            if trak_and_id.is_none() {
                if let Some(moov) = top.iter().find(|s| &s.kind == b"moov") {
                    let moov_body = &bytes[moov.body..moov.end];
                    let (trak, source_id) =
                        extract_trak(moov_body, handler, track_id, missing.clone())?;
                    let trex = extract_trex(moov_body, source_id, track_id)?;
                    trak_and_id = Some((trak, trex));
                }
            }
            // Walking to discover fragments is not an option: the caller holds heads and
            // nothing else, so the index has to come from the index box.
            let Some(sidx) = top.iter().find(|s| &s.kind == b"sidx") else {
                continue;
            };
            let index = parse_sidx(&bytes[sidx.body..sidx.end], base + sidx.end as u64)?;
            // Every piece restarts its own `sidx` timeline at zero, so decode times are
            // carried forward across pieces rather than taken as stated. Without this
            // every fragment claims to start at the beginning and the interleave puts
            // them all in one place.
            let offset_ms = to_ms(total_ticks, index.timescale.max(1));
            fragments.extend(index.fragments.into_iter().map(|mut f| {
                f.start_ms += offset_ms;
                f
            }));
            total_ticks += index.total_ticks;
            timescale = index.timescale;
        }

        let (trak, trex) =
            trak_and_id.ok_or(Mp4Error::Malformed("fragmented input has no moov"))?;
        if fragments.is_empty() {
            return Err(Mp4Error::Malformed(
                "fragmented input has no sidx to index its fragments",
            ));
        }

        Ok(Self {
            trak,
            trex,
            fragments,
            total_ticks,
            timescale,
        })
    }

    fn duration_ms(&self) -> u128 {
        to_ms(self.total_ticks, self.timescale)
    }
}

/// One fragment of one input, as its `sidx` describes it.
struct Fragment {
    offset: u64,
    len: u64,
    /// Decode time of the fragment's first sample, in milliseconds.
    start_ms: u128,
}

/// Merge the two fragment lists into the order they must be written in.
///
/// The same two-finger merge [`crate::mux::interleave`] does, for the same reason:
/// whichever track's next fragment starts earlier goes first, compared in milliseconds so
/// a 90 kHz video track and a 44.1 kHz audio track are commensurable, in `u128` because a
/// 90 kHz tick count multiplied by 1000 leaves `u64` after a few years of media. Ties go
/// to video, which is deterministic and what a player prefers: the video for an instant
/// should already be buffered when its audio arrives.
fn interleave(video: &Input, audio: &Input) -> Vec<FragmentRead> {
    let mut reads = Vec::with_capacity(video.fragments.len() + audio.fragments.len());
    let (mut v, mut a) = (0usize, 0usize);
    while v < video.fragments.len() || a < audio.fragments.len() {
        let next_video = video.fragments.get(v).map(|f| f.start_ms);
        let next_audio = audio.fragments.get(a).map(|f| f.start_ms);
        let take_video = match (next_video, next_audio) {
            (Some(vt), Some(at)) => vt <= at,
            (Some(_), None) => true,
            _ => false,
        };
        let (source, fragment) = if take_video {
            v += 1;
            (Source::Video, &video.fragments[v - 1])
        } else {
            a += 1;
            (Source::Audio, &audio.fragments[a - 1])
        };
        reads.push(FragmentRead {
            source,
            offset: fragment.offset,
            len: fragment.len,
        });
    }
    reads
}

/// Ticks in `timescale` units, as milliseconds.
fn to_ms(ticks: u64, timescale: u32) -> u128 {
    u128::from(ticks) * INTERLEAVE_TIMESCALE / u128::from(timescale)
}

// --- the combined init segment ---

/// `ftyp` plus a `moov` describing both tracks.
///
/// Built fresh rather than by editing one input's `moov` in place: the parts that must be
/// carried through verbatim — each `trak` with its `mdhd` timescale, its `stsd` and the
/// `avcC` or `esds` inside it, and each `trex` with its per-track sample defaults — are
/// copied whole, and the parts that describe the *file* rather than a track are the ones
/// that would have needed rewriting anyway.
fn init_segment(video: &Input, audio: &Input, duration_ms: u64) -> Vec<u8> {
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"iso5");
    ftyp_body.extend_from_slice(&512u32.to_be_bytes());
    // The same brands [`crate::fmp4`] writes, so a merged file and a remuxed one declare
    // themselves identically.
    for brand in [b"iso5", b"iso6", b"mp41", b"avc1", b"dash"] {
        ftyp_body.extend_from_slice(brand);
    }

    let mut moov = mvhd();
    moov.extend_from_slice(&video.trak);
    moov.extend_from_slice(&audio.trak);
    moov.extend_from_slice(&mvex(video, audio, duration_ms));

    let mut out = bx(b"ftyp", &ftyp_body);
    out.extend_from_slice(&bx(b"moov", &moov));
    out
}

/// The movie header. Zero duration, which is both correct and normal for a fragmented
/// file — the samples are not described here — with the real total in the `mehd` instead.
fn mvhd() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time — fixed for determinism
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&[0u8; 8]); // reserved
    for v in UNITY_MATRIX {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&[0u8; 24]); // pre_defined
                                     // Two tracks, so the next one a writer could add is 3.
    b.extend_from_slice(&3u32.to_be_bytes());
    full_bx(b"mvhd", 0, 0, &b)
}

/// `mvex` announces that fragments follow; without it a player reads the empty sample
/// tables, concludes the file has no samples, and plays nothing.
///
/// Each `trex` is the input's own, so per-track default sample duration, size and flags
/// survive — a fragment whose `trun` omits a field falls back to these, and substituting
/// zeroes makes such a fragment decode with the wrong durations.
fn mvex(video: &Input, audio: &Input, duration_ms: u64) -> Vec<u8> {
    // `mehd` is where a fragmented file states its total duration. Version 0 is 32-bit,
    // which is 49 days of milliseconds; saturating is harmless at that scale.
    let fragment_duration = u32::try_from(duration_ms).unwrap_or(u32::MAX);
    let mut body = full_bx(b"mehd", 0, 0, &fragment_duration.to_be_bytes());
    body.extend_from_slice(&video.trex);
    body.extend_from_slice(&audio.trex);
    bx(b"mvex", &body)
}

/// Copy the `trak` with the given handler type out of a `moov` body, renumbered.
///
/// Returns the copy and the track id it used to carry. YouTube's video and audio
/// renditions are separate files and both call themselves track 1, which is exactly why
/// the audio one has to be renumbered here — two tracks sharing an id in one `moov`
/// describe a file no player can demux.
fn extract_trak(
    moov_body: &[u8],
    handler: &[u8; 4],
    track_id: u32,
    missing: Mp4Error,
) -> Result<(Vec<u8>, u32), Mp4Error> {
    let mut found = None;
    for span in spans(moov_body)? {
        if &span.kind != b"trak" {
            continue;
        }
        let body = &moov_body[span.body..span.end];
        let Some(mdia) = child(body, b"mdia")? else {
            continue;
        };
        if handler_type(mdia)? == Some(*handler) {
            found = Some(span);
            break;
        }
    }
    let span = found.ok_or(missing)?;

    let mut trak = moov_body[span.start..span.end].to_vec();
    let header_len = span.body - span.start;
    let tkhd = spans(&trak[header_len..])?
        .into_iter()
        .find(|s| &s.kind == b"tkhd")
        .ok_or(Mp4Error::Malformed("trak has no tkhd"))?;
    // Full box: version+flags(4), then creation and modification times — 4 bytes each in
    // version 0 and 8 in version 1 — then the track id.
    let at = header_len + tkhd.body;
    let id_at = match trak.get(at) {
        Some(0) => at + 12,
        Some(1) => at + 20,
        _ => return Err(Mp4Error::Malformed("tkhd version")),
    };
    let source_id = be_u32(&trak, id_at).ok_or(Mp4Error::Malformed("tkhd"))?;
    trak[id_at..id_at + 4].copy_from_slice(&track_id.to_be_bytes());
    Ok((trak, source_id))
}

/// Copy the `trex` describing `source_id` out of a `moov` body, renumbered.
///
/// A `mvex` with one `trex` in it is the normal case for a single-track rendition, so an
/// id that matches nothing falls back to the only entry rather than failing: some writers
/// number the `trex` independently of the `tkhd`.
fn extract_trex(moov_body: &[u8], source_id: u32, track_id: u32) -> Result<Vec<u8>, Mp4Error> {
    let mvex = spans(moov_body)?
        .into_iter()
        .find(|s| &s.kind == b"mvex")
        .ok_or(Mp4Error::Malformed("fragmented input has no mvex"))?;
    let body = &moov_body[mvex.body..mvex.end];

    let mut fallback = None;
    let mut matched = None;
    for span in spans(body)? {
        if &span.kind != b"trex" {
            continue;
        }
        // Full box: version+flags(4), then track_ID.
        let id = be_u32(body, span.body + 4).ok_or(Mp4Error::Malformed("trex"))?;
        fallback.get_or_insert(span.clone());
        if id == source_id {
            matched = Some(span);
            break;
        }
    }
    let span = matched
        .or(fallback)
        .ok_or(Mp4Error::Malformed("fragmented input has no trex"))?;

    let mut trex = body[span.start..span.end].to_vec();
    let id_at = (span.body - span.start) + 4;
    trex[id_at..id_at + 4].copy_from_slice(&track_id.to_be_bytes());
    Ok(trex)
}

// --- the fragment index ---

/// What one `sidx` says about where the fragments are and how long they last.
struct Index {
    timescale: u32,
    fragments: Vec<Fragment>,
    total_ticks: u64,
}

/// Parse a `sidx` body (ISO/IEC 14496-12 §8.16.3).
///
/// `sidx_end` is the box's end offset in the file, which is what the referenced ranges
/// are measured from: the first referenced byte is at `sidx_end + first_offset`, and each
/// subsequent one follows immediately. Each range is exactly one `moof`+`mdat` pair, and
/// `subsegment_duration` gives its length on the timeline — so the whole interleave can
/// be planned from the head, without fetching a single fragment.
fn parse_sidx(body: &[u8], sidx_end: u64) -> Result<Index, Mp4Error> {
    let version = *body.first().ok_or(Mp4Error::Malformed("sidx"))?;
    let timescale = be_u32(body, 8).ok_or(Mp4Error::Malformed("sidx"))?;
    if timescale == 0 {
        return Err(Mp4Error::Malformed("sidx timescale is zero"));
    }
    // version+flags(4), reference_ID(4), timescale(4), then earliest_presentation_time
    // and first_offset — 32-bit each in version 0, 64-bit each in version 1. Everything
    // after them is identical in both versions.
    let (first_offset, at) = match version {
        0 => (
            u64::from(be_u32(body, 16).ok_or(Mp4Error::Malformed("sidx first_offset"))?),
            20,
        ),
        1 => (
            be_u64(body, 20).ok_or(Mp4Error::Malformed("sidx first_offset"))?,
            28,
        ),
        _ => return Err(Mp4Error::Malformed("sidx version")),
    };

    // reserved(2), reference_count(2), then 12 bytes per reference.
    let count = usize::from(be_u16(body, at + 2).ok_or(Mp4Error::Malformed("sidx"))?);
    let entries = body
        .get(at + 4..)
        .filter(|e| e.len() >= count * 12)
        .ok_or(Mp4Error::Malformed("sidx references"))?;

    let mut offset = sidx_end
        .checked_add(first_offset)
        .ok_or(Mp4Error::Malformed("sidx first_offset"))?;
    let mut ticks = 0u64;
    let mut fragments = Vec::with_capacity(count);
    for e in entries[..count * 12].as_chunks::<12>().0 {
        let word = u32::from_be_bytes(e[0..4].try_into().expect("4-byte slice"));
        // The top bit is reference_type. 1 means the reference points at another `sidx`
        // rather than at media, which describes a hierarchical index this planner cannot
        // follow: the nested box is somewhere the caller has not fetched.
        if word >> 31 == 1 {
            return Err(Mp4Error::Malformed(
                "sidx references a nested index rather than media",
            ));
        }
        let len = u64::from(word & 0x7FFF_FFFF);
        if len == 0 {
            return Err(Mp4Error::Malformed("sidx reference is empty"));
        }
        let duration = u64::from(u32::from_be_bytes(
            e[4..8].try_into().expect("4-byte slice"),
        ));
        // The third word is SAP flags, which say nothing about where the bytes are.
        fragments.push(Fragment {
            offset,
            len,
            start_ms: to_ms(ticks, timescale),
        });
        offset = offset
            .checked_add(len)
            .ok_or(Mp4Error::Malformed("sidx references overflow the file"))?;
        ticks = ticks.saturating_add(duration);
    }
    if fragments.is_empty() {
        return Err(Mp4Error::Malformed("sidx describes no fragments"));
    }
    Ok(Index {
        timescale,
        fragments,
        total_ticks: ticks,
    })
}

// --- fragment rewriting ---

/// Copy one fed range through, patching the two fields that must change.
///
/// Returns the bytes to append and the next sequence number. Every byte of every `moof`
/// and `mdat` survives except `mfhd.sequence_number` and each `tfhd.track_ID`, both
/// written in place so no size changes and `trun.data_offset` stays valid.
fn rewrite_fragments(
    bytes: &[u8],
    track_id: u32,
    mut sequence: u32,
) -> Result<(Vec<u8>, u32), Mp4Error> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut pairs = 0usize;
    let mut expect_mdat = false;
    for span in spans(bytes)? {
        if expect_mdat {
            // Dropping a box from between a `moof` and its `mdat` would move the media
            // closer to the `moof` and invalidate every `trun.data_offset` in it, so a
            // range shaped like that is refused rather than silently corrupted.
            if &span.kind != b"mdat" {
                return Err(Mp4Error::Malformed(
                    "a box sits between a moof and its mdat",
                ));
            }
            out.extend_from_slice(&bytes[span.start..span.end]);
            expect_mdat = false;
            pairs += 1;
            continue;
        }
        match &span.kind {
            b"moof" => {
                let mut moof = bytes[span.start..span.end].to_vec();
                patch_moof(&mut moof, span.body - span.start, track_id, sequence)?;
                sequence = sequence.wrapping_add(1);
                out.extend_from_slice(&moof);
                expect_mdat = true;
            }
            b"mdat" => return Err(Mp4Error::Malformed("fragment has an mdat before its moof")),
            // Anything else is dropped. A stray `styp` mid-file is merely useless, but a
            // second `sidx` would describe the *input's* layout — offsets that mean
            // nothing in the merged file — and a player that trusted it would seek into
            // the wrong bytes.
            _ => {}
        }
    }
    if expect_mdat {
        return Err(Mp4Error::Malformed("moof has no mdat"));
    }
    if pairs == 0 {
        return Err(Mp4Error::Malformed("fragment range has no moof"));
    }
    Ok((out, sequence))
}

/// Write the shared sequence number and the output track id into a copied `moof`.
fn patch_moof(
    moof: &mut [u8],
    header_len: usize,
    track_id: u32,
    sequence: u32,
) -> Result<(), Mp4Error> {
    // Sites are collected before anything is written: the walk borrows the buffer, and
    // the checks below must all pass before the first byte changes.
    let mut sequence_at = None;
    let mut track_id_at = Vec::new();
    for span in spans(&moof[header_len..])? {
        let body = header_len + span.body;
        match &span.kind {
            // Full box: version+flags(4), then sequence_number.
            b"mfhd" => sequence_at = Some(body + 4),
            b"traf" => {
                let end = header_len + span.end;
                for inner in spans(&moof[body..end])? {
                    if &inner.kind != b"tfhd" {
                        continue;
                    }
                    let tfhd = body + inner.body;
                    let flags =
                        be_u32(moof, tfhd).ok_or(Mp4Error::Malformed("tfhd"))? & 0x00FF_FFFF;
                    // With `base-data-offset-present` the fragment states an absolute
                    // file position for its media, which stops being true the moment the
                    // fragment is written at a different offset. Every DASH writer uses
                    // `default-base-is-moof` or the implicit moof-relative base instead,
                    // so refusing is better than relocating a field on a file we have
                    // never seen.
                    if flags & TFHD_BASE_DATA_OFFSET != 0 {
                        return Err(Mp4Error::Malformed(
                            "tfhd carries an absolute base_data_offset",
                        ));
                    }
                    track_id_at.push(tfhd + 4);
                }
            }
            _ => {}
        }
    }
    let sequence_at = sequence_at.ok_or(Mp4Error::Malformed("moof has no mfhd"))?;
    if track_id_at.is_empty() {
        return Err(Mp4Error::Malformed("moof has no traf"));
    }

    write_u32(moof, sequence_at, sequence)?;
    for at in track_id_at {
        write_u32(moof, at, track_id)?;
    }
    Ok(())
}

fn write_u32(buf: &mut [u8], at: usize, value: u32) -> Result<(), Mp4Error> {
    buf.get_mut(at..at + 4)
        .ok_or(Mp4Error::Malformed("fragment truncated mid-field"))?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

// --- box spans ---

/// One box's extent within the buffer it was found in.
///
/// [`crate::mp4::children`] hands back body slices, which is all a parser needs. Merging
/// also has to copy whole boxes and patch fields at known positions inside them, and that
/// needs the offsets themselves.
#[derive(Debug, Clone)]
struct Span {
    kind: [u8; 4],
    /// Offset of the box header.
    start: usize,
    /// Offset of the box body, i.e. just past the header.
    body: usize,
    /// Offset just past the box.
    end: usize,
}

/// Split a buffer into the boxes it holds, requiring every one of them to fit.
fn spans(buf: &[u8]) -> Result<Vec<Span>, Mp4Error> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < buf.len() {
        let h = box_header(&buf[at..]).ok_or(Mp4Error::Malformed("box header"))?;
        let end = if h.size == 0 {
            buf.len()
        } else {
            at.checked_add(usize::try_from(h.size).map_err(|_| Mp4Error::Malformed("box size"))?)
                .filter(|&e| e <= buf.len())
                .ok_or(Mp4Error::Malformed("box runs past its parent"))?
        };
        let body = at + usize::from(h.header_len);
        if body > end {
            return Err(Mp4Error::Malformed("box is smaller than its header"));
        }
        out.push(Span {
            kind: h.kind,
            start: at,
            body,
            end,
        });
        at = end;
    }
    Ok(out)
}

/// The same walk over a *prefix* of a file, stopping at the first box the prefix does not
/// hold in full.
///
/// A head is a truncated file by construction, so a box running off the end of it is
/// expected rather than damage. The boxes that matter — `ftyp`, `moov`, `sidx` — all
/// precede the media, so 64 KiB reaches them and the fragment that follows is the one
/// left half-read.
fn head_spans(head: &[u8]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < head.len() {
        let Some(h) = box_header(&head[at..]) else {
            break;
        };
        let Some(end) = usize::try_from(h.size)
            .ok()
            .and_then(|size| at.checked_add(size))
            .filter(|&e| e <= head.len() && h.size != 0)
        else {
            break;
        };
        let body = at + usize::from(h.header_len);
        if body > end {
            break;
        }
        out.push(Span {
            kind: h.kind,
            start: at,
            body,
            end,
        });
        at = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fmp4::inspect::*;
    use crate::fmp4::{self, Sample, TrackConfig, TrackKind};

    /// A fragmented input, plus the facts about it a test wants to assert against.
    struct Fixture {
        bytes: Vec<u8>,
        /// Bytes from 0 up to and including the `sidx`: what a caller would pass in.
        head_len: usize,
        /// Absolute offset of the first fragment.
        first: u64,
        /// The size of each fragment, as the `sidx` declares it.
        sizes: Vec<u64>,
    }

    impl Fixture {
        fn head(&self) -> &[u8] {
            &self.bytes[..self.head_len]
        }

        fn range(&self, read: FragmentRead) -> &[u8] {
            &self.bytes[read.offset as usize..(read.offset + read.len) as usize]
        }
    }

    /// How a fixture is laid out, so each knob can be exercised on its own.
    #[derive(Debug, Clone, Copy, Default)]
    struct Layout {
        /// Version 0 stores the `sidx`'s times and `first_offset` in 32 bits, version 1
        /// in 64; both are in the wild.
        sidx_version: u8,
        /// Bytes between the end of the `sidx` and the first fragment, written as a
        /// `free` box the way a real file pads its index.
        first_offset: u64,
        /// Force `reference_type = 1` on the second entry: a hierarchical index.
        index_reference: bool,
    }

    /// Compose a synthetic fragmented MP4: `ftyp`, `moov`, `sidx`, then one
    /// `moof`+`mdat` pair per fragment whose sizes match the `sidx` exactly.
    ///
    /// The init segment and the fragments come from [`crate::fmp4`], which already writes
    /// precisely this shape — empty sample tables, a `mvex`/`trex`, `moof`-relative
    /// `trun` offsets — so the fixture exercises the merger rather than a second writer.
    fn fragmented(
        cfg: &TrackConfig,
        fragments: &[Vec<usize>],
        frame_duration: u32,
        fill: u8,
        layout: Layout,
    ) -> Fixture {
        let init = fmp4::write_init_segment(core::slice::from_ref(cfg));

        let mut bodies = Vec::new();
        let mut durations = Vec::new();
        let mut decode_time = 0u64;
        let mut byte = fill;
        for (i, sizes) in fragments.iter().enumerate() {
            let samples: Vec<Sample> = sizes
                .iter()
                .map(|&len| {
                    byte = byte.wrapping_add(17);
                    Sample {
                        // Distinct bytes per sample, so a fragment fed to the wrong track
                        // shows up as wrong content rather than as plausible noise.
                        data: vec![byte; len],
                        duration: frame_duration,
                        is_sync: i == 0,
                        cts_offset: 0,
                    }
                })
                .collect();
            bodies.push(fmp4::write_fragment(
                (i + 1) as u32,
                cfg.track_id,
                decode_time,
                &samples,
            ));
            decode_time += u64::from(frame_duration) * sizes.len() as u64;
            durations.push(frame_duration * sizes.len() as u32);
        }
        let sizes: Vec<u64> = bodies.iter().map(|b| b.len() as u64).collect();

        let sidx = sidx(cfg.timescale, &sizes, &durations, layout);
        let padding = if layout.first_offset == 0 {
            Vec::new()
        } else {
            assert!(
                layout.first_offset >= 8,
                "padding is a box, and boxes have headers"
            );
            bx(b"free", &vec![0u8; layout.first_offset as usize - 8])
        };

        let mut bytes = init;
        bytes.extend_from_slice(&sidx);
        let head_len = bytes.len();
        bytes.extend_from_slice(&padding);
        let first = bytes.len() as u64;
        for body in &bodies {
            bytes.extend_from_slice(body);
        }
        Fixture {
            bytes,
            head_len,
            first,
            sizes,
        }
    }

    /// A stream shaped the way Vimeo ships one: an init segment, then media segments
    /// that each carry their own `sidx` describing only themselves.
    ///
    /// Returns the whole stream and, for each piece, the bytes a caller would hold and
    /// the offset that piece begins at — an init segment and one head per media segment.
    fn segmented(
        cfg: &TrackConfig,
        fragments: &[Vec<usize>],
        frame_duration: u32,
        fill: u8,
    ) -> (Vec<u8>, Vec<(std::ops::Range<usize>, u64)>) {
        let mut bytes = fmp4::write_init_segment(core::slice::from_ref(cfg));
        // The init segment is a span in its own right: it holds the `moov` and no `sidx`.
        let mut spans = vec![(0..bytes.len(), 0u64)];

        let mut decode_time = 0u64;
        let mut byte = fill;
        for (i, sizes) in fragments.iter().enumerate() {
            let samples: Vec<Sample> = sizes
                .iter()
                .map(|&len| {
                    byte = byte.wrapping_add(17);
                    Sample {
                        data: vec![byte; len],
                        duration: frame_duration,
                        is_sync: i == 0,
                        cts_offset: 0,
                    }
                })
                .collect();
            let body = fmp4::write_fragment((i + 1) as u32, cfg.track_id, decode_time, &samples);
            decode_time += u64::from(frame_duration) * sizes.len() as u64;

            // Each segment indexes itself and nothing else, starting from zero — which
            // is the whole reason a single head cannot index the stream.
            let index = sidx(
                cfg.timescale,
                &[body.len() as u64],
                &[frame_duration * sizes.len() as u32],
                Layout::default(),
            );
            let start = bytes.len();
            bytes.extend_from_slice(&index);
            spans.push((start..bytes.len(), start as u64));
            bytes.extend_from_slice(&body);
        }
        (bytes, spans)
    }

    fn sidx(timescale: u32, sizes: &[u64], durations: &[u32], layout: Layout) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_be_bytes()); // reference_ID
        b.extend_from_slice(&timescale.to_be_bytes());
        if layout.sidx_version == 0 {
            b.extend_from_slice(&0u32.to_be_bytes()); // earliest_presentation_time
            b.extend_from_slice(&(layout.first_offset as u32).to_be_bytes());
        } else {
            b.extend_from_slice(&0u64.to_be_bytes());
            b.extend_from_slice(&layout.first_offset.to_be_bytes());
        }
        b.extend_from_slice(&0u16.to_be_bytes()); // reserved
        b.extend_from_slice(&(sizes.len() as u16).to_be_bytes());
        for (i, (&size, &duration)) in sizes.iter().zip(durations).enumerate() {
            let index = layout.index_reference && i == 1;
            let word = (u32::from(index) << 31) | size as u32;
            b.extend_from_slice(&word.to_be_bytes());
            b.extend_from_slice(&duration.to_be_bytes());
            // SAP flags: starts_with_SAP = 1, SAP_type = 1.
            b.extend_from_slice(&0x9000_0000u32.to_be_bytes());
        }
        full_bx(b"sidx", layout.sidx_version, 0, &b)
    }

    /// Both renditions call themselves track 1, exactly as YouTube's do.
    fn video_cfg() -> TrackConfig {
        TrackConfig {
            track_id: 1,
            timescale: 90_000,
            kind: TrackKind::Video {
                width: 640,
                height: 360,
                avcc: crate::h264::build_avcc(crate::fixtures::SPS, crate::fixtures::PPS),
            },
        }
    }

    fn audio_cfg() -> TrackConfig {
        fmp4::audio_track(1, 2, 44_100, 2)
    }

    /// Six fragments of two 30 fps frames each: 66.7 ms apiece.
    fn video_file(layout: Layout) -> Fixture {
        let fragments: Vec<Vec<usize>> = (0..6).map(|i| vec![300 + i, 120 + i]).collect();
        fragmented(&video_cfg(), &fragments, 3000, 0x10, layout)
    }

    /// Four fragments of four 1024-sample AAC frames each: 92.9 ms apiece.
    fn audio_file(layout: Layout) -> Fixture {
        let fragments: Vec<Vec<usize>> = (0..4)
            .map(|i| vec![40 + i, 41 + i, 42 + i, 43 + i])
            .collect();
        fragmented(&audio_cfg(), &fragments, 1024, 0x80, layout)
    }

    fn merger(video: &Fixture, audio: &Fixture) -> FragmentMerger {
        FragmentMerger::from_heads(video.head(), audio.head()).unwrap()
    }

    /// Feed every read in order and return the merger alongside the whole output.
    fn run(video: &Fixture, audio: &Fixture) -> (FragmentMerger, Vec<u8>) {
        let mut m = merger(video, audio);
        let mut out = Vec::new();
        for i in 0..m.reads().len() {
            let read = m.reads()[i];
            let src = match read.source {
                Source::Video => video,
                Source::Audio => audio,
            };
            out.extend(m.push(i, src.range(read)).unwrap());
        }
        (m, out)
    }

    /// Top-level `(type, offset, size)` triples of a whole file.
    fn top_level(file: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        spans(file)
            .expect("valid top-level boxes")
            .into_iter()
            .map(|s| (s.kind, s.start, s.end - s.start))
            .collect()
    }

    /// Every fragment of the output as `(moof bytes, mdat payload)`.
    fn fragments_of(buf: &[u8]) -> Vec<(&[u8], &[u8])> {
        let boxes = top_level(buf);
        let mut out = Vec::new();
        for (i, &(kind, at, size)) in boxes.iter().enumerate() {
            if &kind != b"moof" {
                continue;
            }
            let (mdat_kind, mdat_at, mdat_size) = boxes[i + 1];
            assert_eq!(
                &mdat_kind, b"mdat",
                "every moof must be followed by its mdat"
            );
            out.push((&buf[at..at + size], &buf[mdat_at + 8..mdat_at + mdat_size]));
        }
        out
    }

    fn track_id_of(moof: &[u8]) -> u32 {
        let tfhd = find_box(moof, "tfhd").expect("tfhd must exist");
        u32::from_be_bytes(tfhd[4..8].try_into().unwrap())
    }

    fn moov_of(file: &[u8]) -> &[u8] {
        let (_, at, size) = top_level(file)
            .into_iter()
            .find(|(k, _, _)| k == b"moov")
            .expect("file has a moov");
        &file[at..at + size]
    }

    /// The bodies of every box of the given type anywhere in the tree, shallowly: enough
    /// to count `trak`s and `trex`es in a `moov`.
    fn count_boxes(buf: &[u8], want: &[u8; 4]) -> usize {
        let mut n = 0;
        for span in spans(buf).expect("valid boxes") {
            if &span.kind == want {
                n += 1;
            }
            // Only containers are descended into; a payload that happened to spell
            // "trak" would not parse as a box tree anyway.
            if [b"moov", b"mvex", b"moof"].contains(&&span.kind) {
                n += count_boxes(&buf[span.body..span.end], want);
            }
        }
        n
    }

    // --- planning ---

    #[test]
    fn a_sidx_indexes_every_fragment_of_its_input() {
        for version in [0u8, 1] {
            let layout = Layout {
                sidx_version: version,
                ..Layout::default()
            };
            let (video, audio) = (video_file(layout), audio_file(layout));
            let m = merger(&video, &audio);
            for (source, fixture) in [(Source::Video, &video), (Source::Audio, &audio)] {
                let mine: Vec<FragmentRead> = m
                    .reads()
                    .iter()
                    .copied()
                    .filter(|r| r.source == source)
                    .collect();
                assert_eq!(
                    mine.iter().map(|r| r.len).collect::<Vec<u64>>(),
                    fixture.sizes,
                    "sidx v{version} {source:?} sizes"
                );
            }
        }
    }

    #[test]
    fn the_reads_tile_the_fragments_exactly_and_do_not_overlap() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let m = merger(&video, &audio);
        for (source, fixture) in [(Source::Video, &video), (Source::Audio, &audio)] {
            let mut at = fixture.first;
            for read in m.reads().iter().filter(|r| r.source == source) {
                assert_eq!(read.offset, at, "{source:?} reads must not gap or overlap");
                at += read.len;
            }
            assert_eq!(
                at,
                fixture.bytes.len() as u64,
                "{source:?} reads must reach the end of the file"
            );
        }
    }

    #[test]
    fn first_offset_moves_the_fragments_away_from_the_sidx() {
        let layout = Layout {
            first_offset: 64,
            ..Layout::default()
        };
        let (video, audio) = (video_file(layout), audio_file(layout));
        let m = merger(&video, &audio);
        let first = m
            .reads()
            .iter()
            .find(|r| r.source == Source::Video)
            .unwrap();
        assert_eq!(first.offset, video.head_len as u64 + 64);
        assert_eq!(first.offset, video.first);
        // And the bytes there really are a fragment rather than the padding.
        let (_, out) = run(&video, &audio);
        assert_eq!(&box_types(&out)[2..4], &["moof", "mdat"]);
    }

    #[test]
    fn each_read_is_exactly_one_moof_and_its_mdat() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let m = merger(&video, &audio);
        for &read in m.reads() {
            let src = match read.source {
                Source::Video => &video,
                Source::Audio => &audio,
            };
            assert_eq!(box_types(src.range(read)), vec!["moof", "mdat"]);
        }
    }

    #[test]
    fn the_reads_are_interleaved_rather_than_one_track_then_the_other() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let m = merger(&video, &audio);
        let order: Vec<Source> = m.reads().iter().map(|r| r.source).collect();
        // Six video fragments at 66.7 ms against four audio fragments at 92.9 ms.
        use Source::{Audio as A, Video as V};
        assert_eq!(order, vec![V, A, V, A, V, A, V, V, A, V]);
    }

    #[test]
    fn the_longer_track_gives_the_duration() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let m = merger(&video, &audio);
        // Twelve 30 fps frames is 400 ms; sixteen 1024-sample frames at 44.1 kHz is 371.
        assert_eq!(m.duration_ms(), 400);
    }

    // --- output structure ---

    #[test]
    fn output_is_an_init_segment_then_alternating_moof_and_mdat() {
        let (m, out) = run(
            &video_file(Layout::default()),
            &audio_file(Layout::default()),
        );
        let types = box_types(&out);
        assert_eq!(&types[..2], &["ftyp", "moov"]);
        let rest: Vec<&str> = types[2..].iter().map(String::as_str).collect();
        assert_eq!(rest.len(), 20, "ten fragments, a moof and an mdat each");
        for pair in rest.chunks(2) {
            assert_eq!(pair, ["moof", "mdat"]);
        }
        assert!(walk_and_validate_all_box_sizes(&out));
        assert!(m.is_complete());
    }

    #[test]
    fn the_combined_moov_describes_both_tracks() {
        let (_, out) = run(
            &video_file(Layout::default()),
            &audio_file(Layout::default()),
        );
        let moov = moov_of(&out);
        assert_eq!(count_boxes(moov, b"trak"), 2, "video and audio");
        assert_eq!(
            count_boxes(moov, b"trex"),
            2,
            "one per track, or no fragment plays"
        );
        assert!(contains_box(moov, "avcC"), "the video decoder config");
        assert!(contains_box(moov, "esds"), "the audio decoder config");
        assert!(contains_box(moov, "mvex"));
        assert!(contains_box(moov, "mehd"));
    }

    #[test]
    fn the_two_tracks_are_renumbered_one_and_two() {
        let (_, out) = run(
            &video_file(Layout::default()),
            &audio_file(Layout::default()),
        );
        let moov_body = &moov_of(&out)[8..];
        let ids: Vec<u32> = spans(moov_body)
            .unwrap()
            .into_iter()
            .filter(|s| &s.kind == b"trak")
            .map(|s| {
                let tkhd = find_box(&moov_body[s.start..s.end], "tkhd").unwrap();
                u32::from_be_bytes(tkhd[12..16].try_into().unwrap())
            })
            .collect();
        assert_eq!(ids, vec![1, 2], "video first, then the renumbered audio");

        let mvex = find_box(moov_body, "mvex").unwrap();
        let trex_ids: Vec<u32> = spans(mvex)
            .unwrap()
            .into_iter()
            .filter(|s| &s.kind == b"trex")
            .map(|s| u32::from_be_bytes(mvex[s.body + 4..s.body + 8].try_into().unwrap()))
            .collect();
        assert_eq!(
            trex_ids,
            vec![1, 2],
            "a trex must name the track it defaults"
        );
    }

    #[test]
    fn each_trexs_sample_defaults_are_the_inputs_own() {
        // A track whose fragments rely on trex defaults: zeroing them here would make
        // every sample play with the wrong duration.
        let mut video = video_file(Layout::default());
        let mut audio = audio_file(Layout::default());
        set_trex_default_duration(&mut video.bytes, 3000);
        set_trex_default_duration(&mut audio.bytes, 1024);

        let (_, out) = run(&video, &audio);
        let mvex = find_box(moov_of(&out), "mvex").unwrap();
        let defaults: Vec<u32> = spans(mvex)
            .unwrap()
            .into_iter()
            .filter(|s| &s.kind == b"trex")
            // version+flags(4), track_ID(4), default_sample_description_index(4), then
            // default_sample_duration.
            .map(|s| u32::from_be_bytes(mvex[s.body + 12..s.body + 16].try_into().unwrap()))
            .collect();
        assert_eq!(defaults, vec![3000, 1024]);
    }

    /// Overwrite `default_sample_duration` in a fixture's only `trex`.
    fn set_trex_default_duration(file: &mut [u8], duration: u32) {
        let at = file
            .windows(4)
            .position(|w| w == b"trex")
            .expect("the fixture has a trex");
        // The fourcc, then version+flags(4), track_ID(4), sample_description_index(4).
        let field = at + 4 + 12;
        file[field..field + 4].copy_from_slice(&duration.to_be_bytes());
    }

    #[test]
    fn audio_fragments_claim_track_two_in_the_output() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let (_, out) = run(&video, &audio);
        let ids: Vec<u32> = fragments_of(&out)
            .into_iter()
            .map(|(moof, _)| track_id_of(moof))
            .collect();
        assert_eq!(ids, vec![1, 2, 1, 2, 1, 2, 1, 1, 2, 1]);
        // The inputs both called themselves track 1, so this is a real renumbering.
        assert_eq!(
            track_id_of(video.range(merger(&video, &audio).reads()[0])),
            1
        );
        let audio_read = *merger(&video, &audio)
            .reads()
            .iter()
            .find(|r| r.source == Source::Audio)
            .unwrap();
        assert_eq!(track_id_of(audio.range(audio_read)), 1, "before merging");
    }

    #[test]
    fn sequence_numbers_count_up_from_one_across_both_tracks() {
        let (_, out) = run(
            &video_file(Layout::default()),
            &audio_file(Layout::default()),
        );
        let seqs: Vec<u32> = fragments_of(&out)
            .into_iter()
            .map(|(moof, _)| read_mfhd_sequence(moof))
            .collect();
        assert_eq!(seqs, (1..=10).collect::<Vec<u32>>());
    }

    #[test]
    fn patching_a_moof_does_not_change_its_size() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let m = merger(&video, &audio);
        let planned: Vec<usize> = m
            .reads()
            .iter()
            .map(|&r| {
                let src = match r.source {
                    Source::Video => &video,
                    Source::Audio => &audio,
                };
                top_level(src.range(r))[0].2
            })
            .collect();
        let (_, out) = run(&video, &audio);
        let emitted: Vec<usize> = fragments_of(&out)
            .into_iter()
            .map(|(m, _)| m.len())
            .collect();
        assert_eq!(
            emitted, planned,
            "trun.data_offset is moof-relative, so a resized moof breaks every sample"
        );
        // And the offset still points at the first byte of the mdat payload.
        for (moof, _) in fragments_of(&out) {
            assert_eq!(read_trun_data_offset(moof) as usize, moof.len() + 8);
        }
    }

    #[test]
    fn the_media_bytes_are_copied_through_untouched() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let m = merger(&video, &audio);
        let want: Vec<Vec<u8>> = m
            .reads()
            .iter()
            .map(|&r| {
                let src = match r.source {
                    Source::Video => &video,
                    Source::Audio => &audio,
                };
                let range = src.range(r);
                let (_, at, size) = top_level(range)[1];
                range[at + 8..at + size].to_vec()
            })
            .collect();
        let (_, out) = run(&video, &audio);
        let got: Vec<Vec<u8>> = fragments_of(&out)
            .into_iter()
            .map(|(_, data)| data.to_vec())
            .collect();
        assert_eq!(got, want, "no sample is ever rewritten");
    }

    #[test]
    fn only_the_first_push_carries_the_init_segment() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let mut m = merger(&video, &audio);
        let mut emitted = Vec::new();
        for i in 0..2 {
            let read = m.reads()[i];
            let src = match read.source {
                Source::Video => &video,
                Source::Audio => &audio,
            };
            emitted.push(m.push(i, src.range(read)).unwrap());
        }
        assert_eq!(&box_types(&emitted[0])[..2], &["ftyp", "moov"]);
        assert_eq!(box_types(&emitted[1]), vec!["moof", "mdat"]);
    }

    #[test]
    fn a_stray_styp_or_sidx_in_a_fed_range_is_dropped() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let mut m = merger(&video, &audio);
        let read = m.reads()[0];
        let mut bytes = bx(b"styp", b"iso5\0\0\x02\0iso5dash");
        bytes.extend_from_slice(&sidx(90_000, &[999], &[3000], Layout::default()));
        bytes.extend_from_slice(video.range(read));

        // A fed range is only checked against its planned length, so a longer one has to
        // be planned for the merger to accept it.
        m.reads[0].len = bytes.len() as u64;
        let out = m.push(0, &bytes).unwrap();
        assert_eq!(
            &box_types(&out)[2..],
            &["moof", "mdat"],
            "a second sidx would describe the input's layout, not the output's"
        );
    }

    // --- error paths ---

    #[test]
    fn an_input_without_a_sidx_cannot_be_indexed() {
        let video = video_file(Layout::default());
        let audio = audio_file(Layout::default());
        // The init segment alone: a real moov, no index.
        let init_only = &video.bytes[..video.head_len - 100];
        assert_eq!(
            FragmentMerger::from_heads(init_only, audio.head()).err(),
            Some(Mp4Error::Malformed(
                "fragmented input has no sidx to index its fragments"
            ))
        );
    }

    #[test]
    fn a_sidx_entry_pointing_at_another_index_is_refused() {
        let layout = Layout {
            index_reference: true,
            ..Layout::default()
        };
        let video = video_file(layout);
        assert_eq!(
            FragmentMerger::from_heads(video.head(), audio_file(Layout::default()).head()).err(),
            Some(Mp4Error::Malformed(
                "sidx references a nested index rather than media"
            ))
        );
    }

    #[test]
    fn feeding_a_read_out_of_order_is_refused_without_changing_state() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let mut m = merger(&video, &audio);
        let second = m.reads()[1];
        assert_eq!(
            m.push(1, audio.range(second)),
            Err(Mp4Error::OutOfOrder {
                expected: 0,
                got: 1
            })
        );
        let first = m.reads()[0];
        let out = m.push(0, video.range(first)).unwrap();
        assert_eq!(read_mfhd_sequence(&out[..]), 1, "the counter did not move");
    }

    #[test]
    fn a_read_of_the_wrong_length_is_refused() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let mut m = merger(&video, &audio);
        let first = m.reads()[0];
        let short = &video.range(first)[..first.len as usize - 1];
        assert_eq!(
            m.push(0, short),
            Err(Mp4Error::ChunkLength {
                expected: first.len,
                got: first.len - 1
            })
        );
    }

    #[test]
    fn pushing_past_the_last_read_is_out_of_order() {
        let (mut m, _) = run(
            &video_file(Layout::default()),
            &audio_file(Layout::default()),
        );
        assert_eq!(
            m.push(10, &[]),
            Err(Mp4Error::OutOfOrder {
                expected: 10,
                got: 10
            })
        );
    }

    #[test]
    fn an_audio_only_file_as_the_video_input_is_refused() {
        let audio = audio_file(Layout::default());
        assert_eq!(
            FragmentMerger::from_heads(audio.head(), audio.head()).err(),
            Some(Mp4Error::Malformed("video input has no video track"))
        );
    }

    #[test]
    fn a_video_only_file_as_the_audio_input_is_refused() {
        let video = video_file(Layout::default());
        assert_eq!(
            FragmentMerger::from_heads(video.head(), video.head()).err(),
            Some(Mp4Error::NoAudioTrack)
        );
    }

    #[test]
    fn a_fed_range_that_is_not_a_fragment_is_malformed_not_a_panic() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let mut m = merger(&video, &audio);
        let first = m.reads()[0];
        let junk = vec![0u8; first.len as usize];
        assert!(matches!(m.push(0, &junk), Err(Mp4Error::Malformed(_))));
    }

    #[test]
    fn a_truncated_head_is_malformed_not_a_panic() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        for cut in (0..video.head_len).step_by(7) {
            assert!(
                FragmentMerger::from_heads(&video.bytes[..cut], audio.head()).is_err(),
                "video head cut at {cut}"
            );
        }
        for cut in (0..audio.head_len).step_by(7) {
            assert!(
                FragmentMerger::from_heads(video.head(), &audio.bytes[..cut]).is_err(),
                "audio head cut at {cut}"
            );
        }
    }

    // --- determinism ---

    #[test]
    fn output_is_byte_identical_across_runs() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        assert_eq!(run(&video, &audio).1, run(&video, &audio).1);
        assert_eq!(
            video.bytes,
            video_file(Layout::default()).bytes,
            "the fixture itself must be deterministic"
        );
    }

    #[test]
    fn is_complete_only_once_the_last_read_has_been_fed() {
        let (video, audio) = (video_file(Layout::default()), audio_file(Layout::default()));
        let mut m = merger(&video, &audio);
        for i in 0..m.reads().len() {
            assert!(!m.is_complete(), "not complete before read {i}");
            let read = m.reads()[i];
            let src = match read.source {
                Source::Video => &video,
                Source::Audio => &audio,
            };
            m.push(i, src.range(read)).unwrap();
        }
        assert!(m.is_complete());
    }

    #[test]
    fn a_fragment_read_round_trips_through_serde() {
        let read = FragmentRead {
            source: Source::Audio,
            offset: 1 << 40,
            len: 12_345,
        };
        let json = serde_json::to_string(&read).unwrap();
        assert_eq!(serde_json::from_str::<FragmentRead>(&json).unwrap(), read);
    }
    /// An input that arrives as many self-indexing segments is indexed whole.
    ///
    /// A single `sidx` at the head is how YouTube and Bilibili ship a rendition, and
    /// reading only that one indexed one segment of a Vimeo stream out of twenty-one:
    /// a download that reported success having written three per cent of the video.
    /// Every segment's index has to be read, and each one's offsets placed where that
    /// segment actually sits.
    #[test]
    fn a_stream_of_self_indexing_segments_is_indexed_in_full() {
        let video_cfg = video_cfg();
        let audio_cfg = audio_cfg();
        let (video_bytes, video_spans) =
            segmented(&video_cfg, &[vec![600, 400], vec![500], vec![700]], 3000, 1);
        let (audio_bytes, audio_spans) =
            segmented(&audio_cfg, &[vec![300], vec![200], vec![250]], 1024, 90);

        fn borrow<'a>(
            bytes: &'a [u8],
            spans: &[(std::ops::Range<usize>, u64)],
        ) -> Vec<(&'a [u8], u64)> {
            spans
                .iter()
                .map(|(r, at)| (&bytes[r.clone()], *at))
                .collect()
        }
        let merger = FragmentMerger::from_segment_heads(
            &borrow(&video_bytes, &video_spans),
            &borrow(&audio_bytes, &audio_spans),
        )
        .expect("indexes every segment");

        // Three fragments per side, not one.
        assert_eq!(merger.reads().len(), 6, "every segment of both inputs");

        // Every read must name bytes that exist and start at a real `moof`.
        for read in merger.reads() {
            let bytes = if read.source == Source::Video {
                &video_bytes
            } else {
                &audio_bytes
            };
            let end = (read.offset + read.len) as usize;
            assert!(end <= bytes.len(), "read runs past the stream: {read:?}");
            let at = read.offset as usize;
            assert_eq!(
                &bytes[at + 4..at + 8],
                b"moof",
                "a read must start at a fragment"
            );
        }

        // And the whole thing merges: every read fed in order produces a file.
        let mut merger = merger;
        let mut out = Vec::new();
        for (i, read) in merger.reads().to_vec().iter().enumerate() {
            let bytes = if read.source == Source::Video {
                &video_bytes
            } else {
                &audio_bytes
            };
            let at = read.offset as usize;
            out.extend_from_slice(
                &merger
                    .push(i, &bytes[at..at + read.len as usize])
                    .expect("fragment is accepted"),
            );
        }
        assert!(out.starts_with(b"\0\0\0"), "output begins with a box");
        let kinds = box_types(&out);
        assert!(kinds.contains(&"ftyp".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"moov".to_string()), "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|k| *k == "moof").count(),
            6,
            "one fragment per read, from both inputs: {kinds:?}"
        );
    }
}
