//! The TS → fMP4 pipeline.
//!
//! One [`Remuxer`] handles one download. Feed it HLS segments in playlist order and it
//! returns the bytes to append to the output file: the init segment plus a fragment on
//! the first call, a fragment on every call after that.
//!
//! Nothing is re-encoded. Samples are copied through unchanged; only their framing
//! changes. That is what makes this fast, lossless, and deterministic.

use serde::{Deserialize, Serialize};

use crate::aac;
use crate::fmp4::{self, Sample, TrackConfig, TrackKind};
use crate::h264;
use crate::ts::{PesPacket, TsDemuxer, TsError, STREAM_TYPE_AAC_ADTS, STREAM_TYPE_H264};

/// MPEG-TS timestamps are always 90 kHz.
const TS_TIMESCALE: u64 = 90_000;
/// Fallback duration for a lone video sample, equivalent to 30 fps.
const DEFAULT_VIDEO_DURATION: u32 = 3000;

const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemuxError {
    Ts(TsError),
    /// The segment contained no stream we can carry into MP4.
    NoTracks,
    /// H.264 was present but its parameter sets never appeared, so no decoder
    /// configuration can be written and the track would be undecodable.
    MissingParameterSets,
}

impl From<TsError> for RemuxError {
    fn from(e: TsError) -> Self {
        RemuxError::Ts(e)
    }
}

impl core::fmt::Display for RemuxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RemuxError::Ts(e) => write!(f, "{e}"),
            RemuxError::NoTracks => f.write_str("segment contains no supported audio or video"),
            RemuxError::MissingParameterSets => {
                f.write_str("H.264 stream is missing its SPS/PPS parameter sets")
            }
        }
    }
}

#[derive(Default)]
pub struct Remuxer {
    demux: TsDemuxer,
    /// Drop the video track entirely and mux audio alone.
    ///
    /// This is a genuine extraction rather than a re-encode: the AAC frames are the
    /// same bytes they were in the transport stream, so an audio-only output is
    /// bit-identical to the audio of the full file.
    audio_only: bool,
    init_emitted: bool,
    /// Monotonic across every fragment of the output, regardless of track.
    seq: u32,
    /// The first decode timestamp seen, subtracted from everything downstream.
    ///
    /// HLS segments carry the timestamps they had in the origin stream, which routinely
    /// start at some large arbitrary value. Writing those verbatim produces a file whose
    /// first frame is minutes in, which players render as a long blank lead-in.
    dts_origin: Option<u64>,
    audio_timescale: Option<u32>,
}

/// A video access unit, after Annex B → AVCC conversion.
struct VideoAu {
    data: Vec<u8>,
    dts: u64,
    pts: u64,
    is_sync: bool,
}

/// The carry-over a remuxer needs to continue an output file it did not start.
///
/// Deliberately small. Once the init segment has been written, every subsequent
/// fragment is self-contained — the track configurations are already in the file's
/// `moov` and are never referenced again. What genuinely cannot be rediscovered is
/// the fragment sequence number and the decode-time origin, because both are
/// relative to where the output began rather than to anything in the next segment.
///
/// The demuxer is *not* carried over: every HLS segment starts with its own PAT and
/// PMT, which is what makes segments independently decodable in the first place, so
/// a fresh demuxer re-learns the stream layout from the next segment it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemuxState {
    pub audio_only: bool,
    pub init_emitted: bool,
    pub seq: u32,
    pub dts_origin: Option<u64>,
    pub audio_timescale: Option<u32>,
}

impl Remuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A remuxer that discards video and writes an audio-only file.
    pub fn audio_only() -> Self {
        Self {
            audio_only: true,
            ..Self::default()
        }
    }

    /// Snapshot the carry-over state, for persisting alongside a paused job.
    pub fn state(&self) -> RemuxState {
        RemuxState {
            audio_only: self.audio_only,
            init_emitted: self.init_emitted,
            seq: self.seq,
            dts_origin: self.dts_origin,
            audio_timescale: self.audio_timescale,
        }
    }

    /// Continue an output file that an earlier remuxer started.
    ///
    /// Assumes the next segment belongs to the same rendition — the init segment is
    /// not re-emitted, so feeding segments with different codec parameters would
    /// produce a file whose `moov` disagrees with its fragments.
    pub fn restore(state: &RemuxState) -> Self {
        Self {
            demux: TsDemuxer::new(),
            audio_only: state.audio_only,
            init_emitted: state.init_emitted,
            seq: state.seq,
            dts_origin: state.dts_origin,
            audio_timescale: state.audio_timescale,
        }
    }

    /// Feed one segment; get back the bytes to append to the output.
    pub fn push_ts_segment(&mut self, ts: &[u8]) -> Result<Vec<u8>, RemuxError> {
        let pes = self.demux.push(ts)?;
        if pes.is_empty() {
            return Err(RemuxError::NoTracks);
        }

        let video: Vec<&PesPacket> = pes
            .iter()
            .filter(|p| p.stream_type == STREAM_TYPE_H264)
            .collect();
        let audio: Vec<&PesPacket> = pes
            .iter()
            .filter(|p| p.stream_type == STREAM_TYPE_AAC_ADTS)
            .collect();

        // Collected before the audio-only check so the error paths below still
        // distinguish "no video in this stream" from "video deliberately dropped".
        let aus = if self.audio_only {
            Vec::new()
        } else {
            self.collect_video_aus(&video)
        };
        let (audio_samples, audio_cfg) = self.collect_audio(&audio);

        if aus.is_empty() && audio_samples.is_empty() {
            return Err(RemuxError::NoTracks);
        }

        // Anchor the timeline on the first sample we ever see, whichever track it is on.
        if self.dts_origin.is_none() {
            let first = aus
                .first()
                .map(|a| a.dts)
                .into_iter()
                .chain(audio.first().and_then(|p| p.dts.or(p.pts)))
                .min();
            self.dts_origin = Some(first.unwrap_or(0));
        }
        let origin = self.dts_origin.unwrap_or(0);

        let mut out = Vec::new();
        if !self.init_emitted {
            let mut tracks = Vec::new();
            if !aus.is_empty() {
                tracks.push(self.video_track_config(&video)?);
            }
            if let Some(cfg) = &audio_cfg {
                self.audio_timescale = Some(cfg.timescale);
                tracks.push(cfg.clone());
            }
            if tracks.is_empty() {
                return Err(RemuxError::NoTracks);
            }
            out.extend(fmp4::write_init_segment(&tracks));
            self.init_emitted = true;
        }

        if !aus.is_empty() {
            let base = aus[0].dts.saturating_sub(origin);
            let samples = video_samples(&aus);
            self.seq += 1;
            out.extend(fmp4::write_fragment(
                self.seq,
                VIDEO_TRACK_ID,
                base,
                &samples,
            ));
        }

        if !audio_samples.is_empty() {
            let timescale = u64::from(self.audio_timescale.unwrap_or(44_100));
            let first_pts = audio
                .first()
                .and_then(|p| p.dts.or(p.pts))
                .unwrap_or(origin)
                .saturating_sub(origin);
            // Rescale from the transport stream's 90 kHz clock into the audio track's
            // own timescale, in 128-bit to keep a long stream from overflowing.
            let base =
                (u128::from(first_pts) * u128::from(timescale) / u128::from(TS_TIMESCALE)) as u64;
            self.seq += 1;
            out.extend(fmp4::write_fragment(
                self.seq,
                AUDIO_TRACK_ID,
                base,
                &audio_samples,
            ));
        }

        Ok(out)
    }

    /// True once an init segment has been written — the caller uses this to decide
    /// whether the output so far is playable.
    pub fn has_init(&self) -> bool {
        self.init_emitted
    }

    fn collect_video_aus(&self, video: &[&PesPacket]) -> Vec<VideoAu> {
        video
            .iter()
            .filter(|p| h264::has_slice(&p.data))
            .map(|p| {
                let dts = p.dts.or(p.pts).unwrap_or(0);
                VideoAu {
                    data: h264::annexb_to_avcc(&p.data),
                    dts,
                    pts: p.pts.unwrap_or(dts),
                    is_sync: h264::is_keyframe(&p.data),
                }
            })
            .filter(|au| !au.data.is_empty())
            .collect()
    }

    fn video_track_config(&self, video: &[&PesPacket]) -> Result<TrackConfig, RemuxError> {
        let mut sps = None;
        let mut pps = None;
        for p in video {
            let (s, pp) = h264::find_parameter_sets(&p.data);
            if sps.is_none() {
                sps = s;
            }
            if pps.is_none() {
                pps = pp;
            }
            if sps.is_some() && pps.is_some() {
                break;
            }
        }
        let (Some(sps), Some(pps)) = (sps, pps) else {
            return Err(RemuxError::MissingParameterSets);
        };
        let (width, height) = h264::sps_resolution(&sps).unwrap_or((1280, 720));
        Ok(TrackConfig {
            track_id: VIDEO_TRACK_ID,
            timescale: TS_TIMESCALE as u32,
            kind: TrackKind::Video {
                width,
                height,
                avcc: h264::build_avcc(&sps, &pps),
            },
        })
    }

    /// Strip ADTS headers, one MP4 sample per AAC frame.
    fn collect_audio(&self, audio: &[&PesPacket]) -> (Vec<Sample>, Option<TrackConfig>) {
        let mut samples = Vec::new();
        let mut cfg = None;
        for p in audio {
            for frame in aac::parse_adts(&p.data) {
                if cfg.is_none() {
                    cfg = Some(fmp4::audio_track(
                        AUDIO_TRACK_ID,
                        frame.channels,
                        frame.sample_rate,
                        frame.object_type,
                    ));
                }
                samples.push(Sample {
                    data: p.data[frame.payload_range.0..frame.payload_range.1].to_vec(),
                    // AAC-LC is always 1024 samples per frame, in the track's own
                    // timescale — which is the sample rate, so this needs no scaling.
                    duration: aac::SAMPLES_PER_FRAME,
                    is_sync: true, // every AAC frame is independently decodable
                    cts_offset: 0,
                });
            }
        }
        (samples, cfg)
    }
}

/// Turn access units into MP4 samples, deriving each duration from the gap to the next.
fn video_samples(aus: &[VideoAu]) -> Vec<Sample> {
    let mut samples = Vec::with_capacity(aus.len());
    for (i, au) in aus.iter().enumerate() {
        let duration = match aus.get(i + 1) {
            Some(next) => (next.dts.saturating_sub(au.dts)) as u32,
            // The final sample has no successor to measure against; reuse the previous
            // gap so the track does not end with a zero-length frame.
            None => samples
                .last()
                .map(|s: &Sample| s.duration)
                .unwrap_or(DEFAULT_VIDEO_DURATION),
        };
        samples.push(Sample {
            data: au.data.clone(),
            duration: if duration == 0 {
                DEFAULT_VIDEO_DURATION
            } else {
                duration
            },
            is_sync: au.is_sync,
            cts_offset: (au.pts as i64 - au.dts as i64) as i32,
        });
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::builder::adts_frame;
    use crate::fixtures::{delta, ts_segment as fixture_ts_segment};
    use crate::fmp4::inspect::*;
    use crate::ts::builder::*;
    use crate::ts::{STREAM_TYPE_AAC_ADTS, STREAM_TYPE_H264};

    #[test]
    fn first_segment_emits_init_then_fragments() {
        let mut r = Remuxer::new();
        let out = r.push_ts_segment(&fixture_ts_segment(900_000)).unwrap();
        let types = box_types(&out);
        assert_eq!(&types[..2], &["ftyp", "moov"]);
        assert!(types.contains(&"moof".to_string()));
        assert!(types.contains(&"mdat".to_string()));
        assert!(r.has_init());
    }

    #[test]
    fn init_segment_describes_both_tracks_with_real_dimensions() {
        let mut r = Remuxer::new();
        let out = r.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        assert!(contains_box(&out, "avcC"));
        assert!(contains_box(&out, "esds"));
        let tkhd = find_box(&out, "tkhd").unwrap();
        let w = u32::from_be_bytes(tkhd[76..80].try_into().unwrap()) >> 16;
        let h = u32::from_be_bytes(tkhd[80..84].try_into().unwrap()) >> 16;
        assert_eq!((w, h), (640, 360), "dimensions must come from the real SPS");
    }

    #[test]
    fn later_segments_emit_only_fragments() {
        let mut r = Remuxer::new();
        r.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        let out = r.push_ts_segment(&fixture_ts_segment(6000)).unwrap();
        assert_eq!(box_types(&out), vec!["moof", "mdat", "moof", "mdat"]);
    }

    #[test]
    fn sequence_numbers_advance_monotonically() {
        let mut r = Remuxer::new();
        r.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        let a = read_mfhd_sequence(&r.push_ts_segment(&fixture_ts_segment(6000)).unwrap());
        let b = read_mfhd_sequence(&r.push_ts_segment(&fixture_ts_segment(12000)).unwrap());
        assert!(b > a, "fragment sequence must increase ({a} then {b})");
    }

    #[test]
    fn the_output_timeline_starts_at_zero_regardless_of_the_source_timeline() {
        // A stream whose timestamps begin 10 seconds in must still produce a file whose
        // first frame is at t=0, or players show a long blank lead-in.
        let mut r = Remuxer::new();
        let out = r.push_ts_segment(&fixture_ts_segment(900_000)).unwrap();
        let tfdt = find_box(&out, "tfdt").unwrap();
        assert_eq!(u64::from_be_bytes(tfdt[4..12].try_into().unwrap()), 0);
    }

    #[test]
    fn later_fragments_carry_the_elapsed_decode_time() {
        let mut r = Remuxer::new();
        r.push_ts_segment(&fixture_ts_segment(900_000)).unwrap();
        let out = r.push_ts_segment(&fixture_ts_segment(906_000)).unwrap();
        let tfdt = find_box(&out, "tfdt").unwrap();
        assert_eq!(
            u64::from_be_bytes(tfdt[4..12].try_into().unwrap()),
            6000,
            "second segment starts 6000 ticks after the first"
        );
    }

    #[test]
    fn video_sample_durations_come_from_the_timestamp_gaps() {
        let mut r = Remuxer::new();
        let out = r.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        let trun = find_box(&out, "trun").unwrap();
        let first_duration = u32::from_be_bytes(trun[12..16].try_into().unwrap());
        assert_eq!(first_duration, 3000);
    }

    #[test]
    fn the_keyframe_is_marked_sync_and_the_delta_frame_is_not() {
        let mut r = Remuxer::new();
        let out = r.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        let trun = find_box(&out, "trun").unwrap();
        let first_flags = u32::from_be_bytes(trun[20..24].try_into().unwrap());
        let second_flags = u32::from_be_bytes(trun[36..40].try_into().unwrap());
        assert_ne!(first_flags, second_flags);
    }

    #[test]
    fn audio_samples_have_their_adts_headers_stripped() {
        let mut r = Remuxer::new();
        let out = r.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        // The audio fragment is the second moof/mdat pair.
        let sizes = box_types(&out);
        assert!(sizes.contains(&"mdat".to_string()));
        // Raw payloads were 4 bytes each; with headers they would be 11.
        let audio_fragment = &out[fragment_offsets(&out)[1]..];
        let trun = find_box(audio_fragment, "trun").unwrap();
        let sample_size = u32::from_be_bytes(trun[16..20].try_into().unwrap());
        assert_eq!(sample_size, 4, "ADTS header must not reach the mdat");
    }

    /// Byte offsets of each `moof` in the buffer.
    fn fragment_offsets(buf: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            if &buf[i + 4..i + 8] == b"moof" {
                out.push(i);
            }
            if size < 8 {
                break;
            }
            i += size;
        }
        out
    }

    /// The whole point of `RemuxState`: an interrupted download that resumes must
    /// produce the same file as one that never stopped. If the carried-over state is
    /// missing anything, the seam shows up here as diverging bytes — a duplicated init
    /// segment, a repeated fragment sequence number, or a decode time that jumps back
    /// to zero mid-file.
    #[test]
    fn a_resumed_remux_continues_the_output_exactly() {
        let uninterrupted = {
            let mut r = Remuxer::new();
            let mut out = Vec::new();
            for i in 0..3u64 {
                out.extend(r.push_ts_segment(&fixture_ts_segment(i * 6000)).unwrap());
            }
            out
        };

        let resumed = {
            let mut first = Remuxer::new();
            let mut out = Vec::new();
            for i in 0..2u64 {
                out.extend(
                    first
                        .push_ts_segment(&fixture_ts_segment(i * 6000))
                        .unwrap(),
                );
            }
            // Persist, throw the remuxer away, rebuild from the snapshot alone.
            let snapshot = serde_json::to_string(&first.state()).unwrap();
            drop(first);
            let state: RemuxState = serde_json::from_str(&snapshot).unwrap();
            let mut second = Remuxer::restore(&state);
            out.extend(
                second
                    .push_ts_segment(&fixture_ts_segment(2 * 6000))
                    .unwrap(),
            );
            out
        };

        assert_eq!(uninterrupted, resumed);
    }

    #[test]
    fn audio_only_mode_drops_the_video_track_entirely() {
        let mut r = Remuxer::audio_only();
        let out = r.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        assert!(contains_box(&out, "esds"), "audio must survive");
        assert!(!contains_box(&out, "avcC"), "video must not be described");
        // One moof/mdat pair, not two: video contributes no fragment at all.
        assert_eq!(
            box_types(&out).iter().filter(|t| *t == "moof").count(),
            1,
            "a dropped video track must not still emit fragments"
        );
    }

    #[test]
    fn audio_only_extraction_is_bit_identical_to_the_full_files_audio() {
        // Nothing is re-encoded, so the AAC payloads must match byte for byte.
        let full = {
            let mut r = Remuxer::new();
            r.push_ts_segment(&fixture_ts_segment(0)).unwrap()
        };
        let audio = {
            let mut r = Remuxer::audio_only();
            r.push_ts_segment(&fixture_ts_segment(0)).unwrap()
        };
        // The audio fragment is the last moof/mdat pair of the full output.
        let full_audio_mdat = last_mdat_payload(&full);
        let audio_mdat = last_mdat_payload(&audio);
        assert_eq!(full_audio_mdat, audio_mdat);
    }

    /// Payload of the final `mdat` in a buffer.
    fn last_mdat_payload(buf: &[u8]) -> Vec<u8> {
        let mut found = Vec::new();
        let mut i = 0;
        while i + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            if &buf[i + 4..i + 8] == b"mdat" {
                found = buf[i + 8..i + size].to_vec();
            }
            if size < 8 {
                break;
            }
            i += size;
        }
        found
    }

    #[test]
    fn audio_only_survives_a_state_round_trip() {
        let mut first = Remuxer::audio_only();
        first.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        let restored = Remuxer::restore(&first.state());
        assert!(
            restored.state().audio_only,
            "losing the audio-only flag on resume would start writing video mid-file"
        );
    }

    #[test]
    fn a_resumed_remux_does_not_repeat_the_init_segment() {
        let mut first = Remuxer::new();
        first.push_ts_segment(&fixture_ts_segment(0)).unwrap();
        let mut second = Remuxer::restore(&first.state());
        let out = second.push_ts_segment(&fixture_ts_segment(6000)).unwrap();
        assert!(
            !box_types(&out).contains(&"ftyp".to_string()),
            "a second ftyp/moov mid-file makes the output unplayable"
        );
    }

    #[test]
    fn concatenated_output_is_byte_identical_across_runs() {
        let run = || {
            let mut r = Remuxer::new();
            let mut o = Vec::new();
            for i in 0..3u64 {
                o.extend(r.push_ts_segment(&fixture_ts_segment(i * 6000)).unwrap());
            }
            o
        };
        assert_eq!(run(), run(), "remux must be deterministic");
    }

    #[test]
    fn every_box_in_a_full_remux_has_a_correct_size() {
        let mut r = Remuxer::new();
        let mut out = Vec::new();
        for i in 0..3u64 {
            out.extend(r.push_ts_segment(&fixture_ts_segment(i * 6000)).unwrap());
        }
        assert!(walk_and_validate_all_box_sizes(&out));
    }

    #[test]
    fn a_segment_with_no_supported_stream_is_an_error_not_an_empty_file() {
        let mut r = Remuxer::new();
        let mut only_tables = ts_packet(0, true, 0, &pat());
        only_tables.extend(ts_packet(4096, true, 0, &pmt(&[])));
        assert_eq!(r.push_ts_segment(&only_tables), Err(RemuxError::NoTracks));
    }

    #[test]
    fn h264_without_parameter_sets_is_refused() {
        let mut out = ts_packet(0, true, 0, &pat());
        out.extend(ts_packet(
            4096,
            true,
            0,
            &pmt(&[(VIDEO_PID, STREAM_TYPE_H264)]),
        ));
        out.extend(pes_to_packets(VIDEO_PID, &pes(0xE0, 0, None, &delta(0x33))));
        assert_eq!(
            Remuxer::new().push_ts_segment(&out),
            Err(RemuxError::MissingParameterSets)
        );
    }

    #[test]
    fn an_audio_only_segment_produces_an_audio_only_file() {
        let mut out = ts_packet(0, true, 0, &pat());
        out.extend(ts_packet(
            4096,
            true,
            0,
            &pmt(&[(AUDIO_PID, STREAM_TYPE_AAC_ADTS)]),
        ));
        out.extend(pes_to_packets(
            AUDIO_PID,
            &pes(0xC0, 0, None, &adts_frame(2, 4, 2, &[1, 2, 3, 4])),
        ));
        let result = Remuxer::new().push_ts_segment(&out).unwrap();
        assert!(contains_box(&result, "esds"));
        assert!(!contains_box(&result, "avcC"));
    }
}
