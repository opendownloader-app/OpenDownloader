// Tools that operate on a file the user already has.
//
// Two of them, both running through the same Rust code the downloader uses:
//
//  - remux a `.ts` file (or a folder of them, or a local playlist) into a
//    playable fMP4, which is what turns a stalled download's leftovers into
//    something that plays;
//  - extract the audio track of a progressive MP4 without re-encoding it, which
//    is the "save the audio" case that does not need an encoder at all.
//
// Neither uploads anything, and neither needs an encoder — they rearrange the
// bytes that are already there.

import { withExtension } from "./format";
import { loadCore } from "./wasm";

export interface RemuxLocalOptions {
  /** Transport-stream segments, in playback order. */
  files: File[];
  audioOnly?: boolean;
  signal?: AbortSignal;
  onProgress?: (done: number, total: number) => void;
}

export interface LocalResult {
  blob: Blob;
  filename: string;
  /** SHA-256 of the produced bytes, from the same hasher downloads use. */
  sha256: string;
}

/**
 * Remux local MPEG-TS segments into one fragmented MP4.
 *
 * Segment order is the order given: HLS names its segments sequentially, but
 * `seg10.ts` sorts before `seg2.ts` lexicographically, so the caller sorts
 * naturally before calling and the UI shows the resulting order for review.
 */
export async function remuxLocalSegments(opts: RemuxLocalOptions): Promise<LocalResult> {
  const core = await loadCore();
  if (opts.files.length === 0) throw new Error("no segments were given");

  const session = new core.DownloadSession(undefined, false, true, opts.audioOnly ?? false);
  const parts: Uint8Array[] = [];

  for (const [i, file] of opts.files.entries()) {
    if (opts.signal?.aborted) throw new DOMException("aborted", "AbortError");
    const bytes = new Uint8Array(await file.arrayBuffer());
    parts.push(session.pushSegment(bytes));
    opts.onProgress?.(i + 1, opts.files.length);
  }

  session.resetHash();
  for (const part of parts) session.hashUpdate(part);

  const first = opts.files[0];
  const name = withExtension(first?.name ?? "remuxed", opts.audioOnly ? "m4a" : "mp4");
  return {
    blob: new Blob(parts as BlobPart[], { type: opts.audioOnly ? "audio/mp4" : "video/mp4" }),
    filename: name,
    sha256: session.hashHex(),
  };
}

export interface ExtractAudioOptions {
  file: File;
  signal?: AbortSignal;
  onProgress?: (done: number, total: number) => void;
}

/** Bytes read at once while walking the top-level boxes looking for `moov`. */
const SCAN_CHUNK = 64 * 1024;

/**
 * Extract an MP4's AAC track into an M4A, losslessly.
 *
 * The samples are copied, not re-encoded: what lands in the output is byte for
 * byte the audio that was in the input, which is both faster than any encoder
 * and the only version of "extract audio" that loses nothing.
 *
 * The file is read in pieces at the offsets Rust asks for, so a 4 GB video does
 * not have to fit in memory to give up its soundtrack.
 */
export async function extractMp4Audio(opts: ExtractAudioOptions): Promise<LocalResult> {
  const core = await loadCore();
  const file = opts.file;

  const moov = await findMoov(file);
  if (!moov) {
    throw new Error(
      "no MP4 movie header found — this is not an MP4, or it is still downloading",
    );
  }

  const extractor = core.Mp4AudioExtractor.fromMoov(moov);
  const chunks = JSON.parse(extractor.chunks()) as {
    offset: number;
    len: number;
    sample_count: number;
  }[];

  const parts: Uint8Array[] = [];
  for (const [i, chunk] of chunks.entries()) {
    if (opts.signal?.aborted) throw new DOMException("aborted", "AbortError");
    const slice = file.slice(chunk.offset, chunk.offset + chunk.len);
    const bytes = new Uint8Array(await slice.arrayBuffer());
    parts.push(extractor.pushChunk(i, bytes));
    opts.onProgress?.(i + 1, chunks.length);
  }

  extractor.resetHash();
  for (const part of parts) extractor.hashUpdate(part);

  return {
    blob: new Blob(parts as BlobPart[], { type: "audio/mp4" }),
    filename: withExtension(file.name, "m4a"),
    sha256: extractor.hashHex(),
  };
}

/**
 * Read the `moov` box out of an MP4 without reading the file.
 *
 * Box order is not fixed: a file written for streaming puts `moov` first, and
 * one written by a plain muxer puts it after the `mdat` — which can be
 * gigabytes. Walking the top-level headers and seeking past `mdat` costs a
 * handful of small reads either way.
 */
async function findMoov(file: File): Promise<Uint8Array | null> {
  let offset = 0;
  while (offset + 8 <= file.size) {
    const head = new Uint8Array(await file.slice(offset, offset + SCAN_CHUNK).arrayBuffer());
    const view = new DataView(head.buffer, head.byteOffset, head.byteLength);
    if (head.length < 8) return null;

    let size = view.getUint32(0);
    const kind = String.fromCharCode(head[4]!, head[5]!, head[6]!, head[7]!);
    let headerLen = 8;
    if (size === 1) {
      if (head.length < 16) return null;
      // 64-bit `largesize`. Files this large are real; a truncating read is not
      // an option, so read it as two 32-bit halves and combine in JS numbers,
      // which are exact to 2^53 — far beyond any file a browser can open.
      size = view.getUint32(8) * 2 ** 32 + view.getUint32(12);
      headerLen = 16;
    } else if (size === 0) {
      // "extends to the end of the file"
      size = file.size - offset;
    }
    if (size < headerLen) return null;

    if (kind === "moov") {
      return new Uint8Array(await file.slice(offset, offset + size).arrayBuffer());
    }
    offset += size;
  }
  return null;
}
