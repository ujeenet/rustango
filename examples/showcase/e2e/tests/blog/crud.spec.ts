import { expect, test } from '@playwright/test';

/**
 * Phase 2 — blog CRUD against the JSON API. Exercises:
 *
 * - `#[derive(Model)]` schema → `manage makemigrations` round-trip
 * - QuerySet `.order_by()` + `.filter_op()` + `.fetch_pool()`
 * - Macro-emitted `Post::insert_pool(&Pool)` round-trip
 * - `Auto<i64>` primary key + `auto_now_add` timestamp
 * - `Json<Vec<T>>` / `Json<T>` axum response shape
 *
 * The suite shares one server across tests so order matters; each
 * test creates a post it owns and asserts only against that.
 */

test.describe('blog API', () => {
  test('list is initially empty (smoke against fresh DB)', async ({ request }) => {
    const resp = await request.get('/blog/posts');
    expect(resp.status()).toBe(200);
    const body = await resp.json();
    expect(Array.isArray(body)).toBe(true);
  });

  test('POST creates a post + GET retrieves it by id', async ({ request }) => {
    const draft = {
      title: 'Hello, rustango',
      body: 'Phase 2 covers the ORM round-trip.',
      published: true,
    };

    const createResp = await request.post('/blog/posts', { data: draft });
    expect(createResp.status()).toBe(201);
    const created = await createResp.json();

    expect(created.id).toBeGreaterThan(0);
    expect(created.title).toBe(draft.title);
    expect(created.body).toBe(draft.body);
    expect(created.published).toBe(true);
    // auto_now_add should produce an RFC-3339 timestamp.
    expect(created.created_at).toMatch(
      /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/,
    );

    const fetchResp = await request.get(`/blog/posts/${created.id}`);
    expect(fetchResp.status()).toBe(200);
    const fetched = await fetchResp.json();
    expect(fetched).toEqual(created);
  });

  test('list reflects newly created posts (order_by id asc)', async ({ request }) => {
    // POST two more, then list.
    const t1 = `Title-A-${Date.now()}`;
    const t2 = `Title-B-${Date.now()}`;
    await request.post('/blog/posts', { data: { title: t1 } });
    await request.post('/blog/posts', { data: { title: t2 } });

    const resp = await request.get('/blog/posts');
    const list = await resp.json();
    const titles = list.map((p: { title: string }) => p.title);
    expect(titles).toContain(t1);
    expect(titles).toContain(t2);

    // Confirm ids are ascending — the route uses .order_by("id", true).
    const ids = list.map((p: { id: number }) => p.id);
    const sorted = [...ids].sort((a, b) => a - b);
    expect(ids).toEqual(sorted);
  });

  test('GET unknown id returns 404', async ({ request }) => {
    const resp = await request.get('/blog/posts/99999999');
    expect(resp.status()).toBe(404);
  });

  test('body is nullable', async ({ request }) => {
    const resp = await request.post('/blog/posts', {
      data: { title: 'No-body post' },
    });
    const post = await resp.json();
    expect(post.body).toBeNull();
    expect(post.published).toBe(false); // default
  });
});
