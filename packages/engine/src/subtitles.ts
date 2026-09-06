// Subtitles: fetch an HLS subtitle rendition, merge it, save it.
//
// The merging is the part that is easy to get wrong and lives in Rust
// (`dl_core::subs`): HLS delivers WebVTT in segments, each carrying its own
// `X-TIMESTAMP-MAP`, and cues that straddle a segment boundary appear in both
// segments. Concatenating the files produces a subtitle track that restarts its
// clock every few seconds and stutters on every boundary.

import { fetchWithRetry } from "./fetch-retry";
import type { Cue, MediaPlaylist, ParsedPlaylist, Rendition } from "./types";
import { loadCore, type DlCore } from "./wasm";

export type SubtitleFormat = "vtt" | "srt";

export interface SubtitleResult {
  /** The finished document, ready to save. */
  text: string;
  filename: string;
  cues: Cue[];
}

export interface FetchSubtitleOptions {
  format?: SubtitleFormat;
  /** Base name for the output file; the extension is set from `format`. */
  baseName?: string;
  signal?: AbortSignal;
  onProgress?: (done: number, total: number) => void;
}

/**
 * Download one subtitle rendition and return it as one document.
 *
 * A rendition URL points at a media playlist whose "segments" are WebVTT files.
 * They are fetched in order, then handed to Rust as a whole so the timestamp
 * arithmetic and de-duplication happen where they are tested.
 */
export async function fetchSubtitleRendition(
  rendition: Rendition,
  opts: FetchSubtitleOptions = {},
): Promise<SubtitleResult> {
  if (!rendition.url) {
    throw new Error(`"${rendition.name}" is muxed into the video and has no separate track`);
  }
  const core = await loadCore();
  const playlist = await fetchMediaPlaylist(rendition.url, core, opts.signal);

  const parts: string[] = [];
  for (const [i, segment] of playlist.segments.entries()) {
    const headers: Record<string, string> = {};
    if (segment.byte_range) {
      headers.Range = `bytes=${segment.byte_range.start}-${segment.byte_range.end}`;
    }
    const res = await fetchWithRetry(segment.url, { headers, signal: opts.signal });
    if (!res.ok && res.status !== 206) {
      throw new Error(`subtitle segment ${i + 1} failed: ${res.status}`);
    }
    parts.push(await res.text());
    opts.onProgress?.(i + 1, playlist.segments.length);
  }

  const vtt = core.merge_vtt_segments_js(JSON.stringify(parts));
  const format = opts.format ?? "vtt";
  const text = format === "srt" ? core.vtt_to_srt(vtt) : vtt;
  const cues = JSON.parse(core.parse_cues_js(vtt)) as Cue[];

  // Composed rather than run through `withExtension`, which would read the
  // language tag as the extension and strip it: "stream.en" would become
  // "stream.srt", so two renditions of one stream would overwrite each other.
  const stem = opts.baseName ?? "subtitles";
  const language = rendition.language ? `.${rendition.language}` : "";
  return { text, cues, filename: `${stem}${language}.${format}` };
}

async function fetchMediaPlaylist(
  url: string,
  core: DlCore,
  signal?: AbortSignal,
): Promise<MediaPlaylist> {
  const text = await (await fetchWithRetry(url, { signal })).text();
  const parsed = JSON.parse(core.parse_playlist_js(text, url)) as ParsedPlaylist;
  if ("Master" in parsed) {
    throw new Error("that subtitle track points at a master playlist, not a track");
  }
  return parsed.Media;
}

/** Convert between the two formats without refetching anything. */
export async function convertSubtitles(text: string, to: SubtitleFormat): Promise<string> {
  const core = await loadCore();
  const cues = core.parse_cues_js(text);
  return to === "srt" ? core.cues_to_srt_js(cues) : core.cues_to_vtt_js(cues);
}

/** Serialize a cue list — used after translation rewrites the text. */
export async function cuesToText(cues: Cue[], format: SubtitleFormat): Promise<string> {
  const core = await loadCore();
  const json = JSON.stringify(cues);
  return format === "srt" ? core.cues_to_srt_js(json) : core.cues_to_vtt_js(json);
}

export async function parseCues(text: string): Promise<Cue[]> {
  const core = await loadCore();
  return JSON.parse(core.parse_cues_js(text)) as Cue[];
}
