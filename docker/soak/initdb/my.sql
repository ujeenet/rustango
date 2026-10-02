-- Databases the soak needs, created when the MySQL volume is first
-- initialised. See the header of `pg.sql` for why this is here rather
-- than in bootstrap.sh.
--
-- MySQL needs far more than Postgres does, because `--mode schema` is
-- Postgres-only: MySQL has no `SET search_path` and a MySQL "schema"
-- *is* a database. So every MySQL tenant is database-mode, and rustango
-- does not create tenant databases — `provision.rs` says "database-mode
-- tenants bring their own database", and preflight creates and drops a
-- table to prove migrations can run, so a missing database fails
-- provisioning outright rather than warning.
--
-- `MYSQL_DATABASE=commerce` already exists; everything else is here.

CREATE DATABASE IF NOT EXISTS commerce_saas_my;

CREATE DATABASE IF NOT EXISTS commerce_saas_t01;
CREATE DATABASE IF NOT EXISTS commerce_saas_t02;
CREATE DATABASE IF NOT EXISTS commerce_saas_t03;
CREATE DATABASE IF NOT EXISTS commerce_saas_t04;
CREATE DATABASE IF NOT EXISTS commerce_saas_t05;
CREATE DATABASE IF NOT EXISTS commerce_saas_t06;
CREATE DATABASE IF NOT EXISTS commerce_saas_t07;
CREATE DATABASE IF NOT EXISTS commerce_saas_t08;
CREATE DATABASE IF NOT EXISTS commerce_saas_t09;
CREATE DATABASE IF NOT EXISTS commerce_saas_t10;
CREATE DATABASE IF NOT EXISTS commerce_saas_t11;
CREATE DATABASE IF NOT EXISTS commerce_saas_t12;
CREATE DATABASE IF NOT EXISTS commerce_saas_t13;
CREATE DATABASE IF NOT EXISTS commerce_saas_t14;
CREATE DATABASE IF NOT EXISTS commerce_saas_t15;
CREATE DATABASE IF NOT EXISTS commerce_saas_t16;
CREATE DATABASE IF NOT EXISTS commerce_saas_t17;
CREATE DATABASE IF NOT EXISTS commerce_saas_t18;
CREATE DATABASE IF NOT EXISTS commerce_saas_t19;
CREATE DATABASE IF NOT EXISTS commerce_saas_t20;

-- One wildcard grant rather than twenty-one, so the app user never
-- needs root. `\_` escapes the underscore, which is otherwise a
-- single-character wildcard in a MySQL grant pattern — without the
-- escape this would also match `commerceXsaasXt01`.
GRANT ALL PRIVILEGES ON `commerce\_saas\_%`.* TO 'rustango'@'%';
FLUSH PRIVILEGES;
