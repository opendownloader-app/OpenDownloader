// User settings, and the download-folder handle.
//
// Both live in IndexedDB rather than `localStorage`: the folder handle is a
// `FileSystemDirectoryHandle`, which only IndexedDB can persist (by structured
// clone), and keeping the plain settings beside it means one storage path
// instead of two.

import { idbGet, idbPut, STORE_SETTINGS } from "./idb";

export interface Settings {
  /** Jobs the queue runs at the same time. */
  maxConcurrentJobs: number;
  /** Parallel range requests per progressive job, when the server and sink allow. */
  connectionsPerJob: number;
  /** Start queued jobs automatically whenever a sink can be opened without a gesture. */
  autoStart: boolean;
  /**
   * A self-hosted relay (`crates/dl-relay`) the web app fetches through when a
   * host does not send CORS headers. Empty means fetch directly. The extension
   * never needs one — its host permissions bypass CORS.
   */
  relayUrl: string;
  /** Route every fetch through the relay, not just the ones that fail CORS. */
  useRelay: boolean;
}

export const DEFAULT_SETTINGS: Settings = {
  maxConcurrentJobs: 3,
  connectionsPerJob: 4,
  autoStart: true,
  relayUrl: "",
  useRelay: false,
};

export const MAX_CONCURRENT_JOBS = 6;
export const MAX_CONNECTIONS = 8;

const KEY_SETTINGS = "settings";
const KEY_FOLDER = "folder";

function clamp(value: unknown, fallback: number, min: number, max: number): number {
  const n = typeof value === "number" && Number.isFinite(value) ? Math.round(value) : fallback;
  return Math.min(max, Math.max(min, n));
}

/** Read settings, filling anything missing with the defaults. */
export async function getSettings(): Promise<Settings> {
  const stored = (await idbGet<Partial<Settings>>(STORE_SETTINGS, KEY_SETTINGS)) ?? {};
  return {
    maxConcurrentJobs: clamp(stored.maxConcurrentJobs, DEFAULT_SETTINGS.maxConcurrentJobs, 1, MAX_CONCURRENT_JOBS),
    connectionsPerJob: clamp(stored.connectionsPerJob, DEFAULT_SETTINGS.connectionsPerJob, 1, MAX_CONNECTIONS),
    autoStart: stored.autoStart ?? DEFAULT_SETTINGS.autoStart,
    relayUrl: typeof stored.relayUrl === "string" ? stored.relayUrl.trim() : "",
    useRelay: stored.useRelay ?? false,
  };
}

export async function updateSettings(patch: Partial<Settings>): Promise<Settings> {
  const next = { ...(await getSettings()), ...patch };
  await idbPut(STORE_SETTINGS, next, KEY_SETTINGS);
  return next;
}

/** The directory the folder sink writes into, if the user has chosen one. */
export async function getFolderHandle(): Promise<FileSystemDirectoryHandle | undefined> {
  return idbGet<FileSystemDirectoryHandle>(STORE_SETTINGS, KEY_FOLDER);
}

export async function setFolderHandle(handle: FileSystemDirectoryHandle | null): Promise<void> {
  await idbPut(STORE_SETTINGS, handle ?? undefined, KEY_FOLDER);
}
