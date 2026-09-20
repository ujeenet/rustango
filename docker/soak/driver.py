#!/usr/bin/env python3
"""Load driver and assertion harness for the commerce soak.

Two jobs, kept separate on purpose:

  * **assertions** — one per behaviour change since 0.57.0. Each is a
    named check with a verdict of PASS, FAIL, or NOT-COVERED. A check
    that cannot run reports NOT-COVERED with a reason; it never reports
    PASS. "The request didn't error" is not a pass.

  * **load** — sustained concurrent traffic across every instance, to
    put the assertions under contention rather than running them on an
    idle server.

Four verdicts, and the difference between them is the point:

  * **PASS** — the behaviour was exercised and is correct.
  * **FAIL** — exercised and wrong. Fails the run.
  * **NOT-COVERED** — this harness cannot decide the question, with a
    stated reason. Never awarded to something that merely passed.
  * **KNOWN-GAP** — exercised, wrong, filed, and shipped knowingly.
    Carries its issue number, is printed separately on every run, and is
    never counted as a pass. Use it only for a defect with an issue and
    a decision behind it, never to quiet a failure.

The report is written to /results/report.json and printed. The exit code
is non-zero if any assertion FAILED; NOT-COVERED and KNOWN-GAP do not
fail the run, because a stated gap is not a regression.
"""

from __future__ import annotations

import asyncio
import json
import os
import random
import sys
import time
from dataclasses import dataclass, field, asdict

import httpx

APEX = os.environ.get("SOAK_APEX", "soak.localhost")
DURATION = int(os.environ.get("SOAK_DURATION_SECS", "1800"))
CONCURRENCY = int(os.environ.get("SOAK_CONCURRENCY", "24"))
TENANTS = int(os.environ.get("SOAK_TENANTS", "20"))
FAIL_RATIO = int(os.environ.get("SOAK_FAIL_RATIO_PCT", "10"))
RESULTS = os.environ.get("SOAK_RESULTS", "/results")

# A nonce for every value that lands in a `unique` column.
#
# The load seed below is fixed on purpose — the same run shape every
# time. But `sku` and `email` are unique, and a fixed seed means run two
# generates run one's values: against a database that persists between
# runs, nearly every insert then collides and comes back 400. Four
# consecutive runs against one Postgres volume showed 80% of customer
# POSTs and 29% of product POSTs rejected, which reads like a server
# fault and is not one.
#
# So: deterministic *choices*, unique *values*. Override to replay a
# specific run's data.
RUN_ID = os.environ.get("SOAK_RUN_ID") or f"{time.time_ns():x}"

# name -> base URL. The SaaS entries are reached with a Host override.
SINGLE = {
    "single-pg": "http://web-single-pg:8080",
    "single-my": "http://web-single-my:8080",
    "single-sq": "http://web-single-sq:8080",
}
SAAS = {
    "saas-pg": "http://web-saas-pg:8080",
    "saas-my": "http://web-saas-my:8080",
    "saas-sq": "http://web-saas-sq:8080",
}


# ----------------------------------------------------------------- report


@dataclass
class Check:
    name: str
    issue: str
    verdict: str  # PASS | FAIL | NOT-COVERED | KNOWN-GAP
    detail: str = ""
    instance: str = ""


@dataclass
class Report:
    checks: list[Check] = field(default_factory=list)
    requests: dict = field(default_factory=dict)
    jobs: dict = field(default_factory=dict)
    started: float = field(default_factory=time.time)

    def add(self, name, issue, verdict, detail="", instance=""):
        self.checks.append(Check(name, issue, verdict, detail, instance))
        mark = {"PASS": "ok  ", "FAIL": "FAIL",
                "NOT-COVERED": "----", "KNOWN-GAP": "KNOWN"}[verdict]
        where = f" [{instance}]" if instance else ""
        print(f"  {mark} {issue:>6}  {name}{where}"
              + (f"\n           {detail}" if detail else ""))

    def failed(self):
        return [c for c in self.checks if c.verdict == "FAIL"]

    def known_gaps(self):
        return [c for c in self.checks if c.verdict == "KNOWN-GAP"]


REPORT = Report()


def tenant_host(i: int) -> str:
    return f"t{i:02d}.{APEX}"


# ----------------------------------------------------------- health / info


async def wait_ready(client: httpx.AsyncClient, name: str, base: str,
                     headers: dict | None = None, timeout: int = 180) -> bool:
    """Poll until an instance answers.

    Single-tenant only. Never use this shape against a *tenant* host: the
    resolver caches negative results for 30s, so an early probe poisons
    the cache and makes a working tenant look broken for a minute.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = await client.get(f"{base}/_soak/info", headers=headers, timeout=5)
            if r.status_code == 200:
                return True
        except Exception:
            pass
        await asyncio.sleep(2)
    return False


# ---------------------------------------------------------------- checks


async def check_null_fk(client, name, base, headers=None):
    """#1450 — a nullable bigint FK.

    Postgres-only as a *test*: the MySQL and SQLite binders were
    deliberately left unchanged, so those two are controls. A tri-dialect
    run that only asserts "no error" does not distinguish the fix, which
    is why the verdict below says so explicitly.
    """
    issue = "#1450"
    prefix = f"{base}/api/v1/orders-raw"

    cust = await client.post(f"{base}/api/v1/customers", headers=headers, json={
        "contact_email": f"null-fk-{time.time_ns()}@example.test",
        "full_name": "Null FK Probe", "loyalty_tier": "bronze",
    })
    if cust.status_code not in (200, 201):
        REPORT.add("nullable FK insert", issue, "NOT-COVERED",
                   f"could not create a customer: {cust.status_code} {cust.text[:160]}", name)
        return
    cid = cust.json().get("id")

    # INSERT with the nullable bigint absent — the original failure.
    body = {
        "reference": f"NULLFK-{time.time_ns()}",
        "customer_id": cid,
        "status": "pending",
        "total_cents": 1234,
        "note": None,
    }
    r = await client.post(prefix, headers=headers, json=body)
    if r.status_code not in (200, 201):
        REPORT.add("INSERT leaving a nullable bigint FK unset", issue, "FAIL",
                   f"{r.status_code} {r.text[:300]}", name)
        return
    oid = r.json().get("id")
    REPORT.add("INSERT leaving a nullable bigint FK unset", issue, "PASS",
               "" if "pg" in name else "control: this dialect's binder was never changed", name)

    # UPDATE binding NULL explicitly — a separate call site.
    r2 = await client.patch(f"{prefix}/{oid}", headers=headers,
                            json={"assigned_picker_id": None})
    if r2.status_code not in (200, 204):
        REPORT.add("PATCH binding a nullable bigint FK to null", issue, "FAIL",
                   f"{r2.status_code} {r2.text[:300]}", name)
    else:
        REPORT.add("PATCH binding a nullable bigint FK to null", issue, "PASS",
                   "" if "pg" in name else "control", name)


async def check_serializer_carries_fk(client, name, base, headers=None):
    """#1454 — a serializer can declare a foreign-key field.

    It could not until 0.57.5. `ForeignKey<T, K>` implemented neither
    `Deserialize`, `Default` nor `OpenApiSchema`, and a serializer field
    must match its model field's type exactly — so there was no spelling
    that compiled. The consequence was not cosmetic: the nullable-FK
    write path (#1450) could only be reached through a serializer-less
    ViewSet, which is why this soak has an `orders-raw` route at all.

    `/api/v1/orders` is the serializer-backed twin, and the FK is
    published as `picker_id` over the column `assigned_picker_id` — a
    rename *on* the nullable FK, where #1386 and #1450 meet.
    """
    issue = "#1454"
    cust = await client.post(f"{base}/api/v1/customers", headers=headers, json={
        "contact_email": f"ser-fk-{time.time_ns()}@example.test",
        "full_name": "Serializer FK Probe", "loyalty_tier": "bronze",
    })
    if cust.status_code not in (200, 201):
        REPORT.add("serializer carries a foreign key", issue, "NOT-COVERED",
                   f"could not create a customer: {cust.status_code}", name)
        return
    cid = cust.json().get("id")

    r = await client.post(f"{base}/api/v1/orders", headers=headers, json={
        "ref_code": f"SERFK-{time.time_ns()}",
        "customer_id": cid,
        "picker_id": None,
        "note": None,
        "status": "pending",
        "total_cents": 4321,
    })
    if r.status_code not in (200, 201):
        REPORT.add("serializer carries a foreign key", issue, "FAIL",
                   f"POST through the serializer gave {r.status_code}: {r.text[:300]}", name)
        return
    created = r.json()
    oid = created.get("id")

    # The published name must be there and the column name must not —
    # otherwise the rename leaked, which is the #1386 half.
    if "picker_id" not in created:
        REPORT.add("serializer carries a foreign key", issue, "FAIL",
                   f"created row has no `picker_id`: {json.dumps(created)[:300]}", name)
        return
    if "assigned_picker_id" in created:
        REPORT.add("serializer carries a foreign key", issue, "FAIL",
                   "the response leaks the column name `assigned_picker_id`", name)
        return
    if created.get("customer_id") != cid:
        REPORT.add("serializer carries a foreign key", issue, "FAIL",
                   f"customer_id did not round-trip: sent {cid}, got "
                   f"{created.get('customer_id')}", name)
        return
    REPORT.add("serializer carries a foreign key", issue, "PASS",
               "non-null FK and nullable FK both declared and round-tripped", name)

    # And through the UPDATE binder, a separate call site from INSERT.
    r2 = await client.patch(f"{base}/api/v1/orders/{oid}", headers=headers,
                            json={"picker_id": None})
    if r2.status_code in (200, 204):
        REPORT.add("serializer PATCHes a nullable FK to null", issue, "PASS", "", name)
    else:
        REPORT.add("serializer PATCHes a nullable FK to null", issue, "FAIL",
                   f"{r2.status_code}: {r2.text[:300]}", name)


async def check_bulk_atomic(client, name, base, headers=None):
    """#1403 — bulk create is atomic in the writes, not just validation."""
    issue = "#1403"
    tag = time.time_ns()

    # A batch whose 5th element repeats the 1st SKU. unique(sku) is a
    # property of the table, so validation cannot catch it up front —
    # that is the class the old loop handled worst.
    items = [{"sku": f"BULK-{tag}-{i}", "name": f"Bulk {i}",
              "blurb": None, "price_cents": 100 + i, "active": True}
             for i in range(10)]
    items[5]["sku"] = items[0]["sku"]

    r = await client.post(f"{base}/api/v1/products", headers=headers, json=items)
    if r.status_code < 400:
        REPORT.add("bulk create rejects a constraint-violating batch", issue, "FAIL",
                   f"expected 4xx, got {r.status_code}", name)
        return

    # The assertion that matters: zero rows committed.
    check = await client.get(f"{base}/api/v1/products", headers=headers,
                             params={"sku": items[0]["sku"]})
    n = 0
    if check.status_code == 200:
        body = check.json()
        rows = body.get("results", body) if isinstance(body, dict) else body
        n = len(rows) if isinstance(rows, list) else 0
    if n == 0:
        REPORT.add("bulk create leaves zero rows on rollback", issue, "PASS",
                   f"rejected with {r.status_code}, 0 rows committed", name)
    else:
        REPORT.add("bulk create leaves zero rows on rollback", issue, "FAIL",
                   f"rejected with {r.status_code} but {n} row(s) committed", name)


async def check_serializer_source(client, name, base, headers=None):
    """#1386 — a `source` rename must not leak the column name."""
    issue = "#1386"
    # `blurb` is published for the column `description`.
    r = await client.post(f"{base}/api/v1/products", headers=headers, json={
        "sku": f"SRC-{time.time_ns()}", "name": "Source Rename Probe",
        "price_cents": "not-an-integer", "active": True,
    })
    if r.status_code < 400:
        REPORT.add("serializer rename does not leak the column", issue, "NOT-COVERED",
                   "expected a validation error and got none", name)
        return
    text = r.text
    if "description" in text:
        REPORT.add("serializer rename does not leak the column", issue, "FAIL",
                   f"error names the column `description`: {text[:200]}", name)
    else:
        REPORT.add("serializer rename does not leak the column", issue, "PASS",
                   "", name)


async def check_pagination(client, name, base, headers=None):
    """Both paginators answer: limit/offset on products, cursor on orders."""
    lo = await client.get(f"{base}/api/v1/products", headers=headers,
                          params={"limit": 5, "offset": 0})
    cur = await client.get(f"{base}/api/v1/orders", headers=headers,
                           params={"page_size": 5})
    ok = lo.status_code == 200 and cur.status_code == 200
    REPORT.add("both pagination styles answer", "—", "PASS" if ok else "FAIL",
               "" if ok else f"limit/offset {lo.status_code}, cursor {cur.status_code}", name)


async def check_health_endpoints(client, name, base, headers=None):
    """#1457 — `/health` and `/ready` on **every** backend.

    They were mounted only in the `#[cfg(feature = "postgres")]` arm of
    `runserver`, so on a SQLite or MySQL build `Cli::with_health()` set
    its flag, the server started, and both endpoints answered 404. A
    container HEALTHCHECK aimed at /health then reported the service
    permanently unhealthy with nothing logged to say why.

    This is the check the fleet exists for: the same assertion against
    six instances, of which four are not Postgres. Run it against the
    apex rather than a tenant host — health is the process's, not a
    tenant's, and a load balancer probes it without a Host override.
    """
    issue = "#1457"
    for path in ("/health", "/ready"):
        try:
            r = await client.get(f"{base}{path}", headers=headers, timeout=10)
        except Exception as e:                       # noqa: BLE001
            REPORT.add(f"{path} answers", issue, "FAIL", f"request failed: {e}", name)
            continue
        if r.status_code == 200:
            REPORT.add(f"{path} answers", issue, "PASS", "", name)
        else:
            REPORT.add(f"{path} answers", issue, "FAIL",
                       f"{r.status_code} — with_health() is a no-op on this build",
                       name)


async def check_cursor_walks(client, name, base, headers=None):
    """#1459 — a timestamp cursor must serve pages and advance.

    `cursor_pagination_desc("placed_at")` was accepted at build time and
    then returned 500 on *every* request: cursors only handled integer
    columns. The ViewSet built, the process started, health checks
    passed, and the endpoint was dead.

    Status 200 alone is too weak a bar. A token that decoded to the
    wrong type would still be a string in the response while paging
    forever over page one, so this follows `next` and requires the
    second page to hold different rows.
    """
    issue = "#1459"
    first = await client.get(f"{base}/api/v1/orders", headers=headers,
                             params={"page_size": 5})
    if first.status_code != 200:
        REPORT.add("timestamp cursor serves a page", issue, "FAIL",
                   f"{first.status_code}: {first.text[:200]}", name)
        return
    body = first.json()
    ids_1 = [r.get("id") for r in body.get("results", [])]
    token = body.get("next")
    REPORT.add("timestamp cursor serves a page", issue, "PASS",
               f"{len(ids_1)} rows", name)

    if not ids_1:
        REPORT.add("timestamp cursor advances", issue, "NOT-COVERED",
                   "no orders yet — nothing to page through", name)
        return
    if not token:
        # Fewer rows than a page: correct, and not evidence either way.
        REPORT.add("timestamp cursor advances", issue, "NOT-COVERED",
                   f"only {len(ids_1)} order(s); no second page to walk to", name)
        return

    # `next` is a full URL in some configurations and a bare token in
    # others; accept both rather than guessing.
    url = token if token.startswith("http") else f"{base}/api/v1/orders?cursor={token}"
    second = await client.get(url, headers=headers)
    if second.status_code != 200:
        REPORT.add("timestamp cursor advances", issue, "FAIL",
                   f"following `next` gave {second.status_code}: {second.text[:200]}", name)
        return
    ids_2 = [r.get("id") for r in second.json().get("results", [])]
    if not ids_2:
        REPORT.add("timestamp cursor advances", issue, "FAIL",
                   "`next` was offered but its page is empty", name)
    elif set(ids_1) & set(ids_2):
        overlap = sorted(set(ids_1) & set(ids_2))
        # SQLite used to be excused here as a KNOWN-GAP for #1464: its
        # `auto_now_add` columns were written by `DEFAULT
        # CURRENT_TIMESTAMP` as "YYYY-MM-DD HH:MM:SS" while sqlx bound
        # RFC3339, so the cursor predicate matched every row and page
        # two was page one.
        #
        # #1464 is fixed, and the excuse goes with it. Leaving it would
        # make this leg unable to fail — which would also make it unable
        # to prove anything, and a regression would read as "known gap"
        # forever.
        REPORT.add("timestamp cursor advances", issue, "FAIL",
                   f"page two repeats rows from page one: {overlap}", name)
    else:
        REPORT.add("timestamp cursor advances", issue, "PASS",
                   f"{len(ids_1)} then {len(ids_2)} distinct rows", name)


async def check_malformed_cursor_is_400(client, name, base, headers=None):
    """A bad cursor is the caller's fault: 400, not 500."""
    r = await client.get(f"{base}/api/v1/orders", headers=headers,
                         params={"cursor": "not-a-real-token"})
    if r.status_code == 400:
        REPORT.add("malformed cursor is a client error", "#1459", "PASS", "", name)
    else:
        REPORT.add("malformed cursor is a client error", "#1459", "FAIL",
                   f"expected 400, got {r.status_code}", name)


async def check_page_cache(client, name, base, headers=None):
    """The storefront is actually cached, and only for its own tenant.

    Two separate claims, and the second is the dangerous one.

    The app carried the `cache-redis` feature and the fleet ran a Redis
    container for several commits while nothing was cached at all — the
    only evidence was a doc comment saying otherwise. So: assert a real
    HIT, not the presence of a header.

    And `CachePageLayer` keys on `(method, path, vary-on headers)`.
    Under tenancy every storefront is the *same path*, so without
    `vary_on(["host"])` the first tenant to warm the cache serves its
    catalogue to every other one. That is a cross-tenant data leak
    caused by a caching layer working exactly as documented, which is
    why it gets its own check rather than riding along on this one.
    """
    r1 = await client.get(f"{base}/shop/products", headers=headers)
    if r1.status_code != 200:
        REPORT.add("storefront is page-cached", "—", "NOT-COVERED",
                   f"storefront answered {r1.status_code}", name)
        return
    r2 = await client.get(f"{base}/shop/products", headers=headers)
    status = r2.headers.get("x-cache-status", "<absent>")
    if status.upper() == "HIT":
        REPORT.add("storefront is page-cached", "—", "PASS",
                   "second request served from Redis", name)
    else:
        REPORT.add("storefront is page-cached", "—", "FAIL",
                   f"second request was `x-cache-status: {status}` — the cache "
                   f"is wired to nothing", name)


async def check_page_cache_is_per_tenant(client, name, base):
    """A cached page must not cross tenants (`vary_on(["host"])`)."""
    a, b = tenant_host(1), tenant_host(2)
    # Warm t01 twice so the entry is certainly stored, then ask as t02.
    await client.get(f"{base}/shop/products", headers={"Host": a})
    await client.get(f"{base}/shop/products", headers={"Host": a})
    r = await client.get(f"{base}/shop/products", headers={"Host": b})
    if r.status_code != 200:
        REPORT.add("page cache does not cross tenants", "—", "NOT-COVERED",
                   f"t02 storefront answered {r.status_code}", name)
        return
    body = r.text
    # The page renders `Catalogue — <slug>`, so the leak is legible.
    if "t01" in body:
        REPORT.add("page cache does not cross tenants", "—", "FAIL",
                   "t02 was served t01's cached storefront — the page cache "
                   "is not keyed on Host", name)
    elif "t02" in body:
        REPORT.add("page cache does not cross tenants", "—", "PASS", "", name)
    else:
        REPORT.add("page cache does not cross tenants", "—", "NOT-COVERED",
                   f"could not identify the tenant in the response: {body[:160]}", name)


async def check_page_cache_does_not_cross_apps(client, live_single, live_saas):
    """Two applications sharing one Redis must not share cache entries.

    `vary_on(["host"])` separates tenants; it does not separate
    *deployments*. Both apps used the literal prefix `commerce.storefront`
    and `/shop/products` is the same path on all six instances, so with
    one Redis they collided on Host alone — the single-tenant catalogue
    came back from the multi-tenant instance under a tenant hostname.

    Found by reading the page, not by an error: the response was a
    well-formed 200 with the wrong body. The single-tenant storefront
    renders a bare `Catalogue`, the multi-tenant one `Catalogue — <slug>`,
    which is what makes the two distinguishable here at all.
    """
    if not live_single or not live_saas:
        REPORT.add("page cache does not cross applications", "—", "NOT-COVERED",
                   "needs one instance of each app up", "")
        return
    host = tenant_host(1)

    # Warm the single-tenant instance under a tenant hostname — the
    # collision needs the same Host on both.
    single_name, single_base = next(iter(live_single.items()))
    saas_name, saas_base = next(iter(live_saas.items()))
    await client.get(f"{single_base}/shop/products", headers={"Host": host})
    await client.get(f"{single_base}/shop/products", headers={"Host": host})

    r = await client.get(f"{saas_base}/shop/products", headers={"Host": host})
    body = r.text
    if r.status_code != 200:
        REPORT.add("page cache does not cross applications", "—", "NOT-COVERED",
                   f"{saas_name} storefront answered {r.status_code}", saas_name)
    elif "Catalogue —" not in body:
        REPORT.add("page cache does not cross applications", "—", "FAIL",
                   f"{saas_name} served a page with no tenant heading after "
                   f"{single_name} warmed the same Host — the two apps share a "
                   f"cache key: {body[:200]}", saas_name)
    else:
        REPORT.add("page cache does not cross applications", "—", "PASS",
                   "each deployment has its own key namespace", saas_name)


async def check_soak_info(client, name, base, headers=None, expect_tenant=None):
    r = await client.get(f"{base}/_soak/info", headers=headers)
    if r.status_code != 200:
        REPORT.add("instance reachable", "—", "FAIL", f"{r.status_code}", name)
        return None
    info = r.json()

    # #1456 — the deployment's pool sizing must have reached the pools.
    # `TenantPoolsConfig` existed but nothing on `Cli` reached the
    # `TenantPools` it built internally, so a deployment could set every
    # knob it liked and get 16 connections per tenant regardless. The
    # compose file sets 6; the framework default is 16, so a report of
    # 16 here means the config went nowhere.
    pool = info.get("pool")
    if pool is None:
        pass  # single-tenant app: no tenant pools to size
    elif pool.get("max_connections") == 6:
        REPORT.add("tenant pool sizing reaches the pools", "#1456", "PASS",
                   f"max_connections={pool['max_connections']}, "
                   f"cache_max={pool.get('cache_max')}", name)
    else:
        REPORT.add("tenant pool sizing reaches the pools", "#1456", "FAIL",
                   f"compose set TENANT_POOL_MAX_CONNECTIONS=6 but the process "
                   f"reports {pool.get('max_connections')}", name)

    if expect_tenant and info.get("tenant") != expect_tenant:
        # Asserted on the *response*, not on the Host we sent: the two
        # can differ for ~30s after provisioning because the resolver
        # caches negative results too.
        REPORT.add("Host routed to the intended tenant", "—", "FAIL",
                   f"sent {expect_tenant}, reached {info.get('tenant')}", name)
    elif expect_tenant:
        REPORT.add("Host routed to the intended tenant", "—", "PASS",
                   f"{expect_tenant} on {info.get('dialect')}", name)
    return info


async def check_registered_host(client, name, base):
    """RegisteredHostResolver — the middle link of the default chain.

    t19/t20 were given hostnames that are not `<slug>.<apex>`, so
    SubdomainResolver must miss and the host table must hit.
    """
    for host, slug in (("shop-t19.example.test", "t19"),
                       ("storefront-t20.example.test", "t20")):
        r = await client.get(f"{base}/_soak/info", headers={"Host": host})
        if r.status_code == 200 and r.json().get("tenant") == slug:
            REPORT.add(f"custom hostname resolves ({host})", "—", "PASS", "", name)
        else:
            REPORT.add(f"custom hostname resolves ({host})", "—", "FAIL",
                       f"{r.status_code} {r.text[:120]}", name)


async def check_tenant_isolation(client, name, base):
    """Two tenants must not see each other's rows."""
    a, b = tenant_host(1), tenant_host(2)
    sku = f"ISO-{time.time_ns()}"
    r = await client.post(f"{base}/api/v1/products", headers={"Host": a}, json={
        "sku": sku, "name": "Isolation probe", "blurb": None,
        "price_cents": 999, "active": True})
    if r.status_code not in (200, 201):
        REPORT.add("tenant isolation", "—", "NOT-COVERED",
                   f"could not create in t01: {r.status_code} {r.text[:160]}", name)
        return
    other = await client.get(f"{base}/api/v1/products", headers={"Host": b},
                             params={"sku": sku})
    rows = []
    if other.status_code == 200:
        body = other.json()
        rows = body.get("results", body) if isinstance(body, dict) else body
    if rows:
        REPORT.add("tenant isolation", "—", "FAIL",
                   f"t02 can see t01's product {sku}", name)
    else:
        REPORT.add("tenant isolation", "—", "PASS", "", name)


async def check_apex_does_not_serve_app(client, name, base):
    """The apex host is the operator console, not a tenant."""
    r = await client.get(f"{base}/api/v1/products", headers={"Host": APEX})
    if r.status_code in (200, 201):
        REPORT.add("apex host does not serve tenant routes", "—", "FAIL",
                   f"apex answered {r.status_code}", name)
    else:
        REPORT.add("apex host does not serve tenant routes", "—", "PASS",
                   f"{r.status_code}", name)


async def check_supervisor_retires(client, name, base):
    """The refresh loop removes queues, not just adds them.

    It only ever added. A tenant that was deactivated or deleted kept
    its two workers polling a database it no longer used, held its pool
    against the server's connection limit and against the 64-pool cache
    cap (which has no eviction), and was drained on every deploy.

    Nothing reported it: a deactivated tenant produces no error, it just
    stops appearing in the registry query. So the only way to see the
    behaviour is to *cause* the absence and watch the queue go.

    Run last, on a tenant the other checks do not touch — t01/t02 carry
    the isolation checks and t19/t20 the custom hostnames.
    """
    victim = f"t{TENANTS - 2:02d}"
    observer = {"Host": tenant_host(1)}

    before = await client.get(f"{base}/_soak/jobs", headers=observer)
    if before.status_code != 200:
        REPORT.add("supervisor retires a deactivated tenant", "—", "NOT-COVERED",
                   f"could not read /_soak/jobs: {before.status_code}", name)
        return
    if victim not in before.json().get("queues", []):
        REPORT.add("supervisor retires a deactivated tenant", "—", "NOT-COVERED",
                   f"{victim} had no queue to retire", name)
        return

    r = await client.post(f"{base}/_soak/tenants/{victim}/active",
                          headers=observer, content="false")
    if r.status_code != 200:
        REPORT.add("supervisor retires a deactivated tenant", "—", "NOT-COVERED",
                   f"could not deactivate {victim}: {r.status_code} {r.text[:160]}", name)
        return

    # The refresh loop ticks every 15s; give it two plus slack rather
    # than polling, which would say nothing useful about the latency.
    await asyncio.sleep(40)
    after = await client.get(f"{base}/_soak/jobs", headers=observer)
    body = after.json() if after.status_code == 200 else {}
    queues, pools = body.get("queues", []), body.get("pools", [])

    if victim in queues:
        REPORT.add("supervisor retires a deactivated tenant", "—", "FAIL",
                   f"{victim} was deactivated 40s ago and still has a running "
                   f"queue: {queues}", name)
    elif victim in pools:
        REPORT.add("supervisor retires a deactivated tenant", "—", "FAIL",
                   f"{victim}'s queue stopped but its pool is still registered — "
                   f"the connections leak: {pools}", name)
    else:
        REPORT.add("supervisor retires a deactivated tenant", "—", "PASS",
                   f"{victim} left both the queue map and the pool registry", name)

    # Put it back, so a second run of the driver starts from the same
    # fleet state as the first.
    await client.post(f"{base}/_soak/tenants/{victim}/active",
                      headers=observer, content="true")


def report_uncoverable(live_single, live_saas):
    """Name what this harness cannot decide, and why.

    A finding that no check covers is a finding that silently counts as
    passing, which is the failure mode this whole report is built to
    avoid. These are stated rather than omitted.
    """
    booted = len(live_single) + len(live_saas)

    # #1458 — concurrent `CREATE TABLE IF NOT EXISTS` racing itself.
    # Postgres is not atomic here: two sessions can both pass the
    # existence check and one gets `42P07`/`23505` on a pg_catalog
    # index. The fleet boots six instances plus workers against three
    # shared databases, so the race is *run*, but the only observable
    # afterwards is that everything came up. A run where it did not
    # reproduce is not evidence the predicate is right.
    if booted:
        REPORT.add(
            "concurrent boot does not hit a DDL race", "#1458", "PASS",
            f"{booted} instance(s) plus workers raced `ensure_table` on shared "
            f"databases and all came up. Note: a non-reproduction is weak "
            f"evidence — the predicate itself is pinned by unit tests",
        )
    else:
        REPORT.add("concurrent boot does not hit a DDL race", "#1458",
                   "NOT-COVERED", "no instance came up", "")

    # #1455 — `make:job` emitted a scheduler task with a hardcoded
    # PgPool. Pure code generation: the verb's output never reaches a
    # running server, so no request can see it.
    REPORT.add(
        "make:job scaffolds a real Job", "#1455", "NOT-COVERED",
        "scaffolder-only — no runtime surface. Covered by "
        "crates/rustango/tests/make_job_scaffolds_a_real_job.rs",
    )

    # #1450 on the two dialects that were never changed.
    REPORT.add(
        "nullable bigint FK binding (MySQL/SQLite)", "#1450", "NOT-COVERED",
        "the MySQL and SQLite binders were deliberately left unchanged; those "
        "legs are controls, and a green there does not distinguish the fix",
    )


# ------------------------------------------------------------------ load


@dataclass
class Counters:
    ok: int = 0
    err: int = 0
    by_status: dict = field(default_factory=dict)
    confirmed: int = 0

    def record(self, status):
        self.by_status[str(status)] = self.by_status.get(str(status), 0) + 1
        if 200 <= status < 400:
            self.ok += 1
        else:
            self.err += 1


async def load_worker(client, counters: Counters, targets, until, rng):
    """Sustained mixed traffic until the deadline."""
    while time.time() < until:
        name, base, headers = rng.choice(targets)
        try:
            roll = rng.random()
            if roll < 0.35:
                r = await client.get(f"{base}/api/v1/products", headers=headers,
                                     params={"limit": 25})
            elif roll < 0.5:
                r = await client.get(f"{base}/shop/products", headers=headers)
            elif roll < 0.7:
                r = await client.post(f"{base}/api/v1/products", headers=headers, json={
                    "sku": f"LOAD-{RUN_ID}-{rng.getrandbits(32):x}", "name": "Load item",
                    "blurb": None, "price_cents": rng.randint(100, 9999),
                    "active": True})
            elif roll < 0.85:
                r = await client.get(f"{base}/api/v1/orders", headers=headers)
            else:
                # Order + confirm: this is what dispatches jobs.
                cu = await client.post(f"{base}/api/v1/customers", headers=headers, json={
                    "contact_email": f"load-{RUN_ID}-{rng.getrandbits(32):x}@example.test",
                    "full_name": "Load buyer", "loyalty_tier": "bronze"})
                if cu.status_code not in (200, 201):
                    counters.record(cu.status_code)
                    continue
                # Half the writes through the serializer, half through
                # the raw ViewSet. Every order used to go through
                # `orders-raw`, so the serializer write path — the newest
                # surface on this branch, and the one #1454 made possible
                # at all — never saw a single request under load.
                #
                # The two bodies differ because that is the point: the
                # serializer publishes `reference` as `ref_code` and
                # `assigned_picker_id` as `picker_id`.
                if rng.random() < 0.5:
                    o = await client.post(f"{base}/api/v1/orders", headers=headers, json={
                        "ref_code": f"LOAD-{RUN_ID}-{rng.getrandbits(32):x}",
                        "customer_id": cu.json().get("id"),
                        "picker_id": None,
                        "status": "pending", "total_cents": rng.randint(100, 50000),
                        "note": None})
                else:
                    o = await client.post(f"{base}/api/v1/orders-raw", headers=headers, json={
                        "reference": f"LOAD-{RUN_ID}-{rng.getrandbits(32):x}",
                        "customer_id": cu.json().get("id"),
                        "status": "pending", "total_cents": rng.randint(100, 50000),
                        "note": None})
                counters.record(o.status_code)
                if o.status_code in (200, 201):
                    oid = o.json().get("id")
                    r = await client.post(f"{base}/api/v1/orders/{oid}/confirm",
                                          headers=headers)
                    if r.status_code == 202:
                        counters.confirmed += 1
                else:
                    continue
            counters.record(r.status_code)
        except Exception:
            counters.err += 1


# ------------------------------------------------------------------ main


async def main():
    os.makedirs(RESULTS, exist_ok=True)
    rng = random.Random(20260914)
    limits = httpx.Limits(max_connections=CONCURRENCY * 2,
                          max_keepalive_connections=CONCURRENCY)
    async with httpx.AsyncClient(timeout=30, limits=limits) as client:
        print("\n== waiting for instances ==")
        live_single, live_saas = {}, {}
        for name, base in SINGLE.items():
            if await wait_ready(client, name, base):
                live_single[name] = base
                print(f"  up   {name}")
            else:
                # FAIL, not NOT-COVERED. NOT-COVERED means "this harness
                # cannot decide the question"; an instance that never
                # came up is the fleet answering it. Scoring it as a skip
                # let the run exit 0 with a third of the matrix dead.
                REPORT.add("instance reachable", "—", "FAIL",
                           "never became ready", name)
                print(f"  DOWN {name}")

        # Tenant hosts: sleep-then-probe, never poll. The resolver caches
        # negative results for 30s, so an early miss would make a working
        # tenant look broken for a minute.
        for name, base in SAAS.items():
            hdr = {"Host": tenant_host(1)}
            if await wait_ready(client, name, base, headers=hdr):
                live_saas[name] = base
                print(f"  up   {name}")
            else:
                REPORT.add("instance reachable", "—", "FAIL",
                           "never became ready", name)
                print(f"  DOWN {name}")

        print("\n== assertions: single-tenant ==")
        for name, base in live_single.items():
            await check_soak_info(client, name, base)
            await check_health_endpoints(client, name, base)
            await check_null_fk(client, name, base)
            await check_serializer_carries_fk(client, name, base)
            await check_bulk_atomic(client, name, base)
            await check_serializer_source(client, name, base)
            await check_pagination(client, name, base)
            await check_cursor_walks(client, name, base)
            await check_malformed_cursor_is_400(client, name, base)
            await check_page_cache(client, name, base)

        print("\n== assertions: multi-tenant ==")
        for name, base in live_saas.items():
            hdr = {"Host": tenant_host(1)}
            await check_soak_info(client, name, base, headers=hdr,
                                  expect_tenant="t01")
            await check_health_endpoints(client, name, base)
            await check_null_fk(client, name, base, headers=hdr)
            await check_serializer_carries_fk(client, name, base, headers=hdr)
            await check_bulk_atomic(client, name, base, headers=hdr)
            await check_serializer_source(client, name, base, headers=hdr)
            await check_pagination(client, name, base, headers=hdr)
            await check_cursor_walks(client, name, base, headers=hdr)
            await check_malformed_cursor_is_400(client, name, base, headers=hdr)
            await check_page_cache(client, name, base, headers=hdr)
            await check_page_cache_is_per_tenant(client, name, base)
            await check_tenant_isolation(client, name, base)
            await check_registered_host(client, name, base)
            await check_apex_does_not_serve_app(client, name, base)

        await check_page_cache_does_not_cross_apps(client, live_single, live_saas)
        report_uncoverable(live_single, live_saas)

        targets = [(n, b, None) for n, b in live_single.items()]
        targets += [(n, b, {"Host": tenant_host(rng.randint(1, TENANTS))})
                    for n, b in live_saas.items() for _ in range(3)]
        if not targets:
            print("\nno live instances — nothing to drive")
            REPORT.requests = {"ok": 0, "err": 0}
            write_report()
            return 1

        print(f"\n== load: {CONCURRENCY} workers for {DURATION}s across "
              f"{len(targets)} target(s) ==")
        counters = Counters()
        until = time.time() + DURATION
        await asyncio.gather(*[
            load_worker(client, counters, targets, until, random.Random(rng.getrandbits(32)))
            for _ in range(CONCURRENCY)
        ])
        REPORT.requests = {"ok": counters.ok, "err": counters.err,
                           "by_status": counters.by_status,
                           "orders_confirmed": counters.confirmed}

        # The load phase sends only well-formed requests, so its own
        # error rate is a signal and nothing was reading it. A fixed RNG
        # seed against a persistent database had 80% of customer POSTs
        # and 29% of product POSTs coming back 400 on duplicate keys,
        # across four runs, and the report showed the number without
        # anyone having to agree it was acceptable.
        #
        # 5xx is held at zero separately: a 4xx under load can be a
        # legitimate collision, a 5xx never is.
        total = counters.ok + counters.err
        by = counters.by_status
        server_errors = sum(v for k, v in by.items() if 500 <= int(k) < 600)
        if server_errors:
            REPORT.add("no server errors under load", "—", "FAIL",
                       f"{server_errors} 5xx response(s): "
                       f"{ {k: v for k, v in by.items() if 500 <= int(k) < 600} }")
        else:
            REPORT.add("no server errors under load", "—", "PASS",
                       f"0 of {total} responses were 5xx")

        rate = (counters.err / total * 100) if total else 0.0
        if total < 100:
            REPORT.add("load error rate is sane", "—", "NOT-COVERED",
                       f"only {total} request(s) — too few to judge")
        elif rate <= 5.0:
            REPORT.add("load error rate is sane", "—", "PASS",
                       f"{rate:.1f}% 4xx ({counters.err} of {total})")
        else:
            worst = sorted(((v, k) for k, v in by.items() if int(k) >= 400),
                           reverse=True)[:3]
            REPORT.add("load error rate is sane", "—", "FAIL",
                       f"{rate:.1f}% of load requests failed ({counters.err} of "
                       f"{total}); the load phase sends only well-formed requests, "
                       f"so this is the app or the fixture, not the traffic. "
                       f"Top statuses: {[(k, v) for v, k in worst]}")
        print(f"  {counters.ok} ok, {counters.err} error, "
              f"{counters.confirmed} orders confirmed")

        # Let the queues drain before counting them.
        print("\n== waiting 60s for queues to drain ==")
        await asyncio.sleep(60)
        jobs = {}
        for name, base in list(live_single.items()):
            r = await client.get(f"{base}/_soak/jobs")
            if r.status_code == 200:
                jobs[name] = r.json()
        for name, base in list(live_saas.items()):
            r = await client.get(f"{base}/_soak/jobs", headers={"Host": tenant_host(1)})
            if r.status_code == 200:
                jobs[name] = r.json()
        REPORT.jobs = jobs
        for name, j in jobs.items():
            pending = j.get("pending")
            if pending in (0, None):
                REPORT.add("job queue drained", "—", "PASS", f"pending={pending}", name)
            else:
                REPORT.add("job queue drained", "—", "FAIL",
                           f"{pending} still pending after 60s", name)

        # Last, because it deactivates a tenant: anything after it would
        # be running against a fleet in a state the rest did not expect.
        print("\n== supervisor teardown ==")
        for name, base in live_saas.items():
            await check_supervisor_retires(client, name, base)

    return write_report()


def write_report():
    out = {
        "checks": [asdict(c) for c in REPORT.checks],
        "requests": REPORT.requests,
        "jobs": REPORT.jobs,
        "duration_secs": round(time.time() - REPORT.started, 1),
    }
    path = os.path.join(RESULTS, "report.json")
    try:
        with open(path, "w") as fh:
            json.dump(out, fh, indent=2)
    except OSError as e:
        print(f"could not write {path}: {e}")

    npass = sum(1 for c in REPORT.checks if c.verdict == "PASS")
    nfail = len(REPORT.failed())
    nskip = sum(1 for c in REPORT.checks if c.verdict == "NOT-COVERED")
    known = REPORT.known_gaps()
    print("\n" + "=" * 62)
    print(f"  {npass} passed, {nfail} FAILED, {len(known)} known gap(s), "
          f"{nskip} not covered")
    if nfail:
        print("\n  failures:")
        for c in REPORT.failed():
            print(f"    {c.issue:>6}  {c.name} [{c.instance}]\n           {c.detail}")
    # Printed even on a green run, and never folded into the pass count:
    # a known gap is a behaviour that is broken and shipped knowingly,
    # which is a different thing from one that is covered and working.
    if known:
        print("\n  known gaps — filed, failing, and shipped deliberately:")
        for c in known:
            print(f"    {c.issue:>6}  {c.name} [{c.instance}]\n           {c.detail}")
    print("=" * 62)
    return 1 if nfail else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
