// Engine-wide knobs a host sets once at startup.
//
// These are not user settings (those live in `settings.ts`); they are things
// only a build or a host knows — how big a range request should be, and
// whether URLs need rewriting through a relay.

export interface EngineConfig {
  /**
   * Bytes per range request. Large enough to amortise the round trip.
   *
   * The extension's E2E build shrinks this drastically so a small fixture
   * still produces many chunks — the multi-chunk resume path is the one worth
   * testing, and it would never be reached by an 8 MiB chunk against a
   * 512 KiB file.
   */
  chunkSize: number;
  /**
   * Rewrite a URL before it is fetched. The web app installs one that routes
   * through a relay when the user has configured one; the extension never
   * needs it.
   */
  rewriteUrl: (url: string) => string;
  /**
   * Whether this host can put a forbidden header on the wire.
   *
   * True in the extension, which sets them with `declarativeNetRequest`. False in a
   * plain page, where the only route is a relay. Declared rather than inferred: the
   * engine's other clue — whether the URL was rewritten — is false in the extension for
   * the opposite reason, so inferring from it blames the wrong thing.
   */
  canSendForbiddenHeaders: boolean;
  /**
   * Never open a File System Access sink, even where one is available.
   *
   * Set by the extension's E2E build. Automation can click a button but it
   * cannot answer a native save dialog, so a test run that reached
   * `showSaveFilePicker` would hang or be cancelled by the browser and every
   * download would sit queued forever. Forcing the blob sink exercises the same
   * engine, the same verification pass and the same queue — only the
   * destination differs, and the destination is the one thing a headless run
   * cannot assert about anyway.
   */
  forceBlobSink: boolean;
}

export const engineConfig: EngineConfig = {
  chunkSize: 8 * 1024 * 1024,
  rewriteUrl: (url) => url,
  canSendForbiddenHeaders: false,
  forceBlobSink: false,
};

export function configureEngine(patch: Partial<EngineConfig>): void {
  Object.assign(engineConfig, patch);
}
