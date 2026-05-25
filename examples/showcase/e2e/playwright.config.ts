import { defineConfig, devices } from '@playwright/test';

/**
 * Playwright config for the rustango showcase E2E suite.
 *
 * `webServer.command` invokes the same `manage` CLI a normal rustango
 * project would: `migrate` first, then `runserver`. The DB URL +
 * backend feature come from env vars set by the CI matrix (or the
 * defaults below for local runs against SQLite).
 *
 * To switch backend locally:
 *   SHOWCASE_BACKEND=postgres DATABASE_URL=postgres://... npx playwright test
 *   SHOWCASE_BACKEND=mysql    DATABASE_URL=mysql://...    npx playwright test
 *   (defaults to sqlite::memory: with no env set)
 */

const BACKEND = process.env.SHOWCASE_BACKEND ?? 'sqlite';
const DATABASE_URL =
  process.env.DATABASE_URL ?? 'sqlite:///tmp/rustango-showcase-e2e.db';
const PORT = process.env.SHOWCASE_PORT ?? '8765';
const BASE_URL = `http://127.0.0.1:${PORT}`;

const CARGO_FEATURE_FLAGS = `--no-default-features --features ${BACKEND}`;

export default defineConfig({
  testDir: './tests',
  fullyParallel: false, // one shared server; tests share state
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? [['github'], ['list']] : 'list',

  use: {
    baseURL: BASE_URL,
    trace: 'on-first-retry',
    screenshot: 'only-on-failure',
  },

  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],

  webServer: {
    // Apply migrations, then boot. The `&&` chain means playwright
    // only considers the URL probe once both succeed. Pre-build the
    // binary in CI to keep boot time under playwright's timeout.
    command: `bash -c "cargo run ${CARGO_FEATURE_FLAGS} --quiet -- migrate && cargo run ${CARGO_FEATURE_FLAGS} --quiet -- runserver"`,
    url: `${BASE_URL}/__showcase__/info`,
    reuseExistingServer: !process.env.CI,
    timeout: 180_000,
    cwd: '..',
    env: {
      DATABASE_URL,
      RUSTANGO_BIND: `127.0.0.1:${PORT}`,
      SHOWCASE_JWT_SECRET:
        process.env.SHOWCASE_JWT_SECRET ?? 'showcase-e2e-jwt-secret-not-for-production',
    },
  },
});
