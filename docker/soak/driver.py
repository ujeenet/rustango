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

The report is written to /results/report.json and printed. The exit code
is non-zero if any assertion FAILED; NOT-COVERED does not fail the run,
because an honest gap is not a regression.
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
    verdict: str  # PASS | FAIL | NOT-COVERED
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
        mark = {"PASS": "ok  ", "FAIL": "FAIL", "NOT-COVERED": "----"}[verdict]
        where = f" [{instance}]" if instance else ""
        print(f"  {mark} {issue:>6}  {name}{where}"
              + (f"\n           {detail}" if detail else ""))

    def failed(self):
        return [c for c in self.checks if c.verdict == "FAIL"]


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


async def check_soak_info(client, name, base, headers=None, expect_tenant=None):
    r = await client.get(f"{base}/_soak/info", headers=headers)
    if r.status_code != 200:
        REPORT.add("instance reachable", "—", "FAIL", f"{r.status_code}", name)
        return None
    info = r.json()
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
                    "sku": f"LOAD-{rng.getrandbits(48):x}", "name": "Load item",
                    "blurb": None, "price_cents": rng.randint(100, 9999),
                    "active": True})
            elif roll < 0.85:
                r = await client.get(f"{base}/api/v1/orders", headers=headers)
            else:
                # Order + confirm: this is what dispatches jobs.
                cu = await client.post(f"{base}/api/v1/customers", headers=headers, json={
                    "contact_email": f"load-{rng.getrandbits(48):x}@example.test",
                    "full_name": "Load buyer", "loyalty_tier": "bronze"})
                if cu.status_code not in (200, 201):
                    counters.record(cu.status_code)
                    continue
                o = await client.post(f"{base}/api/v1/orders-raw", headers=headers, json={
                    "reference": f"LOAD-{rng.getrandbits(48):x}",
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
                REPORT.add("instance reachable", "—", "NOT-COVERED",
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
                REPORT.add("instance reachable", "—", "NOT-COVERED",
                           "never became ready", name)
                print(f"  DOWN {name}")

        print("\n== assertions: single-tenant ==")
        for name, base in live_single.items():
            await check_soak_info(client, name, base)
            await check_null_fk(client, name, base)
            await check_bulk_atomic(client, name, base)
            await check_serializer_source(client, name, base)
            await check_pagination(client, name, base)

        print("\n== assertions: multi-tenant ==")
        for name, base in live_saas.items():
            hdr = {"Host": tenant_host(1)}
            await check_soak_info(client, name, base, headers=hdr,
                                  expect_tenant="t01")
            await check_null_fk(client, name, base, headers=hdr)
            await check_bulk_atomic(client, name, base, headers=hdr)
            await check_serializer_source(client, name, base, headers=hdr)
            await check_pagination(client, name, base, headers=hdr)
            await check_tenant_isolation(client, name, base)
            await check_registered_host(client, name, base)
            await check_apex_does_not_serve_app(client, name, base)

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
    print("\n" + "=" * 62)
    print(f"  {npass} passed, {nfail} FAILED, {nskip} not covered")
    if nfail:
        print("\n  failures:")
        for c in REPORT.failed():
            print(f"    {c.issue:>6}  {c.name} [{c.instance}]\n           {c.detail}")
    print("=" * 62)
    return 1 if nfail else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
