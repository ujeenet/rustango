// The operator console's tenant-database journey: save a server, create a
// tenant on it, hand over its first admin, rotate, refuse a taken name,
// purge. Steps run in file order and pass state through `journey.ts`.
import { expect, test, type Browser, type Page } from '@playwright/test';
import cfg from '../scripts/config.cjs';
import * as db from './db';
import { journey, save, startJourney } from './journey';

const sqlite = cfg.backend === 'sqlite';
const ADMIN_PASSWORD = 'First-admin-pass-9876!';
const RIGHTS_OK = /Ready: the login has every right this needs\./;
const SUCCEEDED = /Run #\d+ — succeeded/;
const STEPS = sqlite
  ? ['create_database', 'register_org', 'migrate', 'create_admin', 'activate']
  : ['create_database', 'create_login', 'grant', 'check_login', 'register_org', 'migrate', 'create_admin', 'activate'];

test.beforeEach(async ({ page }) => {
  // Delete, rotate and purge ask for confirmation.
  page.on('dialog', (d) => d.accept());
});

async function signIn(page: Page) {
  await page.goto('/login');
  await page.getByLabel('Username').fill(cfg.operator.username);
  await page.getByLabel('Password').fill(cfg.operator.password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('button', { name: 'Sign out' })).toBeVisible();
}

async function expectNoServerPassword(page: Page) {
  if (!sqlite) expect(await page.content()).not.toContain(cfg.server.password);
}

function serverRow(page: Page, name: string) {
  return page.getByRole('row').filter({ hasText: name });
}

/** A server's Test result: a full-width row under the server's row. */
function probeResult(page: Page, name: string) {
  return page.locator(`.probe-result[data-for="${name}"]`);
}

/** The run page has one row per step, at its latest status. */
function stepRow(page: Page, step: string) {
  return page.getByRole('row').filter({ has: page.getByRole('cell', { name: step, exact: true }) });
}

/** A fresh browser on the tenant host, with no console cookies. */
async function tenantPage(browser: Browser) {
  const ctx = await browser.newContext();
  return ctx.newPage();
}

/** Submits the tenant login form. */
async function tenantSignIn(page: Page, slug: string, username: string, password: string) {
  await page.goto(`${cfg.tenantBase(slug)}/login`);
  await page.getByLabel('Username').fill(username);
  await page.getByLabel('Password').fill(password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await page.waitForLoadState();
}

/** Signed in: the tenant admin opens instead of bouncing to the login. */
async function expectTenantAdmin(page: Page, slug: string) {
  await expect(page).not.toHaveURL(/\/login/);
  const admin = await page.goto(`${cfg.tenantBase(slug)}/admin`);
  expect(admin?.status()).toBe(200);
  await expect(page).toHaveURL(new RegExp(`^${cfg.tenantBase(slug)}/admin`));
}

test('operator signs in and no key warning shows', async ({ page }) => {
  startJourney();
  await signIn(page);
  for (const path of ['/db-servers', '/orgs/new']) {
    await page.goto(path);
    await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
    await expect(page.getByText('RUSTANGO_SECRET_KEY')).toHaveCount(0);
  }
});

test('saves a database server and never shows its password', async ({ page }) => {
  const j = journey();
  await signIn(page);
  await page.goto('/db-servers');
  await page.getByLabel(/^name$/i).fill(j.server);
  if (sqlite) {
    await page.getByLabel(/^directory/i).fill(cfg.server.dir);
  } else {
    await page.getByLabel(/^host$/i).fill(cfg.server.host);
    await page.getByLabel(/^port$/i).fill(cfg.server.port);
    await page.getByLabel(/^admin user$/i).fill(cfg.server.user);
    await page.getByLabel(/^admin password$/i).fill(cfg.server.password);
    await page.getByLabel(/^tls$/i).selectOption('disable');
  }
  await page.getByRole('button', { name: /^save server$/i }).click();
  await expect(page.getByText(`saved \`${j.server}\``)).toBeVisible();
  await expectNoServerPassword(page);

  const row = serverRow(page, j.server);
  await row.getByRole('button', { name: /^test$/i }).click();
  await expect(probeResult(page, j.server)).toContainText(RIGHTS_OK);
  await expect(probeResult(page, j.server)).not.toContainText('Not ready');

  // A blank password on edit keeps the saved one.
  await row.getByRole('link', { name: /^edit$/i }).click();
  await expectNoServerPassword(page);
  if (!sqlite) await expect(page.getByText(/saved — leave blank to keep it/i)).toBeVisible();
  await page.getByRole('button', { name: /^save changes$/i }).click();
  await expect(page.getByText(`updated \`${j.server}\``)).toBeVisible();
  await expectNoServerPassword(page);
  await page.goto('/db-servers');
  await serverRow(page, j.server).getByRole('button', { name: /^test$/i }).click();
  await expect(probeResult(page, j.server)).toContainText(RIGHTS_OK);
});

test('creates a tenant on the saved server and shows its link once', async ({ page }) => {
  const j = journey();
  await signIn(page);
  await page.goto('/orgs/new');
  await page.getByLabel(/^slug$/i).fill(j.slug);
  await expect(page.getByLabel(/create a new database on a saved server/i)).toBeChecked();
  await page.getByLabel(/^server$/i).selectOption(j.server);
  await page.getByLabel(/^email$/i).fill(j.adminEmail);
  await page.getByRole('button', { name: /^test connection$/i }).click();
  await expect(page.getByText(RIGHTS_OK)).toBeVisible();

  await page.getByRole('button', { name: /^create tenant$/i }).click();
  await expect(page.getByRole('heading', { level: 1 })).toContainText(j.slug);
  await expect(page.getByText(SUCCEEDED)).toBeVisible();
  for (const step of STEPS) {
    const row = stepRow(page, step);
    await expect(row, `step ${step} has one row`).toHaveCount(1);
    await expect(row.getByRole('cell', { name: /^ok$/i }), `step ${step} is ok`).toBeVisible();
  }
  await expect(page.getByRole('cell', { name: /^failed$/i })).toHaveCount(0);

  const link = await page.getByRole('textbox', { name: /set-password link/i }).inputValue();
  expect(link.startsWith(`${cfg.tenantBase(j.slug)}/`), link).toBe(true);
  await expect(page.getByRole('button', { name: /^copy$/i })).toBeVisible();
  save({ ...j, link });

  // The run page, opened again, does not show the link.
  const runId = /Run #(\d+)/.exec(await page.getByText(SUCCEEDED).innerText())![1];
  await page.goto(`/orgs/provision/${runId}`);
  await expect(page.getByText(SUCCEEDED)).toBeVisible();
  await expect(page.getByRole('textbox', { name: /set-password link/i })).toHaveCount(0);
  expect(await page.content()).not.toContain(link.split('?')[1]);
});

test('first admin sets a password through the link and signs in', async ({ browser }) => {
  const j = journey();
  const page = await tenantPage(browser);
  const opened = await page.goto(j.link!);
  expect(opened?.status()).toBe(200);
  await expect(page.getByText('Set your password for')).toBeVisible();
  await page.getByLabel(/^new password$/i).fill(ADMIN_PASSWORD);
  await page.getByLabel(/^confirm new password$/i).fill(ADMIN_PASSWORD);
  const [posted] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === 'POST'),
    page.getByRole('button', { name: /^set password$/i }).click(),
  ]);
  expect(posted.status(), 'set-password POST').toBe(200);
  await expect(page.getByText('Password set. You can sign in now.')).toBeVisible();
  save({ ...j, adminPasswordSet: true });

  await tenantSignIn(page, j.slug, j.adminEmail, ADMIN_PASSWORD);
  await expectTenantAdmin(page, j.slug);
});

test('a used set-password link is refused', async ({ browser }) => {
  const j = journey();
  expect(j.adminPasswordSet, 'needs the previous step').toBe(true);
  const page = await tenantPage(browser);
  await page.goto(j.link!);
  await expect(page.getByText('This link is invalid, used or expired.')).toBeVisible();
  await expect(page.getByLabel(/^new password$/i)).toHaveCount(0);
});

test('refuses a slug whose database exists and leaves nothing behind', async ({ page }) => {
  const j = journey();
  await db.createDatabase(j.taken);
  try {
    await signIn(page);
    await page.goto('/orgs/new');
    await page.getByLabel(/^slug$/i).fill(j.taken);
    await page.getByLabel(/^server$/i).selectOption(j.server);
    await page.getByRole('button', { name: /^test connection$/i }).click();
    await expect(page.getByText(/already exists/)).toBeVisible();

    await page.getByRole('button', { name: /^create tenant$/i }).click();
    await expect(page.getByRole('heading', { name: /^new tenant$/i })).toBeVisible();
    await expect(page.getByText(/already exists/)).toBeVisible();

    await page.goto('/orgs');
    await expect(page.getByText(j.taken)).toHaveCount(0);
    expect(await db.loginExists(j.taken), 'no login made').toBe(false);
    expect(await db.databaseExists(j.taken), 'the existing database is untouched').toBe(true);
  } finally {
    await db.dropDatabase(j.taken);
  }

  // The first tenant still uses the server, so it cannot be removed.
  await page.goto('/db-servers');
  await serverRow(page, j.server).getByRole('button', { name: /^delete$/i }).click();
  await expect(page.getByText(new RegExp(`may be used by tenant\\(s\\) ${j.slug}`))).toBeVisible();
  await expect(serverRow(page, j.server)).toHaveCount(1);
});

test('rotates the tenant database password and the tenant keeps serving', async ({ page, browser }) => {
  test.skip(sqlite, 'a SQLite tenant has no login to rotate');
  const j = journey();
  await signIn(page);
  await page.goto(`/orgs/${j.slug}/edit`);
  await page.getByRole('button', { name: /^rotate database password$/i }).click();
  await expect(page.getByText(SUCCEEDED)).toBeVisible({ timeout: 60_000 });

  // The tenant's stored URL now carries the new password.
  await page.goto(`/orgs/${j.slug}/edit`);
  await page.getByRole('button', { name: /^test connection$/i }).click();
  await expect(page.getByText(/this role can create tables/i)).toBeVisible();

  // A wrong password is answered from the tenant database, not with an error page.
  const tenant = await tenantPage(browser);
  await tenantSignIn(tenant, j.slug, j.adminEmail, 'not-the-password');
  await expect(tenant).toHaveURL(/error=Invalid/);
  expect(j.adminPasswordSet, 'needs the set-password step').toBe(true);
  await tenantSignIn(tenant, j.slug, j.adminEmail, ADMIN_PASSWORD);
  await expectTenantAdmin(tenant, j.slug);
});

test('purges the tenant, then the server can be removed', async ({ page, browser }) => {
  const j = journey();
  await signIn(page);
  await page.goto(`/orgs/${j.slug}/edit`);
  await page.getByLabel(/^type the slug$/i).fill(j.slug);
  await page.getByLabel(/^drop the database$/i).check();
  await page.getByRole('button', { name: new RegExp(`^purge ${j.slug}$`, 'i') }).click();
  await expect(page.getByText(SUCCEEDED)).toBeVisible({ timeout: 60_000 });

  const tenant = await tenantPage(browser);
  await expect
    .poll(async () => (await tenant.goto(`${cfg.tenantBase(j.slug)}/login`))?.status(), {
      message: 'the purged tenant host answers 404',
      timeout: 30_000,
    })
    .toBe(404);
  expect(await db.databaseExists(j.slug), 'database dropped').toBe(false);
  expect(await db.loginExists(j.slug), 'login dropped').toBe(false);

  await page.goto('/db-servers');
  await serverRow(page, j.server).getByRole('button', { name: /^delete$/i }).click();
  await expect(page.getByText(`removed database server \`${j.server}\``)).toBeVisible();
  await expect(serverRow(page, j.server)).toHaveCount(0);
});
