// The supported-site panel in the popup.
//
// Detection by sniffing tells you a page fetched something; it cannot tell you that the
// something is "1080p of the video you are watching" rather than "segment 43". On the
// large platforms that difference is the whole product, so when the current tab is one
// the extractors cover, the popup offers the site's own list of renditions instead of
// whatever the network happened to reveal.
//
// The reading happens in the tab the user is already on, using the per-site permission
// they have already granted. Nothing here fetches the page again, because on most of
// these sites a fetch from outside the browser is answered with a bot wall.
//
// # Two ways to choose
//
// Most people want the best and want it now, so the first thing offered is one button
// that takes the best picture with the best sound. Underneath, for anyone who cares — a
// slow connection, a phone with no space, a second language track — the same renditions
// appear as two pickers, because on YouTube and Bilibili the picture and the sound are
// genuinely separate files and collapsing them into one list takes away a real choice.

import {
  jobKindForStream,
  audioOnly,
  extract,
  formatSize,
  hasAudioChoice,
  isSupportedSite,
  mediaHostPatterns,
  jobIdFor,
  pair,
  pairingProblem,
  putJob,
  siteFor,
  type AudioChoice,
  type Extraction,
  type MediaOption,
  type VideoChoice,
} from "@opendownloader/engine";
import { t } from "@opendownloader/ui";

import { ext, extensionPlatform, openManagerTab } from "../platform/webext";

const siteEl = document.getElementById("site-panel") as HTMLDivElement;
const siteSupportedEl = document.getElementById("site-supported") as HTMLDivElement;
const siteButton = document.getElementById("site-extract") as HTMLButtonElement;
const siteStatus = document.getElementById("site-status") as HTMLDivElement;
const siteOptions = document.getElementById("site-options") as HTMLDivElement;

/**
 * Ensure the extension may read this tab, asking if it may not.
 *
 * `scripting.executeScript` needs host permission for the tab's origin — the same
 * per-site grant detection uses, so enabling a site for one enables it for both and the
 * user is never asked twice. The request must come from a click, which is why this runs
 * from the button handler rather than when the panel is rendered.
 */
async function ensurePermission(url: string): Promise<boolean> {
  const origins = [
    `${new URL(url).protocol}//${new URL(url).hostname}/*`,
    // The CDNs this site streams from. Reading the page is only half of it: on a site
    // that plays through MSE the page holds a `blob:` URL and the real addresses are
    // only ever seen by the network listener, which needs permission for those hosts.
    ...(await mediaHostPatterns(url)),
  ];
  if (await ext.permissions.contains({ origins })) return true;
  return ext.permissions.request({ origins });
}

/** Ask the tab for its own HTML. Requires the host permission detection already uses. */
async function readPageState(tabId: number): Promise<string> {
  const results = await ext.scripting.executeScript({
    target: { tabId },
    // Only the markup is read, so the isolated world is enough — and it is the safer of
    // the two, since nothing it returns can be redefined by page script.
    world: "ISOLATED",
    func: () => document.documentElement.outerHTML,
  });
  const html = results[0]?.result;
  if (typeof html !== "string" || html.length === 0) {
    throw new Error("could not read this page — reload it and try again");
  }
  return html;
}

/** Turn a chosen option into a job. */
async function queueOption(
  site: string,
  option: MediaOption,
  pageUrl: string,
): Promise<void> {
  // The id is derived from the streams rather than the page URL, so picking 1080p and
  // then also picking the audio-only track produces two jobs rather than one silently
  // replacing the other.
  const id = jobIdFor(option.streams.map((s) => s.url).join("|"));
  const merged = option.streams.length > 1;
  const first = option.streams[0]!;
  const now = Date.now();

  await putJob({
    id,
    // A single-stream option downloads through the ordinary progressive path, which
    // fetches `url`; a merge reads its two streams from `mergeStreams` and uses `url`
    // only for display. So `url` is the stream in the first case and the page in the
    // second, and `pageUrl` always holds where it came from.
    url: merged ? pageUrl : first.url,
    filename: option.filename,
    kind: merged ? "merge" : await jobKindForStream(first),
    status: "queued",
    stateJson: "",
    totalBytes: optionSize(option),
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
    // Several of these CDNs answer 403 without a Referer naming their own site. `fetch`
    // is forbidden from setting that header, so the extension installs it as a
    // declarativeNetRequest rule for the duration of the download instead.
    requestHeaders: first.headers.length > 0 ? first.headers : undefined,
    // A hard limit from the host, not a preference: above it the request is refused.
    maxChunkBytes: first.max_chunk ?? undefined,
    ...(merged
      ? { mergeStreams: [option.streams[0]!, option.streams[1]!] }
      : {}),
  });
}

/** Total bytes an option will transfer, when every stream states a size. */
function optionSize(option: MediaOption): number | null {
  return option.streams.reduce<number | null>(
    (sum, s) => (sum === null || s.size === null ? null : sum + s.size),
    0,
  );
}

/** A filename stem from the media's own title. */
function stem(title: string): string {
  const cleaned = title
    .replace(/[/\\:*?"<>|]/g, "_")
    .trim()
    .slice(0, 120);
  return cleaned || "video";
}

/** One row: a two-line description on the left, a button on the right. */
function row(label: string, meta: string, onClick: () => void): HTMLElement {
  const el = document.createElement("div");
  el.className = "card item row";

  const text = document.createElement("div");
  text.className = "grow";
  const top = document.createElement("div");
  // Deliberately not truncated: the rendition is the thing being chosen, and "2160p60 ·
  // AV1 · …" hides exactly the part that distinguishes one row from the next. The size
  // on the second line is what can afford to be clipped.
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
    button.textContent = "Queued";
    onClick();
  });

  el.append(text, button);
  return el;
}

interface Picker<T> {
  field: HTMLElement;
  select: HTMLSelectElement;
  get(): T;
}

/** A labelled `<select>` over choices, with the best one named as such and preselected. */
function picker<T extends { id: string; best: boolean }>(
  caption: string,
  choices: T[],
  describe: (c: T) => string,
): Picker<T> {
  const select = document.createElement("select");
  for (const choice of choices) {
    const option = document.createElement("option");
    option.value = choice.id;
    // "Best" is said rather than implied by position: a dropdown that has been scrolled
    // shows no position at all, and the whole point is that it is easy to find.
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

function describeVideo(v: VideoChoice): string {
  return v.size ? `${v.label} · ${formatSize(v.size)}` : v.label;
}

function describeAudio(a: AudioChoice): string {
  return a.size ? `${a.label} · ${formatSize(a.size)}` : a.label;
}

/** What a chosen combination will cost and whether it needs joining. */
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

function renderOptions(extraction: Extraction, pageUrl: string): void {
  siteOptions.replaceChildren();
  siteStatus.className = "muted";
  siteStatus.textContent = extraction.title;

  // Why the best entry below may be lower than the site's own player offers. The web
  // app has shown this since it was added; the popup did not, so the one surface where
  // a signed-in viewer would expect their own renditions explained nothing at all.
  if (extraction.note) {
    const note = document.createElement("p");
    note.className = "muted hint warn";
    note.textContent = extraction.note;
    siteOptions.append(note);
  }

  const name = stem(extraction.title);
  // The flagged one, not the first: "best" means best *deliverable*, and the largest
  // rendition is routinely one that cannot be given sound. Taking `[0]` here is how the
  // one-click button ends up offering a combination the merger will refuse.
  const bestVideo =
    extraction.videos.find((v) => v.best) ?? extraction.videos[0];
  const bestAudio =
    extraction.audios.find((a) => a.best) ?? extraction.audios[0];
  const audioIsChoosable = hasAudioChoice(extraction);

  const queue = (option: MediaOption) =>
    void queueOption(extraction.site, option, pageUrl).then(handOff);

  // ---- the one-click path, which is what most people want -----------------
  if (bestVideo) {
    const best = pair(bestVideo, bestAudio, name);
    siteOptions.append(
      row(`Best available — ${bestVideo.label}`, metaFor(best), () =>
        queue(best),
      ),
    );
  }

  if (bestAudio) {
    const only = audioOnly(bestAudio, name);
    siteOptions.append(
      row(
        `Best audio only — ${bestAudio.label}`,
        bestAudio.size ? formatSize(bestAudio.size) : "sound without picture",
        () => queue(only),
      ),
    );
  }

  // ---- choosing for yourself ---------------------------------------------
  if (extraction.videos.length > 1 || audioIsChoosable) {
    const details = document.createElement("details");
    details.className = "panel card";
    const summary = document.createElement("summary");
    summary.textContent = "Choose quality yourself";
    details.append(summary);

    const body = document.createElement("div");
    body.className = "stack";

    const video = picker("Video", extraction.videos, describeVideo);
    body.append(video.field);

    const audio = audioIsChoosable
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
      go.textContent = "Queued";
      queue(pair(video.get(), audio?.get(), name));
    });

    const line = document.createElement("div");
    line.className = "muted";
    const refresh = (): void => {
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
    details.append(body);
    siteOptions.append(details);
  }

  if (!bestVideo && !bestAudio) {
    siteStatus.textContent = "Nothing downloadable was found on this page.";
  }
}

async function handOff(): Promise<void> {
  await openManagerTab();
  window.close();
}

/** Show the panel when the tab is a site an extractor covers. */
export async function initSitePanel(
  tab: chrome.tabs.Tab | undefined,
): Promise<void> {
  const url = tab?.url;
  const tabId = tab?.id;
  if (!url || tabId === undefined || !(await isSupportedSite(url))) {
    siteEl.hidden = true;
    return;
  }

  siteEl.hidden = false;
  siteSupportedEl.textContent = t("{site} is supported directly — read this page for its own list of qualities.", {
    site: (await siteFor(url)) ?? t("this site"),
  });
  siteStatus.textContent = "";
  siteOptions.replaceChildren();

  // Say up front that a permission is coming, rather than springing a browser prompt on
  // a click labelled something else.
  const origins = [`${new URL(url).protocol}//${new URL(url).hostname}/*`];
  const granted = await ext.permissions
    .contains({ origins })
    .catch(() => false);
  siteButton.textContent = granted
    ? "See what this page offers"
    : "Allow this site, then see what it offers";

  siteButton.addEventListener("click", () => {
    void (async () => {
      siteButton.disabled = true;
      siteStatus.className = "muted";
      siteStatus.textContent = "Reading this page…";
      try {
        if (!(await ensurePermission(url))) {
          siteStatus.textContent =
            "Reading this page needs your permission for this site. Nothing is read " +
            "until you grant it, and it can be revoked at any time.";
          return;
        }
        const extraction = await extract(url, {
          readPageState: () => readPageState(tabId),
          // YouTube's InnerTube endpoint refuses any `Origin` but its own, and an
          // extension page's fetch carries `chrome-extension://…`. Rewriting it on the
          // way out is the only thing that makes the call work from here.
          applyRequestHeaders: extensionPlatform.applyRequestHeaders,
        });
        renderOptions(extraction, url);
      } catch (e) {
        siteStatus.className = "status-error";
        siteStatus.textContent = e instanceof Error ? e.message : String(e);
      } finally {
        siteButton.disabled = false;
      }
    })();
  });
}
