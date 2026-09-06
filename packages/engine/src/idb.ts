// A minimal IndexedDB wrapper.
//
// IndexedDB is used rather than `chrome.storage` for three reasons the storage
// API cannot meet: it holds the partially-downloaded bytes of the blob sink
// (which would blow through `storage.local`'s 10 MB quota within seconds), it
// can store File System Access handles by structured clone, and it is reachable
// identically from the popup, the manager tab, the service worker and a plain
// web page.

const DB_NAME = "opendownloader";
const DB_VERSION = 2;

/** Job records, keyed by job id. */
export const STORE_JOBS = "jobs";
/** Sink chunks for the non-seeking fallback, keyed by `[jobId, position]`. */
export const STORE_CHUNKS = "chunks";
/** Settings and the download-folder handle, keyed by name. */
export const STORE_SETTINGS = "settings";
/** Per-job `FileSystemFileHandle`s, keyed by job id, so a resume reopens the same file. */
export const STORE_HANDLES = "handles";

let dbPromise: Promise<IDBDatabase> | null = null;

export function openDb(): Promise<IDBDatabase> {
  if (!dbPromise) {
    dbPromise = new Promise((resolve, reject) => {
      const req = indexedDB.open(DB_NAME, DB_VERSION);
      req.onupgradeneeded = () => {
        const db = req.result;
        if (!db.objectStoreNames.contains(STORE_JOBS)) {
          db.createObjectStore(STORE_JOBS, { keyPath: "id" });
        }
        if (!db.objectStoreNames.contains(STORE_CHUNKS)) {
          // Compound key so a job's chunks read back in position order, which is
          // exactly the order they must be concatenated in.
          db.createObjectStore(STORE_CHUNKS, { keyPath: ["jobId", "pos"] });
        }
        if (!db.objectStoreNames.contains(STORE_SETTINGS)) {
          db.createObjectStore(STORE_SETTINGS);
        }
        if (!db.objectStoreNames.contains(STORE_HANDLES)) {
          db.createObjectStore(STORE_HANDLES);
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error ?? new Error("failed to open IndexedDB"));
    });
  }
  return dbPromise;
}

function promisify<T>(req: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("IndexedDB request failed"));
  });
}

export async function idbPut(store: string, value: unknown, key?: IDBValidKey): Promise<void> {
  const db = await openDb();
  const tx = db.transaction(store, "readwrite");
  await promisify(tx.objectStore(store).put(value, key));
}

export async function idbGet<T>(store: string, key: IDBValidKey): Promise<T | undefined> {
  const db = await openDb();
  const tx = db.transaction(store, "readonly");
  return promisify(tx.objectStore(store).get(key) as IDBRequest<T | undefined>);
}

export async function idbGetAll<T>(store: string, query?: IDBKeyRange): Promise<T[]> {
  const db = await openDb();
  const tx = db.transaction(store, "readonly");
  return promisify(tx.objectStore(store).getAll(query) as IDBRequest<T[]>);
}

export async function idbDelete(store: string, key: IDBValidKey | IDBKeyRange): Promise<void> {
  const db = await openDb();
  const tx = db.transaction(store, "readwrite");
  await promisify(tx.objectStore(store).delete(key));
}
