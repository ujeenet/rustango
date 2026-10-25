# Tenant databases on a saved server

A database-mode tenant can bring its own database URL, or rustango can make
the database for it. This page is the second path, end to end: save a server,
test it, create tenants on it, rotate their passwords, purge them.

rustango makes each tenant a database named as the slug and a login of its
own with a random password, granted that database only. It records both in
`rustango_tenant_databases`. Only a recorded database is ever dropped or has
its password changed by rustango.

## Table of contents

- [Before you start](#before-you-start)
- [Rights](#rights)
- [1. Set the secret key](#1-set-the-secret-key)
- [2. Save the server](#2-save-the-server)
- [3. Test it](#3-test-it)
- [4. Create a tenant](#4-create-a-tenant)
- [5. Rotate a tenant's password](#5-rotate-a-tenants-password)
- [6. Purge a tenant](#6-purge-a-tenant)
- [The secret key: backup and rotation](#the-secret-key-backup-and-rotation)
- [Backup and restore](#backup-and-restore)
- [PostgreSQL notes](#postgresql-notes)
- [MySQL notes](#mysql-notes)
- [Local development with Docker](#local-development-with-docker)

## Before you start

- The server runs the registry's backend. A Postgres registry makes Postgres
  tenants, and so on.
- **PostgreSQL 15 or later.** Before 15 every login may create tables in the
  `public` schema of every database, the registry's included. `test-db-server`
  refuses older servers.
- The admin login needs these rights:

| Backend | Rights |
|---|---|
| PostgreSQL | `CREATEDB` and `CREATEROLE`, or superuser |
| MySQL / MariaDB | global `CREATE`, `DROP`, `CREATE USER`, and every database privilege `WITH GRANT OPTION` |
| SQLite | a writable directory; tenants are `<dir>/<slug>.db` |

Rights from MySQL roles count when the role is active by default
(`SET DEFAULT ROLE`). See [Rights](#rights) for the full picture.

## Rights

Each way of creating a tenant uses a different login and needs different
rights. **Test connection**, `test-db-server` and the create itself check
the same list, name what is missing and print the statements that fix it.
A create is refused before anything is made.

| Option | Where the data lives | Login used | Tenant's own login | Rights needed |
|---|---|---|---|---|
| Database, new on a saved server | a new database named as the slug | the server's admin login, once | yes | create databases, create logins, grant |
| Database, existing URL | the database in the URL | the login in the URL | the URL's | none: nothing is created; the login must create tables |
| Database on the registry's server | an existing database there | the registry's (`DATABASE_URL`), shared | no, not isolated | none: nothing is created |
| Schema (PostgreSQL only) | a schema in the registry database | the registry's (`DATABASE_URL`), shared | no | `CREATE` on the registry database |

To give a login the rights, run as an admin. MySQL / MariaDB, as `root`:

```sql
GRANT CREATE, DROP, CREATE USER ON *.* TO 'rustango'@'%';
GRANT ALL PRIVILEGES ON *.* TO 'rustango'@'%' WITH GRANT OPTION;
```

PostgreSQL, as a superuser:

```sql
ALTER ROLE rustango CREATEDB CREATEROLE;
-- schema mode: the registry's login
GRANT CREATE ON DATABASE app TO rustango;
```

The generated docker-compose's MySQL login `rustango` has rights on its own
database only: save the server with `root` as its admin login instead. When
the server will not say what a login may do, the check reports "unknown" and
lets the create try. Or skip all of this: use an existing database URL.

## 1. Set the secret key

Server passwords and new tenant URLs are stored sealed (`enc:v1:`), with a key
derived from `RUSTANGO_SECRET_KEY`. Use 32 or more random bytes:

```bash
export RUSTANGO_SECRET_KEY="$(openssl rand -base64 48)"
```

Without a key, or with a blank one, a server password is refused and a tenant
URL is stored unsealed with a warning (refused under
`RUSTANGO_REQUIRE_SEALED_SECRETS=1`). The console's database server pages and
its **New tenant** page show a banner while the key is missing or short.

## 2. Save the server

Console: **Organizations → Database servers**. CLI:

```bash
cargo run -- add-db-server main --host db.internal --user admin --tls require
```

The password is asked at a hidden prompt and stored sealed. TLS defaults to
`require`. See [manage.md](manage.md) for every flag.

## 3. Test it

```bash
cargo run -- test-db-server main
```

It logs in and reports the server version and, one line each, whether the
admin may create databases, create logins and grant rights. When one is
missing it names it and prints the statements that fix it (see
[Rights](#rights)). On the console's **New tenant**
page, **Test connection** with a slug also runs every check a create runs
before any DDL: slug free, routing free, and no database or login of that
name.

## 4. Create a tenant

Console: **Organizations → New tenant**, database mode, **Create a new
database on a saved server**. CLI:

```bash
cargo run -- create-tenant acme --server main
```

The run creates the database, the login and the grant, signs in as the new
login, registers the tenant, runs its migrations and activates it. A failure
after the database exists drops what the run made. The tenant's URL holds the
new login's password and is stored sealed.

To give the tenant its first admin, fill in the admin email on the form
(username optional; it defaults to the email), or on the CLI:

```bash
cargo run -- create-tenant acme --server main --admin-email admin@example.com
```

The run creates a superuser with no usable password, before the tenant goes
live. The admin sets their own password through a link that works once, for
72 hours, on this tenant only. With `Builder::first_admin_mailer` it is mailed
to them; otherwise the console shows it **once** after you submit, and the CLI
prints it. Copy it then. A lost or expired link:
`cargo run -- set-password-link acme admin@example.com`. Details:
[First admin](operator-console.md#first-admin).

## 5. Rotate a tenant's password

Console: the tenant's page, **Rotate database password**. CLI:

```bash
cargo run -- rotate-tenant-password acme
```

Both run as a recorded run with these steps:

1. `check`: the tenant has a record, its URL still names that database and
   login, the URL is stored literal or sealed (an `env://` or vault reference
   is refused: change the password in that store), and the admin may alter
   the login.
2. `alter_password`: a new random password on the server.
3. `store_password`: the new URL, sealed, is written only while the stored
   URL is still the one read (compare-and-set). If that fails, `revert` puts
   the old password back, but only while the stored URL still holds it;
   otherwise run `rotate-tenant-password` again.
4. `invalidate_pools`: this process drops its pools for the tenant.
5. `discard_old_password`: see the window below.

**The window.** Other pods notice the new URL on their next `rustango_orgs`
fingerprint poll (`RUSTANGO_HOST_GEN_CHECK_SECS`, default 5 s). Until then
their *new* connections use the old password. Open connections are never cut.

- On MySQL 8.0.14 and later the old password is kept valid with
  `RETAIN CURRENT PASSWORD` for two poll intervals, then dropped with
  `DISCARD OLD PASSWORD`. No connection fails. The admin's global
  `CREATE USER` is enough for both. If the process stops during the wait,
  the old password stays valid until the next rotation or purge.
- On PostgreSQL and MariaDB there are no dual passwords: the old one stops at
  once, so other pods may fail to open new connections for up to one poll
  interval.

A SQLite tenant has no password. A database rustango did not make is refused.

## 6. Purge a tenant

Console: the tenant's page, **Purge** (type the slug, tick **drop the
database**). CLI:

```bash
cargo run -- purge-tenant acme --confirm acme --purge-database
```

With a provisioning console the purge is a recorded run, followed live on its
run page; the CLI prints the same steps. Every refusal comes first, in
`check`, so a refused purge changes nothing:

- the tenant's URL no longer names the recorded database or login, for
  example after `edit-tenant --database-url` or `migrate-tenant-storage`.
  Nothing is dropped: put the URL back, or drop that database by hand and
  delete its `rustango_tenant_databases` row;
- another tenant's URL names the same database, or can't be read to rule
  it out (on MySQL and SQLite each one is checked by the live server and
  file, so an alias counts), or it is the registry's;
- another purge or password change is running for the tenant;
- the admin does not own the database (Postgres: it was made by another
  admin), or may not drop the login (see
  [PostgreSQL notes](#postgresql-notes)); on MySQL it needs global `DROP`
  and `CREATE USER`;
- a session not opened by this tenant's pools is open in the database (an
  operator, a backup).

Then: `deactivate`, `purge_media`, `drop_database` (ending this tenant's
own sessions on every pod first), `drop_login`, `remove_org`,
`delete_record`. A retry after a failed step skips what is already gone. SQLite deletes the tenant's file and its `-wal`, `-shm` and
`-journal` files.

A database the operator brought has no record and keeps the old behaviour:
on Postgres it is dropped through its own URL; elsewhere it is left in place
and the purge says so.

## The secret key: backup and rotation

**Back up `RUSTANGO_SECRET_KEY` with the registry backup.** Without it no
sealed tenant URL or server password opens, and every tenant on a saved
server is unreachable until each URL is entered again.

To change the key:

1. Set `RUSTANGO_SECRET_KEY_PREVIOUS` to the old key and
   `RUSTANGO_SECRET_KEY` to the new one, on every process, and restart. The
   old key still opens what it sealed.
2. `cargo run -- reseal-tenant-secrets` lists what would change.
3. `cargo run -- reseal-tenant-secrets --apply` re-seals tenant URLs, server
   passwords and SSO client secrets with the new key.
4. Remove `RUSTANGO_SECRET_KEY_PREVIOUS` and restart.

Values sealed under a blank key before blank keys were refused still open
when `RUSTANGO_SECRET_KEY_PREVIOUS` is set and empty; re-seal them the same way.

## Backup and restore

A dump of a tenant database does not hold its login: Postgres roles and MySQL
users live outside any one database. To restore a tenant on another server:

- Postgres: restore the roles first (`pg_dumpall --roles-only`), or create the
  tenant's login, restore the database, grant it, and run
  `rotate-tenant-password` so the stored URL and the login agree.
- MySQL: create the user and its grant, restore the database, then
  `rotate-tenant-password`.

Restore the registry together with its `rustango_tenant_databases` rows, and
point the saved server at the new host with `edit-db-server`.

## PostgreSQL notes

- **New databases copy `template1`.** Its encoding, locale and any objects or
  extensions in it reach every tenant. Keep `template1` in `UTF8`.
- **Extensions.** The tenant login has every right on its database but is not
  a superuser. It can `CREATE EXTENSION` for *trusted* extensions (`pgcrypto`,
  `citext`, `hstore`, `pg_trgm`, `uuid-ossp`, `unaccent`, `ltree` and others
  marked trusted). An untrusted one, such as PostGIS, must be installed by a
  superuser: in `template1` before the tenant is created, or in the tenant's
  database after.
- **Changing the admin login (PostgreSQL 16+).** A `CREATEROLE` login may
  alter or drop only roles it holds `ADMIN OPTION` on, and only the admin that
  created a tenant login holds it. After you switch a server to another admin,
  rotation and purge of older tenants are refused with the `GRANT … WITH ADMIN
  OPTION` to run. A purge also needs the admin to own the database (or be a
  superuser).
- **Ending sessions.** Tenant pools connect with
  `application_name=rustango-tenant:<id>`. A purge ends those sessions, which
  needs a superuser, membership in the tenant's role, or `pg_signal_backend`;
  without it the purge is refused while such sessions are open.

## MySQL notes

- The tenant login is `'<login>'@'%'`: it may connect from any host the
  server's network allows. Limit it with the network or the server's
  `bind-address`; per-host logins are not offered yet.
- Tenant databases are `utf8mb4` with the registry database's collation.
- Dual passwords need MySQL 8.0.14+; MariaDB rotates without them.

## Local development with Docker

The stock `postgres` image has no TLS, so save it with `--tls disable`.
`mysql:8` serves TLS with a self-signed certificate: `--tls require` works,
`verify-ca` needs that certificate. On MySQL use `root`: the generated
docker-compose's `rustango` login lacks the rights (see [Rights](#rights)).

```bash
docker run -d --name pg -e POSTGRES_PASSWORD=pw -p 5432:5432 postgres:16
cargo run -- add-db-server local --host 127.0.0.1 --port 5432 --user postgres --tls disable

docker run -d --name my -e MYSQL_ROOT_PASSWORD=pw -p 3306:3306 mysql:8
cargo run -- add-db-server local --host 127.0.0.1 --port 3306 --user root --tls require
```

Never use `--tls disable` for a server reached over a network you do not
control: the admin login can drop every database on it.
