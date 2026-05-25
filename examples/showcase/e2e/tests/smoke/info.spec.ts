import { expect, test } from '@playwright/test';

/**
 * Phase 1 smoke test. Confirms the showcase server is reachable +
 * binds to the backend the CI matrix said it would. Subsequent phases
 * add per-app folders alongside `smoke/`.
 */

test.describe('phase 1 smoke', () => {
  test('info endpoint serves framework metadata', async ({ request }) => {
    const resp = await request.get('/__showcase__/info');
    expect(resp.ok()).toBeTruthy();

    const body = await resp.json();
    expect(body.framework).toBe('rustango');
    expect(body.version).toMatch(/^\d+\.\d+\.\d+/);
    expect(['postgres', 'mysql', 'sqlite']).toContain(body.backend);
    expect(Array.isArray(body.apps)).toBeTruthy();
  });

  test('backend matches the SHOWCASE_BACKEND env if set', async ({ request }) => {
    const expected = process.env.SHOWCASE_BACKEND;
    test.skip(!expected, 'no SHOWCASE_BACKEND env — local run');

    const resp = await request.get('/__showcase__/info');
    const body = await resp.json();
    expect(body.backend).toBe(expected);
  });
});
