// End-to-end configuration.
//
// Deliberately not part of `nix flake check`. The checks in `flake.nix` build
// in a sandbox with no network, and a browser engine is a few hundred megabytes
// that `pnpm install` does not fetch — `pnpmConfigHook` installs with
// `--ignore-scripts`, so Playwright's own postinstall never runs there. What
// the flake does get for free is *type-checking* of everything under `e2e/`,
// because `pnpm build` is `tsc -b` and `tsconfig.node.json` includes it. A test
// that does not compile fails the flake; a test that needs Chromium does not
// run there at all.
import {defineConfig, devices} from "@playwright/test";

// Vite's own default, overridable so a run can sit beside a dev server already
// on 5173 instead of fighting it for the port.
const PORT = Number(process.env.E2E_PORT ?? 5173);

// The app is served from a subtree, not from `/` — see `vite.config.ts` for why
// — so the trailing `/ui/` belongs in the base URL rather than in every
// `page.goto`.
const BASE_URL = `http://localhost:${PORT}/ui/`;

export default defineConfig({
  testDir: "./e2e",
  // A failing assertion about a page that polls every five seconds is usually
  // a real failure, not a slow one; keep the default short so a mistake is
  // reported rather than waited on.
  expect: {timeout: 5_000},
  fullyParallel: true,
  // A `.only` left in a file passes locally and silently narrows CI to one
  // test. Refuse it there instead.
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? "github" : "list",

  use: {
    baseURL: BASE_URL,
    trace: "on-first-retry",
  },

  projects: [
    {name: "chromium", use: {...devices["Desktop Chrome"]}},
    {name: "firefox", use: {...devices["Desktop Firefox"]}},
    // A phone, because most of this CSS exists for one: the column cap refuses
    // to apply below 48rem, the cards fold to one column, and the control bar
    // wraps. None of that is exercised by a desktop viewport.
    {name: "mobile", use: {...devices["iPhone 15"]}},
  ],

  webServer: {
    command: `pnpm dev --port ${PORT}`,
    url: BASE_URL,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
