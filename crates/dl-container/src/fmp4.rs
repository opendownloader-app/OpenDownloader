//! Fragmented MP4 writing.
//!
//! Progressive MP4 puts a `moov` describing every sample at the end of the file (or
//! requires a second pass to move it to the front). Either way it needs a backwards seek
//! once the total sample count is known. Fragmented MP4 does not: an init segment states
//! the tracks, and each fragment carries its own sample table. That makes the whole
//! output append-only, which is what lets the same code path serve a seeking sink on
//! Chrome and a non-seeking one on Firefox.
//!
//! Everything here is deterministic. No timestamps are written (creation and
//! modification times are fixed at zero), no identifiers are generated, and nothing
//! iterates a hash map.

use crate::aac;

/// Movie timescale. Track timescales are per-track; this one only scales `mvhd`.
const MOVIE_TIMESCALE: u32 = 1000;

/// The 3x3 unity transformation matrix, in 16.16 / 2.30 fixed point.
const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// Sample flags for a sync sample: depends on nothing, is not a non-sync sample.
const FLAGS_SYNC: u32 = 0x0200_0000;
/// Sample flags for a delta sample: depends on others, is a non-sync sample.
const FLAGS_NON_SYNC: u32 = 0x0101_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackKind {
    Video {
        width: u32,
        height: u32,
        /// `AVCDecoderConfigurationRecord`, from `h264::build_avcc`.
        avcc: Vec<u8>,
    },
    Audio {
        channels: u8,
        sample_rate: u32,
        /// `AudioSpecificConfig`, from `aac::audio_specific_config`.
        asc: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackConfig {
    pub track_id: u32,
    pub timescale: u32,
    pub kind: TrackKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub data: Vec<u8>,
    pub duration: u32,
    pub is_sync: bool,
    /// Composition-time offset (PTS − DTS), in track timescale units. Signed, because
    /// B-frames legitimately present before they decode.
    pub cts_offset: i32,
}

/// Wrap a body in a box header.
pub(crate) fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(body);
    out
}

/// Wrap a body in a full-box header (box header plus version and flags).
pub(crate) fn full_bx(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + body.len());
    inner.push(version);
    inner.extend_from_slice(&flags.to_be_bytes()[1..]); // flags is 24-bit
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

/// Build the init segment: `ftyp` followed by `moov`.
pub fn write_init_segment(tracks: &[TrackConfig]) -> Vec<u8> {
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"iso5");
    ftyp_body.extend_from_slice(&512u32.to_be_bytes());
    for brand in [b"iso5", b"iso6", b"mp41", b"avc1", b"dash"] {
        ftyp_body.extend_from_slice(brand);
    }

    let mut moov_parts = vec![mvhd(tracks)];
    for t in tracks {
        moov_parts.push(trak(t));
    }
    moov_parts.push(mvex(tracks));

    concat(&[bx(b"ftyp", &ftyp_body), bx(b"moov", &concat(&moov_parts))])
}

fn mvhd(tracks: &[TrackConfig]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time — fixed for determinism
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // duration: unknown in a fragmented file
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&[0u8; 8]); // reserved
    for v in UNITY_MATRIX {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&[0u8; 24]); // pre_defined
    let next_track_id = tracks.iter().map(|t| t.track_id).max().unwrap_or(0) + 1;
    b.extend_from_slice(&next_track_id.to_be_bytes());
    full_bx(b"mvhd", 0, 0, &b)
}

fn trak(t: &TrackConfig) -> Vec<u8> {
    bx(b"trak", &concat(&[tkhd(t), mdia(t)]))
}

fn tkhd(t: &TrackConfig) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&t.track_id.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // reserved
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&0u16.to_be_bytes()); // layer
    b.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    let volume: u16 = match t.kind {
        TrackKind::Audio { .. } => 0x0100,
        TrackKind::Video { .. } => 0,
    };
    b.extend_from_slice(&volume.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    for v in UNITY_MATRIX {
        b.extend_from_slice(&v.to_be_bytes());
    }
    // Width and height are 16.16 fixed point, and must be real: a track declaring 0x0
    // renders as a blank rectangle in most players even when the bitstream is intact.
    let (w, h) = match t.kind {
        TrackKind::Video { width, height, .. } => (width, height),
        TrackKind::Audio { .. } => (0, 0),
    };
    b.extend_from_slice(&(w << 16).to_be_bytes());
    b.extend_from_slice(&(h << 16).to_be_bytes());
    // flags 7 = track_enabled | track_in_movie | track_in_preview
    full_bx(b"tkhd", 0, 7, &b)
}

fn mdia(t: &TrackConfig) -> Vec<u8> {
    bx(b"mdia", &concat(&[mdhd(t), hdlr(t), minf(t)]))
}

fn mdhd(t: &TrackConfig) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&t.timescale.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
    b.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    full_bx(b"mdhd", 0, 0, &b)
}

fn hdlr(t: &TrackConfig) -> Vec<u8> {
    let (handler, name): (&[u8; 4], &[u8]) = match t.kind {
        TrackKind::Video { .. } => (b"vide", b"VideoHandler\0"),
        TrackKind::Audio { .. } => (b"soun", b"SoundHandler\0"),
    };
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    b.extend_from_slice(handler);
    b.extend_from_slice(&[0u8; 12]); // reserved
    b.extend_from_slice(name);
    full_bx(b"hdlr", 0, 0, &b)
}

fn minf(t: &TrackConfig) -> Vec<u8> {
    let header = match t.kind {
        // vmhd flags must be 1; a zero there is rejected by strict parsers.
        TrackKind::Video { .. } => full_bx(b"vmhd", 0, 1, &[0u8; 8]),
        TrackKind::Audio { .. } => full_bx(b"smhd", 0, 0, &[0, 0, 0, 0]),
    };
    // dref with a single self-contained entry: the media lives in this same file.
    let url = full_bx(b"url ", 0, 1, &[]);
    let mut dref_body = 1u32.to_be_bytes().to_vec();
    dref_body.extend_from_slice(&url);
    let dinf = bx(b"dinf", &full_bx(b"dref", 0, 0, &dref_body));
    bx(b"minf", &concat(&[header, dinf, stbl(t)]))
}

fn stbl(t: &TrackConfig) -> Vec<u8> {
    // The sample tables are deliberately empty: in a fragmented file every sample is
    // described by its fragment's `trun`, not here.
    let empty_entries = full_bx(b"stts", 0, 0, &0u32.to_be_bytes());
    let stsc = full_bx(b"stsc", 0, 0, &0u32.to_be_bytes());
    let mut stsz_body = 0u32.to_be_bytes().to_vec(); // sample_size 0 = per-sample sizes
    stsz_body.extend_from_slice(&0u32.to_be_bytes()); // sample_count
    let stsz = full_bx(b"stsz", 0, 0, &stsz_body);
    let stco = full_bx(b"stco", 0, 0, &0u32.to_be_bytes());
    bx(
        b"stbl",
        &concat(&[stsd(t), empty_entries, stsc, stsz, stco]),
    )
}

fn stsd(t: &TrackConfig) -> Vec<u8> {
    let entry = match &t.kind {
        TrackKind::Video {
            width,
            height,
            avcc,
        } => {
            let mut e = Vec::new();
            e.extend_from_slice(&[0u8; 6]); // reserved
            e.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
            e.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
            e.extend_from_slice(&(*width as u16).to_be_bytes());
            e.extend_from_slice(&(*height as u16).to_be_bytes());
            e.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution 72dpi
            e.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution
            e.extend_from_slice(&0u32.to_be_bytes()); // reserved
            e.extend_from_slice(&1u16.to_be_bytes()); // frame_count
            e.extend_from_slice(&[0u8; 32]); // compressorname
            e.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
            e.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
            e.extend_from_slice(&bx(b"avcC", avcc));
            bx(b"avc1", &e)
        }
        TrackKind::Audio {
            channels,
            sample_rate,
            asc,
        } => {
            let mut e = Vec::new();
            e.extend_from_slice(&[0u8; 6]);
            e.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
            e.extend_from_slice(&[0u8; 8]); // version, revision, vendor
            e.extend_from_slice(&u16::from(*channels).to_be_bytes());
            e.extend_from_slice(&16u16.to_be_bytes()); // samplesize
            e.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
            e.extend_from_slice(&0u16.to_be_bytes()); // reserved
                                                      // 16.16 fixed point; rates above 65535 cannot be expressed and are clamped
                                                      // by the format itself, which is why AAC at 96 kHz uses the esds rate.
            e.extend_from_slice(&(sample_rate << 16).to_be_bytes());
            e.extend_from_slice(&esds(t.track_id, asc));
            bx(b"mp4a", &e)
        }
    };
    let mut body = 1u32.to_be_bytes().to_vec(); // entry_count
    body.extend_from_slice(&entry);
    full_bx(b"stsd", 0, 0, &body)
}

/// The MPEG-4 elementary stream descriptor that carries the `AudioSpecificConfig`.
fn esds(track_id: u32, asc: &[u8]) -> Vec<u8> {
    // DecoderSpecificInfo
    let mut dsi = vec![0x05, asc.len() as u8];
    dsi.extend_from_slice(asc);

    // DecoderConfigDescriptor
    let mut dcd_body = vec![
        0x40, // objectTypeIndication: MPEG-4 Audio
        0x15, // streamType 5 (audio) << 2 | upStream 0 | reserved 1
        0x00, 0x00, 0x00, // bufferSizeDB
    ];
    dcd_body.extend_from_slice(&0u32.to_be_bytes()); // maxBitrate
    dcd_body.extend_from_slice(&0u32.to_be_bytes()); // avgBitrate
    dcd_body.extend_from_slice(&dsi);
    let mut dcd = vec![0x04, dcd_body.len() as u8];
    dcd.extend_from_slice(&dcd_body);

    // ES_Descriptor
    let mut es_body = Vec::new();
    es_body.extend_from_slice(&(track_id as u16).to_be_bytes()); // ES_ID
    es_body.push(0x00); // no dependency, no URL, no OCR, priority 0
    es_body.extend_from_slice(&dcd);
    es_body.extend_from_slice(&[0x06, 0x01, 0x02]); // SLConfigDescriptor: MP4 default
    let mut es = vec![0x03, es_body.len() as u8];
    es.extend_from_slice(&es_body);

    full_bx(b"esds", 0, 0, &es)
}

/// `mvex` announces that fragments follow. Without it, a player reads the empty sample
/// tables in `stbl`, concludes the file has no samples, and plays nothing.
fn mvex(tracks: &[TrackConfig]) -> Vec<u8> {
    let mut parts = Vec::new();
    for t in tracks {
        let mut b = Vec::new();
        b.extend_from_slice(&t.track_id.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
        b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_duration
        b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
        b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
        parts.push(full_bx(b"trex", 0, 0, &b));
    }
    bx(b"mvex", &concat(&parts))
}

/// Build one fragment: `moof` followed by `mdat`.
///
/// `base_decode_time` is this fragment's first sample's decode time in the track's own
/// timescale, and it is what keeps audio and video aligned across segment boundaries.
pub fn write_fragment(
    seq: u32,
    track_id: u32,
    base_decode_time: u64,
    samples: &[Sample],
) -> Vec<u8> {
    let mfhd = full_bx(b"mfhd", 0, 0, &seq.to_be_bytes());

    // default-base-is-moof: sample offsets are relative to this moof, not to the file.
    // Anything else breaks the moment a fragment is served from a different byte offset
    // than the one it was written at.
    let tfhd = full_bx(b"tfhd", 0, 0x02_0000, &track_id.to_be_bytes());
    let tfdt = full_bx(b"tfdt", 1, 0, &base_decode_time.to_be_bytes());

    // data-offset | sample-duration | sample-size | sample-flags | sample-cts-offset
    const TRUN_FLAGS: u32 = 0x0001 | 0x0100 | 0x0200 | 0x0400 | 0x0800;
    let mut trun_body = Vec::with_capacity(8 + 16 * samples.len());
    trun_body.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    trun_body.extend_from_slice(&0i32.to_be_bytes()); // data_offset placeholder
    for s in samples {
        trun_body.extend_from_slice(&s.duration.to_be_bytes());
        trun_body.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
        trun_body.extend_from_slice(
            &if s.is_sync {
                FLAGS_SYNC
            } else {
                FLAGS_NON_SYNC
            }
            .to_be_bytes(),
        );
        trun_body.extend_from_slice(&s.cts_offset.to_be_bytes());
    }
    // Version 1 makes the composition offset signed, which B-frames require.
    let trun = full_bx(b"trun", 1, TRUN_FLAGS, &trun_body);

    let traf = bx(b"traf", &concat(&[tfhd, tfdt, trun]));
    let mut moof = bx(b"moof", &concat(&[mfhd, traf]));

    // The single most common way to produce an fMP4 that no player will touch is to get
    // this wrong. data_offset is measured from the start of the moof, and the samples
    // begin immediately after the mdat box header.
    let data_offset = (moof.len() + 8) as i32;
    let offset_pos = moof.len() - 16 * samples.len() - 4;
    moof[offset_pos..offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());

    let payload_len: usize = samples.iter().map(|s| s.data.len()).sum();
    let mut out = Vec::with_capacity(moof.len() + 8 + payload_len);
    out.extend_from_slice(&moof);
    out.extend_from_slice(&((8 + payload_len) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for s in samples {
        out.extend_from_slice(&s.data);
    }
    out
}

/// Convenience for callers that only have an ADTS sample rate to hand.
pub fn audio_track(track_id: u32, channels: u8, sample_rate: u32, object_type: u8) -> TrackConfig {
    TrackConfig {
        track_id,
        timescale: sample_rate,
        kind: TrackKind::Audio {
            channels,
            sample_rate,
            asc: aac::audio_specific_config(object_type, sample_rate, channels),
        },
    }
}

#[cfg(test)]
pub(crate) mod inspect {
    //! Box-walking helpers. Shared with the remux tests, which assert on the same
    //! structure from one level up.

    /// The type codes of the top-level boxes, in order.
    pub fn box_types(buf: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            out.push(String::from_utf8_lossy(&buf[i + 4..i + 8]).to_string());
            if size < 8 {
                break;
            }
            i += size;
        }
        out
    }

    /// Depth-first search for a box type anywhere in the tree.
    pub fn find_box<'a>(buf: &'a [u8], want: &str) -> Option<&'a [u8]> {
        const CONTAINERS: &[&str] = &[
            "moov", "trak", "mdia", "minf", "stbl", "dinf", "mvex", "moof", "traf", "stsd", "avc1",
            "mp4a",
        ];
        let mut i = 0;
        while i + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            if size < 8 || i + size > buf.len() {
                return None;
            }
            let typ = String::from_utf8_lossy(&buf[i + 4..i + 8]).to_string();
            let body = &buf[i + 8..i + size];
            if typ == want {
                return Some(body);
            }
            if CONTAINERS.contains(&typ.as_str()) {
                // Sample entries and full-box containers carry a fixed preamble before
                // their child boxes; descending without skipping it reads the preamble
                // as a box header and finds nothing.
                let skip = match typ.as_str() {
                    "avc1" => 78,
                    "mp4a" => 28,
                    "stsd" => 8, // version+flags, entry_count
                    _ => 0,
                };
                if body.len() > skip {
                    if let Some(found) = find_box(&body[skip..], want) {
                        return Some(found);
                    }
                }
            }
            i += size;
        }
        None
    }

    pub fn contains_box(buf: &[u8], want: &str) -> bool {
        find_box(buf, want).is_some()
    }

    /// Read the `data_offset` field out of the fragment's `trun`.
    pub fn read_trun_data_offset(fragment: &[u8]) -> i32 {
        let trun = find_box(fragment, "trun").expect("trun must exist");
        // version+flags(4), sample_count(4), then data_offset.
        i32::from_be_bytes(trun[8..12].try_into().unwrap())
    }

    pub fn read_mfhd_sequence(fragment: &[u8]) -> u32 {
        let mfhd = find_box(fragment, "mfhd").expect("mfhd must exist");
        u32::from_be_bytes(mfhd[4..8].try_into().unwrap())
    }

    /// Recursively confirm every box's declared size matches its actual extent. A
    /// mismatch anywhere makes the file unparseable, and is invisible to a test that
    /// only checks the boxes it expects are present.
    pub fn walk_and_validate_all_box_sizes(buf: &[u8]) -> bool {
        const CONTAINERS: &[&str] = &[
            "moov", "trak", "mdia", "minf", "stbl", "dinf", "mvex", "moof", "traf",
        ];
        let mut i = 0;
        while i < buf.len() {
            if i + 8 > buf.len() {
                return false;
            }
            let size = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            if size < 8 || i + size > buf.len() {
                return false;
            }
            let typ = String::from_utf8_lossy(&buf[i + 4..i + 8]).to_string();
            if CONTAINERS.contains(&typ.as_str())
                && !walk_and_validate_all_box_sizes(&buf[i + 8..i + size])
            {
                return false;
            }
            i += size;
        }
        i == buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::inspect::*;
    use super::*;

    fn video_track() -> TrackConfig {
        TrackConfig {
            track_id: 1,
            timescale: 90_000,
            kind: TrackKind::Video {
                width: 640,
                height: 360,
                avcc: crate::h264::build_avcc(&[0x67, 0x42, 0xC0, 0x1E], &[0x68, 0xCE]),
            },
        }
    }

    fn sample(len: usize) -> Sample {
        Sample {
            data: vec![0xAB; len],
            duration: 3000,
            is_sync: true,
            cts_offset: 0,
        }
    }

    #[test]
    fn init_segment_has_ftyp_then_moov() {
        let init = write_init_segment(&[video_track()]);
        assert_eq!(box_types(&init), vec!["ftyp", "moov"]);
        assert!(
            contains_box(&init, "mvex"),
            "no mvex means no fragments play"
        );
        assert!(contains_box(&init, "trex"));
        assert!(contains_box(&init, "avcC"));
        assert!(contains_box(&init, "vmhd"));
    }

    #[test]
    fn audio_init_segment_carries_an_esds() {
        let init = write_init_segment(&[audio_track(2, 2, 44100, 2)]);
        assert!(contains_box(&init, "mp4a"));
        assert!(contains_box(&init, "esds"));
        assert!(contains_box(&init, "smhd"));
    }

    #[test]
    fn track_dimensions_reach_both_tkhd_and_the_sample_entry() {
        let init = write_init_segment(&[video_track()]);
        let tkhd = find_box(&init, "tkhd").unwrap();
        // version+flags(4) + 4+4+4+4+4 + 8 + 2+2+2+2 + 36 = 76 bytes before width.
        let w = u32::from_be_bytes(tkhd[76..80].try_into().unwrap());
        let h = u32::from_be_bytes(tkhd[80..84].try_into().unwrap());
        assert_eq!(w >> 16, 640);
        assert_eq!(h >> 16, 360);
    }

    #[test]
    fn fragment_is_moof_then_mdat() {
        let f = write_fragment(1, 1, 0, &[sample(100), sample(100)]);
        assert_eq!(box_types(&f), vec!["moof", "mdat"]);
    }

    #[test]
    fn data_offset_points_at_the_first_byte_of_mdat_payload() {
        for n in 1..8 {
            let samples: Vec<Sample> = (0..n).map(|_| sample(50)).collect();
            let f = write_fragment(1, 1, 0, &samples);
            let moof_size = u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize;
            let data_offset = read_trun_data_offset(&f) as usize;
            assert_eq!(
                data_offset,
                moof_size + 8,
                "data_offset must skip the moof and the mdat header ({n} samples)"
            );
            // And the bytes actually there must be the first sample.
            assert_eq!(f[data_offset], 0xAB);
        }
    }

    #[test]
    fn mdat_length_matches_the_samples_it_contains() {
        let f = write_fragment(1, 1, 0, &[sample(10), sample(20), sample(30)]);
        let moof_size = u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize;
        let mdat_size =
            u32::from_be_bytes(f[moof_size..moof_size + 4].try_into().unwrap()) as usize;
        assert_eq!(mdat_size, 8 + 60);
        assert_eq!(f.len(), moof_size + mdat_size);
    }

    #[test]
    fn base_decode_time_and_sequence_are_written_verbatim() {
        let f = write_fragment(7, 1, 123_456, &[sample(10)]);
        assert_eq!(read_mfhd_sequence(&f), 7);
        let tfdt = find_box(&f, "tfdt").unwrap();
        assert_eq!(u64::from_be_bytes(tfdt[4..12].try_into().unwrap()), 123_456);
    }

    #[test]
    fn sync_and_delta_samples_get_different_flags() {
        let mut delta = sample(10);
        delta.is_sync = false;
        let f = write_fragment(1, 1, 0, &[sample(10), delta]);
        let trun = find_box(&f, "trun").unwrap();
        // version+flags(4) + sample_count(4) + data_offset(4) = 12, then 16 per sample.
        let first_flags = u32::from_be_bytes(trun[12 + 8..12 + 12].try_into().unwrap());
        let second_flags = u32::from_be_bytes(trun[12 + 16 + 8..12 + 16 + 12].try_into().unwrap());
        assert_eq!(first_flags, FLAGS_SYNC);
        assert_eq!(second_flags, FLAGS_NON_SYNC);
    }

    #[test]
    fn negative_composition_offsets_survive_the_round_trip() {
        let mut s = sample(10);
        s.cts_offset = -3000;
        let f = write_fragment(1, 1, 0, &[s]);
        let trun = find_box(&f, "trun").unwrap();
        let cts = i32::from_be_bytes(trun[12 + 12..12 + 16].try_into().unwrap());
        assert_eq!(cts, -3000, "B-frames need signed composition offsets");
    }

    #[test]
    fn output_is_byte_identical_across_runs() {
        assert_eq!(
            write_init_segment(&[video_track()]),
            write_init_segment(&[video_track()])
        );
        assert_eq!(
            write_fragment(3, 1, 90_000, &[sample(64)]),
            write_fragment(3, 1, 90_000, &[sample(64)])
        );
    }

    #[test]
    fn all_box_sizes_match_their_actual_extent() {
        let init = write_init_segment(&[video_track(), audio_track(2, 2, 44100, 2)]);
        assert!(
            walk_and_validate_all_box_sizes(&init),
            "a box size field disagrees with its content"
        );
        let frag = write_fragment(1, 1, 0, &[sample(33), sample(17)]);
        assert!(walk_and_validate_all_box_sizes(&frag));
    }
}
