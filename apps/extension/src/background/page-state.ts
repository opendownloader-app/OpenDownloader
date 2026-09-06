// Reading the page the user is already looking at.
//
// The large video platforms cannot usefully be fetched from outside a browser: TikTok
// answers a bot wall, Bilibili answers 412, Facebook redirects to a login. All of that
// is about the *request*, not about the content — and the extension does not have to
// make that request, because the tab in front of the user has already made it. The page
// holds the player's own description of every rendition, because the player could not
// play without it.
//
// So this reads the loaded document rather than refetching the URL. It is the difference
// between working and not working on five of the six sites the extractors cover.

import { ext } from "../platform/webext";

/**
 * The serialized document of a tab.
 *
 * `documentElement.outerHTML` rather than a fresh fetch, and rather than reaching into
 * page variables directly: an extension's injected script runs in an isolated world with
 * no access to the page's JavaScript globals, but the inline `<script>` tags that *set*
 * those globals are right there in the markup. Every extractor parses the markup for
 * that reason, and it also means one code path covers both the server-rendered and the
 * hydrated case.
 */
export async function readPageState(tabId: number): Promise<string> {
  const results = await ext.scripting.executeScript({
    target: { tabId },
    // MAIN vs ISOLATED makes no difference here since only the markup is read, and
    // ISOLATED is the safer of the two: nothing this returns can be influenced by page
    // script redefining a global out from under it.
    world: "ISOLATED",
    func: () => document.documentElement.outerHTML,
  });

  const html = results[0]?.result;
  if (typeof html !== "string" || html.length === 0) {
    throw new Error("could not read that page — reload it and try again");
  }
  return html;
}

/**
 * Whether the extension may inject into this tab.
 *
 * `scripting.executeScript` needs host permission for the tab's origin, which is the
 * same per-site grant detection already uses. So a site the user has enabled can be read,
 * and one they have not cannot — the permission model does not change or widen for this.
 */
export async function canReadPage(url: string | undefined): Promise<boolean> {
  if (!url) return false;
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return false;
    return await ext.permissions.contains({
      origins: [`${parsed.protocol}//${parsed.hostname}/*`],
    });
  } catch {
    return false;
  }
}
