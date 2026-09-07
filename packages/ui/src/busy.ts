// The wait, made visible.
//
// Resolving a magnet or a `.torrent` is the one thing either front end does that can
// take a minute with nothing to show: the app has to be started, the swarm has to be
// found, and a peer has to answer with the file list. Until it does there is no
// percentage that would be true, so this deliberately shows an *indeterminate* bar
// rather than a progress bar inventing a number — and carries the words that say which
// part of the wait is happening.

import { el } from "./dom";

/** A line to show once the wait has gone on this long, in seconds. */
export interface Stage {
  after: number;
  text: string;
}

export class Busy {
  /** The bar. Mount it wherever the wait belongs; the line is mounted separately. */
  readonly root: HTMLElement;
  private readonly line: HTMLElement;
  private readonly bar: HTMLElement;
  private timers: ReturnType<typeof setTimeout>[] = [];

  /**
   * @param line An existing status paragraph to write into. Without one a paragraph is
   *   created and `root` carries both it and the bar, which is what the manager wants;
   *   the web app already has a status line under its box and passes it in.
   */
  constructor(line?: HTMLElement) {
    const bar = el("div", { class: "bar" }, el("i"));
    bar.hidden = true;
    bar.setAttribute("role", "progressbar");
    this.line = line ?? el("p", { class: "muted" });
    // A live region, so the staged messages below reach a screen reader too — where the
    // bar says only "still working", the words are the whole of the information.
    this.line.setAttribute("role", "status");
    this.line.setAttribute("aria-live", "polite");
    this.root = line ? bar : el("div", { class: "stack" }, bar, this.line);
    this.bar = bar;
  }

  /** Begin a wait: show the bar, say what is happening, and schedule the later lines. */
  start(message: string, stages: Stage[] = []): void {
    this.clear();
    this.bar.hidden = false;
    this.say(message);
    for (const stage of stages) {
      this.timers.push(
        setTimeout(() => this.say(stage.text), stage.after * 1000),
      );
    }
  }

  /** Change the line without touching the bar or the schedule. */
  say(message: string): void {
    this.line.className = "muted";
    this.line.textContent = message;
  }

  /**
   * Take the bar down and stop the schedule, leaving the line exactly as it is.
   *
   * For a caller that has already written its own last word — the web app's `add()`
   * ends a dozen different ways, each with its own sentence, and all of them want the
   * bar gone.
   */
  settle(): void {
    this.clear();
    this.bar.hidden = true;
  }

  /** End the wait. An empty message leaves the line blank, which is the usual case. */
  done(message = ""): void {
    this.settle();
    this.line.className = "muted";
    this.line.textContent = message;
  }

  /** End the wait badly. The message stays on screen; the bar does not. */
  fail(message: string): void {
    this.settle();
    this.line.className = "status-error";
    this.line.textContent = message;
  }

  private clear(): void {
    for (const timer of this.timers) clearTimeout(timer);
    this.timers = [];
  }
}

/**
 * What to say while a torrent is being resolved, and when.
 *
 * The first two are the honest description of a wait that is nobody's fault: a swarm
 * answers when a peer feels like it. The last is the point at which "slow" and "dead"
 * stop being distinguishable from here, so it says so rather than spinning silently —
 * the bridge's own 60-second timeout will follow with the peer counts.
 */
export const TORRENT_STAGES: Stage[] = [
  {
    after: 8,
    text: "Still looking for peers. A torrent has to find someone sharing it before it can say what is inside.",
  },
  {
    after: 25,
    text: "Still no answer. A quiet swarm can take a minute; one with nobody left in it never answers at all.",
  },
];
