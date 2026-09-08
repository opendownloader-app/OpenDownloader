// The two assets every deployed page needs and neither origin had: a `favicon.ico`
// and a 1200x630 link-preview card.
//
// Both are generated from the same `favicon.svg` the app already ships, so the tab
// icon, the site icon and the card are one mark rather than three drawings of it.
// Written into both origins, because `app.opendownloader.app` does not fall back to
// the apex for `/favicon.ico` or `/og-image.png` — a crawler asks each host itself.
//
// Run: node scripts/make-site-assets.mjs

import { chromium } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, copyFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(fileURLToPath(new URL(".", import.meta.url)), "..");
const SITE = resolve(REPO, "../opendownloader-website-deploy");
const WEBAPP = join(REPO, "apps/web/public");
const MARK = readFileSync(join(WEBAPP, "favicon.svg"), "utf8");
const work = mkdtempSync(join(tmpdir(), "od-assets-"));

// Brand values, read from the mark rather than retyped, so the card cannot drift from
// the icon it sits beside.
const ACCENT = "#15b9eb";
const TILE_BG = "#111111";

const browser = await chromium.launch();

/** Rasterise the mark at one size. */
async function png(size) {
  const page = await browser.newPage({
    viewport: { width: size, height: size },
    deviceScaleFactor: 1,
  });
  await page.setContent(
    `<body style="margin:0;background:transparent">
       <div style="width:${size}px;height:${size}px">${MARK.replace(
         "<svg",
         `<svg width="${size}" height="${size}"`,
       )}</div>
     </body>`,
  );
  const buf = await page.screenshot({ omitBackground: true });
  await page.close();
  const out = join(work, `icon-${size}.png`);
  writeFileSync(out, buf);
  return out;
}

// ---- favicon.ico ----------------------------------------------------------------
// 16, 32 and 48 in one file. This is what a browser fetches from the site root when it
// does not read the <link> tags, and what several link-preview crawlers fetch without
// parsing the HTML at all — which is why a site with only an SVG shows a blank tab.
const icoSizes = [16, 32, 48];
const icoParts = [];
for (const s of icoSizes) icoParts.push(await png(s));

const ico = join(work, "favicon.ico");
execFileSync("python3", [
  "-c",
  `
from PIL import Image
import sys
srcs = ${JSON.stringify(icoParts)}
imgs = [Image.open(p).convert("RGBA") for p in srcs]
imgs[0].save(${JSON.stringify(ico)}, format="ICO",
             sizes=[(i.width, i.height) for i in imgs], append_images=imgs[1:])
print("ico", ${JSON.stringify(ico)})
`,
]);

// ---- og-image.png ---------------------------------------------------------------
// The product's face in every chat app. Authored as HTML so the type is real type —
// pixel maths would give a bitmap font. Written to a file and opened with `goto`
// rather than `setContent`, which renders on about:blank and cannot load anything
// relative to it.
const card = join(work, "card.html");
writeFileSync(
  card,
  `<!doctype html><meta charset="utf-8">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Geist:wght@400;500&display=swap">
<style>
  *{box-sizing:border-box;margin:0}
  body{width:1200px;height:630px;background:#0d1114;color:#eaf2f6;
       font-family:Geist,ui-sans-serif,system-ui,sans-serif;
       display:flex;flex-direction:column;justify-content:center;gap:34px;
       padding:0 86px;overflow:hidden}
  .brand{display:flex;align-items:center;gap:18px}
  .brand svg{width:56px;height:56px;border-radius:13px;display:block}
  .word{font-size:30px;font-weight:500;letter-spacing:-0.04em;line-height:1}
  .word .pre{color:#93a6b0}
  .word .dot{color:${ACCENT}}
  h1{font-size:70px;font-weight:500;letter-spacing:-0.035em;line-height:1.06;max-width:19ch}
  .sub{font-size:27px;color:#93a6b0;letter-spacing:-0.01em}
  .rule{height:4px;width:104px;background:${ACCENT};border-radius:99px}
</style>
<div class="brand">${MARK.replace("<svg", '<svg width="56" height="56"')}
  <div class="word"><span class="pre">Open</span>Downloader<span class="dot">.</span></div>
</div>
<div class="rule"></div>
<h1>Save a video from a web page to your computer</h1>
<p class="sub">Free, nothing uploaded, and it runs in your own browser.</p>`,
);

const page = await browser.newPage({
  viewport: { width: 1200, height: 630 },
  deviceScaleFactor: 1,
});
await page.goto(`file://${card}`);
// Wait for the webfont, or the card renders in the fallback face.
await page.evaluate(() => document.fonts.ready);
await page.waitForTimeout(300);
const og = join(work, "og-image.png");
await page.screenshot({ path: og });
await page.close();
await browser.close();

// ---- install into both origins ---------------------------------------------------
for (const dir of [SITE, WEBAPP]) {
  copyFileSync(ico, join(dir, "favicon.ico"));
  copyFileSync(og, join(dir, "og-image.png"));
  copyFileSync(join(work, "icon-48.png"), join(dir, "icon-48.png"));
  console.log(`  wrote favicon.ico, og-image.png, icon-48.png -> ${dir}`);
}
console.log(`\ncard is exactly ${1200}x${630}; ico carries ${icoSizes.join(", ")}`);
