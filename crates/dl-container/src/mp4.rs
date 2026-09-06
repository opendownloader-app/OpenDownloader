//! Progressive MP4 → audio-only fragmented MP4.
//!
//! A progressive MP4 (`.mp4`, `.m4a`, `.m4v`) describes every sample up front in the
//! `moov` box: which byte ranges of `mdat` belong to which track, how long each sample
//! lasts, and how the samples are grouped into chunks. Pulling the AAC track out of such
//! a file therefore needs no decoding and no scan of the media data — read the `moov`,
//! compute the byte ranges of the audio chunks, and copy those ranges through.
//!
//! The caller reads the source in pieces by byte offset and never holds the whole file,
//! so the extractor is built around that: [`AudioExtractor::from_moov`] turns the `moov`
//! into a [`ChunkPlan`] list, the caller fetches each range in turn, and
//! [`AudioExtractor::push_chunk`] turns each one into one fragment of output. Peak memory
//! is one chunk plus its fragment, regardless of how long the file is.
//!
//! The AAC payloads are copied byte for byte and the `AudioSpecificConfig` is carried
//! through unchanged, so the result is a genuine extraction rather than a transcode.

use serde::{Deserialize, Serialize};

use crate::fmp4::{self, Sample, TrackConfig, TrackKind};

/// The track id the audio track gets in the output. Matches what [`crate::remux`] uses
/// for its audio track, so the two paths produce interchangeable init segments.
const AUDIO_TRACK_ID: u32 = 2;

/// `objectTypeIndication` for MPEG-4 Audio in a `DecoderConfigDescriptor`.
const OTI_MPEG4_AUDIO: u8 = 0x40;

/// Upper bound on the bytes of one [`ChunkPlan`].
///
/// An `stsc` chunk is normally a fraction of a second of audio, but an audio-only file
/// written without interleaving can legitimately put the entire track in a single
/// chunk. A plan is the unit the caller reads into memory, so a chunk above this size
/// is split at sample boundaries into several plans — the memory bound must hold for
/// whatever a muxer chose to do, not only for the well-behaved case.
pub(crate) const MAX_PLAN_BYTES: u64 = 4 << 20;

/// The header of one ISO BMFF box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: [u8; 4],
    /// Total box size, header included. 0 means "extends to end of file".
    pub size: u64,
    /// 8 for a 32-bit size, 16 when the box uses `largesize`.
    pub header_len: u8,
}

/// Parse one box header from the start of `bytes`.
///
/// Needs 8 bytes, or 16 when the 32-bit size field is 1 (meaning a 64-bit `largesize`
/// follows). Returns `None` for truncated input and for sizes that cannot describe a
/// box at all — a size of 2..8 would have the box end inside its own header.
pub fn box_header(bytes: &[u8]) -> Option<BoxHeader> {
    let size32 = be_u32(bytes, 0)?;
    let kind: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
    match size32 {
        0 => Some(BoxHeader {
            kind,
            size: 0,
            header_len: 8,
        }),
        1 => {
            let size = be_u64(bytes, 8)?;
            (size >= 16).then_some(BoxHeader {
                kind,
                size,
                header_len: 16,
            })
        }
        2..=7 => None,
        n => Some(BoxHeader {
            kind,
            size: u64::from(n),
            header_len: 8,
        }),
    }
}

/// One byte range of the source file that the caller must fetch and feed back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkPlan {
    /// Absolute byte offset in the source file.
    pub offset: u64,
    pub len: u64,
    /// How many AAC frames the range holds.
    pub sample_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mp4Error {
    /// The `moov` could not be parsed. The message names the box or field at fault.
    Malformed(&'static str),
    /// No track with a `soun` handler exists.
    NoAudioTrack,
    /// A sound track exists but is not MPEG-4 AAC in an `mp4a` entry.
    UnsupportedCodec(String),
    /// Chunks must be fed strictly in [`AudioExtractor::chunks`] order.
    OutOfOrder { expected: usize, got: usize },
    /// The bytes fed for a chunk are not exactly the planned length.
    ChunkLength { expected: u64, got: u64 },
}

impl core::fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Mp4Error::Malformed(what) => write!(f, "malformed MP4: {what}"),
            Mp4Error::NoAudioTrack => f.write_str("MP4 has no audio track"),
            Mp4Error::UnsupportedCodec(codec) => {
                write!(f, "audio track is not MPEG-4 AAC ({codec})")
            }
            Mp4Error::OutOfOrder { expected, got } => {
                write!(f, "chunk {got} fed out of order; expected chunk {expected}")
            }
            Mp4Error::ChunkLength { expected, got } => {
                write!(f, "chunk is {got} bytes; planned {expected}")
            }
        }
    }
}

/// Sample durations as `stts` stores them: runs of `(count, delta)`.
///
/// Kept run-length encoded rather than expanded because a long, constant-rate track is
/// a single run, and expanding it would cost one `u32` per frame for nothing.
#[derive(Debug, Clone)]
pub(crate) struct DurationRuns {
    runs: Vec<(u32, u32)>,
    /// Cursor: index of the current run, and samples already consumed from it.
    run: usize,
    used: u32,
}

impl DurationRuns {
    pub(crate) fn new(runs: Vec<(u32, u32)>) -> Self {
        Self {
            runs,
            run: 0,
            used: 0,
        }
    }

    /// Total sample count described by every run.
    pub(crate) fn total_samples(&self) -> u64 {
        self.runs.iter().map(|&(count, _)| u64::from(count)).sum()
    }

    /// Sum of the first `n` sample durations.
    pub(crate) fn duration_of_first(&self, mut n: u64) -> u64 {
        let mut total = 0u64;
        for &(count, delta) in &self.runs {
            let take = n.min(u64::from(count));
            total += take * u64::from(delta);
            n -= take;
            if n == 0 {
                break;
            }
        }
        total
    }

    /// The next sample's duration, advancing the cursor.
    pub(crate) fn next(&mut self) -> Option<u32> {
        while let Some(&(count, delta)) = self.runs.get(self.run) {
            if self.used < count {
                self.used += 1;
                return Some(delta);
            }
            self.run += 1;
            self.used = 0;
        }
        None
    }
}

/// Sample sizes as `stsz` stores them.
#[derive(Debug, Clone)]
pub(crate) enum SampleSizes {
    Constant { size: u32, count: u32 },
    Table(Vec<u32>),
}

impl SampleSizes {
    pub(crate) fn count(&self) -> u32 {
        match self {
            SampleSizes::Constant { count, .. } => *count,
            SampleSizes::Table(t) => t.len() as u32,
        }
    }

    pub(crate) fn get(&self, index: u32) -> Option<u32> {
        match self {
            SampleSizes::Constant { size, count } => (index < *count).then_some(*size),
            SampleSizes::Table(t) => t.get(index as usize).copied(),
        }
    }
}

/// The half of a `trak` that says nothing about the codec: where the samples are, how
/// big they are and how long they last.
///
/// Shared with [`crate::mux`], which needs exactly this for a video track too. Keeping
/// it codec-independent is what lets one parser serve both.
pub(crate) struct SampleTables {
    pub(crate) timescale: u32,
    pub(crate) durations: DurationRuns,
    pub(crate) sizes: SampleSizes,
    /// `stsc` runs: `(first_chunk, samples_per_chunk)`, 1-based chunk numbers.
    pub(crate) stsc: Vec<(u32, u32)>,
    pub(crate) chunk_offsets: Vec<u64>,
}

/// Everything read from one audio `trak`.
struct AudioTrack {
    codec: Mp4aCodec,
    tables: SampleTables,
}

/// Pulls the AAC track out of a progressive MP4, one chunk at a time.
pub struct AudioExtractor {
    track: TrackConfig,
    chunks: Vec<ChunkPlan>,
    durations: DurationRuns,
    sizes: SampleSizes,
    duration: u64,
    /// Index into `chunks` of the next chunk expected.
    next_chunk: usize,
    /// Index of the first sample of `chunks[next_chunk]`.
    next_sample: u32,
    /// Decode time of that sample, in track timescale units.
    decode_time: u64,
    init_emitted: bool,
}

impl AudioExtractor {
    /// Parse a complete `moov` box (header included) and index its first AAC audio
    /// track.
    pub fn from_moov(moov: &[u8]) -> Result<Self, Mp4Error> {
        let body = moov_body(moov)?;

        let mut saw_sound_track = false;
        let mut other_codec: Option<String> = None;
        let mut chosen: Option<AudioTrack> = None;
        for (kind, trak_body) in children(body)? {
            if &kind != b"trak" {
                continue;
            }
            match parse_trak(trak_body)? {
                TrakOutcome::NotSound => {}
                TrakOutcome::OtherCodec(fourcc) => {
                    saw_sound_track = true;
                    other_codec.get_or_insert(fourcc);
                }
                TrakOutcome::Aac(track) => {
                    chosen = Some(*track);
                    break;
                }
            }
        }
        let track = match (chosen, saw_sound_track, other_codec) {
            (Some(t), _, _) => t,
            (None, true, Some(codec)) => return Err(Mp4Error::UnsupportedCodec(codec)),
            _ => return Err(Mp4Error::NoAudioTrack),
        };

        let chunks = plan_chunks(&track.tables)?;
        let sample_count = u64::from(track.tables.sizes.count());
        if track.tables.durations.total_samples() < sample_count {
            return Err(Mp4Error::Malformed(
                "stts describes fewer samples than stsz",
            ));
        }
        let duration = track.tables.durations.duration_of_first(sample_count);
        let cfg = track
            .codec
            .track_config(AUDIO_TRACK_ID, track.tables.timescale);

        Ok(Self {
            track: cfg,
            chunks,
            durations: track.tables.durations,
            sizes: track.tables.sizes,
            duration,
            next_chunk: 0,
            next_sample: 0,
            decode_time: 0,
            init_emitted: false,
        })
    }

    /// Absolute file ranges to read, in order. Each is one `stsc` chunk's worth of this
    /// track's samples, or a sample-aligned slice of one when a chunk is very large.
    pub fn chunks(&self) -> &[ChunkPlan] {
        &self.chunks
    }

    pub fn sample_rate(&self) -> u32 {
        match self.track.kind {
            TrackKind::Audio { sample_rate, .. } => sample_rate,
            TrackKind::Video { .. } => unreachable!("extractor only builds audio tracks"),
        }
    }

    pub fn channels(&self) -> u8 {
        match self.track.kind {
            TrackKind::Audio { channels, .. } => channels,
            TrackKind::Video { .. } => unreachable!("extractor only builds audio tracks"),
        }
    }

    /// The track's own timescale, normally its sample rate. Fragment decode times and
    /// [`Self::duration`] are in these units.
    pub fn timescale(&self) -> u32 {
        self.track.timescale
    }

    /// Total duration in track timescale units, summed from the sample table.
    ///
    /// Summed rather than read from `mdhd`, whose duration field is routinely 0 or
    /// all-ones in files written by a streaming muxer; the sample table is what the
    /// output actually contains.
    pub fn duration(&self) -> u64 {
        self.duration
    }

    /// Feed chunk `index`, which must be the next one in order and exactly
    /// `chunks()[index].len` bytes long.
    ///
    /// Returns the bytes to append to the output: the init segment followed by the
    /// first fragment on the first call, one fragment per call after that. A rejected
    /// call leaves the extractor untouched, so the caller can re-fetch and retry.
    pub fn push_chunk(&mut self, index: usize, bytes: &[u8]) -> Result<Vec<u8>, Mp4Error> {
        // A complete extractor has no next chunk; reporting `expected == chunks.len()`
        // tells the caller exactly that.
        let plan = match self.chunks.get(index) {
            Some(&plan) if index == self.next_chunk => plan,
            _ => {
                return Err(Mp4Error::OutOfOrder {
                    expected: self.next_chunk,
                    got: index,
                })
            }
        };
        if bytes.len() as u64 != plan.len {
            return Err(Mp4Error::ChunkLength {
                expected: plan.len,
                got: bytes.len() as u64,
            });
        }

        let mut samples = Vec::with_capacity(plan.sample_count as usize);
        let mut at = 0usize;
        for i in 0..plan.sample_count {
            let size = self
                .sizes
                .get(self.next_sample + i)
                .ok_or(Mp4Error::Malformed("chunk plan exceeds stsz"))?
                as usize;
            let duration = self
                .durations
                .next()
                .ok_or(Mp4Error::Malformed("chunk plan exceeds stts"))?;
            samples.push(Sample {
                data: bytes[at..at + size].to_vec(),
                duration,
                is_sync: true, // every AAC frame is independently decodable
                cts_offset: 0,
            });
            at += size;
        }

        let mut out = Vec::new();
        if !self.init_emitted {
            out.extend(fmp4::write_init_segment(core::slice::from_ref(&self.track)));
            self.init_emitted = true;
        }
        let seq = (index + 1) as u32;
        out.extend(fmp4::write_fragment(
            seq,
            AUDIO_TRACK_ID,
            self.decode_time,
            &samples,
        ));

        self.decode_time += samples.iter().map(|s| u64::from(s.duration)).sum::<u64>();
        self.next_sample += plan.sample_count;
        self.next_chunk += 1;
        Ok(out)
    }

    /// True once every planned chunk has been fed.
    pub fn is_complete(&self) -> bool {
        self.next_chunk == self.chunks.len()
    }
}

/// Expand `stsc` + `stco` into absolute byte ranges, one per chunk, splitting any chunk
/// larger than [`MAX_PLAN_BYTES`] at sample boundaries.
pub(crate) fn plan_chunks(track: &SampleTables) -> Result<Vec<ChunkPlan>, Mp4Error> {
    let chunk_count = track.chunk_offsets.len() as u32;
    let sample_count = track.sizes.count();
    if sample_count == 0 {
        return Err(Mp4Error::Malformed("track has no samples"));
    }
    if chunk_count == 0 {
        return Err(Mp4Error::Malformed("track has no chunks"));
    }
    if track.stsc.first().map(|r| r.0) != Some(1) {
        return Err(Mp4Error::Malformed("stsc must start at chunk 1"));
    }

    let mut plans = Vec::with_capacity(track.chunk_offsets.len());
    let mut sample = 0u32;
    for (run, &(first_chunk, per_chunk)) in track.stsc.iter().enumerate() {
        // Each run covers chunks up to the next run's first chunk; the last run
        // extends to the final chunk in `stco`.
        let end_chunk = track
            .stsc
            .get(run + 1)
            .map_or(chunk_count, |next| next.0.saturating_sub(1))
            .min(chunk_count);
        if first_chunk > end_chunk {
            return Err(Mp4Error::Malformed("stsc runs are not ascending"));
        }
        for chunk in first_chunk..=end_chunk {
            let offset = track.chunk_offsets[(chunk - 1) as usize];
            let mut at = offset;
            let mut plan = ChunkPlan {
                offset,
                len: 0,
                sample_count: 0,
            };
            for _ in 0..per_chunk {
                let size = u64::from(
                    track
                        .sizes
                        .get(sample)
                        .ok_or(Mp4Error::Malformed("stsc describes more samples than stsz"))?,
                );
                if plan.sample_count > 0 && plan.len + size > MAX_PLAN_BYTES {
                    plans.push(plan);
                    plan = ChunkPlan {
                        offset: at,
                        len: 0,
                        sample_count: 0,
                    };
                }
                plan.len += size;
                plan.sample_count += 1;
                at += size;
                sample += 1;
            }
            if plan.sample_count > 0 {
                plans.push(plan);
            }
        }
    }
    if sample != sample_count {
        return Err(Mp4Error::Malformed(
            "stsc describes fewer samples than stsz",
        ));
    }
    Ok(plans)
}

enum TrakOutcome {
    NotSound,
    OtherCodec(String),
    /// Boxed to keep the enum small; the track carries whole sample tables.
    Aac(Box<AudioTrack>),
}

fn parse_trak(body: &[u8]) -> Result<TrakOutcome, Mp4Error> {
    let Some(mdia) = child(body, b"mdia")? else {
        return Ok(TrakOutcome::NotSound);
    };
    if handler_type(mdia)? != Some(*b"soun") {
        return Ok(TrakOutcome::NotSound);
    }

    let (timescale, stbl) = media_tables(mdia)?;
    let (entry_kind, entry) = first_sample_entry(stbl)?;
    if &entry_kind != b"mp4a" {
        return Ok(TrakOutcome::OtherCodec(fourcc(&entry_kind)));
    }
    let codec = parse_mp4a(entry)?;
    let tables = parse_sample_tables(stbl, timescale)?;

    Ok(TrakOutcome::Aac(Box::new(AudioTrack { codec, tables })))
}

/// The body of a `moov` box, header stripped.
pub(crate) fn moov_body(moov: &[u8]) -> Result<&[u8], Mp4Error> {
    let header = box_header(moov).ok_or(Mp4Error::Malformed("moov header"))?;
    if &header.kind != b"moov" {
        return Err(Mp4Error::Malformed("expected a moov box"));
    }
    let end = if header.size == 0 {
        moov.len()
    } else {
        usize::try_from(header.size)
            .ok()
            .filter(|&s| s <= moov.len())
            .ok_or(Mp4Error::Malformed("moov truncated"))?
    };
    Ok(&moov[usize::from(header.header_len)..end])
}

/// The four-character handler type of a `mdia`, or `None` when it has no usable `hdlr`.
///
/// `soun` and `vide` are the only two that matter here; anything else is a subtitle,
/// timecode or metadata track that neither the extractor nor the muxer carries.
pub(crate) fn handler_type(mdia: &[u8]) -> Result<Option<[u8; 4]>, Mp4Error> {
    let Some(hdlr) = child(mdia, b"hdlr")? else {
        return Ok(None);
    };
    // Full box: version+flags(4), pre_defined(4), handler_type(4).
    Ok(hdlr.get(8..12).and_then(|h| h.try_into().ok()))
}

/// A `mdia`'s media timescale and the body of its `stbl`.
pub(crate) fn media_tables(mdia: &[u8]) -> Result<(u32, &[u8]), Mp4Error> {
    let mdhd = child(mdia, b"mdhd")?.ok_or(Mp4Error::Malformed("track has no mdhd"))?;
    let timescale = match mdhd.first() {
        Some(0) => be_u32(mdhd, 12),
        Some(1) => be_u32(mdhd, 20),
        _ => None,
    }
    .ok_or(Mp4Error::Malformed("mdhd"))?;
    if timescale == 0 {
        return Err(Mp4Error::Malformed("mdhd timescale is zero"));
    }
    let minf = child(mdia, b"minf")?.ok_or(Mp4Error::Malformed("track has no minf"))?;
    let stbl = child(minf, b"stbl")?.ok_or(Mp4Error::Malformed("track has no stbl"))?;
    Ok((timescale, stbl))
}

/// The type and body of the first `stsd` sample entry.
///
/// Only the first matters: a track that switches sample description mid-stream is not
/// something a copy-through can represent, and in practice never happens.
pub(crate) fn first_sample_entry(stbl: &[u8]) -> Result<Child<'_>, Mp4Error> {
    let stsd = child(stbl, b"stsd")?.ok_or(Mp4Error::Malformed("track has no stsd"))?;
    // Full box, then entry_count, then the entries.
    let entries = stsd.get(8..).ok_or(Mp4Error::Malformed("stsd"))?;
    children(entries)?
        .into_iter()
        .next()
        .ok_or(Mp4Error::Malformed("stsd has no entries"))
}

/// Read the codec-independent sample tables out of a `stbl`.
pub(crate) fn parse_sample_tables(stbl: &[u8], timescale: u32) -> Result<SampleTables, Mp4Error> {
    let stts = child(stbl, b"stts")?.ok_or(Mp4Error::Malformed("stbl has no stts"))?;
    let durations = DurationRuns::new(parse_pairs(stts, "stts")?);

    let sizes = parse_stsz(child(stbl, b"stsz")?.ok_or(Mp4Error::Malformed("stbl has no stsz"))?)?;

    let stsc = child(stbl, b"stsc")?.ok_or(Mp4Error::Malformed("stbl has no stsc"))?;
    let stsc = parse_stsc(stsc)?;

    let chunk_offsets = match (child(stbl, b"stco")?, child(stbl, b"co64")?) {
        (Some(stco), _) => parse_offsets(stco, false)?,
        (None, Some(co64)) => parse_offsets(co64, true)?,
        (None, None) => return Err(Mp4Error::Malformed("stbl has neither stco nor co64")),
    };

    Ok(SampleTables {
        timescale,
        durations,
        sizes,
        stsc,
        chunk_offsets,
    })
}

/// What the `mp4a` sample entry and its `esds` say about the stream.
pub(crate) struct Mp4aCodec {
    channels: u8,
    sample_rate: u32,
    object_type: u8,
    /// The `AudioSpecificConfig` exactly as the file carried it, if it had one.
    asc: Option<Vec<u8>>,
}

impl Mp4aCodec {
    /// The output track this codec describes.
    ///
    /// `audio_track` recomputes a two-byte ASC from the decoded parameters. For AAC-LC
    /// that reproduces the file's bytes exactly; for anything with an extension (SBR
    /// signalling, explicit frequencies) it would not, so the file's own bytes win.
    pub(crate) fn track_config(self, track_id: u32, timescale: u32) -> TrackConfig {
        let mut cfg =
            fmp4::audio_track(track_id, self.channels, self.sample_rate, self.object_type);
        cfg.timescale = timescale;
        if let (Some(asc), TrackKind::Audio { asc: out, .. }) = (self.asc, &mut cfg.kind) {
            *out = asc;
        }
        cfg
    }
}

/// Parse an `mp4a` `AudioSampleEntry` body (after the box header).
pub(crate) fn parse_mp4a(entry: &[u8]) -> Result<Mp4aCodec, Mp4Error> {
    // 6 reserved + data_reference_index(2), then version(2), revision(2), vendor(4),
    // channelcount(2), samplesize(2), pre_defined(2), reserved(2), samplerate(16.16).
    let version = be_u16(entry, 8).ok_or(Mp4Error::Malformed("mp4a entry"))?;
    let entry_channels = be_u16(entry, 16).ok_or(Mp4Error::Malformed("mp4a entry"))?;
    let entry_rate = be_u32(entry, 24).ok_or(Mp4Error::Malformed("mp4a entry"))? >> 16;
    // QuickTime sound sample description versions 1 and 2 insert extra fields between
    // the fixed part and the child boxes.
    let children_at = match version {
        0 => 28,
        1 => 28 + 16,
        2 => 28 + 36,
        _ => return Err(Mp4Error::Malformed("mp4a entry version")),
    };
    let child_boxes = entry.get(children_at..).unwrap_or(&[]);

    // The esds is normally a direct child, but QuickTime nests it inside `wave`.
    let esds = match child(child_boxes, b"esds")? {
        Some(e) => Some(e),
        None => match child(child_boxes, b"wave")? {
            Some(wave) => child(wave, b"esds")?,
            None => None,
        },
    };

    let mut codec = Mp4aCodec {
        channels: entry_channels.min(255) as u8,
        sample_rate: entry_rate,
        object_type: 2,
        asc: None,
    };
    let Some(esds) = esds else {
        // No decoder configuration at all: the sample entry is all we have. AAC-LC is
        // the only sensible reading of an mp4a entry with no esds.
        return Ok(codec);
    };
    let dsi = parse_esds(esds.get(4..).ok_or(Mp4Error::Malformed("esds"))?)?;
    if let Some(asc) = dsi {
        let decoded =
            parse_audio_specific_config(&asc).ok_or(Mp4Error::Malformed("AudioSpecificConfig"))?;
        codec.object_type = decoded.object_type;
        // Index 0xF means an explicit frequency; channel configuration 0 means the
        // layout is in a PCE inside the stream. Either way the sample entry's values
        // are the best description we have.
        if decoded.sample_rate != 0 {
            codec.sample_rate = decoded.sample_rate;
        }
        if decoded.channels != 0 {
            codec.channels = decoded.channels;
        }
        codec.asc = Some(asc);
    }
    Ok(codec)
}

/// Walk the descriptor tree inside an `esds` (after its version+flags) and return the
/// `DecoderSpecificInfo` payload, if present.
fn parse_esds(bytes: &[u8]) -> Result<Option<Vec<u8>>, Mp4Error> {
    let (tag, es) = descriptor(bytes, 0).ok_or(Mp4Error::Malformed("esds descriptor"))?;
    if tag != 0x03 {
        return Err(Mp4Error::Malformed(
            "esds does not start with an ES_Descriptor",
        ));
    }
    // ES_ID(2), then a flags byte whose bits gate optional fields.
    let flags = *es.get(2).ok_or(Mp4Error::Malformed("ES_Descriptor"))?;
    let mut at = 3;
    if flags & 0x80 != 0 {
        at += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        let url_len = *es.get(at).ok_or(Mp4Error::Malformed("ES_Descriptor URL"))?;
        at += 1 + usize::from(url_len);
    }
    if flags & 0x20 != 0 {
        at += 2; // OCR_ES_Id
    }

    // Sub-descriptors follow; find the DecoderConfigDescriptor among them.
    while at < es.len() {
        let (tag, body) = descriptor(es, at).ok_or(Mp4Error::Malformed("ES sub-descriptor"))?;
        if tag == 0x04 {
            let oti = *body
                .first()
                .ok_or(Mp4Error::Malformed("DecoderConfigDescriptor"))?;
            if oti != OTI_MPEG4_AUDIO {
                return Err(Mp4Error::UnsupportedCodec(format!(
                    "objectTypeIndication 0x{oti:02X}"
                )));
            }
            // objectTypeIndication(1), streamType(1), bufferSizeDB(3), maxBitrate(4),
            // avgBitrate(4), then optional sub-descriptors.
            let mut inner = 13;
            while inner < body.len() {
                let (tag, dsi) = descriptor(body, inner)
                    .ok_or(Mp4Error::Malformed("DecoderConfigDescriptor child"))?;
                if tag == 0x05 {
                    return Ok(Some(dsi.to_vec()));
                }
                inner += descriptor_len(body, inner);
            }
            return Ok(None);
        }
        at += descriptor_len(es, at);
    }
    Err(Mp4Error::Malformed("esds has no DecoderConfigDescriptor"))
}

/// Read a descriptor's tag and payload at `at`.
///
/// The length is a base-128 varint: up to four bytes, each carrying 7 bits, with the
/// high bit meaning "more follows". Writers commonly emit the four-byte form for small
/// values, so all four must be accepted.
fn descriptor(bytes: &[u8], at: usize) -> Option<(u8, &[u8])> {
    let tag = *bytes.get(at)?;
    let mut len = 0usize;
    let mut i = at + 1;
    for _ in 0..4 {
        let b = *bytes.get(i)?;
        i += 1;
        len = (len << 7) | usize::from(b & 0x7F);
        if b & 0x80 == 0 {
            break;
        }
    }
    Some((tag, bytes.get(i..i + len)?))
}

/// Total bytes occupied by the descriptor at `at`, header included. Only called after
/// `descriptor` succeeded at the same position, so the tail is known to be well formed.
fn descriptor_len(bytes: &[u8], at: usize) -> usize {
    let mut len = 0usize;
    let mut i = at + 1;
    for _ in 0..4 {
        let b = bytes[i];
        i += 1;
        len = (len << 7) | usize::from(b & 0x7F);
        if b & 0x80 == 0 {
            break;
        }
    }
    (i - at) + len
}

struct DecodedAsc {
    object_type: u8,
    /// 0 when the config used an explicit frequency or an index we do not know.
    sample_rate: u32,
    /// 0 when the channel layout is signalled in-band.
    channels: u8,
}

/// Decode the leading fields of an `AudioSpecificConfig`.
fn parse_audio_specific_config(asc: &[u8]) -> Option<DecodedAsc> {
    let mut bits = Bits { bytes: asc, pos: 0 };
    let mut object_type = bits.read(5)? as u8;
    if object_type == 31 {
        object_type = (bits.read(6)? + 32) as u8;
    }
    let freq_index = bits.read(4)? as usize;
    let sample_rate = if freq_index == 0xF {
        bits.read(24)?
    } else {
        crate::aac::SAMPLE_RATES
            .get(freq_index)
            .copied()
            .unwrap_or(0)
    };
    let channels = bits.read(4)? as u8;
    Some(DecodedAsc {
        object_type,
        sample_rate,
        channels,
    })
}

/// MSB-first bit reader, just enough for the ASC header.
struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.bytes.get(self.pos / 8)?;
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            v = (v << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(v)
    }
}

/// `stts`-style full box: entry_count then `(u32, u32)` pairs.
pub(crate) fn parse_pairs(
    full_box: &[u8],
    name: &'static str,
) -> Result<Vec<(u32, u32)>, Mp4Error> {
    let count = be_u32(full_box, 4).ok_or(Mp4Error::Malformed(name))? as usize;
    let table = full_box
        .get(8..)
        .filter(|t| t.len() >= count * 8)
        .ok_or(Mp4Error::Malformed(name))?;
    Ok(table[..count * 8]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|e| (be_u32_exact(&e[0..4]), be_u32_exact(&e[4..8])))
        .collect())
}

fn parse_stsz(full_box: &[u8]) -> Result<SampleSizes, Mp4Error> {
    let size = be_u32(full_box, 4).ok_or(Mp4Error::Malformed("stsz"))?;
    let count = be_u32(full_box, 8).ok_or(Mp4Error::Malformed("stsz"))?;
    if size != 0 {
        return Ok(SampleSizes::Constant { size, count });
    }
    let table = full_box
        .get(12..)
        .filter(|t| t.len() >= count as usize * 4)
        .ok_or(Mp4Error::Malformed("stsz table"))?;
    Ok(SampleSizes::Table(
        table[..count as usize * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|e| u32::from_be_bytes(*e))
            .collect(),
    ))
}

fn parse_stsc(full_box: &[u8]) -> Result<Vec<(u32, u32)>, Mp4Error> {
    let count = be_u32(full_box, 4).ok_or(Mp4Error::Malformed("stsc"))? as usize;
    let table = full_box
        .get(8..)
        .filter(|t| t.len() >= count * 12)
        .ok_or(Mp4Error::Malformed("stsc table"))?;
    let runs: Vec<(u32, u32)> = table[..count * 12]
        .as_chunks::<12>()
        .0
        .iter()
        // The third field, sample_description_index, is ignored: see `parse_trak`.
        .map(|e| (be_u32_exact(&e[0..4]), be_u32_exact(&e[4..8])))
        .collect();
    if runs.is_empty() {
        return Err(Mp4Error::Malformed("stsc is empty"));
    }
    Ok(runs)
}

fn parse_offsets(full_box: &[u8], wide: bool) -> Result<Vec<u64>, Mp4Error> {
    let name = if wide { "co64" } else { "stco" };
    let count = be_u32(full_box, 4).ok_or(Mp4Error::Malformed(name))? as usize;
    let width = if wide { 8 } else { 4 };
    let table = full_box
        .get(8..)
        .filter(|t| t.len() >= count * width)
        .ok_or(Mp4Error::Malformed(name))?;
    Ok(table[..count * width]
        .chunks_exact(width)
        .map(|e| {
            if wide {
                u64::from_be_bytes(e.try_into().expect("8-byte chunk"))
            } else {
                u64::from(be_u32_exact(e))
            }
        })
        .collect())
}

/// One child box: its type and its body (header stripped).
pub(crate) type Child<'a> = ([u8; 4], &'a [u8]);

/// Split a container body into `(type, body)` children.
///
/// A child whose size runs past the parent is an error rather than a truncation: the
/// parent's size came from the same writer, so the two disagreeing means the data is
/// damaged, and the sample tables that follow would be wrong.
pub(crate) fn children(body: &[u8]) -> Result<Vec<Child<'_>>, Mp4Error> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < body.len() {
        let h = box_header(&body[at..]).ok_or(Mp4Error::Malformed("child box header"))?;
        let end = if h.size == 0 {
            body.len()
        } else {
            at.checked_add(usize::try_from(h.size).map_err(|_| Mp4Error::Malformed("box size"))?)
                .filter(|&e| e <= body.len())
                .ok_or(Mp4Error::Malformed("child box runs past its parent"))?
        };
        out.push((h.kind, &body[at + usize::from(h.header_len)..end]));
        at = end;
    }
    Ok(out)
}

/// The body of the first child of the given type.
pub(crate) fn child<'a>(body: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>, Mp4Error> {
    Ok(children(body)?
        .into_iter()
        .find(|(k, _)| k == kind)
        .map(|(_, b)| b))
}

pub(crate) fn fourcc(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

pub(crate) fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_be_bytes([s[0], s[1]]))
}

pub(crate) fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(be_u32_exact)
}

pub(crate) fn be_u64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_be_bytes(s.try_into().expect("8-byte slice")))
}

/// `u32` from a slice already known to be 4 bytes.
fn be_u32_exact(s: &[u8]) -> u32 {
    u32::from_be_bytes(s.try_into().expect("4-byte slice"))
}

#[cfg(any(test, feature = "fixtures"))]
pub mod builder {
    //! Synthetic progressive MP4s for tests and the test media server.
    //!
    //! Deliberately independent of `fmp4`'s writer: a builder that shared the extractor's
    //! own box helpers would let a matching bug on both sides cancel out.

    use crate::aac;
    use crate::fixtures::{PPS, SPS};
    use crate::h264;

    /// Movie timescale for `mvhd`, and what `tkhd` durations are expressed in.
    const MOVIE_TIMESCALE: u32 = 1000;
    /// Video track timescale and per-frame duration (30 fps in 90 kHz).
    const VIDEO_TIMESCALE: u32 = 90_000;
    const VIDEO_FRAME_DURATION: u32 = 3000;

    /// Layout choices that change how the same audio is described, so the parser's
    /// handling of each `stbl` variant can be exercised separately.
    #[derive(Debug, Clone, Copy)]
    pub struct Options {
        pub sample_rate: u32,
        pub channels: u8,
        pub samples_per_chunk: u32,
        /// Add a video track whose chunks alternate with the audio chunks in `mdat`,
        /// so the audio ranges are not contiguous.
        pub interleave_video: bool,
        /// Write `co64` (64-bit chunk offsets) instead of `stco`.
        pub co64: bool,
        /// Write a constant `sample_size` in `stsz` instead of a per-sample table.
        /// Only valid when every frame has the same length.
        pub constant_sample_size: bool,
    }

    /// How the video track is described. The defaults reproduce the interleaved dummy
    /// track [`progressive_mp4`] has always written, so exercising a new knob cannot
    /// change any existing fixture's bytes.
    #[derive(Debug, Clone)]
    pub struct VideoOptions {
        pub samples_per_chunk: u32,
        /// One composition offset per sample. `None` writes no `ctts` at all, which is
        /// what a stream without B-frames looks like.
        pub cts_offsets: Option<Vec<i32>>,
        /// 1-based sample numbers listed in `stss`. `None` writes no `stss`, which
        /// means every sample is a sync sample.
        pub sync_samples: Option<Vec<u32>>,
        pub co64: bool,
        /// The `stsd` sample entry's four-character code. Anything but `avc1`/`avc3`
        /// gives a track the muxer must reject.
        pub codec: [u8; 4],
    }

    impl Default for VideoOptions {
        fn default() -> Self {
            Self {
                samples_per_chunk: 1,
                cts_offsets: None,
                sync_samples: None,
                co64: false,
                codec: *b"avc1",
            }
        }
    }

    /// A progressive MP4: `ftyp`, `moov`, `mdat`, in that order.
    ///
    /// The `moov` holds one audio track whose samples are `frames`, grouped
    /// `samples_per_chunk` at a time. With `interleave_video`, a video track's chunks
    /// alternate with the audio chunks inside `mdat`, which is what proves an extractor
    /// follows `stco` rather than assuming the media data is all audio.
    pub fn progressive_mp4(
        frames: &[Vec<u8>],
        sample_rate: u32,
        channels: u8,
        samples_per_chunk: u32,
        interleave_video: bool,
    ) -> Vec<u8> {
        progressive_mp4_with(
            frames,
            Options {
                sample_rate,
                channels,
                samples_per_chunk,
                interleave_video,
                co64: false,
                constant_sample_size: false,
            },
        )
    }

    /// [`progressive_mp4`] with every layout knob exposed.
    pub fn progressive_mp4_with(frames: &[Vec<u8>], opts: Options) -> Vec<u8> {
        assert!(opts.samples_per_chunk > 0, "chunks must hold samples");
        if opts.constant_sample_size {
            assert!(
                frames.windows(2).all(|w| w[0].len() == w[1].len()),
                "constant sample size needs equal frames"
            );
        }

        let audio_chunks: Vec<&[Vec<u8>]> =
            frames.chunks(opts.samples_per_chunk as usize).collect();
        // One dummy video sample per audio chunk, AVCC framed (4-byte length prefix).
        let video_samples: Vec<Vec<u8>> = if opts.interleave_video {
            (0..audio_chunks.len())
                .map(|i| {
                    let nal = [0x65, i as u8, 0xAA, 0x55];
                    let mut s = (nal.len() as u32).to_be_bytes().to_vec();
                    s.extend_from_slice(&nal);
                    s
                })
                .collect()
        } else {
            Vec::new()
        };

        // mdat layout: for each audio chunk, its video sample (if any) then the audio.
        let mut mdat_payload = Vec::new();
        let mut audio_offsets = Vec::with_capacity(audio_chunks.len());
        let mut video_offsets = Vec::with_capacity(video_samples.len());
        for (i, chunk) in audio_chunks.iter().enumerate() {
            if let Some(v) = video_samples.get(i) {
                video_offsets.push(mdat_payload.len() as u64);
                mdat_payload.extend_from_slice(v);
            }
            audio_offsets.push(mdat_payload.len() as u64);
            for f in *chunk {
                mdat_payload.extend_from_slice(f);
            }
        }

        let ftyp = ftyp();

        // Offsets are relative to the file, and the file starts with ftyp and moov.
        // moov's size does not depend on the offset values, only on how many there are,
        // so build it once with placeholders to learn its size, then for real.
        let build_moov = |shift: u64| {
            let audio = audio_offsets.iter().map(|o| o + shift).collect::<Vec<_>>();
            let video = video_offsets.iter().map(|o| o + shift).collect::<Vec<_>>();
            moov(frames, &audio, &video_samples, &video, opts)
        };
        let moov_len = build_moov(0).len() as u64;
        let moov = build_moov(ftyp.len() as u64 + moov_len + 8);
        assert_eq!(moov.len() as u64, moov_len);

        let mut out = ftyp;
        out.extend_from_slice(&moov);
        out.extend_from_slice(&bx(b"mdat", &mdat_payload));
        out
    }

    fn ftyp() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"isom");
        b.extend_from_slice(&512u32.to_be_bytes());
        for brand in [b"isom", b"iso2", b"mp41"] {
            b.extend_from_slice(brand);
        }
        bx(b"ftyp", &b)
    }

    /// A progressive MP4 holding one video track and nothing else — the video half of
    /// what a split-format site serves.
    ///
    /// `samples` are AVCC-framed access units; the builder only lays them out, so a
    /// test that cares about byte-for-byte pass-through can use arbitrary bytes.
    pub fn progressive_mp4_video_only(samples: &[Vec<u8>], opts: VideoOptions) -> Vec<u8> {
        assert!(opts.samples_per_chunk > 0, "chunks must hold samples");

        let mut mdat_payload = Vec::new();
        let mut offsets = Vec::new();
        for chunk in samples.chunks(opts.samples_per_chunk as usize) {
            offsets.push(mdat_payload.len() as u64);
            for s in chunk {
                mdat_payload.extend_from_slice(s);
            }
        }

        let duration = samples.len() as u64 * u64::from(VIDEO_FRAME_DURATION);
        let movie_ms = (duration * u64::from(MOVIE_TIMESCALE) / u64::from(VIDEO_TIMESCALE)) as u32;

        let ftyp = ftyp();
        // Same two-pass trick as `progressive_mp4_with`: the moov's size depends on how
        // many chunk offsets there are, not on their values.
        let build_moov = |shift: u64| {
            let shifted: Vec<u64> = offsets.iter().map(|o| o + shift).collect();
            bx(
                b"moov",
                &concat(&[
                    mvhd(movie_ms, 2),
                    video_trak(samples, &shifted, movie_ms, &opts),
                ]),
            )
        };
        let moov_len = build_moov(0).len() as u64;
        let moov = build_moov(ftyp.len() as u64 + moov_len + 8);
        assert_eq!(moov.len() as u64, moov_len);

        let mut out = ftyp;
        out.extend_from_slice(&moov);
        out.extend_from_slice(&bx(b"mdat", &mdat_payload));
        out
    }

    fn moov(
        frames: &[Vec<u8>],
        audio_offsets: &[u64],
        video_samples: &[Vec<u8>],
        video_offsets: &[u64],
        opts: Options,
    ) -> Vec<u8> {
        let audio_duration = frames.len() as u64 * u64::from(aac::SAMPLES_PER_FRAME);
        let audio_ms = audio_duration * u64::from(MOVIE_TIMESCALE) / u64::from(opts.sample_rate);
        let video_duration = video_samples.len() as u64 * u64::from(VIDEO_FRAME_DURATION);
        let video_ms = video_duration * u64::from(MOVIE_TIMESCALE) / u64::from(VIDEO_TIMESCALE);
        let movie_ms = audio_ms.max(video_ms);

        let mut parts = vec![mvhd(
            movie_ms as u32,
            if video_samples.is_empty() { 2 } else { 3 },
        )];
        if !video_samples.is_empty() {
            parts.push(video_trak(
                video_samples,
                video_offsets,
                video_ms as u32,
                &VideoOptions {
                    co64: opts.co64,
                    ..VideoOptions::default()
                },
            ));
        }
        parts.push(audio_trak(
            frames,
            audio_offsets,
            audio_duration as u32,
            audio_ms as u32,
            opts,
        ));
        bx(b"moov", &concat(&parts))
    }

    fn audio_trak(
        frames: &[Vec<u8>],
        chunk_offsets: &[u64],
        duration: u32,
        movie_duration: u32,
        opts: Options,
    ) -> Vec<u8> {
        let asc = aac::audio_specific_config(2, opts.sample_rate, opts.channels);
        let mp4a = {
            let mut e = Vec::new();
            e.extend_from_slice(&[0u8; 6]);
            e.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
            e.extend_from_slice(&[0u8; 8]); // version, revision, vendor
            e.extend_from_slice(&u16::from(opts.channels).to_be_bytes());
            e.extend_from_slice(&16u16.to_be_bytes()); // samplesize
            e.extend_from_slice(&[0u8; 4]); // pre_defined, reserved
            e.extend_from_slice(&(opts.sample_rate << 16).to_be_bytes());
            e.extend_from_slice(&esds(&asc));
            bx(b"mp4a", &e)
        };
        let sizes: Vec<u32> = frames.iter().map(|f| f.len() as u32).collect();
        let stsz = if opts.constant_sample_size {
            let mut b = sizes.first().copied().unwrap_or(0).to_be_bytes().to_vec();
            b.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
            full_bx(b"stsz", 0, 0, &b)
        } else {
            stsz_table(&sizes)
        };
        let stbl = concat(&[
            stsd(&mp4a),
            stts(&[(frames.len() as u32, aac::SAMPLES_PER_FRAME)]),
            stsc(frames.len() as u32, opts.samples_per_chunk),
            stsz,
            chunk_offset_box(chunk_offsets, opts.co64),
        ]);
        let minf = concat(&[
            full_bx(b"smhd", 0, 0, &[0, 0, 0, 0]),
            dinf(),
            bx(b"stbl", &stbl),
        ]);
        let mdia = concat(&[
            mdhd(opts.sample_rate, duration),
            hdlr(b"soun", b"SoundHandler\0"),
            bx(b"minf", &minf),
        ]);
        bx(
            b"trak",
            &concat(&[tkhd(2, movie_duration, true, 0, 0), bx(b"mdia", &mdia)]),
        )
    }

    fn video_trak(
        samples: &[Vec<u8>],
        offsets: &[u64],
        movie_duration: u32,
        opts: &VideoOptions,
    ) -> Vec<u8> {
        let (width, height) = h264::sps_resolution(SPS).unwrap_or((640, 360));
        let avc1 = {
            let mut e = Vec::new();
            e.extend_from_slice(&[0u8; 6]);
            e.extend_from_slice(&1u16.to_be_bytes());
            e.extend_from_slice(&[0u8; 16]);
            e.extend_from_slice(&(width as u16).to_be_bytes());
            e.extend_from_slice(&(height as u16).to_be_bytes());
            e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
            e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
            e.extend_from_slice(&0u32.to_be_bytes());
            e.extend_from_slice(&1u16.to_be_bytes());
            e.extend_from_slice(&[0u8; 32]);
            e.extend_from_slice(&0x0018u16.to_be_bytes());
            e.extend_from_slice(&0xFFFFu16.to_be_bytes());
            e.extend_from_slice(&bx(b"avcC", &h264::build_avcc(SPS, PPS)));
            bx(&opts.codec, &e)
        };
        let sizes: Vec<u32> = samples.iter().map(|s| s.len() as u32).collect();
        let mut stbl_parts = vec![
            stsd(&avc1),
            stts(&[(samples.len() as u32, VIDEO_FRAME_DURATION)]),
        ];
        if let Some(offsets) = &opts.cts_offsets {
            stbl_parts.push(ctts(offsets));
        }
        stbl_parts.push(stsc(samples.len() as u32, opts.samples_per_chunk));
        stbl_parts.push(stsz_table(&sizes));
        if let Some(sync) = &opts.sync_samples {
            stbl_parts.push(stss(sync));
        }
        stbl_parts.push(chunk_offset_box(offsets, opts.co64));
        let stbl = concat(&stbl_parts);
        let minf = concat(&[
            full_bx(b"vmhd", 0, 1, &[0u8; 8]),
            dinf(),
            bx(b"stbl", &stbl),
        ]);
        let duration = samples.len() as u32 * VIDEO_FRAME_DURATION;
        let mdia = concat(&[
            mdhd(VIDEO_TIMESCALE, duration),
            hdlr(b"vide", b"VideoHandler\0"),
            bx(b"minf", &minf),
        ]);
        bx(
            b"trak",
            &concat(&[
                tkhd(1, movie_duration, false, width, height),
                bx(b"mdia", &mdia),
            ]),
        )
    }

    fn mvhd(duration: u32, next_track_id: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_be_bytes()); // creation_time — fixed for determinism
        b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        b.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
        b.extend_from_slice(&duration.to_be_bytes());
        b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
        b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        b.extend_from_slice(&[0u8; 10]); // reserved
        unity_matrix(&mut b);
        b.extend_from_slice(&[0u8; 24]); // pre_defined
        b.extend_from_slice(&next_track_id.to_be_bytes());
        full_bx(b"mvhd", 0, 0, &b)
    }

    fn tkhd(track_id: u32, duration: u32, audio: bool, width: u32, height: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&track_id.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&duration.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0u16.to_be_bytes()); // layer
        b.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
        b.extend_from_slice(&(if audio { 0x0100u16 } else { 0 }).to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        unity_matrix(&mut b);
        b.extend_from_slice(&(width << 16).to_be_bytes());
        b.extend_from_slice(&(height << 16).to_be_bytes());
        full_bx(b"tkhd", 0, 7, &b)
    }

    fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&timescale.to_be_bytes());
        b.extend_from_slice(&duration.to_be_bytes());
        b.extend_from_slice(&0x55C4u16.to_be_bytes()); // "und"
        b.extend_from_slice(&0u16.to_be_bytes());
        full_bx(b"mdhd", 0, 0, &b)
    }

    fn hdlr(handler: &[u8; 4], name: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(handler);
        b.extend_from_slice(&[0u8; 12]);
        b.extend_from_slice(name);
        full_bx(b"hdlr", 0, 0, &b)
    }

    fn dinf() -> Vec<u8> {
        let url = full_bx(b"url ", 0, 1, &[]);
        let mut dref = 1u32.to_be_bytes().to_vec();
        dref.extend_from_slice(&url);
        bx(b"dinf", &full_bx(b"dref", 0, 0, &dref))
    }

    fn stsd(entry: &[u8]) -> Vec<u8> {
        let mut b = 1u32.to_be_bytes().to_vec();
        b.extend_from_slice(entry);
        full_bx(b"stsd", 0, 0, &b)
    }

    /// The `esds` as a mainstream encoder writes it, including the 4-byte varint
    /// descriptor lengths that a strict-minded parser might not expect.
    fn esds(asc: &[u8]) -> Vec<u8> {
        let dsi = descriptor(0x05, asc);
        let mut dcd = vec![0x40, 0x15, 0x00, 0x00, 0x00];
        dcd.extend_from_slice(&0u32.to_be_bytes());
        dcd.extend_from_slice(&0u32.to_be_bytes());
        dcd.extend_from_slice(&dsi);
        let dcd = descriptor(0x04, &dcd);
        let mut es = vec![0x00, 0x02, 0x00]; // ES_ID 2, no optional fields
        es.extend_from_slice(&dcd);
        es.extend_from_slice(&descriptor(0x06, &[0x02]));
        full_bx(b"esds", 0, 0, &descriptor(0x03, &es))
    }

    /// A descriptor with its length in the expanded four-byte form.
    fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
        let len = body.len() as u32;
        let mut d = vec![
            tag,
            0x80 | ((len >> 21) & 0x7F) as u8,
            0x80 | ((len >> 14) & 0x7F) as u8,
            0x80 | ((len >> 7) & 0x7F) as u8,
            (len & 0x7F) as u8,
        ];
        d.extend_from_slice(body);
        d
    }

    fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
        let mut b = (runs.len() as u32).to_be_bytes().to_vec();
        for &(count, delta) in runs {
            b.extend_from_slice(&count.to_be_bytes());
            b.extend_from_slice(&delta.to_be_bytes());
        }
        full_bx(b"stts", 0, 0, &b)
    }

    /// `stsc` for `sample_count` samples grouped `per_chunk` at a time: one run, plus a
    /// second for the shorter final chunk when the count does not divide evenly.
    fn stsc(sample_count: u32, per_chunk: u32) -> Vec<u8> {
        let full_chunks = sample_count / per_chunk;
        let remainder = sample_count % per_chunk;
        let mut runs: Vec<(u32, u32)> = Vec::new();
        if full_chunks > 0 {
            runs.push((1, per_chunk));
        }
        if remainder > 0 {
            runs.push((full_chunks + 1, remainder));
        }
        let mut b = (runs.len() as u32).to_be_bytes().to_vec();
        for (first_chunk, samples) in runs {
            b.extend_from_slice(&first_chunk.to_be_bytes());
            b.extend_from_slice(&samples.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
        }
        full_bx(b"stsc", 0, 0, &b)
    }

    /// `ctts`, run-length encoded. Version 1 (signed offsets) as soon as any offset is
    /// negative, which is the only way to express a frame that presents before the one
    /// it decodes after.
    fn ctts(offsets: &[i32]) -> Vec<u8> {
        let mut runs: Vec<(u32, i32)> = Vec::new();
        for &o in offsets {
            match runs.last_mut() {
                Some((count, prev)) if *prev == o => *count += 1,
                _ => runs.push((1, o)),
            }
        }
        let version = u8::from(offsets.iter().any(|&o| o < 0));
        let mut b = (runs.len() as u32).to_be_bytes().to_vec();
        for (count, offset) in runs {
            b.extend_from_slice(&count.to_be_bytes());
            b.extend_from_slice(&offset.to_be_bytes());
        }
        full_bx(b"ctts", version, 0, &b)
    }

    /// `stss`: the 1-based numbers of the sync samples.
    fn stss(samples: &[u32]) -> Vec<u8> {
        let mut b = (samples.len() as u32).to_be_bytes().to_vec();
        for s in samples {
            b.extend_from_slice(&s.to_be_bytes());
        }
        full_bx(b"stss", 0, 0, &b)
    }

    fn stsz_table(sizes: &[u32]) -> Vec<u8> {
        let mut b = 0u32.to_be_bytes().to_vec(); // sample_size 0 = table follows
        b.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        for s in sizes {
            b.extend_from_slice(&s.to_be_bytes());
        }
        full_bx(b"stsz", 0, 0, &b)
    }

    fn chunk_offset_box(offsets: &[u64], co64: bool) -> Vec<u8> {
        let mut b = (offsets.len() as u32).to_be_bytes().to_vec();
        for &o in offsets {
            if co64 {
                b.extend_from_slice(&o.to_be_bytes());
            } else {
                b.extend_from_slice(&(o as u32).to_be_bytes());
            }
        }
        full_bx(if co64 { b"co64" } else { b"stco" }, 0, 0, &b)
    }

    fn unity_matrix(b: &mut Vec<u8>) {
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            b.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        out.extend_from_slice(typ);
        out.extend_from_slice(body);
        out
    }

    fn full_bx(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut inner = Vec::with_capacity(4 + body.len());
        inner.push(version);
        inner.extend_from_slice(&flags.to_be_bytes()[1..]);
        inner.extend_from_slice(body);
        bx(typ, &inner)
    }

    fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
        for p in parts {
            out.extend_from_slice(p);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::builder::{progressive_mp4, progressive_mp4_with, Options};
    use super::*;
    use crate::aac::builder::adts_frame;
    use crate::fmp4::inspect::*;
    use crate::remux::Remuxer;
    use crate::ts::builder::{pat, pes, pes_to_packets, pmt, ts_packet, AUDIO_PID};
    use crate::ts::STREAM_TYPE_AAC_ADTS;

    /// Five AAC "frames" of differing lengths so a misaligned sample boundary shows.
    fn frames() -> Vec<Vec<u8>> {
        (0..5u8)
            .map(|i| (0..(4 + i as usize)).map(|j| i * 16 + j as u8).collect())
            .collect()
    }

    fn options(interleave_video: bool) -> Options {
        Options {
            sample_rate: 44_100,
            channels: 2,
            samples_per_chunk: 2,
            interleave_video,
            co64: false,
            constant_sample_size: false,
        }
    }

    /// Top-level `(type, offset, size)` triples of a whole file.
    fn top_level(file: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < file.len() {
            let h = box_header(&file[at..]).expect("valid top-level header");
            let size = if h.size == 0 {
                file.len() - at
            } else {
                h.size as usize
            };
            out.push((h.kind, at, size));
            at += size;
        }
        out
    }

    /// The `moov` box, header included, wherever it sits in the file.
    fn moov_of(file: &[u8]) -> &[u8] {
        let (_, at, size) = top_level(file)
            .into_iter()
            .find(|(k, _, _)| k == b"moov")
            .expect("file has a moov");
        &file[at..at + size]
    }

    /// Where the builder put the audio chunks: byte ranges of `mdat` that are not
    /// video samples. Derived from the input alone, without consulting `stco`.
    fn expected_audio_ranges(file: &[u8], frames: &[Vec<u8>], opts: Options) -> Vec<(u64, u64)> {
        let (_, mdat_at, _) = top_level(file)
            .into_iter()
            .find(|(k, _, _)| k == b"mdat")
            .unwrap();
        let mut at = (mdat_at + 8) as u64;
        let mut out = Vec::new();
        for chunk in frames.chunks(opts.samples_per_chunk as usize) {
            if opts.interleave_video {
                at += 8; // one dummy video sample: 4-byte length + 4-byte NAL
            }
            let len: u64 = chunk.iter().map(|f| f.len() as u64).sum();
            out.push((at, len));
            at += len;
        }
        out
    }

    fn run_extractor(file: &[u8]) -> (AudioExtractor, Vec<u8>) {
        let mut x = AudioExtractor::from_moov(moov_of(file)).unwrap();
        let mut out = Vec::new();
        for i in 0..x.chunks().len() {
            let plan = x.chunks()[i];
            let bytes = &file[plan.offset as usize..(plan.offset + plan.len) as usize];
            out.extend(x.push_chunk(i, bytes).unwrap());
        }
        (x, out)
    }

    /// Every `mdat` payload in the buffer, concatenated.
    fn mdat_payloads(buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (kind, at, size) in top_level(buf) {
            if &kind == b"mdat" {
                out.extend_from_slice(&buf[at + 8..at + size]);
            }
        }
        out
    }

    fn tfdt_of_fragment(buf: &[u8], n: usize) -> u64 {
        let moofs: Vec<(usize, usize)> = top_level(buf)
            .into_iter()
            .filter(|(k, _, _)| k == b"moof")
            .map(|(_, at, size)| (at, size))
            .collect();
        let (at, size) = moofs[n];
        let tfdt = find_box(&buf[at..at + size], "tfdt").unwrap();
        u64::from_be_bytes(tfdt[4..12].try_into().unwrap())
    }

    // --- box_header ---

    #[test]
    fn box_header_reads_a_32_bit_size() {
        let mut b = 24u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(&[0; 16]);
        assert_eq!(
            box_header(&b),
            Some(BoxHeader {
                kind: *b"ftyp",
                size: 24,
                header_len: 8
            })
        );
    }

    #[test]
    fn box_header_reads_a_largesize() {
        let mut b = 1u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"mdat");
        b.extend_from_slice(&0x1_0000_0010u64.to_be_bytes());
        assert_eq!(
            box_header(&b),
            Some(BoxHeader {
                kind: *b"mdat",
                size: 0x1_0000_0010,
                header_len: 16
            })
        );
    }

    #[test]
    fn box_header_size_zero_means_to_end_of_file() {
        let mut b = 0u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"mdat");
        let h = box_header(&b).unwrap();
        assert_eq!(h.size, 0);
        assert_eq!(h.header_len, 8);
    }

    #[test]
    fn box_header_rejects_truncated_input() {
        assert_eq!(box_header(&[]), None);
        assert_eq!(box_header(&[0, 0, 0, 24, b'f', b't', b'y']), None);
        // largesize needs 16 bytes, not 8.
        let mut b = 1u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"mdat");
        assert_eq!(box_header(&b), None);
    }

    #[test]
    fn box_header_rejects_sizes_smaller_than_a_header() {
        for size in 2u32..8 {
            let mut b = size.to_be_bytes().to_vec();
            b.extend_from_slice(b"free");
            assert_eq!(box_header(&b), None, "size {size} cannot hold a header");
        }
    }

    // --- chunk plan ---

    #[test]
    fn chunk_plan_matches_where_the_builder_placed_the_audio() {
        for interleave in [false, true] {
            let opts = options(interleave);
            let file = progressive_mp4_with(&frames(), opts);
            let x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
            let got: Vec<(u64, u64)> = x.chunks().iter().map(|c| (c.offset, c.len)).collect();
            assert_eq!(
                got,
                expected_audio_ranges(&file, &frames(), opts),
                "interleave_video = {interleave}"
            );
            assert_eq!(
                x.chunks()
                    .iter()
                    .map(|c| c.sample_count)
                    .collect::<Vec<_>>(),
                vec![2, 2, 1],
                "five frames, two per chunk"
            );
        }
    }

    #[test]
    fn interleaved_video_makes_the_audio_ranges_non_contiguous() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, true);
        let x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        for pair in x.chunks().windows(2) {
            assert!(
                pair[1].offset > pair[0].offset + pair[0].len,
                "a gap (the video sample) must separate consecutive audio chunks"
            );
        }
    }

    #[test]
    fn track_parameters_are_read_from_the_moov() {
        let file = progressive_mp4(&frames(), 48_000, 1, 2, false);
        let x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        assert_eq!(x.sample_rate(), 48_000);
        assert_eq!(x.channels(), 1);
        assert_eq!(x.timescale(), 48_000);
        assert_eq!(x.duration(), 5 * 1024);
    }

    // --- output structure ---

    #[test]
    fn output_is_an_init_segment_then_one_fragment_per_chunk() {
        for interleave in [false, true] {
            let file = progressive_mp4(&frames(), 44_100, 2, 2, interleave);
            let (x, out) = run_extractor(&file);
            let types = box_types(&out);
            assert_eq!(&types[..2], &["ftyp", "moov"]);
            let rest: Vec<&str> = types[2..].iter().map(String::as_str).collect();
            assert_eq!(rest, vec!["moof", "mdat", "moof", "mdat", "moof", "mdat"]);
            assert!(walk_and_validate_all_box_sizes(&out));
            assert!(contains_box(&out, "esds"));
            assert!(!contains_box(&out, "avcC"), "video must not leak through");
            assert!(x.is_complete());
        }
    }

    #[test]
    fn mdat_payloads_are_the_input_frames_byte_for_byte() {
        for interleave in [false, true] {
            let file = progressive_mp4(&frames(), 44_100, 2, 2, interleave);
            let (_, out) = run_extractor(&file);
            assert_eq!(mdat_payloads(&out), frames().concat());
        }
    }

    #[test]
    fn fragment_decode_times_accumulate_the_sample_durations() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let (_, out) = run_extractor(&file);
        assert_eq!(tfdt_of_fragment(&out, 0), 0);
        assert_eq!(tfdt_of_fragment(&out, 1), 1024 * 2);
        assert_eq!(tfdt_of_fragment(&out, 2), 1024 * 4);
    }

    #[test]
    fn fragment_sequence_numbers_start_at_one_and_count_up() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let mut x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        for i in 0..3 {
            let plan = x.chunks()[i];
            let out = x
                .push_chunk(
                    i,
                    &file[plan.offset as usize..(plan.offset + plan.len) as usize],
                )
                .unwrap();
            assert_eq!(read_mfhd_sequence(&out), i as u32 + 1);
        }
    }

    #[test]
    fn only_the_first_push_carries_the_init_segment() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let mut x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        let slice = |p: ChunkPlan| &file[p.offset as usize..(p.offset + p.len) as usize];
        let first = x.push_chunk(0, slice(x.chunks()[0])).unwrap();
        let second = x.push_chunk(1, slice(x.chunks()[1])).unwrap();
        assert_eq!(&box_types(&first)[..2], &["ftyp", "moov"]);
        assert_eq!(box_types(&second), vec!["moof", "mdat"]);
    }

    #[test]
    fn the_files_audio_specific_config_is_carried_through_unchanged() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let (_, out) = run_extractor(&file);
        let in_esds = find_box(&file, "esds").unwrap();
        let out_esds = find_box(&out, "esds").unwrap();
        // The DecoderSpecificInfo is the last descriptor inside the DecoderConfig;
        // in both files it holds the same two ASC bytes.
        let in_dsi = parse_esds(&in_esds[4..]).unwrap().unwrap();
        let out_dsi = parse_esds(&out_esds[4..]).unwrap().unwrap();
        assert_eq!(in_dsi, out_dsi);
        assert_eq!(
            out_dsi,
            crate::aac::audio_specific_config(2, 44_100, 2),
            "for AAC-LC the recomputed ASC equals the file's"
        );
    }

    #[test]
    fn the_asc_bytes_are_passed_through_rather_than_recomputed() {
        // Same object type, rate and channels as the recompute would produce, but with
        // a trailing bit set that `aac::audio_specific_config` never writes. If the
        // extractor rebuilt the ASC from the decoded parameters, the bit would vanish.
        let plain = crate::aac::audio_specific_config(2, 44_100, 2);
        let odd = vec![plain[0], plain[1] | 0x01];
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let mut moov = moov_of(&file).to_vec();
        let pos = moov
            .windows(plain.len())
            .position(|w| w == plain.as_slice())
            .unwrap();
        moov[pos..pos + plain.len()].copy_from_slice(&odd);

        let mut x = AudioExtractor::from_moov(&moov).unwrap();
        assert_eq!((x.sample_rate(), x.channels()), (44_100, 2));
        let p = x.chunks()[0];
        let out = x
            .push_chunk(0, &file[p.offset as usize..(p.offset + p.len) as usize])
            .unwrap();
        let out_esds = find_box(&out, "esds").unwrap();
        assert_eq!(parse_esds(&out_esds[4..]).unwrap().unwrap(), odd);
    }

    // --- error paths ---

    #[test]
    fn feeding_a_chunk_out_of_order_is_refused_without_changing_state() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let mut x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        let p1 = x.chunks()[1];
        let err = x.push_chunk(1, &file[p1.offset as usize..(p1.offset + p1.len) as usize]);
        assert_eq!(
            err,
            Err(Mp4Error::OutOfOrder {
                expected: 0,
                got: 1
            })
        );
        // Chunk 0 must still be accepted afterwards.
        let p0 = x.chunks()[0];
        assert!(x
            .push_chunk(0, &file[p0.offset as usize..(p0.offset + p0.len) as usize])
            .is_ok());
    }

    #[test]
    fn a_chunk_of_the_wrong_length_is_refused() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let mut x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        let p0 = x.chunks()[0];
        let short = &file[p0.offset as usize..(p0.offset + p0.len - 1) as usize];
        assert_eq!(
            x.push_chunk(0, short),
            Err(Mp4Error::ChunkLength {
                expected: p0.len,
                got: p0.len - 1
            })
        );
    }

    #[test]
    fn pushing_past_the_last_chunk_is_out_of_order() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let (mut x, _) = run_extractor(&file);
        assert_eq!(
            x.push_chunk(3, &[]),
            Err(Mp4Error::OutOfOrder {
                expected: 3,
                got: 3
            })
        );
    }

    #[test]
    fn a_moov_with_only_a_video_track_has_no_audio() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, true);
        let moov = moov_of(&file);
        // Drop the audio trak (the last child) and shrink the moov size to match.
        let body = &moov[8..];
        let kids = children(body).unwrap();
        let audio_len = kids.last().unwrap().1.len() + 8;
        let mut video_only = moov[..moov.len() - audio_len].to_vec();
        video_only[..4].copy_from_slice(&((moov.len() - audio_len) as u32).to_be_bytes());
        assert_eq!(
            AudioExtractor::from_moov(&video_only).err(),
            Some(Mp4Error::NoAudioTrack)
        );
    }

    #[test]
    fn a_sound_track_with_another_codec_is_reported_by_name() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let mut moov = moov_of(&file).to_vec();
        let pos = moov.windows(4).position(|w| w == b"mp4a").unwrap();
        moov[pos..pos + 4].copy_from_slice(b"ac-3");
        assert_eq!(
            AudioExtractor::from_moov(&moov).err(),
            Some(Mp4Error::UnsupportedCodec("ac-3".into()))
        );
    }

    #[test]
    fn a_non_mpeg4_object_type_indication_is_unsupported() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let mut moov = moov_of(&file).to_vec();
        // The DecoderConfigDescriptor's first payload byte follows tag 0x04 and its
        // four-byte length; the builder writes exactly this preamble.
        let pos = moov
            .windows(6)
            .position(|w| w[0] == 0x04 && w[1] == 0x80 && w[5] == 0x40)
            .unwrap();
        moov[pos + 5] = 0x69; // MPEG-2 Audio (MP3)
        assert_eq!(
            AudioExtractor::from_moov(&moov).err(),
            Some(Mp4Error::UnsupportedCodec(
                "objectTypeIndication 0x69".into()
            ))
        );
    }

    #[test]
    fn a_non_moov_box_is_malformed() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        assert!(matches!(
            AudioExtractor::from_moov(&file),
            Err(Mp4Error::Malformed(_))
        ));
        assert!(matches!(
            AudioExtractor::from_moov(&[]),
            Err(Mp4Error::Malformed(_))
        ));
    }

    #[test]
    fn a_truncated_moov_is_malformed_not_a_panic() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, false);
        let moov = moov_of(&file);
        for cut in (8..moov.len()).step_by(7) {
            let r = AudioExtractor::from_moov(&moov[..cut]);
            assert!(matches!(r, Err(Mp4Error::Malformed(_))), "cut at {cut}");
        }
    }

    // --- stbl variants ---

    #[test]
    fn constant_sample_size_stsz_is_expanded() {
        let equal: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i; 7]).collect();
        let mut opts = options(false);
        opts.constant_sample_size = true;
        opts.samples_per_chunk = 4;
        let file = progressive_mp4_with(&equal, opts);
        assert!(
            find_box(&file, "stsz").map(|b| &b[4..8]) == Some(&7u32.to_be_bytes()[..]),
            "builder must have written a constant size"
        );
        let x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        assert_eq!(
            x.chunks()
                .iter()
                .map(|c| (c.len, c.sample_count))
                .collect::<Vec<_>>(),
            vec![(28, 4), (14, 2)]
        );
        let (_, out) = run_extractor(&file);
        assert_eq!(mdat_payloads(&out), equal.concat());
    }

    #[test]
    fn co64_offsets_are_followed() {
        for interleave in [false, true] {
            let mut opts = options(interleave);
            opts.co64 = true;
            let file = progressive_mp4_with(&frames(), opts);
            assert!(contains_box(&file, "co64"));
            assert!(!contains_box(&file, "stco"));
            let x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
            let got: Vec<(u64, u64)> = x.chunks().iter().map(|c| (c.offset, c.len)).collect();
            assert_eq!(got, expected_audio_ranges(&file, &frames(), opts));
            let (_, out) = run_extractor(&file);
            assert_eq!(mdat_payloads(&out), frames().concat());
        }
    }

    #[test]
    fn a_single_huge_chunk_is_split_into_bounded_plans() {
        // 40 frames of 1 MiB in one chunk: without splitting, the caller would have to
        // hold 40 MiB at once.
        let frames: Vec<Vec<u8>> = (0..40u8).map(|i| vec![i; 1 << 20]).collect();
        let mut opts = options(false);
        opts.constant_sample_size = true;
        opts.samples_per_chunk = 40;
        let file = progressive_mp4_with(&frames, opts);
        let x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        assert_eq!(x.chunks().len(), 10, "4 MiB per plan");
        assert!(x.chunks().iter().all(|c| c.len <= MAX_PLAN_BYTES));
        assert_eq!(x.chunks().iter().map(|c| c.sample_count).sum::<u32>(), 40);
        for pair in x.chunks().windows(2) {
            assert_eq!(
                pair[1].offset,
                pair[0].offset + pair[0].len,
                "plans stay contiguous"
            );
        }
        let (_, out) = run_extractor(&file);
        assert_eq!(mdat_payloads(&out).len(), 40 << 20);
        assert_eq!(tfdt_of_fragment(&out, 1), 4 * 1024);
    }

    #[test]
    fn stsc_runs_expand_to_the_last_chunk() {
        // Eleven frames, four per chunk: runs (1, 4) and (3, 3).
        let frames: Vec<Vec<u8>> = (0..11u8).map(|i| vec![i; 3]).collect();
        let file = progressive_mp4(&frames, 44_100, 2, 4, false);
        let x = AudioExtractor::from_moov(moov_of(&file)).unwrap();
        assert_eq!(
            x.chunks()
                .iter()
                .map(|c| c.sample_count)
                .collect::<Vec<_>>(),
            vec![4, 4, 3]
        );
        let (_, out) = run_extractor(&file);
        assert_eq!(mdat_payloads(&out), frames.concat());
    }

    #[test]
    fn a_moov_placed_after_the_mdat_works_the_same() {
        let opts = options(true);
        let file = progressive_mp4_with(&frames(), opts);
        let reordered = move_moov_to_end(&file);
        let kinds: Vec<[u8; 4]> = top_level(&reordered).iter().map(|t| t.0).collect();
        assert_eq!(kinds, vec![*b"ftyp", *b"mdat", *b"moov"]);

        let x = AudioExtractor::from_moov(moov_of(&reordered)).unwrap();
        let got: Vec<(u64, u64)> = x.chunks().iter().map(|c| (c.offset, c.len)).collect();
        assert_eq!(got, expected_audio_ranges(&reordered, &frames(), opts));
        let (_, out) = run_extractor(&reordered);
        assert_eq!(mdat_payloads(&out), frames().concat());
        // And the moov's position does not change the output at all.
        assert_eq!(out, run_extractor(&file).1);
    }

    /// Move the `moov` behind the `mdat`, shifting every chunk offset accordingly,
    /// the way a muxer that writes `moov` last lays the file out.
    fn move_moov_to_end(file: &[u8]) -> Vec<u8> {
        let boxes = top_level(file);
        let (_, moov_at, moov_size) = *boxes.iter().find(|b| &b.0 == b"moov").unwrap();
        let mut moov = file[moov_at..moov_at + moov_size].to_vec();
        shift_chunk_offsets(&mut moov, -(moov_size as i64));
        let mut out = Vec::new();
        for (kind, at, size) in &boxes {
            if kind != b"moov" {
                out.extend_from_slice(&file[*at..*at + *size]);
            }
        }
        out.extend_from_slice(&moov);
        out
    }

    fn shift_chunk_offsets(buf: &mut [u8], delta: i64) {
        const CONTAINERS: &[&[u8; 4]] = &[b"moov", b"trak", b"mdia", b"minf", b"stbl"];
        let mut at = 0;
        while at + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = buf[at + 4..at + 8].try_into().unwrap();
            if CONTAINERS.contains(&&kind) {
                shift_chunk_offsets(&mut buf[at + 8..at + size], delta);
            } else if &kind == b"stco" || &kind == b"co64" {
                let wide = &kind == b"co64";
                let count = u32::from_be_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                let mut p = at + 16;
                for _ in 0..count {
                    if wide {
                        let v = u64::from_be_bytes(buf[p..p + 8].try_into().unwrap());
                        buf[p..p + 8].copy_from_slice(&((v as i64 + delta) as u64).to_be_bytes());
                        p += 8;
                    } else {
                        let v = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap());
                        buf[p..p + 4]
                            .copy_from_slice(&((i64::from(v) + delta) as u32).to_be_bytes());
                        p += 4;
                    }
                }
            }
            at += size;
        }
    }

    // --- determinism and cross-checks ---

    #[test]
    fn output_is_byte_identical_across_runs() {
        let file = progressive_mp4(&frames(), 44_100, 2, 2, true);
        assert_eq!(run_extractor(&file).1, run_extractor(&file).1);
        assert_eq!(
            progressive_mp4(&frames(), 44_100, 2, 2, true),
            file,
            "the builder itself must be deterministic"
        );
    }

    #[test]
    fn builder_output_has_valid_box_sizes_and_the_expected_layout() {
        for interleave in [false, true] {
            let file = progressive_mp4(&frames(), 44_100, 2, 2, interleave);
            assert!(walk_and_validate_all_box_sizes(&file));
            assert_eq!(box_types(&file), vec!["ftyp", "moov", "mdat"]);
            assert_eq!(contains_box(&file, "avcC"), interleave);
            assert!(contains_box(&file, "esds"));
            assert!(contains_box(&file, "smhd"));
        }
    }

    #[test]
    fn matches_the_transport_stream_path_for_the_same_frames() {
        // The same AAC frames through the TS remuxer, audio-only, all in one segment.
        let mut adts = Vec::new();
        for f in frames() {
            adts.extend(adts_frame(2, 4, 2, &f));
        }
        let mut ts = ts_packet(0, true, 0, &pat());
        ts.extend(ts_packet(
            4096,
            true,
            0,
            &pmt(&[(AUDIO_PID, STREAM_TYPE_AAC_ADTS)]),
        ));
        ts.extend(pes_to_packets(AUDIO_PID, &pes(0xC0, 0, None, &adts)));
        let from_ts = Remuxer::audio_only().push_ts_segment(&ts).unwrap();

        let file = progressive_mp4(&frames(), 44_100, 2, 5, false);
        let (_, from_mp4) = run_extractor(&file);

        assert_eq!(
            find_box(&from_ts, "esds").unwrap(),
            find_box(&from_mp4, "esds").unwrap(),
            "both paths must describe the codec identically"
        );
        assert_eq!(mdat_payloads(&from_ts), mdat_payloads(&from_mp4));
        // Same codec parameters, so the whole init segment is the same too.
        let init_len = |b: &[u8]| {
            let t = top_level(b);
            t[0].2 + t[1].2
        };
        assert_eq!(
            &from_ts[..init_len(&from_ts)],
            &from_mp4[..init_len(&from_mp4)]
        );
    }

    #[test]
    fn chunk_plan_round_trips_through_serde() {
        let plan = ChunkPlan {
            offset: 1 << 40,
            len: 12_345,
            sample_count: 7,
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<ChunkPlan>(&json).unwrap(), plan);
    }

    #[test]
    fn errors_display_their_specifics() {
        assert_eq!(
            Mp4Error::OutOfOrder {
                expected: 2,
                got: 5
            }
            .to_string(),
            "chunk 5 fed out of order; expected chunk 2"
        );
        assert_eq!(
            Mp4Error::UnsupportedCodec("ac-3".into()).to_string(),
            "audio track is not MPEG-4 AAC (ac-3)"
        );
        assert_eq!(
            Mp4Error::Malformed("stsc").to_string(),
            "malformed MP4: stsc"
        );
    }
}
