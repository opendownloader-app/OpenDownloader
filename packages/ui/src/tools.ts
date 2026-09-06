// The local tools panel: things you can do to a file you already have.
//
// Four of them, and none of them touches the network:
//
//  - remux `.ts` segments into a playable MP4 (Rust, lossless);
//  - extract an MP4's audio track (Rust, lossless);
//  - convert between containers and codecs (WebCodecs, via mediabunny);
//  - convert and translate a subtitle file (Rust, plus the browser's on-device
//    translator when it has one).
//
// Each is a `<details>` that does nothing until it is opened, which is what
// keeps the conversion library out of the initial parse.

import {
  cuesToText,
  extractMp4Audio,
  formatSize,
  isTranslationSupported,
  parseCues,
  remuxLocalSegments,
  translateCues,
  translationAvailability,
  webPlatform,
  withExtension,
  TRANSLATION_LANGUAGES,
  type Cue,
  type Platform,
} from "@opendownloader/engine";
import type { ConvertTarget } from "@opendownloader/engine/convert";

import { checkbox, el, field, naturalSort, pickFiles } from "./dom";

export interface ToolsOptions {
  root: HTMLElement;
  platform?: Platform;
}

export function mountTools(options: ToolsOptions): void {
  const platform = options.platform ?? webPlatform;
  options.root.replaceChildren(
    el("h2", { text: "Local tools" }),
    el("p", {
      class: "muted",
      text:
        "These work on files already on this computer, and are free like everything else here. " +
        "Nothing is uploaded: the remux and audio extraction are the same Rust code the " +
        "downloader uses, and conversion runs on the browser's own codecs.",
    }),
    remuxPanel(platform),
    audioPanel(platform),
    convertPanel(platform),
    subtitlePanel(platform),
  );
}

/** A panel with a status line the actions write into. */
function panel(
  title: string,
  description: string,
  build: (status: HTMLElement) => HTMLElement[],
): HTMLElement {
  const status = el("div", { class: "muted" });
  return el(
    "details",
    { class: "panel card" },
    el("summary", { text: title }),
    el(
      "div",
      { class: "stack" },
      el("p", { class: "muted", text: description }),
      ...build(status),
      status,
    ),
  );
}

function report(status: HTMLElement, message: string, kind: "" | "bad" = ""): void {
  status.className = kind === "bad" ? "status-error" : "muted";
  status.textContent = message;
}

function remuxPanel(platform: Platform): HTMLElement {
  return panel(
    "Remux .ts segments into MP4",
    "Pick the transport-stream segments of a stream — a download that was interrupted, or " +
      "segments saved by something else — and they are joined into one playable MP4 without " +
      "re-encoding. Segments are sorted naturally, so seg2 comes before seg10.",
    (status) => {
      let audioOnly = false;
      const audio = checkbox("Audio only (.m4a)", false, (checked) => (audioOnly = checked));

      const run = async (): Promise<void> => {
        const files = naturalSort(await pickFiles(".ts,video/mp2t", true));
        if (files.length === 0) return;
        report(status, `Remuxing ${files.length} segments…`);
        try {
          const result = await remuxLocalSegments({
            files,
            audioOnly,
            onProgress: (done, total) => report(status, `Segment ${done} of ${total}…`),
          });
          await platform.saveBlob(result.blob, result.filename);
          report(
            status,
            `Saved ${result.filename} · ${formatSize(result.blob.size)} · sha256 ${result.sha256}`,
          );
        } catch (e) {
          report(status, describe(e), "bad");
        }
      };

      return [
        el(
          "div",
          { class: "row wrap" },
          el("button", { class: "primary", text: "Choose .ts segments", onClick: () => void run() }),
          audio.row,
        ),
      ];
    },
  );
}

function audioPanel(platform: Platform): HTMLElement {
  return panel(
    "Extract the audio from an MP4",
    "The AAC track is copied out as-is into an .m4a. Nothing is re-encoded, so the audio is " +
      "byte for byte what was in the video, and a large file is read in pieces rather than " +
      "loaded whole.",
    (status) => {
      const run = async (): Promise<void> => {
        const [file] = await pickFiles(".mp4,.m4v,.mov,video/mp4", false);
        if (!file) return;
        report(status, `Reading ${file.name}…`);
        try {
          const result = await extractMp4Audio({
            file,
            onProgress: (done, total) => report(status, `Chunk ${done} of ${total}…`),
          });
          await platform.saveBlob(result.blob, result.filename);
          report(
            status,
            `Saved ${result.filename} · ${formatSize(result.blob.size)} · sha256 ${result.sha256}`,
          );
        } catch (e) {
          report(status, describe(e), "bad");
        }
      };
      return [
        el("div", { class: "row" }, el("button", { class: "primary", text: "Choose an MP4", onClick: () => void run() })),
      ];
    },
  );
}

function convertPanel(platform: Platform): HTMLElement {
  return panel(
    "Convert a video or audio file",
    "Runs on the browser's own encoders, so nothing is uploaded and no converter binary is " +
      "shipped. Which formats are offered depends on what this browser can encode.",
    (status) => {
      const select = el("select");
      const button = el("button", { class: "primary", text: "Choose a file", disabled: true });
      let target: ConvertTarget = "mp3";
      let convert: typeof import("@opendownloader/engine/convert") | null = null;

      // The conversion library is large and most sessions never open this
      // panel, so it is imported the first time the panel is rendered rather
      // than at startup.
      void (async () => {
        convert = await import("@opendownloader/engine/convert");
        const available = await convert.availableTargets();
        for (const t of convert.CONVERT_TARGETS) {
          const ok = available[t.id];
          const option = el("option", {
            value: t.id,
            text: ok ? t.label : `${t.label} — not supported by this browser`,
            disabled: !ok,
          });
          select.append(option);
        }
        const first = convert.CONVERT_TARGETS.find((t) => available[t.id]);
        if (first) {
          target = first.id;
          select.value = first.id;
        }
        button.disabled = false;
      })();

      select.addEventListener("change", () => (target = select.value as ConvertTarget));

      button.addEventListener("click", () => {
        void (async () => {
          if (!convert) return;
          const [file] = await pickFiles("video/*,audio/*", false);
          if (!file) return;
          report(status, "Converting… this runs locally and can take a while.");
          try {
            const result = await convert.convertFile({
              file,
              target,
              filename: file.name,
              onProgress: (fraction) =>
                report(status, `Converting… ${Math.round(fraction * 100)}%`),
            });
            await platform.saveBlob(result.blob, result.filename);
            const lost = result.discarded.length
              ? ` (dropped: ${result.discarded.join(", ")})`
              : "";
            report(status, `Saved ${result.filename} · ${formatSize(result.blob.size)}${lost}`);
          } catch (e) {
            report(status, describe(e), "bad");
          }
        })();
      });

      return [el("div", { class: "row wrap" }, field("Convert to", select), button)];
    },
  );
}

function subtitlePanel(platform: Platform): HTMLElement {
  return panel(
    "Convert or translate subtitles",
    "Converts between WebVTT and SRT. Translation uses the browser's own on-device translator " +
      "when it has one — the text never leaves this machine, and there is no cloud fallback.",
    (status) => {
      let cues: Cue[] = [];
      let baseName = "subtitles";

      const format = el("select");
      format.append(
        el("option", { value: "srt", text: "SRT" }),
        el("option", { value: "vtt", text: "WebVTT" }),
      );

      const source = el("select");
      const targetLang = el("select");
      for (const lang of TRANSLATION_LANGUAGES) {
        source.append(el("option", { value: lang.code, text: lang.label }));
        targetLang.append(el("option", { value: lang.code, text: lang.label }));
      }
      source.value = "en";
      targetLang.value = "es";

      const saveBtn = el("button", { text: "Save", disabled: true });
      const translateBtn = el("button", {
        class: "primary",
        text: "Translate and save",
        disabled: true,
      });

      const load = async (): Promise<void> => {
        const [file] = await pickFiles(".srt,.vtt,text/vtt,text/plain", false);
        if (!file) return;
        try {
          cues = await parseCues(await file.text());
          baseName = file.name.replace(/\.[^.]+$/, "");
          saveBtn.disabled = cues.length === 0;
          translateBtn.disabled = cues.length === 0 || !isTranslationSupported();
          report(status, `${file.name}: ${cues.length} cues`);
        } catch (e) {
          report(status, describe(e), "bad");
        }
      };

      const save = async (list: Cue[], suffix: string): Promise<void> => {
        const to = format.value === "srt" ? "srt" : "vtt";
        const body = await cuesToText(list, to);
        const filename = withExtension(`${baseName}${suffix}`, to);
        await platform.saveBlob(
          new Blob([body], { type: to === "srt" ? "text/plain" : "text/vtt" }),
          filename,
        );
        report(status, `Saved ${filename} · ${list.length} cues`);
      };

      saveBtn.addEventListener("click", () => void save(cues, "").catch((e) => report(status, describe(e), "bad")));

      translateBtn.addEventListener("click", () => {
        void (async () => {
          const from = source.value;
          const to = targetLang.value;
          if (from === to) {
            report(status, "Pick two different languages.", "bad");
            return;
          }
          const availability = await translationAvailability(from, to);
          if (availability === "unsupported" || availability === "unavailable") {
            report(
              status,
              availability === "unsupported"
                ? "This browser has no on-device translator. Chrome 138 or newer has one."
                : "This browser cannot translate that language pair on-device.",
              "bad",
            );
            return;
          }
          report(
            status,
            availability === "downloadable"
              ? "Downloading the language model — this happens once."
              : "Translating…",
          );
          try {
            const translated = await translateCues(cues, {
              sourceLanguage: from,
              targetLanguage: to,
              onDownload: (fraction) =>
                report(status, `Downloading the language model… ${Math.round(fraction * 100)}%`),
              onProgress: (done, total) => report(status, `Translating cue ${done} of ${total}…`),
            });
            await save(translated, `.${to}`);
          } catch (e) {
            report(status, describe(e), "bad");
          }
        })();
      });

      const rows: HTMLElement[] = [
        el(
          "div",
          { class: "row wrap" },
          el("button", { text: "Choose a subtitle file", onClick: () => void load() }),
          field("Save as", format),
          saveBtn,
        ),
        el(
          "div",
          { class: "row wrap" },
          field("From", source),
          field("To", targetLang),
          translateBtn,
        ),
      ];

      if (!isTranslationSupported()) {
        rows.push(
          el("p", {
            class: "muted hint warn",
            text:
              "Translation is unavailable here: this browser has no built-in Translator API. " +
              "Converting between SRT and WebVTT still works.",
          }),
        );
      }
      return rows;
    },
  );
}

function describe(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
