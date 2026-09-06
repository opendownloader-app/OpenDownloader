// What the engine needs from its host, and the two hosts that provide it.
//
// The engine runs unchanged in an extension page and in a plain web page. The
// only things that genuinely differ are how a finished Blob is handed to the
// user (the `downloads` API versus an `<a download>` click) and whether that
// can happen without a user gesture — so those are the whole interface.

export interface Platform {
  /** Hand a finished file to the user. */
  saveBlob(blob: Blob, filename: string): Promise<void>;
  /**
   * Make the given headers apply to requests for `urls`, and return a function that
   * undoes it.
   *
   * Exists because `fetch` cannot set `Referer`, which is exactly the header several
   * platform CDNs require. The extension satisfies this with a session-scoped
   * `declarativeNetRequest` rule; a plain web page has no equivalent and leaves this
   * undefined, which is one more reason the extension reaches sites the web app cannot.
   */
  applyRequestHeaders?(
    urls: string[],
    headers: [string, string][],
  ): Promise<() => Promise<void>>;
  /**
   * Whether `saveBlob` works without a user gesture. True for the extension
   * (`downloads.download` needs none) and for the web page (a single
   * programmatic download is allowed; browsers only gate *repeated* ones,
   * which the queue spaces out anyway).
   */
  readonly canSaveSilently: boolean;
}

/** A plain web page: an anchor with `download`, clicked. */
export const webPlatform: Platform = {
  canSaveSilently: true,
  // No `applyRequestHeaders`: a page cannot override a forbidden header, and pretending
  // otherwise would turn a clear "this needs the extension" into a mysterious 403.
  async saveBlob(blob, filename) {
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    a.style.display = "none";
    document.body.append(a);
    a.click();
    a.remove();
    // Revoking immediately would race the download starting.
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  },
};

/**
 * Whether this browser can write to a caller-chosen file on disk.
 *
 * Chrome and Edge expose the File System Access API, which gives the engine a
 * real, seekable file handle — the prerequisite for writing parallel range
 * downloads out of order, and for a download folder that needs one prompt per
 * session rather than one per file. Firefox and Safari have no equivalent, so
 * they fall back to accumulating in IndexedDB and saving a Blob, which is
 * sequential-only. This is the one genuine capability difference between the
 * targets, and it is feature-detected rather than inferred from a user agent.
 */
export function hasFileSystemAccess(): boolean {
  const g = globalThis as { showSaveFilePicker?: unknown; showDirectoryPicker?: unknown };
  return typeof g.showSaveFilePicker === "function" && typeof g.showDirectoryPicker === "function";
}
