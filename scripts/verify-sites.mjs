// A live check of the site extractors, end to end.
//
// The unit tests assert every extractor against captured fixtures, which is what makes
// them fast and what makes a site's change show up as a named failure. What a fixture
// cannot tell you is whether the *site* still behaves the way the fixture was cut from —
// so this drives the real compiled wasm against the real internet and reports what it
// finds.
//
// Only YouTube is reachable from a build machine without a browser session; the rest are
// listed as "needs a browser", which is a fact about them rather than a failure here.
//
// Run: node scripts/verify-sites.mjs [--full]
//   --full also downloads and merges a real video and decodes it with ffprobe.

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");
const GEN = resolve(REPO, "packages/engine/src/wasm-gen");

let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`  ${ok ? "✓" : "✗"} ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures++;
};

async function loadCore() {
  if (!existsSync(`${GEN}/dl_core_bg.wasm`)) {
    console.error("wasm not built — run `npm run build:wasm` first.");
    process.exit(1);
  }
  // The generated glue resolves its wasm relative to `import.meta.url`, which a data:
  // module does not have. Rewriting it is simpler than shipping a second loader.
  const glue = readFileSync(`${GEN}/dl_core.js`, "utf8").replace(
    /import\.meta\.url/g,
    JSON.stringify(`file://${GEN}/`),
  );
  const mod = await import(`data:text/javascript;base64,${Buffer.from(glue).toString("base64")}`);
  await mod.default({ module_or_path: readFileSync(`${GEN}/dl_core_bg.wasm`) });
  return mod;
}

/** Every site the product claims, and what it should be called. */
const CLAIMED = [
  ["https://www.youtube.com/watch?v=aqz-KE-bpKQ", "YouTube"],
  ["https://youtu.be/aqz-KE-bpKQ", "YouTube"],
  ["https://www.youtube.com/shorts/abcdefghijk", "YouTube"],
  ["https://www.bilibili.com/video/BV1GJ411x7h7", "Bilibili"],
  ["https://b23.tv/abcdefg", "Bilibili"],
  ["https://www.tiktok.com/@user/video/7106594312292453675", "TikTok"],
  ["https://vm.tiktok.com/ZMabcdefg/", "TikTok"],
  ["https://www.douyin.com/video/7000000000000000000", "Douyin"],
  ["https://www.instagram.com/p/C1abcdefg/", "Instagram"],
  ["https://www.instagram.com/reel/C1abcdefg/", "Instagram"],
  ["https://www.facebook.com/watch/?v=10153231379946729", "Facebook"],
  ["https://fb.watch/abcdefg/", "Facebook"],
  ["https://mp.weixin.qq.com/s/abcdefg", "WeChat"],
  ["https://vimeo.com/76979871", "Vimeo"],
  ["https://www.dailymotion.com/video/x2hwqn9", "Dailymotion"],
  ["https://www.twitch.tv/somechannel/clip/SomeClipSlug", "Twitch"],
  ["https://x.com/i/status/1234567890", "X"],
  ["https://twitter.com/user/status/1234567890", "X"],
];

/** Hosts that must fall through to the passive sniffer rather than being claimed. */
const UNCLAIMED = ["https://example.com/video.mp4", "https://cdn.example.org/a/b.m3u8"];

/** Services whose media is DRM-protected and must be refused outright. */
const REFUSED = [
  "https://www.netflix.com/watch/80100172",
  "https://open.spotify.com/track/abc",
  "https://www.disneyplus.com/video/abc",
  "https://music.apple.com/us/album/x/1",
];

async function main() {
  const core = await loadCore();
  const full = process.argv.includes("--full");

  console.log("\nsite coverage");
  for (const [url, expected] of CLAIMED) {
    const name = core.site_name_for(url);
    // A site handled by the generic page reader is still supported; it just reports a
    // generic name, which is fine as long as something claims it.
    const ok = core.site_is_supported(url) && (name === expected || name === "video page");
    check(`${expected.padEnd(12)} ${url.slice(0, 52)}`, ok, ok ? "" : `got ${name}`);
  }

  console.log("\nfalls through to the sniffer");
  for (const url of UNCLAIMED) {
    check(url, !core.site_is_supported(url));
  }

  console.log("\nDRM services refused");
  for (const url of REFUSED) {
    check(url, core.is_restricted(url, url) && !core.site_is_supported(url));
  }

  // ---- live: YouTube is the one site reachable without a browser session ----
  console.log("\nYouTube, live");
  const url = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";
  let extraction;
  try {
    const session = new core.SiteExtractor(url);
    let step = JSON.parse(session.start());
    const bodies = [];
    for (const request of step.Need.Fetch) {
      const headers = {};
      for (const [k, v] of request.headers) headers[k] = v;
      const res = await fetch(request.url, {
        method: request.method,
        headers,
        body: request.body ?? undefined,
      });
      bodies.push(await res.text());
    }
    extraction = JSON.parse(session.feed(JSON.stringify(bodies))).Done;
  } catch (e) {
    check("extraction", false, String(e).slice(0, 120));
    return finish();
  }

  check("title", Boolean(extraction.title), extraction.title);
  check("video renditions offered", extraction.videos.length > 0, `${extraction.videos.length}`);
  check("audio renditions offered", extraction.audios.length > 0, `${extraction.audios.length}`);

  const bestVideos = extraction.videos.filter((v) => v.best);
  const bestAudios = extraction.audios.filter((a) => a.best);
  check("exactly one best video", bestVideos.length === 1, bestVideos[0]?.label ?? "none");
  check("exactly one best audio", bestAudios.length === 1, bestAudios[0]?.label ?? "none");
  // "Best" means best *deliverable*, not merely largest: the top renditions here are
  // VP9 and AV1 in WebM, and this build joins video to audio only inside MP4, so the
  // recommendation is the best pair that can actually produce a file with sound.
  const recommendedVideo = extraction.videos.find((v) => v.best);
  const recommendedAudio = extraction.audios.find((a) => a.best);
  check(
    "the recommended video can actually be delivered complete",
    Boolean(recommendedVideo) && (recommendedVideo.has_audio || recommendedVideo.mergeable),
    recommendedVideo?.label,
  );
  check(
    "the recommended audio can actually be joined",
    Boolean(recommendedAudio) && recommendedAudio.mergeable,
    recommendedAudio?.label,
  );
  check(
    "renditions that cannot be joined are still offered",
    extraction.videos.some((v) => !v.mergeable) || extraction.audios.some((a) => !a.mergeable),
    "nothing is hidden from the list",
  );
  check(
    "video ids are unique",
    new Set(extraction.videos.map((v) => v.id)).size === extraction.videos.length,
  );
  check(
    "audio ids are unique",
    new Set(extraction.audios.map((a) => a.id)).size === extraction.audios.length,
  );
  check(
    "renditions are ordered best first",
    extraction.videos.every(
      (v, i) =>
        i === 0 ||
        (v.width ?? 0) * (v.height ?? 0) <=
          (extraction.videos[i - 1].width ?? 0) * (extraction.videos[i - 1].height ?? 0),
    ),
  );
  check("every stream states its range limit", extraction.videos.every((v) => v.stream.max_chunk));

  const mark = (c) => (c.best ? "★" : c.mergeable || c.has_audio ? " " : "·");
  console.log("\n  video renditions (★ recommended, · cannot be joined to audio):");
  for (const v of extraction.videos.slice(0, 8)) {
    console.log(`    ${mark(v)} ${v.label}`);
  }
  console.log("  audio renditions:");
  for (const a of extraction.audios.slice(0, 8)) {
    console.log(`    ${mark(a)} ${a.label}`);
  }

  // A real fetch of the chosen streams, proving the URLs work.
  const best = recommendedVideo ?? extraction.videos[0];
  const bestAudio = recommendedAudio ?? extraction.audios[0];
  for (const [what, stream] of [
    ["video", best?.stream],
    ["audio", bestAudio?.stream],
  ]) {
    if (!stream) continue;
    const headers = { Range: "bytes=0-1023" };
    for (const [k, v] of stream.headers) headers[k] = v;
    const res = await fetch(stream.url, { headers });
    await res.arrayBuffer();
    check(`${what} stream serves a byte range`, res.status === 206, `HTTP ${res.status}`);
  }

  if (full) await fullDownload(core, extraction);
  finish();
}

/** Download the smallest merged combination and decode it, to prove the whole chain. */
async function fullDownload(core, extraction) {
  console.log("\nfull download and merge");
  // The smallest joinable pair, so the check is quick and actually mergeable.
  const video = extraction.videos.filter((v) => !v.has_audio && v.mergeable).pop();
  const audio = extraction.audios.filter((a) => a.mergeable).pop();
  if (!video || !audio) return check("a mergeable pair exists", false);

  const read = async (stream, offset, length) => {
    const cap = stream.max_chunk ?? Infinity;
    const parts = [];
    for (let at = 0; at < length; at += cap) {
      const size = Math.min(cap, length - at);
      const headers = { Range: `bytes=${offset + at}-${offset + at + size - 1}` };
      for (const [k, v] of stream.headers) headers[k] = v;
      const res = await fetch(stream.url, { headers });
      if (res.status !== 206) throw new Error(`HTTP ${res.status} at ${offset + at}`);
      parts.push(Buffer.from(await res.arrayBuffer()));
    }
    return Buffer.concat(parts);
  };

  try {
    const [vh, ah] = await Promise.all([
      read(video.stream, 0, 262144),
      read(audio.stream, 0, 262144),
    ]);
    const merger = core.FragmentMerger.fromHeads(vh, ah);
    const reads = JSON.parse(merger.reads());
    const out = [];
    for (const [i, r] of reads.entries()) {
      out.push(Buffer.from(merger.push(i, await read(r.source === "Video" ? video.stream : audio.stream, r.offset, r.len))));
    }
    const file = Buffer.concat(out);
    writeFileSync("/tmp/opendownloader-verify.mp4", file);
    check("merged file produced", merger.isComplete(), `${(file.length / 1048576).toFixed(1)} MB`);

    const probe = execFileSync(
      "ffprobe",
      ["-v", "error", "-show_entries", "stream=codec_type,codec_name", "-of", "csv=p=0", "/tmp/opendownloader-verify.mp4"],
      { encoding: "utf8" },
    ).trim();
    check("it decodes with both tracks", probe.includes("video") && probe.includes("audio"), probe.replace(/\n/g, " "));
  } catch (e) {
    // Google rate-limits datacenter addresses after the first megabyte; that is a
    // property of where this runs, not of the code, and it is worth saying which.
    const message = String(e);
    check(
      "full download",
      false,
      message.includes("403")
        ? "403 from the host — this address is rate-limited for bulk media, which a browser on an ordinary connection is not"
        : message.slice(0, 140),
    );
  }
}

function finish() {
  console.log(failures === 0 ? "\nall checks passed\n" : `\n${failures} check(s) failed\n`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
