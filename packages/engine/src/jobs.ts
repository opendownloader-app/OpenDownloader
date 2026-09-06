// The job store: the durable record of what has been asked for and how far it got.
//
// Every mutation is written straight through to IndexedDB rather than held in a
// module variable. That is not caution for its own sake — the manager tab can be
// closed mid-download and the service worker can be evicted at any moment, and a
// resumable downloader that forgets its progress when either happens is not
// actually resumable.

import {
  idbDelete,
  idbGet,
  idbGetAll,
  idbPut,
  openDb,
  STORE_CHUNKS,
  STORE_HANDLES,
  STORE_JOBS,
} from "./idb";
import type { Job, MediaCandidate } from "./types";

/**
 * A job id derived from the URL, so re-adding the same media resumes the
 * existing job instead of starting a competing second copy of it.
 */
export function jobIdFor(url: string): string {
  let hash = 2166136261;
  for (let i = 0; i < url.length; i++) {
    hash ^= url.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(36) + "-" + url.length.toString(36);
}

/** Every job, in queue order (lowest `order` first). */
export async function listJobs(): Promise<Job[]> {
  const jobs = await idbGetAll<Job>(STORE_JOBS);
  // Records written before `order` existed sort by creation time, which is
  // what the old UI showed — newest first — inverted so the queue reads
  // top-down in the order things will run.
  return jobs
    .map((j) => ({ ...j, order: j.order ?? j.createdAt }))
    .sort((a, b) => a.order - b.order || a.createdAt - b.createdAt);
}

export async function getJob(id: string): Promise<Job | undefined> {
  return idbGet<Job>(STORE_JOBS, id);
}

export async function putJob(job: Job): Promise<void> {
  await idbPut(STORE_JOBS, job);
}

/** Merge a partial update into a stored job. Returns the updated record. */
export async function updateJob(
  id: string,
  patch: Partial<Job>,
): Promise<Job | undefined> {
  const existing = await getJob(id);
  if (!existing) return undefined;
  const updated = { ...existing, ...patch };
  await putJob(updated);
  return updated;
}

export interface EnqueueOptions {
  pageUrl?: string;
  audioOnly?: boolean;
  expectedSha256?: string | null;
  expectedEd2k?: string | null;
}

/** Create a job from a candidate, or return the existing one. */
export async function enqueueCandidate(
  candidate: MediaCandidate,
  opts: EnqueueOptions = {},
): Promise<Job> {
  const id = jobIdFor(candidate.url);
  const existing = await getJob(id);
  if (existing) {
    // Re-queuing a failed or paused job is how the UI's "retry" works.
    if (existing.status === "error" || existing.status === "paused") {
      const revived = { ...existing, status: "queued" as const, error: null };
      await putJob(revived);
      return revived;
    }
    return existing;
  }

  const now = Date.now();
  const filename =
    opts.audioOnly && candidate.kind === "hlsplaylist"
      ? candidate.filename.replace(/\.[^.]+$/, "") + ".m4a"
      : candidate.filename;
  const job: Job = {
    id,
    url: candidate.url,
    filename,
    kind: candidate.kind,
    status: "queued",
    stateJson: "",
    totalBytes: candidate.size,
    receivedBytes: 0,
    outputBytes: 0,
    sha256: null,
    error: null,
    createdAt: now,
    order: now,
    pageUrl: opts.pageUrl,
    audioOnly: opts.audioOnly ?? false,
    expectedSha256: opts.expectedSha256 ?? null,
    expectedEd2k: opts.expectedEd2k ?? null,
    verification: "unverified",
  };
  await putJob(job);
  return job;
}

/** Remove a job, every sink chunk it accumulated, and its file handle. */
export async function deleteJob(id: string): Promise<void> {
  await clearChunks(id);
  await idbDelete(STORE_HANDLES, id);
  await idbDelete(STORE_JOBS, id);
}

export async function clearChunks(jobId: string): Promise<void> {
  const db = await openDb();
  const tx = db.transaction(STORE_CHUNKS, "readwrite");
  // The compound key is [jobId, pos], so this range covers exactly one job's
  // chunks and nothing else.
  tx.objectStore(STORE_CHUNKS).delete(
    IDBKeyRange.bound([jobId, 0], [jobId, Number.MAX_SAFE_INTEGER]),
  );
  await new Promise<void>((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error ?? new Error("failed to clear chunks"));
  });
}

/**
 * Swap a job's queue position with its neighbour.
 *
 * `delta` is −1 to move up (run sooner) or +1 to move down. Orders are swapped
 * rather than renumbered so two records change, not the whole store.
 */
export async function moveJob(id: string, delta: -1 | 1): Promise<void> {
  const jobs = await listJobs();
  const index = jobs.findIndex((j) => j.id === id);
  const other = jobs[index + delta];
  const self = jobs[index];
  if (!self || !other) return;
  const a = self.order;
  const b = other.order;
  // Two jobs enqueued in the same millisecond share an order; nudge so the
  // swap actually changes the sort.
  const [newSelf, newOther] =
    a === b ? (delta < 0 ? [a - 1, a] : [a + 1, a]) : [b, a];
  await putJob({ ...self, order: newSelf });
  await putJob({ ...other, order: newOther });
}

/** The stored file handle for a job, if one was opened on a seekable sink. */
export async function getJobHandle(
  id: string,
): Promise<FileSystemFileHandle | undefined> {
  return idbGet<FileSystemFileHandle>(STORE_HANDLES, id);
}

export async function putJobHandle(
  id: string,
  handle: FileSystemFileHandle,
): Promise<void> {
  await idbPut(STORE_HANDLES, handle, id);
}
