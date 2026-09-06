//! Video-only MP4 + audio-only MP4 → one fragmented MP4.
//!
//! YouTube stopped offering muxed formats: its iOS client answers with video-only and
//! audio-only renditions and nothing combined, and Bilibili's DASH is split the same
//! way. Downloading "1080p with sound" therefore means fetching two files and putting
//! them back together, and that has to happen here — losslessly, and without a
//! transcode.
//!
//! This is [`crate::mp4::AudioExtractor`] run in reverse. That one reads one
//! progressive MP4 and writes an audio-only fragmented MP4; this one reads two and
//! writes one carrying both tracks. Neither re-encodes anything: the samples are the
//! bytes the inputs held, and only their framing changes.
//!
//! Both inputs are indexed from their `moov` alone, so the caller still reads by byte
//! range and never holds a whole file. [`Muxer::reads`] lists those ranges already
//! interleaved by decode time, so feeding them in order produces an output that plays
//! progressively rather than one that buffers a whole track before the other starts.

use serde::{Deserialize, Serialize};

use crate::fmp4::{self, Sample, TrackConfig, TrackKind};
use crate::mp4::{
    be_u16, be_u32, child, children, first_sample_entry, fourcc, handler_type, media_tables,
    moov_body, parse_mp4a, parse_pairs, parse_sample_tables, plan_chunks, DurationRuns,
    SampleSizes, SampleTables,
};

pub use crate::mp4::{ChunkPlan, Mp4Error};

/// Output track ids. The same two [`crate::remux`] uses, so a merged file and a remuxed
/// one describe their tracks identically.
const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;

/// The common timebase the two tracks' decode times are compared in. Milliseconds is
/// coarse enough that neither timescale divides awkwardly and fine enough that a
/// fragment never lands on the wrong side of its neighbour.
const INTERLEAVE_TIMESCALE: u128 = 1000;

/// Bytes of a `VisualSampleEntry` before its child boxes begin.
const VISUAL_ENTRY_HEADER: usize = 78;

/// Which input a planned read belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Video,
    Audio,
}

/// One byte range of one input that the caller must fetch and feed back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRead {
    pub source: Source,
    /// Absolute byte offset in that input's file.
    pub offset: u64,
    pub len: u64,
    /// How many samples the range holds.
    pub sample_count: u32,
}

impl MergeRead {
    fn of(source: Source, plan: ChunkPlan) -> Self {
        Self {
            source,
            offset: plan.offset,
            len: plan.len,
            sample_count: plan.sample_count,
        }
    }
}

/// Merges a video-only and an audio-only MP4 into one fragmented MP4, a chunk at a time.
pub struct Muxer {
    video: Input,
    audio: Input,
    reads: Vec<MergeRead>,
    /// Index into `reads` of the next read expected.
    next_read: usize,
    init_emitted: bool,
}

impl Muxer {
    /// Index both inputs from their `moov` boxes, headers included.
    ///
    /// Each input is required to carry the track it is named for: an audio-only file
    /// handed in as the video input is a caller mistake worth reporting rather than a
    /// file to silently produce a soundtrack-shaped video from.
    pub fn from_moovs(video_moov: &[u8], audio_moov: &[u8]) -> Result<Self, Mp4Error> {
        let video = index_video(moov_body(video_moov)?)?;
        let audio = index_audio(moov_body(audio_moov)?)?;
        let reads = interleave(&video, &audio);
        Ok(Self {
            video: video.input,
            audio: audio.input,
            reads,
            next_read: 0,
            init_emitted: false,
        })
    }

    /// The reads to perform, in the order they must be fed.
    ///
    /// Each is one `stsc` chunk's worth of one input's samples, or a sample-aligned
    /// slice of one when the chunk is very large — a whole video chunk can be tens of
    /// megabytes, and the caller holds one read at a time.
    pub fn reads(&self) -> &[MergeRead] {
        &self.reads
    }

    /// Feed read `index`, which must be the next one in order and exactly
    /// `reads()[index].len` bytes long.
    ///
    /// Returns the bytes to append to the output: the init segment describing *both*
    /// tracks followed by the first fragment on the first call, one fragment per call
    /// after that. A rejected call leaves the muxer untouched, so the caller can
    /// re-fetch and retry.
    pub fn push(&mut self, index: usize, bytes: &[u8]) -> Result<Vec<u8>, Mp4Error> {
        // A complete muxer has no next read; reporting `expected == reads.len()` tells
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

        let (track_id, base, samples) = {
            let (track_id, input) = match read.source {
                Source::Video => (VIDEO_TRACK_ID, &mut self.video),
                Source::Audio => (AUDIO_TRACK_ID, &mut self.audio),
            };
            let samples = input.take(&read, bytes)?;
            let base = input.decode_time;
            input.decode_time += samples.iter().map(|s| u64::from(s.duration)).sum::<u64>();
            input.next_sample += read.sample_count;
            (track_id, base, samples)
        };

        let mut out = Vec::new();
        if !self.init_emitted {
            out.extend(fmp4::write_init_segment(&[
                self.video.cfg.clone(),
                self.audio.cfg.clone(),
            ]));
            self.init_emitted = true;
        }
        // One sequence number space shared by both tracks, as `remux` does: the reads
        // are already in output order, so their index is that number.
        let seq = (index + 1) as u32;
        out.extend(fmp4::write_fragment(seq, track_id, base, &samples));
        self.next_read += 1;
        Ok(out)
    }

    /// True once every planned read has been fed.
    pub fn is_complete(&self) -> bool {
        self.next_read == self.reads.len()
    }

    pub fn video_timescale(&self) -> u32 {
        self.video.timescale
    }

    pub fn audio_timescale(&self) -> u32 {
        self.audio.timescale
    }

    /// Total duration in milliseconds: the longer of the two tracks.
    ///
    /// Summed from the sample tables rather than read from `mdhd`, whose duration field
    /// is routinely 0 or all-ones in files written by a streaming muxer.
    pub fn duration_ms(&self) -> u64 {
        let longest = to_ms(self.video.total_duration, self.video.timescale)
            .max(to_ms(self.audio.total_duration, self.audio.timescale));
        u64::try_from(longest).unwrap_or(u64::MAX)
    }
}

/// One indexed input: how to describe its samples, and how far through them we are.
struct Input {
    cfg: TrackConfig,
    timescale: u32,
    durations: DurationRuns,
    sizes: SampleSizes,
    ctts: CttsRuns,
    stss: SyncSamples,
    /// Every sample's duration summed, in this track's timescale.
    total_duration: u64,
    /// Index of the first sample of the next read.
    next_sample: u32,
    /// Decode time of that sample, in this track's timescale.
    decode_time: u64,
}

impl Input {
    /// Turn one read's bytes into `trun` samples, advancing every cursor.
    fn take(&mut self, read: &MergeRead, bytes: &[u8]) -> Result<Vec<Sample>, Mp4Error> {
        let mut samples = Vec::with_capacity(read.sample_count as usize);
        let mut at = 0usize;
        for i in 0..read.sample_count {
            let index = self.next_sample + i;
            let size = self
                .sizes
                .get(index)
                .ok_or(Mp4Error::Malformed("read exceeds stsz"))? as usize;
            let duration = self
                .durations
                .next()
                .ok_or(Mp4Error::Malformed("read exceeds stts"))?;
            let data = bytes
                .get(at..at + size)
                .ok_or(Mp4Error::Malformed("read is shorter than its samples"))?;
            samples.push(Sample {
                data: data.to_vec(),
                duration,
                // `stss` numbers samples from one, not from zero.
                is_sync: self.stss.is_sync(index + 1),
                cts_offset: self.ctts.next(),
            });
            at += size;
        }
        Ok(samples)
    }
}

/// An input plus the planning that decides where its fragments go. The plans and their
/// start times are only needed while building the read order, so they never reach the
/// [`Muxer`] itself.
struct Indexed {
    input: Input,
    plans: Vec<ChunkPlan>,
    /// Decode time of each plan's first sample, in the track's own timescale.
    starts: Vec<u64>,
}

/// Merge the two plan lists into the order their fragments must be written in.
///
/// Whichever track's next chunk starts earlier goes first, compared in milliseconds so
/// a 90 kHz video track and a 44.1 kHz audio track are commensurable. The arithmetic is
/// done in `u128` because a 90 kHz duration multiplied by 1000 leaves `u64` after a few
/// years of media, and a merge must not reorder itself on a long file.
///
/// Ties go to video, which is both deterministic and what a player prefers: the video
/// sample for a given instant should already be buffered when its audio arrives.
fn interleave(video: &Indexed, audio: &Indexed) -> Vec<MergeRead> {
    let mut reads = Vec::with_capacity(video.plans.len() + audio.plans.len());
    let (mut v, mut a) = (0usize, 0usize);
    while v < video.plans.len() || a < audio.plans.len() {
        let next_video = video
            .starts
            .get(v)
            .map(|&t| to_ms(t, video.input.timescale));
        let next_audio = audio
            .starts
            .get(a)
            .map(|&t| to_ms(t, audio.input.timescale));
        let take_video = match (next_video, next_audio) {
            (Some(vt), Some(at)) => vt <= at,
            (Some(_), None) => true,
            _ => false,
        };
        if take_video {
            reads.push(MergeRead::of(Source::Video, video.plans[v]));
            v += 1;
        } else {
            reads.push(MergeRead::of(Source::Audio, audio.plans[a]));
            a += 1;
        }
    }
    reads
}

/// Ticks in `timescale` units, as milliseconds.
fn to_ms(ticks: u64, timescale: u32) -> u128 {
    u128::from(ticks) * INTERLEAVE_TIMESCALE / u128::from(timescale)
}

/// Index the first video track of a `moov` body.
fn index_video(body: &[u8]) -> Result<Indexed, Mp4Error> {
    let (timescale, stbl) =
        find_track(body, b"vide")?.ok_or(Mp4Error::Malformed("video input has no video track"))?;
    let (entry_kind, entry) = first_sample_entry(stbl)?;
    // `fmp4::TrackKind::Video` carries an `avcC` and nothing else, so an AV1 (`av1C`) or
    // HEVC (`hvcC`) configuration has nowhere to go. Naming the fourcc is what lets the
    // caller tell the user which format to pick instead.
    if &entry_kind != b"avc1" && &entry_kind != b"avc3" {
        return Err(Mp4Error::UnsupportedCodec(fourcc(&entry_kind)));
    }
    // 6 reserved + data_reference_index(2) + pre_defined/reserved(16), then width and
    // height as 16-bit integers.
    let width = be_u16(entry, 24).ok_or(Mp4Error::Malformed("visual sample entry"))?;
    let height = be_u16(entry, 26).ok_or(Mp4Error::Malformed("visual sample entry"))?;
    let child_boxes = entry.get(VISUAL_ENTRY_HEADER..).unwrap_or(&[]);
    let avcc = child(child_boxes, b"avcC")?
        .ok_or(Mp4Error::Malformed("video sample entry has no avcC"))?
        .to_vec();

    let cfg = TrackConfig {
        track_id: VIDEO_TRACK_ID,
        timescale,
        kind: TrackKind::Video {
            width: u32::from(width),
            height: u32::from(height),
            avcc,
        },
    };
    index(cfg, parse_sample_tables(stbl, timescale)?, stbl)
}

/// Index the first audio track of a `moov` body.
fn index_audio(body: &[u8]) -> Result<Indexed, Mp4Error> {
    let (timescale, stbl) = find_track(body, b"soun")?.ok_or(Mp4Error::NoAudioTrack)?;
    let (entry_kind, entry) = first_sample_entry(stbl)?;
    if &entry_kind != b"mp4a" {
        return Err(Mp4Error::UnsupportedCodec(fourcc(&entry_kind)));
    }
    let cfg = parse_mp4a(entry)?.track_config(AUDIO_TRACK_ID, timescale);
    index(cfg, parse_sample_tables(stbl, timescale)?, stbl)
}

/// The media timescale and `stbl` of the first `trak` with the given handler type.
fn find_track<'a>(body: &'a [u8], handler: &[u8; 4]) -> Result<Option<(u32, &'a [u8])>, Mp4Error> {
    for (kind, trak) in children(body)? {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia")? else {
            continue;
        };
        if handler_type(mdia)? != Some(*handler) {
            continue;
        }
        return media_tables(mdia).map(Some);
    }
    Ok(None)
}

/// Plan one track's reads and pre-compute where each of them starts on the timeline.
fn index(cfg: TrackConfig, tables: SampleTables, stbl: &[u8]) -> Result<Indexed, Mp4Error> {
    let plans = plan_chunks(&tables)?;
    let ctts = CttsRuns::new(match child(stbl, b"ctts")? {
        Some(body) => parse_ctts(body)?,
        None => Vec::new(),
    });
    let stss = SyncSamples::new(match child(stbl, b"stss")? {
        Some(body) => Some(parse_stss(body)?),
        None => None,
    });

    // Walked on a clone so the muxer's own cursor still starts at sample zero.
    let mut cursor = tables.durations.clone();
    let mut starts = Vec::with_capacity(plans.len());
    let mut elapsed = 0u64;
    for plan in &plans {
        starts.push(elapsed);
        for _ in 0..plan.sample_count {
            elapsed += u64::from(cursor.next().ok_or(Mp4Error::Malformed(
                "stts describes fewer samples than stsz",
            ))?);
        }
    }

    Ok(Indexed {
        input: Input {
            cfg,
            timescale: tables.timescale,
            durations: tables.durations,
            sizes: tables.sizes,
            ctts,
            stss,
            total_duration: elapsed,
            next_sample: 0,
            decode_time: 0,
        },
        plans,
        starts,
    })
}

/// `ctts` runs: `(count, offset)`.
///
/// Version 0 declares the offset unsigned and version 1 signed, but it is the same 32
/// bits either way and no file carries a version-0 offset above 2^31 — that would be a
/// composition delay of hours — so both are read as `i32`.
fn parse_ctts(full_box: &[u8]) -> Result<Vec<(u32, i32)>, Mp4Error> {
    Ok(parse_pairs(full_box, "ctts")?
        .into_iter()
        .map(|(count, offset)| (count, offset as i32))
        .collect())
}

/// `stss`: the 1-based numbers of the sync samples, ascending.
fn parse_stss(full_box: &[u8]) -> Result<Vec<u32>, Mp4Error> {
    let count = be_u32(full_box, 4).ok_or(Mp4Error::Malformed("stss"))? as usize;
    let table = full_box
        .get(8..)
        .filter(|t| t.len() >= count * 4)
        .ok_or(Mp4Error::Malformed("stss table"))?;
    Ok(table[..count * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|e| u32::from_be_bytes(*e))
        .collect())
}

/// Composition offsets as `ctts` stores them, walked with a cursor for the same reason
/// [`DurationRuns`] is: a constant-offset track is one run, and expanding it would cost
/// four bytes per frame for nothing.
struct CttsRuns {
    runs: Vec<(u32, i32)>,
    /// Cursor: index of the current run, and samples already consumed from it.
    run: usize,
    used: u32,
}

impl CttsRuns {
    fn new(runs: Vec<(u32, i32)>) -> Self {
        Self {
            runs,
            run: 0,
            used: 0,
        }
    }

    /// The next sample's composition offset, advancing the cursor.
    ///
    /// Zero once the table runs out. A track with no `ctts` and a `ctts` that stops
    /// short both mean the same thing: those samples present when they decode.
    fn next(&mut self) -> i32 {
        while let Some(&(count, offset)) = self.runs.get(self.run) {
            if self.used < count {
                self.used += 1;
                return offset;
            }
            self.run += 1;
            self.used = 0;
        }
        0
    }
}

/// The `stss` table, walked in sample order.
struct SyncSamples {
    samples: Option<Vec<u32>>,
    at: usize,
}

impl SyncSamples {
    fn new(samples: Option<Vec<u32>>) -> Self {
        Self { samples, at: 0 }
    }

    /// Whether sample `number` (1-based) is a sync sample.
    ///
    /// No `stss` at all means every sample is one — that is what the spec says an
    /// absent table means, and it is what an all-keyframe track relies on. Getting this
    /// backwards for a track that does have a `stss` is what makes a player refuse to
    /// seek: it looks for a sync sample and finds none.
    fn is_sync(&mut self, number: u32) -> bool {
        let Some(samples) = &self.samples else {
            return true;
        };
        while self.at < samples.len() && samples[self.at] < number {
            self.at += 1;
        }
        samples.get(self.at) == Some(&number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fmp4::inspect::*;
    use crate::mp4::builder::{progressive_mp4, progressive_mp4_video_only, VideoOptions};
    use crate::mp4::MAX_PLAN_BYTES;

    /// `fmp4`'s sample flags, which are private to that module. Repeated rather than
    /// exposed so a test reading the wrong constant cannot be masked by the writer
    /// having been changed to match.
    const FLAGS_SYNC: u32 = 0x0200_0000;
    const FLAGS_NON_SYNC: u32 = 0x0101_0000;

    /// Six AVCC-framed access units of differing lengths, so a misaligned sample
    /// boundary shows up as wrong bytes rather than as a shifted-but-plausible stream.
    fn video_samples() -> Vec<Vec<u8>> {
        (0..6u8)
            .map(|i| {
                let nal: Vec<u8> = (0..(5 + i as usize)).map(|j| i * 32 + j as u8).collect();
                let mut s = (nal.len() as u32).to_be_bytes().to_vec();
                s.extend_from_slice(&nal);
                s
            })
            .collect()
    }

    /// Eight AAC "frames", likewise of differing lengths.
    fn audio_frames() -> Vec<Vec<u8>> {
        (0..8u8)
            .map(|i| {
                (0..(4 + i as usize))
                    .map(|j| 0x80 + i * 8 + j as u8)
                    .collect()
            })
            .collect()
    }

    fn video_file(opts: VideoOptions) -> Vec<u8> {
        progressive_mp4_video_only(&video_samples(), opts)
    }

    /// An audio-only file: eight frames, two to a chunk.
    fn audio_file() -> Vec<u8> {
        progressive_mp4(&audio_frames(), 44_100, 2, 2, false)
    }

    /// Top-level `(type, offset, size)` triples of a whole file.
    fn top_level(file: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < file.len() {
            let h = crate::mp4::box_header(&file[at..]).expect("valid top-level header");
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

    fn box_of<'a>(file: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
        let (_, at, size) = top_level(file)
            .into_iter()
            .find(|(k, _, _)| k == kind)
            .expect("file has the box");
        &file[at..at + size]
    }

    fn moov_of(file: &[u8]) -> &[u8] {
        box_of(file, b"moov")
    }

    /// The absolute range of a progressive file's `mdat` payload.
    fn mdat_range(file: &[u8]) -> (u64, u64) {
        let (_, at, size) = top_level(file)
            .into_iter()
            .find(|(k, _, _)| k == b"mdat")
            .expect("file has an mdat");
        ((at + 8) as u64, (at + size) as u64)
    }

    fn track_id_of(moof: &[u8]) -> u32 {
        let tfhd = find_box(moof, "tfhd").expect("tfhd must exist");
        u32::from_be_bytes(tfhd[4..8].try_into().unwrap())
    }

    fn tfdt_of(moof: &[u8]) -> u64 {
        let tfdt = find_box(moof, "tfdt").expect("tfdt must exist");
        u64::from_be_bytes(tfdt[4..12].try_into().unwrap())
    }

    /// `(duration, size, flags, cts_offset)` for every sample of a fragment's `trun`.
    fn trun_samples(moof: &[u8]) -> Vec<(u32, u32, u32, i32)> {
        let trun = find_box(moof, "trun").expect("trun must exist");
        let count = u32::from_be_bytes(trun[4..8].try_into().unwrap()) as usize;
        (0..count)
            .map(|i| {
                let e = &trun[12 + 16 * i..12 + 16 * (i + 1)];
                (
                    u32::from_be_bytes(e[0..4].try_into().unwrap()),
                    u32::from_be_bytes(e[4..8].try_into().unwrap()),
                    u32::from_be_bytes(e[8..12].try_into().unwrap()),
                    i32::from_be_bytes(e[12..16].try_into().unwrap()),
                )
            })
            .collect()
    }

    /// Every fragment of the output as `(track id, moof bytes, mdat payload)`.
    fn fragments(buf: &[u8]) -> Vec<(u32, &[u8], &[u8])> {
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
            out.push((
                track_id_of(&buf[at..at + size]),
                &buf[at..at + size],
                &buf[mdat_at + 8..mdat_at + mdat_size],
            ));
        }
        out
    }

    /// The concatenated `mdat` payloads of one track's fragments.
    fn payload_of(buf: &[u8], track_id: u32) -> Vec<u8> {
        fragments(buf)
            .into_iter()
            .filter(|&(id, _, _)| id == track_id)
            .flat_map(|(_, _, data)| data.to_vec())
            .collect()
    }

    fn muxer(video: &[u8], audio: &[u8]) -> Muxer {
        Muxer::from_moovs(moov_of(video), moov_of(audio)).unwrap()
    }

    /// Feed every read in order and return the muxer alongside the whole output.
    fn run(video: &[u8], audio: &[u8]) -> (Muxer, Vec<u8>) {
        let mut m = muxer(video, audio);
        let mut out = Vec::new();
        for i in 0..m.reads().len() {
            let read = m.reads()[i];
            let src = match read.source {
                Source::Video => video,
                Source::Audio => audio,
            };
            let bytes = &src[read.offset as usize..(read.offset + read.len) as usize];
            out.extend(m.push(i, bytes).unwrap());
        }
        (m, out)
    }

    // --- planning ---

    #[test]
    fn the_reads_tile_each_inputs_media_data_exactly_once() {
        let video = video_file(VideoOptions::default());
        let audio = audio_file();
        let m = muxer(&video, &audio);

        for (source, file, samples) in [
            (Source::Video, &video, video_samples().len() as u32),
            (Source::Audio, &audio, audio_frames().len() as u32),
        ] {
            let mine: Vec<MergeRead> = m
                .reads()
                .iter()
                .copied()
                .filter(|r| r.source == source)
                .collect();
            let (start, end) = mdat_range(file);
            let mut at = start;
            for read in &mine {
                assert_eq!(read.offset, at, "{source:?} reads must not gap or overlap");
                at += read.len;
            }
            assert_eq!(at, end, "{source:?} reads must reach the end of the mdat");
            assert_eq!(
                mine.iter().map(|r| r.sample_count).sum::<u32>(),
                samples,
                "{source:?} reads must cover every sample"
            );
        }
    }

    #[test]
    fn the_reads_are_interleaved_rather_than_one_track_then_the_other() {
        let m = muxer(&video_file(VideoOptions::default()), &audio_file());
        let order: Vec<Source> = m.reads().iter().map(|r| r.source).collect();
        // 6 video chunks at 33.3 ms each against 4 audio chunks at 46.4 ms each.
        use Source::{Audio as A, Video as V};
        assert_eq!(order, vec![V, A, V, A, V, A, V, V, A, V]);
    }

    #[test]
    fn track_timescales_and_the_longer_duration_are_reported() {
        let m = muxer(&video_file(VideoOptions::default()), &audio_file());
        assert_eq!(m.video_timescale(), 90_000);
        assert_eq!(m.audio_timescale(), 44_100);
        // Six 30 fps frames is 200 ms; eight 1024-sample frames at 44.1 kHz is 185 ms.
        assert_eq!(m.duration_ms(), 200);
    }

    #[test]
    fn a_huge_video_chunk_is_split_into_bounded_reads() {
        // Ten 1 MiB frames in one chunk: without splitting the caller would have to
        // hold the lot, and a real 4K keyframe run is far bigger than this.
        let big: Vec<Vec<u8>> = (0..10u8).map(|i| vec![i; 1 << 20]).collect();
        let video = progressive_mp4_video_only(
            &big,
            VideoOptions {
                samples_per_chunk: 10,
                ..VideoOptions::default()
            },
        );
        let audio = audio_file();
        let m = muxer(&video, &audio);
        let reads: Vec<MergeRead> = m
            .reads()
            .iter()
            .copied()
            .filter(|r| r.source == Source::Video)
            .collect();
        assert_eq!(reads.len(), 3, "4 MiB per read");
        assert!(reads.iter().all(|r| r.len <= MAX_PLAN_BYTES));
        assert_eq!(reads.iter().map(|r| r.sample_count).sum::<u32>(), 10);
    }

    // --- output structure ---

    #[test]
    fn output_is_an_init_segment_then_alternating_moof_and_mdat() {
        let (m, out) = run(&video_file(VideoOptions::default()), &audio_file());
        let types = box_types(&out);
        assert_eq!(&types[..2], &["ftyp", "moov"]);
        let rest: Vec<&str> = types[2..].iter().map(String::as_str).collect();
        assert_eq!(rest.len(), 20, "ten reads, a moof and an mdat each");
        for pair in rest.chunks(2) {
            assert_eq!(pair, ["moof", "mdat"]);
        }
        assert!(walk_and_validate_all_box_sizes(&out));
        assert!(m.is_complete());
    }

    #[test]
    fn the_init_segment_describes_both_tracks() {
        let (_, out) = run(&video_file(VideoOptions::default()), &audio_file());
        let moov = moov_of(&out);
        let traks = children(crate::mp4::moov_body(moov).unwrap())
            .unwrap()
            .into_iter()
            .filter(|(kind, _)| kind == b"trak")
            .count();
        assert_eq!(traks, 2, "a merged file describes video and audio");
        assert!(contains_box(&out, "avcC"), "the video decoder config");
        assert!(contains_box(&out, "esds"), "the audio decoder config");
        assert!(contains_box(&out, "vmhd"));
        assert!(contains_box(&out, "smhd"));
    }

    #[test]
    fn only_the_first_push_carries_the_init_segment() {
        let (video, audio) = (video_file(VideoOptions::default()), audio_file());
        let mut m = muxer(&video, &audio);
        let mut emitted = Vec::new();
        for i in 0..2 {
            let read = m.reads()[i];
            let src = match read.source {
                Source::Video => &video,
                Source::Audio => &audio,
            };
            emitted.push(
                m.push(
                    i,
                    &src[read.offset as usize..(read.offset + read.len) as usize],
                )
                .unwrap(),
            );
        }
        assert_eq!(&box_types(&emitted[0])[..2], &["ftyp", "moov"]);
        assert_eq!(box_types(&emitted[1]), vec!["moof", "mdat"]);
    }

    #[test]
    fn each_tracks_mdat_bytes_are_its_input_samples_byte_for_byte() {
        let (_, out) = run(&video_file(VideoOptions::default()), &audio_file());
        assert_eq!(payload_of(&out, VIDEO_TRACK_ID), video_samples().concat());
        assert_eq!(payload_of(&out, AUDIO_TRACK_ID), audio_frames().concat());
    }

    #[test]
    fn fragments_alternate_between_the_two_tracks() {
        let (_, out) = run(&video_file(VideoOptions::default()), &audio_file());
        let ids: Vec<u32> = fragments(&out).into_iter().map(|(id, _, _)| id).collect();
        assert_eq!(
            &ids[..4],
            &[
                VIDEO_TRACK_ID,
                AUDIO_TRACK_ID,
                VIDEO_TRACK_ID,
                AUDIO_TRACK_ID
            ],
            "one track must not run to completion before the other starts"
        );
        assert!(ids.contains(&AUDIO_TRACK_ID));
        assert_eq!(*ids.last().unwrap(), VIDEO_TRACK_ID);
    }

    #[test]
    fn fragment_sequence_numbers_count_up_across_both_tracks() {
        let (_, out) = run(&video_file(VideoOptions::default()), &audio_file());
        let seqs: Vec<u32> = fragments(&out)
            .into_iter()
            .map(|(_, moof, _)| read_mfhd_sequence(moof))
            .collect();
        assert_eq!(seqs, (1..=10).collect::<Vec<u32>>());
    }

    #[test]
    fn each_tracks_second_fragment_starts_at_its_own_accumulated_duration() {
        let (_, out) = run(&video_file(VideoOptions::default()), &audio_file());
        let per_track = |want: u32| -> Vec<u64> {
            fragments(&out)
                .into_iter()
                .filter(|&(id, _, _)| id == want)
                .map(|(_, moof, _)| tfdt_of(moof))
                .collect()
        };
        // One 3000-tick frame per video chunk, two 1024-sample frames per audio chunk,
        // each counted in its own timescale.
        assert_eq!(per_track(VIDEO_TRACK_ID)[0], 0);
        assert_eq!(per_track(VIDEO_TRACK_ID)[1], 3000);
        assert_eq!(per_track(AUDIO_TRACK_ID)[0], 0);
        assert_eq!(per_track(AUDIO_TRACK_ID)[1], 2048);
    }

    // --- what a video track carries that an audio track does not ---

    #[test]
    fn composition_offsets_reach_the_trun() {
        // Crossing a run boundary inside one fragment is the case a run-length cursor
        // gets wrong, so the chunks hold two samples and the runs are three long.
        let offsets = vec![0, 1500, 1500, -3000, 0, 0];
        let video = video_file(VideoOptions {
            samples_per_chunk: 2,
            cts_offsets: Some(offsets.clone()),
            ..VideoOptions::default()
        });
        let (_, out) = run(&video, &audio_file());
        let got: Vec<i32> = fragments(&out)
            .into_iter()
            .filter(|&(id, _, _)| id == VIDEO_TRACK_ID)
            .flat_map(|(_, moof, _)| trun_samples(moof))
            .map(|(_, _, _, cts)| cts)
            .collect();
        assert_eq!(got, offsets, "B-frames stutter if these are dropped");
    }

    #[test]
    fn audio_samples_get_no_composition_offset() {
        let (_, out) = run(&video_file(VideoOptions::default()), &audio_file());
        let got: Vec<i32> = fragments(&out)
            .into_iter()
            .filter(|&(id, _, _)| id == AUDIO_TRACK_ID)
            .flat_map(|(_, moof, _)| trun_samples(moof))
            .map(|(_, _, _, cts)| cts)
            .collect();
        assert!(got.iter().all(|&c| c == 0));
    }

    #[test]
    fn stss_decides_which_video_samples_are_sync() {
        let video = video_file(VideoOptions {
            sync_samples: Some(vec![1, 4]),
            ..VideoOptions::default()
        });
        let (_, out) = run(&video, &audio_file());
        let flags: Vec<u32> = fragments(&out)
            .into_iter()
            .filter(|&(id, _, _)| id == VIDEO_TRACK_ID)
            .flat_map(|(_, moof, _)| trun_samples(moof))
            .map(|(_, _, f, _)| f)
            .collect();
        assert_eq!(
            flags,
            vec![
                FLAGS_SYNC,
                FLAGS_NON_SYNC,
                FLAGS_NON_SYNC,
                FLAGS_SYNC,
                FLAGS_NON_SYNC,
                FLAGS_NON_SYNC,
            ]
        );
    }

    #[test]
    fn a_video_track_without_stss_is_all_sync_samples() {
        let (_, out) = run(&video_file(VideoOptions::default()), &audio_file());
        let flags: Vec<u32> = fragments(&out)
            .into_iter()
            .flat_map(|(_, moof, _)| trun_samples(moof))
            .map(|(_, _, f, _)| f)
            .collect();
        assert!(flags.iter().all(|&f| f == FLAGS_SYNC));
    }

    #[test]
    fn co64_offsets_are_followed_on_either_input() {
        let video = video_file(VideoOptions {
            co64: true,
            ..VideoOptions::default()
        });
        assert!(contains_box(&video, "co64"));
        let (_, out) = run(&video, &audio_file());
        assert_eq!(payload_of(&out, VIDEO_TRACK_ID), video_samples().concat());
        assert_eq!(payload_of(&out, AUDIO_TRACK_ID), audio_frames().concat());
    }

    // --- error paths ---

    #[test]
    fn feeding_a_read_out_of_order_is_refused_without_changing_state() {
        let (video, audio) = (video_file(VideoOptions::default()), audio_file());
        let mut m = muxer(&video, &audio);
        let second = m.reads()[1];
        let bytes = &audio[second.offset as usize..(second.offset + second.len) as usize];
        assert_eq!(
            m.push(1, bytes),
            Err(Mp4Error::OutOfOrder {
                expected: 0,
                got: 1
            })
        );
        // Read 0 must still be accepted afterwards.
        let first = m.reads()[0];
        assert!(m
            .push(
                0,
                &video[first.offset as usize..(first.offset + first.len) as usize]
            )
            .is_ok());
    }

    #[test]
    fn a_read_of_the_wrong_length_is_refused() {
        let (video, audio) = (video_file(VideoOptions::default()), audio_file());
        let mut m = muxer(&video, &audio);
        let first = m.reads()[0];
        let short = &video[first.offset as usize..(first.offset + first.len - 1) as usize];
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
        let (mut m, _) = run(&video_file(VideoOptions::default()), &audio_file());
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
        let audio = audio_file();
        assert_eq!(
            Muxer::from_moovs(moov_of(&audio), moov_of(&audio)).err(),
            Some(Mp4Error::Malformed("video input has no video track"))
        );
    }

    #[test]
    fn a_video_only_file_as_the_audio_input_is_refused() {
        let video = video_file(VideoOptions::default());
        assert_eq!(
            Muxer::from_moovs(moov_of(&video), moov_of(&video)).err(),
            Some(Mp4Error::NoAudioTrack)
        );
    }

    #[test]
    fn a_non_avc_video_codec_is_reported_by_name() {
        for codec in [*b"av01", *b"hvc1"] {
            let video = video_file(VideoOptions {
                codec,
                ..VideoOptions::default()
            });
            assert_eq!(
                Muxer::from_moovs(moov_of(&video), moov_of(&audio_file())).err(),
                Some(Mp4Error::UnsupportedCodec(
                    String::from_utf8(codec.to_vec()).unwrap()
                )),
                "fmp4 can only describe an avcC"
            );
        }
    }

    #[test]
    fn a_truncated_moov_is_malformed_not_a_panic() {
        let video = video_file(VideoOptions::default());
        let audio = audio_file();
        let (v, a) = (moov_of(&video), moov_of(&audio));
        for cut in (8..v.len()).step_by(11) {
            assert!(
                matches!(Muxer::from_moovs(&v[..cut], a), Err(Mp4Error::Malformed(_))),
                "video cut at {cut}"
            );
        }
        for cut in (8..a.len()).step_by(11) {
            assert!(
                matches!(Muxer::from_moovs(v, &a[..cut]), Err(Mp4Error::Malformed(_))),
                "audio cut at {cut}"
            );
        }
    }

    #[test]
    fn a_non_moov_box_is_malformed() {
        let video = video_file(VideoOptions::default());
        let audio = audio_file();
        assert!(matches!(
            Muxer::from_moovs(&video, moov_of(&audio)),
            Err(Mp4Error::Malformed(_))
        ));
        assert!(matches!(
            Muxer::from_moovs(moov_of(&video), &[]),
            Err(Mp4Error::Malformed(_))
        ));
    }

    // --- determinism ---

    #[test]
    fn output_is_byte_identical_across_runs() {
        let (video, audio) = (video_file(VideoOptions::default()), audio_file());
        assert_eq!(run(&video, &audio).1, run(&video, &audio).1);
        assert_eq!(
            video,
            video_file(VideoOptions::default()),
            "the builder itself must be deterministic"
        );
    }

    #[test]
    fn is_complete_only_once_the_last_read_has_been_fed() {
        let (video, audio) = (video_file(VideoOptions::default()), audio_file());
        let mut m = muxer(&video, &audio);
        let total = m.reads().len();
        for i in 0..total {
            assert!(!m.is_complete(), "not complete before read {i}");
            let read = m.reads()[i];
            let src = match read.source {
                Source::Video => &video,
                Source::Audio => &audio,
            };
            m.push(
                i,
                &src[read.offset as usize..(read.offset + read.len) as usize],
            )
            .unwrap();
        }
        assert!(m.is_complete());
    }

    #[test]
    fn a_merge_read_round_trips_through_serde() {
        let read = MergeRead {
            source: Source::Video,
            offset: 1 << 40,
            len: 12_345,
            sample_count: 7,
        };
        let json = serde_json::to_string(&read).unwrap();
        assert_eq!(serde_json::from_str::<MergeRead>(&json).unwrap(), read);
    }
}
