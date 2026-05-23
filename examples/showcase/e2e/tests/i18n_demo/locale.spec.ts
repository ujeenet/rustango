import { expect, test } from '@playwright/test';

/**
 * Phase 5 — `LocaleMiddleware` + URL-prefix locale composition.
 * Exercises the framework's documented pick order:
 *
 *   cookie > Accept-Language > configured default
 *
 * plus the documented `Router::nest("/<lang>", ...)` pattern for
 * URL-prefix locales.
 */

test.describe('locale resolution', () => {
  test('default is en when no header / cookie set', async ({ request }) => {
    const resp = await request.get('/i18n/greeting');
    expect(resp.status()).toBe(200);
    const body = await resp.json();
    expect(body.locale).toBe('en');
    expect(body.greeting).toBe('Hello, world!');
  });

  test('Accept-Language picks among available locales', async ({ request }) => {
    const resp = await request.get('/i18n/greeting', {
      headers: { 'Accept-Language': 'fr-FR,fr;q=0.9,en;q=0.8' },
    });
    const body = await resp.json();
    expect(body.locale).toBe('fr');
    expect(body.greeting).toBe('Bonjour, monde !');
  });

  test('Accept-Language with no match falls back to default', async ({ request }) => {
    const resp = await request.get('/i18n/greeting', {
      headers: { 'Accept-Language': 'ja,zh' },
    });
    const body = await resp.json();
    expect(body.locale).toBe('en');
  });

  test('cookie overrides Accept-Language', async ({ request }) => {
    const resp = await request.get('/i18n/greeting', {
      headers: {
        'Accept-Language': 'fr',
        Cookie: 'django_language=es',
      },
    });
    const body = await resp.json();
    expect(body.locale).toBe('es');
    expect(body.greeting).toBe('¡Hola, mundo!');
  });

  test('unknown cookie value falls through to Accept-Language', async ({ request }) => {
    const resp = await request.get('/i18n/greeting', {
      headers: {
        'Accept-Language': 'fr',
        Cookie: 'django_language=de',
      },
    });
    const body = await resp.json();
    expect(body.locale).toBe('fr');
  });
});

test.describe('URL-prefix locale (Router::nest composition)', () => {
  test('/fr/i18n/greeting renders French regardless of headers', async ({ request }) => {
    const resp = await request.get('/fr/i18n/greeting', {
      headers: { 'Accept-Language': 'en' },
    });
    const body = await resp.json();
    expect(body.locale).toBe('fr');
    expect(body.greeting).toBe('Bonjour, monde !');
  });

  test('/es/i18n/greeting renders Spanish', async ({ request }) => {
    const resp = await request.get('/es/i18n/greeting');
    const body = await resp.json();
    expect(body.locale).toBe('es');
  });

  test('/en/i18n/greeting renders English even with cookie set', async ({ request }) => {
    const resp = await request.get('/en/i18n/greeting', {
      headers: { Cookie: 'django_language=fr' },
    });
    const body = await resp.json();
    expect(body.locale).toBe('en');
  });

  test('unknown locale prefix is a 404 (not silently falling back)', async ({ request }) => {
    const resp = await request.get('/de/i18n/greeting');
    expect(resp.status()).toBe(404);
  });
});
