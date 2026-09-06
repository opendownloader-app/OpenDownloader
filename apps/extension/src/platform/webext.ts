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

/** Open the manager tab, focusing the existing one rather than opening a second. */
export async function openManagerTab(): Promise<void> {
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
