//! Subtitle handling: WebVTT and SRT.
//!
//! HLS delivers subtitles as a playlist of WebVTT segments, each a complete little
//! document with its own header and a timestamp map. A download wants one file, and
//! users mostly want it as `.srt` because that is what every player and every editor
//! opens without argument. Everything needed for that lives here: a tolerant parser
//! that accepts either format, writers for both, and the segment merger.
//!
//! All of it is pure. The same bytes in give the same bytes out, which is what lets the
//! merge be re-run on resume and still produce an identical file.

/// One subtitle cue. Times are milliseconds from the start of the presentation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Cue payload with its original line breaks and inline markup intact.
    pub text: String,
    /// WebVTT cue settings (`line:0 align:start`), verbatim. Always `None` for SRT.
    pub settings: Option<String>,
}

/// MPEG-TS timestamps (`MPEGTS:` in `X-TIMESTAMP-MAP`) are always 90 kHz.
const MPEGTS_TIMESCALE: u64 = 90_000;

/// Parse a WebVTT or SRT document into cues.
///
/// The two formats share the same block structure — optional identifier, a timing
/// line, payload — and differ in the decimal separator, so one parser covers both by
/// accepting `.` and `,` alike. Anything that is not a cue (the `WEBVTT` header and its
/// `X-TIMESTAMP-MAP`, `NOTE`, `STYLE` and `REGION` blocks) is skipped, and a block whose
/// timing line does not parse is dropped rather than failing the whole document:
/// subtitle files in the wild are hand-edited far more often than video files are.
pub fn parse_cues(text: &str) -> Vec<Cue> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");

    let mut cues = Vec::new();
    for block in normalized.split("\n\n") {
        let lines: Vec<&str> = block.lines().skip_while(|l| l.trim().is_empty()).collect();
        let Some(first) = lines.first() else {
            continue;
        };
        let first = first.trim_start();
        if first.starts_with("WEBVTT")
            || first.starts_with("NOTE")
            || first.starts_with("STYLE")
            || first.starts_with("REGION")
        {
            continue;
        }
        let Some(timing_idx) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let Some((start_ms, end_ms, settings)) = parse_timing_line(lines[timing_idx]) else {
            continue;
        };
        let text = lines[timing_idx + 1..].join("\n");
        cues.push(Cue {
            start_ms,
            end_ms,
            text,
            settings,
        });
    }
    cues
}

/// Render cues as a WebVTT document.
pub fn cues_to_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        out.push_str(&format_timestamp(cue.start_ms, '.'));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(cue.end_ms, '.'));
        if let Some(settings) = cue.settings.as_deref().filter(|s| !s.is_empty()) {
            out.push(' ');
            out.push_str(settings);
        }
        out.push('\n');
        out.push_str(&cue.text);
        out.push_str("\n\n");
    }
    out
}

/// Render cues as an SRT document.
///
/// SRT has no cue settings and no markup beyond `<b>`, `<i>` and `<u>`, so positioning
/// is dropped and WebVTT-only tags are stripped. Leaving them in is worse than losing
/// them: players that do not understand `<c.yellow>` render it as literal text.
pub fn cues_to_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, cue) in cues.iter().enumerate() {
        out.push_str(&(i + 1).to_string());
        out.push('\n');
        out.push_str(&format_timestamp(cue.start_ms, ','));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(cue.end_ms, ','));
        out.push('\n');
        out.push_str(&strip_vtt_markup(&cue.text));
        out.push_str("\n\n");
    }
    out
}

/// Convert a WebVTT document to SRT.
pub fn vtt_to_srt(vtt: &str) -> String {
    cues_to_srt(&parse_cues(vtt))
}

/// Stitch the WebVTT segments of an HLS subtitle playlist into one document.
///
/// Each segment may carry `X-TIMESTAMP-MAP=LOCAL:<ts>,MPEGTS:<n>`, which says "the
/// cue time `LOCAL` corresponds to transport-stream time `MPEGTS`". Packagers use it in
/// two incompatible ways: some write cues with absolute times and repeat the same map in
/// every segment; others restart cue times at zero in every segment and move the map
/// instead. Both are handled by the same rule — shift each segment by how far its map
/// has moved from the first segment's map — because in the first case that shift is
/// zero and in the second it is exactly the segment's position in the stream.
///
/// Cues that straddle a segment boundary are repeated by the packager in both segments,
/// so identical cues are collapsed and same-text cues that overlap or touch are merged
/// into one. The result is sorted by start time.
pub fn merge_vtt_segments(segments: &[&str]) -> String {
    let offsets: Vec<Option<i64>> = segments.iter().map(|s| timestamp_map_offset(s)).collect();
    let baseline = offsets.iter().flatten().next().copied().unwrap_or(0);

    let mut cues: Vec<Cue> = Vec::new();
    for (segment, offset) in segments.iter().zip(&offsets) {
        let shift = offset.unwrap_or(0) - baseline;
        for mut cue in parse_cues(segment) {
            cue.start_ms = shift_ms(cue.start_ms, shift);
            cue.end_ms = shift_ms(cue.end_ms, shift);
            cues.push(cue);
        }
    }
    cues.sort_by_key(|c| c.start_ms);

    // Cues are in start order, so a later cue overlaps an earlier same-text cue exactly
    // when the earlier one has not ended before the later one begins. An exact duplicate
    // is the degenerate case of this and needs no separate pass.
    let mut merged: Vec<Cue> = Vec::with_capacity(cues.len());
    for cue in cues {
        let existing = merged
            .iter_mut()
            .find(|m| m.text == cue.text && m.settings == cue.settings && m.end_ms >= cue.start_ms);
        match existing {
            Some(m) => m.end_ms = m.end_ms.max(cue.end_ms),
            None => merged.push(cue),
        }
    }
    cues_to_vtt(&merged)
}

/// The shift a segment's `X-TIMESTAMP-MAP` implies, in milliseconds: transport-stream
/// time minus local cue time. `None` when the segment has no map.
fn timestamp_map_offset(segment: &str) -> Option<i64> {
    let line = segment
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}'))
        .find(|l| l.starts_with("X-TIMESTAMP-MAP="))?;
    let mut local_ms: Option<u64> = None;
    let mut mpegts: Option<u64> = None;
    for field in line["X-TIMESTAMP-MAP=".len()..].split(',') {
        let Some((key, value)) = field.trim().split_once(':') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "LOCAL" => local_ms = parse_timestamp(value),
            "MPEGTS" => mpegts = value.trim().parse().ok(),
            _ => {}
        }
    }
    let mpegts_ms = (mpegts? * 1000 + MPEGTS_TIMESCALE / 2) / MPEGTS_TIMESCALE;
    Some(mpegts_ms as i64 - local_ms? as i64)
}

fn shift_ms(ms: u64, shift: i64) -> u64 {
    (ms as i64).saturating_add(shift).max(0) as u64
}

/// Split `start --> end [settings]` into its parts.
fn parse_timing_line(line: &str) -> Option<(u64, u64, Option<String>)> {
    let (start, rest) = line.split_once("-->")?;
    let start_ms = parse_timestamp(start)?;
    let mut rest = rest.trim().splitn(2, char::is_whitespace);
    let end_ms = parse_timestamp(rest.next()?)?;
    let settings = rest
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((start_ms, end_ms, settings))
}

/// Parse `HH:MM:SS.mmm` or `MM:SS.mmm`, with `,` accepted in place of `.`.
///
/// Fractional parts shorter or longer than three digits are scaled rather than
/// rejected, since hand-edited files have both.
fn parse_timestamp(s: &str) -> Option<u64> {
    let s = s.trim();
    let (whole, frac) = match s.split_once(['.', ',']) {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    let parts: Vec<u64> = whole
        .split(':')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => (0, *m, *s),
        _ => return None,
    };
    let millis = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let scale = 10u64.pow(digits.len() as u32);
        digits.parse::<u64>().ok()? * 1000 / scale
    };
    Some(((h * 60 + m) * 60 + sec) * 1000 + millis)
}

fn format_timestamp(ms: u64, decimal: char) -> String {
    let h = ms / 3_600_000;
    let m = (ms / 60_000) % 60;
    let s = (ms / 1000) % 60;
    let frac = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}{decimal}{frac:03}")
}

/// Drop every WebVTT tag except `<b>`, `<i>`, `<u>` and their closers.
///
/// Anything else — class spans, voices, ruby, language, inline timestamps — has no SRT
/// meaning. Tag *content* is always kept; only the angle-bracketed markup goes.
fn strip_vtt_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close_rel) = rest[open..].find('>') else {
            // An unterminated `<` is literal text, not markup.
            out.push_str(&rest[open..]);
            return out;
        };
        let tag = &rest[open..=open + close_rel];
        if is_srt_tag(tag) {
            out.push_str(tag);
        }
        rest = &rest[open + close_rel + 1..];
    }
    out.push_str(rest);
    out
}

fn is_srt_tag(tag: &str) -> bool {
    let inner = tag.trim_start_matches('<').trim_end_matches('>');
    let name = inner.trim_start_matches('/');
    let name = name
        .split(|c: char| c.is_whitespace() || c == '.')
        .next()
        .unwrap_or("");
    matches!(name, "b" | "i" | "u")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: u64, end_ms: u64, text: &str) -> Cue {
        Cue {
            start_ms,
            end_ms,
            text: text.into(),
            settings: None,
        }
    }

    const VTT: &str = "\u{feff}WEBVTT - Test\r\nKind: captions\r\n\r\n\
NOTE this is a comment\r\nspanning two lines\r\n\r\n\
STYLE\r\n::cue { color: white }\r\n\r\n\
REGION\r\nid:top\r\n\r\n\
intro\r\n00:00:01.000 --> 00:00:03.500 line:0 align:start\r\nHello\r\nworld\r\n\r\n\
00:05.000 --> 00:06.250\r\nShort form\r\n";

    #[test]
    fn parses_vtt_with_header_note_style_region_identifiers_and_settings() {
        let cues = parse_cues(VTT);
        assert_eq!(
            cues,
            vec![
                Cue {
                    start_ms: 1000,
                    end_ms: 3500,
                    text: "Hello\nworld".into(),
                    settings: Some("line:0 align:start".into()),
                },
                cue(5000, 6250, "Short form"),
            ]
        );
    }

    #[test]
    fn parses_srt() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nFirst\n\n2\n01:02:03,456 --> 01:02:04,000\nSecond\nline two\n\n";
        assert_eq!(
            parse_cues(srt),
            vec![
                cue(1000, 2000, "First"),
                cue(3_723_456, 3_724_000, "Second\nline two"),
            ]
        );
    }

    #[test]
    fn tolerates_a_timing_line_that_does_not_parse() {
        let bad =
            "WEBVTT\n\n00:00:01.000 --> garbage\nDropped\n\n00:00:02.000 --> 00:00:03.000\nKept\n";
        assert_eq!(parse_cues(bad), vec![cue(2000, 3000, "Kept")]);
    }

    #[test]
    fn short_and_long_fractions_are_scaled_to_milliseconds() {
        assert_eq!(parse_timestamp("00:00:01.5"), Some(1500));
        assert_eq!(parse_timestamp("00:00:01.12345"), Some(1123));
        assert_eq!(parse_timestamp("00:01"), Some(1000));
        assert_eq!(parse_timestamp("1:00:00.000"), Some(3_600_000));
        assert_eq!(parse_timestamp("nope"), None);
    }

    #[test]
    fn cues_round_trip_through_vtt() {
        let original = vec![
            Cue {
                start_ms: 1000,
                end_ms: 3500,
                text: "Hello\nworld".into(),
                settings: Some("align:end".into()),
            },
            cue(3_723_456, 3_724_000, "<i>later</i>"),
        ];
        let vtt = cues_to_vtt(&original);
        assert!(
            vtt.starts_with("WEBVTT\n\n00:00:01.000 --> 00:00:03.500 align:end\nHello\nworld\n\n")
        );
        assert_eq!(parse_cues(&vtt), original);
    }

    #[test]
    fn srt_output_is_numbered_and_uses_comma_decimals() {
        let srt = cues_to_srt(&[cue(1000, 2000, "One"), cue(3_723_456, 3_724_000, "Two")]);
        assert_eq!(
            srt,
            "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n2\n01:02:03,456 --> 01:02:04,000\nTwo\n\n"
        );
    }

    #[test]
    fn srt_output_strips_vtt_only_markup_but_keeps_basic_styling() {
        let cues = vec![cue(
            0,
            1000,
            "<v Bob><c.yellow>Hi</c> <i>there</i> <b>and</b> <u>you</u></v>\n\
<ruby>漢<rt>kan</rt></ruby> <lang en>word</lang> <00:00:00.500>late",
        )];
        let srt = cues_to_srt(&cues);
        assert!(srt.contains("Hi <i>there</i> <b>and</b> <u>you</u>\n漢kan word late\n"));
        assert!(!srt.contains("<c"));
        assert!(!srt.contains("<v"));
        assert!(!srt.contains("00:00:00.500>"));
    }

    #[test]
    fn srt_output_drops_cue_settings() {
        let cues = vec![Cue {
            start_ms: 0,
            end_ms: 1000,
            text: "x".into(),
            settings: Some("line:0".into()),
        }];
        assert_eq!(
            cues_to_srt(&cues),
            "1\n00:00:00,000 --> 00:00:01,000\nx\n\n"
        );
    }

    #[test]
    fn an_unterminated_angle_bracket_is_literal_text() {
        assert_eq!(strip_vtt_markup("a < b"), "a < b");
        assert_eq!(strip_vtt_markup("<i>a</i> < b"), "<i>a</i> < b");
    }

    #[test]
    fn vtt_to_srt_converts_a_whole_document() {
        assert_eq!(
            vtt_to_srt(VTT),
            "1\n00:00:01,000 --> 00:00:03,500\nHello\nworld\n\n2\n00:00:05,000 --> 00:00:06,250\nShort form\n\n"
        );
    }

    #[test]
    fn merge_with_a_constant_map_leaves_times_unchanged() {
        let seg0 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n\
00:00:00.000 --> 00:00:02.000\nA\n";
        let seg1 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n\
00:00:06.000 --> 00:00:08.000\nB\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1])),
            vec![cue(0, 2000, "A"), cue(6000, 8000, "B")]
        );
    }

    #[test]
    fn merge_with_per_segment_maps_shifts_later_segments() {
        let seg0 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n\
00:00:00.000 --> 00:00:02.000\nA\n";
        // The map moved by 540000 ticks = 6 s, so this segment's cues start 6 s later.
        let seg1 = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:1440000,LOCAL:00:00:00.000\n\n\
00:00:00.000 --> 00:00:02.000\nB\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1])),
            vec![cue(0, 2000, "A"), cue(6000, 8000, "B")]
        );
    }

    #[test]
    fn a_nonzero_local_time_is_subtracted_from_the_shift() {
        let seg0 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\n\
00:00:00.000 --> 00:00:01.000\nA\n";
        // LOCAL 1 s ↔ MPEGTS 10 s: the segment's own clock already carries one of the
        // ten seconds, so cues move by nine.
        let seg1 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:01.000,MPEGTS:900000\n\n\
00:00:01.000 --> 00:00:02.000\nB\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1])),
            vec![cue(0, 1000, "A"), cue(10_000, 11_000, "B")]
        );
    }

    #[test]
    fn segments_without_a_map_are_not_shifted() {
        let seg0 = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nA\n";
        let seg1 = "WEBVTT\n\n00:00:04.000 --> 00:00:05.000\nB\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1])),
            vec![cue(0, 1000, "A"), cue(4000, 5000, "B")]
        );
    }

    #[test]
    fn a_shift_below_zero_saturates_at_zero() {
        let seg0 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n\
00:00:00.000 --> 00:00:01.000\nA\n";
        let seg1 = "WEBVTT\n\n00:00:00.500 --> 00:00:03.000\nB\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1])),
            // Both now start at 0; the stable sort keeps segment order for ties.
            vec![cue(0, 1000, "A"), cue(0, 0, "B")]
        );
    }

    #[test]
    fn duplicate_cues_across_a_boundary_are_removed() {
        let seg0 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n\
00:00:01.000 --> 00:00:03.000\nA\n\n00:00:05.000 --> 00:00:07.000\nStraddles\n";
        let seg1 = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n\
00:00:05.000 --> 00:00:07.000\nStraddles\n\n00:00:08.000 --> 00:00:09.000\nC\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1])),
            vec![
                cue(1000, 3000, "A"),
                cue(5000, 7000, "Straddles"),
                cue(8000, 9000, "C"),
            ]
        );
    }

    #[test]
    fn overlapping_and_touching_same_text_cues_are_merged() {
        let seg0 = "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nSame\n";
        let seg1 = "WEBVTT\n\n00:00:03.000 --> 00:00:05.000\nSame\n";
        let seg2 = "WEBVTT\n\n00:00:04.500 --> 00:00:06.000\nSame\n\n\
00:00:04.500 --> 00:00:06.000\nDifferent\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1, seg2])),
            vec![cue(1000, 6000, "Same"), cue(4500, 6000, "Different")]
        );
    }

    #[test]
    fn same_text_cues_with_a_gap_stay_separate() {
        let seg = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nSame\n\n\
00:00:03.000 --> 00:00:04.000\nSame\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg])),
            vec![cue(1000, 2000, "Same"), cue(3000, 4000, "Same")]
        );
    }

    #[test]
    fn same_text_cues_with_different_settings_are_not_merged() {
        let seg = "WEBVTT\n\n00:00:01.000 --> 00:00:03.000 line:0\nSame\n\n\
00:00:02.000 --> 00:00:04.000 line:1\nSame\n";
        assert_eq!(parse_cues(&merge_vtt_segments(&[seg])).len(), 2);
    }

    #[test]
    fn merged_output_is_sorted_by_start() {
        let seg0 = "WEBVTT\n\n00:00:05.000 --> 00:00:06.000\nLate\n";
        let seg1 = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nEarly\n";
        assert_eq!(
            parse_cues(&merge_vtt_segments(&[seg0, seg1])),
            vec![cue(1000, 2000, "Early"), cue(5000, 6000, "Late")]
        );
    }

    #[test]
    fn merge_is_deterministic() {
        let segs = [
            "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n00:00:00.000 --> 00:00:02.000\nA\n",
            "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:1440000\n\n00:00:00.000 --> 00:00:02.000\nB\n",
        ];
        assert_eq!(merge_vtt_segments(&segs), merge_vtt_segments(&segs));
    }

    #[test]
    fn merging_nothing_gives_just_the_header() {
        assert_eq!(merge_vtt_segments(&[]), "WEBVTT\n\n");
        assert_eq!(cues_to_vtt(&[]), "WEBVTT\n\n");
        assert_eq!(cues_to_srt(&[]), "");
    }

    #[test]
    fn cues_serialize_as_plain_json() {
        let json = serde_json::to_string(&cue(1000, 2000, "x")).unwrap();
        assert_eq!(
            json,
            "{\"start_ms\":1000,\"end_ms\":2000,\"text\":\"x\",\"settings\":null}"
        );
        assert_eq!(
            serde_json::from_str::<Cue>(&json).unwrap(),
            cue(1000, 2000, "x")
        );
    }
}
