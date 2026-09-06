import { chromium } from "playwright";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
const dist = join(process.cwd(), "apps/extension/dist");
const profile = mkdtempSync(join(tmpdir(), "opendl-fetch-"));
const ctx = await chromium.launchPersistentContext(profile, {
  channel: "chromium",
  args: [`--disable-extensions-except=${dist}`, `--load-extension=${dist}`],
  userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
  viewport: { width: 1280, height: 900 },
});
let w = ctx.serviceWorkers()[0] ?? (await ctx.waitForEvent("serviceworker", { timeout: 20000 }));
await w.evaluate(() => true);
const id = new URL(w.url()).host;

// The manager runs the queue; its E2E hook lets us drive it without a save dialog.
const mgr = await ctx.newPage();
await mgr.goto(`chrome-extension://${id}/manager.html`);
await mgr.waitForFunction(() => "__test" in globalThis, undefined, { timeout: 20000 });

for (const [name, url] of [
  ["TikTok",    "https://www.tiktok.com/@ducktokdaily/video/7675775924964445462"],
  ["Douyin",    "https://www.douyin.com/jingxuan?modal_id=7681584405361560878"],
  ["Facebook",  "https://www.facebook.com/reel/1753493462584492"],
  ["Instagram", "https://www.instagram.com/reel/DZ-DRpuDYcr/"],
]) {
  await ctx.serviceWorkers()[0].evaluate(() => chrome.storage.session.clear());
  const page = await ctx.newPage();
  try {
    await page.goto(url, { waitUntil: "domcontentloaded", timeout: 45000 });
    await page.waitForTimeout(2500);
    await page.reload({ waitUntil: "domcontentloaded", timeout: 45000 });
  } catch {}
  let best = null;
  for (let i = 0; i < 10; i++) {
    await page.waitForTimeout(2500);
    await page.evaluate(() => document.querySelector("video")?.play?.()).catch(() => {});
    const found = await ctx.serviceWorkers()[0].evaluate(async () => {
      const all = await chrome.storage.session.get(null);
      return Object.entries(all).filter(([k]) => k.startsWith("candidates:"))
        .flatMap(([, v]) => v).filter(c => String(c.mime).startsWith("video/"));
    });
    if (found.length) {
      best = found.sort((a, b) => (b.size ?? 0) - (a.size ?? 0))[0];
      if ((best.size ?? 0) > 300000) break;
    }
  }
  if (!best) { console.log(`\n[${name}] no video candidate`); await page.close(); continue; }

  // Download it through the real queue, with the real verification pass.
  await mgr.evaluate((c) => globalThis.__test.enqueue({
    url: c.url, kind: "progressive", filename: c.filename, mime: c.mime,
  }), best);
  let job = null;
  for (let i = 0; i < 40; i++) {
    await mgr.waitForTimeout(1500);
    const jobs = await mgr.evaluate(() => globalThis.__test.listJobs());
    job = jobs.find(j => j.filename === best.filename);
    if (job && (job.status === "done" || job.status === "error")) break;
  }
  console.log(`\n[${name}] ${best.filename.slice(0, 46)}  offered ${best.size}B`);
  console.log(`   -> ${job?.status}  ${job?.outputBytes ?? 0} bytes on disk`);
  if (job?.sha256) console.log(`   sha256 ${job.sha256}`);
  if (job?.error) console.log(`   error: ${String(job.error).slice(0, 90)}`);
  await page.close();
}
await ctx.close(); rmSync(profile, { recursive: true, force: true });
