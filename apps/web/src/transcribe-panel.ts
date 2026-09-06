// The transcription panel, appended to the shared tools section.
//
// Kept out of `@opendownloader/ui` because the extension does not ship it: the
// speech model dependency is large, and an extension bundle is reviewed by a
// store. The web app has neither constraint.

import { webPlatform, withExtension } from "@opendownloader/engine";
import { el, field, pickFiles } from "@opendownloader/ui";

import {
  TRANSCRIBE_MODELS,
  transcribeSupport,
  transcribeToSubtitles,
} from "./transcribe";

export function mountTranscribePanel(root: HTMLElement): void {
  const status = el("div", { class: "muted" });

  const model = el("select");
  for (const m of TRANSCRIBE_MODELS) {
    model.append(el("option", { value: m.id, text: `${m.label} · ${m.size} · ${m.note}` }));
  }
  model.value = TRANSCRIBE_MODELS[1]!.id;

  const format = el("select");
  format.append(
    el("option", { value: "srt", text: "SRT" }),
    el("option", { value: "vtt", text: "WebVTT" }),
  );

  const button = el("button", { class: "primary", text: "Choose a video or audio file" });

  button.addEventListener("click", () => {
    void (async () => {
      const [file] = await pickFiles("video/*,audio/*", false);
      if (!file) return;
      status.className = "muted";
      status.textContent = "Preparing…";
      try {
        const to = format.value === "srt" ? "srt" : "vtt";
        const text = await transcribeToSubtitles({
          file,
          model: model.value,
          format: to,
          onProgress: (p) => {
            status.textContent =
              p.fraction === null ? p.note : `${p.note} — ${Math.round(p.fraction * 100)}%`;
          },
        });
        const filename = withExtension(file.name, to);
        await webPlatform.saveBlob(
          new Blob([text], { type: to === "srt" ? "text/plain" : "text/vtt" }),
          filename,
        );
        status.textContent = `Saved ${filename}.`;
      } catch (e) {
        status.className = "status-error";
        status.textContent = e instanceof Error ? e.message : String(e);
      }
    })();
  });

  const body = el(
    "div",
    { class: "stack" },
    el("p", {
      class: "muted",
      text:
        "Generates subtitles from a file's own speech, using a model that runs here. The audio " +
        "is never uploaded. The model itself is downloaded once from Hugging Face and then " +
        "cached by the browser — that is the only network request this makes.",
    }),
    el("div", { class: "row wrap" }, field("Model", model), field("Save as", format), button),
    status,
  );

  const panel = el(
    "details",
    { class: "panel card" },
    el("summary", { text: "Make subtitles from speech" }),
    body,
  );
  root.append(panel);

  // Whether this can run at all depends on the browser, so say so up front
  // rather than after a model download.
  void transcribeSupport().then((support) => {
    if (!support.ok) {
      button.disabled = true;
      status.className = "status-error";
      status.textContent = support.reason ?? "Transcription is not available in this browser.";
      return;
    }
    if (support.device === "wasm") {
      body.insertBefore(
        el("p", {
          class: "muted hint warn",
          text:
            "This browser has no WebGPU, so the model runs on the CPU. It works, and it is " +
            "several times slower — a long recording will take a while.",
        }),
        status,
      );
    }
  });
}
