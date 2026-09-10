// The two files a crawler reads before anything else.
//
// APP-59 reported both as wrong. What was actually wrong was robots.txt:
// it carried only a wildcard `Allow: /`, while the sibling products name
// the AI crawlers explicitly — several of them treat a wildcard-only
// file as a reason to be cautious, and Google-Extended is a separate
// opt-in from ordinary Search, so a site can rank and still be invisible
// to the thing that summarises it.
//
// No browser needed, so this runs in a second rather than behind a
// build. Run: node e2e/seo.mjs
import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const PUBLIC = join(HERE, "..", "public");
const HOST = "https://opendownloader.app";

// The marketing repo keeps its own copy, and only this one reaches the
// apex — the app root replaced the site root there on 2026-09-09. Fixing
// the copy that is no longer served would look exactly like a fix and
// change nothing, so the two are held equal.
const MIRROR = join(HERE, "..", "..", "..", "..", "opendownloader-website-deploy");

let failures = 0;
const check = (ok, what) => {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${what}`);
  if (!ok) failures += 1;
};

const robots = readFileSync(join(PUBLIC, "robots.txt"), "utf8");
const sitemap = readFileSync(join(PUBLIC, "sitemap.xml"), "utf8");

console.log("robots.txt");
check(/^User-agent: \*\s*$/m.test(robots), "the wildcard block is present");
check(robots.includes(`Sitemap: ${HOST}/sitemap.xml`), "it points at the sitemap, on this host");
for (const bot of ["GPTBot", "OAI-SearchBot", "ClaudeBot", "PerplexityBot",
                   "Google-Extended", "Applebot-Extended", "Bingbot", "CCBot"]) {
  check(new RegExp(`^User-agent: ${bot}\\s*$`, "m").test(robots), `${bot} is named explicitly`);
}
check(!/^Disallow: \/\s*$/m.test(robots), "nothing blanket-disallows the site");

console.log("sitemap.xml");
check(sitemap.startsWith("<?xml"), "starts at the declaration — no BOM, no leading blank line");
const locs = [...sitemap.matchAll(/<loc>([^<]+)<\/loc>/g)].map((m) => m[1]);
check(locs.length > 0, `lists ${locs.length} URL(s)`);
check(locs.every((u) => u.startsWith(`${HOST}/`)),
  "every URL is on the host the file is served from");
check(!locs.some((u) => /\/(account|login|signin)/.test(u)),
  "no sign-in page listed — useless as a search result");
for (const loc of locs) {
  const path = loc.slice(HOST.length);
  const file = path === "/" ? "index.html" : path.replace(/^\//, "");
  check(existsSync(join(PUBLIC, "..", file)) || existsSync(join(PUBLIC, file)),
    `${path} is a page that exists`);
}

console.log("the two copies");
for (const name of ["robots.txt", "sitemap.xml"]) {
  const mirror = join(MIRROR, name);
  if (!existsSync(mirror)) {
    console.log(`  skip  ${name}: no mirror at ${MIRROR}`);
    continue;
  }
  check(readFileSync(mirror, "utf8") === readFileSync(join(PUBLIC, name), "utf8"),
    `${name} matches the marketing repo's copy`);
}

console.log(failures === 0 ? "\nall good" : `\n${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
