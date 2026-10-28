# The operator console

A multi-tenant project has two kinds of administrator, and they never mix:

- **Operators** run the *deployment*. They live in the registry, sign in at the apex domain, and can reach every tenant.
- **Tenant users** run *one tenant*. They live in that tenant's database and never see the console.

The operator console is the web interface for the first kind — provisioning tenants, binding hostnames, managing operators, reading the audit trail, and taking a tenant out of service. Nearly everything here is also a [`manage` verb](manage.md#tenancy-commands), because an action that exists on only one surface cannot be automated, and one that exists only in a shell cannot be delegated.

The gap worth knowing about: the console can migrate **one** tenant
(`POST /orgs/{slug}/migrate`), while the CLI's `migrate-tenants` takes no slug
and runs the whole registry. There is no per-tenant migrate on the command line.

[![The operator console's tenant list — every tenant in the registry, with its storage mode, host pattern and active state, plus actions to provision, migrate and pre-warm](img/operator-console.png)](img/operator-console.png)

## Table of contents

- [Mounting it](#mounting-it)
- [What each page does](#what-each-page-does)
- [Hostnames](#hostnames)
- [Unknown hosts](#unknown-hosts)
- [Operators](#operators)
- [Two-factor sign-in](#two-factor-sign-in)
- [The audit log](#the-audit-log)
- [Provisioning runs](#provisioning-runs)
- [Database servers](#database-servers)
- [Branding](#branding)
- [The three capability levels](#the-three-capability-levels)

---

## Mounting it

The console is a router you mount; how much it can do depends on what you hand it.

```rust
use rustango::tenancy::operator_console::{router, router_with_pools, router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;

// `pools` here is an `Arc<TenantPools<_>>`.

// Read-only: browse tenants, operators and the audit log.
let app = router(registry.clone(), SessionSecret::from_env_or_random());

// …plus editing tenants, managing operators, binding hostnames, pre-warming pools.
let app = router_with_pools(registry.clone(), pools.clone().into_invalidator(), secret);

// …plus provisioning new tenants and running migrations.
let provisioner = Provisioner::new(pools.clone(), registry_url.clone(), "migrations").erased();
let app = router_with_provisioning(
    registry.clone(),
    pools.clone().into_invalidator(),
    provisioner,
    secret,
);
```

Two conversions are doing work there and neither is optional. `pools`
goes in as `Arc<dyn TenantPoolInvalidator>`, so it needs
`.into_invalidator()` — the console only ever invalidates pools, and
taking the narrow trait is what stops it reaching the rest of them.
`Provisioner::new` closes over the pools, the registry URL and the
migrations directory, and `.erased()` puts it behind
`Arc<dyn TenantProvisioner>`.

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
| **Database servers** | Servers tenant databases are created on, with their admin login |
| **Two-factor sign-in** | Your own authenticator app (`totp` feature) |

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

## Unknown hosts

By default a request whose host resolves no tenant goes to your app's routes,
where the `Tenant` extractor and the tenant admin answer 404. To serve
something there instead — a first-run setup page, a "this site is not set up"
page — hand a router to `unresolved_tenant`, for example one `/` route that answers "Create your first site" while `TenantRegistry::has_tenants()` is false. `Builder::unresolved_tenant` has a compiled example.

It runs for an unknown host and for an apex path the console does not serve.
It never runs for a host that resolves to a tenant, and the console keeps its
own routes. An inactive tenant does not resolve, so its host reaches this
router too. The router gets the registry as `Extension<TenantRegistry>`, never
a tenant pool; `has_tenants()` counts inactive tenants too. `/health` and
`/ready` stay served next to it.

**Security:** anyone who can reach the server can reach this router. Check the
state yourself (for example, offer setup only while `has_tenants()` is false)
and protect anything that writes.

The same setter exists on `rustango::server::Builder`.

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

## Two-factor sign-in

With the `totp` feature, an operator can add an authenticator app under **Two-factor sign-in** (`/account/totp`). The page shows a setup key and an `otpauth://` link until a code confirms it. Turning it off takes a current code.

Once it is on, the password alone only opens the code step (`/login/totp`); no console page answers until the code is right. Each code works once, and wrong codes count against the same limits and lock as wrong passwords. Enrolling, turning it off and every wrong code are written to the [audit log](#the-audit-log) as `totp_enroll`, `totp_disable` and `totp_failed`, never with the code.

It is optional per operator. Set `RUSTANGO_OPERATOR_REQUIRE_TOTP=1` to require it: an operator without a device then sets one up at the code step before the first session. Read at start-up.

The secrets live in `rustango_operator_totp` in the registry, sealed under `RUSTANGO_SECRET_KEY`, which must be set to enroll. `migrate` creates the table, or the console does on the first sign-in. If the table cannot be read, sign-in is refused rather than let through on the password. There are no recovery codes yet: an operator who loses the device needs a shell on the registry to delete their row.

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

### Creating a tenant

**Organizations → New tenant**. Each option says on the form where the data
lives, which login is used and which rights it needs:

- **Schema** (PostgreSQL registry only): a schema in the registry database.
  It uses the registry's login (`DATABASE_URL`); tenants share it and get no
  login of their own. Needs `CREATE` on the registry database.

In database mode, pick one:

- **Create a new database on a saved server**: choose the server. It uses the
  server's admin login once; the tenant gets its own login. The
  database is named exactly as the slug and gets a login of its own with a
  random password, granted that database only. The run shows
  `create_database`, `create_login`, `grant` and `check_login` (the new login
  signs in). An existing database of that name is refused. If a later step
  fails, the run's `rollback` step drops the new database, login and tenant
  row, and the form comes back with the reason. This is the default once a
  server is saved.
- **Use an existing database URL**: nothing is created; the database must
  exist. Masked, and never sent back to the page. No admin rights needed.
- **A database on the registry's server**: a database name; it uses the
  registry's login (`DATABASE_URL`), so tenants are not isolated from each
  other. Nothing is created.

**Test connection** checks what the chosen option needs and creates nothing.
On a saved server it runs every check a submit runs before creating anything
(slug free, routing, admin rights, no database or login of that name) and
lists each right as yes, no or unknown. In schema mode it checks the
registry login's `CREATE` on the registry database. A missing right comes
with the statements that fix it, and the create is refused with the same
text before anything is made. See
[Rights](tenant-databases.md#rights). Otherwise it reaches the database. The
backend is always the registry's.

### First admin

Give an admin email (and optionally a username; it defaults to the email)
and the run adds a `create_admin` step after the migrations, before the
tenant goes live. It creates a superuser with no usable password. If the step
fails, the run rolls back like any other step.

The admin sets their own password through a set-password link. The link
works once, expires in 72 hours, and opens only this tenant's
change-password page. With `Builder::first_admin_mailer(mailer, from)` it is
mailed to the admin. Without a mailer, the run page shows it to you **once**,
right after you submit; copy it and give it to the admin. It is never stored,
audited or logged; the audit log records the user's creation only.

The fields appear when the provisioner has a handover
(`Provisioner::with_first_admin_handover`; the standard `Builder` sets one).
A lost or expired link: run `manage set-password-link <slug> <username>`.

From the CLI: [`list-runs` / `show-run`](manage.md#inspecting-what-happened).

---

## Database servers

**Organizations → Database servers** saves a server once: host, port, admin
user, admin password, TLS mode and, on Postgres, a maintenance database to use
instead of `postgres`. On a SQLite registry a "server" is a directory for
tenant files. The backend is always the registry's.

The password is stored sealed (`enc:v1:`), so `RUSTANGO_SECRET_KEY` must be
set; without it the save is refused and the page shows a warning. It is never shown again: the edit form
leaves the field empty, and a blank field keeps the saved one. Audit rows name
the fields that changed, never their values.

**Test** logs in and lists the server version and, one line each, whether the
admin login may create databases, create logins and grant rights. When it
cannot provision tenants it says why (a missing right, or PostgreSQL before
15) and shows the `GRANT` / `ALTER ROLE` statements that fix it. Rights from
active MySQL roles count.

TLS defaults to `require` for a new server. An edit made from a page that
someone else changed in the meantime is refused; reload and edit again.

In the scaffolded `docker compose` dev stack, Postgres has no TLS, so save it
with TLS `disable`; MySQL 8 serves TLS, so `require` works. The compose MySQL
`rustango` user cannot create users or grant rights; save that server as
`root`.

Delete is refused while a database-mode tenant's URL points at the server's
host and port (SQLite: a file in its directory), or cannot be resolved, while
a database rustango created there is still recorded, and on a console built
without tenant pools.

A tenant whose database was made on a saved server gets **Rotate database
password** on its page. That and **Purge** run as recorded runs, followed live
like a create. The whole flow is in
[Tenant databases on a saved server](tenant-databases.md).

From the CLI: [`add-db-server` and friends](manage.md#database-servers).

---

## Branding

The console's name, tagline, logo, accent colour and theme come from
`[brand]` in `config/default.toml` or `config/<env>_settings.toml`:

```toml
[brand]
name          = "Acme Operator"
tagline       = "Production"
logo_url      = "https://cdn.example.com/acme-logo.png"
primary_color = "#2c6fb0"
theme_mode    = "auto"
```

| Key | Default | What it does |
|---|---|---|
| `name` | `Rustango` | Name in the header and on the sign-in page |
| `tagline` | none; the sign-in page shows "Operator console" | Line under the name |
| `logo_url` | `/__static__/rustango.png` | Logo image; the default is the bundled logo, served by the console |
| `logo_path` | none | Local logo file the console serves itself; see [Your own logo](#your-own-logo) |
| `primary_color` | the theme's colour | Tints primary buttons and links. Hex: `#rrggbb` or `#rgb` |
| `theme_mode` | `auto` | Starting theme: `auto`, `light` or `dark` |

An empty value counts as unset. A `primary_color` or `theme_mode` that does
not parse is ignored, and the default stays.

Environment variables override the file, so you can rebrand one deploy
without a config change:

| Variable | Overrides |
|---|---|
| `RUSTANGO_OPERATOR_BRAND_NAME` | `name` |
| `RUSTANGO_OPERATOR_TAGLINE` | `tagline` |
| `RUSTANGO_OPERATOR_LOGO_URL` | `logo_url` |
| `RUSTANGO_OPERATOR_LOGO_PATH` | `logo_path` |
| `RUSTANGO_OPERATOR_PRIMARY_COLOR` | `primary_color` |
| `RUSTANGO_OPERATOR_THEME_MODE` | `theme_mode` |

The order is: env variable, then `[brand]` (the tier file over
`default.toml`), then the default. An empty variable is ignored. The console
reads all of this once at boot, so restart it after a change.

### Your own logo

The apex domain serves only the console: a `Cli::with_static` mount is served
on tenant hosts, not on the apex. So a logo is either a file the console
serves, or a full URL.

`logo_path = "assets/acme-logo.png"` names a local file, relative to the
working directory. The console reads it once at boot, with the same checks as
a tenant logo upload: PNG, JPEG, WebP or ICO (no SVG), at most
`RUSTANGO_BRAND_MAX_BYTES` (1 MiB). It serves the file at
`/__static__/brand-logo.<ext>?v=<hash>` with a year-long cache; the hash
changes with the file. A missing or refused file logs a warning and the
bundled logo stays.

`logo_url` goes straight into `<img src>`, so the browser must be able to load
it: a CDN, an object-storage bucket or your website. If you set
`[security] csp`, allow that host in `img-src`.

If both are set, the env variables win over the file, and within one of them
`logo_url` wins over `logo_path`.

In a project without tenancy the admin shares a host with your routes, so a
static mount works there; see [the admin's branding section](admin.md#config-admin-and-brand).

The console sets no favicon, and there is no key for one.

### The `dev` and `staging` taglines

`cargo rustango new` writes `tagline = "dev"` under `[brand]` in
`config/dev_settings.toml` and `tagline = "staging"` in
`staging_settings.toml`, so you can tell the tiers apart at a glance; the
tagline shows as a badge.
`prod_settings.toml` sets none. The tier file wins over `default.toml`, so this
replaces a tagline you set there.

To change it, edit the line, for example `tagline = "Acme dev"`. To remove
it, delete the line or set `tagline = ""`. In a project with the admin, the
same tagline is the admin's subtitle.

### Per-tenant branding

Each tenant has its own brand on its `Org` row, set on
**Organizations → Edit**: `brand_name`, `brand_tagline`, `primary_color` and
`theme_mode`. The **Branding assets** form on the same page uploads a logo
(PNG, JPEG or WebP) and a favicon (PNG or ICO), up to 1 MB each
(`RUSTANGO_BRAND_MAX_BYTES`). Files are stored under
`RUSTANGO_BRAND_STORAGE_DIR`, default `./var/brand`.

The tenant admin and its sign-in page use these; `[brand]` does not reach
them. Without `brand_name` the tenant's display name is shown, and without an
uploaded logo the framework logo. The favicon shows on the tenant's sign-in and
change-password pages. See [the admin's branding section](admin.md#the-tenant-admin).

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
| Save and test database servers | | | ✓ |

Routes that are not mounted return 404 rather than 403 — a read-only console does not advertise what it cannot do.

### Three more constructors, for wiring rather than capability

The three above are the capability levels. The rest take the same
levels and add wiring, so they are not a fourth and fifth rung:

| Constructor | Level | What else it takes |
|---|---|---|
| `router_with_brand_storage` | read-only, or edit if you pass `Some(pools)` | a `BoxedStorage` for branding uploads, when the default `LocalStorage` is not where you want them |
| `router_with_impersonation` | edit | brand storage, the tenant session secret and a handoff URL, so an operator can enter a tenant |
| `router_full` | whatever you pass | all of the above as `Option`s — `None` simply does not mount those routes |

`Server::Builder::serve` mounts impersonation for you; a custom mount
point opts in by using `router_with_impersonation` in place of
`router_with_pools`. Reach for `router_full` when you need a
combination the shorthands do not name — it exists so that a new
pairing does not need a new positional constructor.

**Every operator is fully capable.** There are no per-operator permission gates: an operator can reach every tenant and every action the console offers. The access-control boundary is the operator list itself, which is why deactivating one takes effect on their next request.
