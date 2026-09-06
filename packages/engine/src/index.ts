// The engine's public surface.
//
// Both front ends — the extension's manager tab and the standalone web app —
// import from here and nowhere else, which is what keeps them from drifting
// into two different downloaders.

export * from "./types";
export * from "./format";
export * from "./config";
export * from "./platform";
export * from "./settings";
export * from "./jobs";
export * from "./sinks";
export * from "./queue";
export * from "./subtitles";
export * from "./translate";
export * from "./local-tools";
export * from "./extract";
export { mergeSize, runMerge, type MergeOptions } from "./merge";
export {
  PausedError,
  audioRenditionFor,
  listPlaylistOptions,
  resolvePlaylist,
  runJob,
  type PlaylistOptions,
  type ResolvedPlaylist,
  type RunOptions,
} from "./engine";
export { fetchWithRetry, looksLikeCorsFailure, type RetryOptions } from "./fetch-retry";
export { loadCore, type DlCore } from "./wasm";
export {
  idbDelete,
  idbGet,
  idbGetAll,
  idbPut,
  openDb,
  STORE_CHUNKS,
  STORE_HANDLES,
  STORE_JOBS,
  STORE_SETTINGS,
} from "./idb";

// Conversion is exported from its own module rather than re-exported here:
// pulling it into the barrel would drag `mediabunny` into the extension's
// service worker bundle, which only ever classifies requests.
