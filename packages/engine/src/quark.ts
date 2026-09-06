// Reading a Quark netdisk share.
//
// A share link names a directory tree that Quark will describe to anyone who asks — the
// same thing a visitor sees when they open the page — and this reads it: names, sizes and
// structure, without an account.
//
// It stops there, deliberately. Turning a file id into a download URL is Quark's third
// step, and that one refuses a caller with no session: `code 23018`, measured against
// files of 155 MB, 605 MB and 61 GB in the same share, so it is the missing account and
// not the size of any one file. What lifts it is signing in to Quark, and the speed that
// follows is the one the account is entitled to. There is no version of this module that
// gets around that, and none is wanted.
//
// Every call here goes through the relay. Quark's API allows exactly one origin, its own,
// and answers `403` to any other — including the preflight, which is why no header a page
// can set makes it work. The relay is a program on the user's own machine and sends no
// `Origin` at all.

import { fetchWithRetry } from "./fetch-retry";
import { loadCore } from "./wasm";

const QUARK_API = "https://drive-pc.quark.cn/1/clouddrive";
const QUARK_PARAMS = "pr=ucpro&fr=pc";

/** What Quark's own web client calls itself. Its API is terse with anything else. */
const QUARK_UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 " +
  "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/** One entry in a share — a file to download, or a directory to open. */
export interface QuarkEntry {
  fid: string;
  name: string;
  /** Bytes. Zero for a directory. */
  size: number;
  isDir: boolean;
}

/** An opened share: its title, a session token, and the top of its tree. */
export interface QuarkShare {
  pwdId: string;
  /** Quark's session token for this share. Needed by every listing call. */
  stoken: string;
  title: string;
  entries: QuarkEntry[];
}

/** Whether this URL is a Quark share link. */
export async function isQuarkShare(url: string): Promise<boolean> {
  const core = await loadCore();
  return core.parse_quark_share(url) !== undefined;
}

/**
 * Thrown when the share was read but Quark will not hand over the bytes.
 *
 * Its own type because it is not a failure of this code and not something a retry fixes —
 * the caller should say what it means rather than render it as an error.
 */
export class QuarkNeedsAccount extends Error {
  constructor(readonly filename: string) {
    super(
      `Quark will not release "${filename}" to a visitor who is not signed in — it ` +
        `answers the download request with a size limit whatever the file's size. ` +
        `Opening the share in a browser where you are signed in to Quark, and saving ` +
        `the file from there, is the way to get it at the speed your account allows.`,
    );
    this.name = "QuarkNeedsAccount";
  }
}

/** Quark's replies are `{status, code, message, data}`; anything else is a broken hop. */
async function quarkCall(
  url: string,
  init?: { method: "POST"; body: string },
): Promise<Record<string, unknown>> {
  const response = await fetchWithRetry(url, {
    ...(init ? { method: init.method, body: init.body } : {}),
    headers: {
      "Content-Type": "application/json",
      // Forbidden to a page; the relay puts them back on the wire.
      "x-relay-referer": "https://pan.quark.cn/",
      "x-relay-user-agent": QUARK_UA,
    },
  });
  if (response.status === 403) {
    throw new Error(
      "Quark refused that request outright. Its API answers only its own site, so this " +
        "needs the local relay running — start it with `npm start` and try again.",
    );
  }
  const body = (await response.json().catch(() => null)) as {
    code?: number;
    message?: string;
    data?: unknown;
  } | null;
  if (!body)
    throw new Error(`Quark answered ${response.status} with nothing readable.`);
  if (body.code !== 0) {
    throw new Error(
      body.message ? `Quark: ${body.message}` : `Quark refused (${body.code}).`,
    );
  }
  return (body.data ?? {}) as Record<string, unknown>;
}

function entriesFrom(list: unknown): QuarkEntry[] {
  if (!Array.isArray(list)) return [];
  return list.map((raw) => {
    const f = raw as {
      fid?: string;
      file_name?: string;
      size?: number;
      dir?: boolean;
    };
    return {
      fid: f.fid ?? "",
      name: f.file_name ?? "(unnamed)",
      size: typeof f.size === "number" ? f.size : 0,
      isDir: f.dir === true,
    };
  });
}

/** Open a share: exchange the link for a token, then list its top directory. */
export async function readQuarkShare(url: string): Promise<QuarkShare> {
  const core = await loadCore();
  const parsed = core.parse_quark_share(url);
  if (!parsed) throw new Error("That does not look like a Quark share link.");
  const { pwdId, passcode } = JSON.parse(parsed) as {
    pwdId: string;
    passcode: string;
  };

  const token = await quarkCall(
    `${QUARK_API}/share/sharepage/token?${QUARK_PARAMS}`,
    {
      method: "POST",
      body: JSON.stringify({ pwd_id: pwdId, passcode }),
    },
  );
  const stoken = typeof token.stoken === "string" ? token.stoken : "";
  if (!stoken) {
    throw new Error(
      "Quark did not open that share. It may have expired, or it may need a passcode — " +
        "add it to the link as `?pwd=…`.",
    );
  }

  const detail = await listing(pwdId, stoken, "0");
  const share = detail.share as { title?: string } | undefined;
  return {
    pwdId,
    stoken,
    title: share?.title ?? "Quark share",
    entries: entriesFrom(detail.list),
  };
}

/** List one directory inside an already-opened share. */
export async function listQuarkFolder(
  share: QuarkShare,
  fid: string,
): Promise<QuarkEntry[]> {
  const detail = await listing(share.pwdId, share.stoken, fid);
  return entriesFrom(detail.list);
}

async function listing(
  pwdId: string,
  stoken: string,
  fid: string,
): Promise<Record<string, unknown>> {
  const query = new URLSearchParams({
    pr: "ucpro",
    fr: "pc",
    pwd_id: pwdId,
    stoken,
    pdir_fid: fid,
    _page: "1",
    // One request per directory. Shares deeper than this page back through folders
    // rather than listing thousands of entries at once.
    _size: "200",
    _fetch_total: "1",
    // Without this Quark omits the `share` object, and with it the share's title.
    _fetch_share: "1",
    _sort: "file_type:asc,updated_at:desc",
  });
  return quarkCall(`${QUARK_API}/share/sharepage/detail?${query}`);
}

/**
 * Ask Quark for a download URL.
 *
 * Kept because the call is the honest way to find out — Quark decides, not this code, and
 * an account that is entitled to the file gets a URL here. Without one it answers 23018
 * and that becomes {@link QuarkNeedsAccount} rather than a bare error string.
 */
export async function resolveQuarkDownload(
  share: QuarkShare,
  entry: QuarkEntry,
): Promise<string> {
  let data: Record<string, unknown>;
  try {
    data = await quarkCall(`${QUARK_API}/file/download?${QUARK_PARAMS}`, {
      method: "POST",
      body: JSON.stringify({
        pwd_id: share.pwdId,
        stoken: share.stoken,
        fids: [entry.fid],
      }),
    });
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    if (/size limit/i.test(message) || /\b23018\b/.test(message)) {
      throw new QuarkNeedsAccount(entry.name);
    }
    throw e;
  }
  const first = Array.isArray(data)
    ? data[0]
    : (data as { download_url?: string });
  const url = (first as { download_url?: string })?.download_url;
  if (!url) throw new QuarkNeedsAccount(entry.name);
  return url;
}
