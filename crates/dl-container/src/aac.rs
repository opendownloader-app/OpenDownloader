//! AAC in ADTS framing.
//!
//! MPEG-TS carries AAC wrapped in ADTS headers, which repeat the codec configuration on
//! every single frame. MP4 states the configuration once, in an `esds` box, and stores
//! only the raw AAC payloads. So the remuxer has to read one ADTS header for the
//! configuration and then strip every header it sees.

/// Sampling frequencies indexed by the 4-bit ADTS field.
pub const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Samples produced per AAC frame. Fixed for AAC-LC, and what the muxer uses for the
/// per-sample duration.
pub const SAMPLES_PER_FRAME: u32 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsFrame {
    /// Byte range of the raw AAC payload within the buffer, header excluded.
    pub payload_range: (usize, usize),
    pub sample_rate: u32,
    pub channels: u8,
    /// Audio object type: 2 for AAC-LC, the only thing in practical use here.
    pub object_type: u8,
}

/// Walk a buffer of concatenated ADTS frames.
///
/// Stops at the first frame that does not begin with a valid syncword rather than trying
/// to resynchronise: a mid-buffer desync means the demux upstream was wrong, and guessing
/// would produce plausible-sounding garbage.
pub fn parse_adts(buf: &[u8]) -> Vec<AdtsFrame> {
    let mut frames = Vec::new();
    let mut i = 0;
    while i + 7 <= buf.len() {
        // 12-bit syncword.
        if buf[i] != 0xFF || (buf[i + 1] & 0xF0) != 0xF0 {
            break;
        }
        let protection_absent = buf[i + 1] & 0x01;
        let object_type = ((buf[i + 2] >> 6) & 0x03) + 1;
        let freq_index = ((buf[i + 2] >> 2) & 0x0F) as usize;
        let channels = ((buf[i + 2] & 0x01) << 2) | ((buf[i + 3] >> 6) & 0x03);
        let frame_length = (usize::from(buf[i + 3] & 0x03) << 11)
            | (usize::from(buf[i + 4]) << 3)
            | (usize::from(buf[i + 5]) >> 5);

        let Some(&sample_rate) = SAMPLE_RATES.get(freq_index) else {
            break;
        };
        // A CRC, when present, sits between the header and the payload.
        let header_len = if protection_absent == 1 { 7 } else { 9 };
        if frame_length < header_len || i + frame_length > buf.len() {
            break;
        }

        frames.push(AdtsFrame {
            payload_range: (i + header_len, i + frame_length),
            sample_rate,
            channels,
            object_type,
        });
        i += frame_length;
    }
    frames
}

/// Build the two-byte `AudioSpecificConfig` that goes inside an `esds` box.
///
/// Layout: 5 bits object type, 4 bits sampling frequency index, 4 bits channel
/// configuration, then padding to a byte boundary.
pub fn audio_specific_config(object_type: u8, sample_rate: u32, channels: u8) -> Vec<u8> {
    let freq_index = SAMPLE_RATES
        .iter()
        .position(|&r| r == sample_rate)
        .unwrap_or(4) as u8; // 44100 is the safest fallback
    let b0 = (object_type << 3) | (freq_index >> 1);
    let b1 = ((freq_index & 0x01) << 7) | ((channels & 0x0F) << 3);
    vec![b0, b1]
}

#[cfg(any(test, feature = "fixtures"))]
pub mod builder {
    //! ADTS frame construction for tests and remux fixtures.

    /// One ADTS frame with a 7-byte header (no CRC) wrapping `payload`.
    pub fn adts_frame(object_type: u8, freq_index: u8, channels: u8, payload: &[u8]) -> Vec<u8> {
        let frame_length = 7 + payload.len();
        let mut f = vec![
            0xFF,
            0xF1, // MPEG-4, layer 0, protection absent
            ((object_type - 1) << 6) | ((freq_index & 0x0F) << 2) | ((channels >> 2) & 0x01),
            ((channels & 0x03) << 6) | ((frame_length >> 11) as u8 & 0x03),
            ((frame_length >> 3) & 0xFF) as u8,
            (((frame_length & 0x07) << 5) as u8) | 0x1F, // buffer fullness = VBR
            0xFC,                                        // one raw data block per frame
        ];
        f.extend_from_slice(payload);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::builder::adts_frame;
    use super::*;

    #[test]
    fn parses_adts_headers_and_frame_boundaries() {
        let mut buf = adts_frame(2, 4, 2, &[1, 2, 3, 4]);
        buf.extend(adts_frame(2, 4, 2, &[5, 6, 7]));
        let f = parse_adts(&buf);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].sample_rate, 44100);
        assert_eq!(f[0].channels, 2);
        assert_eq!(f[0].object_type, 2);
        assert_eq!(
            &buf[f[0].payload_range.0..f[0].payload_range.1],
            &[1, 2, 3, 4]
        );
        assert_eq!(&buf[f[1].payload_range.0..f[1].payload_range.1], &[5, 6, 7]);
    }

    #[test]
    fn stops_at_a_desync_rather_than_guessing() {
        let mut buf = adts_frame(2, 4, 2, &[1, 2, 3, 4]);
        buf.extend_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);
        assert_eq!(parse_adts(&buf).len(), 1);
    }

    #[test]
    fn ignores_a_frame_that_claims_more_bytes_than_exist() {
        let mut f = adts_frame(2, 4, 2, &[1, 2, 3, 4]);
        f.truncate(9); // header says 11 bytes, only 9 present
        assert!(parse_adts(&f).is_empty());
    }

    #[test]
    fn handles_other_sample_rates_and_channel_counts() {
        let buf = adts_frame(2, 3, 1, &[9, 9]);
        let f = parse_adts(&buf);
        assert_eq!(f[0].sample_rate, 48000);
        assert_eq!(f[0].channels, 1);
    }

    #[test]
    fn builds_a_two_byte_audio_specific_config() {
        assert_eq!(audio_specific_config(2, 44100, 2), vec![0x12, 0x10]);
        assert_eq!(audio_specific_config(2, 48000, 1), vec![0x11, 0x88]);
    }
}
