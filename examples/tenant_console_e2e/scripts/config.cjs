// One source of settings for the app script and the specs, read from env.
const os = require('node:os');
const path = require('node:path');

const backend = process.env.TENANT_E2E_BACKEND ?? 'sqlite';
if (!['postgres', 'mysql', 'sqlite'].includes(backend)) {
  throw new Error(`TENANT_E2E_BACKEND must be postgres, mysql or sqlite, got ${backend}`);
}
const workdir = path.resolve(
  process.env.TENANT_E2E_WORKDIR ?? path.join(os.tmpdir(), `rustango-tenant-e2e-${backend}`),
);
const port = process.env.TENANT_E2E_PORT ?? '8766';

// The admin login a saved server uses (PG superuser, MySQL root). The
// password should be distinctive: the specs assert it never reaches a page.
const server =
  backend === 'sqlite'
    ? { dir: path.join(workdir, 'tenants') }
    : {
        host: process.env.TENANT_E2E_DB_HOST ?? '127.0.0.1',
        port: process.env.TENANT_E2E_DB_PORT ?? (backend === 'postgres' ? '5432' : '3306'),
        user: process.env.TENANT_E2E_DB_USER ?? (backend === 'postgres' ? 'rustango' : 'root'),
        password: process.env.TENANT_E2E_DB_PASSWORD ?? '',
      };

module.exports = {
  backend,
  workdir,
  port,
  appName: 'tenant_e2e_app',
  appDir: path.join(workdir, 'tenant_e2e_app'),
  targetDir: path.resolve(process.env.CARGO_TARGET_DIR ?? path.join(workdir, 'target')),
  registryUrl:
    process.env.DATABASE_URL ||
    (backend === 'sqlite' ? `sqlite://${path.join(workdir, 'registry.db')}?mode=rwc` : ''),
  server,
  operator: { username: 'e2e-operator', password: 'E2e-operator-pass-1234!' },
  // Test-only sealing key (48 random bytes, base64).
  secretKey: 'pL0dSx3m8v2QeV7c1nH5tK9yB4wZ6rJ0aF2gU8iO3sD7lM1qX5eC9vT4bN6hY0kR',
  apexBase: `http://localhost:${port}`,
  tenantBase: (slug) => `http://${slug}.localhost:${port}`,
  stateFile: path.join(workdir, 'journey.json'),
};
