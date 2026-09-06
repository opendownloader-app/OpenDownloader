// Where downloaded bytes go.
//
// Three implementations, one interface. The difference between them is not
// cosmetic: the two File System Access sinks have a real seekable file handle,
// so they can accept range chunks written out of order and therefore support
// parallel connections. `BlobSink` cannot seek, so its jobs run on a single
// connection. `canSeek` is what the engine reads to decide, rather than
// sniffing the user agent.
//
// The second axis is prompting. `FileSink` asks the user where to save, once
// per file, and that ask needs a live user gesture — which is what stops a
// queue from running unattended. `FolderSink` asks once for a directory and
// then creates files inside it with no further prompts, which is what makes
// batch downloading and auto-start possible at all.

import { engineConfig } from "./config";
import { getFolderHandle } from "./settings";
import { idbGetAll, idbPut, STORE_CHUNKS } from "./idb";
import { clearChunks, getJobHandle, putJobHandle } from "./jobs";
import { hasFileSystemAccess, type Platform } from "./platform";
import type { SinkKind } from "./types";


export interface Sink {
  readonly kind: SinkKind;
  /** True when `write` may be called with positions out of order. */
  readonly canSeek: boolean;
  write(pos: number, bytes: Uint8Array): Promise<void>;
  /** Flush and hand the finished file to the user. */
  finalize(): Promise<void>;
  /**
   * Stream the completed file back for verification.
   *
   * Hashing on the way *out* would only prove we hashed what we meant to write.
   * Reading back proves what actually landed, which is the only version of this
   * check that means anything for a download that was interrupted and resumed.
   *
   * **Must be called after `finalize()`.** File System Access writes go to a
   * swap file that is not committed to the real file until the writable stream
   * closes, so reading back before finalizing returns the file as it was
   * *before* the download — for a newly created save target, zero bytes. That
   * would yield the empty-string digest for every download: a plausible-looking
   * constant that verifies nothing.
   */
  readBack(onChunk: (bytes: Uint8Array) => void): Promise<void>;
  /**
   * Release anything the sink was holding for `readBack`. Separate from
   * `finalize` precisely so verification can happen in between.
   */
  cleanup(): Promise<void>;
}

/** Chunk size for the verification read-back. Bounded so memory stays flat. */
const READ_BACK_CHUNK = 4 * 1024 * 1024;

/**
 * A file on disk the engine holds a handle to.
 *
 * Both File System Access sinks share this: the only thing that differs between
 * them is how the handle was obtained — a save dialog for one file, or
 * `getFileHandle` inside a previously granted directory.
 */
class HandleSink implements Sink {
  readonly kind = "file" as const;
  readonly canSeek = true;
  private writable: FileSystemWritableFileStream | null = null;

  constructor(private readonly handle: FileSystemFileHandle) {}

  /** Open the file for writing. `keepExistingData` is what makes resuming work. */
  async open(resuming: boolean): Promise<void> {
    // Without keepExistingData the file is truncated the moment the stream
    // opens, so a resume would write its remaining ranges into a file whose
    // earlier bytes had just been thrown away. Equally, a *fresh* download must
    // NOT keep existing data: re-downloading over a longer previous file would
    // leave that file's tail past the end of the new content.
    this.writable = await this.handle.createWritable({ keepExistingData: resuming });
  }

  async write(pos: number, bytes: Uint8Array): Promise<void> {
    if (!this.writable) throw new Error("sink already finalized");
    // TypeScript models `Uint8Array` as possibly backed by a SharedArrayBuffer,
    // which `FileSystemWriteChunkType` rejects. That case cannot arise here: no
    // page here sets COOP/COEP, so SharedArrayBuffer is not even constructible
    // in this realm, and every buffer reaching this method comes from `fetch`
    // or from wasm-bindgen (which copies out of wasm memory).
    const data = bytes as unknown as BufferSource;
    await this.writable.write({ type: "write", position: pos, data });
  }

  async finalize(): Promise<void> {
    if (!this.writable) return;
    await this.writable.close();
    this.writable = null;
  }

  async readBack(onChunk: (bytes: Uint8Array) => void): Promise<void> {
    if (this.writable) {
      // Reading now would return the pre-download file, since the swap file has
      // not been committed yet. Fail loudly rather than hand back a digest of
      // zero bytes that looks like a successful verification.
      throw new Error("readBack called before finalize; the file is not committed yet");
    }
    const file = await this.handle.getFile();
    for (let offset = 0; offset < file.size; offset += READ_BACK_CHUNK) {
      const slice = file.slice(offset, Math.min(offset + READ_BACK_CHUNK, file.size));
      onChunk(new Uint8Array(await slice.arrayBuffer()));
    }
  }

  async cleanup(): Promise<void> {
    // Nothing to release: the file belongs to the user's filesystem now.
  }
}

interface ChunkRecord {
  jobId: string;
  pos: number;
  bytes: ArrayBuffer;
}

/**
 * No file handle: accumulate in IndexedDB, then hand a Blob to the platform.
 *
 * IndexedDB is disk-backed, so a multi-gigabyte download does not sit in memory
 * while it accumulates. The genuine ceiling is at the end, when the chunks are
 * materialised into a single Blob to be saved — that is a real limit, and it is
 * stated plainly in the UI rather than being discovered as a crash.
 */
export class BlobSink implements Sink {
  readonly kind = "blob" as const;
  readonly canSeek = false;

  constructor(
    private readonly jobId: string,
    private readonly filename: string,
    private readonly mime: string,
    private readonly platform: Platform,
  ) {}

  async write(pos: number, bytes: Uint8Array): Promise<void> {
    const record: ChunkRecord = {
      jobId: this.jobId,
      pos,
      // Copy out of the wasm-adjacent view before storing: structured clone of a
      // subarray would otherwise capture the whole underlying buffer.
      bytes: bytes.slice().buffer,
    };
    await idbPut(STORE_CHUNKS, record);
  }

  private async chunks(): Promise<ChunkRecord[]> {
    const chunks = await idbGetAll<ChunkRecord>(
      STORE_CHUNKS,
      IDBKeyRange.bound([this.jobId, 0], [this.jobId, Number.MAX_SAFE_INTEGER]),
    );
    // The compound key sorts by position already, but an explicit sort makes the
    // ordering guarantee local to this function instead of implicit in the schema.
    chunks.sort((a, b) => a.pos - b.pos);
    return chunks;
  }

  async finalize(): Promise<void> {
    const chunks = await this.chunks();
    const blob = new Blob(
      chunks.map((c) => c.bytes),
      { type: this.mime },
    );
    await this.platform.saveBlob(blob, this.filename);
  }

  async readBack(onChunk: (bytes: Uint8Array) => void): Promise<void> {
    // The chunks are still in IndexedDB here — `cleanup()`, not `finalize()`,
    // is what clears them, so this reads exactly the bytes that were assembled
    // into the Blob handed to the platform.
    for (const c of await this.chunks()) {
      onChunk(new Uint8Array(c.bytes));
    }
  }

  async cleanup(): Promise<void> {
    await clearChunks(this.jobId);
  }
}

/** Whether a handle's permission is already granted, without prompting. */
async function hasPermission(
  handle: FileSystemHandle & {
    queryPermission?: (d: { mode: "read" | "readwrite" }) => Promise<PermissionState>;
  },
): Promise<boolean> {
  if (!handle.queryPermission) return true;
  try {
    return (await handle.queryPermission({ mode: "readwrite" })) === "granted";
  } catch {
    return false;
  }
}

/**
 * Ask for a handle's permission back. Needs a user gesture, so it is only
 * called from a click — a folder chosen in a previous session comes back as
 * `prompt` and the UI offers a button rather than failing the download.
 */
export async function requestPermission(
  handle: FileSystemHandle & {
    requestPermission?: (d: { mode: "read" | "readwrite" }) => Promise<PermissionState>;
  },
): Promise<boolean> {
  if (!handle.requestPermission) return true;
  try {
    return (await handle.requestPermission({ mode: "readwrite" })) === "granted";
  } catch {
    return false;
  }
}

/** Let the user pick the folder every unattended download is written into. */
export async function pickFolder(): Promise<FileSystemDirectoryHandle> {
  const picker = (
    globalThis as unknown as {
      showDirectoryPicker: (o: {
        mode: "readwrite";
        id: string;
      }) => Promise<FileSystemDirectoryHandle>;
    }
  ).showDirectoryPicker;
  return picker({ mode: "readwrite", id: "opendownloader" });
}

/**
 * A name inside `dir` that is not already taken by a *different* download.
 *
 * `wanted` itself is returned when the directory does not contain it, or when
 * this job already owns it (a resume must land in the same file, not in
 * "video (1).mp4"). Otherwise the usual " (n)" suffix is appended.
 */
async function freeName(
  dir: FileSystemDirectoryHandle,
  wanted: string,
  ownedByThisJob: string | undefined,
): Promise<string> {
  const exists = async (name: string): Promise<boolean> => {
    try {
      await dir.getFileHandle(name);
      return true;
    } catch {
      return false;
    }
  };

  if (ownedByThisJob) return ownedByThisJob;
  if (!(await exists(wanted))) return wanted;

  const dot = wanted.lastIndexOf(".");
  const stem = dot > 0 ? wanted.slice(0, dot) : wanted;
  const ext = dot > 0 ? wanted.slice(dot) : "";
  for (let n = 1; n < 1000; n++) {
    const candidate = `${stem} (${n})${ext}`;
    if (!(await exists(candidate))) return candidate;
  }
  // Practically unreachable; a timestamp is still better than overwriting.
  return `${stem} (${Date.now()})${ext}`;
}

export interface SinkRequest {
  jobId: string;
  filename: string;
  mime: string;
  /** True when the job already has bytes on disk that must be preserved. */
  resuming: boolean;
  platform: Platform;
}

/**
 * Whether a sink can be opened right now without a user gesture.
 *
 * This is what the queue consults before auto-starting: on Chrome and Edge that
 * means a download folder has been chosen and its permission is still granted;
 * everywhere else the blob sink needs no gesture at all and the answer is yes.
 */
export async function canOpenSinkSilently(): Promise<boolean> {
  if (engineConfig.forceBlobSink) return true;
  if (!hasFileSystemAccess()) return true;
  const folder = await getFolderHandle();
  if (!folder) return false;
  return hasPermission(folder);
}

/**
 * Open the best sink available for this job, without prompting.
 *
 * Order: the file handle this job already used (a resume must continue the same
 * file), then a file inside the chosen download folder, then the blob fallback.
 * A save dialog is never opened from here — that is `createSinkInteractive`,
 * which must be called straight from a click.
 */
export async function createSink(req: SinkRequest): Promise<Sink> {
  if (hasFileSystemAccess() && !engineConfig.forceBlobSink) {
    const existing = await getJobHandle(req.jobId);
    if (existing && (await hasPermission(existing))) {
      const sink = new HandleSink(existing);
      await sink.open(req.resuming);
      return sink;
    }

    const folder = await getFolderHandle();
    if (folder && (await hasPermission(folder))) {
      const name = await freeName(folder, req.filename, undefined);
      const handle = await folder.getFileHandle(name, { create: true });
      await putJobHandle(req.jobId, handle);
      const sink = new HandleSink(handle);
      // A brand-new file has nothing to keep, and `resuming` can only be true
      // here if the previous handle was lost — in which case the old bytes are
      // gone anyway and the job restarts.
      await sink.open(false);
      return sink;
    }
  }
  return new BlobSink(req.jobId, req.filename, req.mime, req.platform);
}

/**
 * Open a sink, prompting with a save dialog when that is the only option.
 *
 * **Must be called synchronously from a user gesture.** `showSaveFilePicker`
 * requires transient activation, and activation is consumed or expires across
 * the awaits a download performs — so the sink is opened first, in the click
 * handler, and handed to the engine afterwards.
 */
export async function createSinkInteractive(req: SinkRequest): Promise<Sink> {
  if (hasFileSystemAccess() && !engineConfig.forceBlobSink) {
    const existing = await getJobHandle(req.jobId);
    if (existing && (await hasPermission(existing))) {
      const sink = new HandleSink(existing);
      await sink.open(req.resuming);
      return sink;
    }
    const folder = await getFolderHandle();
    if (folder && (await hasPermission(folder))) return createSink(req);

    try {
      const handle = await (
        globalThis as unknown as {
          showSaveFilePicker: (o: { suggestedName: string }) => Promise<FileSystemFileHandle>;
        }
      ).showSaveFilePicker({ suggestedName: req.filename });
      await putJobHandle(req.jobId, handle);
      const sink = new HandleSink(handle);
      await sink.open(req.resuming);
      return sink;
    } catch (e) {
      // Three distinct ways the picker can decline to produce a handle, all of
      // which mean "no file handle available" rather than "this download is
      // broken": the user cancelled (AbortError), there was no valid transient
      // activation or the embedder forbids it (NotAllowedError/SecurityError).
      // Anything else is a real fault and must not be swallowed.
      const name = (e as { name?: string }).name;
      if (name !== "AbortError" && name !== "NotAllowedError" && name !== "SecurityError") {
        throw e;
      }
      if (name === "AbortError") throw e;
    }
  }
  return new BlobSink(req.jobId, req.filename, req.mime, req.platform);
}
