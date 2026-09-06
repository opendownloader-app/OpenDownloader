import { copyFileSync, createReadStream, existsSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { defineConfig, type Plugin } from "vite";

// The standalone site. One page, no router, no framework — the whole app is the
// shared manager plus a link box.
// OPENDOWNLOADER_E2E gates one test-only affordance: a `__test` hook exposing
// the local tools, so the smoke test can drive them without the file picker —
// which is browser UI and cannot be automated. Unset, `if (false)` eliminates
// the hook from the shipped bundle entirely.
const e2e = process.env.OPENDOWNLOADER_E2E === "1";

/**
 * Put ONNX Runtime's WebAssembly next to the app, so it is served from this origin.
 *
 * `transcribe.ts` points ORT at `./ort/` precisely so the runtime does not come from a
 * public CDN — the product's claim is that nothing leaves your machine except what you
 * chose to fetch, and a silent third-party runtime download would make that false. But
 * pointing at a directory is only half the job: without this, `./ort/` does not exist,
 * every request for the runtime 404s, and transcription fails on a file that was never
 * copied. Nothing caught it because the panel opens a native file picker, which the
 * smoke test cannot drive.
 *
 * Only the builds ORT can actually select are copied — the plain, JSEP and asyncify
 * threaded ones. ORT ships about 74 MB of variants and most are for targets this app
 * never runs on, so copying the lot to serve three of them would be silly.
 */
function onnxRuntimeAssets(): Plugin {
  const from = resolve(__dirname, "../../node_modules/onnxruntime-web/dist");
  // `.mjs` loader beside each `.wasm`: ORT fetches the pair, not the binary alone.
  const needed = [
    "ort-wasm-simd-threaded.wasm",
    "ort-wasm-simd-threaded.mjs",
    "ort-wasm-simd-threaded.jsep.wasm",
    "ort-wasm-simd-threaded.jsep.mjs",
    // Asyncify too, not because `transcribe.ts` asks for it but because ORT decides
    // which build to load from what the browser supports. Any variant it can choose has
    // to be here: one that is missing does not fall back to a CDN, it 404s, and the
    // whole point of the `./ort/` redirect is that no third party is ever asked.
    "ort-wasm-simd-threaded.asyncify.wasm",
    "ort-wasm-simd-threaded.asyncify.mjs",
  ];
  return {
    name: "onnx-runtime-assets",
    // Dev: serve them from the same `/ort/` path the built site uses, so the two
    // behave identically rather than only production being right.
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const name = req.url?.split("?")[0]?.replace(/^\/ort\//, "");
        if (!name || !needed.includes(name)) return next();
        const file = join(from, name);
        if (!existsSync(file)) return next();
        res.setHeader(
          "Content-Type",
          name.endsWith(".wasm") ? "application/wasm" : "text/javascript",
        );
        createReadStream(file).pipe(res);
      });
    },
    closeBundle() {
      const to = resolve(__dirname, "dist/ort");
      mkdirSync(to, { recursive: true });
      for (const name of needed) {
        const file = join(from, name);
        if (!existsSync(file)) {
          // Loud, not silent: a missing runtime is a broken feature, and finding that
          // out at build time beats finding it out when someone transcribes something.
          throw new Error(
            `onnxruntime-web is missing ${name} — transcription would 404 at runtime`,
          );
        }
        copyFileSync(file, join(to, name));
      }
    },
  };
}

export default defineConfig({
  root: resolve(__dirname),
  plugins: [onnxRuntimeAssets()],
  define: {
    __OPENDOWNLOADER_E2E__: JSON.stringify(e2e),
  },
  // Relative asset URLs so the built site works from any path — a subdirectory,
  // a static host's root, or file:// — without a rebuild.
  base: "./",
  resolve: {
    // The workspace packages are consumed as TypeScript source, not as a build
    // artifact: there is no compile step between them and this bundle, so a
    // change in the engine shows up here with no rebuild dance.
    alias: {
      "@opendownloader/engine/convert": resolve(
        __dirname,
        "../../packages/engine/src/convert.ts",
      ),
      "@opendownloader/engine": resolve(
        __dirname,
        "../../packages/engine/src/index.ts",
      ),
      "@opendownloader/ui": resolve(
        __dirname,
        "../../packages/ui/src/index.ts",
      ),
    },
  },
  build: {
    // Two entry points: the app, and the sign-in page. Sign-in is a separate document
    // on purpose — see the comment at the top of src/login.ts.
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
        login: resolve(__dirname, "login.html"),
      },
    },
    target: "es2022",
    // The wasm is imported as a URL and fetched at runtime; never inline it as a
    // base64 data URI, which would inflate it by a third and block streaming
    // compilation.
    assetsInlineLimit: 4096,
  },
  server: { port: 5180, strictPort: false },
});
