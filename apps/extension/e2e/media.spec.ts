// End-to-end: the parts of the product that produce a file other than a copy of
// what the server sent — subtitles merged out of HLS segments, and an MP4's
// audio track lifted out without re-encoding.
//
// Both are asserted against the Rust implementation running natively on the same
// input, served by the test media server. Equality there means the browser
// pipeline and the tested Rust pipeline agree exactly, not merely that something
// of the right shape came out.

import { expect, test } from "./fixtures";

test("a master playlist's alternate audio and subtitle renditions are listed", async ({
  manager,
  serverUrl,
}) => {
  const options = await manager.evaluate(
    async (base) => (globalThis as any).__test.listPlaylistOptions(`${base}/hls/master-alt.m3u8`),
    serverUrl,
  );

  expect(options.variants).toHaveLength(2);
  // Every variant names the groups it belongs to, which is what lets the right
  // audio rendition be picked for the chosen quality.
  expect(options.variants[0].audio_group).toBe("aud");
  expect(options.variants[0].subtitles_group).toBe("subs");

  expect(options.audio).toHaveLength(1);
  expect(options.audio[0].name).toBe("English");
  expect(options.audio[0].url).toContain("/hls/audio-en.m3u8");

  expect(options.subtitles.map((s: { language: string }) => s.language)).toEqual(["en", "de"]);
  // A CLOSED-CAPTIONS entry has no URI and is not a downloadable track; it must
  // not appear as one.
  expect(options.subtitles.every((s: { url: string | null }) => s.url !== null)).toBe(true);
});

test("subtitle segments merge into the document the native merger produces", async ({
  manager,
  serverUrl,
}) => {
  // Two renditions, two different real-world packagings: English carries a
  // per-segment timestamp map with segment-relative cues, German carries one
  // constant map with absolute cues and repeats every boundary cue. Naive
  // concatenation gets each of them wrong in a different way.
  for (const language of ["en", "de"]) {
    const expected = await (await fetch(`${serverUrl}/hls/subs-${language}.expected.srt`)).text();

    const actual = await manager.evaluate(
      async ({ base, lang }) => {
        const options = await (globalThis as any).__test.listPlaylistOptions(
          `${base}/hls/master-alt.m3u8`,
        );
        const rendition = options.subtitles.find(
          (s: { language: string }) => s.language === lang,
        );
        const result = await (globalThis as any).__test.fetchSubtitleRendition(rendition, {
          format: "srt",
          baseName: "stream",
        });
        return { text: result.text, filename: result.filename, cues: result.cues.length };
      },
      { base: serverUrl, lang: language },
    );

    expect(actual.text, `${language} subtitles`).toBe(expected);
    // The language is part of the filename, so two renditions of one stream do
    // not overwrite each other.
    expect(actual.filename).toBe(`stream.${language}.srt`);
  }
});

test("the audio of a progressive MP4 is extracted byte for byte", async ({
  manager,
  serverUrl,
}) => {
  const expected = (await (await fetch(`${serverUrl}/media.mp4.m4a.sha256`)).text()).trim();

  const result = await manager.evaluate(async (base) => {
    // Fetched rather than picked from disk: the file picker is browser UI and
    // cannot be automated, but everything after it — the box walk, the chunk
    // plan, the fragment writing — is the code under test.
    const bytes = await (await fetch(`${base}/media.mp4`)).arrayBuffer();
    const file = new File([bytes], "media.mp4", { type: "video/mp4" });
    const out = await (globalThis as any).__test.extractMp4Audio({ file });
    return { size: out.blob.size, sha256: out.sha256, filename: out.filename };
  }, serverUrl);

  expect(result.sha256).toBe(expected);
  expect(result.filename).toBe("media.m4a");
  expect(result.size).toBeGreaterThan(0);
});

test("local transport-stream segments remux to the same bytes a download would produce", async ({
  manager,
  serverUrl,
}) => {
  // The same digest the streaming path is asserted against, reached from the
  // other direction: loose files on disk rather than segments off the network.
  const expected = (await (await fetch(`${serverUrl}/hls/expected.sha256`)).text()).trim();

  const result = await manager.evaluate(async (base) => {
    const files: File[] = [];
    for (let i = 0; i < 4; i++) {
      const bytes = await (await fetch(`${base}/hls/low/seg${i}.ts`)).arrayBuffer();
      files.push(new File([bytes], `seg${i}.ts`, { type: "video/mp2t" }));
    }
    const out = await (globalThis as any).__test.remuxLocalSegments({ files });
    return { sha256: out.sha256, filename: out.filename, size: out.blob.size };
  }, serverUrl);

  expect(result.sha256).toBe(expected);
  expect(result.filename).toBe("seg0.mp4");
  expect(result.size).toBeGreaterThan(0);
});

test("an audio-only HLS download follows the alternate audio rendition", async ({
  manager,
  serverUrl,
}) => {
  const chosen = await manager.evaluate(async (base) => {
    const options = await (globalThis as any).__test.listPlaylistOptions(
      `${base}/hls/master-alt.m3u8`,
    );
    const rendition = (globalThis as any).__test.audioRenditionFor(options);
    return rendition ? { name: rendition.name, url: rendition.url } : null;
  }, serverUrl);

  // A separate audio playlist is smaller to fetch and needs no video track
  // dropped, so it must win over demuxing the video variant.
  expect(chosen).not.toBeNull();
  expect(chosen!.url).toContain("/hls/audio-en.m3u8");
});
