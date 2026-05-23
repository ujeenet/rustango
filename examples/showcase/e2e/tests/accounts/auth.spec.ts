import { expect, test } from '@playwright/test';

/**
 * Phase 4 — accounts auth flow. Exercises:
 *
 * - `rustango::passwords::{hash, verify}` round-trip via the
 *   register/login pair
 * - `rustango::jwt::{encode, decode}` round-trip via the login →
 *   /me handshake
 * - Bearer-token header parsing on the protected route
 * - `unique` constraint on username (#[rustango(unique)])
 *
 * Each test creates a fresh user with a unique suffix so the shared
 * server suite doesn't trip the unique constraint across reruns.
 */

const tag = () => `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;

async function register(request, suffix: string, password = 'strong-pw-12345') {
  const username = `u-${suffix}`;
  const email = `u-${suffix}@example.com`;
  const resp = await request.post('/accounts/register', {
    data: { username, email, password },
  });
  return { resp, username, email, password };
}

test.describe('accounts auth', () => {
  test('register returns 201 with public profile (no password_hash)', async ({ request }) => {
    const { resp, username, email } = await register(request, tag());
    expect(resp.status()).toBe(201);
    const body = await resp.json();
    expect(body.id).toBeGreaterThan(0);
    expect(body.username).toBe(username);
    expect(body.email).toBe(email);
    expect(body).not.toHaveProperty('password');
    expect(body).not.toHaveProperty('password_hash');
  });

  test('register rejects short passwords', async ({ request }) => {
    const resp = await request.post('/accounts/register', {
      data: { username: `short-${tag()}`, email: `s${tag()}@x.com`, password: 'short' },
    });
    expect(resp.status()).toBe(400);
  });

  test('duplicate username rejected by unique constraint', async ({ request }) => {
    const { username } = await register(request, tag());
    const dup = await request.post('/accounts/register', {
      data: { username, email: `other-${tag()}@example.com`, password: 'another-pw-12345' },
    });
    expect(dup.status()).toBe(409);
  });

  test('login with correct password returns JWT + user', async ({ request }) => {
    const { username, password } = await register(request, tag());

    const resp = await request.post('/accounts/login', {
      data: { username, password },
    });
    expect(resp.status()).toBe(200);
    const body = await resp.json();
    expect(body.token).toMatch(/^eyJ/); // JWT base64-url header prefix
    expect(body.user.username).toBe(username);
  });

  test('login with wrong password returns 401', async ({ request }) => {
    const { username } = await register(request, tag());
    const resp = await request.post('/accounts/login', {
      data: { username, password: 'definitely-wrong' },
    });
    expect(resp.status()).toBe(401);
  });

  test('login with unknown username returns 401', async ({ request }) => {
    const resp = await request.post('/accounts/login', {
      data: { username: `nobody-${tag()}`, password: 'pw-pw-pw-pw' },
    });
    expect(resp.status()).toBe(401);
  });

  test('GET /accounts/me without token is 401', async ({ request }) => {
    const resp = await request.get('/accounts/me');
    expect(resp.status()).toBe(401);
  });

  test('GET /accounts/me with malformed header is 401', async ({ request }) => {
    const resp = await request.get('/accounts/me', {
      headers: { Authorization: 'Token notbearer' },
    });
    expect(resp.status()).toBe(401);
  });

  test('GET /accounts/me with valid Bearer token returns user', async ({ request }) => {
    const { username, password } = await register(request, tag());
    const login = await request.post('/accounts/login', {
      data: { username, password },
    });
    const { token, user } = await login.json();

    const meResp = await request.get('/accounts/me', {
      headers: { Authorization: `Bearer ${token}` },
    });
    expect(meResp.status()).toBe(200);
    const me = await meResp.json();
    expect(me).toEqual(user);
  });

  test('GET /accounts/me with tampered token is 401', async ({ request }) => {
    const { username, password } = await register(request, tag());
    const login = await request.post('/accounts/login', { data: { username, password } });
    const { token } = await login.json();

    // Flip a byte in the middle of the payload.
    const idx = Math.floor(token.length / 2);
    const tampered = token.slice(0, idx) + (token[idx] === 'A' ? 'B' : 'A') + token.slice(idx + 1);

    const resp = await request.get('/accounts/me', {
      headers: { Authorization: `Bearer ${tampered}` },
    });
    expect(resp.status()).toBe(401);
  });
});
