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
// So everything below asks the tab, never the extension. Nothing is proxied, no cookie is
// read or copied anywhere, and a user who is not signed in gets exactly what Quark gives
// a stranger — the listing, and a refusal on the download.
//
// # Why the whole tree, rather than a folder at a time
//
// A share is usually one thing split across folders — a release, a model pack, a season —
// and what someone wants is most of it. Walking it once up front and offering the files
// as a list to tick costs a handful of requests and turns "open, pick, go back, open" into
// one decision.

import { formatSize, jobIdFor, putJob, loadCore } from "@opendownloader/engine";

import { ext, openManagerTab } from "../platform/webext";

const siteEl = document.getElementById("site-panel") as HTMLDivElement;
const siteNameEl = document.getElementById("site-name") as HTMLSpanElement;
const siteButton = document.getElementById("site-extract") as HTMLButtonElement;
const siteStatus = document.getElementById("site-status") as HTMLDivElement;
const siteOptions = document.getElementById("site-options") as HTMLDivElement;

/** One file anywhere in the share, with the folders it sits under. */
interface QuarkFile {
  fid: string;
  /** Folders joined with "/", then the filename. Shown so two same-named files differ. */
  path: string;
  name: string;
  size: number;
}

interface QuarkTree {
  title: string;
  files: QuarkFile[];
  /** True when a limit stopped the walk, so the list is not the whole share. */
  truncated: boolean;
}

interface Resolved {
  fid: string;
  url: string;
}

/**
 * The one function that ever talks to Quark, and it runs in the tab.
 *
 * Serialized and injected, so it closes over nothing and takes everything as arguments —
 * a reference to any module-scope helper would survive type checking and fail at runtime
 * in the page, where nothing here can see it.
 *
 * `world: "MAIN"` at the call site, so this is the page's own `fetch`. An isolated world
 * does not reliably present the page's origin, and Quark rejects any other origin
 * outright rather than degrading.
 */
async function quarkInPage(
  op: "tree" | "download",
  pwdId: string,
  passcode: string,
  fids: string[],
): Promise<{ ok: true; data: unknown } | { ok: false; error: string }> {
  const API = "https://drive-pc.quark.cn/1/clouddrive";
  // Bounds, so a hostile or enormous share cannot spin here forever. Each is generous
  // enough that a real share reaches none of them.
  const MAX_FILES = 2000;
  const MAX_FOLDERS = 400;
  const MAX_DEPTH = 16;

  const call = async (
    url: string,
    body?: unknown,
  ): Promise<{ data: Record<string, unknown>; total: number }> => {
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
      metadata?: { _total?: number };
    };
    if (json.code !== 0) {
      throw new Error(json.message || `Quark refused this (code ${json.code}).`);
    }
    return { data: json.data ?? {}, total: json.metadata?._total ?? 0 };
  };

  const token = await call(`${API}/share/sharepage/token?pr=ucpro&fr=pc`, {
    pwd_id: pwdId,
    passcode,
  });
  const stoken = typeof token.data.stoken === "string" ? token.data.stoken : "";
  if (!stoken) throw new Error("Quark did not open that share.");

  try {
    if (op === "download") {
      const { data } = await call(`${API}/file/download?pr=ucpro&fr=pc`, {
        pwd_id: pwdId,
        stoken,
        fids,
      });
      const list = (Array.isArray(data) ? data : []) as {
        fid?: string;
        download_url?: string;
      }[];
      return {
        ok: true,
        data: list
          .filter((f) => f.fid && f.download_url)
          .map((f) => ({ fid: f.fid!, url: f.download_url! })),
      };
    }

    // ---- walk every folder ------------------------------------------------
    // One page at a time, following `_total` rather than guessing: a folder with more
    // entries than one page holds would otherwise be silently half-read, which is the
    // kind of bug nobody notices until a file is missing from a download.
    const files: QuarkFile[] = [];
    let title = "Quark share";
    let folders = 0;
    let truncated = false;

    const queue: { fid: string; prefix: string; depth: number }[] = [
      { fid: "0", prefix: "", depth: 0 },
    ];

    while (queue.length > 0) {
      const dir = queue.shift()!;
      if (dir.depth > MAX_DEPTH || folders > MAX_FOLDERS) {
        truncated = true;
        break;
      }
      folders += 1;

      let page = 1;
      let seen = 0;
      let total = 0;
      do {
        const query = new URLSearchParams({
          pr: "ucpro",
          fr: "pc",
          pwd_id: pwdId,
          stoken,
          pdir_fid: dir.fid,
          _page: String(page),
          _size: "200",
          _fetch_total: "1",
          _fetch_share: dir.depth === 0 ? "1" : "0",
          _sort: "file_type:asc,updated_at:desc",
        });
        const { data, total: reported } = await call(
          `${API}/share/sharepage/detail?${query}`,
        );
        total = reported;
        if (dir.depth === 0) {
          const share = data.share as { title?: string } | undefined;
          if (share?.title) title = share.title;
        }
        const list = (Array.isArray(data.list) ? data.list : []) as Record<
          string,
          unknown
        >[];
        if (list.length === 0) break;
        seen += list.length;

        for (const raw of list) {
          const name =
            typeof raw.file_name === "string" ? raw.file_name : "(unnamed)";
          const fid = typeof raw.fid === "string" ? raw.fid : "";
          if (raw.dir === true) {
            queue.push({
              fid,
              prefix: dir.prefix ? `${dir.prefix}/${name}` : name,
              depth: dir.depth + 1,
            });
          } else if (files.length < MAX_FILES) {
            files.push({
              fid,
              name,
              path: dir.prefix ? `${dir.prefix}/${name}` : name,
              size: typeof raw.size === "number" ? raw.size : 0,
            });
          } else {
            truncated = true;
          }
        }
        page += 1;
        // 25 pages of 200 is 5000 entries in one folder; past that something is wrong.
      } while (seen < total && page <= 25);
    }

    return { ok: true, data: { title, files, truncated } };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** Run {@link quarkInPage} inside the tab and unwrap its answer. */
async function inTab<T>(
  tabId: number,
  op: "tree" | "download",
  pwdId: string,
  passcode: string,
  fids: string[],
): Promise<T> {
  const results = await ext.scripting.executeScript({
    target: { tabId },
    world: "MAIN",
    func: quarkInPage,
    args: [op, pwdId, passcode, fids],
  });
  const result = results[0]?.result as
    | { ok: true; data: T }
    | { ok: false; error: string }
    | undefined;
  if (!result) {
    throw new Error("could not read this page — reload it and try again");
  }
  if (!result.ok) throw new Error(result.error);
  return result.data;
}

/**
 * Filenames for a selection, disambiguated only where they collide.
 *
 * Two folders in a model pack routinely hold a `config.json`, and saving both under one
 * name means the second silently replaces the first. Prefixing every file with its folder
 * would be noisier for the common case, so it happens only where the name is not unique.
 */
function namesFor(files: QuarkFile[]): Map<string, string> {
  const counts = new Map<string, number>();
  for (const f of files) counts.set(f.name, (counts.get(f.name) ?? 0) + 1);
  const out = new Map<string, string>();
  for (const f of files) {
    const unique = (counts.get(f.name) ?? 0) < 2;
    out.set(
      f.fid,
      unique ? f.name : f.path.replace(/\//g, " - ").replace(/[\\:*?"<>|]/g, "_"),
    );
  }
  return out;
}

/** Queue one file. */
async function queueFile(
  pageUrl: string,
  filename: string,
  size: number,
  url: string,
): Promise<void> {
  const now = Date.now();
  await putJob({
    id: jobIdFor(url),
    url,
    filename,
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
}

/** The whole share as a list to tick. */
function renderTree(
  tabId: number,
  pageUrl: string,
  pwdId: string,
  tree: QuarkTree,
): void {
  siteOptions.replaceChildren();
  const chosen = new Set(tree.files.map((f) => f.fid));

  const totalOf = (fids: Set<string>) =>
    tree.files.reduce((n, f) => (fids.has(f.fid) ? n + f.size : n), 0);

  const action = document.createElement("button");
  action.className = "primary";

  const refresh = (): void => {
    action.disabled = chosen.size === 0;
    action.textContent =
      chosen.size === 0
        ? "Nothing selected"
        : `Download ${chosen.size} file${chosen.size === 1 ? "" : "s"} · ${formatSize(totalOf(chosen))}`;
  };

  // Select-all first, because "everything" is what most people want and it should not
  // require ticking twenty boxes to express.
  const allRow = document.createElement("label");
  allRow.className = "card item row";
  const allBox = document.createElement("input");
  allBox.type = "checkbox";
  allBox.checked = true;
  const allText = document.createElement("div");
  allText.className = "grow";
  const allTop = document.createElement("div");
  allTop.textContent = "Select all";
  const allBottom = document.createElement("div");
  allBottom.className = "muted";
  allBottom.textContent = `${tree.files.length} file${tree.files.length === 1 ? "" : "s"} · ${formatSize(totalOf(new Set(tree.files.map((f) => f.fid))))}`;
  allText.append(allTop, allBottom);
  allRow.append(allBox, allText);
  siteOptions.append(allRow);

  const boxes: HTMLInputElement[] = [];
  // Largest first: in a release folder the thing someone came for is rarely the smallest.
  for (const file of [...tree.files].sort((a, b) => b.size - a.size)) {
    const label = document.createElement("label");
    label.className = "card item row";
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = true;
    box.addEventListener("change", () => {
      if (box.checked) chosen.add(file.fid);
      else chosen.delete(file.fid);
      allBox.checked = chosen.size === tree.files.length;
      allBox.indeterminate = chosen.size > 0 && chosen.size < tree.files.length;
      refresh();
    });
    boxes.push(box);

    const text = document.createElement("div");
    text.className = "grow";
    const top = document.createElement("div");
    top.textContent = file.name;
    const bottom = document.createElement("div");
    bottom.className = "muted";
    // The folder path, not just the size: it is what distinguishes two files that
    // share a name, and what tells someone which half of a pack they are taking.
    bottom.textContent = file.path.includes("/")
      ? `${formatSize(file.size)} · ${file.path.slice(0, file.path.lastIndexOf("/"))}`
      : formatSize(file.size);
    text.append(top, bottom);
    label.append(box, text);
    siteOptions.append(label);
  }

  allBox.addEventListener("change", () => {
    chosen.clear();
    if (allBox.checked) for (const f of tree.files) chosen.add(f.fid);
    for (const b of boxes) b.checked = allBox.checked;
    refresh();
  });

  action.addEventListener("click", () => {
    void download(tabId, pageUrl, pwdId, tree, chosen, action);
  });
  const actionRow = document.createElement("div");
  actionRow.className = "row";
  actionRow.append(action);
  siteOptions.append(actionRow);

  refresh();
  siteStatus.className = "muted";
  siteStatus.textContent = tree.truncated
    ? `${tree.title} — showing the first ${tree.files.length} files; this share is larger than that.`
    : tree.title;
}

/** Resolve the chosen files and queue them. */
async function download(
  tabId: number,
  pageUrl: string,
  pwdId: string,
  tree: QuarkTree,
  chosen: Set<string>,
  action: HTMLButtonElement,
): Promise<void> {
  const files = tree.files.filter((f) => chosen.has(f.fid));
  const names = namesFor(files);
  action.disabled = true;
  siteStatus.className = "muted";

  // In batches rather than one request per file: Quark's download endpoint takes a list,
  // and asking once for forty files is one round trip instead of forty.
  const BATCH = 20;
  let queued = 0;
  const failures: string[] = [];

  for (let i = 0; i < files.length; i += BATCH) {
    const batch = files.slice(i, i + BATCH);
    siteStatus.textContent = `Asking Quark for ${i + 1}–${Math.min(i + BATCH, files.length)} of ${files.length}…`;
    try {
      const resolved = await inTab<Resolved[]>(
        tabId,
        "download",
        pwdId,
        "",
        batch.map((f) => f.fid),
      );
      const byFid = new Map(resolved.map((r) => [r.fid, r.url]));
      for (const file of batch) {
        const url = byFid.get(file.fid);
        if (!url) {
          failures.push(file.name);
          continue;
        }
        await queueFile(pageUrl, names.get(file.fid) ?? file.name, file.size, url);
        queued += 1;
      }
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      // Quark's own words for "not signed in", which do not say so.
      if (/size limit/i.test(message)) {
        siteStatus.className = "status-error";
        siteStatus.textContent =
          "Quark refused these files. That answer means it does not recognise a " +
          "signed-in account on this page — sign in to Quark in this tab, reload, " +
          "and try again.";
        action.disabled = false;
        return;
      }
      failures.push(...batch.map((f) => f.name));
    }
  }

  if (queued === 0) {
    siteStatus.className = "status-error";
    siteStatus.textContent = `Quark released none of those ${files.length} files.`;
    action.disabled = false;
    return;
  }

  // Partial success is reported rather than rounded up: a download that quietly drops
  // three of twelve files is worse than one that says which three.
  if (failures.length > 0) {
    siteStatus.className = "status-error";
    siteStatus.textContent = `Queued ${queued}; Quark refused ${failures.length}: ${failures.slice(0, 3).join(", ")}${failures.length > 3 ? "…" : ""}`;
    action.disabled = false;
    await openManagerTab();
    return;
  }

  await openManagerTab();
  window.close();
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
    ? "List everything in this share"
    : "Allow this site, then list everything in it";

  siteButton.addEventListener("click", () => {
    void (async () => {
      siteButton.disabled = true;
      siteStatus.className = "muted";
      siteStatus.textContent = "Walking every folder in this share…";
      try {
        if (!(await ext.permissions.request({ origins }))) {
          siteStatus.textContent =
            "Reading this share needs your permission for this site. Nothing is read " +
            "until you grant it, and it can be revoked at any time.";
          return;
        }
        const tree = await inTab<QuarkTree>(tabId, "tree", pwdId, passcode, []);
        if (tree.files.length === 0) {
          siteStatus.textContent = "This share holds no files.";
          return;
        }
        renderTree(tabId, url, pwdId, tree);
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
