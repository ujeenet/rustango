// State the ordered steps hand to each other. A file, not a variable: a
// failed step restarts the worker, and the next step must still see it.
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import cfg from '../scripts/config.cjs';

export type Journey = {
  server: string;
  slug: string;
  taken: string;
  adminEmail: string;
  link?: string;
  adminPasswordSet?: boolean;
};

export function startJourney(): Journey {
  const run = Date.now().toString(36);
  const j = { server: `srv-${run}`, slug: `acme-${run}`, taken: `taken-${run}`, adminEmail: `admin@acme-${run}.test` };
  save(j);
  return j;
}

export function journey(): Journey {
  if (!existsSync(cfg.stateFile)) throw new Error('no journey yet: an earlier step did not run');
  return JSON.parse(readFileSync(cfg.stateFile, 'utf8'));
}

export function save(j: Journey): void {
  writeFileSync(cfg.stateFile, JSON.stringify(j, null, 2));
}
