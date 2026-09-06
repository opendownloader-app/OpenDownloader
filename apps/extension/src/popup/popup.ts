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

/**
 * The current tab's title, remembered when the popup loads.
 *
 * Read once rather than per render: it names both the joined card and the file the job
 * saves as, and those two must not be able to disagree.
 */
let pageTitle: string | null = null;

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
 * The newest video track and the newest audio track, when the page has both.
 *
 * Newest of each rather than every combination: a feed page describes several videos, and
 * the one being watched is the one whose tracks were fetched last.
 */
async function findJoinablePair(
  items: DetectedItem[],
): Promise<{ video: DetectedItem; audio: DetectedItem } | null> {
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
  return video && audio ? { video, audio } : null;
}

/** The joined pair, rendered as the page's one answer. */
function joinedCard(pair: {
  video: DetectedItem;
  audio: DetectedItem;
}): HTMLElement {
  const card = document.createElement("div");
  card.className = "card item stack";

  const title = document.createElement("div");
  title.className = "row";
  const name = document.createElement("div");
  name.className = "grow truncate";
  // The page's title, not either track's filename: both are opaque CDN ids, and the
  // title is what the file will be saved as.
  name.textContent = pageTitle ?? "This video";
  const badge = document.createElement("span");
  badge.className = "badge";
  badge.textContent = "video";
  title.append(name, badge);

  const meta = document.createElement("div");
  meta.className = "muted";
  // No size, deliberately. What the listener saw is the chunk the player asked for, not
  // the file: on the reel this was built against those chunks totalled 11 KB while the
  // real tracks were 364 MB and 56 MB. A number that wrong is worse than no number —
  // someone sizes a download by it and is misled by three orders of magnitude.
  meta.textContent =
    "picture and sound arrive separately here; both are downloaded and joined";

  const action = document.createElement("button");
  action.className = "primary";
  action.textContent = "Download";
  action.addEventListener("click", () => {
    action.disabled = true;
    action.textContent = "Queued";
    void queueJoined(pair.video, pair.audio);
  });

  card.append(title, meta, action);
  return card;
}

/** A page title turned into a filename, or null when there is nothing usable in it. */
function safeFilename(title: string | undefined): string | null {
  if (!title) return null;
  const cleaned = title
    .replace(/[/\\:*?"<>|]/g, "_")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 120)
    .trim();
  return cleaned ? `${cleaned}.mp4` : null;
}

/** Queue the pair as one merge job, which is the shape the engine already downloads. */
async function queueJoined(
  video: DetectedItem,
  audio: DetectedItem,
): Promise<void> {
  const now = Date.now();
  // The tab's own title, because neither track carries one: these URLs are opaque ids on
  // a CDN, and "video.mp4" for every download from every site is a folder nobody can
  // read later.
  const filename = safeFilename(pageTitle ?? undefined) ?? "video.mp4";
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
    filename,
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

async function renderCandidates(items: DetectedItem[]): Promise<void> {
  candidates = items;
  for (const url of [...selected]) {
    if (!items.some((i) => i.url === url)) selected.delete(url);
  }

  listEl.replaceChildren();
  emptyEl.hidden = items.length > 0;

  // Sites that stream through MSE send the picture and the sound as two files, and the
  // listener sees two unrelated downloads. Saving either alone gives a file that
  // disappoints: silent video, or audio that will not open as a movie.
  //
  // So when both are present the joined pair *is* the answer, and it is rendered as the
  // one thing on offer. The tracks it is made of, and whatever else the page fetched,
  // go under a fold — listing them alongside turns one obvious choice into eight
  // similar-looking rows, which is how someone ends up downloading half a video.
  const pair = await findJoinablePair(items);
  const rest = pair
    ? items.filter((i) => i !== pair.video && i !== pair.audio)
    : items;

  if (pair) listEl.append(joinedCard(pair));

  // The batch bar acts on the individual files, so it is only useful when they are the
  // thing being chosen from.
  batchEl.hidden = pair !== null || rest.length < 2;

  const host = pair ? foldFor(rest.length) : listEl;
  for (const item of rest) {
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
    host.append(card);
  }
  if (host !== listEl) listEl.append(host.parentElement ?? host);
  updateBatchState();
}

/**
 * A collapsed section for the files that are not the answer.
 *
 * Present rather than hidden: the pairing is a guess about which two tracks belong
 * together, and on a busy feed page it can pair the wrong ones. Someone who needs to
 * correct that has to be able to see the parts.
 */
function foldFor(count: number): HTMLElement {
  // `panel` is the design system's disclosure: it draws the caret and hides the
  // browser's default marker, so this matches every other fold in the product.
  const details = document.createElement("details");
  details.className = "panel stack";
  const summary = document.createElement("summary");
  summary.textContent = `Other files on this page (${count})`;
  const body = document.createElement("div");
  body.className = "stack";
  details.append(summary, body);
  return body;
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
  pageTitle = tab.title?.trim() || null;
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
  if ("candidates" in response) await renderCandidates(response.candidates);
}

selectAllEl.addEventListener("change", () => {
  selected.clear();
  if (selectAllEl.checked) for (const c of candidates) selected.add(c.url);
  void renderCandidates(candidates);
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
