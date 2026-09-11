// Ask the live site for every file its own page references, and fail if any is missing.
//
// This exists because of an outage it would have caught. index.html was deployed on its
// own after a rebuild; the new bundle's filename carries a content hash, so the page
// asked for `main-C565pFEo.js` while the server still held `main-Dw4MUvD0.js`. The page
// loaded, the stylesheet loaded, and the one file that runs the app 404'd. Nothing in
// the UI said so — the Download button simply did nothing — and it stayed that way
// until a user reported it.
//
// A hashed filename makes a partial deploy silent by construction: the HTML and the
// assets only agree if they were shipped together. So the check is not "did rsync exit
// 0" but "does the page the server is serving actually resolve".
//
// Run: node scripts/verify-deploy.mjs https://opendownloader.app/ [more urls…]

const urls = process.argv.slice(2);
if (urls.length === 0) {
  console.error("usage: node scripts/verify-deploy.mjs <url> [url…]");
  process.exit(2);
}

/** Everything the document asks the server for before it can run. */
function referenced(html, pageUrl) {
  const out = new Set();
  const patterns = [
    /<script[^>]+src="([^"]+)"/gi,
    /<link[^>]+href="([^"]+)"[^>]*rel="(?:stylesheet|modulepreload|preload)"/gi,
    /<link[^>]+rel="(?:stylesheet|modulepreload|preload)"[^>]*href="([^"]+)"/gi,
  ];
  for (const re of patterns) {
    for (const m of html.matchAll(re)) {
      const href = m[1];
      // Skip data: and cross-origin: a CDN being down is not a deploy that half-landed.
      if (/^(data:|https?:\/\/)/i.test(href)) continue;
      out.add(new URL(href, pageUrl).toString());
    }
  }
  return [...out];
}

let failures = 0;

for (const pageUrl of urls) {
  // Cache-bust the document itself, or a proxy can hand back the previous deploy's HTML
  // and the check passes against a page nobody is being served.
  const bust = `${pageUrl}${pageUrl.includes("?") ? "&" : "?"}_vd=${Date.now()}`;
  const res = await fetch(bust, { redirect: "follow" });
  console.log(`\n${pageUrl}  ${res.status}`);
  if (!res.ok) {
    console.log("  page itself did not load");
    failures++;
    continue;
  }
  const html = await res.text();
  const assets = referenced(html, res.url);
  if (assets.length === 0) console.log("  (no local scripts or stylesheets referenced)");

  for (const asset of assets) {
    // HEAD, because the body is irrelevant and some of these are megabytes.
    let status;
    try {
      status = (await fetch(asset, { method: "HEAD" })).status;
    } catch (e) {
      status = `unreachable: ${e.message}`;
    }
    const ok = status === 200;
    if (!ok) failures++;
    console.log(`  ${ok ? "ok  " : "FAIL"}  ${status}  ${asset.replace(res.url, "")}`);
  }
}

console.log(
  failures === 0
    ? "\nevery referenced file resolves"
    : `\n${failures} reference${failures === 1 ? "" : "s"} did not resolve — the deploy is incomplete`,
);
process.exit(failures === 0 ? 0 : 1);
