// The standalone web app.
//
// Same engine, same manager, same local tools as the extension — the difference
// is what a web page is allowed to do. It cannot watch what another site loads,
// so there is no detection here and the input is a link you paste. And its
// `fetch` is subject to CORS, which the extension's host permissions exempt it
// from; when a server refuses, that is explained and the relay is offered rather
// than the failure being reported as a generic network error.

import {
  audioOnly,
  configureEngine,
  extract,
  extractMp4Audio,
  fetchWithRetry,
  formatSize,
  getSettings,
  hasAudioChoice,
  isSupportedSite,
  jobIdFor,
  looksLikeCorsFailure,
  pair,
  pairingProblem,
  parseEd2kLink,
  putJob,
  remuxLocalSegments,
  resolveDownloadLink,
  siteAcceptsFetchedPage,
  siteFor,
  updateSettings,
  webPlatform,
  type AudioChoice,
  type Extraction,
  type MediaOption,
  type VideoChoice,
} from "@opendownloader/engine";
import { Manager, candidateForUrl, mountTools } from "@opendownloader/ui";

import { reflectAccountState } from "./account-badge";
import { mountTranscribePanel } from "./transcribe-panel";

// Test-only engine configuration, applied before anything can start a download.
// Automation can click a button but cannot answer a native save dialog, so the
// blob sink — which needs no dialog — is forced for the smoke test.
if (__OPENDOWNLOADER_E2E__) {
  configureEngine({ forceBlobSink: true });
}

const urlInput = document.getElementById("url") as HTMLInputElement;
const goButton = document.getElementById("go") as HTMLButtonElement;
const statusEl = document.getElementById("url-status") as HTMLParagraphElement;
const siteOptionsEl = document.getElementById("site-options") as HTMLDivElement;

/**
 * Route fetches through the user's relay when they have configured one.
 *
 * Installed as a URL rewrite rather than a `fetch` wrapper so range requests,
 * `If-Range`, retries and aborts all behave identically whether a relay is in
 * play or not — the relay forwards those headers verbatim.
 */
let relay = { url: "", enabled: false };

configureEngine({
  rewriteUrl: (url) => {
    if (!relay.enabled || !relay.url) return url;
    // Already relayed: rewriting twice would nest the query parameter.
    if (url.startsWith(relay.url)) return url;
    return `${relay.url.replace(/\/+$/, "")}/fetch?url=${encodeURIComponent(url)}`;
  },
});

/**
 * What a desktop browser calls itself.
 *
 * Sent, through the relay, on the page fetch below. Several of these sites answer a
 * request with no recognisable `User-Agent` with a stub page, and a stub page is
 * indistinguishable from a site that has changed shape.
 */
const DESKTOP_UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 " +
  "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/**
 * Fetch a watch page for an extractor that asked to read one.
 *
 * The extension answers `PageState` by reading the tab the user is looking at. This page
 * has no such tab, so it fetches the same URL instead — a thinner document, since none of
 * the page's own scripts have run against it. Only extractors that have said they can
 * continue from that copy are ever given this.
 *
 * `Referer` and `User-Agent` are forbidden to a page, so they travel under the relay's
 * `x-relay-*` names and are put back on the wire by the relay. With no relay configured
 * they simply do not arrive, and the site answers whatever it answers strangers — which
 * is the case the extractor has already promised to cope with.
 */
async function fetchPageState(url: string): Promise<string> {
  const response = await fetchWithRetry(url, {
    headers: {
      "x-relay-referer": `${new URL(url).origin}/`,
      "x-relay-user-agent": DESKTOP_UA,
      // Both are ordinary headers a page may set, and both matter: a watch page asked
      // for with `Accept: */*` and no language is not a shape any browser produces, and
      // the sites that screen for automation answer it with a refusal rather than a page.
      Accept:
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
      "Accept-Language": "zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7",
    },
  });
  if (!response.ok) {
    throw new Error(
      `${new URL(url).hostname} answered ${response.status} for that page.`,
    );
  }
  return response.text();
}

/**
 * The BitTorrent bridge, if one is running on this machine.
 *
 * A `magnet:` link cannot be downloaded by a tab: joining a swarm means opening TCP and
 * uTP connections to peers, which a page has no way to do. The bridge is a local process
 * that does have those sockets, and it offers each file in a torrent over HTTP with byte
 * ranges — at which point it is an ordinary download, and everything below this line
 * (chunks, resume, the SHA-256 read back off disk) works against it unchanged.
 *
 * Discovered the same way as the relay: loopback only, only when it answers its own
 * health check, never guessed at anywhere else.
 */
const LOCAL_TORRENT_BRIDGE = "http://127.0.0.1:8089";
let torrentBridge: string | null = null;

async function adoptTorrentBridge(): Promise<void> {
  try {
    const probe = await fetch(`${LOCAL_TORRENT_BRIDGE}/healthz`, {
      signal: AbortSignal.timeout(1200),
    });
    if (!probe.ok) return;
    const health = (await probe.json()) as { service?: string };
    if (health.service !== "dl-torrent") return;
  } catch {
    // Nothing there. The common case, and the refusal text below covers it.
    return;
  }
  torrentBridge = LOCAL_TORRENT_BRIDGE;
}

/**
 * Hand a magnet link or `.torrent` to the bridge and offer its files.
 *
 * Returns false when this is not a peer-to-peer link at all, so the caller carries on
 * with the ordinary path.
 */
async function addFromTorrent(link: string): Promise<boolean> {
  const isTorrentFile = /\.torrent(\?|$)/i.test(link);
  if (!/^magnet:/i.test(link) && !/^ed2k:/i.test(link) && !isTorrentFile)
    return false;

  if (/^ed2k:/i.test(link)) return addFromEd2k(link);
  if (!torrentBridge) {
    throw new Error(
      "This is a BitTorrent link, and a browser tab cannot join a swarm — that needs " +
        "TCP connections to other people's machines, which no page, permission or relay " +
        "can open. OpenDownloader ships a local bridge that does it for you: run " +
        "`npm start` (or `cargo run -p dl-torrent`) and reload this page, and the files " +
        "inside the torrent will be listed here like any other download.",
    );
  }

  statusEl.textContent = "Asking the swarm what is in this torrent…";
  const response = await fetch(`${torrentBridge}/torrent`, {
    method: "POST",
    // A magnet goes as text; a `.torrent` URL is fetched by the bridge itself, which can
    // reach hosts this page cannot.
    body: link,
  });
  if (!response.ok) {
    const detail = (await response.json().catch(() => null)) as {
      error?: string;
    } | null;
    throw new Error(detail?.error ?? `the bridge answered ${response.status}`);
  }
  const torrent = (await response.json()) as {
    id: number;
    name: string;
    files: { index: number; name: string; length: number; url: string }[];
  };
  renderTorrentFiles(torrent);
  statusEl.textContent = "";
  return true;
}

/**
 * Act on an `ed2k://` link.
 *
 * The hash in the link is checkable — the engine computes it while reading the finished
 * file back off disk — but the eDonkey network itself is out of reach: joining it needs a
 * client this does not ship, and unlike BitTorrent there is no bridge for it. So a link
 * that carries an `|s=http://…|` source downloads and verifies like anything else, and a
 * link that does not says exactly what it is missing rather than failing vaguely.
 */
async function addFromEd2k(link: string): Promise<boolean> {
  const parsed = await parseEd2kLink(link);
  if (!parsed) throw new Error("that ed2k link could not be read.");

  if (parsed.kind === "server" || parsed.kind === "serverlist") {
    throw new Error(
      "That is an eDonkey server address, not a file — it tells a client where to " +
        "connect, and there is nothing at it to download.",
    );
  }

  if (!parsed.httpSource) {
    throw new Error(
      `This link names a file (${parsed.filename}, ${formatSize(parsed.size)}) by its ` +
        "eD2k hash, and nothing else — the bytes exist only on the eDonkey network, " +
        "which needs a client such as eMule or aMule. Some ed2k links also carry an " +
        "ordinary web address, and those download here. If you fetch this file " +
        "elsewhere, paste its web address and it will be checked against the hash in " +
        "this link.",
    );
  }

  statusEl.textContent = `Found a web source for ${parsed.filename} — checking it…`;
  await manager.enqueue(
    {
      url: parsed.httpSource,
      kind: "progressive",
      // The link's own name, not one guessed from the URL: an ed2k source URL is
      // routinely a numeric id on a mirror.
      filename: parsed.filename,
      mime: null,
      size: parsed.size,
    },
    { start: true, expectedEd2k: parsed.hash },
  );
  statusEl.textContent = `Queued ${parsed.filename}. It will be checked against the eD2k hash in the link.`;
  return true;
}

/**
 * List a torrent's files, each one a download.
 *
 * Deliberately the same shape as a supported site's option list: by this point a torrent
 * is not a special kind of thing, it is a set of URLs that answer range requests.
 */
function renderTorrentFiles(torrent: {
  id: number;
  name: string;
  files: { index: number; name: string; length: number; url: string }[];
}): void {
  siteOptionsEl.replaceChildren();
  siteOptionsEl.hidden = false;

  const heading = document.createElement("p");
  heading.className = "muted";
  heading.textContent = `${torrent.name} — ${torrent.files.length} file${
    torrent.files.length === 1 ? "" : "s"
  }`;
  siteOptionsEl.append(heading);

  // Biggest first: in a season pack or a release folder, the thing someone came for is
  // almost always the largest file, and the rest are samples, artwork and notes.
  const files = [...torrent.files].sort((a, b) => b.length - a.length);
  for (const file of files) {
    siteOptionsEl.append(
      choiceRow(file.name, formatSize(file.length), () => {
        void queueTorrentFile(torrent, file);
      }),
    );
  }

  const note = document.createElement("p");
  note.className = "muted hint";
  note.textContent =
    "Pieces are fetched as they are needed, so downloading one file does not fetch " +
    "the rest of the torrent. Nothing is uploaded back to the swarm.";
  siteOptionsEl.append(note);
}

async function queueTorrentFile(
  torrent: { id: number; name: string },
  file: { index: number; name: string; length: number; url: string },
): Promise<void> {
  // Built directly rather than through `candidateForUrl`, which exists to work out what
  // an unknown URL is by probing it. Nothing here is unknown: the bridge has already
  // said the name, the exact length and the file's place in the torrent, and the URL it
  // serves from ends in `/file/0` — a path with no extension, which is precisely the
  // shape that probing gives up on.
  await manager.enqueue(
    {
      url: `${torrentBridge}${file.url}`,
      kind: "progressive",
      // A torrent path can be `Season 1/ep01.mkv`; only the last segment is a filename.
      filename: file.name.split("/").pop() ?? file.name,
      mime: null,
      size: file.length,
    },
    { start: true },
  );
  siteOptionsEl.hidden = true;
  statusEl.textContent = `Queued ${file.name} from ${torrent.name}.`;
}

/** Where `npm start` puts the relay, and the only address worth guessing. */
const LOCAL_RELAY = "http://127.0.0.1:8088";

/**
 * Adopt a relay running on this machine, if one is.
 *
 * Several sites refuse a web page outright — YouTube's player endpoint answers 403 to
 * every origin but its own — and the relay is the answer, but only if it is switched on.
 * Making someone read a paragraph, find Settings and type an address before anything
 * works is a bad first five minutes, and the relay they would type is one they started
 * themselves on the next port over.
 *
 * So: only ever a loopback address, only when it answers its health check, and Settings
 * says plainly that it was found and how to turn it off. A relay somewhere else is
 * something you configure deliberately, and this never guesses at one.
 */
async function adoptLocalRelay(): Promise<void> {
  const settings = await getSettings();
  relay = { url: settings.relayUrl, enabled: settings.useRelay };
  if (settings.relayUrl) return;

  try {
    const probe = await fetch(`${LOCAL_RELAY}/healthz`, {
      signal: AbortSignal.timeout(1200),
    });
    if (!probe.ok || (await probe.text()).trim() !== "ok") return;
  } catch {
    // Nothing there. The common case, and not worth a word to the user.
    return;
  }

  relay = { url: LOCAL_RELAY, enabled: true };
  await updateSettings({ relayUrl: LOCAL_RELAY, useRelay: true });
  relayAdopted = true;
  mountRelaySettings();
}

/** Set when the relay was found rather than typed, so Settings can say so. */
let relayAdopted = false;

void adoptLocalRelay();
void adoptTorrentBridge();
// Only a hint on the header control. Lazy, failure-tolerant, and never on the path
// of anything the page actually does.
void reflectAccountState(document.getElementById("account-link"));

const manager = new Manager({
  root: document.getElementById("manager") as HTMLElement,
  platform: webPlatform,
  notice:
    "Downloads run in this tab. Closing it pauses them — progress is saved, and " +
    "coming back to this page resumes from where it stopped.",
});

const toolsRoot = document.getElementById("tools") as HTMLElement;
mountTools({ root: toolsRoot, platform: webPlatform });
mountTranscribePanel(toolsRoot);

void manager.start().then(() => mountRelaySettings());

/**
 * Offer what a supported platform's own page says it has.
 *
 * The extension reads this out of the tab the user is on; a web page has no such tab, so
 * here it can only work where the site answers an ordinary request — which today means
 * the API-driven extractors, and for YouTube only through a relay, because its API
 * refuses every `Origin` but its own and its media URLs do the same. The message says so
 * rather than failing vaguely.
 */
async function addFromSite(url: string): Promise<boolean> {
  if (!(await isSupportedSite(url))) return false;

  const site = (await siteFor(url)) ?? "that site";
  statusEl.textContent = `Asking ${site} what it has…`;
  // Only where the extractor has said it can work from a fetched page. Offering it
  // everywhere would turn four sites' clear "use the extension there" into a parse
  // failure that reads as though the site had broken.
  const extraction = await extract(url, {
    readPageState: (await siteAcceptsFetchedPage(url))
      ? fetchPageState
      : undefined,
  });

  if (extraction.videos.length === 0 && extraction.audios.length === 0) {
    throw new Error(
      `${site} did not offer anything downloadable for that link.`,
    );
  }

  renderChoices(extraction, url);
  statusEl.textContent = "";
  return true;
}

function stem(title: string): string {
  const cleaned = title
    .replace(/[/\\:*?"<>|]/g, "_")
    .trim()
    .slice(0, 120);
  return cleaned || "video";
}

function optionSize(option: MediaOption): number | null {
  return option.streams.reduce<number | null>(
    (sum, s) => (sum === null || s.size === null ? null : sum + s.size),
    0,
  );
}

function metaFor(option: MediaOption): string {
  const size = optionSize(option);
  return (
    [
      size ? formatSize(size) : null,
      option.streams.length > 1 ? "video + audio, joined here" : null,
    ]
      .filter(Boolean)
      .join(" · ") || "size unknown"
  );
}

function choiceRow(
  label: string,
  meta: string,
  onClick: () => void,
): HTMLElement {
  const el = document.createElement("div");
  el.className = "card row";
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
  button.textContent = "Download";
  button.addEventListener("click", () => {
    button.disabled = true;
    onClick();
  });
  el.append(text, button);
  return el;
}

function picker<T extends { id: string; best: boolean }>(
  caption: string,
  choices: T[],
  describe: (c: T) => string,
): { field: HTMLElement; select: HTMLSelectElement; get(): T } {
  const select = document.createElement("select");
  for (const choice of choices) {
    const option = document.createElement("option");
    option.value = choice.id;
    // Named rather than implied by position: a scrolled dropdown shows no position.
    option.textContent = choice.best
      ? `${describe(choice)} — best`
      : describe(choice);
    select.append(option);
  }
  // Opens on the recommendation, not on the largest. Defaulting to `[0]` puts the
  // picker straight into the one state that shows a warning and a disabled button,
  // which reads as broken rather than as a choice.
  select.value = (choices.find((c) => c.best) ?? choices[0])?.id ?? "";
  const field = document.createElement("label");
  field.className = "field";
  const span = document.createElement("span");
  span.textContent = caption;
  field.append(span, select);
  return {
    field,
    select,
    get: () => choices.find((c) => c.id === select.value) ?? choices[0]!,
  };
}

const describeVideo = (v: VideoChoice): string =>
  v.size ? `${v.label} · ${formatSize(v.size)}` : v.label;
const describeAudio = (a: AudioChoice): string =>
  a.size ? `${a.label} · ${formatSize(a.size)}` : a.label;

/** The best-first list, plus two pickers for anyone who wants to choose. */
function renderChoices(extraction: Extraction, pageUrl: string): void {
  siteOptionsEl.replaceChildren();
  siteOptionsEl.hidden = false;

  const name = stem(extraction.title);
  const heading = document.createElement("p");
  heading.className = "muted";
  heading.textContent = extraction.title;
  siteOptionsEl.append(heading);

  const queue = (option: MediaOption) =>
    void queueOption(extraction.site, option, pageUrl).then(() => {
      siteOptionsEl.hidden = true;
      statusEl.textContent = `Queued ${option.filename}.`;
    });

  // The flagged one, not the first: "best" means best *deliverable*, and the largest
  // rendition is routinely one that cannot be given sound. Taking `[0]` here is how the
  // one-click button ends up offering a combination the merger will refuse.
  const bestVideo =
    extraction.videos.find((v) => v.best) ?? extraction.videos[0];
  const bestAudio =
    extraction.audios.find((a) => a.best) ?? extraction.audios[0];

  if (bestVideo) {
    const best = pair(bestVideo, bestAudio, name);
    siteOptionsEl.append(
      choiceRow(`Best available — ${bestVideo.label}`, metaFor(best), () =>
        queue(best),
      ),
    );
  }
  if (bestAudio) {
    const only = audioOnly(bestAudio, name);
    siteOptionsEl.append(
      choiceRow(
        `Best audio only — ${bestAudio.label}`,
        bestAudio.size ? formatSize(bestAudio.size) : "sound without picture",
        () => queue(only),
      ),
    );
  }

  if (extraction.videos.length > 1 || hasAudioChoice(extraction)) {
    const details = document.createElement("details");
    details.className = "panel card";
    const summary = document.createElement("summary");
    summary.textContent = "Choose quality yourself";
    const body = document.createElement("div");
    body.className = "stack";

    const video = picker("Video", extraction.videos, describeVideo);
    body.append(video.field);
    const audio = hasAudioChoice(extraction)
      ? picker("Audio", extraction.audios, describeAudio)
      : undefined;
    if (audio) body.append(audio.field);

    // Declared before `refresh`, which disables it: a `const` referenced by a closure
    // that runs before the declaration is a temporal dead zone error, and it takes the
    // whole panel down rather than just the button.
    const go = document.createElement("button");
    go.className = "primary";
    go.textContent = "Download this combination";
    go.addEventListener("click", () => {
      go.disabled = true;
      queue(pair(video.get(), audio?.get(), name));
    });

    const line = document.createElement("div");
    line.className = "muted";
    const refresh = () => {
      const problem = pairingProblem(video.get(), audio?.get());
      line.className = problem ? "muted hint warn" : "muted";
      line.textContent =
        problem ?? metaFor(pair(video.get(), audio?.get(), name));
      go.disabled = problem !== null;
    };
    video.select.addEventListener("change", refresh);
    audio?.select.addEventListener("change", refresh);
    refresh();

    body.append(line, go);
    details.append(summary, body);
    siteOptionsEl.append(details);
  }
}

async function queueOption(
  site: string,
  option: MediaOption,
  pageUrl: string,
): Promise<void> {
  const id = jobIdFor(option.streams.map((s) => s.url).join("|"));
  const merged = option.streams.length > 1;
  const first = option.streams[0]!;
  const now = Date.now();
  await putJob({
    id,
    url: merged ? pageUrl : first.url,
    filename: option.filename,
    kind: merged ? "merge" : "progressive",
    status: "queued",
    stateJson: "",
    totalBytes: option.streams.reduce<number | null>(
      (sum, s) => (sum === null || s.size === null ? null : sum + s.size),
      0,
    ),
    receivedBytes: 0,
    outputBytes: 0,
    sha256: null,
    error: null,
    createdAt: now,
    order: now,
    pageUrl,
    site,
    quality: option.label,
    expectedSha256: null,
    verification: "unverified",
    maxChunkBytes: first.max_chunk ?? undefined,
    ...(merged
      ? { mergeStreams: [option.streams[0]!, option.streams[1]!] }
      : {}),
  });
  await manager.start();
}

async function add(): Promise<void> {
  const typed = urlInput.value.trim();
  if (!typed) return;
  goButton.disabled = true;
  siteOptionsEl.hidden = true;
  statusEl.className = "muted";
  statusEl.textContent = "Checking that link…";
  try {
    // A BitTorrent link goes to the local bridge, which has the sockets a tab does not.
    // When no bridge is running this throws with how to start one, rather than with the
    // flat "not possible" it used to.
    if (await addFromTorrent(typed)) {
      urlInput.value = "";
      return;
    }

    // `thunder://` and its cousins are an ordinary URL in a base64 wrapper. Unwrap it
    // and carry on as though that URL had been pasted, which is what it is.
    const unwrapped = await resolveDownloadLink(typed);
    const url = unwrapped ?? typed;
    if (unwrapped) statusEl.textContent = "Decoded that link — checking it…";

    // A supported platform is asked what it has; anything else is treated as a direct
    // link to a file.
    if (await addFromSite(url)) {
      urlInput.value = "";
      return;
    }
    const candidate = await candidateForUrl(url);
    // Started here rather than merely queued: the click is the user gesture a
    // save dialog needs, and on a browser with no download folder chosen the
    // queue deliberately will not start anything without one.
    await manager.enqueue(candidate, { start: true });
    urlInput.value = "";
    statusEl.textContent = `Queued ${candidate.filename}.`;
  } catch (e) {
    statusEl.className = "status-error";
    const message = e instanceof Error ? e.message : String(e);
    // Two shapes of the same problem: the site answered 403, or the browser refused to
    // let this page read the answer at all. Both mean "a web page cannot ask this site
    // directly", and both have the same two answers — so they get the same sentence
    // rather than one useful message and one shrug.
    if (
      !relay.enabled &&
      (/\b403\b|refused this request/.test(message) || looksLikeCorsFailure(e))
    ) {
      statusEl.textContent =
        "That site will not answer a web page directly — it refuses every origin but its " +
        "own. Run the relay (npm start does it, on port 8088) and this page will find it " +
        "on reload, or use the browser extension, which is never subject to this.";
      goButton.disabled = false;
      return;
    }
    statusEl.textContent = looksLikeCorsFailure(e)
      ? "That server would not let this page read the file. This is a browser restriction, " +
        "not a broken link — the extension is not subject to it, and a relay you run yourself " +
        "gets around it. Both are free."
      : e instanceof Error
        ? e.message
        : String(e);
  } finally {
    goButton.disabled = false;
  }
}

goButton.addEventListener("click", () => void add());
urlInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") void add();
});

// A download in progress must survive an accidental tab close no worse than a
// pause: the state is already persisted, so warn and let the user decide.
window.addEventListener("beforeunload", (e) => {
  if (manager.hasRunningJobs()) {
    e.preventDefault();
    e.returnValue = "";
  }
});

/**
 * The relay settings, appended to the manager's own settings panel.
 *
 * They live here rather than in the shared manager because the extension has no
 * use for them: its host permissions mean it is never blocked by CORS, and
 * offering it a relay setting would imply a problem it does not have.
 */
function mountRelaySettings(): void {
  const panel = document.querySelector("#manager details.panel .stack");
  if (!panel) return;
  // Mounted both after the manager renders and again if a relay is adopted a moment
  // later, so the previous row is replaced rather than stacked on top of itself.
  panel.querySelectorAll("[data-relay-row]").forEach((el) => el.remove());

  const url = document.createElement("input");
  url.type = "url";
  url.className = "grow";
  url.placeholder = "http://localhost:8088";
  url.spellcheck = false;
  url.value = relay.url;

  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = relay.enabled;

  const save = async (): Promise<void> => {
    relay = { url: url.value.trim(), enabled: enabled.checked };
    await updateSettings({ relayUrl: relay.url, useRelay: relay.enabled });
  };
  url.addEventListener("change", () => void save());
  enabled.addEventListener("change", () => void save());

  const label = document.createElement("label");
  label.append(enabled, document.createTextNode("Fetch through a relay"));

  const row = document.createElement("div");
  row.className = "row wrap";
  row.setAttribute("data-relay-row", "");
  row.append(label, url);

  const help = document.createElement("p");
  help.className = "muted hint";
  help.textContent = relayAdopted
    ? `A relay is running at ${LOCAL_RELAY} on this machine, so this page is using it. ` +
      "It is what lets sites that refuse a web page — YouTube's player endpoint answers " +
      "403 to every origin but its own — work here. Untick to stop using it."
    : "Optional, and only useful if a server refuses to be read by this page. The relay is " +
      "in the repository (crates/dl-relay) and is meant to be run by you, on your own " +
      "machine or server. It enforces the same refusals this page does, and there is no " +
      "hosted one to buy.";

  help.setAttribute("data-relay-row", "");
  panel.append(row, help);
}

// Test-only hook, gated by the build flag so it is absent from a shipped bundle.
// It exposes the local tools the smoke test cannot otherwise reach: each of them
// starts at a file picker, which is browser UI and cannot be driven by
// automation, while everything after the picker is the code under test.
if (__OPENDOWNLOADER_E2E__) {
  (globalThis as unknown as { __test: unknown }).__test = {
    remuxLocalSegments,
    extractMp4Audio,
  };
}
