// The popup: what was detected on this tab, and the per-site permission gate.
//
// Note what this file does *not* do — it never asks the service worker to start
// a download. Clicking an item writes a job to IndexedDB and opens the manager
// tab, so an evicted worker cannot drop a queued job on the floor.

import {
  enqueueCandidate,
  formatSize,
  jobIdFor,
  mediaHostPatterns,
  putJob,
  trackKind,
  type DetectedItem,
  type ExtractedStream,
} from "@opendownloader/engine";

import { ext, openManagerTab } from "../platform/webext";
import type { PopupResponse } from "../shared/messages";
import { initQuarkPanel } from "./quark";
import { initSitePanel } from "./site";

const listEl = document.getElementById("list") as HTMLDivElement;
const emptyEl = document.getElementById("empty") as HTMLDivElement;
const permissionEl = document.getElementById("permission") as HTMLDivElement;
const siteEl = document.getElementById("site") as HTMLSpanElement;
const grantBtn = document.getElementById("grant") as HTMLButtonElement;
const openBtn = document.getElementById("open") as HTMLButtonElement;
const clearBtn = document.getElementById("clear") as HTMLButtonElement;
const batchEl = document.getElementById("batch") as HTMLDivElement;
const selectAllEl = document.getElementById("select-all") as HTMLInputElement;
const downloadSelectedBtn = document.getElementById(
  "download-selected",
) as HTMLButtonElement;
const audioOnlyEl = document.getElementById("audio-only") as HTMLInputElement;

/** URLs the user has ticked. Kept here rather than read back off the DOM. */
const selected = new Set<string>();
let candidates: DetectedItem[] = [];

async function activeTab(): Promise<chrome.tabs.Tab | undefined> {
  const [tab] = await ext.tabs.query({ active: true, currentWindow: true });
  return tab;
}

/** The origin pattern this site's detection permission is keyed on. */
function originPattern(url: string | undefined): string | null {
  if (!url) return null;
  try {
    const parsed = new URL(url);
    // Only http(s) pages can be granted a host permission; chrome:// and
    // about: pages cannot, and asking would throw.
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:")
      return null;
    return `${parsed.protocol}//${parsed.hostname}/*`;
  } catch {
    return null;
  }
}

/** Queue one or more candidates, then hand off to the manager tab. */
async function queue(items: DetectedItem[]): Promise<void> {
  if (items.length === 0) return;
  for (const item of items) {
    await enqueueCandidate(item, {
      pageUrl: item.pageUrl,
      // Audio-only is only meaningful for a stream that carries video too.
      audioOnly: audioOnlyEl.checked && item.kind === "hlsplaylist",
    });
  }
  await openManagerTab();
  window.close();
}

/**
 * Offer the newest video track joined to the newest audio track.
 *
 * Newest of each rather than every combination: a feed page describes several videos, and
 * the one being watched is the one whose tracks were fetched last. That is a heuristic
 * and is labelled as a pairing rather than presented as the site's own rendition, so a
 * wrong guess is visible instead of silent.
 */
async function offerJoinedPair(items: DetectedItem[]): Promise<void> {
  const kinds = await Promise.all(
    items.map((i) => trackKind(i.url, i.mime ?? null)),
  );
  const newest = (want: string): DetectedItem | undefined => {
    for (let i = items.length - 1; i >= 0; i--) {
      if (kinds[i] === want) return items[i];
    }
    return undefined;
  };
  const video = newest("video");
  const audio = newest("audio");
  if (!video || !audio) return;

  const card = document.createElement("div");
  card.className = "card item row";
  const text = document.createElement("div");
  text.className = "grow";
  const top = document.createElement("div");
  top.textContent = "Video + audio, joined here";
  const bottom = document.createElement("div");
  bottom.className = "muted";
  bottom.textContent =
    "This site sends the picture and the sound separately. Either one alone is not a " +
    "watchable file; this downloads both and joins them.";
  text.append(top, bottom);

  const button = document.createElement("button");
  button.className = "primary";
  button.textContent = "Download";
  button.addEventListener("click", () => {
    button.disabled = true;
    button.textContent = "Queued";
    void queueJoined(video, audio);
  });
  card.append(text, button);
  // First, above the individual tracks.
  listEl.prepend(card);
}

/** Queue the pair as one merge job, which is the shape the engine already downloads. */
async function queueJoined(
  video: DetectedItem,
  audio: DetectedItem,
): Promise<void> {
  const now = Date.now();
  const stream = (
    item: DetectedItem,
    kind: "videoonly" | "audioonly",
  ): ExtractedStream => ({
    url: item.url,
    kind,
    mime: item.mime ?? null,
    size: item.size ?? null,
    // The sniffer saw these requests as the page made them, so whatever the host needs
    // it already got; nothing extra has to be replayed.
    headers: [],
    max_chunk: null,
  });
  await putJob({
    id: jobIdFor(`${video.url}|${audio.url}`),
    // A merge reads its two streams from `mergeStreams`; `url` is only for display.
    url: video.pageUrl ?? video.url,
    filename: "video.mp4",
    kind: "merge",
    status: "queued",
    stateJson: "",
    totalBytes: (video.size ?? 0) + (audio.size ?? 0) || null,
    receivedBytes: 0,
    outputBytes: 0,
    sha256: null,
    error: null,
    createdAt: now,
    order: now,
    pageUrl: video.pageUrl,
    mergeStreams: [stream(video, "videoonly"), stream(audio, "audioonly")],
  });
  await openManagerTab();
  window.close();
}

function renderCandidates(items: DetectedItem[]): void {
  candidates = items;
  for (const url of [...selected]) {
    if (!items.some((i) => i.url === url)) selected.delete(url);
  }

  listEl.replaceChildren();
  emptyEl.hidden = items.length > 0;
  batchEl.hidden = items.length < 2;

  // Sites that stream through MSE send the picture and the sound as two files, and the
  // listener sees two unrelated downloads. Saving either alone gives a file that
  // disappoints — silent video, or audio that will not open as a movie — so when both
  // are present the joined pair is offered first, as the thing most people came for.
  void offerJoinedPair(items);

  for (const item of items) {
    const card = document.createElement("div");
    card.className = "card item stack";

    const title = document.createElement("div");
    title.className = "row";

    const tick = document.createElement("input");
    tick.type = "checkbox";
    tick.checked = selected.has(item.url);
    tick.addEventListener("change", () => {
      if (tick.checked) selected.add(item.url);
      else selected.delete(item.url);
      updateBatchState();
    });

    const name = document.createElement("div");
    name.className = "grow truncate";
    name.textContent = item.filename;
    name.title = item.url;
    const kind = document.createElement("span");
    kind.className = "badge";
    kind.textContent = item.kind === "hlsplaylist" ? "HLS" : "file";
    title.append(tick, name, kind);

    const meta = document.createElement("div");
    meta.className = "muted";
    meta.textContent =
      item.kind === "hlsplaylist"
        ? "stream — will be remuxed to MP4"
        : `${formatSize(item.size)}${item.mime ? ` · ${item.mime}` : ""}`;

    const action = document.createElement("button");
    action.className = "primary";
    action.textContent = "Download";
    action.addEventListener("click", () => {
      void (async () => {
        action.disabled = true;
        action.textContent = "Queued";
        await queue([item]);
      })();
    });

    card.append(title, meta, action);
    listEl.append(card);
  }
  updateBatchState();
}

function updateBatchState(): void {
  const count = selected.size;
  // The bar is hidden below two candidates, so "Download all 0" is unreachable —
  // but the label is written not to depend on that.
  downloadSelectedBtn.textContent =
    count > 0
      ? `Download ${count} selected`
      : candidates.length > 0
        ? `Download all ${candidates.length}`
        : "Download all";
  selectAllEl.checked = count > 0 && count === candidates.length;
  selectAllEl.indeterminate = count > 0 && count < candidates.length;
}

async function refresh(): Promise<void> {
  const tab = await activeTab();
  if (tab?.id === undefined) return;

  // Offered independently of the permission gate below, because on a supported site the
  // extractor is the better answer and its own button explains what it needs.
  // Quark first: it has no extractor, so the site panel would not claim it, and it is
  // the one source that has to run its requests inside the tab to see the user's session.
  if (!(await initQuarkPanel(tab))) await initSitePanel(tab);

  const pattern = originPattern(tab.url);
  if (pattern) {
    // The same set the grant button asks for. Checking only the page host would call a
    // site "enabled" while the listener still cannot see its media.
    const needed = [pattern, ...(await mediaHostPatterns(tab.url ?? ""))];
    const granted = await ext.permissions.contains({ origins: needed });
    permissionEl.hidden = granted;
    siteEl.textContent = new URL(tab.url ?? "").hostname;
    if (!granted) {
      // Every other surface has to be cleared, not just left alone: a popup
      // opened on a granted site and then on an ungranted one would otherwise
      // keep showing the first site's list and its "download all" bar.
      emptyEl.hidden = true;
      batchEl.hidden = true;
      listEl.replaceChildren();
      candidates = [];
      selected.clear();
      return;
    }
  } else {
    permissionEl.hidden = true;
  }

  const response = (await ext.runtime.sendMessage({
    action: "listCandidates",
    tabId: tab.id,
  })) as PopupResponse;
  if ("candidates" in response) renderCandidates(response.candidates);
}

selectAllEl.addEventListener("change", () => {
  selected.clear();
  if (selectAllEl.checked) for (const c of candidates) selected.add(c.url);
  renderCandidates(candidates);
});

downloadSelectedBtn.addEventListener("click", () => {
  // Nothing ticked means "all of them", which is what the label says.
  const chosen =
    selected.size > 0
      ? candidates.filter((c) => selected.has(c.url))
      : candidates;
  void queue(chosen);
});

grantBtn.addEventListener("click", () => {
  void (async () => {
    const tab = await activeTab();
    const pattern = originPattern(tab?.url);
    if (!pattern) return;
    // The page's own host, plus the CDNs this site streams from. Both are needed and
    // only the first is obvious: the listener that finds a download watches network
    // requests, and the video comes from another domain entirely — `zjcdn.com` for
    // Douyin, `fbcdn.net` for Facebook. Granting only the page host leaves the popup
    // permanently empty on those sites, which reads as "nothing here to download".
    //
    // Asked for together so it is one prompt naming everything, rather than a second
    // prompt later at a moment the user cannot connect to what they clicked.
    const origins = [pattern, ...(await mediaHostPatterns(tab?.url ?? ""))];
    // `permissions.request` needs a user gesture, which this click provides.
    // Granting is per-origin, so enabling one site says nothing about any other.
    const granted = await ext.permissions.request({ origins });
    if (granted) {
      permissionEl.hidden = true;
      // The listener only starts seeing this origin's traffic from now on, so
      // the page has to be reloaded for anything already loaded to be detected.
      if (tab?.id !== undefined) await ext.tabs.reload(tab.id);
      window.close();
    }
  })();
});

openBtn.addEventListener("click", () => {
  void openManagerTab().then(() => window.close());
});

clearBtn.addEventListener("click", () => {
  void (async () => {
    const tab = await activeTab();
    if (tab?.id === undefined) return;
    await ext.runtime.sendMessage({ action: "clearCandidates", tabId: tab.id });
    selected.clear();
    await refresh();
  })();
});

void refresh();
