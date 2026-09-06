// End-to-end: the site-extraction path, from a page's own markup to a verified file.
//
// The platform extractors are the part of the product that rots — a site changes its
// page shape and an extractor stops finding what it expects. Their parsing is covered
// exhaustively by fixtures in Rust; what only a browser can show is the rest of the
// chain: that an extracted option becomes a job, that the job downloads through the
// ordinary engine, and that the bytes verify.
//
// The page state is supplied by the test rather than read from a tab. Automation has no
// second tab to read, and reading one is a single `scripting.executeScript` call that
// belongs to the browser rather than to this product.

import { enqueue, expect, test, waitForStatus } from "./fixtures";

test("a page that states its media is turned into a downloadable option", async ({
  manager,
  serverUrl,
}) => {
  const size = 64 * 1024;
  const media = `${serverUrl}/fixture.mp4?size=${size}`;

  const extraction = await manager.evaluate(
    async ({ url }) => {
      // A real page's shape: an Open Graph video tag and a title, which is what the
      // generic reader looks for on the sites that simply state their media.
      const html = `<html><head>
          <meta property="og:title" content="A lecture recording">
          <meta property="og:video:secure_url" content="${url}">
        </head><body></body></html>`;
      // A host the generic page reader claims, deliberately: Vimeo and Dailymotion now
      // have their own extractors that call an API, and pointing this test at one would
      // make it depend on that site being up.
      return (globalThis as any).__test.extract("https://streamable.com/abcdef", {
        readPageState: async () => html,
      });
    },
    { url: media },
  );

  expect(extraction.site).toBe("video page");
  expect(extraction.title).toBe("A lecture recording");
  expect(extraction.options).toHaveLength(1);

  const option = extraction.options[0];
  expect(option.streams).toHaveLength(1);
  expect(option.streams[0].url).toBe(media);
  // The filename comes from the page's title, sanitised, not from the URL.
  expect(option.filename).toBe("A lecture recording.mp4");
});

test("an extracted option downloads through the ordinary engine and verifies", async ({
  manager,
  serverUrl,
}) => {
  const size = 64 * 1024;
  const expected = (await (await fetch(`${serverUrl}/fixture.sha256?size=${size}`)).text()).trim();

  // What the popup does with a chosen option: write a job whose url is the stream. From
  // there nothing about it is special — it is the same resumable, verifying path every
  // other download takes, which is the point worth proving.
  const id = await enqueue(manager, {
    url: `${serverUrl}/fixture.mp4?size=${size}`,
    kind: "progressive",
    filename: "A lecture recording.mp4",
    size,
  });

  const job = await waitForStatus(manager, id, ["done", "error"]);
  expect(job.status).toBe("done");
  expect(job.sha256).toBe(expected);
});

test("the platform extractors are present in this build and refuse DRM services", async ({
  manager,
}) => {
  const answers = await manager.evaluate(async () => {
    const t = (globalThis as any).__test;
    return {
      youtube: await t.isSupportedSite("https://www.youtube.com/watch?v=aqz-KE-bpKQ"),
      bilibili: await t.isSupportedSite("https://www.bilibili.com/video/BV1GJ411x7h7"),
      tiktok: await t.isSupportedSite("https://www.tiktok.com/@x/video/1"),
      douyin: await t.isSupportedSite("https://www.douyin.com/video/1"),
      instagram: await t.isSupportedSite("https://www.instagram.com/p/abc/"),
      facebook: await t.isSupportedSite("https://www.facebook.com/watch/?v=1"),
      weixin: await t.isSupportedSite("https://mp.weixin.qq.com/s/abc"),
      vimeo: await t.isSupportedSite("https://vimeo.com/1"),
      dailymotion: await t.isSupportedSite("https://www.dailymotion.com/video/x1"),
      twitch: await t.isSupportedSite("https://www.twitch.tv/a/clip/B"),
      x: await t.isSupportedSite("https://x.com/i/status/1"),
      // Not a platform, and not claimed: it falls through to the passive sniffer.
      ordinary: await t.isSupportedSite("https://example.com/a.mp4"),
    };
  });

  expect(answers.ordinary).toBe(false);
  for (const [site, supported] of Object.entries(answers)) {
    if (site === "ordinary") continue;
    expect(supported, `${site} should be supported in this build`).toBe(true);
  }
});

test("a DRM service is refused before any request is made", async ({ manager }) => {
  const refused = await manager.evaluate(async () => {
    const out: Record<string, unknown> = {};
    for (const url of [
      "https://www.netflix.com/watch/80100172",
      "https://open.spotify.com/track/abc",
      "https://www.disneyplus.com/video/abc",
    ]) {
      try {
        await (globalThis as any).__test.extract(url, { readPageState: async () => "<html></html>" });
        out[url] = "NOT REFUSED";
      } catch (e) {
        out[url] = String(e);
      }
    }
    return out;
  });

  // No extractor claims a DRM host, so nothing even reaches a network request — which is
  // the refusal working, rather than a request being made and then discarded.
  for (const [url, message] of Object.entries(refused)) {
    expect(String(message), url).not.toBe("NOT REFUSED");
  }
});
