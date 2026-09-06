// End-to-end: a real Chrome, the real extension, a real HTTP server.
//
// These tests exist to cover what unit tests structurally cannot — that the wasm
// module loads inside an MV3 service worker, that `webRequest` observation
// actually fires, and above all that the bytes which reach disk hash to what the
// server says they should.

import { enqueue, expect, test, waitForStatus } from "./fixtures";

test("the service worker loads the wasm core and detects media on a page", async ({
  context,
  serverUrl,
  extensionId,
}) => {
  // Referencing extensionId forces the worker fixture to resolve first.
  expect(extensionId).toMatch(/^[a-z]{32}$/);

  const page = await context.newPage();
  await page.goto(`${serverUrl}/page.html`);
  // The page fetches the playlist itself; a bare <a href> is never requested and
  // so would never be observable.
  await expect(page).toHaveTitle("ready", { timeout: 20_000 });
  // Reload once so the requests are guaranteed to happen with the extension
  // already listening. A real user's extension is warm long before they browse;
  // a freshly launched automation profile is the only place this ordering is in
  // question, and it is a property of the harness, not of the product.
  await page.reload();
  await expect(page).toHaveTitle("ready", { timeout: 20_000 });

  // Detection is asynchronous and serialised behind a per-tab queue, so the list
  // is not necessarily complete the instant the page finishes loading. Poll
  // rather than sampling once.
  const readCandidates = async () => {
    const worker = context.serviceWorkers()[0]!;
    return worker.evaluate(async () => {
      const all = await chrome.storage.session.get(null);
      return Object.entries(all)
        .filter(([k]) => k.startsWith("candidates:"))
        .flatMap(
          ([, v]) => v as { url: string; kind: string; filename: string }[],
        );
    });
  };

  // Classification ran inside the worker, which means the wasm module
  // initialised there — the thing that cannot be proven outside a browser.
  await expect
    .poll(
      async () => {
        const worker = context.serviceWorkers()[0];
        if (!worker) return "no service worker";
        // Returned as a string so a failure prints what the worker actually has,
        // rather than a bare `false`.
        return worker.evaluate(async () =>
          JSON.stringify(await chrome.storage.session.get(null)),
        );
      },
      { timeout: 20_000 },
    )
    .toContain("/fixture.mp4");

  const found = await readCandidates();
  expect(
    found.some(
      (c) => c.url.includes("/hls/master.m3u8") && c.kind === "hlsplaylist",
    ),
  ).toBe(true);

  // Segments must never be offered individually.
  const candidates = await readCandidates();
  expect(candidates.some((c) => c.url.endsWith(".ts"))).toBe(false);

  await page.close();
});

test("a progressive download verifies against the server's own digest", async ({
  manager,
  serverUrl,
}) => {
  const size = 512 * 1024;
  const expected = await (
    await fetch(`${serverUrl}/fixture.sha256?size=${size}`)
  ).text();

  const id = await enqueue(manager, {
    url: `${serverUrl}/fixture.bin?size=${size}`,
    kind: "progressive",
    filename: "fixture.bin",
    size,
  });

  const job = await waitForStatus(manager, id, ["done", "error"]);

  expect(job.status).toBe("done");
  // The assertion that matters. This is what a wrong finalize/readBack ordering
  // silently broke: it produced the empty-string digest every time.
  expect(job.sha256).toBe(expected);
  expect(job.outputBytes).toBe(size);
});

test("an interrupted download resumes and still verifies", async ({
  manager,
  serverUrl,
}) => {
  const size = 512 * 1024;
  const expected = await (
    await fetch(`${serverUrl}/fixture.sha256?size=${size}`)
  ).text();

  const id = await enqueue(manager, {
    url: `${serverUrl}/fixture.bin?size=${size}`,
    kind: "progressive",
    filename: "resume.bin",
    size,
  });

  // Nothing clicks Start: the queue auto-starts whatever it can open a sink for,
  // which is the behaviour a batch download depends on. Pause as soon as the UI
  // offers it, which is only while a run is in flight.
  // `exact`, because the toolbar's "Pause all" also matches otherwise.
  await manager
    .getByRole("button", { name: "Pause", exact: true })
    .click({ timeout: 20_000 });
  const paused = await waitForStatus(manager, id, ["paused", "done", "error"]);

  // A pause can legitimately land before any chunk finishes — over loopback the whole
  // 512 KB can be in flight at once, and aborting then means nothing was committed and
  // zero is the honest figure. Asserting otherwise made this test race the download:
  // it fails on a fast machine and passes on a slow one, which is the wrong way round.
  // What the test is actually for is the line below it — that a resumed download still
  // verifies — so only the partial case asserts on partial bytes.
  if (paused.status === "paused") {
    expect(paused.receivedBytes).toBeGreaterThanOrEqual(0);
    expect(paused.receivedBytes).toBeLessThan(size);
    await manager.getByRole("button", { name: "Resume", exact: true }).click();
  }

  const job = await waitForStatus(manager, id, ["done", "error"]);
  expect(job.status).toBe("done");
  // A resumed download that spliced its chunks wrongly would still be the right
  // length; only the digest catches it.
  expect(job.sha256).toBe(expected);
});

test("a flaky server is retried rather than failing the job", async ({
  manager,
  serverUrl,
}) => {
  const size = 64 * 1024;
  const expected = await (
    await fetch(`${serverUrl}/fixture.sha256?size=${size}`)
  ).text();

  const id = await enqueue(manager, {
    // The first two requests answer 503 with Retry-After before relenting.
    url: `${serverUrl}/fixture.bin?size=${size}&fail=2`,
    kind: "progressive",
    filename: "flaky.bin",
    size,
  });

  const job = await waitForStatus(manager, id, ["done", "error"]);
  expect(job.status).toBe("done");
  expect(job.sha256).toBe(expected);
});

test("a server without range support still completes", async ({
  manager,
  serverUrl,
}) => {
  const size = 128 * 1024;
  const expected = await (
    await fetch(`${serverUrl}/fixture.sha256?size=${size}`)
  ).text();

  const id = await enqueue(manager, {
    url: `${serverUrl}/fixture.bin?size=${size}&ranges=0`,
    kind: "progressive",
    filename: "noranges.bin",
    size,
  });

  const job = await waitForStatus(manager, id, ["done", "error"]);
  expect(job.status).toBe("done");
  expect(job.sha256).toBe(expected);
});

test("an HLS stream remuxes to bytes identical to the native remuxer's", async ({
  manager,
  serverUrl,
}) => {
  // Computed by running the same Rust remuxer over the same segments natively.
  // Equality here means the browser pipeline and the tested Rust pipeline agree
  // byte for byte, not merely that something MP4-shaped came out.
  const expected = await (
    await fetch(`${serverUrl}/hls/expected.sha256`)
  ).text();

  const id = await enqueue(manager, {
    url: `${serverUrl}/hls/master.m3u8`,
    kind: "hlsplaylist",
    filename: "stream.mp4",
  });

  const job = await waitForStatus(manager, id, ["done", "error"]);

  expect(job.status).toBe("done");
  expect(job.sha256).toBe(expected);
});

test("an encrypted playlist is refused without fetching its key", async ({
  manager,
  serverUrl,
}) => {
  const keyRequests: string[] = [];
  manager.on("request", (r) => {
    if (r.url().includes("key.bin")) keyRequests.push(r.url());
  });

  const id = await enqueue(manager, {
    url: `${serverUrl}/hls/encrypted.m3u8`,
    kind: "hlsplaylist",
    filename: "encrypted.mp4",
  });

  const job = await waitForStatus(manager, id, ["done", "error"]);

  expect(job.status).toBe("error");
  expect(job.error).toContain("encrypted");
  // The refusal must happen before any attempt to obtain a key.
  expect(keyRequests).toHaveLength(0);
});

test("a live stream is refused, since it has no end", async ({
  manager,
  serverUrl,
}) => {
  const id = await enqueue(manager, {
    url: `${serverUrl}/hls/live.m3u8`,
    kind: "hlsplaylist",
    filename: "live.mp4",
  });

  const job = await waitForStatus(manager, id, ["done", "error"]);
  expect(job.status).toBe("error");
  expect(job.error).toContain("live");
});
