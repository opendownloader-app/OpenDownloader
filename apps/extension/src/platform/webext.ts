// The single place extension code reads the WebExtension API from, and the
// `Platform` the shared engine runs on inside an extension page.
//
// Chrome does not define a `browser` global; Firefox does, natively and
// promise-based. Detecting it at runtime means one compiled bundle works on both
// browsers — only the manifest differs (background model, permission key names,
// gecko settings). See vite.config.ts and scripts/copy-static.mjs.

import type { Platform } from "@opendownloader/engine";

export const ext: typeof chrome =
  (globalThis as unknown as { browser?: typeof chrome }).browser ?? chrome;

/**
 * Saving through the extension's own downloads API.
 *
 * Unlike a web page's `<a download>`, this needs no user gesture and no
 * document, which is what lets a queue of blob-sink downloads finish
 * unattended on Firefox.
 */
/**
 * Serializes id allocation, so two downloads started together cannot pick the same id.
 *
 * Each allocation reads the rules already registered before choosing, and that read and
 * the write that follows have to happen as one step. Without this, two concurrent calls
 * both read the same maximum and both add rules numbered from it.
 */
let ruleQueue: Promise<unknown> = Promise.resolve();

/**
 * Ids that are free right now, chosen from the rules already registered.
 *
 * A plain counter was wrong, and the way it was wrong is worth keeping written down.
 * `declarativeNetRequest` session rules live as long as the browser session, but a
 * counter in a page lives only as long as that page — so reloading the manager tab
 * restarted it at 1 while the rules it had already added were still there, and the next
 * download failed with "Rule with id 2 does not have a unique ID". Two extension
 * contexts running at once collided the same way, with no reload involved.
 *
 * Reading the live rules is the only source of truth for what is taken, since they
 * outlive every counter that might be kept.
 */
async function allocateRuleIds(count: number): Promise<number[]> {
  const existing = await ext.declarativeNetRequest.getSessionRules();
  const base = existing.reduce((max, rule) => Math.max(max, rule.id), 0) + 1;
  return Array.from({ length: count }, (_, i) => base + i);
}

export const extensionPlatform: Platform = {
  canSaveSilently: true,

  /**
   * Apply headers `fetch` refuses to set, for the life of one download.
   *
   * The header that matters is `Referer`. Several platform CDNs answer 403 without one
   * naming their own site, and the Fetch standard forbids script from setting it — so
   * the only way to send it is to have the browser add it on the way out, which is what
   * a `declarativeNetRequest` rule does.
   *
   * The rule is scoped to the exact URLs being downloaded and removed as soon as the job
   * ends, successfully or not. A rule that outlived its download would silently rewrite
   * headers on unrelated requests to the same host.
   */
  async applyRequestHeaders(urls, headers) {
    // Queued behind any allocation already in flight, so the read of existing rules and
    // the write that follows it cannot interleave with another download's.
    const run = ruleQueue.then(async () => {
      const ids = await allocateRuleIds(urls.length);
      const rules = urls.map((url, index) => ({
        id: ids[index]!,
        priority: 1,
        action: {
          type: "modifyHeaders" as chrome.declarativeNetRequest.RuleActionType,
          requestHeaders: headers.map(([header, value]) => ({
            header,
            operation: "set" as chrome.declarativeNetRequest.HeaderOperation,
            value,
          })),
        },
        condition: {
          // `urlFilter` treats its argument as a pattern, and a signed media URL is full
          // of characters that mean something there. Matching the exact string is both
          // narrower and safer.
          urlFilter: `|${url}|`,
          // The engine's fetches are all XHR-class from the extension's own pages;
          // `media` is listed too because a range request the browser attributes to a
          // media element would otherwise slip past the rule.
          resourceTypes: [
            "xmlhttprequest",
            "media",
            "other",
          ] as chrome.declarativeNetRequest.ResourceType[],
        },
      }));

      // The ids are removed as well as added. They were free a moment ago, so this is a
      // no-op in the ordinary case — but it makes the call idempotent rather than an
      // error if anything did claim one in between.
      await ext.declarativeNetRequest.updateSessionRules({
        addRules: rules,
        removeRuleIds: ids,
      });
      return ids;
    });
    // The queue advances whether this succeeded or not; a failed allocation must not
    // wedge every download that follows it.
    ruleQueue = run.catch(() => undefined);

    const ids = await run;
    return async () => {
      await ext.declarativeNetRequest.updateSessionRules({
        removeRuleIds: ids,
      });
    };
  },
  async saveBlob(blob, filename) {
    const url = URL.createObjectURL(blob);
    try {
      await ext.downloads.download({ url, filename, saveAs: false });
    } finally {
      // Revoking immediately would race the download starting; the manager tab
      // outlives this by design, so a short delay is safe and bounded.
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    }
  },
};

/**
 * How an extension page tells the others that the stored queue changed.
 *
 * A `BroadcastChannel` rather than `runtime.sendMessage`, for one reason: the popup is
 * usually shouting into an empty room — there may be no manager tab open at all — and
 * `sendMessage` rejects with "Could not establish connection" when nothing is
 * listening, which would mean a caught-and-ignored error on the ordinary path. A
 * channel with no subscribers is simply silent. Both pages are the same extension
 * origin, so they share it.
 */
const JOBS_CHANGED = "opendownloader:jobs";

/**
 * Say that jobs were added or changed, for any manager tab already open.
 *
 * The queue lives in IndexedDB, which announces nothing when it is written. Without
 * this a manager tab that was already open kept showing the list it had read at load —
 * a video queued from the popup appeared only after a manual reload, and since nothing
 * ticked the queue, it did not start either.
 */
export function announceJobsChanged(): void {
  try {
    const channel = new BroadcastChannel(JOBS_CHANGED);
    channel.postMessage("changed");
    channel.close();
  } catch {
    // Not worth failing a download over. The listener side also refreshes when the tab
    // is focused, which covers this and is the same moment the user is looking.
  }
}

/** Run `onChange` whenever another extension page announces a change to the queue. */
export function onJobsChanged(onChange: () => void): void {
  try {
    new BroadcastChannel(JOBS_CHANGED).addEventListener("message", () =>
      onChange(),
    );
  } catch {
    // Same as above: focusing the tab is the backstop.
  }
}

/**
 * Open the manager tab, focusing the existing one rather than opening a second.
 *
 * The announcement is made here rather than at each of the half-dozen call sites,
 * because every one of them reaches this for the same reason — something was just
 * queued and the user is being sent to look at it. A tab that is being created reads
 * the queue as it loads and ignores the message; one that was already open is the case
 * this exists for.
 */
export async function openManagerTab(): Promise<void> {
  announceJobsChanged();
  const url = ext.runtime.getURL("manager.html");
  const existing = await ext.tabs.query({ url });
  const first = existing[0];
  if (first?.id !== undefined) {
    await ext.tabs.update(first.id, { active: true });
    if (first.windowId !== undefined) {
      await ext.windows.update(first.windowId, { focused: true });
    }
    return;
  }
  await ext.tabs.create({ url });
}
