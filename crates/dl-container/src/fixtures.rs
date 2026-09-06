//! Synthetic media, generated rather than committed.
//!
//! The unit tests and the local test media server both build their MPEG-TS from
//! here, so an end-to-end run and a `cargo test` run are asserting against
//! byte-identical input. A committed binary fixture could not offer that — it would
//! drift the moment either side changed what it expected.

use crate::aac::builder::adts_frame;
use crate::ts::builder::{pat, pes, pes_to_packets, pmt, ts_packet, AUDIO_PID, VIDEO_PID};
use crate::ts::{STREAM_TYPE_AAC_ADTS, STREAM_TYPE_H264};

/// A real 640x360 baseline SPS as emitted by x264, so remuxed output carries
/// genuine track dimensions rather than a placeholder.
pub const SPS: &[u8] = &[
    0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x2F, 0xF9, 0x50, 0x10, 0x10, 0x10, 0x40,
];
pub const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

/// Wrap NAL units in Annex B start codes.
pub fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

/// An IDR access unit, parameter sets included as a real encoder would emit them.
pub fn idr(payload: u8) -> Vec<u8> {
    annexb(&[SPS, PPS, &[0x65, payload, payload, payload]])
}

/// A non-IDR access unit.
pub fn delta(payload: u8) -> Vec<u8> {
    annexb(&[&[0x41, payload, payload]])
}

/// One MPEG-TS segment: PAT, PMT, two video frames and two AAC frames.
///
/// `base_pts` is in 90 kHz units. Passing a large value exercises the decode-time
/// normalisation path, which is what stops a stream whose timestamps start minutes
/// in from producing a file with a long blank lead-in.
pub fn ts_segment(base_pts: u64) -> Vec<u8> {
    let mut out = ts_packet(0, true, 0, &pat());
    out.extend(ts_packet(
        4096,
        true,
        0,
        &pmt(&[
            (VIDEO_PID, STREAM_TYPE_H264),
            (AUDIO_PID, STREAM_TYPE_AAC_ADTS),
        ]),
    ));
    out.extend(pes_to_packets(
        VIDEO_PID,
        &pes(0xE0, base_pts, None, &idr(0x11)),
    ));
    out.extend(pes_to_packets(
        VIDEO_PID,
        &pes(0xE0, base_pts + 3000, None, &delta(0x22)),
    ));

    let mut audio = adts_frame(2, 4, 2, &[1, 2, 3, 4]);
    audio.extend(adts_frame(2, 4, 2, &[5, 6, 7, 8]));
    out.extend(pes_to_packets(
        AUDIO_PID,
        &pes(0xC0, base_pts, None, &audio),
    ));
    out
}

/// Deterministic bytes for a progressive-download fixture.
///
/// A cheap integer hash rather than a counter: a repeating pattern would let a
/// misordered or duplicated chunk still produce plausible-looking output, which is
/// precisely the failure a resume test needs to catch.
pub fn progressive_bytes(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| {
            let x = (i as u64).wrapping_mul(2_654_435_761);
            (x >> 13) as u8
        })
        .collect()
}
