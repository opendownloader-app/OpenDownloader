// Media detection.
//
// This is the one thing that genuinely belongs in the service worker: a
// non-blocking `webRequest` observer. Manifest V3 removed `webRequestBlocking`
// for non-policy-installed extensions, but left observational `webRequest`
// completely intact — so watching responses go by needs no `declarativeNetRequest`
// rules and no content script injection.
//
// Detection is opt-in per site. `<all_urls>` is declared as an *optional* host
// permission and is not granted at install, so this listener receives nothing at
// all until the user enables detection for a site from the popup. The filter
// below is not what limits visibility; the granted host permissions are.

import { ext } from "../platform/webext";
import { loadCore } from "@opendownloader/engine";
import type { DetectedItem, MediaCandidate } from "@opendownloader/engine";

/** Cap per tab, so a page issuing thousands of requests cannot grow unbounded. */
const MAX_PER_TAB = 200;

function sessionKey(tabId: number): string {
  return `candidates:${tabId}`;
}

/**
 * Candidates live in `storage.session`, not a module variable.
 *
 * An MV3 service worker is evicted after ~30s idle and its module state is
 * wiped on the next restart. A user who browses, waits, then opens the popup is
 * the *normal* case, not an edge case — holding detections in memory would make
 * the popup look empty exactly when it is most likely to be opened.
 */
async function readTab(tabId: number): Promise<DetectedItem[]> {
  const key = sessionKey(tabId);
  const stored = await ext.storage.session.get(key);
  return (stored[key] as DetectedItem[] | undefined) ?? [];
}

async function writeTab(tabId: number, items: DetectedItem[]): Promise<void> {
  await ext.storage.session.set({ [sessionKey(tabId)]: items });
  await setBadge(tabId, items.length);
}

/**
 * Serialises the read-modify-write on a tab's candidate list.
 *
 * Recording a candidate is read → append → write, and responses arrive
 * concurrently: a page that loads a video and a playlist at the same moment has
 * both handlers read the same empty list and then write over each other, so one
 * candidate is silently lost. Chaining per tab makes each update see the previous
 * one's result. (Found by running in a real browser — no unit test would have
 * produced two genuinely concurrent handler invocations.)
 */
const tabQueues = new Map<number, Promise<void>>();

function serialise(tabId: number, work: () => Promise<void>): Promise<void> {
  const previous = tabQueues.get(tabId) ?? Promise.resolve();
  // Swallow the predecessor's rejection so one failure cannot poison the chain.
  const next = previous.catch(() => undefined).then(work);
  tabQueues.set(tabId, next);
  void next.finally(() => {
    // Drop the entry once this is the tail, so the map does not grow per tab.
    if (tabQueues.get(tabId) === next) tabQueues.delete(tabId);
  });
  return next;
}

async function setBadge(tabId: number, count: number): Promise<void> {
  try {
    await ext.action.setBadgeText({ tabId, text: count > 0 ? String(count) : "" });
    await ext.action.setBadgeBackgroundColor({ tabId, color: "#15b9eb" });
  } catch {
    // The tab can close between detection and badge update; that is not an error.
  }
}

export async function listCandidates(tabId: number): Promise<DetectedItem[]> {
  return readTab(tabId);
}

export async function clearCandidates(tabId: number): Promise<void> {
  await ext.storage.session.remove(sessionKey(tabId));
  await setBadge(tabId, 0);
}

function headerValue(headers: chrome.webRequest.HttpHeader[] | undefined, name: string): string | undefined {
  return headers?.find((h) => h.name.toLowerCase() === name)?.value;
}

/**
 * The page that caused the request. Chrome reports `initiator` (an origin);
 * Firefox reports `originUrl`/`documentUrl` (full URLs). Either is enough for
 * the policy check, which only ever looks at the host.
 */
function pageOrigin(details: chrome.webRequest.WebResponseHeadersDetails): string {
  const d = details as chrome.webRequest.WebResponseHeadersDetails & {
    initiator?: string;
    originUrl?: string;
    documentUrl?: string;
  };
  return d.initiator ?? d.originUrl ?? d.documentUrl ?? "";
}

async function onHeadersReceived(
  details: chrome.webRequest.WebResponseHeadersDetails,
): Promise<void> {
  // Requests not attached to a tab (service worker fetches, prefetches) have
  // nowhere to be shown, so there is no point classifying them.
  if (details.tabId < 0) return;
  // Only successful bodies are downloadable. A 302 to the real asset will be
  // observed again at its destination.
  if (details.statusCode < 200 || details.statusCode >= 300) return;

  const headers = details.responseHeaders;
  const lengthRaw = headerValue(headers, "content-length");
  const parsedLength = lengthRaw === undefined ? NaN : Number(lengthRaw);

  const meta = {
    url: details.url,
    page_origin: pageOrigin(details),
    content_type: headerValue(headers, "content-type") ?? null,
    content_length: Number.isFinite(parsedLength) ? parsedLength : null,
    content_disposition: headerValue(headers, "content-disposition") ?? null,
  };

  const core = await loadCore();
  const json = core.classify_request(JSON.stringify(meta));
  if (!json) return;

  const candidate = JSON.parse(json) as MediaCandidate;

  await serialise(details.tabId, async () => {
    const items = await readTab(details.tabId);
    // The same asset is routinely requested more than once (range probes,
    // retries, a player reloading a playlist). Offering it N times would be noise.
    if (items.some((i) => i.url === candidate.url)) return;

    items.unshift({
      ...candidate,
      tabId: details.tabId,
      pageUrl: meta.page_origin,
      detectedAt: Date.now(),
    });
    await writeTab(details.tabId, items.slice(0, MAX_PER_TAB));
  });
}

export function attachSniffer(): void {
  ext.webRequest.onHeadersReceived.addListener(
    (details) => {
      // Fire and forget: this is a non-blocking listener, so nothing is waiting
      // on the promise, and a rejection here must not take down the worker. But
      // it must not vanish either — a swallowed failure here (a wasm module that
      // did not initialise, say) presents as "detection silently finds nothing",
      // which is indistinguishable from a page that genuinely has no media.
      void onHeadersReceived(details).catch((e: unknown) => {
        const message = e instanceof Error ? e.message : String(e);
        console.error("[opendownloader] detection failed:", message);
        void ext.storage.session.set({ snifferError: message });
      });
    },
    { urls: ["<all_urls>"] },
    ["responseHeaders"],
  );

  // A navigation replaces the page, so its detections no longer describe what
  // the user is looking at. `tabs.onUpdated` is used rather than
  // `webNavigation.onCommitted` because the `tabs` permission is already needed
  // for the popup, and adding `webNavigation` would widen the install prompt for
  // no functional gain.
  ext.tabs.onUpdated.addListener((tabId, changeInfo) => {
    if (changeInfo.url !== undefined) void clearCandidates(tabId);
  });

  ext.tabs.onRemoved.addListener((tabId) => {
    void ext.storage.session.remove(sessionKey(tabId));
  });
}
