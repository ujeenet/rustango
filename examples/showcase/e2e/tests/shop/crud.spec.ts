import { expect, test } from '@playwright/test';

/**
 * Phase 3 — shop CRUD + query-param filtering. Exercises:
 *
 * - i64 money round-trip (`price_cents`) — until #524 lands, prices
 *   are integer cents
 * - `unique` constraint on `sku`
 * - `Option<i64>` nullable `stock` — null vs. 0 distinction
 * - `default = "true"` rendered through Tera DDL → DB default
 * - Query-param `?active=true|false` driving `.filter_op()`
 *
 * Tests build unique SKUs from `Date.now()` so the shared-server
 * suite doesn't trip the UNIQUE constraint on reruns.
 */

const sku = (prefix: string) => `${prefix}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;

test.describe('shop API', () => {
  test('POST creates a product + retrieve by id', async ({ request }) => {
    const draft = {
      name: 'Widget',
      sku: sku('WID'),
      price_cents: 1999,
      stock: 100,
    };
    const createResp = await request.post('/shop/products', { data: draft });
    expect(createResp.status()).toBe(201);
    const created = await createResp.json();
    expect(created.id).toBeGreaterThan(0);
    expect(created.name).toBe(draft.name);
    expect(created.sku).toBe(draft.sku);
    expect(created.price_cents).toBe(1999);
    expect(created.stock).toBe(100);
    expect(created.active).toBe(true); // default

    const fetchResp = await request.get(`/shop/products/${created.id}`);
    expect(fetchResp.status()).toBe(200);
    expect(await fetchResp.json()).toEqual(created);
  });

  test('null stock is preserved (vs. 0)', async ({ request }) => {
    const draft = {
      name: 'No-stock-tracking',
      sku: sku('NST'),
      price_cents: 500,
      // stock omitted → null
    };
    const resp = await request.post('/shop/products', { data: draft });
    const created = await resp.json();
    expect(created.stock).toBeNull();
  });

  test('?active=true filter excludes inactive products', async ({ request }) => {
    // Seed two: one active, one not.
    const a = await request.post('/shop/products', {
      data: { name: 'OnSale', sku: sku('ON'), price_cents: 100, active: true },
    });
    const b = await request.post('/shop/products', {
      data: { name: 'Discontinued', sku: sku('OFF'), price_cents: 100, active: false },
    });
    const activeId = (await a.json()).id;
    const inactiveId = (await b.json()).id;

    const resp = await request.get('/shop/products?active=true');
    const list = await resp.json();
    const ids = list.map((p: { id: number }) => p.id);
    expect(ids).toContain(activeId);
    expect(ids).not.toContain(inactiveId);
  });

  test('?active=false includes inactive products', async ({ request }) => {
    const b = await request.post('/shop/products', {
      data: { name: 'Discontinued-2', sku: sku('OFF2'), price_cents: 100, active: false },
    });
    const inactiveId = (await b.json()).id;

    const resp = await request.get('/shop/products?active=false');
    const list = await resp.json();
    const ids = list.map((p: { id: number }) => p.id);
    expect(ids).toContain(inactiveId);
  });

  test('duplicate sku rejected by unique constraint', async ({ request }) => {
    const dupSku = sku('DUP');
    const first = await request.post('/shop/products', {
      data: { name: 'First', sku: dupSku, price_cents: 100 },
    });
    expect(first.status()).toBe(201);

    const second = await request.post('/shop/products', {
      data: { name: 'Second', sku: dupSku, price_cents: 200 },
    });
    expect(second.status()).toBe(500); // surfaces as a driver error today
  });

  test('GET unknown product is 404', async ({ request }) => {
    const resp = await request.get('/shop/products/99999999');
    expect(resp.status()).toBe(404);
  });
});
