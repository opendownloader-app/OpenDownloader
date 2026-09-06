import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  // Each test drives a persistent browser context with its own profile and its
  // own download engine; running them concurrently would have them competing for
  // the same test-server request counters.
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  expect: { timeout: 20_000 },
  reporter: [["list"]],
});
