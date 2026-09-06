//! MPEG-TS demuxing: 188-byte transport packets → PES packets per elementary stream.
//!
//! Scope is deliberately narrow. This handles exactly what HLS segments contain in
//! practice — a PAT, a PMT, and one or two unscrambled elementary streams — and refuses
//! anything else rather than half-supporting it.
//!
//! Stream state is kept in a `Vec` keyed by PID rather than a `HashMap`. That is not an
//! optimisation: iteration order of a `HashMap` is unspecified, and the muxer downstream
//! must produce byte-identical output for identical input.

/// The size of a transport packet. Fixed by the standard; never varies.
const PACKET_LEN: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PID_PAT: u16 = 0x0000;

/// Stream types we can actually remux.
pub const STREAM_TYPE_H264: u8 = 0x1B;
pub const STREAM_TYPE_AAC_ADTS: u8 = 0x0F;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PesPacket {
    pub pid: u16,
    pub stream_type: u8,
    /// Presentation timestamp, 90 kHz units.
    pub pts: Option<u64>,
    /// Decode timestamp, 90 kHz units. Absent when equal to the PTS.
    pub dts: Option<u64>,
    /// The elementary stream payload, with the PES header removed.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsError {
    /// The buffer does not begin with a sync byte where one is required.
    LostSync,
    /// Length is not a whole number of transport packets.
    Truncated,
    /// The transport_scrambling_control field is set. We do not descramble.
    Scrambled,
}

impl core::fmt::Display for TsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TsError::LostSync => f.write_str("not an MPEG-TS stream (no sync byte)"),
            TsError::Truncated => f.write_str("truncated MPEG-TS segment"),
            TsError::Scrambled => f.write_str("scrambled transport stream; not supported"),
        }
    }
}

/// Per-elementary-stream accumulation state.
struct StreamBuf {
    pid: u16,
    stream_type: u8,
    /// Bytes of the PES packet currently being assembled, header included.
    buf: Vec<u8>,
    started: bool,
}

#[derive(Default)]
pub struct TsDemuxer {
    /// PID carrying the PMT, learned from the PAT.
    pmt_pid: Option<u16>,
    /// Elementary streams, in the order the PMT declared them — stable by construction.
    streams: Vec<StreamBuf>,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a whole segment. Returns every PES packet completed by these bytes.
    ///
    /// A PES packet is only complete once the *next* one on the same PID starts (video
    /// PES packets routinely declare a length of zero, meaning "until further notice"),
    /// so anything still open at the end of the segment is flushed here. That is correct
    /// for HLS, where each segment is independently decodable.
    pub fn push(&mut self, ts: &[u8]) -> Result<Vec<PesPacket>, TsError> {
        if ts.is_empty() || ts[0] != SYNC_BYTE {
            return Err(TsError::LostSync);
        }
        if !ts.len().is_multiple_of(PACKET_LEN) {
            return Err(TsError::Truncated);
        }

        let mut out = Vec::new();
        for packet in ts.as_chunks::<PACKET_LEN>().0 {
            self.consume_packet(packet, &mut out)?;
        }
        self.flush_all(&mut out);
        Ok(out)
    }

    fn consume_packet(&mut self, p: &[u8], out: &mut Vec<PesPacket>) -> Result<(), TsError> {
        if p[0] != SYNC_BYTE {
            return Err(TsError::LostSync);
        }
        // Transport error indicator — the packet is known-corrupt, so skip it rather
        // than splicing garbage into an elementary stream.
        if p[1] & 0x80 != 0 {
            return Ok(());
        }
        if p[3] & 0xC0 != 0 {
            return Err(TsError::Scrambled);
        }

        let pusi = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let afc = (p[3] >> 4) & 0x03;

        // 0 is reserved, 2 is adaptation field only — neither carries payload.
        let payload_start = match afc {
            1 => 4,
            3 => {
                let af_len = usize::from(p[4]);
                let start = 5 + af_len;
                if start >= PACKET_LEN {
                    return Ok(());
                }
                start
            }
            _ => return Ok(()),
        };
        let payload = &p[payload_start..];

        if pid == PID_PAT {
            if pusi {
                self.parse_pat(payload);
            }
            return Ok(());
        }
        if Some(pid) == self.pmt_pid {
            if pusi {
                self.parse_pmt(payload);
            }
            return Ok(());
        }

        let Some(idx) = self.streams.iter().position(|s| s.pid == pid) else {
            // Not an elementary stream we were told about (PCR-only PIDs, null packets).
            return Ok(());
        };

        if pusi {
            // A new PES begins: whatever was open on this PID is now complete.
            if let Some(done) = Self::finish(&mut self.streams[idx]) {
                out.push(done);
            }
            self.streams[idx].started = true;
        }
        if self.streams[idx].started {
            self.streams[idx].buf.extend_from_slice(payload);
        }
        Ok(())
    }

    fn flush_all(&mut self, out: &mut Vec<PesPacket>) {
        for s in &mut self.streams {
            if let Some(done) = Self::finish(s) {
                out.push(done);
            }
        }
    }

    /// Turn an accumulated PES buffer into a packet, resetting the stream's state.
    fn finish(s: &mut StreamBuf) -> Option<PesPacket> {
        if !s.started || s.buf.is_empty() {
            s.buf.clear();
            return None;
        }
        let buf = core::mem::take(&mut s.buf);
        s.started = false;
        let (pts, dts, payload) = parse_pes(&buf)?;
        if payload.is_empty() {
            return None;
        }
        Some(PesPacket {
            pid: s.pid,
            stream_type: s.stream_type,
            pts,
            dts,
            data: payload.to_vec(),
        })
    }

    /// Program Association Table — maps program numbers to their PMT PIDs. We take the
    /// first real program; multi-program transport streams are a broadcast concept that
    /// does not occur in HLS segments.
    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(section) = section_body(payload) else {
            return;
        };
        // program_number(2) + reserved/PID(2) per entry, after the 5-byte section header,
        // stopping before the 4-byte CRC.
        let entries = &section[5..section.len().saturating_sub(4)];
        for e in entries.as_chunks::<4>().0 {
            let program_number = (u16::from(e[0]) << 8) | u16::from(e[1]);
            let pid = (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]);
            // program_number 0 designates the Network Information Table, not a program.
            if program_number != 0 {
                self.pmt_pid = Some(pid);
                return;
            }
        }
    }

    /// Program Map Table — declares the elementary streams of one program.
    fn parse_pmt(&mut self, payload: &[u8]) {
        let Some(section) = section_body(payload) else {
            return;
        };
        if section.len() < 13 {
            return;
        }
        let program_info_len = ((usize::from(section[7]) & 0x0F) << 8) | usize::from(section[8]);
        let mut i = 9 + program_info_len;
        let end = section.len().saturating_sub(4); // drop CRC

        let mut declared: Vec<(u16, u8)> = Vec::new();
        while i + 5 <= end {
            let stream_type = section[i];
            let pid = (u16::from(section[i + 1] & 0x1F) << 8) | u16::from(section[i + 2]);
            let es_info_len =
                ((usize::from(section[i + 3]) & 0x0F) << 8) | usize::from(section[i + 4]);
            if matches!(stream_type, STREAM_TYPE_H264 | STREAM_TYPE_AAC_ADTS) {
                declared.push((pid, stream_type));
            }
            i += 5 + es_info_len;
        }

        // The PMT repeats throughout a segment; only act on the first one so accumulated
        // stream state is never discarded mid-segment.
        if self.streams.is_empty() {
            self.streams = declared
                .into_iter()
                .map(|(pid, stream_type)| StreamBuf {
                    pid,
                    stream_type,
                    buf: Vec::new(),
                    started: false,
                })
                .collect();
        }
    }
}

/// Strip the pointer field and the 3-byte section header prologue, returning the section
/// starting at its `table_id`, trimmed to its declared length.
fn section_body(payload: &[u8]) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    let start = 1 + pointer;
    let section = payload.get(start..)?;
    if section.len() < 4 {
        return None;
    }
    let section_length = ((usize::from(section[1]) & 0x0F) << 8) | usize::from(section[2]);
    // `section_length` counts the bytes after itself; +3 covers table_id and the length
    // field, and the result is what `parse_pat`/`parse_pmt` index from the table_id.
    let total = 3 + section_length;
    let body = section.get(3..total.min(section.len()))?;
    Some(body)
}

/// Split a PES packet into its timestamps and its elementary stream payload.
fn parse_pes(buf: &[u8]) -> Option<(Option<u64>, Option<u64>, &[u8])> {
    if buf.len() < 9 || buf[0] != 0x00 || buf[1] != 0x00 || buf[2] != 0x01 {
        return None;
    }
    let header_data_len = usize::from(buf[8]);
    let payload_start = 9 + header_data_len;
    if payload_start > buf.len() {
        return None;
    }

    let pts_dts_flags = (buf[7] >> 6) & 0x03;
    let (pts, dts) = match pts_dts_flags {
        0b10 if buf.len() >= 14 => (read_timestamp(&buf[9..14]), None),
        0b11 if buf.len() >= 19 => (read_timestamp(&buf[9..14]), read_timestamp(&buf[14..19])),
        _ => (None, None),
    };

    // Declared length of zero means "unbounded", which is normal for video.
    let declared = (usize::from(buf[4]) << 8) | usize::from(buf[5]);
    let end = if declared == 0 {
        buf.len()
    } else {
        // `PES_packet_length` counts from immediately after itself, i.e. from byte 6.
        (6 + declared).min(buf.len())
    };
    if end <= payload_start {
        return Some((pts, dts, &[]));
    }
    Some((pts, dts, &buf[payload_start..end]))
}

/// Decode a 33-bit timestamp from its 5-byte marker-interleaved encoding.
fn read_timestamp(b: &[u8]) -> Option<u64> {
    if b.len() < 5 {
        return None;
    }
    let v = (u64::from(b[0] >> 1) & 0x07) << 30
        | u64::from(b[1]) << 22
        | (u64::from(b[2] >> 1) & 0x7F) << 15
        | u64::from(b[3]) << 7
        | (u64::from(b[4] >> 1) & 0x7F);
    Some(v)
}

#[cfg(any(test, feature = "fixtures"))]
pub mod builder {
    //! Synthetic transport streams for tests. Kept `pub(crate)` so the remux tests can
    //! build fixtures too, rather than committing binary blobs to the repository.

    use super::*;

    pub const VIDEO_PID: u16 = 256;
    pub const AUDIO_PID: u16 = 257;
    const PMT_PID: u16 = 4096;

    /// One transport packet, stuffed to exactly 188 bytes via the adaptation field.
    ///
    /// Padding has to go in an adaptation field rather than after the payload: trailing
    /// bytes appended to the payload would be indistinguishable from elementary stream
    /// data and would corrupt every frame.
    pub fn ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184, "payload must fit one packet");
        let mut p = Vec::with_capacity(PACKET_LEN);
        p.push(SYNC_BYTE);
        p.push((u8::from(pusi) << 6) | ((pid >> 8) as u8 & 0x1F));
        p.push((pid & 0xFF) as u8);

        let stuffing = 184 - payload.len();
        if stuffing == 0 {
            p.push(0x10 | (cc & 0x0F)); // payload only
        } else {
            p.push(0x30 | (cc & 0x0F)); // adaptation field + payload
            p.push((stuffing - 1) as u8);
            if stuffing >= 2 {
                p.push(0x00); // no adaptation flags set
                p.extend(core::iter::repeat_n(0xFFu8, stuffing - 2));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), PACKET_LEN);
        p
    }

    pub fn pat() -> Vec<u8> {
        let mut s = vec![
            0x00, // table_id
            0xB0, 0x0D, // section_syntax + length 13
            0x00, 0x01, // transport_stream_id
            0xC1, // version 0, current
            0x00, 0x00, // section_number, last_section_number
            0x00, 0x01, // program_number 1
        ];
        s.push(0xE0 | ((PMT_PID >> 8) as u8 & 0x1F));
        s.push((PMT_PID & 0xFF) as u8);
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC32 (not verified)
        let mut payload = vec![0x00]; // pointer_field
        payload.extend_from_slice(&s);
        payload
    }

    pub fn pmt(streams: &[(u16, u8)]) -> Vec<u8> {
        let section_length = 9 + 5 * streams.len() + 4;
        let mut s = vec![
            0x02,
            0xB0 | ((section_length >> 8) as u8 & 0x0F),
            (section_length & 0xFF) as u8,
            0x00,
            0x01, // program_number
            0xC1,
            0x00,
            0x00,
        ];
        s.push(0xE0 | ((VIDEO_PID >> 8) as u8 & 0x1F)); // PCR_PID
        s.push((VIDEO_PID & 0xFF) as u8);
        s.push(0xF0);
        s.push(0x00); // program_info_length = 0
        for (pid, stream_type) in streams {
            s.push(*stream_type);
            s.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
            s.push((pid & 0xFF) as u8);
            s.push(0xF0);
            s.push(0x00); // ES_info_length = 0
        }
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC32
        let mut payload = vec![0x00];
        payload.extend_from_slice(&s);
        payload
    }

    fn timestamp_bytes(prefix: u8, ts: u64) -> [u8; 5] {
        [
            (prefix << 4) | (((ts >> 30) as u8 & 0x07) << 1) | 1,
            ((ts >> 22) & 0xFF) as u8,
            ((((ts >> 15) & 0x7F) as u8) << 1) | 1,
            ((ts >> 7) & 0xFF) as u8,
            (((ts & 0x7F) as u8) << 1) | 1,
        ]
    }

    /// A complete PES packet with a PTS, and optionally a distinct DTS.
    pub fn pes(stream_id: u8, pts: u64, dts: Option<u64>, data: &[u8]) -> Vec<u8> {
        let mut p = vec![0x00, 0x00, 0x01, stream_id];
        let header_len: usize = if dts.is_some() { 10 } else { 5 };
        // PES_packet_length counts everything after this field.
        let packet_len = 3 + header_len + data.len();
        p.push((packet_len >> 8) as u8);
        p.push((packet_len & 0xFF) as u8);
        p.push(0x80); // '10' marker, no scrambling
        p.push(if dts.is_some() { 0xC0 } else { 0x80 });
        p.push(header_len as u8);
        p.extend_from_slice(&timestamp_bytes(
            if dts.is_some() { 0b0011 } else { 0b0010 },
            pts,
        ));
        if let Some(d) = dts {
            p.extend_from_slice(&timestamp_bytes(0b0001, d));
        }
        p.extend_from_slice(data);
        p
    }

    /// Split a PES packet across as many transport packets as it needs.
    pub fn pes_to_packets(pid: u16, pes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut first = true;
        let mut cc = 0u8;
        for chunk in pes.chunks(184) {
            out.extend(ts_packet(pid, first, cc, chunk));
            first = false;
            cc = (cc + 1) & 0x0F;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::builder::*;
    use super::*;

    const VIDEO_PAYLOAD: &[u8] = &[0, 0, 0, 1, 0x65, 0xAA, 0xBB, 0xCC];
    const AUDIO_PAYLOAD: &[u8] = &[0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x60, 0x01, 0x02];

    fn build_test_ts() -> Vec<u8> {
        let mut ts = pat();
        let mut out = ts_packet(0, true, 0, &ts);
        ts = pmt(&[
            (VIDEO_PID, STREAM_TYPE_H264),
            (AUDIO_PID, STREAM_TYPE_AAC_ADTS),
        ]);
        out.extend(ts_packet(4096, true, 0, &ts));
        out.extend(pes_to_packets(
            VIDEO_PID,
            &pes(0xE0, 900_000, None, VIDEO_PAYLOAD),
        ));
        out.extend(pes_to_packets(
            AUDIO_PID,
            &pes(0xC0, 900_000, None, AUDIO_PAYLOAD),
        ));
        out
    }

    #[test]
    fn demuxes_pat_pmt_and_pes_payloads() {
        let ts = build_test_ts();
        let mut d = TsDemuxer::new();
        let pes = d.push(&ts).unwrap();
        assert_eq!(pes.len(), 2);
        assert_eq!(pes[0].pid, VIDEO_PID);
        assert_eq!(pes[0].stream_type, STREAM_TYPE_H264);
        assert_eq!(pes[0].pts, Some(900_000));
        assert_eq!(pes[0].data, VIDEO_PAYLOAD);
        assert_eq!(pes[1].pid, AUDIO_PID);
        assert_eq!(pes[1].stream_type, STREAM_TYPE_AAC_ADTS);
        assert_eq!(pes[1].data, AUDIO_PAYLOAD);
    }

    #[test]
    fn reads_a_distinct_dts_when_present() {
        let mut out = ts_packet(0, true, 0, &pat());
        out.extend(ts_packet(
            4096,
            true,
            0,
            &pmt(&[(VIDEO_PID, STREAM_TYPE_H264)]),
        ));
        out.extend(pes_to_packets(
            VIDEO_PID,
            &pes(0xE0, immediate(9000), Some(immediate(3000)), VIDEO_PAYLOAD),
        ));
        let pes = TsDemuxer::new().push(&out).unwrap();
        assert_eq!(pes[0].pts, Some(9000));
        assert_eq!(pes[0].dts, Some(3000));
    }

    fn immediate(v: u64) -> u64 {
        v
    }

    #[test]
    fn rejects_a_stream_with_no_sync_byte() {
        let mut d = TsDemuxer::new();
        assert_eq!(d.push(&[0u8; 188]), Err(TsError::LostSync));
    }

    #[test]
    fn rejects_a_truncated_segment() {
        let mut ts = build_test_ts();
        ts.truncate(300);
        assert_eq!(TsDemuxer::new().push(&ts), Err(TsError::Truncated));
    }

    #[test]
    fn refuses_a_scrambled_stream() {
        let mut ts = build_test_ts();
        // Set transport_scrambling_control on the first elementary stream packet.
        ts[188 * 2 + 3] |= 0x80;
        assert_eq!(TsDemuxer::new().push(&ts), Err(TsError::Scrambled));
    }

    #[test]
    fn tolerates_a_pes_split_across_packets() {
        let long: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut out = ts_packet(0, true, 0, &pat());
        out.extend(ts_packet(
            4096,
            true,
            0,
            &pmt(&[(VIDEO_PID, STREAM_TYPE_H264)]),
        ));
        out.extend(pes_to_packets(VIDEO_PID, &pes(0xE0, 0, None, &long)));
        let pes = TsDemuxer::new().push(&out).unwrap();
        assert_eq!(pes.len(), 1);
        assert_eq!(
            pes[0].data, long,
            "a PES spanning 6 packets must reassemble exactly"
        );
    }

    #[test]
    fn separates_consecutive_pes_packets_on_the_same_pid() {
        let mut out = ts_packet(0, true, 0, &pat());
        out.extend(ts_packet(
            4096,
            true,
            0,
            &pmt(&[(VIDEO_PID, STREAM_TYPE_H264)]),
        ));
        out.extend(pes_to_packets(VIDEO_PID, &pes(0xE0, 0, None, &[1, 2, 3])));
        out.extend(pes_to_packets(
            VIDEO_PID,
            &pes(0xE0, 3600, None, &[4, 5, 6]),
        ));
        let pes = TsDemuxer::new().push(&out).unwrap();
        assert_eq!(pes.len(), 2);
        assert_eq!(pes[0].data, vec![1, 2, 3]);
        assert_eq!(pes[1].data, vec![4, 5, 6]);
        assert_eq!(pes[1].pts, Some(3600));
    }

    #[test]
    fn ignores_packets_flagged_as_corrupt() {
        let mut ts = build_test_ts();
        // transport_error_indicator on the audio PES packet.
        let audio_packet = 188 * 3;
        ts[audio_packet + 1] |= 0x80;
        let pes = TsDemuxer::new().push(&ts).unwrap();
        assert_eq!(
            pes.len(),
            1,
            "the corrupt stream must be dropped, not spliced"
        );
        assert_eq!(pes[0].pid, VIDEO_PID);
    }
}
