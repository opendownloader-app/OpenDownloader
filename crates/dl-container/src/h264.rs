//! H.264 bitstream handling.
//!
//! MPEG-TS carries H.264 as Annex B: NAL units separated by `00 00 01` start codes. MP4
//! carries the same NAL units length-prefixed (AVCC), with the parameter sets hoisted out
//! of the stream and into an `avcC` box in the sample description. Converting between the
//! two is the bulk of what "remuxing" means for video.

/// NAL unit types we care about.
const NAL_SLICE_NON_IDR: u8 = 1;
const NAL_SLICE_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// Split an Annex B bitstream into its NAL units, without their start codes.
///
/// Both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes appear in real
/// streams, often in the same segment.
pub fn split_nals(annexb: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (payload start, start-code length)

    let mut i = 0;
    while i + 3 <= annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 {
            if annexb[i + 2] == 1 {
                starts.push((i + 3, 3));
                i += 3;
                continue;
            }
            if i + 4 <= annexb.len() && annexb[i + 2] == 0 && annexb[i + 3] == 1 {
                starts.push((i + 4, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    for (idx, &(start, _)) in starts.iter().enumerate() {
        // A NAL runs until the start code of the next one, which sits `code_len` bytes
        // before that NAL's payload.
        let end = match starts.get(idx + 1) {
            Some(&(next_start, next_code_len)) => next_start - next_code_len,
            None => annexb.len(),
        };
        if end > start {
            nals.push(&annexb[start..end]);
        }
    }
    nals
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1F)
}

/// Convert Annex B to AVCC (4-byte big-endian length prefixes).
///
/// Parameter sets and access unit delimiters are dropped: the former move to the `avcC`
/// box, and the latter carry no decodable content.
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annexb.len());
    for nal in split_nals(annexb) {
        if matches!(nal_type(nal), NAL_SPS | NAL_PPS | NAL_AUD) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Find the sequence and picture parameter sets, which the MP4 sample description needs.
pub fn find_parameter_sets(annexb: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut sps = None;
    let mut pps = None;
    for nal in split_nals(annexb) {
        match nal_type(nal) {
            NAL_SPS if sps.is_none() => sps = Some(nal.to_vec()),
            NAL_PPS if pps.is_none() => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    (sps, pps)
}

/// True when the access unit can be decoded without any preceding frame.
pub fn is_keyframe(annexb: &[u8]) -> bool {
    split_nals(annexb)
        .iter()
        .any(|n| nal_type(n) == NAL_SLICE_IDR)
}

/// True when the access unit contains any coded slice at all.
pub fn has_slice(annexb: &[u8]) -> bool {
    split_nals(annexb)
        .iter()
        .any(|n| matches!(nal_type(n), NAL_SLICE_NON_IDR | NAL_SLICE_IDR))
}

/// Build the `AVCDecoderConfigurationRecord` that goes inside an `avcC` box.
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(11 + sps.len() + pps.len());
    c.push(1); // configurationVersion
               // Profile, compatibility and level are copied verbatim out of the SPS; a mismatch
               // here makes players reject the track even when the bitstream is fine.
    c.push(*sps.get(1).unwrap_or(&0x42));
    c.push(*sps.get(2).unwrap_or(&0x00));
    c.push(*sps.get(3).unwrap_or(&0x1E));
    c.push(0xFF); // 6 bits reserved + lengthSizeMinusOne = 3 (4-byte prefixes)
    c.push(0xE1); // 3 bits reserved + numOfSequenceParameterSets = 1
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(1); // numOfPictureParameterSets
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    c
}

/// Decode the coded resolution from an SPS.
///
/// Needed because `tkhd`/`stsd` must carry real dimensions — a track declaring 0x0 plays
/// as a blank rectangle in most players even though the bitstream is intact.
pub fn sps_resolution(sps: &[u8]) -> Option<(u32, u32)> {
    if sps.len() < 4 {
        return None;
    }
    let rbsp = remove_emulation_prevention(&sps[1..]);
    let mut r = BitReader::new(&rbsp);

    let profile_idc = r.u(8)? as u8;
    r.u(8)?; // constraint flags + reserved
    r.u(8)?; // level_idc
    r.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.u(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.u(1)? == 1 {
            // seq_scaling_matrix_present_flag
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if r.u(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    r.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = r.ue()?;
    if pic_order_cnt_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        r.u(1)?; // delta_pic_order_always_zero_flag
        r.se()?; // offset_for_non_ref_pic
        r.se()?; // offset_for_top_to_bottom_field
        let n = r.ue()?;
        for _ in 0..n {
            r.se()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.u(1)?; // gaps_in_frame_num_value_allowed_flag

    let pic_width_in_mbs_minus1 = r.ue()?;
    let pic_height_in_map_units_minus1 = r.ue()?;
    let frame_mbs_only_flag = r.u(1)?;
    if frame_mbs_only_flag == 0 {
        r.u(1)?; // mb_adaptive_frame_field_flag
    }
    r.u(1)?; // direct_8x8_inference_flag

    let mut width = (pic_width_in_mbs_minus1 + 1) * 16;
    let mut height = (2 - frame_mbs_only_flag) * (pic_height_in_map_units_minus1 + 1) * 16;

    if r.u(1)? == 1 {
        // frame_cropping_flag — cropping is how 1080 lines fit in 68 macroblock rows.
        let left = r.ue()?;
        let right = r.ue()?;
        let top = r.ue()?;
        let bottom = r.ue()?;
        let (sub_w, sub_h) = match chroma_format_idc {
            0 => (1, 1),
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let crop_unit_x = sub_w;
        let crop_unit_y = sub_h * (2 - frame_mbs_only_flag);
        width = width.saturating_sub((left + right) * crop_unit_x);
        height = height.saturating_sub((top + bottom) * crop_unit_y);
    }

    Some((width as u32, height as u32))
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last_scale: i64 = 8;
    let mut next_scale: i64 = 8;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = r.se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Some(())
}

/// Strip `00 00 03` emulation-prevention bytes, yielding the raw byte sequence payload.
fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros == 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Big-endian bit reader with the exponential-Golomb codes the H.264 syntax uses.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u(&mut self, n: usize) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | u64::from(bit);
            self.pos += 1;
        }
        Some(v)
    }

    /// Unsigned exp-Golomb.
    fn ue(&mut self) -> Option<u64> {
        let mut leading = 0;
        while self.u(1)? == 0 {
            leading += 1;
            // Guards against a corrupt stream driving this unbounded.
            if leading > 32 {
                return None;
            }
        }
        if leading == 0 {
            return Some(0);
        }
        let rest = self.u(leading)?;
        Some((1u64 << leading) - 1 + rest)
    }

    /// Signed exp-Golomb.
    fn se(&mut self) -> Option<i64> {
        let k = self.ue()?;
        let signed = if k % 2 == 0 {
            -((k / 2) as i64)
        } else {
            k.div_ceil(2) as i64
        };
        Some(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_annexb_start_codes_to_length_prefixes() {
        let annexb = [0, 0, 0, 1, 0x65, 0xAA, 0xBB, 0, 0, 1, 0x41, 0xCC];
        assert_eq!(
            annexb_to_avcc(&annexb),
            vec![0, 0, 0, 3, 0x65, 0xAA, 0xBB, 0, 0, 0, 2, 0x41, 0xCC]
        );
    }

    #[test]
    fn parameter_sets_are_dropped_from_the_sample_data() {
        // They belong in avcC, not in the mdat, and duplicating them wastes space.
        let annexb = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x65, 0x11];
        assert_eq!(annexb_to_avcc(&annexb), vec![0, 0, 0, 2, 0x65, 0x11]);
    }

    #[test]
    fn extracts_sps_and_pps_by_nal_type() {
        let annexb = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x11,
        ];
        let (sps, pps) = find_parameter_sets(&annexb);
        assert_eq!(sps, Some(vec![0x67, 0x42, 0x00, 0x1E]));
        assert_eq!(pps, Some(vec![0x68, 0xCE]));
    }

    #[test]
    fn avcc_carries_the_profile_bytes_from_the_sps() {
        let sps = [0x67, 0x42, 0xC0, 0x1E, 0xAA];
        let cfg = build_avcc(&sps, &[0x68, 0xCE]);
        assert_eq!(cfg[0], 1);
        assert_eq!(cfg[1], 0x42);
        assert_eq!(cfg[2], 0xC0);
        assert_eq!(cfg[3], 0x1E);
        assert_eq!(cfg[4], 0xFF);
        assert_eq!(cfg[5], 0xE1);
        assert_eq!(&cfg[6..8], &[0x00, 0x05]);
        assert_eq!(&cfg[8..13], &sps);
    }

    #[test]
    fn detects_idr_keyframes() {
        assert!(is_keyframe(&[0, 0, 0, 1, 0x65, 0x11]));
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 0x11]));
        assert!(has_slice(&[0, 0, 0, 1, 0x41, 0x11]));
        assert!(!has_slice(&[0, 0, 0, 1, 0x67, 0x42]));
    }

    #[test]
    fn handles_three_and_four_byte_start_codes_in_one_stream() {
        let annexb = [
            0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0, 0, 1, 0x65, 0xCC,
        ];
        let nals = split_nals(&annexb);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 0xAA]);
        assert_eq!(nals[1], &[0x68, 0xBB]);
        assert_eq!(nals[2], &[0x65, 0xCC]);
    }

    #[test]
    fn strips_emulation_prevention_bytes() {
        assert_eq!(
            remove_emulation_prevention(&[0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x02]),
            vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x02]
        );
    }

    #[test]
    fn decodes_resolution_from_a_real_baseline_sps() {
        // 640x360 baseline SPS as emitted by x264.
        let sps = [
            0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x2F, 0xF9, 0x50, 0x10, 0x10, 0x10, 0x40,
        ];
        assert_eq!(sps_resolution(&sps), Some((640, 360)));
    }

    #[test]
    fn decodes_cropped_1080_height() {
        // 1920x1080 high-profile SPS: 68 macroblock rows cropped down to 1080 lines.
        let sps = [
            0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0x84, 0x00, 0x00,
            0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC9, 0x20,
        ];
        assert_eq!(sps_resolution(&sps), Some((1920, 1080)));
    }

    #[test]
    fn resolution_of_a_garbage_sps_is_none_not_a_panic() {
        assert_eq!(sps_resolution(&[0x67]), None);
        assert_eq!(sps_resolution(&[]), None);
    }
}
