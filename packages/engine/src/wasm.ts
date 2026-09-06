// Loading the Rust core.
//
// The import below is deliberately **static**. `import()` is disallowed inside a
// ServiceWorkerGlobalScope by the HTML spec regardless of `"type": "module"` —
// that only enables static import/export — so a dynamically-imported wasm glue
// module simply does not work in an MV3 background worker. A static import lets
// Vite bundle the glue and rewrite wasm-bindgen's generated
// `new URL('dl_core_bg.wasm', import.meta.url)` into a proper hashed asset
// reference.
//
// The other half of this: MV3's default extension CSP blocks
// `WebAssembly.instantiate` outright unless `'wasm-unsafe-eval'` is present in
// `content_security_policy.extension_pages`. It is a required manifest field for
// any wasm-using extension, not an optional hardening choice.
import init, * as core from "./wasm-gen/dl_core.js";

let ready: Promise<void> | null = null;

export type DlCore = typeof core;

/** Initialise the wasm module once per JS realm, and hand back its exports. */
export async function loadCore(): Promise<DlCore> {
  if (!ready) {
    ready = init().then(() => undefined);
  }
  await ready;
  return core;
}

export type { DownloadSession, Mp4AudioExtractor } from "./wasm-gen/dl_core.js";
