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
    # What #1610's log scan in finalize.py looks for.
    redaction: dict = field(default_factory=dict)

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


# `ApiError::from_status` — the code each status must carry (#1193).
ERROR_CODES = {400: "bad_request", 401: "unauthorized", 403: "forbidden",
               404: "not_found", 409: "conflict", 422: "validation_failed",
               429: "rate_limited"}

# Driver text that must stay in the log, never in a 4xx body (#1193).
DRIVER_WORDS = ("commerce_", "unique", "duplicate", "constraint", "violates",
                "sqlite", "mysql", "postgres")


def envelope_problem(r) -> str | None:
    """Why `r` is not an `ApiError` body for its status, or None."""
    try:
        body = r.json()
    except ValueError:
        return f"not JSON: {r.text[:120]!r}"
    if not isinstance(body, dict):
        return f"not an object: {r.text[:120]!r}"
    want = ERROR_CODES.get(r.status_code)
    if body.get("status") != r.status_code:
        return f"status field {body.get('status')!r} != {r.status_code}"
    if want and body.get("error") != want:
        return f"error {body.get('error')!r}, want {want!r}"
    if not isinstance(body.get("message"), str):
        return f"no string message: {r.text[:120]!r}"
    return None


def set_cookie_value(r, name: str) -> str | None:
    """The value `r` sets for cookie `name`, read off the raw headers.

    Read by hand: the fleet runs `RUSTANGO_ENV=prod`, so cookies are
    `Secure`, and httpx will not send those back over plain HTTP.
    """
    for h in r.headers.get_list("set-cookie"):
        first = h.split(";", 1)[0]
        if first.startswith(f"{name}="):
            return first[len(name) + 1:]
    return None


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

    # #1193 — a 400 from the database, with the driver text kept out.
    problem = envelope_problem(r) if r.status_code == 400 else f"status {r.status_code}"
    leaked = [w for w in DRIVER_WORDS if w in r.text.lower()]
    if problem or leaked:
        REPORT.add("constraint error is a 400 envelope without driver text", "#1193",
                   "FAIL", problem or f"body names {leaked}: {r.text[:200]}", name)
    else:
        REPORT.add("constraint error is a 400 envelope without driver text", "#1193",
                   "PASS", r.json().get("message", ""), name)


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

    # #1193 — serializer validation is a 422, with the field in details.
    problem = envelope_problem(r) if r.status_code == 422 else f"status {r.status_code}"
    if not problem and "price_cents" not in (r.json().get("details") or {}):
        problem = f"details do not name price_cents: {text[:200]}"
    REPORT.add("serializer validation is a 422 envelope", "#1193",
               "FAIL" if problem else "PASS", problem or "", name)


async def check_not_found_envelope(client, name, base, headers=None):
    """#1193 — a missing row is a 404 `ApiError`, not a bare string."""
    r = await client.get(f"{base}/api/v1/products/987654321987", headers=headers)
    problem = envelope_problem(r) if r.status_code == 404 else f"status {r.status_code}"
    REPORT.add("missing row is a 404 envelope", "#1193",
               "FAIL" if problem else "PASS", problem or "", name)


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
        problem = envelope_problem(r)
        REPORT.add("malformed cursor is a 400 envelope", "#1193",
                   "FAIL" if problem else "PASS", problem or "", name)
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


async def check_unknown_tenant_envelope(client, name, base):
    """#1193 — a tenant rejection is JSON, not plain text."""
    r = await client.get(f"{base}/api/v1/products",
                         headers={"Host": f"no-such-tenant.{APEX}"})
    problem = (envelope_problem(r) if 400 <= r.status_code < 500
               else f"status {r.status_code}")
    REPORT.add("unknown tenant is an error envelope", "#1193",
               "FAIL" if problem else "PASS",
               problem or f"{r.status_code} {r.json().get('error')}", name)


TENANT_ADMIN = ("soakadmin", os.environ.get("SOAK_TENANT_ADMIN_PASSWORD", "soak-admin-pw"))
SESSION_COOKIE = "rustango_tenant_session"


async def login_form(client, base, host):
    """GET the tenant login page; return its CSRF token, or None."""
    r = await client.get(f"{base}/__login", headers={"Host": host})
    return set_cookie_value(r, "rustango_csrf") if r.status_code == 200 else None


async def post_login(client, base, host, token, *, send_field=True, origin=None,
                     user=TENANT_ADMIN):
    headers = {"Host": host, "Cookie": f"rustango_csrf={token}"}
    if origin:
        headers["Origin"] = origin
    form = {"username": user[0], "password": user[1], "next": "/__admin"}
    if send_field:
        form["_csrf"] = token
    return await client.post(f"{base}/__login", headers=headers, data=form)


async def check_tenant_login(client, name, base):
    """The tenant login, end to end, as a browser does it.

    #1607 — the POST needs the CSRF pair. #1692 — the admin gate reads
    the session cookie through the shared cookie reader; no other check
    sends a valid session. GHSA-c4gg-mvfq-h268 — t01's login and t01's
    session must not work on t02.
    """
    a, b = tenant_host(1), tenant_host(2)
    # Under `no-referrer` a browser sends `Origin: null` on every POST,
    # which the Origin check refuses. The driver sets Origin itself, so
    # it can only see the header that causes it. Read off an app route:
    # that is where the configured preset applies.
    api = await client.get(f"{base}/api/v1/products", headers={"Host": a})
    policy = api.headers.get("referrer-policy", "")
    REPORT.add("headers preset lets browsers send their Origin", "#1695",
               "NOT-COVERED" if not policy else
               "FAIL" if policy == "no-referrer" else "PASS",
               f"Referrer-Policy: {policy or '(none sent)'}", name)

    page = await client.get(f"{base}/__login", headers={"Host": a})
    xfo = page.headers.get("x-frame-options")
    REPORT.add("tenant login page carries security headers", "#1699",
               "PASS" if xfo else "FAIL",
               f"X-Frame-Options: {xfo}" if xfo else
               "no X-Frame-Options: the login can be framed", name)

    token = await login_form(client, base, a)
    if not token:
        REPORT.add("tenant login", "#1607", "NOT-COVERED",
                   "GET /__login set no rustango_csrf cookie", name)
        return

    r = await post_login(client, base, a, token, send_field=False)
    REPORT.add("tenant login refuses a POST without the CSRF field", "#1607",
               "PASS" if r.status_code == 403 else "FAIL",
               "" if r.status_code == 403 else f"got {r.status_code}", name)

    # #1529 made Origin the default check, but only in `CsrfLayer`. Every
    # tenant shares the apex, so a page on one tenant can plant the
    # cookie half for another; Origin is what tells them apart.
    r = await post_login(client, base, a, token, origin="http://evil.example")
    REPORT.add("tenant login refuses a foreign Origin", "#1695",
               "PASS" if r.status_code == 403 else "FAIL",
               "" if r.status_code == 403 else
               f"got {r.status_code}: the login checks the token pair but not Origin",
               name)

    r = await post_login(client, base, a, token, origin=f"http://{a}")
    session = set_cookie_value(r, SESSION_COOKIE)
    if r.status_code != 303 or not session:
        REPORT.add("tenant login signs a user in", "#1607", "FAIL",
                   f"{r.status_code}, session cookie set: {bool(session)}", name)
        return
    REPORT.add("tenant login signs a user in", "#1607", "PASS", "", name)

    # Two cookies, session second, as a browser sends them: a reader that
    # splits or trims wrongly misses it.
    cookie = {"Cookie": f"rustango_csrf={token}; {SESSION_COOKIE}={session}"}
    ok = await client.get(f"{base}/__admin", headers={"Host": a, **cookie})
    anon = await client.get(f"{base}/__admin", headers={"Host": a})
    good = ok.status_code == 200 and anon.status_code in (302, 303)
    REPORT.add("tenant admin admits the session cookie, and only it", "#1692",
               "PASS" if good else "FAIL",
               "" if good else f"with cookie {ok.status_code}, without {anon.status_code}",
               name)

    # Refused means sent to the login page; a 500 is not a refusal.
    other = await client.get(f"{base}/__admin", headers={"Host": b, **cookie})
    refused = other.status_code in (302, 303)
    REPORT.add("t01's session is refused on t02", "GHSA-c4gg",
               "PASS" if refused else "FAIL", f"{other.status_code}", name)

    token_b = await login_form(client, base, b)
    if not token_b:
        REPORT.add("t01's password is refused on t02", "GHSA-c4gg", "NOT-COVERED",
                   "t02's login set no CSRF cookie", name)
        return
    r = await post_login(client, base, b, token_b, origin=f"http://{b}")
    signed_in = set_cookie_value(r, SESSION_COOKIE)
    # Only the bad-credentials redirect counts; a 403 or 500 proves nothing.
    refused = (r.status_code == 303 and not signed_in
               and "error=" in r.headers.get("location", ""))
    REPORT.add("t01's password is refused on t02", "GHSA-c4gg",
               "PASS" if refused else "FAIL",
               f"{r.status_code} -> {r.headers.get('location', '')}, "
               f"session cookie set: {bool(signed_in)}", name)


async def check_every_tenant_answers(client, name, base, info):
    """#1527/#1528 — past the pool cap, the idle pool is evicted.

    Database mode used to refuse every tenant past the cap, on every
    request, until a restart. Run after the load phase, which spread
    requests over all tenants.
    """
    pool = (info or {}).get("pool") or {}
    cap = pool.get("scoped_cache_max" if name == "saas-pg" else "cache_max")
    issue = "#1528" if name == "saas-pg" else "#1527"
    if not cap or cap >= TENANTS:
        REPORT.add("every tenant answers past the pool cap", issue, "NOT-COVERED",
                   f"cap {cap} is not below {TENANTS} tenants", name)
        return
    down = []
    for i in range(1, TENANTS + 1):
        r = await client.get(f"{base}/_soak/info", headers={"Host": tenant_host(i)})
        if r.status_code != 200:
            down.append(f"t{i:02d}={r.status_code}")
    # Schema mode answered before its fix too; its bug was unbounded
    # connections, which HTTP cannot count.
    note = " (schema mode: evidence of no refusal only)" if name == "saas-pg" else ""
    REPORT.add("every tenant answers past the pool cap", issue,
               "FAIL" if down else "PASS",
               f"cap {cap}, down: {down}" if down else f"cap {cap}, {TENANTS} tenants{note}",
               name)


async def check_supervisor_retires(client, name, base):
    """The refresh loop removes queues, not just adds them.

    It only ever added. A tenant that was deactivated or deleted kept
    its two workers polling a database it no longer used, held its pool
    against the server's connection limit and against the pool cache
    cap, and was drained on every deploy.

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


# ------------------------------------------------------ 0.58.0 checks
#
# One or more named checks per fix shipped in 0.58.0. Each asserts the
# behaviour the fix changed, so the pre-fix code fails it; where a leg
# cannot tell the two apart it says "control" in its detail.

EDGE_SAAS = ("saas-pg-edge", "http://web-saas-pg-edge:8080")
EDGE_SINGLE = ("single-pg-edge", "http://web-single-pg-edge:8080")
IDP = os.environ.get("SOAK_IDP", "http://idp:9000")
SVC_SECRET = os.environ.get("SOAK_SERVICE_TOKEN_SECRET", "")
PG_URL = os.environ.get("SOAK_PG_URL", "")
# The edge instance redirects plain HTTP; this says the hop was TLS.
TLS = {"X-Forwarded-Proto": "https"}

ADMIN_USER = ("soakadmin", os.environ.get("SOAK_ADMIN_PASSWORD", "soak-admin-pw"))
OPERATOR = ("soakops", os.environ.get("SOAK_OPERATOR_PASSWORD", "soak-operator-pw"))
OP_COOKIE = "rustango_op_session"
ADMIN_COOKIE = "rustango_admin_session"


def pw(user: str) -> str:
    """`bootstrap.sh` gives every probe account `soak-<user>-pw`."""
    return f"soak-{user}-pw"


# Every credential, token and cookie the driver handles: the log scan
# (#1610) fails the run if any of them shows up in a container log.
USED_SECRETS: set[str] = set()


def secret(v):
    if isinstance(v, str) and len(v) >= 8:
        USED_SECRETS.add(v)
    return v


for _v in (ADMIN_USER[1], OPERATOR[1], TENANT_ADMIN[1]):
    secret(_v)

_ip_rng = random.Random(f"ips-{RUN_ID}")


def fresh_ip() -> str:
    """A public client address nobody else in this run has used."""
    r = _ip_rng
    return f"45.{r.randint(1, 254)}.{r.randint(0, 255)}.{r.randint(1, 254)}"


def cookie_count(r, name: str) -> int:
    return sum(1 for h in r.headers.get_list("set-cookie")
               if h.split(";", 1)[0].startswith(f"{name}="))


def host_of(base: str, headers: dict | None) -> str:
    if headers and "Host" in headers:
        return headers["Host"]
    return base.split("://", 1)[1]


def origin_of(base: str, headers: dict | None) -> str:
    return f"http://{host_of(base, headers)}"


def verdict(ok: bool) -> str:
    return "PASS" if ok else "FAIL"


def is_refusal(r) -> bool:
    """A login-gate refusal: 429 with a Retry-After."""
    return r.status_code == 429 and bool(r.headers.get("retry-after"))


# ------------------------------------------------------------- #1673


async def check_forwarded_for(client, name, base, headers=None):
    """#1673 — the client is the rightmost hop no trusted proxy wrote.

    `203.0.113.9` is what a client can write, `::ffff:172.20.0.5` is a
    trusted proxy in IPv4-mapped form. The old code took the leftmost
    hop; a code that only fixed that would stop at the mapped proxy.
    """
    h = dict(headers or {})
    h["X-Forwarded-For"] = "203.0.113.9, 198.51.100.7, ::ffff:172.20.0.5"
    r = await client.get(f"{base}/_soak/ip", headers=h)
    if r.status_code != 200:
        REPORT.add("client IP is the rightmost untrusted XFF hop", "#1673", "FAIL",
                   f"/_soak/ip answered {r.status_code}", name)
        return
    got = r.json().get("trusted_ip")
    why = {"203.0.113.9": "took the leftmost hop, which the client writes",
           "::ffff:172.20.0.5": "a v4-mapped trusted proxy did not match 172.16.0.0/12"}
    REPORT.add("client IP is the rightmost untrusted XFF hop", "#1673",
               verdict(got == "198.51.100.7"),
               f"trusted_ip={got}" + (f" — {why[got]}" if got in why else ""), name)
    plain = await client.get(f"{base}/_soak/ip", headers=headers)
    REPORT.add("no forwarding header, no trusted client IP", "#1673",
               verdict(plain.status_code == 200 and plain.json().get("trusted_ip") is None),
               f"{plain.status_code} {plain.text[:100]}", name)


async def check_v4_mapped_filter(client, name, base, headers=None):
    """#1673 — an IPv4 rule matches an IPv4 client on a dual-stack socket."""
    r = await client.get(f"{base}/_soak/v4-blocked", headers=headers)
    dual = name == "single-sq"
    REPORT.add("ip_filter v4 rule blocks a v4-mapped peer", "#1673",
               verdict(r.status_code == 403),
               (f"{r.status_code}" + ("" if dual else
                                      " — control: this listener is IPv4-only")), name)


async def raw_status(base, head: str, body_chunks) -> int | None:
    """Send one HTTP/1.1 request by hand and return the status.

    httpx gives up when the server answers 413 and closes while the
    body is still going up; a browser or proxy reads the answer anyway.
    """
    from urllib.parse import urlparse
    u = urlparse(base)
    reader, writer = await asyncio.open_connection(u.hostname, u.port or 80)
    try:
        writer.write(head.encode())
        for chunk in body_chunks:
            try:
                writer.write(chunk)
                await writer.drain()
            except (ConnectionError, OSError):
                break
        line = await asyncio.wait_for(reader.readline(), 10)
        return int(line.split()[1]) if line.startswith(b"HTTP/") else None
    finally:
        writer.close()


async def check_body_limit(client, name, base, headers=None):
    """#1673 — `max_body_bytes` (256 KiB) caps chunked bodies and QUERY.

    The chunked body is valid JSON, so without the cap the ViewSet reads
    it and answers 400 on the long name, not 413.
    """
    big = json.dumps({"sku": f"BIG-{RUN_ID}", "name": "x" * 400_000,
                      "blurb": None, "price_cents": 1, "active": False}).encode()
    host = host_of(base, headers)
    chunked = [f"{len(big[i:i + 16_384]):x}\r\n".encode() + big[i:i + 16_384] + b"\r\n"
               for i in range(0, len(big), 16_384)] + [b"0\r\n\r\n"]
    st = await raw_status(base, f"POST /api/v1/products HTTP/1.1\r\nHost: {host}\r\n"
                          "Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n"
                          "Connection: close\r\n\r\n", chunked)
    REPORT.add("chunked body over max_body_bytes is 413", "#1673",
               verdict(st == 413), f"{st}", name)
    q = await raw_status(base, f"QUERY /api/v1/products HTTP/1.1\r\nHost: {host}\r\n"
                         f"Content-Type: application/json\r\nContent-Length: {len(big)}\r\n"
                         "Connection: close\r\n\r\n", [big])
    REPORT.add("QUERY body over max_body_bytes is 413", "#1673", verdict(q == 413), f"{q}", name)


# ------------------------------------------------------------- #1675


async def check_model_shortcut_scopes(client, name, base, headers=None):
    """#1675 — `Model::sum/min/max/avg/destroy/delete_where` see the scope."""
    r = await client.post(f"{base}/_soak/scopes/shortcuts", headers=headers)
    if r.status_code != 200:
        REPORT.add("Model shortcuts apply global scopes", "#1675", "FAIL",
                   f"probe answered {r.status_code}: {r.text[:200]}", name)
        return
    b = r.json()
    problems = [f"{k}: shortcut {b[f'shortcut_{k}']} != queryset {b[f'scoped_{k}']}"
                for k in ("sum", "min", "max", "avg")
                if b[f"shortcut_{k}"] != b[f"scoped_{k}"]]
    if b.get("shortcut_max") == 1_000_000_000 or b.get("shortcut_min") == 1:
        problems.append("an aggregate reached a hidden row")
    if b.get("destroy_hidden") != 0 or b.get("delete_where_hidden") != 0 or b.get("hidden_left") != 2:
        problems.append(f"destroy/delete_where touched hidden rows: {b}")
    if b.get("destroy_visible") != 1:
        problems.append(f"destroy of a visible row removed {b.get('destroy_visible')}")
    REPORT.add("Model shortcuts apply global scopes", "#1675",
               "FAIL" if problems else "PASS", "; ".join(problems), name)


async def check_audited_pool_writes(client, name, base, headers=None):
    """#1675 — audited insert/soft_delete/restore on `&Pool` write rows."""
    r = await client.post(f"{base}/_soak/audit/probe", headers=headers)
    if r.status_code != 200:
        REPORT.add("audited Pool writes record the real PK", "#1675", "FAIL",
                   f"probe answered {r.status_code}: {r.text[:200]}", name)
        return
    b = r.json()
    pk = str(b["pk"])
    ops = [e["operation"] for e in b["entries"]]
    pks = {e["entity_pk"] for e in b["entries"]}
    problems = []
    if ops != ["restore", "soft_delete", "create"]:
        problems.append(f"operations {ops}")
    if pks != {pk}:
        problems.append(f"entity_pk {sorted(pks)} != {pk}")
    by_op = {e["operation"]: e["changes"] for e in b["entries"]}
    if by_op.get("soft_delete", {}).get("deleted_at") in (None, ""):
        problems.append("soft_delete row has no deleted_at")
    if by_op.get("restore", {}).get("deleted_at") is not None:
        problems.append("restore row keeps deleted_at")
    REPORT.add("audited Pool writes record the real PK", "#1675",
               "FAIL" if problems else "PASS", "; ".join(problems) or f"pk {pk}: {ops}", name)


# ------------------------------------------------------ #1746 / #1751


async def check_viewset_scopes(client, name, base, headers=None):
    """#1746 — the ViewSet and the template views skip scoped-out rows."""
    issue = "#1746"
    seed = await client.post(f"{base}/_soak/scopes/seed", headers=headers)
    if seed.status_code != 200:
        REPORT.add("ViewSet applies global scopes", issue, "FAIL",
                   f"seed answered {seed.status_code}: {seed.text[:200]}", name)
        return
    s = seed.json()
    hid, vis = s["hidden"][0], s["visible"][0]
    api = f"{base}/api/v1/promotions"
    lst = await client.get(api, headers=headers, params={"search": s["tag"], "limit": 50})
    codes = [row.get("code", "") for row in (lst.json().get("results", []) if lst.status_code == 200 else [])]
    problems = []
    if lst.status_code != 200 or not any(c.startswith("VIS-") for c in codes):
        problems.append(f"list {lst.status_code}, codes {codes}")
    if any(c.startswith("HID-") for c in codes):
        problems.append(f"list shows hidden rows {codes}")
    det = await client.get(f"{api}/{hid}", headers=headers)
    if det.status_code != 404:
        problems.append(f"detail of a hidden row answered {det.status_code}")
    ok = await client.get(f"{api}/{vis}", headers=headers)
    if ok.status_code != 200:
        problems.append(f"detail of a visible row answered {ok.status_code} (control)")
    up = await client.patch(f"{api}/{hid}", headers=headers, json={"amount_cents": 7})
    if up.status_code != 404:
        problems.append(f"PATCH of a hidden row answered {up.status_code}")
    de = await client.delete(f"{api}/{s['hidden'][1]}", headers=headers)
    if de.status_code != 404:
        problems.append(f"DELETE of a hidden row answered {de.status_code}")
    for pk in s["hidden"]:
        row = await client.get(f"{base}/_soak/scopes/row/{pk}", headers=headers)
        if row.status_code != 200 or row.json().get("amount_cents") != 100:
            problems.append(f"hidden row {pk} changed: {row.status_code} {row.text[:120]}")
    REPORT.add("ViewSet applies global scopes", issue,
               "FAIL" if problems else "PASS", "; ".join(problems), name)

    # Template views: detail, edit and delete of a hidden row are 404.
    pages = f"{base}/promos"
    problems = []
    d = await client.get(f"{pages}/{hid}", headers=headers)
    if d.status_code != 404:
        problems.append(f"detail {d.status_code}")
    e = await client.get(f"{pages}/{hid}/edit", headers=headers)
    if e.status_code != 404:
        problems.append(f"edit form {e.status_code}")
    # Newest first, so the seeded rows are on page one; `total` counts
    # every page and must match the scoped count, not the unscoped one.
    lp = await client.get(pages, headers=headers)
    hidden_code = f"HID-{s['tag']}"
    total = lp.text.rsplit("total=", 1)[-1].split("<", 1)[0].strip()
    if lp.status_code != 200 or hidden_code in lp.text:
        problems.append(f"list {lp.status_code}, hidden code present: {hidden_code in lp.text}")
    if not total.isdigit() or int(total) >= s["all_total"]:
        problems.append(f"list total {total}, scoped {s['scoped_total']}, all {s['all_total']}")
    v = await client.get(f"{pages}/{vis}", headers=headers)
    if v.status_code != 200:
        problems.append(f"visible detail {v.status_code} (control)")
    REPORT.add("template views apply global scopes", issue,
               "FAIL" if problems else "PASS", "; ".join(problems), name)


# ------------------------------------------------------------- #1669


async def pages_csrf(client, base, headers):
    """A CSRF token for the template views, off their own form."""
    r = await client.get(f"{base}/promos/new", headers=headers)
    return set_cookie_value(r, "rustango_csrf") if r.status_code == 200 else None


async def check_urlize_and_cbv_csrf(client, name, base, headers=None):
    """#1669 — `urlize` escapes its output; template-view POSTs need CSRF."""
    label = 'see https://x.test/"><script>alert(1)</script> now'
    code = f"URL-{RUN_ID}-{time.time_ns() % 10**9}"
    r = await client.post(f"{base}/api/v1/promotions", headers=headers, json={
        "code": code, "label": label, "amount_cents": 1, "visible": True, "deleted_at": None})
    if r.status_code not in (200, 201):
        REPORT.add("urlize escapes user text", "#1669", "FAIL",
                   f"could not create a promotion: {r.status_code} {r.text[:200]}", name)
    else:
        pk = r.json().get("id")
        page = await client.get(f"{base}/promos/{pk}", headers=headers)
        body = page.text
        raw = "<script>alert(1)</script>" in body
        REPORT.add("urlize escapes user text", "#1669",
                   verdict(page.status_code == 200 and not raw and "&lt;script&gt;" in body),
                   f"{page.status_code}; raw script {'present' if raw else 'absent'}: "
                   f"{body[body.find('class=') : body.find('class=') + 200]!r}", name)

    token = await pages_csrf(client, base, headers)
    host = origin_of(base, headers)
    form = {"code": f"CBV-{RUN_ID}-{time.time_ns() % 10**9}", "label": "cbv",
            "amount_cents": "1", "visible": "true"}
    no = await client.post(f"{base}/promos/new", headers={**(headers or {}), "Origin": host},
                           data=form)
    empty = await client.post(f"{base}/promos/new", headers={
        **(headers or {}), "Origin": host, "Cookie": "rustango_csrf="}, data={**form, "_csrf": ""})
    REPORT.add("template-view POST without a CSRF token is 403", "#1669",
               verdict(no.status_code == 403), f"{no.status_code}", name)
    REPORT.add("CSRF refuses an empty token pair", "#1693",
               verdict(empty.status_code == 403), f"template view: {empty.status_code}", name)
    if token:
        ok = await client.post(f"{base}/promos/new", headers={
            **(headers or {}), "Origin": host, "Cookie": f"rustango_csrf={token}"},
            data={**form, "_csrf": token})
        REPORT.add("template-view POST with the token goes through", "#1669",
                   verdict(ok.status_code in (302, 303)), f"{ok.status_code} (control)", name)
    else:
        REPORT.add("template-view POST with the token goes through", "#1669", "FAIL",
                   "GET /promos/new set no CSRF cookie", name)


# ------------------------------------------------------------- #1671


async def check_natural_pk(client, name, base, headers=None):
    """#1671 — a ViewSet create keeps the client's primary key."""
    code = f"GC-{RUN_ID}-{time.time_ns() % 10**9}"
    r = await client.post(f"{base}/api/v1/gift-cards", headers=headers,
                          json={"code": code, "balance_cents": 500})
    got = await client.get(f"{base}/api/v1/gift-cards/{code}", headers=headers)
    ok = (r.status_code in (200, 201) and got.status_code == 200
          and got.json().get("code") == code)
    REPORT.add("ViewSet create keeps a natural PK", "#1671", verdict(ok),
               f"POST {r.status_code}, GET by code {got.status_code} {got.text[:120]}", name)
    none = await client.post(f"{base}/api/v1/gift-cards", headers=headers,
                             json={"balance_cents": 1})
    REPORT.add("ViewSet create without a PK is 400", "#1671",
               verdict(none.status_code == 400), f"{none.status_code} {none.text[:120]}", name)


# ------------------------------------------------------------- #1668


async def check_idempotency_scope(client, name, base, headers=None, other_tenant=None):
    """#1668 — a replay is for the same caller, tenant and route only."""
    issue = "#1668"
    url = f"{base}/api/v1/payments"
    key = f"idem-{RUN_ID}-{time.time_ns()}"
    h = headers or {}
    a = {**h, "Idempotency-Key": key, "Authorization": "Bearer caller-a"}
    body = {"amount_cents": 1200}
    first = await client.post(url, headers=a, json=body)
    again = await client.post(url, headers=a, json=body)
    if first.status_code != 201:
        REPORT.add("idempotent replay reaches only its caller", issue, "FAIL",
                   f"payment answered {first.status_code}: {first.text[:160]}", name)
        return
    pid = first.json().get("payment_id")
    REPORT.add("same caller, same key replays", issue,
               verdict(again.json().get("payment_id") == pid), "(control)", name)
    b = await client.post(url, headers={**a, "Authorization": "Bearer caller-b"}, json=body)
    anon = await client.post(url, headers={**h, "Idempotency-Key": key}, json=body)
    refund = await client.post(f"{base}/api/v1/refunds", headers=a, json=body)
    leaks = [label for label, resp in (("another caller", b), ("no caller", anon),
                                       ("another route", refund))
             if resp.status_code == 201 and resp.json().get("payment_id") == pid]
    REPORT.add("idempotent replay reaches only its caller and route", issue,
               "FAIL" if leaks else "PASS",
               f"replayed to: {leaks}" if leaks else "", name)
    if other_tenant:
        t = await client.post(url, headers={**a, "Host": other_tenant}, json=body)
        REPORT.add("idempotent replay stays in its tenant", issue,
                   verdict(t.status_code == 201 and t.json().get("payment_id") != pid),
                   f"{t.status_code}", name)
    changed = await client.post(url, headers=a, json={"amount_cents": 1})
    REPORT.add("reused key with another body is 422", issue,
               verdict(changed.status_code == 422), f"{changed.status_code}", name)
    ck = {**h, "Idempotency-Key": f"{key}-cookie", "Authorization": "Bearer caller-a"}
    c1 = await client.post(url, headers=ck, json={"amount_cents": 5, "remember": True})
    c2 = await client.post(url, headers=ck, json={"amount_cents": 5, "remember": True})
    stored = c1.status_code == 201 and c2.json().get("payment_id") == c1.json().get("payment_id")
    REPORT.add("a response that sets a cookie is not stored", issue, verdict(not stored),
               "second call was a replay" if stored else "", name)


# ------------------------------------------------------------- #1670


async def check_webhook_targets(client, name, base, headers=None):
    """#1670 — deliveries refuse internal targets, and do not redirect."""
    issue = "#1670"
    import socket
    try:
        idp_ip = socket.gethostbyname("idp")
    except OSError:
        idp_ip = "?"
    n = f"{RUN_ID}{time.time_ns() % 10**9}"
    blocked = {
        "loopback (this app)": f"http://127.0.0.1:8080/_soak/hook-sink/{n}-lo",
        "v4-mapped loopback": f"http://[::ffff:127.0.0.1]:8080/_soak/hook-sink/{n}-map",
        "private via DNS": f"{IDP}/hook/{n}-dns",
        "link-local metadata": "http://169.254.169.254/latest/meta-data/",
        "RFC 1918": "http://10.1.2.3/",
        "CGNAT": "http://100.64.0.1/",
        "unspecified": "http://0.0.0.0:8080/",
        "6to4 of loopback": "http://[2002:7f00:1::1]:8080/",
        "NAT64 of loopback": "http://[64:ff9b::7f00:1]:8080/",
        "IPv6 loopback": "http://[::1]:8080/",
    }
    problems = []
    for label, url in blocked.items():
        r = await client.post(f"{base}/_soak/webhooks/probe", headers=headers,
                              json={"url": url})
        out = r.json() if r.status_code == 200 else {}
        err = out.get("error", "")
        if out.get("delivered") or "blocked address" not in err:
            problems.append(f"{label}: {out or r.status_code}")
        if idp_ip in err:
            problems.append(f"{label}: the error names the resolved address")
    await asyncio.sleep(0.5)
    for sink in (f"{base}/_soak/hook-sink/{n}-lo", f"{base}/_soak/hook-sink/{n}-map"):
        hits = (await client.get(sink, headers=headers)).json().get("hits")
        if hits:
            problems.append(f"loopback sink got {hits} hit(s)")
    dns_hits = (await client.get(f"{IDP}/hooks/{n}-dns")).json().get("hits")
    if dns_hits:
        problems.append(f"private target got {dns_hits} hit(s)")
    scheme = await client.post(f"{base}/_soak/webhooks/probe", headers=headers,
                               json={"url": "file:///etc/passwd"})
    if "scheme not allowed" not in scheme.text:
        problems.append(f"file:// not refused: {scheme.text[:120]}")
    REPORT.add("webhook refuses internal targets", issue,
               "FAIL" if problems else "PASS", "; ".join(problems[:6]), name)

    # Controls and the other two rules, with the address check off.
    ok = await client.post(f"{base}/_soak/webhooks/probe", headers=headers,
                           json={"url": f"{IDP}/hook/{n}-ok", "allow_private": True})
    got = (await client.get(f"{IDP}/hooks/{n}-ok")).json().get("hits")
    REPORT.add("webhook delivers with allow_private_targets", issue,
               verdict(ok.json().get("delivered") is True and got == 1),
               f"{ok.text[:120]}, sink hits {got} (control)", name)
    red = await client.post(f"{base}/_soak/webhooks/probe", headers=headers, json={
        "url": f"{IDP}/redirect?to={IDP}/hook/{n}-redir", "allow_private": True})
    rhits = (await client.get(f"{IDP}/hooks/{n}-redir")).json().get("hits")
    REPORT.add("webhook does not follow redirects", issue,
               verdict(red.json().get("delivered") is False and not rhits),
               f"{red.text[:120]}, redirect target hits {rhits}", name)
    st = await client.post(f"{base}/_soak/webhooks/probe", headers=headers, json={
        "url": f"{IDP}/status/500?n={n}", "allow_private": True})
    REPORT.add("failed webhook keeps the status, not the body", issue,
               verdict("status 500" in st.text and "BODY-SECRET" not in st.text),
               st.text[:160], name)


# ------------------------------------------------------------- #1674


async def check_dbcache_long_keys(client, name, base, headers=None):
    """#1674 — keys over 255 bytes round-trip instead of colliding."""
    stem = "k" * 300
    pairs = [[f"{stem}-{RUN_ID}-a", "value-a"], [f"{stem}-{RUN_ID}-b", "value-b"],
             [f"short-{RUN_ID}", "value-short"]]
    r = await client.post(f"{base}/_soak/dbcache", headers=headers, json={"pairs": pairs})
    got = r.json().get("got") if r.status_code == 200 else None
    control = "" if name.endswith("-my") else " — control: this backend never truncated"
    REPORT.add("database cache keeps keys over 255 bytes apart", "#1674",
               verdict(got == ["value-a", "value-b", "value-short"]),
               f"{r.status_code} {str(r.json() if r.status_code == 200 else r.text)[:160]}{control}",
               name)


async def check_page_cache_x_org(client, name, base):
    """#1674 — on a shared Host, `X-Org` tenants get their own page."""
    shared = {"Host": "shared.example.test"}
    for _ in range(2):
        await client.get(f"{base}/shop/products", headers={**shared, "X-Org": "t01"})
    r = await client.get(f"{base}/shop/products", headers={**shared, "X-Org": "t02"})
    body = r.text
    if r.status_code != 200:
        REPORT.add("page cache keys on the X-Org tenant", "#1674", "FAIL",
                   f"X-Org t02 on a shared Host answered {r.status_code}: {body[:160]}", name)
        return
    REPORT.add("page cache keys on the X-Org tenant", "#1674",
               verdict("Catalogue — t02" in body and "Catalogue — t01" not in body),
               body[body.find("<h1>"):body.find("</h1>") + 5], name)


# ------------------------------------------------------------- #1759


async def check_bounded_dml(client, name, base, headers=None):
    """#1666 — `.limit(n)` bounds update() and delete()."""
    r = await client.post(f"{base}/_soak/dml/bounded", headers=headers)
    b = r.json() if r.status_code == 200 else {}
    ok = (b.get("updated") == 2 and b.get("deleted") == 1
          and b.get("left_prices") == [101, 102] and b.get("renamed_left") == 2)
    REPORT.add("bounded update/delete touch only `limit` rows", "#1666", verdict(ok),
               f"{r.status_code} {b or r.text[:200]}", name)


async def check_nested_atomic(client, name, base, headers=None):
    """#1666 — nested atomic() is a savepoint; on_commit waits for the top."""
    r = await client.post(f"{base}/_soak/dml/atomic", headers=headers)
    b = r.json() if r.status_code == 200 else {}
    REPORT.add("outer rollback undoes the inner atomic block", "#1666",
               verdict(b.get("outer_rollback_err") is True
                       and b.get("outer_rollback_kept_outer") is False
                       and b.get("outer_rollback_kept_inner") is False),
               f"{r.status_code} {b or r.text[:200]}", name)
    REPORT.add("inner rollback keeps the outer write", "#1666",
               verdict(b.get("inner_rollback_ok") is True
                       and b.get("inner_rollback_kept_outer") is True
                       and b.get("inner_rollback_kept_inner") is False),
               "(control: two transactions behave the same here)", name)
    REPORT.add("on_commit fires at the outermost commit", "#1666",
               verdict(b.get("on_commit_fired_before_outer_commit") == 0
                       and b.get("on_commit_fired_after") == 1),
               f"before={b.get('on_commit_fired_before_outer_commit')} "
               f"after={b.get('on_commit_fired_after')}", name)


# ------------------------------------------------------------- #1538


def b64url(raw: bytes) -> str:
    import base64
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def forge_jwt(claims: dict, key: str) -> str:
    import hashlib
    import hmac
    head = b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    body = b64url(json.dumps(claims).encode())
    sig = hmac.new(key.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return secret(f"{head}.{body}.{b64url(sig)}")


async def check_jwt_requires_exp(client, name, base, headers=None):
    """#1538 — `jwt::decode` refuses a token with no `exp`."""
    if len(SVC_SECRET) < 32:
        REPORT.add("JWT with no exp is refused", "#1538", "NOT-COVERED",
                   "SOAK_SERVICE_TOKEN_SECRET not given to the driver", name)
        return
    now = int(time.time())
    good = forge_jwt({"sub": "svc", "iat": now, "exp": now + 300}, SVC_SECRET)
    bare = forge_jwt({"sub": "svc", "iat": now}, SVC_SECRET)
    g = await client.get(f"{base}/_soak/service-token",
                         headers={**(headers or {}), "Authorization": f"Bearer {good}"})
    b = await client.get(f"{base}/_soak/service-token",
                         headers={**(headers or {}), "Authorization": f"Bearer {bare}"})
    REPORT.add("JWT with exp is accepted", "#1538", verdict(g.status_code == 200),
               f"{g.status_code} (control: proves the forged signature is right)", name)
    REPORT.add("JWT with no exp is refused", "#1538", verdict(b.status_code == 401),
               f"{b.status_code} {b.text[:100]}", name)


# ------------------------------------------------------------- #1636


async def check_storefront_escapes(client, name, base, headers=None):
    """#1636 — product text is escaped on the storefront."""
    # Sorts before every other SKU, newest first, so it is on page one.
    sku = f"0<b>{10**12 - int(time.time())}"
    r = await client.post(f"{base}/api/v1/products", headers=headers, json={
        "sku": sku, "name": '<script>alert("x")</script>', "blurb": None,
        "price_cents": 1, "active": True})
    if r.status_code not in (200, 201):
        REPORT.add("storefront escapes product text", "#1636", "FAIL",
                   f"could not create the product: {r.status_code} {r.text[:160]}", name)
        return
    page = await client.get(f"{base}/shop/products", headers=headers,
                            params={"fresh": time.time_ns()})
    body = page.text
    raw = "<script>alert" in body or "<b>" in body
    REPORT.add("storefront escapes product text", "#1636",
               verdict(page.status_code == 200 and not raw and "&lt;script&gt;" in body),
               f"{page.status_code}; raw markup {'present' if raw else 'absent'}", name)
    await client.patch(f"{base}/api/v1/products/{r.json().get('id')}", headers=headers,
                       json={"active": False})


# ------------------------------------------------------------ logins


async def admin_token(client, base, headers=None):
    r = await client.get(f"{base}/__admin/login", headers=headers)
    return secret(set_cookie_value(r, "rustango_csrf")), r


async def admin_login(client, base, user, password, ip=None, headers=None,
                      origin: str | None = "", token=None):
    """POST the bare admin login. `origin=None` sends no Origin."""
    if token is None:
        token, _ = await admin_token(client, base, headers)
    h = {**(headers or {}), "Cookie": f"rustango_csrf={token}"}
    if origin is not None:
        h["Origin"] = origin or origin_of(base, headers)
    if ip:
        h["X-Forwarded-For"] = ip
    return await client.post(f"{base}/__admin/login", headers=h, data={
        "_csrf": token, "username": user, "password": secret(password)})


async def tenant_login(client, base, host, user, password, extra=None, origin=""):
    extra = extra or {}
    r = await client.get(f"{base}/__login", headers={"Host": host, **extra})
    token = secret(set_cookie_value(r, "rustango_csrf"))
    h = {"Host": host, "Cookie": f"rustango_csrf={token}", **extra}
    if origin is not None:
        h["Origin"] = origin or f"http://{host}"
    resp = await client.post(f"{base}/__login", headers=h, data={
        "username": user, "password": secret(password), "next": "/__admin", "_csrf": token})
    return resp, secret(set_cookie_value(resp, SESSION_COOKIE)), token


async def console_login(client, base, user, password, origin=""):
    apex = {"Host": APEX}
    r = await client.get(f"{base}/login", headers=apex)
    token = secret(set_cookie_value(r, "rustango_csrf"))
    h = {**apex, "Cookie": f"rustango_csrf={token}"}
    if origin is not None:
        h["Origin"] = origin or f"http://{APEX}"
    resp = await client.post(f"{base}/login", headers=h, data={
        "username": user, "password": secret(password), "next": "/", "_csrf": token})
    return resp, secret(set_cookie_value(resp, OP_COOKIE)), token


async def jwt_login(client, base, host, user, password, ip=None, extra=None):
    h = {"Host": host, **(extra or {})}
    if ip:
        h["X-Forwarded-For"] = ip
    return await client.post(f"{base}/api/auth/login", headers=h,
                             json={"username": user, "password": secret(password)})


async def basic_auth(client, base, host, user, password, ip=None):
    import base64
    cred = base64.b64encode(f"{user}:{secret(password)}".encode()).decode()
    h = {"Host": host, "Authorization": f"Basic {cred}"}
    if ip:
        h["X-Forwarded-For"] = ip
    return await client.get(f"{base}/api/v1/account", headers=h)


def page_csrf(r) -> str | None:
    """The token a rendered form carries (`name="_csrf" value=...`)."""
    import re
    m = re.search(r'name="_csrf"\s+value="([^"]+)"', r.text) or \
        re.search(r'value="([^"]+)"\s+name="_csrf"', r.text)
    return secret(m.group(1)) if m else None


# ------------------------------------------------------ #1609 / #1732


async def lock_probe(attempt, known, surface, name, control=True):
    """Five failures lock a username, known or not, with the same 429."""
    issue = "#1609"
    finals = {}
    for who, (user, good) in (("known", known), ("unknown", (f"ghost-{RUN_ID}-{surface}", "x"))):
        first = await attempt(user, "wrong-0", fresh_ip())
        if is_refusal(first):
            REPORT.add(f"{surface}: five failures lock the username", issue, "NOT-COVERED",
                       f"{who} user {user} is still locked from an earlier run", name)
            return
        for i in range(1, 5):
            await attempt(user, f"wrong-{i}", fresh_ip())
        finals[who] = await attempt(user, good, fresh_ip())
    k, u = finals["known"], finals["unknown"]
    REPORT.add(f"{surface}: five failures lock the username", issue,
               verdict(is_refusal(k) and is_refusal(u)),
               f"known {k.status_code} Retry-After={k.headers.get('retry-after')}, "
               f"unknown {u.status_code} Retry-After={u.headers.get('retry-after')}", name)
    same = (k.status_code == u.status_code and k.text == u.text
            and bool(k.headers.get("retry-after")) == bool(u.headers.get("retry-after")))
    REPORT.add(f"{surface}: locked known and unknown users look the same", issue,
               verdict(same), "" if same else f"{k.text[:80]!r} vs {u.text[:80]!r}", name)
    if control:
        c = await attempt(f"fresh-{RUN_ID}-{surface}", "wrong", fresh_ip())
        REPORT.add(f"{surface}: another username is not locked", issue,
                   verdict(not is_refusal(c)), f"{c.status_code} (control)", name)


async def ip_limit_probe(attempt, known, surface, name, ip=None, tries=26):
    """Past the per-IP limit a login is refused, even with a right password."""
    ip = ip or fresh_ip()
    last = None
    for i in range(tries):
        last = await attempt(f"ipl-{RUN_ID}-{surface}-{i}", "wrong", ip)
        if is_refusal(last):
            break
    if not is_refusal(last):
        REPORT.add(f"{surface}: per-IP login limit", "#1609", "FAIL",
                   f"no 429 after {tries} failures from one IP", name)
        return
    k = await attempt(known[0], known[1], ip)
    REPORT.add(f"{surface}: per-IP login limit", "#1609",
               verdict(is_refusal(k) and k.status_code == last.status_code),
               f"refused after {i + 1} attempt(s); right password then got {k.status_code}", name)


async def check_admin_login_limits(client, name, base):
    """#1609 — the bare admin login: account lock and per-IP limit."""
    async def attempt(user, password, ip):
        return await admin_login(client, base, user, password, ip=ip)

    await lock_probe(attempt, ("lockprobe", os.environ.get("SOAK_LOCKPROBE_PASSWORD",
                                                           "soak-lockprobe-pw")),
                     "admin login", name)
    await ip_limit_probe(attempt, ADMIN_USER, "admin login", name)


async def check_admin_global_limit(client):
    """#1609 — the admin scope's global ceiling (30 on the edge)."""
    name, base = EDGE_SINGLE
    n, refused = 0, None
    for _ in range(4):
        ip = fresh_ip()
        for _ in range(10):
            r = await admin_login(client, base, f"glob-{RUN_ID}-{n}", "wrong", ip=ip)
            n += 1
            if is_refusal(r):
                refused = r
                break
        if refused is not None:
            break
    if refused is None:
        REPORT.add("admin login: global limit", "#1609", "FAIL",
                   f"no 429 after {n} failures from 4 IPs (limit 30)", name)
        return
    k = await admin_login(client, base, *ADMIN_USER, ip=fresh_ip())
    REPORT.add("admin login: global limit", "#1609",
               verdict(n <= 31 and is_refusal(k)),
               f"refused at attempt {n}; a fresh IP with the right password got {k.status_code}",
               name)


async def check_tenancy_login_limits(client, name, base):
    """#1609 — operator console, tenant (JWT and form), HTTP Basic.

    The tenancy server has no RealIp hook, so its forms share the
    driver's IP bucket: the console lock (10 failures) plus the per-IP
    probe fit one 20-failure window. JWT and Basic are on the API router,
    behind RealIpLayer, so each attempt gets its own address.
    """
    host = tenant_host(4)
    lock_user = ("lockprobe", pw("lockprobe"))

    async def jwt(user, password, ip):
        return await jwt_login(client, base, host, user, password, ip=ip)

    await lock_probe(jwt, lock_user, "tenant JWT login", name)
    # Same scope and user table: the lock JWT set holds on the form too.
    form, _, _ = await tenant_login(client, base, host, *lock_user)
    REPORT.add("tenant form login shares the JWT lock", "#1609", verdict(is_refusal(form)),
               f"{form.status_code} Retry-After={form.headers.get('retry-after')}", name)

    async def basic(user, password, ip):
        return await basic_auth(client, base, host, user, password, ip=ip)

    await lock_probe(basic, lock_user, "HTTP Basic", name)
    ip = fresh_ip()
    await ip_limit_probe(jwt, ("jwtuser", pw("jwtuser")), "tenant JWT login", name, ip=ip)
    b = await basic_auth(client, base, tenant_host(6), "jwtuser", pw("jwtuser"), ip=ip)
    REPORT.add("HTTP Basic is refused from a throttled IP", "#1609", verdict(is_refusal(b)),
               f"{b.status_code}", name)

    async def console(user, password, ip):
        r, _, _ = await console_login(client, base, user, password)
        return r

    await lock_probe(console, ("lockops", pw("lockops")), "operator console login", name,
                     control=False)
    await ip_limit_probe(console, OPERATOR, "operator console login", name)


async def check_lock_duration_setting(client):
    """#1609 — `lockout_duration_secs` takes effect (20 s on the edge)."""
    name, base = EDGE_SAAS
    host = tenant_host(4)
    user = ("lockprobe", pw("lockprobe"))
    for i in range(5):
        await jwt_login(client, base, host, user[0], f"wrong-{i}", ip=fresh_ip(), extra=TLS)
    locked = await jwt_login(client, base, host, *user, ip=fresh_ip(), extra=TLS)
    ra = locked.headers.get("retry-after")
    REPORT.add("lockout_duration_secs sets the lock length", "#1609",
               verdict(is_refusal(locked) and ra is not None and int(ra) <= 20),
               f"{locked.status_code} Retry-After={ra} (configured 20, default 900)", name)
    await asyncio.sleep(22)
    after = await jwt_login(client, base, host, *user, ip=fresh_ip(), extra=TLS)
    REPORT.add("the lock ends when lockout_duration_secs says", "#1609",
               verdict(after.status_code == 200), f"{after.status_code} after 22 s", name)


async def check_hash_queue_bounded(client):
    """#1732 — a full hash queue answers 503, the same for any user."""
    name, base = EDGE_SAAS
    host = tenant_host(6)

    async def one(user, password):
        return await jwt_login(client, base, host, user, password, ip=fresh_ip(), extra=TLS)

    tasks = [one("jwtuser", pw("jwtuser")) for _ in range(30)]
    tasks += [one(f"nobody-{RUN_ID}-{i}", "x") for i in range(30)]
    res = await asyncio.gather(*tasks, return_exceptions=True)
    known = [r for r in res[:30] if not isinstance(r, Exception)]
    unknown = [r for r in res[30:] if not isinstance(r, Exception)]
    busy_k = [r for r in known if r.status_code == 503 and r.headers.get("retry-after")]
    busy_u = [r for r in unknown if r.status_code == 503 and r.headers.get("retry-after")]
    other5 = [r.status_code for r in known + unknown if r.status_code >= 500 and r.status_code != 503]
    REPORT.add("busy hash queue answers 503 + Retry-After", "#1732",
               verdict(bool(busy_k) and bool(busy_u) and not other5),
               f"503s: known {len(busy_k)}/30, unknown {len(busy_u)}/30; other 5xx {other5}",
               name)
    if busy_k and busy_u:
        REPORT.add("busy 503 is the same for known and unknown users", "#1732",
                   verdict(busy_k[0].text == busy_u[0].text), busy_k[0].text[:100], name)


async def check_hash_off_runtime(client, name, base):
    """#1709 — logins hash off the runtime, so other requests stay fast.

    200 right-password admin logins at once (10 per IP, under every
    limit) while `/_soak/info` is polled. Argon2 on a Tokio worker used
    to hold that worker for the whole hash.
    """
    # Its own client: the storm fills the shared one's connection pool,
    # and a poll queued there would time the driver, not the server.
    poller = httpx.AsyncClient(timeout=30)

    async def poll(stop, out):
        while not stop.is_set():
            t = time.perf_counter()
            await poller.get(f"{base}/_soak/info")
            out.append(time.perf_counter() - t)
            await asyncio.sleep(0.02)

    base_lat: list[float] = []
    stop = asyncio.Event()
    p = asyncio.create_task(poll(stop, base_lat))
    await asyncio.sleep(1.0)
    stop.set()
    await p
    ips = [fresh_ip() for _ in range(20)]
    token, _ = await admin_token(client, base)
    during: list[float] = []
    stop = asyncio.Event()
    p = asyncio.create_task(poll(stop, during))
    res = await asyncio.gather(*[admin_login(client, base, *ADMIN_USER, ip=ips[i % 20],
                                             token=token) for i in range(200)],
                               return_exceptions=True)
    stop.set()
    await p
    await poller.aclose()
    ok = sum(1 for r in res if not isinstance(r, Exception) and r.status_code == 303)
    during.sort()
    p95 = during[int(len(during) * 0.95) - 1] if during else 0
    worst = during[-1] if during else 0
    b95 = sorted(base_lat)[int(len(base_lat) * 0.95) - 1] if base_lat else 0
    REPORT.add("hashing leaves the runtime free", "#1709",
               verdict(ok >= 150 and worst < 0.5),
               f"{ok}/200 logins ok; /_soak/info p95 {p95 * 1000:.0f} ms, worst "
               f"{worst * 1000:.0f} ms over {len(during)} polls (idle p95 {b95 * 1000:.0f} ms)",
               name)


# ---------------------------------------------------------- #1338


async def change_tenant_password(client, base, host, cookie, token, old, new):
    h = {"Host": host, "Origin": f"http://{host}",
         "Cookie": f"rustango_csrf={token}; {SESSION_COOKIE}={cookie}"}
    return await client.post(f"{base}/__change-password", headers=h, data={
        "_csrf": token, "current_password": secret(old), "new_password": secret(new),
        "confirm_password": new})


async def check_password_change_tenant(client, name, base):
    """#1338 — a password change ends sessions issued the same second."""
    host = tenant_host(5)
    old, new = pw("pwchange"), pw("pwchange") + "-2"
    same_second, a_cookie, changed = False, None, None
    for _ in range(3):
        start = time.time()
        _, a_cookie, _ = await tenant_login(client, base, host, "pwchange", old)
        _, b_cookie, token = await tenant_login(client, base, host, "pwchange", old)
        if not (a_cookie and b_cookie):
            break
        changed = await change_tenant_password(client, base, host, b_cookie, token, old, new)
        same_second = int(start) == int(time.time())
        if changed.status_code in (200, 303) and same_second:
            break
        if changed.status_code in (200, 303):
            # Changed but not within one second: put it back and retry.
            _, c, t = await tenant_login(client, base, host, "pwchange", new)
            await change_tenant_password(client, base, host, c, t, new, old)
    if not a_cookie or changed is None or changed.status_code not in (200, 303):
        REPORT.add("password change ends same-second tenant sessions", "#1338", "FAIL",
                   f"could not log in or change the password: "
                   f"{getattr(changed, 'status_code', None)}", name)
        return
    admin = await client.get(f"{base}/__admin", headers={
        "Host": host, "Cookie": f"{SESSION_COOKIE}={a_cookie}"})
    who = await client.get(f"{base}/_soak/whoami", headers={
        "Host": host, "Cookie": f"{SESSION_COOKIE}={a_cookie}"})
    ended = admin.status_code in (302, 303) and who.json().get("user") is None
    REPORT.add("password change ends same-second tenant sessions", "#1338", verdict(ended),
               f"old session: admin {admin.status_code}, SessionUser {who.json().get('user')}; "
               f"same second: {same_second}", name)
    _, c, t = await tenant_login(client, base, host, "pwchange", new)
    back = await change_tenant_password(client, base, host, c, t, new, old)
    if back.status_code not in (200, 303):
        print(f"  !! could not restore pwchange's password on {name}: {back.status_code}")


async def check_password_change_operator(client, name, base):
    """#1338 — the operator console honours a password change too."""
    old, new = pw("pwops"), pw("pwops") + "-2"
    _, a, _ = await console_login(client, base, "pwops", old)
    _, b, _ = await console_login(client, base, "pwops", old)
    if not (a and b):
        REPORT.add("password change ends older operator sessions", "#1338", "FAIL",
                   "could not sign pwops in", name)
        return

    async def change(cookie, cur, nxt):
        page = await client.get(f"{base}/change-password", headers={
            "Host": APEX, "Cookie": f"{OP_COOKIE}={cookie}"})
        tok = page_csrf(page) or set_cookie_value(page, "rustango_csrf")
        return await client.post(f"{base}/change-password", headers={
            "Host": APEX, "Origin": f"http://{APEX}",
            "Cookie": f"rustango_csrf={tok}; {OP_COOKIE}={cookie}"}, data={
            "_csrf": tok, "current_password": secret(cur), "new_password": secret(nxt),
            "confirm_password": nxt})

    ch = await change(b, old, new)
    home = await client.get(f"{base}/", headers={"Host": APEX, "Cookie": f"{OP_COOKIE}={a}"})
    REPORT.add("password change ends older operator sessions", "#1338",
               verdict(ch.status_code in (200, 303) and home.status_code in (302, 303)),
               f"change {ch.status_code}; old session then got {home.status_code}", name)
    _, c, _ = await console_login(client, base, "pwops", new)
    if c:
        await change(c, new, old)


# ------------------------------------------- bare admin (single app)


async def check_bare_admin(client, name, base):
    """#1711, #1693, #1695, #1699 on the bare admin's login."""
    first = await client.get(f"{base}/__admin/commerce_product")
    login = await client.get(f"{base}/__admin/login")
    n1, n2 = cookie_count(first, "rustango_csrf"), cookie_count(login, "rustango_csrf")
    REPORT.add("admin sets one CSRF cookie on a first visit", "#1711",
               verdict(n1 == 1 and n2 == 1),
               f"protected page {first.status_code}: {n1} cookie(s); login page: {n2}", name)
    xfo = login.headers.get("x-frame-options")
    REPORT.add("admin login carries security headers", "#1699", verdict(bool(xfo)),
               f"X-Frame-Options: {xfo}", name)
    token = set_cookie_value(login, "rustango_csrf")

    def refused(r):
        # The admin login re-renders its form on a CSRF failure; the
        # password is right, so any session cookie means it got through.
        return (r.status_code in (200, 403) and not set_cookie_value(r, ADMIN_COOKIE)
                and (r.status_code == 403 or "form was invalid" in r.text))

    empty = await client.post(f"{base}/__admin/login", headers={
        "Cookie": "rustango_csrf=", "Origin": origin_of(base, None),
        "X-Forwarded-For": fresh_ip()},
        data={"_csrf": "", "username": ADMIN_USER[0], "password": ADMIN_USER[1]})
    REPORT.add("CSRF refuses an empty token pair", "#1693", verdict(refused(empty)),
               f"admin login: {empty.status_code}, session set: "
               f"{bool(set_cookie_value(empty, ADMIN_COOKIE))}", name)
    foreign = await admin_login(client, base, *ADMIN_USER, origin="http://evil.example",
                                token=token, ip=fresh_ip())
    REPORT.add("admin login refuses a foreign Origin", "#1695", verdict(refused(foreign)),
               f"{foreign.status_code}, session set: "
               f"{bool(set_cookie_value(foreign, ADMIN_COOKIE))}", name)
    ok = await admin_login(client, base, *ADMIN_USER, token=token, ip=fresh_ip())
    session = secret(set_cookie_value(ok, ADMIN_COOKIE))
    REPORT.add("admin login signs a superuser in", "#1609",
               verdict(ok.status_code == 303 and bool(session)),
               f"{ok.status_code} (control for the admin login checks)", name)
    return session


async def check_admin_inlines(client, name, base, session_cookie, headers=None,
                              admin="/__admin"):
    """#1667 — an inline row from another order is refused, not moved."""
    issue = "#1667"
    h = headers or {}
    cu = await client.post(f"{base}/api/v1/customers", headers=h, json={
        "contact_email": f"inline-{RUN_ID}-{time.time_ns()}@example.test",
        "full_name": "Inline", "loyalty_tier": "bronze"})
    ids = {}
    try:
        cid = cu.json()["id"]
        for k in ("o1", "o2"):
            o = await client.post(f"{base}/api/v1/orders-raw", headers=h, json={
                "reference": f"INL-{k}-{time.time_ns() % 10**12}", "customer_id": cid,
                "status": "pending", "total_cents": 100, "note": None})
            ids[k] = o.json()["id"]
            p = await client.post(f"{base}/api/v1/products", headers=h, json={
                "sku": f"INL-{k}-{time.time_ns()}", "name": "inline", "blurb": None,
                "price_cents": 1, "active": False})
            ids[f"p{k[1]}"] = p.json()["id"]
        for k, o, p in (("l1", "o1", "p1"), ("l2", "o2", "p2")):
            ln = await client.post(f"{base}/api/v1/order-lines", headers=h, json={
                "order_id": ids[o], "product_id": ids[p], "quantity": 1, "unit_price_cents": 1})
            ids[k] = ln.json()["id"]
    except Exception as e:  # noqa: BLE001
        REPORT.add("admin inline rows stay under their parent", issue, "FAIL",
                   f"could not build the fixture: {e!r}", name)
        return
    page = await client.get(f"{base}{admin}/commerce_order/{ids['o1']}/edit", headers={
        **h, "Cookie": f"{ADMIN_COOKIE if not h else SESSION_COOKIE}={session_cookie}"})
    token = page_csrf(page) or set_cookie_value(page, "rustango_csrf")
    cookie = (f"rustango_csrf={token}; "
              f"{ADMIN_COOKIE if not h else SESSION_COOKIE}={session_cookie}")
    order = await client.get(f"{base}/api/v1/orders-raw/{ids['o1']}", headers=h)
    parent = {k: ("" if v is None else str(v)) for k, v in order.json().items()
              if k not in ("id", "placed_at")}
    # NULL, as a browser form sends it. (SQLite's JSON reads NULL as 0.)
    parent.update({"assigned_picker_id": "", "note": ""})

    def form(line, product, qty):
        return {**parent, "_csrf": token,
                "commerce_order_line-TOTAL_FORMS": "1",
                "commerce_order_line-INITIAL_FORMS": "1",
                "commerce_order_line-MAX_NUM_FORMS": "",
                "commerce_order_line-0-id": str(line),
                "commerce_order_line-0-product_id": str(product),
                "commerce_order_line-0-quantity": str(qty),
                "commerce_order_line-0-unit_price_cents": "1"}

    post_h = {**h, "Cookie": cookie, "Origin": origin_of(base, headers)}
    own = await client.post(f"{base}{admin}/commerce_order/{ids['o1']}", headers=post_h,
                            data=form(ids["l1"], ids["p1"], 7))
    l1 = (await client.get(f"{base}/api/v1/order-lines/{ids['l1']}", headers=h)).json()
    REPORT.add("admin inline edits its own rows", issue,
               verdict(own.status_code in (302, 303) and l1.get("quantity") == 7),
               f"{own.status_code}, line quantity {l1.get('quantity')} (control)", name)
    hij = await client.post(f"{base}{admin}/commerce_order/{ids['o1']}", headers=post_h,
                            data=form(ids["l2"], ids["p2"], 999))
    l2 = (await client.get(f"{base}/api/v1/order-lines/{ids['l2']}", headers=h)).json()
    REPORT.add("admin inline rows stay under their parent", issue,
               verdict(hij.status_code == 404 and l2.get("quantity") == 1
                       and l2.get("order_id") == ids["o2"]),
               f"{hij.status_code}; other order's line now quantity {l2.get('quantity')}, "
               f"order {l2.get('order_id')}", name)


# --------------------------------------------------- tenancy surfaces


async def check_tenant_admin_writes(client, name, base):
    """#1713, #1529, #1693 — every tenant admin write needs the token."""
    host = tenant_host(7)
    r, session, _ = await tenant_login(client, base, host, "csrfadmin", pw("csrfadmin"))
    if not session:
        REPORT.add("tenant admin writes need the CSRF token", "#1713", "FAIL",
                   f"could not sign csrfadmin in: {r.status_code}", name)
        return
    s = {"Host": host, "Cookie": f"{SESSION_COOKIE}={session}"}
    first = await client.get(f"{base}/__admin/commerce_product/new", headers=s)
    REPORT.add("tenant admin sets one CSRF cookie", "#1711",
               verdict(cookie_count(first, "rustango_csrf") <= 1),
               f"{cookie_count(first, 'rustango_csrf')} cookie(s)", name)
    xfo = first.headers.get("x-frame-options")
    REPORT.add("tenant admin pages carry security headers", "#1699", verdict(bool(xfo)),
               f"X-Frame-Options: {xfo}", name)
    token = page_csrf(first) or set_cookie_value(first, "rustango_csrf")
    good = {**s, "Cookie": f"rustango_csrf={token}; {SESSION_COOKIE}={session}",
            "Origin": f"http://{host}"}
    bare = {**s, "Origin": f"http://{host}"}
    prod = {"sku": f"TADM-{RUN_ID}-{time.time_ns() % 10**9}", "name": "tenant admin",
            "description": "", "price_cents": "1", "active": "on"}
    made = await client.post(f"{base}/__admin/commerce_product", headers=good,
                             data={**prod, "_csrf": token})
    loc = made.headers.get("location", "")
    REPORT.add("tenant admin create with the token works", "#1713",
               verdict(made.status_code in (302, 303)), f"{made.status_code} {loc} (control)",
               name)
    pk = loc.rstrip("/").split("/")[-1] if loc else "1"
    writes = {
        "create": ("/__admin/commerce_product", prod),
        "update": (f"/__admin/commerce_product/{pk}", prod),
        "delete": (f"/__admin/commerce_product/{pk}/delete", {}),
        "bulk action": ("/__admin/commerce_product/__action",
                        {"action": "delete_selected", "_selected": pk}),
        "change password": ("/__change-password", {
            "current_password": pw("csrfadmin"), "new_password": "x" * 12,
            "confirm_password": "x" * 12}),
        "logout": ("/__logout", {}),
    }
    bad = []
    for label, (path, data) in writes.items():
        rr = await client.post(f"{base}{path}", headers=bare, data=data)
        if rr.status_code != 403:
            bad.append(f"{label} {rr.status_code}")
    REPORT.add("tenant admin writes need the CSRF token", "#1713",
               "FAIL" if bad else "PASS",
               f"answered without a token: {bad}" if bad else f"{len(writes)} writes refused",
               name)
    foreign = await client.post(f"{base}/__admin/commerce_product", headers={
        **good, "Origin": "http://evil.example"}, data={**prod, "_csrf": token})
    REPORT.add("valid token from a foreign Origin is refused", "#1529",
               verdict(foreign.status_code == 403), f"tenant admin: {foreign.status_code}", name)
    empty = await client.post(f"{base}/__admin/commerce_product", headers={
        **s, "Cookie": f"rustango_csrf=; {SESSION_COOKIE}={session}",
        "Origin": f"http://{host}"}, data={**prod, "_csrf": ""})
    REPORT.add("CSRF refuses an empty token pair", "#1693", verdict(empty.status_code == 403),
               f"tenant admin: {empty.status_code}", name)
    still = await client.get(f"{base}/__admin", headers=s)
    REPORT.add("a logout POST without the token does not log out", "#1713",
               verdict(still.status_code == 200), f"{still.status_code}", name)


CONSOLE_POSTS = [
    "/logout", "/change-password", "/sso-shared", "/sso-shared/999999/delete",
    "/sso-shared/999999/email-link", "/operators", "/orgs/prewarm",
    "/operators/999999/active", "/operators/999999/reset-password",
    "/orgs/no-such-tenant/edit", "/orgs/no-such-tenant/edit/branding",
    "/orgs/no-such-tenant/deactivate", "/orgs/no-such-tenant/purge",
    "/orgs/no-such-tenant/test-connection", "/orgs/no-such-tenant/hosts/add",
    "/orgs/no-such-tenant/hosts/remove", "/orgs/no-such-tenant/hosts/toggle",
    "/orgs/no-such-tenant/impersonate", "/orgs/new", "/orgs/test-connection",
    "/orgs/migrate", "/orgs/no-such-tenant/migrate",
]


async def check_console(client, name, base):
    """#1710 and the console legs of #1695, #1699, #1693, #1663."""
    apex = {"Host": APEX}
    page = await client.get(f"{base}/login", headers=apex)
    REPORT.add("console login carries security headers", "#1699",
               verdict(bool(page.headers.get("x-frame-options"))),
               f"X-Frame-Options: {page.headers.get('x-frame-options')}", name)
    token = set_cookie_value(page, "rustango_csrf")
    nof = await client.post(f"{base}/login", headers={**apex, "Origin": f"http://{APEX}"},
                            data={"username": OPERATOR[0], "password": OPERATOR[1]})
    REPORT.add("console login needs the CSRF token", "#1710",
               verdict(nof.status_code == 403), f"{nof.status_code}", name)
    empty = await client.post(f"{base}/login", headers={
        **apex, "Origin": f"http://{APEX}", "Cookie": "rustango_csrf="},
        data={"username": OPERATOR[0], "password": OPERATOR[1], "_csrf": ""})
    REPORT.add("CSRF refuses an empty token pair", "#1693", verdict(empty.status_code == 403),
               f"console login: {empty.status_code}", name)
    foreign = await client.post(f"{base}/login", headers={
        **apex, "Origin": "http://evil.example", "Cookie": f"rustango_csrf={token}"},
        data={"username": OPERATOR[0], "password": OPERATOR[1], "_csrf": token})
    REPORT.add("console login refuses a foreign Origin", "#1695",
               verdict(foreign.status_code == 403), f"{foreign.status_code}", name)
    nxt = await client.get(f"{base}/orgs", headers=apex)
    loc = nxt.headers.get("location", "")
    REPORT.add("console login redirect encodes `next`", "#1663",
               verdict("next=%2Forgs" in loc), loc, name)

    r, session, _ = await console_login(client, base, *OPERATOR)
    if not session:
        REPORT.add("every console POST needs the CSRF token", "#1710", "FAIL",
                   f"could not sign soakops in: {r.status_code}", name)
        return
    s = {**apex, "Cookie": f"{OP_COOKIE}={session}", "Origin": f"http://{APEX}"}
    home = await client.get(f"{base}/", headers=s)
    REPORT.add("console pages carry security headers", "#1699",
               verdict(home.status_code == 200 and bool(home.headers.get("x-frame-options"))),
               f"{home.status_code} X-Frame-Options: {home.headers.get('x-frame-options')}",
               name)
    reached, mounted = [], 0
    for path in CONSOLE_POSTS:
        rr = await client.post(f"{base}{path}", headers=s, data={"slug": "no-such-tenant"})
        if rr.status_code in (404, 405):
            continue
        mounted += 1
        if rr.status_code != 403:
            reached.append(f"{path} {rr.status_code}")
    REPORT.add("every console POST needs the CSRF token", "#1710",
               "FAIL" if reached else "PASS",
               f"reached a handler without a token: {reached}" if reached
               else f"{mounted} mounted POST routes refused", name)
    tok = page_csrf(home) or set_cookie_value(home, "rustango_csrf") or token
    ok = await client.post(f"{base}/orgs/prewarm", headers={
        **s, "Cookie": f"rustango_csrf={tok}; {OP_COOKIE}={session}"}, data={"_csrf": tok})
    REPORT.add("console POST with the token goes through", "#1710",
               verdict(ok.status_code in (200, 302, 303)), f"prewarm {ok.status_code} (control)",
               name)
    still = await client.get(f"{base}/", headers=s)
    REPORT.add("a console logout without the token does not log out", "#1710",
               verdict(still.status_code == 200), f"{still.status_code}", name)


async def check_hosts_and_settings(client, name, base):
    """#1700, #1702, #1663 on the tenancy server."""
    bad = []
    for path in ("/__login", "/__admin", "/login", "/api/v1/products", "/shop/products",
                 "/_soak/info"):
        r = await client.get(f"{base}{path}", headers={"Host": "evil.example"})
        if r.status_code != 400:
            bad.append(f"{path} {r.status_code}")
    REPORT.add("unlisted Host is 400 on every tenancy route", "#1700",
               "FAIL" if bad else "PASS", f"not refused: {bad}" if bad else "6 routes", name)
    login = await client.get(f"{base}/__login", headers={"Host": tenant_host(1)})
    hsts = login.headers.get("strict-transport-security", "")
    REPORT.add("tenant server applies the settings tier", "#1702",
               verdict("max-age=31536000" in hsts and login.status_code == 200),
               f"HSTS {hsts!r} (from prod_settings.toml); allowed_hosts also enforced", name)
    adm = await client.get(f"{base}/__admin/commerce_product?q=1",
                           headers={"Host": tenant_host(1)})
    loc = adm.headers.get("location", "")
    tail = loc.split("next=", 1)[-1] if "next=" in loc else ""
    REPORT.add("tenant login redirect encodes `next`", "#1663",
               verdict(tail.startswith("%2F__admin%2Fcommerce_product") and "/" not in tail),
               loc, name)


async def check_tls_redirect(client):
    """#1700 — the HTTPS redirect covers login, admin and console."""
    name, base = EDGE_SAAS
    bad = []
    for host, path in ((tenant_host(1), "/__login"), (tenant_host(1), "/__admin"),
                       (APEX, "/login"), (tenant_host(1), "/api/v1/products")):
        r = await client.get(f"{base}{path}", headers={"Host": host})
        loc = r.headers.get("location", "")
        if r.status_code not in (301, 308) or not loc.startswith(f"https://{host}"):
            bad.append(f"{host}{path} {r.status_code} {loc}")
    REPORT.add("plain HTTP is redirected on every tenancy route", "#1700",
               "FAIL" if bad else "PASS", "; ".join(bad), name)
    evil = await client.get(f"{base}/__login", headers={"Host": "evil.example"})
    REPORT.add("bad Host is refused before the redirect", "#1700",
               verdict(evil.status_code == 400), f"{evil.status_code}", name)
    no_origin = await client.post(f"{base}/__login", headers={
        "Host": tenant_host(1), **TLS, "Cookie": "rustango_csrf=abc"},
        data={"_csrf": "abc", "username": "x", "password": "y"})
    REPORT.add("over TLS a login POST without Origin is refused", "#1695",
               verdict(no_origin.status_code == 403), f"{no_origin.status_code}", name)


async def check_tenant_session_misc(client, name, base):
    """Session extractors on every backend; #1663's cookie reader; #1693."""
    host = tenant_host(1)
    token = await login_form(client, base, host)
    empty = await client.post(f"{base}/__login", headers={
        "Host": host, "Origin": f"http://{host}", "Cookie": "rustango_csrf="},
        data={"username": TENANT_ADMIN[0], "password": TENANT_ADMIN[1], "_csrf": ""})
    REPORT.add("CSRF refuses an empty token pair", "#1693", verdict(empty.status_code == 403),
               f"tenant login: {empty.status_code}", name)
    r = await post_login(client, base, host, token, origin=f"http://{host}")
    session = secret(set_cookie_value(r, SESSION_COOKIE))
    if not session:
        REPORT.add("SessionUser resolves the signed-in user", "—", "FAIL",
                   f"tenant login failed: {r.status_code}", name)
        return
    who = await client.get(f"{base}/_soak/whoami", headers={
        "Host": host, "Cookie": f"{SESSION_COOKIE}={session}"})
    anon = await client.get(f"{base}/_soak/whoami", headers={"Host": host})
    control = "" if name == "saas-pg" else \
        " — control: the bug needs a hand-built non-Postgres server with the postgres feature on"
    REPORT.add("SessionUser resolves the signed-in user", "session extractors",
               verdict(who.json().get("user") == TENANT_ADMIN[0]
                       and anon.json().get("user") is None),
               f"{who.json()} / anonymous {anon.json()}{control}", name)
    first = await client.get(f"{base}/__admin", headers={
        "Host": host, "Cookie": f"{SESSION_COOKIE}={session}; {SESSION_COOKIE}=junk"})
    junk = await client.get(f"{base}/__admin", headers={
        "Host": host, "Cookie": f"{SESSION_COOKIE}=junk; {SESSION_COOKIE}={session}"})
    REPORT.add("a repeated cookie resolves to the first one", "#1663",
               verdict(first.status_code == 200 and junk.status_code in (302, 303)),
               f"valid first {first.status_code}, junk first {junk.status_code}", name)


# ------------------------------------------------------------- JWT


async def check_jwt(client, name, base):
    """#1190 revocation and tenant binding; #1672 single-use refresh."""
    host = tenant_host(6)
    r = await jwt_login(client, base, host, "jwtuser", pw("jwtuser"), ip=fresh_ip())
    if r.status_code != 200:
        REPORT.add("JWT login", "#1190", "FAIL", f"{r.status_code} {r.text[:160]}", name)
        return
    pair = r.json()
    access, refresh = secret(pair["access"]), secret(pair["refresh"])
    me = await client.get(f"{base}/api/auth/me",
                          headers={"Host": host, "Authorization": f"Bearer {access}"})
    other = await client.get(f"{base}/api/auth/me", headers={
        "Host": tenant_host(1), "Authorization": f"Bearer {access}"})
    REPORT.add("JWT is bound to its tenant", "#1190",
               verdict(me.status_code == 200 and other.status_code == 401),
               f"own tenant {me.status_code}, t01 {other.status_code}", name)
    race = await asyncio.gather(*[client.post(f"{base}/api/auth/refresh", headers={
        "Host": host}, json={"refresh": refresh}) for _ in range(8)])
    wins = [x for x in race if x.status_code == 200]
    REPORT.add("a refresh token redeems once under concurrency", "#1672",
               verdict(len(wins) == 1),
               f"{len(wins)} of 8 concurrent refreshes succeeded: "
               f"{sorted(x.status_code for x in race)}", name)
    if not wins:
        return
    new = wins[0].json()
    secret(new.get("refresh"))
    again = await client.post(f"{base}/api/auth/refresh", headers={"Host": host},
                              json={"refresh": refresh})
    REPORT.add("a used refresh token is refused", "#1672", verdict(again.status_code == 401),
               f"{again.status_code}", name)
    a2 = secret(new["access"])
    out = await client.post(f"{base}/api/auth/logout",
                            headers={"Host": host, "Authorization": f"Bearer {a2}"})
    after = await client.get(f"{base}/api/auth/me",
                             headers={"Host": host, "Authorization": f"Bearer {a2}"})
    REPORT.add("a logged-out access token is refused", "#1190",
               verdict(out.status_code in (200, 204) and after.status_code == 401),
               f"logout {out.status_code}, then me {after.status_code}", name)
    basic = await basic_auth(client, base, host, "jwtuser", pw("jwtuser"), ip=fresh_ip())
    REPORT.add("HTTP Basic signs a tenant user in", "#1609",
               verdict(basic.status_code == 200 and basic.json().get("user") == "jwtuser"),
               f"{basic.status_code} (control for the Basic limit checks)", name)


# ------------------------------------------------------------- SSO


async def sso_attempt(client, base, provider, sub, email, verified=True):
    """One tenant SSO handshake against the fake IdP; the callback response."""
    host = tenant_host(3)
    begin = await client.get(f"{base}/__login/sso/{provider}", headers={"Host": host})
    loc = begin.headers.get("location", "")
    flow = set_cookie_value(begin, "rustango_admin_sso_flow")
    if "/authorize" not in loc or not flow:
        return None, f"begin {begin.status_code} {loc[:120]}"
    from urllib.parse import parse_qs, urlencode, urlparse
    q = {k: v[0] for k, v in parse_qs(urlparse(loc).query).items()}
    q.update({"sub": sub, "email": email, "email_verified": "true" if verified else "false"})
    auth = await client.get(f"{IDP}/authorize?{urlencode(q)}")
    back = urlparse(auth.headers.get("location", ""))
    bq = {k: v[0] for k, v in parse_qs(back.query).items()}
    secret(bq.get("code"))
    cb = await client.get(f"{base}{back.path}?{back.query}", headers={
        "Host": host, "Cookie": f"rustango_admin_sso_flow={flow}"})
    return cb, ""


async def check_sso(client, name, base):
    """GHSA-3qpg (#1760) — SSO links by subject; email linking is opt-in."""
    issue = "#1760"
    seed = await client.post(f"{base}/_soak/sso/seed", headers={"Host": tenant_host(3)})
    if seed.status_code != 200:
        REPORT.add("SSO fixture", issue, "FAIL", f"seed {seed.status_code} {seed.text[:200]}",
                   name)
        return
    mail = {u: v["email"] for u, v in seed.json()["users"].items()}
    tag = f"{RUN_ID}-{time.time_ns() % 10**9}"

    def outcome(cb):
        if cb is None:
            return "no handshake", None
        return cb.headers.get("location", ""), set_cookie_value(cb, SESSION_COOKIE)

    cases = [
        ("email match without opt-in is refused", "idp-strict", f"s-{tag}-a", mail["sso-user"],
         True, False),
        ("email match with opt-in links a normal user", "idp-link", f"s-{tag}-b",
         mail["sso-user"], True, True),
        ("a linked subject signs in whatever the email says", "idp-link", f"s-{tag}-b",
         mail["sso-other"], True, True),
        ("email linking never reaches a superuser", "idp-link", f"s-{tag}-c", mail["sso-super"],
         True, False),
        ("email linking never reaches a staff user", "idp-link", f"s-{tag}-d", mail["sso-staff"],
         True, False),
        ("an unverified email never links", "idp-link", f"s-{tag}-e", mail["sso-other"],
         False, False),
    ]
    for label, prov, sub, email, verified, should in cases:
        cb, why = await sso_attempt(client, base, prov, sub, email, verified)
        loc, session = outcome(cb)
        secret(session)
        who = None
        if session:
            w = await client.get(f"{base}/_soak/whoami", headers={
                "Host": tenant_host(3), "Cookie": f"{SESSION_COOKIE}={session}"})
            who = w.json().get("user")
        ok = (who == "sso-user") if should else (session is None and "sso_error=" in loc)
        REPORT.add(label, issue, verdict(ok),
                   why or f"-> {loc[:80]}, signed in as {who}", name)

    # Only a superuser may add a link row (the row decides who an IdP
    # identity signs in as).
    r, staff, _ = await tenant_login(client, base, tenant_host(3), "sso-staff", pw("sso-staff"))
    if not staff:
        REPORT.add("a staff user cannot add an SSO link", issue, "FAIL",
                   f"sso-staff could not sign in: {r.status_code}", name)
        return
    h = {"Host": tenant_host(3), "Cookie": f"{SESSION_COOKIE}={staff}"}
    form = await client.get(f"{base}/__admin/rustango_sso_links/new", headers=h)
    tok = page_csrf(form) or set_cookie_value(form, "rustango_csrf") or "x"
    add = await client.post(f"{base}/__admin/rustango_sso_links", headers={
        **h, "Origin": f"http://{tenant_host(3)}",
        "Cookie": f"rustango_csrf={tok}; {SESSION_COOKIE}={staff}"}, data={
        "_csrf": tok, "provider_source": "tenant", "provider_id": "1",
        "issuer": "oidc|http://idp:9000", "subject": f"forged-{tag}",
        "subject_sha256": "0" * 64, "user_id": "1"})
    REPORT.add("a staff user cannot add an SSO link", issue,
               verdict(add.status_code == 403),
               f"form {form.status_code}, POST {add.status_code}", name)


# ------------------------------------------------------ #1645 / #1610


def check_tenant_fk_schemas():
    """#1645 — no FK from a tenant schema points into another schema."""
    name = "saas-pg"
    if not PG_URL:
        REPORT.add("tenant FKs stay in the tenant schema", "#1645", "NOT-COVERED",
                   "SOAK_PG_URL not set", name)
        return
    try:
        import psycopg
        with psycopg.connect(PG_URL, connect_timeout=10) as conn:
            # Read-only catalog query: every FK whose table and target
            # live in different schemas, for the tenant schemas.
            rows = conn.execute("""
                SELECT ns.nspname, cl.relname, fns.nspname, fcl.relname
                FROM pg_constraint c
                JOIN pg_class cl ON cl.oid = c.conrelid
                JOIN pg_namespace ns ON ns.oid = cl.relnamespace
                JOIN pg_class fcl ON fcl.oid = c.confrelid
                JOIN pg_namespace fns ON fns.oid = fcl.relnamespace
                WHERE c.contype = 'f' AND ns.nspname ~ '^t[0-9]+$'
                  AND ns.nspname <> fns.nspname""").fetchall()
            n_fk = conn.execute("""
                SELECT count(*) FROM pg_constraint c
                JOIN pg_class cl ON cl.oid = c.conrelid
                JOIN pg_namespace ns ON ns.oid = cl.relnamespace
                WHERE c.contype = 'f' AND ns.nspname ~ '^t[0-9]+$'""").fetchone()[0]
    except Exception as e:  # noqa: BLE001
        REPORT.add("tenant FKs stay in the tenant schema", "#1645", "FAIL",
                   f"catalog query failed: {e}", name)
        return
    REPORT.add("tenant FKs stay in the tenant schema", "#1645",
               "FAIL" if rows else "PASS",
               f"cross-schema FKs: {rows[:5]}" if rows else
               f"{n_fk} tenant FKs, none leave their schema. Control: every tenant "
               f"schema here has rustango_users, which the bug needs absent", name)


async def emit_redaction_probe(client):
    """#1610 — put a secret in `?invite_token=` with the access log off.

    `finalize.py` then scans the logs: the order line is logged inside
    the request span, so the span's query must be redacted.
    """
    name, base = EDGE_SAAS
    token = secret(f"INVITE-{RUN_ID}-{time.time_ns()}")
    h = {"Host": tenant_host(1), **TLS}
    cu = await client.post(f"{base}/api/v1/customers", headers=h, json={
        "contact_email": f"redact-{RUN_ID}-{time.time_ns()}@example.test",
        "full_name": "Redaction", "loyalty_tier": "bronze"})
    o = await client.post(f"{base}/api/v1/orders-raw", headers=h, json={
        "reference": f"RED-{time.time_ns() % 10**12}", "customer_id": cu.json().get("id"),
        "status": "pending", "total_cents": 1, "note": None})
    r = await client.post(f"{base}/api/v1/orders/{o.json().get('id')}/confirm",
                          headers=h, params={"invite_token": token})
    REPORT.redaction = {"instance": name, "marker": token, "status": r.status_code}


def report_uncoverable_058():
    """0.58.0 items with no runtime surface an HTTP harness can reach."""
    for label, why in (
        ("schema structs, IR structs and core enums are #[non_exhaustive]",
         "compile-time only; pinned by compile_fail doctests and clippy lints"),
    ):
        REPORT.add(label, "#1661", "NOT-COVERED", why)
    REPORT.add("intcomma(i64::MIN) does not panic", "#1663", "NOT-COVERED",
               "in-process text helper; no page renders it")
    REPORT.add("new tenant project template loads its settings", "#1702", "NOT-COVERED",
               "scaffolder output; the soak app was not regenerated. The tier files "
               "reaching the tenancy server is checked as #1702 above")
    REPORT.add("callback inside an atomic migration is refused", "#1626", "NOT-COVERED",
               "loader-time refusal; soak migrations carry no callbacks. finalize.py checks "
               "every bootstrap migrate exited 0")
    REPORT.add("lockout counts failures in a fixed window", "#1672", "NOT-COVERED",
               "the counter window is 1 h and not settable from [auth]")
    REPORT.add("TOTP codes are single use; failed codes count to lockout", "#1672",
               "NOT-COVERED", "browser flow: the Playwright report covers admin TOTP")
    REPORT.add("session extractors on SQLite/MySQL", "session extractors", "NOT-COVERED",
               "the bug needs `server::Builder<Sqlite|MySql>` with the postgres feature "
               "on; every soak image is built through `Cli`, so the -my/-sq legs above "
               "are controls")
    REPORT.add("span redaction with the access log off", "#1610", "NOT-COVERED",
               "decided by finalize.py's log scan (it replaces this row)")


async def guarded(check, *args, name="", **kw):
    """Run one check; a crash is a FAIL for it, not the end of the run."""
    try:
        return await check(*args, **kw)
    except Exception as e:  # noqa: BLE001
        REPORT.add(f"{check.__name__} crashed", "—", "FAIL", repr(e)[:300], name)
        return None


async def run_058_common(client, name, base, headers=None):
    """0.58.0 checks that run the same way on both apps."""
    for check in (check_forwarded_for, check_v4_mapped_filter, check_body_limit,
                  check_model_shortcut_scopes, check_audited_pool_writes,
                  check_viewset_scopes, check_urlize_and_cbv_csrf, check_natural_pk,
                  check_webhook_targets, check_dbcache_long_keys, check_bounded_dml,
                  check_nested_atomic, check_jwt_requires_exp, check_storefront_escapes):
        await guarded(check, client, name, base, headers=headers, name=name)
    other = tenant_host(2) if headers else None
    await guarded(check_idempotency_scope, client, name, base, headers=headers,
                  other_tenant=other, name=name)


async def run_058_saas(client, name, base):
    """0.58.0 checks for the tenancy server: admin, console, SSO, JWT."""
    await guarded(check_page_cache_x_org, client, name, base, name=name)
    await guarded(check_hosts_and_settings, client, name, base, name=name)
    await guarded(check_tenant_session_misc, client, name, base, name=name)
    await guarded(check_tenant_admin_writes, client, name, base, name=name)
    _, session, _ = await tenant_login(client, base, tenant_host(7), "csrfadmin", pw("csrfadmin"))
    if session:
        await guarded(check_admin_inlines, client, name, base, session,
                      headers={"Host": tenant_host(7)}, name=name)
    await guarded(check_console, client, name, base, name=name)
    await guarded(check_jwt, client, name, base, name=name)
    await guarded(check_sso, client, name, base, name=name)
    await guarded(check_password_change_tenant, client, name, base, name=name)
    await guarded(check_password_change_operator, client, name, base, name=name)


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
            await check_not_found_envelope(client, name, base)
            await check_page_cache(client, name, base)
            await run_058_common(client, name, base)
            session = await guarded(check_bare_admin, client, name, base, name=name)
            if session:
                await guarded(check_admin_inlines, client, name, base, session, name=name)

        print("\n== assertions: multi-tenant ==")
        saas_info = {}
        for name, base in live_saas.items():
            hdr = {"Host": tenant_host(1)}
            saas_info[name] = await check_soak_info(client, name, base, headers=hdr,
                                                    expect_tenant="t01")
            await check_health_endpoints(client, name, base)
            await check_null_fk(client, name, base, headers=hdr)
            await check_serializer_carries_fk(client, name, base, headers=hdr)
            await check_bulk_atomic(client, name, base, headers=hdr)
            await check_serializer_source(client, name, base, headers=hdr)
            await check_pagination(client, name, base, headers=hdr)
            await check_cursor_walks(client, name, base, headers=hdr)
            await check_malformed_cursor_is_400(client, name, base, headers=hdr)
            await check_not_found_envelope(client, name, base, headers=hdr)
            await check_page_cache(client, name, base, headers=hdr)
            await check_page_cache_is_per_tenant(client, name, base)
            await check_tenant_isolation(client, name, base)
            await check_registered_host(client, name, base)
            await check_apex_does_not_serve_app(client, name, base)
            await check_unknown_tenant_envelope(client, name, base)
            await check_tenant_login(client, name, base)
            await run_058_common(client, name, base, headers=hdr)
            await run_058_saas(client, name, base)

        await check_page_cache_does_not_cross_apps(client, live_single, live_saas)
        report_uncoverable(live_single, live_saas)

        print("\n== assertions: edge instances ==")
        # Started now, awaited before the load: it sleeps 22 s.
        lock_timer = asyncio.create_task(
            guarded(check_lock_duration_setting, client, name=EDGE_SAAS[0]))
        await guarded(check_tls_redirect, client, name=EDGE_SAAS[0])
        await guarded(check_hash_queue_bounded, client, name=EDGE_SAAS[0])
        await guarded(emit_redaction_probe, client, name=EDGE_SAAS[0])
        await guarded(check_admin_global_limit, client, name=EDGE_SINGLE[0])
        if "single-my" in live_single:
            await guarded(check_hash_off_runtime, client, "single-my", live_single["single-my"],
                          name="single-my")
        check_tenant_fk_schemas()
        report_uncoverable_058()

        # Last, because they spend the driver's per-IP login budget on
        # each instance for a minute; the load phase outlasts that.
        print("\n== assertions: login limits ==")
        for name, base in live_single.items():
            await guarded(check_admin_login_limits, client, name, base, name=name)
        for name, base in live_saas.items():
            await guarded(check_tenancy_login_limits, client, name, base, name=name)
        await lock_timer

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

        for name, base in live_saas.items():
            await check_every_tenant_answers(client, name, base, saas_info.get(name))

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
        "redaction": REPORT.redaction,
        "run_id": RUN_ID,
        "duration_secs": round(time.time() - REPORT.started, 1),
    }
    path = os.path.join(RESULTS, "report.json")
    try:
        with open(path, "w") as fh:
            json.dump(out, fh, indent=2)
        # For finalize.py's log scan only (#1610); test credentials of
        # this fleet, never real ones.
        with open(os.path.join(RESULTS, "secrets.json"), "w") as fh:
            json.dump(sorted(USED_SECRETS), fh)
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
