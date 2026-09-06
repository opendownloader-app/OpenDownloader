// Playwright fixtures: a real Chrome with the built extension loaded, and the
// local media server it downloads from.

import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";

import { test as base, chromium, type BrowserContext, type Worker } from "@playwright/test";

const extDir = dirname(dirname(fileURLToPath(import.meta.url)));

/** Prebuilt by `npm run e2e`; overridable for a different target directory. */
const SERVER_BIN =
  process.env.DL_TESTSERVER_BIN ?? "/tmp/opendownloader-target/debug/dl-testserver";

export interface TestFixtures {
  context: BrowserContext;
  extensionId: string;
  serverUrl: string;
  /** The manager tab, already open, with its test hook available. */
  manager: import("@playwright/test").Page;
}

async function startServer(): Promise<{ url: string; proc: ChildProcess }> {
  const proc = spawn(SERVER_BIN, [], { stdio: ["ignore", "pipe", "pipe"] });
  const url = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("test server did not start")), 10_000);
    proc.stdout?.on("data", (chunk: Buffer) => {
      // The server prints "listening on http://127.0.0.1:PORT" once bound, which
      // is how the suite learns the port rather than hardcoding one that might
      // already be taken.
      const match = /listening on (\S+)/.exec(chunk.toString());
      if (match?.[1]) {
        clearTimeout(timer);
        resolve(match[1]);
      }
    });
    proc.on("error", reject);
  });
  return { url, proc };
}

export const test = base.extend<TestFixtures>({
  serverUrl: async ({}, use) => {
    const { url, proc } = await startServer();
    await use(url);
    proc.kill();
  },

  context: async ({}, use) => {
    const profile = mkdtempSync(join(tmpdir(), "opendownloader-e2e-"));
    const dist = join(extDir, "dist");
    const context = await chromium.launchPersistentContext(profile, {
      // The literal string "chromium" is required. Omitting `channel` picks a
      // differently-behaving launch, and `channel: "chrome"` uses real Chrome's
      // old headless mode, which has never supported loading extensions at all.
      channel: "chromium",
      args: [`--disable-extensions-except=${dist}`, `--load-extension=${dist}`],
    });
    await use(context);
    await context.close();
    rmSync(profile, { recursive: true, force: true });
  },

  extensionId: async ({ context }, use) => {
    // The service worker is registered lazily; wait for it rather than assuming
    // it is up the instant the context exists.
    let worker: Worker | undefined = context.serviceWorkers()[0];
    if (!worker) worker = await context.waitForEvent("serviceworker", { timeout: 20_000 });
    // The `serviceworker` event fires when the worker is *created*, which can be
    // before its top-level script has finished evaluating — and the webRequest
    // listener does not exist until it has. Navigating in that window means the
    // page's requests are simply never observed, with no error to show for it.
    // Evaluating in the worker forces it to be running and its script complete.
    await worker.evaluate(() => true);
    const id = new URL(worker.url()).host;
    await use(id);
  },

  manager: async ({ context, extensionId }, use) => {
    const page = await context.newPage();
    await page.goto(`chrome-extension://${extensionId}/manager.html`);
    await page.waitForFunction(() => "__test" in globalThis, undefined, { timeout: 20_000 });
    await page.evaluate(() => (globalThis as any).__test.reset());
    // Each test gets the shipped defaults, not whatever the previous one set.
    await page.evaluate(() =>
      (globalThis as any).__test.updateSettings({
        maxConcurrentJobs: 3,
        connectionsPerJob: 4,
        autoStart: true,
      }),
    );
    await use(page);
    await page.close();
  },
});

export const expect = test.expect;

/** Seed a job the way the popup would, then return its id. */
export async function enqueue(
  page: import("@playwright/test").Page,
  candidate: {
    url: string;
    kind: "progressive" | "hlsplaylist";
    filename: string;
    mime?: string | null;
    size?: number | null;
  },
  options: { audioOnly?: boolean; expectedSha256?: string | null } = {},
): Promise<string> {
  return page.evaluate(
    async ({ c, o }) => {
      const job = await (globalThis as any).__test.enqueue({ mime: null, size: null, ...c }, o);
      return job.id as string;
    },
    { c: candidate, o: options },
  );
}

/**
 * Poll the job store until a job reaches one of `statuses`.
 *
 * An explicit loop rather than `page.waitForFunction`: that helper resolves on
 * the *Promise object* an async predicate returns, which is always truthy, so it
 * completed immediately and yielded null instead of a job. The loop also lets a
 * timeout report the status the job was actually stuck on, which a bare
 * "predicate timed out" does not.
 */
export async function waitForStatus(
  page: import("@playwright/test").Page,
  id: string,
  statuses: string[],
  timeoutMs = 45_000,
): Promise<any> {
  const deadline = Date.now() + timeoutMs;
  let last: any = null;
  for (;;) {
    last = await page.evaluate(async (jobId) => {
      const jobs = await (globalThis as any).__test.listJobs();
      return jobs.find((j: any) => j.id === jobId) ?? null;
    }, id);
    if (last && statuses.includes(last.status)) return last;
    if (Date.now() > deadline) {
      throw new Error(
        `job ${id} never reached [${statuses.join(", ")}]; last status was ` +
          `${last?.status ?? "(job missing)"}${last?.error ? ` — ${last.error}` : ""}`,
      );
    }
    await page.waitForTimeout(200);
  }
}
