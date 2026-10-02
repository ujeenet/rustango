-- Grants the live MySQL suites need, for `docker compose up -d mysql`.
--
-- CI does the same thing in a workflow step ("Create the tenant
-- database"); without this, a local run fails two tests that CI passes,
-- which is the worst kind of difference to debug.
--
-- Runs once, on first container init. After a `docker compose down -v`
-- the volume is gone and this runs again; on an existing container it
-- does not, so apply it by hand there.

-- Database-mode tenancy provisions into a second database. The
-- framework refuses a tenant pointed at the registry's own database and
-- cannot create this one itself.
CREATE DATABASE IF NOT EXISTS rustango_tenant_test;

-- `*.*`, not the two named databases. `preflight_live` probes a
-- database that does not exist, and MySQL answers a narrowly-granted
-- user with 1044 access-denied rather than 1049 unknown-database — it
-- will not confirm which databases exist. So the very diagnosis that
-- test asserts on can never fire for a restricted user. This is a
-- throwaway test container with no data worth protecting.
GRANT ALL PRIVILEGES ON *.* TO 'rustango'@'%' WITH GRANT OPTION;
FLUSH PRIVILEGES;
