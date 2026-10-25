import { defineConfig, devices } from '@playwright/test';
import cfg from './scripts/config.cjs';

/**
 * The operator console's tenant-database journey, on an app the scaffolder
 * generates fresh (`scripts/app.mjs`). One backend per run:
 *
 *   TENANT_E2E_BACKEND=sqlite npx playwright test
 *   TENANT_E2E_BACKEND=postgres DATABASE_URL=postgres://rustango:<pw>@127.0.0.1:5432/tenant_e2e \
 *     TENANT_E2E_DB_PASSWORD=<pw> npx playwright test
 *   TENANT_E2E_BACKEND=mysql DATABASE_URL=mysql://root:<pw>@127.0.0.1:3306/tenant_e2e \
 *     TENANT_E2E_DB_PASSWORD=<pw> npx playwright test
 *
 * The steps share one journey, so they run in order on one worker with no
 * retries; state between them lives in `cfg.stateFile`.
 */
export default defineConfig({
  testDir: './tests',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: !!process.env.CI,
  timeout: 120_000,
  expect: { timeout: 15_000 },
  reporter: process.env.CI ? [['github'], ['list'], ['html', { open: 'never' }]] : 'list',

  use: {
    baseURL: cfg.apexBase,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },

  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],

  webServer: {
    command: 'node scripts/app.mjs serve',
    url: `http://127.0.0.1:${cfg.port}/health`,
    reuseExistingServer: !process.env.CI,
    // A cold prepare compiles rustango; CI prepares in an earlier step.
    timeout: 1_800_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
});
