// Making subtitles from a file's own audio, on this machine.
//
// Whisper, exported to ONNX and run by transformers.js on WebGPU (falling back
// to the WASM backend). The audio never leaves the browser; the only thing
// fetched from the network is the model itself, once, after which the browser
// caches it.
//
// **Why this is not a paid cloud feature.** A hosted transcription API would be
// more accurate on hard audio, and it would also mean uploading the soundtrack
// of whatever you downloaded to somebody's server and metering it. The whole
// product is free and local; this is the version of the feature that can stay
// that way. Where a local model is not good enough, that is said plainly rather
// than upsold.
//
// This lives in the web app rather than in the shared engine because it is the
// one capability with a large lazy dependency and no place in an extension
// bundle a store has to review.

import { cuesToText, type Cue } from "@opendownloader/engine";

/** Whisper's native input rate. Anything else is resampled to it. */
const TARGET_SAMPLE_RATE = 16_000;

export interface TranscribeModel {
  id: string;
  label: string;
  size: string;
  note: string;
  englishOnly: boolean;
}

/**
 * Deliberately three. A dropdown of fifteen checkpoints is a worse product than
 * three that span the real trade-off, which is download size against accuracy.
 */
export const TRANSCRIBE_MODELS: TranscribeModel[] = [
  {
    id: "onnx-community/whisper-tiny.en",
    label: "Tiny (English)",
    size: "~40 MB",
    note: "Fastest. Fine for clear speech.",
    englishOnly: true,
  },
  {
    id: "onnx-community/whisper-base",
    label: "Base",
    size: "~80 MB",
    note: "A good default. Handles 99 languages.",
    englishOnly: false,
  },
  {
    id: "onnx-community/whisper-small",
    label: "Small",
    size: "~250 MB",
    note: "The most accurate here, and the slowest.",
    englishOnly: false,
  },
];

export interface TranscribeProgress {
  stage: "audio" | "model" | "transcribing";
  fraction: number | null;
  note: string;
}

export interface TranscribeOptions {
  file: File | Blob;
  model?: string;
  /** BCP-47 code, or undefined to let Whisper detect it. Ignored by `.en` models. */
  language?: string;
  signal?: AbortSignal;
  onProgress?: (p: TranscribeProgress) => void;
}

export interface TranscribeSupport {
  ok: boolean;
  device: "webgpu" | "wasm";
  reason?: string;
}

/** Whether transcription can run here at all, and on what. */
export async function transcribeSupport(): Promise<TranscribeSupport> {
  if (typeof AudioContext === "undefined" && typeof OfflineAudioContext === "undefined") {
    return { ok: false, device: "wasm", reason: "This browser has no Web Audio support." };
  }
  try {
    const gpu = (navigator as unknown as { gpu?: { requestAdapter(): Promise<unknown> } }).gpu;
    if (gpu && (await gpu.requestAdapter())) return { ok: true, device: "webgpu" };
  } catch {
    // A GPU that reports itself and then refuses an adapter is not an error
    // worth surfacing; the WASM backend still works.
  }
  return { ok: true, device: "wasm" };
}

let pipelinePromise: Promise<unknown> | null = null;
let loadedModelId: string | null = null;

interface WhisperChunk {
  text: string;
  timestamp: [number, number | null];
}

/**
 * Decode a media file's audio to 16 kHz mono.
 *
 * `decodeAudioData` handles every container the browser can play, which is the
 * same set this app can download, so no separate demuxer is needed here.
 */
async function decodeAudio(
  file: File | Blob,
  onProgress?: (fraction: number) => void,
): Promise<Float32Array> {
  onProgress?.(0.1);
  const bytes = await file.arrayBuffer();
  onProgress?.(0.5);

  // A short-lived context purely for decoding; the sample rate is set here so
  // the browser resamples during decode rather than us doing it afterwards.
  const context = new OfflineAudioContext(1, 1, TARGET_SAMPLE_RATE);
  const decoded = await context.decodeAudioData(bytes);

  const frames = Math.ceil(decoded.duration * TARGET_SAMPLE_RATE);
  const offline = new OfflineAudioContext(1, frames, TARGET_SAMPLE_RATE);
  const source = offline.createBufferSource();
  source.buffer = decoded;
  source.connect(offline.destination);
  source.start();
  const rendered = await offline.startRendering();
  onProgress?.(1);
  return rendered.getChannelData(0).slice();
}

/**
 * Transcribe a file into subtitle cues.
 *
 * Timestamps are per segment, not per word: word-level timings need a model
 * exported with cross-attentions, which the small quantised ONNX Whisper builds
 * are not, and asking for them throws outright.
 */
export async function transcribeToCues(opts: TranscribeOptions): Promise<Cue[]> {
  const support = await transcribeSupport();
  if (!support.ok) throw new Error(support.reason ?? "transcription is not supported here");

  const model = opts.model ?? TRANSCRIBE_MODELS[1]!.id;

  opts.onProgress?.({ stage: "audio", fraction: 0, note: "Reading the audio" });
  const audio = await decodeAudio(opts.file, (fraction) =>
    opts.onProgress?.({ stage: "audio", fraction, note: "Reading the audio" }),
  );
  if (opts.signal?.aborted) throw new DOMException("aborted", "AbortError");

  opts.onProgress?.({ stage: "model", fraction: null, note: "Loading the speech model" });

  // transformers.js is roughly 10 MB and most visits never transcribe, so it is
  // imported the first time this runs rather than at page load.
  const { pipeline, env } = await import("@huggingface/transformers");

  // Serve ONNX Runtime's WebAssembly backend from our own origin.
  //
  // Left alone, transformers.js points ORT at a public CDN and fetches its
  // runtime from there the first time a model runs. This product's claim is
  // that nothing leaves your machine except what you chose to fetch; a silent
  // runtime dependency on a third party would make that claim false. The
  // Hugging Face request for the model weights is the one network call this
  // feature makes, and the UI says so.
  const wasm = env.backends?.onnx?.wasm;
  if (wasm) wasm.wasmPaths = new URL("./ort/", document.baseURI).href;

  if (loadedModelId !== model) {
    pipelinePromise = null;
    loadedModelId = model;
  }
  pipelinePromise ??= pipeline("automatic-speech-recognition", model, {
    device: support.device,
    // Quantised weights on WebGPU, full precision on WASM — a hard constraint,
    // not a tuning preference. `q8` fails outright on ONNX Runtime's WASM
    // backend for these Whisper exports (session creation dies on a missing
    // scale), while WebGPU loads them happily. Choosing q8 everywhere works on
    // the developer's machine and breaks for everyone without WebGPU.
    dtype: support.device === "webgpu" ? "q8" : "fp32",
    progress_callback: (event: { status?: string; progress?: number }) => {
      if (event.status === "progress" && typeof event.progress === "number") {
        opts.onProgress?.({
          stage: "model",
          fraction: event.progress / 100,
          note: `Downloading the speech model (${Math.round(event.progress)}%)`,
        });
      }
    },
  });

  const transcriber = (await pipelinePromise) as (
    audio: Float32Array,
    options: Record<string, unknown>,
  ) => Promise<{ text: string; chunks?: WhisperChunk[] }>;

  if (opts.signal?.aborted) throw new DOMException("aborted", "AbortError");
  opts.onProgress?.({ stage: "transcribing", fraction: null, note: "Listening to the audio" });

  const englishOnly = model.endsWith(".en");
  const result = await transcriber(audio, {
    return_timestamps: true,
    // Whisper's context is 30 seconds; longer audio is windowed, with overlap so
    // a word spanning a boundary is not lost.
    chunk_length_s: 30,
    stride_length_s: 5,
    ...(englishOnly ? {} : { language: opts.language ?? null, task: "transcribe" }),
  });

  const cues: Cue[] = [];
  for (const chunk of result.chunks ?? []) {
    const text = chunk.text.trim();
    if (!text) continue;
    const [start, end] = chunk.timestamp;
    if (typeof start !== "number") continue;
    cues.push({
      start_ms: Math.round(start * 1000),
      // A final chunk can come back with a null end; give it a plausible length
      // rather than a zero-length cue no player will show.
      end_ms: Math.round(
        (typeof end === "number" && end > start ? end : start + Math.max(0.3, text.length * 0.06)) *
          1000,
      ),
      text,
      settings: null,
    });
  }

  if (cues.length === 0) {
    throw new Error(
      "No speech was recognised. If the audio is not silent, try a larger model.",
    );
  }
  return cues;
}

/** Transcribe straight to a subtitle document. */
export async function transcribeToSubtitles(
  opts: TranscribeOptions & { format: "srt" | "vtt" },
): Promise<string> {
  return cuesToText(await transcribeToCues(opts), opts.format);
}
