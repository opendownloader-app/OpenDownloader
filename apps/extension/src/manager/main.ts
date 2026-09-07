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
  putJob,
  resolveVimeoManifest,
  vimeoStreams,
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
import { Manager, candidateForUrl, mountTools } from "@opendownloader/ui";

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
  addLink: addPastedLink,
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
    listPlaylistOptions: async (url: string) =>
      listPlaylistOptions(url, await loadCore()),
    audioRenditionFor,
    fetchSubtitleRendition,
    extractMp4Audio,
    remuxLocalSegments,
    // Site extraction, driven with a page supplied by the test rather than read from a
    // tab — automation has no second tab to read, and the parsing is the part under test.
    extract,
    isSupportedSite,
    // Vimeo's JSON adaptive path, which the popup drives from a toolbar click that
    // automation cannot produce reliably — the button needs a focused window.
    resolveVimeoManifest,
    vimeoStreams,
    putJob,
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

/**
 * Where a torrent bridge might be listening on this machine.
 *
 * Two shapes, because there are two ways to have one. The app serves everything on one
 * port and mounts the bridge under a path — and it walks up from 5180 when that port is
 * taken, so a few are worth trying. The standalone `dl-torrent` sits on 8089.
 *
 * An extension page may talk to loopback, which is what makes any of this possible here:
 * a page on https may not, whatever is running.
 */
const BRIDGE_CANDIDATES = [
  "http://127.0.0.1:8089",
  // The same span the app itself walks when its preferred port is taken. Five was not
  // enough: with two stale servers holding 5180 and 5181 the app landed on 5182, and one
  // more collision would have put it past the end of this list. A probe range narrower
  // than where the app can be is a bridge that is running and not found.
  ...Array.from(
    { length: 12 },
    (_, i) => `http://127.0.0.1:${5180 + i}/torrent-bridge`,
  ),
];

/**
 * Ask the browser to start the bridge, and get back the port it is on.
 *
 * This is the arrangement with nothing to run: the app registers itself as a native
 * messaging host when it is first opened, and from then on the *browser* starts the
 * bridge on demand and stops it when this page lets go. The app itself need not be
 * running, and nothing is typed anywhere.
 *
 * Returns null when no host is registered — the app has never been opened, or is not
 * installed — so the caller falls back to looking for one already listening.
 */
async function startBridgeViaBrowser(): Promise<string | null> {
  if (!chrome.runtime?.connectNative) return null;
  return new Promise((resolve) => {
    let port: chrome.runtime.Port;
    try {
      port = chrome.runtime.connectNative("app.opendownloader.bridge");
    } catch {
      return resolve(null);
    }
    // Held open deliberately: the host lives as long as this port does, so dropping it
    // would stop the bridge in the middle of the download it was started for.
    nativePort = port;

    const settle = (value: string | null) => {
      if (value === null && nativePort === port) nativePort = null;
      resolve(value);
    };
    port.onMessage.addListener(
      (message: { ok?: boolean; url?: string; error?: string }) => {
        // A full URL rather than a port: the host may hand back a bridge that is already
        // running, and those are not all mounted at the same path.
        settle(message?.ok && message.url ? message.url : null);
      },
    );
    port.onDisconnect.addListener(() => settle(null));
    port.postMessage({ type: "start" });
    // A host that is registered but broken would otherwise hang this forever.
    setTimeout(() => settle(null), 4000);
  });
}

/** Kept for the life of the page, because the bridge stops when this closes. */
let nativePort: chrome.runtime.Port | null = null;

/** The first bridge that answers, or null. Probed together so this costs one wait. */
async function findBridge(): Promise<string | null> {
  const probes = BRIDGE_CANDIDATES.map(async (base) => {
    const response = await fetch(`${base}/healthz`, {
      signal: AbortSignal.timeout(1200),
    });
    const health = (await response.json()) as { service?: string };
    if (health.service !== "dl-torrent") throw new Error("not the bridge");
    return base;
  });
  // `any` rather than `all`: the first that answers wins and the rest are irrelevant.
  return Promise.any(probes).catch(() => null);
}

/**
 * Handle a link pasted into the manager's box.
 *
 * It used to fetch whatever it was given as a file, so a magnet was refused here while
 * the very same magnet worked in the web app — two boxes that look alike and behave
 * differently, and this is the one that sits beside the downloads.
 */
async function addPastedLink(url: string): Promise<void> {
  const isPeerLink = /^magnet:/i.test(url) || /\.torrent(\?|$)/i.test(url);
  if (!isPeerLink) {
    await manager.enqueue(await candidateForUrl(url), { start: true });
    return;
  }

  // The browser-started host first: it needs nothing to be running. Falling back to a
  // bridge already listening covers the app being open, or the standalone binary.
  const bridge = (await startBridgeViaBrowser()) ?? (await findBridge());
  if (!bridge) {
    throw new Error(
      "A magnet names content on other people's machines, and a browser tab cannot " +
        "connect to them. The OpenDownloader app can. Install it and open it once — " +
        "that is all it needs; after that this page starts it by itself whenever a " +
        "torrent is pasted, and it does not have to be running.",
    );
  }

  const response = await fetch(`${bridge}/torrent`, {
    method: "POST",
    body: url,
  });
  if (!response.ok) {
    const detail = (await response.json().catch(() => null)) as {
      error?: string;
    } | null;
    throw new Error(detail?.error ?? `the bridge answered ${response.status}`);
  }
  const torrent = (await response.json()) as {
    name: string;
    files: { index: number; name: string; length: number; url: string }[];
  };
  if (torrent.files.length === 0) {
    throw new Error(`${torrent.name} contains no files.`);
  }

  // Every file, largest first. A torrent is a thing someone asked for whole, and picking
  // one of them here would be guessing — the queue shows them all and each can be
  // removed.
  for (const file of [...torrent.files].sort((a, b) => b.length - a.length)) {
    await manager.enqueue(
      {
        url: `${bridge}${file.url}`,
        kind: "progressive",
        // A torrent path can be `Season 1/ep01.mkv`; only the last segment is a name.
        filename: file.name.split("/").pop() ?? file.name,
        mime: null,
        size: file.length,
      },
      { start: true },
    );
  }
}
