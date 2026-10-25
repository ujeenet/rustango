// Direct checks against the saved server, from outside the app: what the
// console created or dropped really exists or is really gone.
import { existsSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { Client } from 'pg';
import mysql from 'mysql2/promise';
import cfg from '../scripts/config.cjs';

type Server = { host: string; port: string; user: string; password: string };

async function pg<T>(f: (c: Client) => Promise<T>): Promise<T> {
  const s = cfg.server as Server;
  const c = new Client({
    host: s.host,
    port: Number(s.port),
    user: s.user,
    password: s.password,
    database: 'postgres',
  });
  await c.connect();
  try {
    return await f(c);
  } finally {
    await c.end();
  }
}

async function my<T>(f: (c: mysql.Connection) => Promise<T>): Promise<T> {
  const s = cfg.server as Server;
  const c = await mysql.createConnection({
    host: s.host,
    port: Number(s.port),
    user: s.user,
    password: s.password,
  });
  try {
    return await f(c);
  } finally {
    await c.end();
  }
}

const sqliteFile = (name: string) => path.join((cfg.server as { dir: string }).dir, `${name}.db`);

export async function createDatabase(name: string): Promise<void> {
  if (cfg.backend === 'postgres') await pg((c) => c.query(`CREATE DATABASE "${name}"`));
  else if (cfg.backend === 'mysql') await my((c) => c.query(`CREATE DATABASE \`${name}\``));
  else writeFileSync(sqliteFile(name), '');
}

export async function dropDatabase(name: string): Promise<void> {
  if (cfg.backend === 'postgres') await pg((c) => c.query(`DROP DATABASE IF EXISTS "${name}"`));
  else if (cfg.backend === 'mysql') await my((c) => c.query(`DROP DATABASE IF EXISTS \`${name}\``));
  else rmSync(sqliteFile(name), { force: true });
}

export async function databaseExists(name: string): Promise<boolean> {
  if (cfg.backend === 'postgres') {
    return pg(async (c) => (await c.query('SELECT 1 FROM pg_database WHERE datname = $1', [name])).rowCount === 1);
  }
  if (cfg.backend === 'mysql') {
    return my(async (c) => {
      const [rows] = await c.query('SELECT 1 FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?', [name]);
      return (rows as unknown[]).length === 1;
    });
  }
  return existsSync(sqliteFile(name));
}

/** SQLite has no logins, so never. */
export async function loginExists(name: string): Promise<boolean> {
  if (cfg.backend === 'postgres') {
    return pg(async (c) => (await c.query('SELECT 1 FROM pg_roles WHERE rolname = $1', [name])).rowCount === 1);
  }
  if (cfg.backend === 'mysql') {
    return my(async (c) => {
      const [rows] = await c.query('SELECT 1 FROM mysql.user WHERE User = ?', [name]);
      return (rows as unknown[]).length > 0;
    });
  }
  return false;
}
