// The Quark share panel in the popup.
//
// Quark is the one source here where being signed in is the whole problem. Its API
// answers only one origin — `https://pan.quark.cn`, with `403` for every other, preflight
// included — and it releases files only to a session it recognises. The web app cannot
// satisfy either condition: a page cannot forge an origin, and the local relay it calls
// through is a separate program that holds none of the user's cookies. Signing in to
// Quark in the browser does nothing for it, which is a confusing thing to discover.
//
// The extension can satisfy both at once, and only because of where it runs the code.
// Injecting into the Quark tab means the fetch *is* the page's fetch: the origin is
// `pan.quark.cn`, and `credentials: "include"` sends the session cookies the user already
// has. Quark answers that with `access-control-allow-credentials: true`.
//
// So the panel below asks the tab, never the extension. Nothing is proxied, no cookie is
// read or copied anywhere, and a user who is not signed in gets exactly what Quark gives
// a stranger — the listing, and a refusal on the download.

import { formatSize, jobIdFor, putJob, loadCore } from "@opendownloader/engine";

import { ext, openManagerTab } from "../platform/webext";

const siteEl = document.getElementById("site-panel") as HTMLDivElement;
const siteNameEl = document.getElementById("site-name") as HTMLSpanElement;
const siteButton = document.getElementById("site-extract") as HTMLButtonElement;
const siteStatus = document.getElementById("site-status") as HTMLDivElement;
const siteOptions = document.getElementById("site-options") as HTMLDivElement;

interface QuarkEntry {
  fid: string;
  name: string;
  size: number;
  isDir: boolean;
}

interface QuarkOpened {
  stoken: string;
  title: string;
  entries: QuarkEntry[];
}

/**
 * The one function that ever talks to Quark, and it runs in the tab.
 *
 * Serialized and injected, so it closes over nothing and takes everything as arguments.
 * `world: "MAIN"` at the call site, so this is the page's own `fetch` — the alternative,
 * an isolated world, does not reliably present the page's origin, and Quark rejects any
 * other origin outright rather than degrading.
 */
async function quarkInPage(
  op: "open" | "list" | "download",
  pwdId: string,
  passcode: string,
  stoken: string,
  fid: string,
): Promise<{ ok: true; data: unknown } | { ok: false; error: string }> {
  const API = "https://drive-pc.quark.cn/1/clouddrive";
  const call = async (
    url: string,
    body?: unknown,
  ): Promise<Record<string, unknown>> => {
    const response = await fetch(url, {
      method: body === undefined ? "GET" : "POST",
      // The whole reason this runs in the page: the session travels with the request.
      credentials: "include",
      ...(body === undefined
        ? {}
        : {
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
          }),
    });
    const json = (await response.json()) as {
      code?: number;
      message?: string;
      data?: Record<string, unknown>;
    };
    if (json.code !== 0) {
      throw new Error(json.message || `Quark refused this (code ${json.code}).`);
    }
    return json.data ?? {};
  };

  const listing = async (token: string, dir: string) => {
    const query = new URLSearchParams({
      pr: "ucpro",
      fr: "pc",
      pwd_id: pwdId,
      stoken: token,
      pdir_fid: dir,
      _page: "1",
      _size: "200",
      _fetch_total: "1",
      _fetch_share: "1",
      _sort: "file_type:asc,updated_at:desc",
    });
    return call(`${API}/share/sharepage/detail?${query}`);
  };

  const entriesOf = (list: unknown): QuarkEntry[] =>
    Array.isArray(list)
      ? list.map((f: Record<string, unknown>) => ({
          fid: typeof f.fid === "string" ? f.fid : "",
          name: typeof f.file_name === "string" ? f.file_name : "(unnamed)",
          size: typeof f.size === "number" ? f.size : 0,
          isDir: f.dir === true,
        }))
      : [];

  try {
    if (op === "open") {
      const token = await call(
        `${API}/share/sharepage/token?pr=ucpro&fr=pc`,
        { pwd_id: pwdId, passcode },
      );
      const got = typeof token.stoken === "string" ? token.stoken : "";
      if (!got) throw new Error("Quark did not open that share.");
      const detail = await listing(got, "0");
      const share = detail.share as { title?: string } | undefined;
      return {
        ok: true,
        data: {
          stoken: got,
          title: share?.title ?? "Quark share",
          entries: entriesOf(detail.list),
        },
      };
    }

    if (op === "list") {
      const detail = await listing(stoken, fid);
      return { ok: true, data: entriesOf(detail.list) };
    }

    const data = await call(`${API}/file/download?pr=ucpro&fr=pc`, {
      pwd_id: pwdId,
      stoken,
      fids: [fid],
    });
    const first = Array.isArray(data) ? data[0] : data;
    const url = (first as { download_url?: string })?.download_url;
    if (!url) throw new Error("Quark returned no download URL for that file.");
    return { ok: true, data: url };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** Run {@link quarkInPage} inside the tab and unwrap its answer. */
async function inTab<T>(
  tabId: number,
  op: "open" | "list" | "download",
  pwdId: string,
  passcode: string,
  stoken: string,
  fid: string,
): Promise<T> {
  const results = await ext.scripting.executeScript({
    target: { tabId },
    world: "MAIN",
    func: quarkInPage,
    args: [op, pwdId, passcode, stoken, fid],
  });
  const result = results[0]?.result as
    | { ok: true; data: T }
    | { ok: false; error: string }
    | undefined;
  if (!result) throw new Error("could not read this page — reload it and try again");
  if (!result.ok) throw new Error(result.error);
  return result.data;
}

/** Turn one file into a job and open the manager. */
async function queueFile(
  pageUrl: string,
  name: string,
  size: number,
  url: string,
): Promise<void> {
  const now = Date.now();
  await putJob({
    id: jobIdFor(url),
    url,
    filename: name,
    kind: "progressive",
    status: "queued",
    stateJson: "",
    totalBytes: size,
    receivedBytes: 0,
    outputBytes: 0,
    sha256: null,
    error: null,
    createdAt: now,
    order: now,
    // Quark's file hosts want a Referer from the share page, and the queue derives one
    // from `pageUrl`. Without it they answer 403 on a URL that is otherwise valid.
    pageUrl,
    site: "Quark",
  });
  await openManagerTab();
  window.close();
}

/** Show one directory, with a way back up. */
function render(
  tabId: number,
  pageUrl: string,
  pwdId: string,
  share: QuarkOpened,
  title: string,
  entries: QuarkEntry[],
  trail: { name: string; entries: QuarkEntry[] }[],
): void {
  siteOptions.replaceChildren();
  siteStatus.className = "muted";
  siteStatus.textContent = trail.length
    ? [...trail.map((t) => t.name), title].slice(1).join(" / ")
    : `${entries.length} item${entries.length === 1 ? "" : "s"}`;

  if (trail.length) {
    const up = trail[trail.length - 1]!;
    siteOptions.append(
      row("↑ Up a level", up.name, "Open", () => {
        render(tabId, pageUrl, pwdId, share, up.name, up.entries, trail.slice(0, -1));
      }),
    );
  }

  // Folders first, then the largest files: a share is browsed rather than read, and the
  // thing someone came for is rarely the smallest item in it.
  const sorted = [...entries].sort((a, b) =>
    a.isDir === b.isDir ? b.size - a.size : a.isDir ? -1 : 1,
  );
  for (const entry of sorted) {
    siteOptions.append(
      entry.isDir
        ? row(`📁 ${entry.name}`, "folder", "Open", () => {
            void (async () => {
              siteStatus.textContent = `Opening ${entry.name}…`;
              try {
                const next = await inTab<QuarkEntry[]>(
                  tabId, "list", pwdId, "", share.stoken, entry.fid,
                );
                render(tabId, pageUrl, pwdId, share, entry.name, next, [
                  ...trail,
                  { name: title, entries },
                ]);
              } catch (e) {
                siteStatus.className = "status-error";
                siteStatus.textContent = e instanceof Error ? e.message : String(e);
              }
            })();
          })
        : row(entry.name, formatSize(entry.size), "Download", () => {
            void (async () => {
              siteStatus.className = "muted";
              siteStatus.textContent = `Asking Quark for ${entry.name}…`;
              try {
                const url = await inTab<string>(
                  tabId, "download", pwdId, "", share.stoken, entry.fid,
                );
                await queueFile(pageUrl, entry.name, entry.size, url);
              } catch (e) {
                siteStatus.className = "status-error";
                const message = e instanceof Error ? e.message : String(e);
                // Quark's own words for "not signed in", which do not say so.
                siteStatus.textContent = /size limit/i.test(message)
                  ? "Quark refused this file. That answer means it does not recognise a " +
                    "signed-in account on this page — sign in to Quark in this tab, " +
                    "reload, and try again."
                  : message;
              }
            })();
          }),
    );
  }
}

/** One row: description on the left, a button on the right. */
function row(
  label: string,
  meta: string,
  action: string,
  onClick: () => void,
): HTMLElement {
  const el = document.createElement("div");
  el.className = "card item row";
  const text = document.createElement("div");
  text.className = "grow";
  const top = document.createElement("div");
  top.textContent = label;
  const bottom = document.createElement("div");
  bottom.className = "muted";
  bottom.textContent = meta;
  text.append(top, bottom);
  const button = document.createElement("button");
  button.className = "primary";
  button.textContent = action;
  button.addEventListener("click", onClick);
  el.append(text, button);
  return el;
}

/**
 * Show the panel when the tab is a Quark share.
 *
 * Returns false when it is not one, so the caller falls through to the extractor panel.
 */
export async function initQuarkPanel(
  tab: chrome.tabs.Tab | undefined,
): Promise<boolean> {
  const url = tab?.url;
  const tabId = tab?.id;
  if (!url || tabId === undefined) return false;
  const core = await loadCore();
  const parsed = core.parse_quark_share(url);
  if (!parsed) return false;
  const { pwdId, passcode } = JSON.parse(parsed) as {
    pwdId: string;
    passcode: string;
  };

  siteEl.hidden = false;
  siteNameEl.textContent = "Quark";
  siteStatus.textContent = "";
  siteOptions.replaceChildren();

  const origins = [`${new URL(url).protocol}//${new URL(url).hostname}/*`];
  const granted = await ext.permissions.contains({ origins }).catch(() => false);
  siteButton.textContent = granted
    ? "See what this share holds"
    : "Allow this site, then see what it holds";

  siteButton.addEventListener("click", () => {
    void (async () => {
      siteButton.disabled = true;
      siteStatus.className = "muted";
      siteStatus.textContent = "Opening this share…";
      try {
        if (!(await ext.permissions.request({ origins }))) {
          siteStatus.textContent =
            "Reading this share needs your permission for this site. Nothing is read " +
            "until you grant it, and it can be revoked at any time.";
          return;
        }
        const share = await inTab<QuarkOpened>(
          tabId, "open", pwdId, passcode, "", "",
        );
        render(tabId, url, pwdId, share, share.title, share.entries, []);
      } catch (e) {
        siteStatus.className = "status-error";
        siteStatus.textContent = e instanceof Error ? e.message : String(e);
      } finally {
        siteButton.disabled = false;
      }
    })();
  });
  return true;
}
