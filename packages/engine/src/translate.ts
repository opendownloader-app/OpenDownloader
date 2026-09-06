// Translating subtitles, on this machine.
//
// Chrome 138+ ships a Translator API backed by models the browser downloads and
// runs locally: no key, no server, no cost, and the text never leaves the
// machine. That is the only translation this product does. A cloud translation
// API would be the obvious alternative and is deliberately not used — it would
// turn a local tool into a metered one and put subtitle text on someone else's
// server.
//
// Everything here is feature-detected. On a browser without the API the UI says
// so plainly rather than offering a button that fails.

import type { Cue } from "./types";

interface TranslatorInstance {
  translate(text: string): Promise<string>;
  destroy?: () => void;
}

interface TranslatorApi {
  availability(o: {
    sourceLanguage: string;
    targetLanguage: string;
  }): Promise<"unavailable" | "downloadable" | "downloading" | "available">;
  create(o: {
    sourceLanguage: string;
    targetLanguage: string;
    monitor?: (m: EventTarget) => void;
  }): Promise<TranslatorInstance>;
}

function api(): TranslatorApi | undefined {
  return (globalThis as unknown as { Translator?: TranslatorApi }).Translator;
}

export function isTranslationSupported(): boolean {
  return api() !== undefined;
}

export type TranslationAvailability =
  | "unsupported"
  | "unavailable"
  | "downloadable"
  | "downloading"
  | "available";

/** Whether this browser can translate this pair, and whether it must download first. */
export async function translationAvailability(
  sourceLanguage: string,
  targetLanguage: string,
): Promise<TranslationAvailability> {
  const translator = api();
  if (!translator) return "unsupported";
  try {
    return await translator.availability({ sourceLanguage, targetLanguage });
  } catch {
    return "unavailable";
  }
}

export interface TranslateOptions {
  sourceLanguage: string;
  targetLanguage: string;
  signal?: AbortSignal;
  /** Model download progress, 0–1, before any translation happens. */
  onDownload?: (fraction: number) => void;
  onProgress?: (done: number, total: number) => void;
}

/**
 * Translate every cue, keeping the timings exactly as they were.
 *
 * Cues are translated one at a time rather than as one blob. It is slower, and
 * it is the only version that cannot go wrong: a single request whose response
 * comes back with a different number of lines would silently shift every
 * subtitle after the first mismatch.
 */
export async function translateCues(cues: Cue[], opts: TranslateOptions): Promise<Cue[]> {
  const translator = api();
  if (!translator) {
    throw new Error(
      "This browser has no on-device translator. Chrome 138 or newer has one; " +
        "nothing is sent to a server, so there is no fallback to offer here.",
    );
  }

  const instance = await translator.create({
    sourceLanguage: opts.sourceLanguage,
    targetLanguage: opts.targetLanguage,
    monitor: (m) => {
      m.addEventListener("downloadprogress", (e) => {
        const loaded = (e as Event & { loaded?: number }).loaded;
        if (typeof loaded === "number") opts.onDownload?.(loaded);
      });
    },
  });

  try {
    const out: Cue[] = [];
    for (const [i, cue] of cues.entries()) {
      if (opts.signal?.aborted) throw new DOMException("aborted", "AbortError");
      // An empty or whitespace-only cue has nothing to translate and some
      // backends reject it outright.
      const text = cue.text.trim() ? await instance.translate(cue.text) : cue.text;
      out.push({ ...cue, text });
      opts.onProgress?.(i + 1, cues.length);
    }
    return out;
  } finally {
    instance.destroy?.();
  }
}

/**
 * Language codes the UI offers.
 *
 * Deliberately short: the browser decides what it can actually do (see
 * `translationAvailability`), and a list of a hundred codes most of which
 * report "unavailable" is a worse UI than a dozen that mostly work.
 */
export const TRANSLATION_LANGUAGES: { code: string; label: string }[] = [
  { code: "en", label: "English" },
  { code: "es", label: "Spanish" },
  { code: "fr", label: "French" },
  { code: "de", label: "German" },
  { code: "it", label: "Italian" },
  { code: "pt", label: "Portuguese" },
  { code: "ru", label: "Russian" },
  { code: "ar", label: "Arabic" },
  { code: "hi", label: "Hindi" },
  { code: "ja", label: "Japanese" },
  { code: "ko", label: "Korean" },
  { code: "zh", label: "Chinese (Simplified)" },
  { code: "zh-Hant", label: "Chinese (Traditional)" },
  { code: "vi", label: "Vietnamese" },
  { code: "th", label: "Thai" },
  { code: "id", label: "Indonesian" },
  { code: "tr", label: "Turkish" },
  { code: "nl", label: "Dutch" },
  { code: "pl", label: "Polish" },
];
