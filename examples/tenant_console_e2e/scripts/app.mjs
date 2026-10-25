// Generates the tenant app with the scaffolder, then builds, migrates and
// serves it, so the suite always runs on what `cargo rustango new` emits.
//
//   node scripts/app.mjs prepare   scaffold, build, migrate, create the operator
//   node scripts/app.mjs serve     prepare (unless TENANT_E2E_SKIP_PREPARE=1), then run
import { spawn, spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync, rmSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import cfg from './config.cjs';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');
const bin = path.join(cfg.targetDir, 'debug', cfg.appName);

// The generated project pins `stable`; build with the repo's toolchain instead.
const toolchain = /channel\s*=\s*"([^"]+)"/.exec(
  readFileSync(path.join(repo, 'rust-toolchain.toml'), 'utf8'),
)[1];
const cargoEnv = {
  ...process.env,
  RUSTUP_TOOLCHAIN: toolchain,
  CARGO_TARGET_DIR: cfg.targetDir,
  CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS: 'fallback',
};

function appEnv() {
  if (!cfg.registryUrl) throw new Error('DATABASE_URL (the registry) is required for ' + cfg.backend);
  return {
    ...process.env,
    DATABASE_URL: cfg.registryUrl,
    RUSTANGO_BIND: `127.0.0.1:${cfg.port}`,
    RUSTANGO_APEX_DOMAIN: 'localhost',
    RUSTANGO_SECRET_KEY: cfg.secretKey,
    RUSTANGO_ENV: 'dev',
    RUST_LOG: process.env.RUST_LOG ?? 'info,sqlx=warn',
  };
}

function run(cmd, args, opts) {
  console.log(`[tenant-e2e] ${path.basename(cmd)} ${args.join(' ')}`);
  const r = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(' ')} exited with ${r.status}`);
}

function prepare() {
  run('cargo', ['build', '-p', 'cargo-rustango'], { cwd: repo, env: cargoEnv });
  mkdirSync(cfg.workdir, { recursive: true });
  rmSync(cfg.appDir, { recursive: true, force: true });
  run(
    path.join(cfg.targetDir, 'debug', 'cargo-rustango'),
    ['rustango', 'new', cfg.appName, '--template', 'tenant', '--backend', cfg.backend,
      '--rustango-path', path.join(repo, 'crates', 'rustango')],
    { cwd: cfg.workdir },
  );
  // Seed with the workspace lock so the build reuses tested versions.
  copyFileSync(path.join(repo, 'Cargo.lock'), path.join(cfg.appDir, 'Cargo.lock'));
  run('cargo', ['build', '--no-default-features', '--features', cfg.backend],
    { cwd: cfg.appDir, env: cargoEnv });

  rmSync(cfg.stateFile, { force: true });
  if (cfg.backend === 'sqlite') {
    for (const f of ['registry.db', 'registry.db-wal', 'registry.db-shm']) {
      rmSync(path.join(cfg.workdir, f), { force: true });
    }
    rmSync(cfg.server.dir, { recursive: true, force: true });
    mkdirSync(cfg.server.dir, { recursive: true });
  }
  const env = appEnv();
  run(bin, ['makemigrations'], { cwd: cfg.appDir, env });
  run(bin, ['migrate'], { cwd: cfg.appDir, env });
  const op = spawnSync(
    bin,
    ['create-operator', cfg.operator.username, '--password', cfg.operator.password],
    { cwd: cfg.appDir, env, encoding: 'utf8' },
  );
  const out = `${op.stdout}${op.stderr}`;
  if (op.status !== 0 && !out.includes('already exists')) {
    throw new Error(`create-operator failed:\n${out}`);
  }
}

function serve() {
  if (process.env.TENANT_E2E_SKIP_PREPARE !== '1') prepare();
  const child = spawn(bin, ['runserver'], { cwd: cfg.appDir, env: appEnv(), stdio: 'inherit' });
  for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => child.kill(sig));
  child.on('exit', (code, signal) => process.exit(code ?? (signal ? 0 : 1)));
}

const verb = process.argv[2];
if (verb === 'prepare') prepare();
else if (verb === 'serve') serve();
else {
  console.error('usage: node scripts/app.mjs prepare|serve');
  process.exit(2);
}
