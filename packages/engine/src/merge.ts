// Downloading a video and its audio separately, then making one file of them.
//
// YouTube no longer offers a muxed format above 360p, and Bilibili's DASH is split the
// same way, so "1080p with sound" means two files and a third step. Nothing is
// re-encoded: the samples are copied and only their framing changes.
//
// The two downloads themselves are ordinary — a platform media URL honours byte ranges
// like any other, so the same retrying, range-planning code handles them.
//
// # Two input shapes, two mergers
//
// There is no single "MP4" here.
//
// - **Fragmented** (`ftyp moov sidx moof mdat …`) is what YouTube and Bilibili serve.
//   Its sample tables are deliberately empty; each fragment describes itself, and the
//   `sidx` indexes them. Merging is a matter of renumbering tracks and passing fragments
//   through, which is `FragmentMerger`.
// - **Progressive** (`ftyp moov mdat` with real `stsc`/`stsz`/`stco`) is what an
//   ordinary file on a web server looks like. Merging means reading both sample tables
//   and writing fresh fragments, which is `Muxer`.
//
// Which one applies is decided by looking at the bytes rather than by trusting the site,
// because a site can change what it serves without telling anyone.

import { fetchWithRetry } from "./fetch-retry";
import { updateJob } from "./jobs";
import type { Sink } from "./sinks";
import type { ExtractedStream, Job, Progress } from "./types";
import { loadCore } from "./wasm";

/**
 * Bytes fetched from the front of each stream before planning.
 *
 * Has to cover `ftyp` + `moov` + `sidx`, which for a long video's index is the largest
 * of the three. 256 KiB is comfortably above what YouTube produces for a ten-minute
 * 4K stream and is one round trip.
 */
const HEAD_BYTES = 256 * 1024;

export interface MergeOptions {
  onProgress: (p: Progress) => void;
  signal?: AbortSignal;
}

function headersOf(stream: ExtractedStream): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [name, value] of stream.headers ?? []) out[name] = value;
  return out;
}

function abortIf(signal: AbortSignal | undefined): void {
  if (signal?.aborted) throw new DOMException("aborted", "AbortError");
}

/**
 * Read one byte range of a remote stream, splitting it if the host caps range size.
 *
 * Every read is a range request, which is what lets a merge start immediately: the
 * merger asks for a piece at a time, in the order the output needs them, so memory stays
 * bounded by one fragment rather than by the size of the video.
 *
 * The splitting is not an optimisation. Google's media hosts answer `206` for a range of
 * exactly 1 MiB and **403** for anything larger, and a fragmented video's `moof`+`mdat`
 * pair is routinely bigger than that — so a merge that asked for a whole fragment in one
 * request would fail on every YouTube video above the lowest quality, with a status that
 * reads like an authorisation problem.
 */
async function readRange(
  stream: ExtractedStream,
  start: number,
  length: number,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  const limit = stream.max_chunk ?? Number.MAX_SAFE_INTEGER;
  if (length <= limit) return readOnce(stream, start, length, signal);

  const pieces: Uint8Array[] = [];
  for (let offset = 0; offset < length; offset += limit) {
    if (signal?.aborted) throw new DOMException("aborted", "AbortError");
    pieces.push(
      await readOnce(
        stream,
        start + offset,
        Math.min(limit, length - offset),
        signal,
      ),
    );
  }

  const out = new Uint8Array(pieces.reduce((sum, p) => sum + p.length, 0));
  let written = 0;
  for (const piece of pieces) {
    out.set(piece, written);
    written += piece.length;
  }
  return out;
}

async function readOnce(
  stream: ExtractedStream,
  start: number,
  length: number,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  // A segmented stream is not one resource, so there is no range to ask for: the bytes
  // are assembled from the pieces that cover the span instead. The merger cannot tell
  // the difference, which is the point — it asks for offsets either way.
  if (stream.segments) {
    return readFromSegments(stream, start, length, signal);
  }
  const res = await fetchWithRetry(stream.url, {
    headers: {
      ...headersOf(stream),
      Range: `bytes=${start}-${start + length - 1}`,
    },
    signal,
    // A host that states a request size is one that throttles, and its 403 means "slow
    // down" rather than "no".
    retryForbidden: stream.max_chunk !== null,
  });
  if (!res.ok && res.status !== 206) {
    throw new Error(
      `stream answered ${res.status} for bytes ${start}-${start + length - 1}`,
    );
  }
  return new Uint8Array(await res.arrayBuffer());
}

/**
 * One fetched piece, kept so consecutive reads inside it cost one request.
 *
 * The merger reads a stream in order but not in segment-sized bites — it takes a box
 * header, then a fragment, then the next — so without this a 2 MB segment is refetched
 * for every small read inside it. One piece is enough precisely because the reads
 * advance.
 */
let cached: { url: string; bytes: Uint8Array } | null = null;

/** Fetch one segment whole, or return the init segment decoded from the manifest. */
async function piece(
  stream: ExtractedStream,
  url: string | null,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  if (url === null) {
    // The init segment arrives inside the manifest as base64, so it is never fetched.
    const binary = atob(stream.initBase64 ?? "");
    return Uint8Array.from(binary, (c) => c.charCodeAt(0));
  }
  if (cached?.url === url) return cached.bytes;
  const res = await fetchWithRetry(url, {
    headers: headersOf(stream),
    signal,
    retryForbidden: stream.max_chunk !== null,
  });
  if (!res.ok) {
    throw new Error(`segment answered ${res.status}`);
  }
  const bytes = new Uint8Array(await res.arrayBuffer());
  cached = { url, bytes };
  return bytes;
}

/**
 * Serve a byte range of a segmented stream.
 *
 * The stream is the init segment followed by every segment in order, and the manifest
 * states each length, so which pieces a span touches is arithmetic rather than a probe.
 */
async function readFromSegments(
  stream: ExtractedStream,
  start: number,
  length: number,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  const initLength = stream.segments?.[0]?.offset ?? 0;
  // The init segment first, then each segment at the offset the manifest laid out.
  const pieces: { url: string | null; start: number; end: number }[] = [
    { url: null, start: 0, end: initLength },
    ...(stream.segments ?? []).map((s) => ({
      url: s.url,
      start: s.offset,
      end: s.offset + s.size,
    })),
  ];

  const out = new Uint8Array(length);
  let written = 0;
  for (const p of pieces) {
    const from = Math.max(start, p.start);
    const to = Math.min(start + length, p.end);
    if (to <= from) continue;
    const bytes = await piece(stream, p.url, signal);
    // A segment that is not the length the manifest promised would put every later
    // offset wrong, so it is caught here rather than producing a silently broken file.
    if (bytes.length !== p.end - p.start) {
      throw new Error(
        `a segment was ${bytes.length} bytes where the manifest said ${p.end - p.start}; ` +
          "the manifest and the media no longer agree",
      );
    }
    out.set(bytes.subarray(from - p.start, to - p.start), from - start);
    written += to - from;
  }
  if (written !== length) {
    throw new Error(
      `only ${written} of ${length} bytes are covered by this stream's segments`,
    );
  }
  return out;
}

/** The top-level box types present at the front of a stream. */
function boxTypes(head: Uint8Array): string[] {
  const view = new DataView(head.buffer, head.byteOffset, head.byteLength);
  const types: string[] = [];
  let offset = 0;
  while (offset + 8 <= head.length) {
    let size = view.getUint32(offset);
    const kind = String.fromCharCode(
      head[offset + 4]!,
      head[offset + 5]!,
      head[offset + 6]!,
      head[offset + 7]!,
    );
    types.push(kind);
    if (size === 1) {
      if (offset + 16 > head.length) break;
      size = view.getUint32(offset + 8) * 2 ** 32 + view.getUint32(offset + 12);
    } else if (size === 0) {
      break;
    }
    if (size < 8) break;
    offset += size;
  }
  return types;
}

/** The `moov` box out of an already-fetched head, or null when it is further in. */
function moovIn(head: Uint8Array): Uint8Array | null {
  const view = new DataView(head.buffer, head.byteOffset, head.byteLength);
  let offset = 0;
  while (offset + 8 <= head.length) {
    let size = view.getUint32(offset);
    const kind = String.fromCharCode(
      head[offset + 4]!,
      head[offset + 5]!,
      head[offset + 6]!,
      head[offset + 7]!,
    );
    let headerLen = 8;
    if (size === 1) {
      if (offset + 16 > head.length) return null;
      size = view.getUint32(offset + 8) * 2 ** 32 + view.getUint32(offset + 12);
      headerLen = 16;
    } else if (size === 0) {
      return null;
    }
    if (size < headerLen) return null;
    if (kind === "moov") {
      return offset + size <= head.length
        ? head.subarray(offset, offset + size)
        : null;
    }
    offset += size;
  }
  return null;
}

/**
 * Find a progressive MP4's `moov` when it is not near the front.
 *
 * A file written by a plain muxer puts `moov` after the `mdat`, which can be gigabytes.
 * Walking the top-level headers over range requests costs a handful of small reads, and
 * is the difference between starting a merge instantly and downloading the whole file
 * before learning anything.
 */
async function seekMoov(
  stream: ExtractedStream,
  head: Uint8Array,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  const inHead = moovIn(head);
  if (inHead) return inHead;

  let offset = 0;
  for (let hop = 0; hop < 64; hop++) {
    abortIf(signal);
    const chunk =
      hop === 0 ? head : await readRange(stream, offset, 64 * 1024, signal);
    if (chunk.length < 8) break;
    const view = new DataView(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    let size = view.getUint32(0);
    const kind = String.fromCharCode(
      chunk[4]!,
      chunk[5]!,
      chunk[6]!,
      chunk[7]!,
    );
    let headerLen = 8;
    if (size === 1) {
      if (chunk.length < 16) break;
      size = view.getUint32(8) * 2 ** 32 + view.getUint32(12);
      headerLen = 16;
    } else if (size === 0) {
      break;
    }
    if (size < headerLen) break;
    if (kind === "moov") {
      return size <= chunk.length
        ? chunk.subarray(0, size)
        : readRange(stream, offset, size, signal);
    }
    offset += size;
  }
  throw new Error("no MP4 movie header found in that stream");
}

/** What a merger needs from the caller: a read plan, and a way to feed it. */
interface Merger {
  reads(): string;
  push(index: number, bytes: Uint8Array): Uint8Array;
  resetHash(): void;
  hashUpdate(bytes: Uint8Array): void;
  hashHex(): string;
  hashedLen(): bigint;
}

/**
 * Download a video stream and an audio stream and write one merged MP4 to `sink`.
 *
 * Resume granularity is the whole job rather than the byte: a merger's state is an
 * interleave position across two inputs, and persisting that safely is more machinery
 * than the case warrants. A paused merge therefore restarts, and the UI says so rather
 * than implying otherwise.
 */
export async function runMerge(
  job: Job,
  video: ExtractedStream,
  audio: ExtractedStream,
  sink: Sink,
  opts: MergeOptions,
): Promise<{ sha256: string; bytes: number }> {
  const core = await loadCore();

  opts.onProgress({
    received: 0,
    total: null,
    status: "probing",
    message: "reading both streams' headers",
  });

  // Both heads first, concurrently: nothing can be planned until both are in hand, and
  // fetching them in parallel halves the wait before anything visible happens.
  const [videoHead, audioHead] = await Promise.all([
    readRange(video, 0, HEAD_BYTES, opts.signal),
    readRange(audio, 0, HEAD_BYTES, opts.signal),
  ]);
  abortIf(opts.signal);

  // Decided from the bytes, not from the site: a `sidx` means the file indexes its own
  // fragments, which is the fragmented shape.
  const fragmented =
    boxTypes(videoHead).includes("sidx") &&
    boxTypes(audioHead).includes("sidx");

  let merger: Merger;
  if (fragmented) {
    merger = core.FragmentMerger.fromHeads(
      videoHead,
      audioHead,
    ) as unknown as Merger;
  } else {
    const [videoMoov, audioMoov] = await Promise.all([
      seekMoov(video, videoHead, opts.signal),
      seekMoov(audio, audioHead, opts.signal),
    ]);
    merger = core.Muxer.fromMoovs(videoMoov, audioMoov) as unknown as Merger;
  }

  const reads = JSON.parse(merger.reads()) as {
    source: "Video" | "Audio";
    offset: number;
    len: number;
  }[];

  // The total is known up front because the plan covers both inputs exactly once.
  const total = reads.reduce((sum, r) => sum + r.len, 0);
  let fetched = 0;
  let written = 0;
  const startedAt = Date.now();

  for (const [index, read] of reads.entries()) {
    abortIf(opts.signal);
    const stream = read.source === "Video" ? video : audio;
    const bytes = await readRange(stream, read.offset, read.len, opts.signal);
    if (bytes.length !== read.len) {
      throw new Error(
        `stream returned ${bytes.length} bytes where ${read.len} were asked for; ` +
          "the file changed on the server",
      );
    }

    const out = merger.push(index, bytes);
    await sink.write(written, out);
    written += out.length;
    fetched += bytes.length;

    const elapsed = (Date.now() - startedAt) / 1000;
    const rate = elapsed > 0.25 ? fetched / elapsed : undefined;
    opts.onProgress({
      received: fetched,
      total,
      status: "downloading",
      message: `merging · piece ${index + 1} of ${reads.length}`,
      bytesPerSecond: rate,
      etaSeconds: rate ? (total - fetched) / rate : null,
    });

    // Persisted for display only. A merge does not resume, so this is progress
    // reporting rather than a resume point.
    if (index % 8 === 0) {
      await updateJob(job.id, {
        receivedBytes: fetched,
        outputBytes: written,
        totalBytes: total,
      });
    }
  }

  opts.onProgress({ received: fetched, total, status: "verifying" });
  await sink.finalize();

  // The same read-back digest an ordinary download produces, over the file that now
  // exists rather than over what was intended.
  merger.resetHash();
  await sink.readBack((chunk) => merger.hashUpdate(chunk));
  const sha256 = merger.hashHex();
  const bytes = Number(merger.hashedLen());
  await sink.cleanup();

  return { sha256, bytes };
}

/** Bytes the two streams will transfer, when both report a size. */
export function mergeSize(
  video: ExtractedStream,
  audio: ExtractedStream,
): number | null {
  if (video.size === null || audio.size === null) return null;
  return video.size + audio.size;
}
