// Screenshots for the test guide, taken from the built app.
//
// Two sources, because no single one can show everything honestly:
//
//  - The desktop app on 5181 serves the production bundle *and* carries the relay and
//    the torrent bridge, so the site extractions and the magnet wait are real.
//  - The E2E build, served statically, is the same bundle with the blob sink forced —
//    the one difference that lets a download finish without a native save dialog no
//    automation can answer. It is what the smoke suite already runs against.
//
// Usage: node e2e/capture.mjs <output-dir>

import { chromium } from "@playwright/test";
import { createServer } from "node:http";
import { readFileSync, existsSync } from "node:fs";
import { extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const OUT = resolve(process.argv[2] ?? "docs/screenshots");
const HERE = fileURLToPath(new URL(".", import.meta.url));
const DIST = resolve(HERE, "../dist");
const APP = process.env.OD_APP ?? "http://127.0.0.1:5181";
const FIXTURES = process.env.OD_FIXTURES ?? "";
// Which phases to run. The app phases need the desktop build on 5181; the fixture phase
// needs the E2E bundle in dist. They are captured in separate passes.
const ONLY = process.env.OD_ONLY ?? "";

const MIME = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".mjs": "text/javascript",
  ".css": "text/css", ".wasm": "application/wasm", ".json": "application/json", ".svg": "image/svg+xml",
  ".png": "image/png", ".woff2": "font/woff2" };

function serveDist(port) {
  const server = createServer((req, res) => {
    const path = decodeURIComponent(req.url.split("?")[0]);
    let file = join(DIST, path === "/" ? "index.html" : path);
    if (!existsSync(file)) file = join(DIST, "index.html");
    res.writeHead(200, {
      "content-type": MIME[extname(file)] ?? "application/octet-stream",
      "cross-origin-opener-policy": "same-origin",
      "cross-origin-embedder-policy": "require-corp",
    });
    res.end(readFileSync(file));
  });
  return new Promise((ok) => server.listen(port, "127.0.0.1", () => ok(server)));
}

const shots = [];
async function shoot(page, name, target) {
  const el = target ? page.locator(target) : null;
  if (el) await el.scrollIntoViewIfNeeded();
  await page.waitForTimeout(250);
  const file = join(OUT, `${name}.png`);
  if (el) await el.screenshot({ path: file });
  else await page.screenshot({ path: file });
  shots.push(name);
  console.log(`  ${name}`);
}

const browser = await chromium.launch();
const ctx = (scheme) => browser.newContext({
  viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: scheme,
});

// ---- 1. The page at rest, both themes. What a first-time visitor sees. ----
if (ONLY !== "fixtures") for (const [n, scheme] of [["01-at-rest-light", "light"], ["02-at-rest-dark", "dark"]]) {
  const c = await ctx(scheme);
  const page = await c.newPage();
  await page.goto(APP);
  await page.waitForFunction(() => !!document.querySelector("#site-list .site"), null, { timeout: 20000 });
  await shoot(page, n);
  await c.close();
}

// ---- 2. A real Bilibili extraction, including the withheld-rendition note. ----
if (ONLY !== "fixtures") {
  const c = await ctx("light");
  const page = await c.newPage();
  await page.goto(APP);
  await page.waitForTimeout(1500);
  await page.fill("#url", "https://www.bilibili.com/video/BV1GJ411x7h7");
  await page.click("#go");
  await page.waitForFunction(() => {
    const el = document.querySelector("#site-options");
    return el && !el.hidden && el.querySelectorAll(".card").length > 0;
  }, null, { timeout: 60000 });
  await shoot(page, "03-bilibili-choices", "#add");
  await c.close();
}

// ---- 3. The magnet wait: an indeterminate bar and what it says while waiting. ----
if (ONLY !== "fixtures") {
  const c = await ctx("light");
  const page = await c.newPage();
  await page.goto(APP);
  await page.waitForTimeout(1500);
  await page.fill("#url", "magnet:?xt=urn:btih:" + "7f".repeat(20));
  await page.click("#go");
  // Wait for the first staged line rather than a fixed sleep, so the shot is never of
  // an empty status bar on a slow machine.
  await page.waitForFunction(() => /Still looking for peers/.test(
    document.querySelector("#url-status")?.textContent ?? ""), null, { timeout: 30000 });
  await shoot(page, "04-magnet-waiting", "#add");
  await c.close();
}

// ---- 4. The catalogue and the local tools. ----
if (ONLY !== "fixtures") {
  const c = await ctx("light");
  const page = await c.newPage();
  await page.goto(APP);
  await page.waitForFunction(() => !!document.querySelector("#site-list .site"), null, { timeout: 20000 });
  await shoot(page, "07-tools", "#tools");
  await shoot(page, "08-sites", "#sites");
  await c.close();
}

// ---- 5. A download that actually finishes, digest and all. ----
if (FIXTURES) {
  const server = await serveDist(5199);
  const c = await ctx("light");
  const page = await c.newPage();
  await page.goto("http://127.0.0.1:5199/");
  await page.waitForTimeout(1500);
  // Large enough that the transfer is still running when it is photographed. At 4 MB
  // from a loopback server it finished first, and the "downloading" shot came out as a
  // second copy of the finished one — which is the sort of thing only looking catches.
  await page.fill("#url", `${FIXTURES}/fixture.mp4?size=268435456`);
  await page.click("#go");
  // Mid-flight, and provably so: a progress element that is neither empty nor full.
  await page.waitForFunction(() => {
    const bar = document.querySelector("#manager progress");
    return bar && bar.value > bar.max * 0.05 && bar.value < bar.max * 0.85;
  }, null, { timeout: 30000 });
  await shoot(page, "05-downloading", "#manager");
  // Finished: the SHA-256 read back off disk.
  await page.waitForFunction(() => /sha256/.test(document.querySelector("#manager")?.textContent ?? ""),
    null, { timeout: 60000 });
  await shoot(page, "06-verified", "#manager");
  await c.close();
  server.close();
}

await browser.close();
console.log(`\n${shots.length} shots -> ${OUT}`);
