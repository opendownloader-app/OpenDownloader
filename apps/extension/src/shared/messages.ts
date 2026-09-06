// Messages between the popup and the service worker.
//
// Only detection state goes through the worker. Enqueuing a job does not: the
// popup writes straight to the shared IndexedDB job store, which every extension
// context can reach because they are all the same origin. That keeps the worker
// off the critical path of starting a download, so an evicted worker can never
// lose a job.

import type { DetectedItem } from "@opendownloader/engine";

export type PopupRequest =
  | { action: "listCandidates"; tabId: number }
  | { action: "clearCandidates"; tabId: number };

export type PopupResponse =
  | { ok: true; candidates: DetectedItem[] }
  | { ok: true }
  | { ok: false; error: string };
