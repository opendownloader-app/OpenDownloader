// The manager tab.
//
// It is a thin host: the queue, the job list and the local tools all live in
// `@opendownloader/ui` so the standalone web app runs exactly the same code.
// What belongs here is only what is true of an extension page — the platform
// that saves through the downloads API, and the test hook.
//
// Downloads run in this tab rather than the service worker because an MV3
// worker is terminated after ~30s idle, after 5 minutes on a request, and if a
// `fetch()` response takes over 30s — all of which a real download violates
// routinely. A normal document has none of those limits.

import {
  audioRenditionFor,
  configureEngine,
  deleteJob,
  enqueueCandidate,
  extract,
  extractMp4Audio,
  fetchSubtitleRendition,
  isSupportedSite,
  listJobs,
  listPlaylistOptions,
  loadCore,
  remuxLocalSegments,
  updateSettings,
} from "@opendownloader/engine";
import { Manager, mountTools } from "@opendownloader/ui";

import { extensionPlatform } from "../platform/webext";

// Test-only engine configuration, applied before anything can start a download.
//
// The chunk size is shrunk drastically so a small fixture still produces many
// chunks — the multi-chunk resume path is the one worth testing, and an 8 MiB
// chunk would never reach it against a 512 KiB file. The blob sink is forced
// because automation cannot answer a native save dialog.
if (__OPENDOWNLOADER_E2E__) {
  configureEngine({ chunkSize: 64 * 1024, forceBlobSink: true });
}

const manager = new Manager({
  root: document.getElementById("manager") as HTMLElement,
  platform: extensionPlatform,
  showUrlInput: true,
  notice:
    "Downloads run in this tab. Closing it pauses them — progress is saved, and " +
    "reopening this page resumes from where it stopped.",
});

mountTools({
  root: document.getElementById("tools") as HTMLElement,
  platform: extensionPlatform,
});

// Test-only hook, installed before the first render so it exists from first
// paint. Seeding a job normally happens in the popup, which automation cannot
// open — Chrome's toolbar popup is not a tab and there is no element to click.
// This calls the same `enqueueCandidate` the popup calls, so the suite can then
// drive the *real* Start button and exercise the real engine rather than a
// stand-in for it.
if (__OPENDOWNLOADER_E2E__) {
  (globalThis as unknown as { __test: unknown }).__test = {
    enqueue: async (
      candidate: Parameters<typeof enqueueCandidate>[0],
      opts?: Parameters<typeof enqueueCandidate>[1],
    ) => {
      const job = await enqueueCandidate(candidate, opts);
      await manager.start();
      return job;
    },
    listJobs,
    manager,
    updateSettings,
    // The media pipelines, reachable without the file picker automation cannot
    // drive. Everything after the picker is the code under test.
    listPlaylistOptions: async (url: string) => listPlaylistOptions(url, await loadCore()),
    audioRenditionFor,
    fetchSubtitleRendition,
    extractMp4Audio,
    remuxLocalSegments,
    // Site extraction, driven with a page supplied by the test rather than read from a
    // tab — automation has no second tab to read, and the parsing is the part under test.
    extract,
    isSupportedSite,
    reset: async () => {
      for (const j of await listJobs()) await deleteJob(j.id);
      await manager.start();
    },
  };
}

void manager.start();

// A download in progress must survive an accidental tab close no worse than a
// pause: the state is already persisted, so warn and let the user decide.
window.addEventListener("beforeunload", (e) => {
  if (manager.hasRunningJobs()) {
    e.preventDefault();
    e.returnValue = "";
  }
});
