// Runs after `vite build`.
//
// Two jobs:
//   1. Vite mirrors an HTML entry's path relative to `root`, so
//      src/popup/popup.html lands at dist/src/popup/popup.html. The manifest
//      references flat paths, so flatten them and drop the empty dist/src tree.
//   2. publicDir copies BOTH manifests into every build. Install the right one
//      as manifest.json and delete the other, so each dist ends up with exactly
//      one correctly-shaped manifest.
import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const extDir = dirname(dirname(fileURLToPath(import.meta.url)));
const targetBrowser = process.env.TARGET_BROWSER === "firefox" ? "firefox" : "chrome";
const distDir = join(extDir, targetBrowser === "firefox" ? "dist-firefox" : "dist");

function flattenHtmlEntry(nestedRelPath, flatName) {
  const nested = join(distDir, nestedRelPath);
  if (!existsSync(nested)) {
    throw new Error(`expected Vite to emit ${nestedRelPath} — did the entry name or path change?`);
  }
  renameSync(nested, join(distDir, flatName));
}

flattenHtmlEntry("src/popup/popup.html", "popup.html");
flattenHtmlEntry("src/manager/manager.html", "manager.html");
rmSync(join(distDir, "src"), { recursive: true, force: true });

const firefoxManifest = join(distDir, "manifest.firefox.json");
if (targetBrowser === "firefox") {
  rmSync(join(distDir, "manifest.json"), { force: true });
  renameSync(firefoxManifest, join(distDir, "manifest.json"));
  console.log("copy-static: installed manifest.firefox.json as manifest.json");
} else {
  rmSync(firefoxManifest, { force: true });
}

// The sniffer only ever sees traffic for origins the user granted, and a grant
// needs `permissions.request` from a real gesture — which automation cannot
// produce, since there is no DOM element for a permission prompt. For E2E only,
// widen host access so the detection path can be exercised. This must never run
// for a build that gets published.
if (process.env.OPENDOWNLOADER_E2E === "1") {
  const manifestPath = join(distDir, "manifest.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  manifest.host_permissions = ["<all_urls>"];
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
  console.log("copy-static: E2E BUILD — host_permissions widened to <all_urls>");
}

console.log(`copy-static: ${targetBrowser} build ready in ${distDir}`);
