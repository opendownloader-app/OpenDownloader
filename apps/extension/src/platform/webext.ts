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
 * Rule ids this extension owns.
 *
 * `declarativeNetRequest` session rules are global to the extension, so ids have to be
 * unique across concurrent downloads. A counter is enough: session rules do not survive
 * a browser restart, and nothing else in this extension registers any.
 */
let nextRuleId = 1;

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
    const ids: number[] = [];
    const rules = urls.map((url) => {
      const id = nextRuleId++;
      ids.push(id);
      return {
        id,
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
      };
    });

    await ext.declarativeNetRequest.updateSessionRules({ addRules: rules, removeRuleIds: [] });
    return async () => {
      await ext.declarativeNetRequest.updateSessionRules({ removeRuleIds: ids });
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
