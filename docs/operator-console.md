# The operator console

A multi-tenant project has two kinds of administrator, and they never mix:

- **Operators** run the *deployment*. They live in the registry, sign in at the apex domain, and can reach every tenant.
- **Tenant users** run *one tenant*. They live in that tenant's database and never see the console.

The operator console is the web interface for the first kind — provisioning tenants, binding hostnames, managing operators, reading the audit trail, and taking a tenant out of service. Everything here is also a [`manage` verb](manage.md#tenancy-commands), because an action that exists on only one surface cannot be automated, and one that exists only in a shell cannot be delegated.

[![The operator console's tenant list — every tenant in the registry, with its storage mode, host pattern and active state, plus actions to provision, migrate and pre-warm](img/operator-console.png)](img/operator-console.png)

## Table of contents

- [Mounting it](#mounting-it)
- [What each page does](#what-each-page-does)
- [Hostnames](#hostnames)
- [Operators](#operators)
- [The audit log](#the-audit-log)
- [Provisioning runs](#provisioning-runs)
- [The three capability levels](#the-three-capability-levels)

---

## Mounting it

The console is a router you mount; how much it can do depends on what you hand it.

```rust
use rustango::tenancy::operator_console::{router, router_with_pools, router_with_provisioning, SessionSecret};

// Read-only: browse tenants, operators and the audit log.
let app = router(registry.clone(), SessionSecret::from_env_or_random());

// …plus editing tenants, managing operators, binding hostnames, pre-warming pools.
let app = router_with_pools(registry.clone(), pools.clone(), secret);

// …plus provisioning new tenants and running migrations.
let app = router_with_provisioning(registry.clone(), pools.clone(), provisioner, secret);
```

In a scaffolded `tenant` project this is already wired — `Cli::new().tenancy().with_tenant_provisioning("migrations")` mounts the full version. See [Scaffolding](scaffolding.md).

The console answers on the **apex** domain (`RUSTANGO_APEX_DOMAIN`), not on a tenant subdomain: `http://localhost:8080/login` with apex `localhost`. A request to `127.0.0.1` is not the apex and will not match.

---

## What each page does

| Page | What it is for |
|---|---|
| **Organizations** | Every tenant, with storage mode, host pattern and active state |
| **Organizations → Edit** | Display name, host pattern, path prefix, port, database URL, branding |
| **Hostnames** | The extra domains a tenant answers on |
| **Operators** | Who can sign in to this console |
| **Audit log** | Every change made through the console |
| **Runs** | Provisioning and migration runs, streamed live |

---

## Hostnames

A tenant is reachable at its subdomain and, optionally, at extra hostnames you bind to it. One hostname routes to exactly one tenant.

[![The hostnames page for one tenant — the base host marked as such and undeletable, extra hosts with Park and Remove actions, and a form to add one](img/operator-console-hosts.png)](img/operator-console-hosts.png)

Two things worth knowing:

**The base host has no delete button.** It comes from the tenant's `host_pattern` column rather than the hostnames table, so there is no row to remove — change it on the tenant's edit page instead. That is structural, not a UI rule: the engine refuses it too, so an operator who guesses the `POST` gets the same answer as one who reads the page.

**Parking keeps the row.** *Park* takes a host out of service without losing the record — useful while DNS propagates, or when retiring a domain you may want back. *Serve* puts it back.

Hostnames are normalized on the way in: lowercased, no scheme, no port, no path. The stored value is compared byte-for-byte against the `Host` header, so a value that could never match is refused rather than saved.

From the CLI: [`list-hosts` / `add-host` / `remove-host` / `set-host-enabled`](manage.md#hostnames).

---

## Operators

[![The operators page — the signed-in operator marked "that's you", with a form to add another and the option to generate a password](img/operator-console-operators.png)](img/operator-console-operators.png)

Operators are deactivated, never deleted: the row stays, so a later "who was this?" still resolves. The console re-reads it on **every** request, so a deactivation takes effect on the target's next click rather than whenever their cookie expires.

Two things the page will not let you do, for the same reason:

- **Deactivate yourself.** Your next request would be rejected.
- **Deactivate the last active operator.** That locks everyone out, and only a shell on the registry could undo it.

A generated password is shown **once**, in the response body — never through a redirect, which would put it in the URL bar, the history, the referrer and every access log in between.

From the CLI: [`list-operators` / `set-operator-active`](manage.md#list-operators).

---

## The audit log

[![The console's audit log — who did what to which record and when, filterable by entity, id and operation](img/operator-console-audit.png)](img/operator-console-audit.png)

Every console mutation is recorded: tenant edits, hostname changes, operator management, impersonation, purges, pre-warms. The `source` column carries `operator:<id>:<verb>`, so operator activity is separable from tenant-user activity after the fact.

This is the **registry's** log. A tenant's own history lives in that tenant's admin — mixing them would mean fanning a query across every tenant pool to render one page.

It grows with console use, so trim it on a schedule with [`audit-cleanup`](manage.md#audit-cleanup), which sweeps the registry's log and every active tenant's.

From the CLI: [`audit-log`](manage.md#inspecting-what-happened).

---

## Provisioning runs

[![The provisioning runs page — each run with its kind, tenant, state and timing, linking to the recorded steps](img/operator-console-runs.png)](img/operator-console-runs.png)

Provisioning a tenant is several steps against a database that may be slow or unreachable, so it is recorded as a **run** rather than a request that either returns or doesn't. Each run streams its steps live and survives a reload, a reconnect, or being watched from a second pod.

Tenants created from the CLI are recorded too, tagged `requested_by = cli`, so the history covers both surfaces.

From the CLI: [`list-runs` / `show-run`](manage.md#inspecting-what-happened).

---

## The three capability levels

Which routes exist depends on which constructor you mounted:

| | `router` | `router_with_pools` | `router_with_provisioning` |
|---|---|---|---|
| Browse tenants, operators, audit log | ✓ | ✓ | ✓ |
| Edit a tenant, manage hostnames | | ✓ | ✓ |
| Add / deactivate operators | | ✓ | ✓ |
| Pre-warm pools | | ✓ | ✓ |
| Decommission a tenant | | ✓ | ✓ |
| Provision a new tenant, run migrations | | | ✓ |
| View provisioning runs | | | ✓ |

Routes that are not mounted return 404 rather than 403 — a read-only console does not advertise what it cannot do.

**Every operator is fully capable.** There are no per-operator permission gates: an operator can reach every tenant and every action the console offers. The access-control boundary is the operator list itself, which is why deactivating one takes effect on their next request.
