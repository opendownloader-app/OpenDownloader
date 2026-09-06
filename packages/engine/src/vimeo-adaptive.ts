// Vimeo's JSON adaptive manifest, turned into something downloadable.
//
// The parsing is the core's; this fetches the manifest and shapes the result into the
// two streams the merger already knows how to join. Nothing here understands MP4 — the
// renditions become `ExtractedStream`s carrying their segment lists, and the merger reads
// them exactly as it reads an ordinary ranged URL.

import { fetchWithRetry } from "./fetch-retry";
import type { ExtractedStream } from "./types";
import { loadCore } from "./wasm";

/** One rendition: an init segment and the segments that follow it. */
export interface VimeoRendition {
  id: string;
  mime: string;
  codecs: string | null;
  bitrate: number | null;
  width: number | null;
  height: number | null;
  duration_ms: number | null;
  init_base64: string;
  segments: {
    url: string;
    size: number;
    offset: number;
    duration_ms: number;
  }[];
  total_bytes: number;
}

export interface VimeoManifest {
  clip_id: string;
  /** Best first. */
  video: VimeoRendition[];
  audio: VimeoRendition[];
}

/** Whether this URL is one of Vimeo's adaptive manifests. */
export async function isVimeoManifest(url: string): Promise<boolean> {
  const core = await loadCore();
  return core.is_vimeo_manifest(url);
}

/**
 * Fetch and read a manifest.
 *
 * The URL is signed and short-lived — it is only ever obtained by watching what the page
 * requested — so this is called when the download is chosen rather than when the page is
 * listed, and a stale one fails as an expired link rather than as a parse error.
 */
export async function resolveVimeoManifest(
  url: string,
): Promise<VimeoManifest> {
  const response = await fetchWithRetry(url, {
    headers: { "x-relay-referer": "https://player.vimeo.com/" },
  });
  if (!response.ok) {
    throw new Error(
      response.status === 403 || response.status === 410
        ? "That Vimeo stream address has expired. Reload the page, let the video play " +
            "for a moment, and try again."
        : `Vimeo's stream manifest answered ${response.status}.`,
    );
  }
  const core = await loadCore();
  const parsed = core.parse_vimeo_manifest(await response.text(), url);
  if (!parsed) {
    throw new Error(
      "Vimeo's stream manifest was not in a shape this can read.",
    );
  }
  return JSON.parse(parsed) as VimeoManifest;
}

/**
 * The exact length of one segment, from the server.
 *
 * The manifest's own `size` is an estimate and cannot be used for this. Measured against
 * a live clip it was out by −6940, +3669 and +3150 bytes on three consecutive video
 * segments — close enough to look right in a listing, and useless for laying out a byte
 * stream, where being wrong by four bytes puts every later offset in the wrong place.
 */
async function measure(url: string, signal?: AbortSignal): Promise<number> {
  const res = await fetchWithRetry(url, { method: "HEAD", signal });
  const length = Number(res.headers.get("content-length"));
  if (!res.ok || !Number.isFinite(length) || length <= 0) {
    throw new Error("Vimeo did not state the length of a segment.");
  }
  return length;
}

/** Measure every segment, a few at a time, and lay out the real offsets. */
async function measuredSegments(
  rendition: VimeoRendition,
  signal?: AbortSignal,
): Promise<{ url: string; size: number; offset: number }[]> {
  const sizes: number[] = [];
  // Six at a time: enough to hide the latency of forty-odd requests, few enough not to
  // look like a burst to a CDN that throttles.
  const BATCH = 6;
  for (let i = 0; i < rendition.segments.length; i += BATCH) {
    const batch = rendition.segments.slice(i, i + BATCH);
    sizes.push(
      ...(await Promise.all(batch.map((s) => measure(s.url, signal)))),
    );
  }

  let offset = atob(rendition.init_base64).length;
  return rendition.segments.map((s, index) => {
    const size = sizes[index]!;
    const at = offset;
    offset += size;
    return { url: s.url, size, offset: at };
  });
}

/** A rendition as a stream the merger can read. */
function streamFor(
  rendition: VimeoRendition,
  kind: "videoonly" | "audioonly",
  segments: { url: string; size: number; offset: number }[],
): ExtractedStream {
  return {
    // Never fetched: a segmented stream is read through `segments`. Kept as the first
    // segment's address so anything logging the stream names something real.
    url: rendition.segments[0]?.url ?? "",
    kind,
    mime: rendition.mime,
    // The measured total, not the manifest's: this is what the merger will read.
    size: (segments.at(-1)?.offset ?? 0) + (segments.at(-1)?.size ?? 0),
    headers: [],
    max_chunk: null,
    segments,
    initBase64: rendition.init_base64,
  };
}

/**
 * The video and audio streams for one chosen rendition.
 *
 * The audio is whichever rendition has the highest bitrate, since Vimeo offers no
 * language choice here and the best is the only sensible default.
 */
export async function vimeoStreams(
  manifest: VimeoManifest,
  videoIndex = 0,
  signal?: AbortSignal,
): Promise<{
  video: ExtractedStream;
  audio: ExtractedStream;
  label: string;
} | null> {
  const video = manifest.video[videoIndex];
  const audio = manifest.audio[0];
  if (!video || !audio) return null;
  const [videoSegments, audioSegments] = await Promise.all([
    measuredSegments(video, signal),
    measuredSegments(audio, signal),
  ]);
  return {
    video: streamFor(video, "videoonly", videoSegments),
    audio: streamFor(audio, "audioonly", audioSegments),
    label: video.height ? `${video.height}p` : "Video",
  };
}
