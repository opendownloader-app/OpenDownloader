// Converting a file that is already on this machine.
//
// The Rust core handles the one conversion this product is built around — HLS
// segments into a playable MP4 — because that one has to be deterministic,
// resumable and streamable. General format conversion is a different problem
// with a different answer: `mediabunny` drives the browser's own WebCodecs
// encoders and decoders, so an H.264 or Opus encode runs on hardware rather
// than in wasm, and no ffmpeg build is shipped at all.
//
// Nothing is uploaded. WebCodecs runs in the page.

import {
  ALL_FORMATS,
  AdtsOutputFormat,
  BlobSource,
  BufferTarget,
  Conversion,
  Input,
  Mp3OutputFormat,
  Mp4OutputFormat,
  OggOutputFormat,
  Output,
  WavOutputFormat,
  WebMOutputFormat,
  canEncodeAudio,
  type AudioCodec,
  type OutputFormat,
} from "mediabunny";
import { registerMp3Encoder } from "@mediabunny/mp3-encoder";

import { withExtension } from "./format";

export type ConvertTarget = "mp4" | "webm" | "m4a" | "mp3" | "wav" | "ogg";

export interface ConvertTargetInfo {
  id: ConvertTarget;
  label: string;
  extension: string;
  mime: string;
  /** True when the output has no video track by definition. */
  audioOnly: boolean;
}

export const CONVERT_TARGETS: ConvertTargetInfo[] = [
  { id: "mp4", label: "MP4 (H.264 + AAC)", extension: "mp4", mime: "video/mp4", audioOnly: false },
  { id: "webm", label: "WebM (VP9 + Opus)", extension: "webm", mime: "video/webm", audioOnly: false },
  { id: "m4a", label: "M4A (AAC audio)", extension: "m4a", mime: "audio/mp4", audioOnly: true },
  { id: "mp3", label: "MP3 audio", extension: "mp3", mime: "audio/mpeg", audioOnly: true },
  { id: "wav", label: "WAV (uncompressed)", extension: "wav", mime: "audio/wav", audioOnly: true },
  { id: "ogg", label: "OGG (Opus audio)", extension: "ogg", mime: "audio/ogg", audioOnly: true },
];

/**
 * mediabunny has no MP3 encoder of its own and WebCodecs does not define one,
 * so MP3 output comes from a separate LAME-derived wasm package that registers
 * itself as an encoder. Registering twice is harmless but pointless.
 */
let mp3Registered = false;
function ensureMp3Encoder(): void {
  if (mp3Registered) return;
  registerMp3Encoder();
  mp3Registered = true;
}

function outputFormatFor(target: ConvertTarget): OutputFormat {
  switch (target) {
    case "mp4":
      return new Mp4OutputFormat();
    case "webm":
      return new WebMOutputFormat();
    case "m4a":
      // An MP4 container carrying only an AAC track is what ".m4a" is; ADTS is
      // the raw-stream alternative and is what a player is least likely to seek
      // in, so the container wins.
      return new Mp4OutputFormat();
    case "mp3":
      return new Mp3OutputFormat();
    case "wav":
      return new WavOutputFormat();
    case "ogg":
      return new OggOutputFormat();
  }
}

/** Codec asked for per target, when it differs from the container's default. */
function audioCodecFor(target: ConvertTarget): AudioCodec | undefined {
  switch (target) {
    case "mp4":
    case "m4a":
      return "aac";
    case "webm":
    case "ogg":
      return "opus";
    case "mp3":
      return "mp3";
    case "wav":
      return "pcm-s16";
  }
}

export interface ConvertOptions {
  file: File | Blob;
  target: ConvertTarget;
  /** Output name; the extension is replaced to match the target. */
  filename: string;
  signal?: AbortSignal;
  onProgress?: (fraction: number) => void;
}

export interface ConvertResult {
  blob: Blob;
  filename: string;
  /** Tracks the target container cannot hold, so the UI can say what was lost. */
  discarded: string[];
}

/**
 * Convert one local file.
 *
 * The whole output is buffered in memory before it is handed back. That is a
 * real ceiling and it is the honest one for this API: `BufferTarget` is what
 * lets a conversion be handed to the caller as a Blob without a file handle,
 * and a streaming target would need a save location chosen up front — which the
 * tools panel deliberately does not ask for until there is something to save.
 */
export async function convertFile(opts: ConvertOptions): Promise<ConvertResult> {
  const info = CONVERT_TARGETS.find((t) => t.id === opts.target);
  if (!info) throw new Error(`unknown conversion target: ${opts.target}`);
  if (opts.target === "mp3") ensureMp3Encoder();

  const input = new Input({ source: new BlobSource(opts.file), formats: ALL_FORMATS });
  const output = new Output({ format: outputFormatFor(opts.target), target: new BufferTarget() });

  const codec = audioCodecFor(opts.target);
  const conversion = await Conversion.init({
    input,
    output,
    video: info.audioOnly ? { discard: true } : undefined,
    audio: codec ? { codec } : undefined,
  });

  if (!conversion.isValid) {
    const reasons = conversion.discardedTracks
      .map((t) => `${t.track.type} track: ${t.reason}`)
      .join("; ");
    throw new Error(
      reasons
        ? `nothing could be converted into ${info.label} — ${reasons}`
        : `nothing in this file can be converted into ${info.label}`,
    );
  }

  conversion.onProgress = (fraction) => opts.onProgress?.(fraction);
  opts.signal?.addEventListener("abort", () => conversion.cancel(), { once: true });

  await conversion.execute();

  const buffer = (output.target as BufferTarget).buffer;
  if (!buffer) throw new Error("the conversion produced no output");

  return {
    blob: new Blob([buffer], { type: info.mime }),
    filename: withExtension(opts.filename, info.extension),
    discarded: conversion.discardedTracks.map((t) => `${t.track.type}: ${t.reason}`),
  };
}

/**
 * Which targets this browser can actually produce.
 *
 * WebCodecs encoder support genuinely varies — Firefox has no AAC encoder, for
 * one — so the UI asks rather than assuming, and a target that cannot work is
 * disabled with a reason instead of failing halfway through a long encode.
 */
export async function availableTargets(): Promise<Record<ConvertTarget, boolean>> {
  ensureMp3Encoder();
  const entries = await Promise.all(
    CONVERT_TARGETS.map(async (t) => {
      const codec = audioCodecFor(t.id);
      // PCM needs no encoder at all, so WAV is always available.
      const ok = !codec || codec.startsWith("pcm-") || (await canEncodeAudio(codec));
      return [t.id, ok] as const;
    }),
  );
  return Object.fromEntries(entries) as Record<ConvertTarget, boolean>;
}

export { AdtsOutputFormat };
