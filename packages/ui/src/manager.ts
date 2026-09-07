// The download manager: the queue, the jobs, and everything you can do to them.
//
// Mounted by both front ends. The extension puts it in its manager tab; the web
// app puts it under the URL box. Neither knows anything about how a job runs —
// that is `Queue` — and this file knows nothing about which host it is in,
// beyond the `Platform` it is handed.

import {
  Queue,
  audioRenditionFor,
  canOpenSinkSilently,
  deleteJob,
  enqueueCandidate,
  type EnqueueOptions,
  fetchSubtitleRendition,
  formatEta,
  formatSize,
  formatSpeed,
  fetchWithRetry,
  looksLikeCorsFailure,
  getFolderHandle,
  getSettings,
  listJobs,
  listPlaylistOptions,
  loadCore,
  moveJob,
  pickFolder,
  requestPermission,
  setFolderHandle,
  updateJob,
  updateSettings,
  webPlatform,
  MAX_CONCURRENT_JOBS,
  MAX_CONNECTIONS,
  type Job,
  type MediaCandidate,
  type Platform,
  type PlaylistOptions,
  type Progress,
  type Rendition,
  type Settings,
  hasFileSystemAccess,
} from "@opendownloader/engine";

import { Busy, TORRENT_STAGES } from "./busy";
import { checkbox, el, field } from "./dom";

export interface ManagerOptions {
  /** Where the manager renders. Cleared and owned by it. */
  root: HTMLElement;
  platform?: Platform;
  /** Shown at the top; the two hosts explain their own constraints differently. */
  notice?: string;
  /** Adds an "Add URL" box. The web app has its own, larger one. */
  showUrlInput?: boolean;
  /**
   * What the box does with a pasted link, where the host can do more than fetch a URL.
   *
   * Without it the box only knows how to treat a link as a file to download, so magnets,
   * `ed2k:`, Mega and Quark links are all refused by it — including on a page where the
   * host handles every one of them a few centimetres higher up.
   */
  addLink?: (url: string) => Promise<void>;
  /**
   * What to do with a `.torrent` chosen from disk.
   *
   * A separate hook from {@link addLink} because a file is not a URL and cannot be typed
   * into a box. Without it a torrent held as a file — which is how most of them arrive,
   * saved from a page — had no way into the product at all: the bridge accepted the
   * bytes, and nothing could give them to it.
   */
  addTorrentFile?: (file: File) => Promise<void>;
}

/** Minimum gap between re-renders, so a fast download does not rebuild the list per chunk. */
const RENDER_INTERVAL_MS = 250;

/**
 * Is this a link the local bridge has to resolve?
 *
 * Only used to decide what to say while waiting — the host's own `addLink` makes the
 * real decision. Getting it wrong here costs a slightly wrong sentence, nothing more.
 */
function isPeerLink(url: string): boolean {
  return /^magnet:/i.test(url) || /\.torrent(\?|$)/i.test(url);
}

export class Manager {
  private readonly root: HTMLElement;
  private readonly platform: Platform;
  private readonly queue: Queue;
  private readonly liveProgress = new Map<string, Progress>();
  private readonly playlistCache = new Map<string, PlaylistOptions>();
  private readonly busy = new Map<string, string>();
  /** The link box's wait indicator, once it has been rendered. See {@link say}. */
  private wait: Busy | null = null;

  private jobs: Job[] = [];
  private settings: Settings | null = null;
  private folderName: string | null = null;
  private folderNeedsPermission = false;
  /** Whether the queue can open a file without asking, which is what lets it run alone. */
  private canRunUnattended = true;
  private renderTimer: number | null = null;
  private renderPending = false;

  private readonly jobsEl = el("div", { class: "stack" });
  private readonly toolbarEl = el("div", { class: "toolbar" });
  private readonly emptyEl = el("div", { class: "card muted", hidden: true });
  /** Why the queue is not running itself, shown above the jobs it is holding up. */
  private readonly blockedEl = el("div", { class: "card stack", hidden: true });
  private readonly settingsBody = el("div", { class: "stack" });

  constructor(private readonly options: ManagerOptions) {
    this.root = options.root;
    this.platform = options.platform ?? webPlatform;
    this.queue = new Queue(this.platform, {
      onChange: () => this.scheduleRender(),
      onProgress: (id, p) => {
        this.liveProgress.set(id, p);
        this.scheduleRender();
      },
    });
    this.mount();
  }

  /** Load state and start the queue. Call once, after construction. */
  async start(): Promise<void> {
    await this.refresh();
    await this.queue.tick();
  }

  /**
   * Re-read stored state, then let the queue run.
   *
   * The same two steps as {@link start}, for a host that has been told the jobs changed
   * under it. The manager reads its queue from IndexedDB, which announces nothing — so
   * a job added by the popup was invisible in an already-open manager tab until it was
   * reloaded by hand, and because nothing ticked the queue, it also never started.
   */
  async sync(): Promise<void> {
    await this.refresh();
    await this.queue.tick();
  }

  /** Whether anything is mid-download, so a host can warn before closing. */
  hasRunningJobs(): boolean {
    return this.queue.activeCount > 0;
  }

  /** How many jobs are running right now. Read by the E2E suite. */
  activeCount(): number {
    return this.queue.activeCount;
  }

  /**
   * Add a job from outside — the popup's "download" button, or a pasted URL.
   *
   * `start` runs it immediately rather than leaving it queued, and must only be
   * passed from a real user gesture: without a download folder the sink is a
   * save dialog, and that needs transient activation which does not survive an
   * await. It is what makes pressing "Download" download something on a browser
   * where the queue cannot start jobs unattended.
   */
  async enqueue(
    candidate: MediaCandidate,
    // The engine's own option type, not a copy of it. This was a hand-written
    // duplicate, and it silently dropped every option the engine grew after it
    // was written — a caller could pass `decrypt` and watch it vanish with no
    // error anywhere.
    opts: EnqueueOptions & { start?: boolean } = {},
  ): Promise<Job> {
    const job = await enqueueCandidate(candidate, opts);
    await this.refresh();
    if (opts.start) await this.queue.startInteractive(job.id);
    else await this.queue.tick();
    return job;
  }

  private mount(): void {
    this.root.replaceChildren();
    this.emptyEl.textContent =
      "Nothing queued yet. Open the extension on a page with media, or paste a link above.";

    const header = el("div", { class: "stack" });
    if (this.options.notice) {
      header.append(
        el("p", { class: "muted hint", text: this.options.notice }),
      );
    }

    const settingsPanel = el(
      "details",
      { class: "panel card" },
      el("summary", { text: "Settings" }),
      this.settingsBody,
    );

    this.root.append(
      header,
      this.toolbarEl,
      settingsPanel,
      this.blockedEl,
      this.emptyEl,
      this.jobsEl,
    );
    if (this.options.showUrlInput) header.append(this.urlInput());
  }

  /**
   * Say what is happening during a wait the host started.
   *
   * A host's `addLink` can take half a minute — starting the local app, finding a
   * swarm — and only the host knows which part it is in. The bar is the manager's, so
   * this is how the host writes to it. Silent when there is no wait in progress.
   */
  say(message: string): void {
    this.wait?.say(message);
  }

  private urlInput(): HTMLElement {
    const input = el("input", {
      type: "url",
      class: "grow",
      placeholder: "https://example.com/video.mp4 or .m3u8",
      spellcheck: false,
    });
    const wait = new Busy();
    this.wait = wait;
    const addButton = el("button", {
      class: "primary",
      text: "Add",
      onClick: () => void add(),
    });
    // Every button that starts a wait, disabled for the duration of one. A magnet takes
    // seconds to resolve and used to look like nothing had happened, so it was pressed
    // again — and a second press queues the same torrent twice.
    const buttons: HTMLButtonElement[] = [addButton];
    let pending = false;
    const working = (on: boolean): void => {
      pending = on;
      for (const button of buttons) button.disabled = on;
    };

    const add = async (): Promise<void> => {
      const url = input.value.trim();
      if (!url || pending) return;
      working(true);
      // A peer link is the slow case and the one that needs saying out loud: the app has
      // to be found, then the swarm, then a peer that will name the files.
      wait.start(
        isPeerLink(url)
          ? "Looking for the OpenDownloader app, then for peers…"
          : "Checking that link…",
        isPeerLink(url) ? TORRENT_STAGES : [],
      );
      try {
        // The host's own handler where it has one. Without this the manager knew only
        // how to fetch a plain URL, so a magnet pasted here was refused — while the same
        // magnet pasted into the box further up the same page downloaded. Two boxes that
        // look alike and behave differently is a trap, and the one that refuses is the
        // one people reach for, because it sits next to the downloads.
        if (this.options.addLink) {
          await this.options.addLink(url);
          input.value = "";
          wait.done();
          return;
        }
        const candidate = await candidateForUrl(url);
        // Pressing the button is the gesture a save dialog needs, and this is
        // the last point at which it is still valid.
        await this.enqueue(candidate, { start: true });
        input.value = "";
        wait.done();
      } catch (e) {
        wait.fail(e instanceof Error ? e.message : String(e));
      } finally {
        working(false);
      }
    };

    input.addEventListener("keydown", (e) => {
      if ((e as KeyboardEvent).key === "Enter") void add();
    });

    const row = el("div", { class: "row" }, input, addButton);

    if (this.options.addTorrentFile) {
      // A hidden input driven by a button, because the browser's own file control cannot
      // be styled and reads as a foreign object in a row of the product's own controls.
      const picker = el("input", {
        type: "file",
        accept: ".torrent,application/x-bittorrent",
      }) as HTMLInputElement;
      picker.hidden = true;
      picker.addEventListener("change", () => {
        const file = picker.files?.[0];
        if (!file) return;
        working(true);
        wait.start(`Reading ${file.name}…`, TORRENT_STAGES);
        void this.options
          .addTorrentFile?.(file)
          .then(() => {
            wait.done();
          })
          .catch((e: unknown) => {
            wait.fail(e instanceof Error ? e.message : String(e));
          })
          .finally(() => {
            working(false);
            // Cleared so choosing the same file twice fires `change` the second time.
            picker.value = "";
          });
      });
      const openButton = el("button", {
        text: "Open .torrent",
        onClick: () => picker.click(),
      });
      buttons.push(openButton);
      row.append(openButton, picker);
    }

    return el("div", { class: "stack" }, row, wait.root);
  }

  private scheduleRender(): void {
    if (this.renderTimer !== null) {
      this.renderPending = true;
      return;
    }
    void this.refresh();
    this.renderTimer = window.setTimeout(() => {
      this.renderTimer = null;
      if (this.renderPending) {
        this.renderPending = false;
        this.scheduleRender();
      }
    }, RENDER_INTERVAL_MS);
  }

  private async refresh(): Promise<void> {
    this.jobs = await listJobs();
    this.settings = await getSettings();
    const folder = await getFolderHandle();
    this.folderName = folder?.name ?? null;
    this.canRunUnattended = await canOpenSinkSilently();
    this.folderNeedsPermission = Boolean(folder) && !this.canRunUnattended;
    this.render();
  }

  // ---- rendering -------------------------------------------------------

  private render(): void {
    this.renderToolbar();
    this.renderSettings();
    this.renderBlocked();
    this.emptyEl.hidden = this.jobs.length > 0;
    this.jobsEl.replaceChildren(...this.jobs.map((job) => this.renderJob(job)));
  }

  /**
   * Why the queue is sitting still, said where the queue is.
   *
   * There are only two reasons a queued job does not start, and both were only
   * explained inside the Settings panel — which is a collapsed `<details>`, so in
   * practice they were not explained at all: a job clicked from the popup landed as
   * "queued" beside a Start button, with nothing on screen saying what it was waiting
   * for. Shown only when something is actually being held up, so it is a live
   * explanation rather than a permanent warning.
   */
  private renderBlocked(): void {
    const queued = this.jobs.filter((j) => j.status === "queued").length;
    const autoStart = this.settings?.autoStart ?? true;
    const held = queued > 0 && (!autoStart || !this.canRunUnattended);
    this.blockedEl.hidden = !held;
    if (!held) return;

    const waiting =
      queued === 1
        ? "One download is waiting"
        : `${queued} downloads are waiting`;

    if (!autoStart) {
      this.blockedEl.replaceChildren(
        el("p", {
          class: "muted hint",
          text:
            `${waiting} because starting them automatically is switched off. ` +
            "Press Start on one, or turn it back on in Settings.",
        }),
      );
      return;
    }

    // Auto-start is on, so the sink is what is missing. A save dialog can only be
    // opened from a real click, which is why an unattended queue needs a folder.
    this.blockedEl.replaceChildren(
      el("p", {
        class: "muted hint",
        text: this.folderNeedsPermission
          ? `${waiting} because this browser has forgotten its permission for ` +
            `“${this.folderName}”. Granting it again lets them run on their own.`
          : `${waiting} because no download folder is chosen. Without one the browser ` +
            "has to ask where to save each file, and it will only ask on a click — so " +
            "each download needs its Start button. Choose a folder once and they start " +
            "by themselves.",
      }),
      el(
        "div",
        { class: "row wrap" },
        this.folderNeedsPermission
          ? el("button", {
              class: "primary",
              text: "Re-grant access",
              onClick: () => void this.regrantFolder(),
            })
          : el("button", {
              class: "primary",
              text: "Choose download folder",
              onClick: () => void this.chooseFolder(),
            }),
      ),
    );
  }

  private renderToolbar(): void {
    const active = this.queue.activeCount;
    const queued = this.jobs.filter((j) => j.status === "queued").length;
    const failed = this.jobs.filter((j) => j.status === "error").length;
    const done = this.jobs.filter((j) => j.status === "done").length;
    const paused = this.jobs.filter((j) => j.status === "paused").length;

    // With no jobs there is nothing to summarise and no action to offer, and the
    // empty-state card below already says so — two "nothing here" messages
    // stacked on top of each other is worse than one.
    this.toolbarEl.hidden = this.jobs.length === 0;
    if (this.jobs.length === 0) {
      this.toolbarEl.replaceChildren();
      return;
    }

    const summary = el("span", {
      class: "muted grow",
      text: `${active} running · ${queued} queued · ${paused} paused · ${done} done${
        failed ? ` · ${failed} failed` : ""
      }`,
    });

    const children: (HTMLElement | false)[] = [
      summary,
      (active > 0 || queued > 0) &&
        el("button", {
          text: "Pause all",
          onClick: () => this.queue.pauseAll(),
        }),
      paused > 0 &&
        el("button", {
          class: "primary",
          text: "Resume all",
          onClick: () => void this.queue.resumeAll(),
        }),
      failed > 0 &&
        el("button", {
          text: "Retry failed",
          onClick: () => void this.queue.retryFailed(),
        }),
      done > 0 &&
        el("button", {
          text: "Clear finished",
          onClick: () => void this.clearDone(),
        }),
    ];

    this.toolbarEl.replaceChildren(
      ...children.filter((c): c is HTMLElement => Boolean(c)),
    );
  }

  private renderSettings(): void {
    const s = this.settings;
    if (!s) return;

    const concurrency = el("select", {
      onChange: (e) =>
        void this.saveSettings({
          maxConcurrentJobs: Number((e.target as HTMLSelectElement).value),
        }),
    });
    for (let n = 1; n <= MAX_CONCURRENT_JOBS; n++) {
      const option = el("option", { value: String(n), text: String(n) });
      option.selected = s.maxConcurrentJobs === n;
      concurrency.append(option);
    }

    const connections = el("select", {
      onChange: (e) =>
        void this.saveSettings({
          connectionsPerJob: Number((e.target as HTMLSelectElement).value),
        }),
    });
    for (let n = 1; n <= MAX_CONNECTIONS; n++) {
      const option = el("option", { value: String(n), text: String(n) });
      option.selected = s.connectionsPerJob === n;
      connections.append(option);
    }

    const auto = checkbox(
      "Start queued downloads automatically",
      s.autoStart,
      (checked) => void this.saveSettings({ autoStart: checked }),
    );

    const rows: HTMLElement[] = [
      el(
        "div",
        { class: "row wrap" },
        field("Downloads at once", concurrency),
        field("Connections per file", connections),
        auto.row,
      ),
    ];

    if (hasFileSystemAccess()) {
      const status = this.folderName
        ? this.folderNeedsPermission
          ? `“${this.folderName}” — permission needs re-granting`
          : `Saving to “${this.folderName}”`
        : "No folder chosen: each download asks where to save, so the queue cannot run unattended.";

      rows.push(
        el(
          "div",
          { class: "row wrap" },
          el("span", { class: "muted grow", text: status }),
          el("button", {
            text: this.folderName ? "Change folder" : "Choose download folder",
            onClick: () => void this.chooseFolder(),
          }),
          this.folderNeedsPermission
            ? el("button", {
                class: "primary",
                text: "Re-grant",
                onClick: () => void this.regrantFolder(),
              })
            : undefined,
          this.folderName
            ? el("button", {
                class: "link",
                text: "Forget",
                onClick: () => void this.forgetFolder(),
              })
            : undefined,
        ),
      );
    } else {
      rows.push(
        el("p", {
          class: "muted hint",
          text:
            "This browser has no File System Access API, so finished files are handed to the " +
            "browser's own downloads instead of being written directly. Downloads run on one " +
            "connection each, and very large files are limited by memory at the final assembly step.",
        }),
      );
    }

    this.settingsBody.replaceChildren(...rows);
  }

  private renderJob(job: Job): HTMLElement {
    const progress = this.liveProgress.get(job.id);
    const running = this.queue.isRunning(job.id);
    const card = el("div", { class: "card stack" });

    const status = el("span", {
      class:
        job.verification === "mismatch"
          ? "status-error"
          : job.status === "done"
            ? "status-done"
            : job.status === "error"
              ? "status-error"
              : "muted",
      text: statusLine(job, progress),
    });

    // The name owns a line of its own.
    //
    // It used to share one flex row with the badges, and lost: `.grow` carries
    // `min-width: 0` so the name shrank to nothing, while a badge is `nowrap` and gave
    // up not one pixel. A YouTube job showed as "Rust …" beside a badge spelling out
    // "1080P · AVC · 711 KBPS + ARABIC · 131 KBPS · AAC" — the codec in full, and the
    // video's name unreadable. The name is what identifies the row; the badges only
    // describe it, so they go underneath and may wrap as far as they like.
    const name = el("div", {
      class: "job-name truncate",
      // Both, because the visible text is the one that gets cut off.
      title: `${job.filename}\n${job.url}`,
      text: job.filename,
    });
    const tags = el("div", { class: "row wrap job-tags" });
    if (job.site) tags.append(el("span", { class: "badge", text: job.site }));
    if (job.kind === "hlsplaylist")
      tags.append(el("span", { class: "badge", text: "HLS" }));
    if (job.kind === "merge") {
      tags.append(
        el("span", {
          class: "badge",
          title:
            "The platform serves picture and sound as separate files, so both are " +
            "downloaded and joined here. Nothing is re-encoded.",
          text: "video + audio",
        }),
      );
    }
    // What was actually chosen, when the site offered a choice. Worth showing: two jobs
    // for one video differ only in this.
    if (job.quality)
      tags.append(el("span", { class: "badge", text: job.quality }));
    if (job.audioOnly)
      tags.append(el("span", { class: "badge", text: "audio" }));
    if (job.verification === "verified") {
      tags.append(el("span", { class: "badge ok", text: "verified" }));
    }
    if (job.verification === "mismatch") {
      tags.append(el("span", { class: "badge bad", text: "hash mismatch" }));
    }
    tags.append(status);
    card.append(name, tags);

    const total = progress?.total ?? job.totalBytes;
    const received = progress?.received ?? job.receivedBytes;
    if (job.status !== "done" && job.status !== "error") {
      const bar = el("progress");
      if (total) {
        bar.max = total;
        bar.value = received;
      }
      card.append(bar);
    }

    if (job.sha256) {
      card.append(
        el("div", {
          class: "mono",
          // The digest is of the bytes read back off disk after the file was
          // committed, so it describes the file that exists.
          text: `sha256 ${job.sha256}`,
        }),
      );
    }
    // A job from an ed2k link is judged on its eD2k hash, not its SHA-256. Showing only
    // the SHA-256 under "the digest does not match" points at the wrong number — the one
    // that was checked has to be the one on screen.
    if (job.ed2k) {
      card.append(el("div", { class: "mono", text: `ed2k ${job.ed2k}` }));
    }
    const expectedDigest = job.expectedEd2k
      ? { label: "ed2k", value: job.expectedEd2k }
      : job.expectedSha256
        ? { label: "sha256", value: job.expectedSha256 }
        : null;
    if (job.verification === "mismatch" && expectedDigest) {
      card.append(
        el("div", {
          class: "mono status-error",
          text: `expected ${expectedDigest.label} ${expectedDigest.value}`,
        }),
      );
    }

    const note = this.busy.get(job.id);
    if (note) card.append(el("div", { class: "muted", text: note }));

    if (!running) {
      const options = this.renderJobOptions(job);
      if (options) card.append(options);
    }

    card.append(this.renderJobActions(job, running));
    return card;
  }

  /** Per-job options: quality, audio-only, subtitles, expected digest. */
  private renderJobOptions(job: Job): HTMLElement | null {
    if (job.status === "done") return null;
    const rows: HTMLElement[] = [];

    // Options are baked into the output once bytes are written; changing
    // quality mid-file would splice two renditions together.
    const locked = job.receivedBytes > 0;

    if (job.kind === "hlsplaylist" && !locked) {
      rows.push(this.renderHlsOptions(job));
    }

    if (!locked && job.kind === "progressive") {
      const input = el("input", {
        type: "text",
        class: "grow mono",
        placeholder: "expected sha256 (optional)",
        spellcheck: false,
        value: job.expectedSha256 ?? "",
      });
      input.addEventListener("change", () => {
        void updateJob(job.id, {
          expectedSha256: input.value.trim() || null,
        }).then(() => this.scheduleRender());
      });
      rows.push(el("div", { class: "row" }, input));
    }

    return rows.length ? el("div", { class: "stack" }, ...rows) : null;
  }

  private renderHlsOptions(job: Job): HTMLElement {
    const row = el("div", { class: "row wrap" });
    const options = this.playlistCache.get(job.id);

    if (!options) {
      row.append(
        el("button", {
          text: "Quality, audio and subtitles…",
          onClick: (e) =>
            void this.loadPlaylistOptions(job, e.target as HTMLButtonElement),
        }),
      );
    } else if (options.variants.length > 0) {
      const select = el("select", {
        onChange: (e) =>
          void updateJob(job.id, {
            variantUrl: (e.target as HTMLSelectElement).value,
          }).then(() => this.scheduleRender()),
      });
      for (const v of options.variants) {
        const option = el("option", { value: v.url, text: describeVariant(v) });
        option.selected = job.variantUrl === v.url;
        select.append(option);
      }
      row.append(field("Quality", select));
    } else {
      row.append(el("span", { class: "muted", text: "Single quality only" }));
    }

    const audio = checkbox("Audio only", job.audioOnly ?? false, (checked) => {
      const base = job.filename.replace(/\.[^.]+$/, "");
      const rendition = options
        ? audioRenditionFor(options, job.variantUrl)
        : undefined;
      void updateJob(job.id, {
        audioOnly: checked,
        // A separate audio rendition is smaller to fetch and needs no demuxing,
        // so it is preferred when the master offers one.
        audioRenditionUrl: checked ? (rendition?.url ?? null) : null,
        filename: `${base}.${checked ? "m4a" : "mp4"}`,
      }).then(() => this.scheduleRender());
    });
    row.append(audio.row);

    if (options?.subtitles.length) {
      const select = el("select");
      for (const s of options.subtitles) {
        select.append(
          el("option", {
            value: s.group_id + "|" + s.name,
            text: describeRendition(s),
          }),
        );
      }
      const format = el("select");
      format.append(
        el("option", { value: "srt", text: "SRT" }),
        el("option", { value: "vtt", text: "WebVTT" }),
      );
      row.append(
        field("Subtitles", select),
        format,
        el("button", {
          text: "Save subtitles",
          onClick: () => {
            const chosen = options.subtitles.find(
              (s) => s.group_id + "|" + s.name === select.value,
            );
            if (chosen) {
              void this.saveSubtitles(
                job,
                chosen,
                format.value === "srt" ? "srt" : "vtt",
              );
            }
          },
        }),
      );
    }

    return row;
  }

  private renderJobActions(job: Job, running: boolean): HTMLElement {
    const actions = el("div", { class: "row wrap" });

    if (job.status !== "done") {
      if (running) {
        actions.append(
          el("button", {
            text: "Pause",
            onClick: () => this.queue.pause(job.id),
          }),
        );
      } else {
        actions.append(
          el("button", {
            class: "primary",
            text:
              job.receivedBytes > 0 ||
              job.status === "paused" ||
              job.status === "error"
                ? "Resume"
                : "Start",
            onClick: () => void this.queue.startInteractive(job.id),
          }),
        );
      }
    }

    const index = this.jobs.indexOf(job);
    if (this.jobs.length > 1) {
      actions.append(
        el("button", {
          text: "↑",
          title: "Run sooner",
          disabled: index === 0,
          onClick: () => void moveJob(job.id, -1).then(() => this.refresh()),
        }),
        el("button", {
          text: "↓",
          title: "Run later",
          disabled: index === this.jobs.length - 1,
          onClick: () => void moveJob(job.id, 1).then(() => this.refresh()),
        }),
      );
    }

    actions.append(
      el("span", { class: "grow" }),
      el("button", {
        class: "danger",
        text: "Remove",
        disabled: running,
        onClick: () => void deleteJob(job.id).then(() => this.refresh()),
      }),
    );
    return actions;
  }

  // ---- actions ---------------------------------------------------------

  private async saveSettings(patch: Partial<Settings>): Promise<void> {
    this.settings = await updateSettings(patch);
    this.render();
    await this.queue.tick();
  }

  private async chooseFolder(): Promise<void> {
    try {
      const handle = await pickFolder();
      await setFolderHandle(handle);
      await this.refresh();
      await this.queue.tick();
    } catch (e) {
      if ((e as { name?: string }).name !== "AbortError") throw e;
    }
  }

  private async regrantFolder(): Promise<void> {
    const folder = await getFolderHandle();
    if (folder && (await requestPermission(folder))) {
      await this.refresh();
      await this.queue.tick();
    }
  }

  private async forgetFolder(): Promise<void> {
    await setFolderHandle(null);
    await this.refresh();
  }

  private async clearDone(): Promise<void> {
    for (const job of this.jobs) {
      if (job.status === "done") await deleteJob(job.id);
    }
    await this.refresh();
  }

  private async loadPlaylistOptions(
    job: Job,
    button: HTMLButtonElement,
  ): Promise<void> {
    button.disabled = true;
    button.textContent = "Loading…";
    try {
      const core = await loadCore();
      const options = await listPlaylistOptions(job.url, core);
      this.playlistCache.set(job.id, options);
      await updateJob(job.id, {
        subtitles: options.subtitles,
        audioRenditions: options.audio,
      });
    } catch (e) {
      button.textContent = e instanceof Error ? e.message : "could not load";
      return;
    }
    this.scheduleRender();
  }

  private async saveSubtitles(
    job: Job,
    rendition: Rendition,
    format: "srt" | "vtt",
  ): Promise<void> {
    this.busy.set(job.id, `Fetching “${rendition.name}” subtitles…`);
    this.render();
    try {
      const result = await fetchSubtitleRendition(rendition, {
        format,
        baseName: job.filename.replace(/\.[^.]+$/, ""),
        onProgress: (done, total) => {
          this.busy.set(job.id, `Subtitles: segment ${done} of ${total}`);
          this.scheduleRender();
        },
      });
      await this.platform.saveBlob(
        new Blob([result.text], {
          type: format === "srt" ? "text/plain" : "text/vtt",
        }),
        result.filename,
      );
      this.busy.set(
        job.id,
        `Saved ${result.filename} (${result.cues.length} cues)`,
      );
    } catch (e) {
      this.busy.set(job.id, e instanceof Error ? e.message : String(e));
    }
    this.render();
  }
}

function describeVariant(v: {
  resolution: [number, number] | null;
  bandwidth: number;
  codecs: string | null;
}): string {
  const size = v.resolution ? `${v.resolution[1]}p` : "unknown size";
  return `${size} · ${(v.bandwidth / 1_000_000).toFixed(1)} Mbps`;
}

function describeRendition(r: Rendition): string {
  const parts = [r.name];
  if (r.language && r.language !== r.name) parts.push(`(${r.language})`);
  if (r.forced) parts.push("· forced");
  return parts.join(" ");
}

function statusLine(job: Job, progress: Progress | undefined): string {
  if (job.status === "error") return job.error ?? "failed";
  if (job.status === "done") {
    if (job.verification === "mismatch")
      return "complete, but the digest does not match";
    return `complete · ${formatSize(job.outputBytes)}`;
  }
  if (job.status === "paused") {
    // A merge has no resume point: its state is an interleave position across two
    // inputs, and persisting that safely is more machinery than the case warrants.
    // Saying "kept" would promise something the next start cannot deliver.
    return job.kind === "merge"
      ? "paused · will start again from the beginning"
      : `paused · ${formatSize(job.receivedBytes)} kept`;
  }
  if (job.status === "verifying") return "verifying checksum…";
  if (progress?.message) return progress.message;

  const received = progress?.received ?? job.receivedBytes;
  const total = progress?.total ?? job.totalBytes;
  const speed = formatSpeed(progress?.bytesPerSecond);
  const eta = formatEta(progress?.etaSeconds);
  const size = total
    ? `${formatSize(received)} of ${formatSize(total)}`
    : formatSize(received);
  if (received <= 0 && !speed) return job.status;
  return [size, speed, eta && `${eta} left`].filter(Boolean).join(" · ");
}

/**
 * Turn a pasted URL into a candidate.
 *
 * Classification normally happens in the service worker from real response
 * headers. A pasted URL has none, so this asks the server for them with a
 * one-byte range request before deciding — the same probe the engine makes
 * before a download, so it costs nothing extra and it is what lets a link like
 * `https://cdn.example/asset?id=9` be recognised at all. A URL whose path ends
 * in a known extension is classified without the probe, so an unreachable server
 * still produces a usable job rather than a refusal.
 */
export async function candidateForUrl(input: string): Promise<MediaCandidate> {
  const core = await loadCore();

  // A magnet or .torrent link is not malformed, it is unreachable from a browser tab,
  // and "only http and https URLs can be downloaded" does not say why. Checked first, so
  // the explanation wins over the generic scheme complaint below.
  const refusal = core.peer_link_refusal(input);
  if (refusal) throw new Error(refusal);

  // `thunder://`, `flashget://` and `qqdl://` are an ordinary URL inside a base64
  // wrapper. Unwrapped here rather than at each call site, so every host that accepts a
  // pasted link accepts these too.
  const url = core.resolve_download_link(input) ?? input;

  // A page on a site with its own extractor is not a media file, and saying "that link
  // does not look like a media file" about a Vimeo page is technically true and useless.
  // This box classifies a URL by what the server returns; extraction happens in the
  // popup, on the page itself. Name the site and say where to go.
  if (core.site_is_supported(url)) {
    const site = core.site_name_for(url) ?? "that site";
    throw new Error(
      `That is a ${site} page, not a media file. Open it in a tab and use the ` +
        `extension there — it reads the page and offers the qualities ${site} has. ` +
        `This box takes a direct link to a file or an .m3u8 playlist.`,
    );
  }

  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new Error("that is not a URL");
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("only http and https URLs can be downloaded");
  }

  if (core.is_restricted(parsed.origin, url)) {
    throw new Error(
      "opendownloader does not download from this host: it is a DRM or terms-restricted service.",
    );
  }

  const classify = (meta: {
    content_type: string | null;
    content_length: number | null;
    content_disposition: string | null;
  }): MediaCandidate | null => {
    const json = core.classify_request(
      JSON.stringify({ url, page_origin: parsed.origin, ...meta }),
    );
    return json ? (JSON.parse(json) as MediaCandidate) : null;
  };

  // The URL alone first: it needs no network, and a link ending in `.mp4` is
  // not made more identifiable by asking.
  const fromUrl = classify({
    content_type: null,
    content_length: null,
    content_disposition: null,
  });
  if (fromUrl) return fromUrl;

  let probeFailed = false;
  try {
    const response = await fetchWithRetry(url, {
      headers: { Range: "bytes=0-0" },
    });
    // Drain, so the connection is reusable for the download that follows.
    await response.arrayBuffer();

    // A 206 reports the slice's length, not the file's; the total is after the
    // slash in Content-Range.
    const contentRange = response.headers.get("content-range");
    const total = contentRange
      ? Number(contentRange.split("/")[1])
      : Number(response.headers.get("content-length"));

    const fromServer = classify({
      content_type: response.headers.get("content-type"),
      content_length: Number.isFinite(total) ? total : null,
      content_disposition: response.headers.get("content-disposition"),
    });
    if (fromServer) return fromServer;
  } catch (e) {
    probeFailed = looksLikeCorsFailure(e);
  }

  throw new Error(
    probeFailed
      ? "That server would not let this page read the file, so what it is cannot be " +
          "determined. The browser extension is not subject to that restriction, and neither " +
          "is a relay you run yourself."
      : "That link does not look like a media file. Direct links to a file or an .m3u8 " +
          "playlist work.",
  );
}
