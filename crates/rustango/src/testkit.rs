//! Test-support helpers: build the framework's own tables from each
//! model's [`SCHEMA`](crate::core::Model::SCHEMA) — never hand-written
//! DDL — and construct fully-populated framework model instances via
//! factories.
//!
//! ## Why this module exists
//!
//! Adding a nullable column to a framework model (e.g. the SSO change
//! that added `Org.sso_*` / `User.email` / `AdminUser.email`) used to
//! break the test suite in two mechanical ways:
//!
//! * ~20 files that hand-write `CREATE TABLE rustango_users/orgs/…`
//!   (in all three dialect quote styles) then fail at runtime with
//!   "no such column" once the model's insert/fetch emits the new one.
//! * ~75 struct-literal sites (`Org { … }` / `User { … }` /
//!   `AdminUser { … }`) that fail to compile (E0063) because these
//!   models can't derive `Default` (`Auto<i64>` PK + `NOT NULL`
//!   fields).
//!
//! Routing table creation through the **same DDL emitter the migration
//! runner uses** ([`apply_all_pool`](crate::migrate::apply_all_pool)),
//! and construction through **one factory per model**, means a new
//! column flows in automatically — a schema change touches only the
//! factory, never the call sites.
//!
//! ## Availability
//!
//! Gated behind `#[cfg(any(test, feature = "testkit"))]`. Integration
//! tests are *external* crates, so they can't see `#[cfg(test)]` items;
//! enable the helpers there with `--features testkit` (dev-only — the
//! feature pulls in nothing and adds no runtime cost to a normal build).

use crate::core::ModelSchema;
use crate::migrate::{ddl, MigrateError};
use crate::sql::Pool;

/// Create every managed framework table (`rustango_*`) for the pool's
/// dialect, sourced from each model's `SCHEMA`.
///
/// Mirrors [`crate::migrate::apply_all_pool`] (tables → FK constraints →
/// column/table comments) but **filtered to framework-owned tables**, so
/// user models registered in the test's inventory aren't created here.
/// Tables use `CREATE TABLE IF NOT EXISTS`, so it's safe to call on a
/// database whose framework tables were already bootstrapped.
///
/// # Errors
/// Any DDL failure ([`MigrateError`]).
pub async fn create_framework_tables(pool: &Pool) -> Result<(), MigrateError> {
    let models: Vec<&'static ModelSchema> = crate::migrate::registered_models()
        .into_iter()
        .filter(|m| m.managed && m.table.starts_with("rustango_"))
        .collect();
    emit_tables(pool, &models).await
}

/// Create the given models' tables for the pool's dialect, from their
/// `SCHEMA`. Same create → constraints → comments sequence as
/// [`create_framework_tables`].
///
/// # Errors
/// Any DDL failure ([`MigrateError`]).
pub async fn create_tables(
    pool: &Pool,
    models: &[&'static ModelSchema],
) -> Result<(), MigrateError> {
    emit_tables(pool, models).await
}

/// Create a single model's table (convenience over [`create_tables`]).
///
/// # Errors
/// Any DDL failure ([`MigrateError`]).
pub async fn create_tables_for<M: crate::core::Model>(pool: &Pool) -> Result<(), MigrateError> {
    emit_tables(pool, &[M::SCHEMA]).await
}

/// Create the given models' tables inside a specific SQL schema
/// (Postgres per-tenant `"{schema}"."rustango_…"`).
///
/// Intended for the schema-storage-mode tenancy tests, which create the
/// framework tables under a per-tenant Postgres schema. Only the
/// `CREATE TABLE` target is schema-qualified, so this is correct for
/// tables without cross-`rustango_` foreign keys (e.g. `rustango_users`,
/// the sole in-schema table the suite needs). Postgres-only.
///
/// # Errors
/// Any DDL failure ([`MigrateError`]).
pub async fn create_tables_in_schema(
    pool: &Pool,
    schema: &str,
    models: &[&'static ModelSchema],
) -> Result<(), MigrateError> {
    let dialect = pool.dialect();
    for model in models {
        let bare = ddl::create_table_if_not_exists_sql_with_dialect(dialect, model);
        let quoted = dialect.quote_ident(model.table);
        let qualified_target = format!("\"{schema}\".{quoted}");
        // Replace only the CREATE-target occurrence (first), leaving any
        // later references untouched — safe for the FK-less in-schema
        // tables the suite creates.
        let sql = bare.replacen(&quoted, &qualified_target, 1);
        crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
    }
    Ok(())
}

/// Shared create → constraints → comments walk, matching
/// [`crate::migrate::apply_all_pool`] but with `IF NOT EXISTS` tables.
async fn emit_tables(pool: &Pool, models: &[&'static ModelSchema]) -> Result<(), MigrateError> {
    let dialect = pool.dialect();
    for model in models {
        let sql = ddl::create_table_if_not_exists_sql_with_dialect(dialect, model);
        crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
    }
    for model in models {
        for sql in ddl::create_constraints_sql_with_dialect(dialect, model) {
            crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
        }
    }
    for model in models {
        for sql in ddl::column_comment_statements_with_dialect(dialect, model) {
            crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
        }
    }
    for model in models {
        for sql in ddl::table_comment_statements_with_dialect(dialect, model) {
            crate::sql::raw_execute_pool(pool, &sql, ::std::vec::Vec::new()).await?;
        }
    }
    Ok(())
}

/// Generate the framework's system-app migrations from the current
/// models and apply the **whole** set (registry + tenant scope) to
/// `pool` — the complete framework schema (tables, FK constraints AND
/// the composite-unique indexes that `create_framework_tables` /
/// `apply_all_pool` don't emit), built exactly the way provisioning
/// builds it.
///
/// Use this in tests that need the full framework schema — e.g. the
/// permission engine, which relies on the `(role_id, codename)` /
/// `(user_id, codename)` unique indexes.
///
/// # Errors
/// Any generation or apply failure ([`MigrateError`]).
#[cfg(feature = "tenancy")]
pub async fn migrate_framework(pool: &Pool) -> Result<(), MigrateError> {
    // Deduped snapshot of every framework (`rustango_*`) table — the
    // shared audit/content_types tables appear once — materialized in one
    // pass (tables → indexes → FK ALTERs), exactly what the migration
    // engine emits. Fresh-DB use only (tests).
    let mut seen = std::collections::HashSet::new();
    let models: Vec<&'static ModelSchema> = crate::migrate::registered_models()
        .into_iter()
        .filter(|m| m.managed && m.table.starts_with("rustango_") && seen.insert(m.table))
        .collect();
    let snapshot = crate::migrate::SchemaSnapshot::from_models(&models);
    let empty = crate::migrate::SchemaSnapshot::default();
    let changes = crate::migrate::detect_changes(&empty, &snapshot);
    let batch =
        crate::migrate::render_changes_split_with_dialect(&changes, &snapshot, pool.dialect())
            .map_err(MigrateError::Validation)?;
    for sql in batch.immediate.iter().chain(batch.deferred_fks.iter()) {
        // Idempotent: `render` emits plain CREATE (not IF NOT EXISTS), so
        // swallow "already exists" / duplicate errors — a test may have
        // pre-created some framework tables (e.g. via apply_all_pool).
        if let Err(e) = crate::sql::raw_execute_pool(pool, sql, ::std::vec::Vec::new()).await {
            let msg = format!("{e}").to_lowercase();
            if msg.contains("already exists") || msg.contains("duplicate") {
                continue;
            }
            return Err(e.into());
        }
    }
    Ok(())
}

// ======================================================= factories

/// A fully-populated [`Org`](crate::tenancy::Org) with valid defaults,
/// for struct-update construction: `Org { slug: "x".into(), ..testkit::org() }`.
///
/// Adding a nullable/defaulted column to `Org` updates only this
/// function — never the call sites.
#[cfg(feature = "tenancy")]
#[must_use]
pub fn org() -> crate::tenancy::Org {
    crate::tenancy::Org {
        id: crate::sql::Auto::Unset,
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "postgres".into(),
        database_url: None,
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
        #[cfg(feature = "admin-sso")]
        sso_enabled: false,
        #[cfg(feature = "admin-sso")]
        sso_provider: None,
        #[cfg(feature = "admin-sso")]
        sso_issuer_url: None,
        #[cfg(feature = "admin-sso")]
        sso_client_id: None,
        #[cfg(feature = "admin-sso")]
        sso_secret_ref: None,
    }
}

/// A fully-populated tenant [`User`](crate::tenancy::User) with valid
/// defaults, for struct-update construction. `password_hash` is empty —
/// set it (or use [`crate::tenancy::User`]'s own helpers) when the test
/// authenticates.
#[cfg(feature = "tenancy")]
#[must_use]
pub fn user() -> crate::tenancy::User {
    crate::tenancy::User {
        id: crate::sql::Auto::Unset,
        username: "alice".into(),
        password_hash: String::new(),
        #[cfg(feature = "admin-sso")]
        email: None,
        is_superuser: false,
        active: true,
        created_at: chrono::Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
    }
}

/// A fully-populated [`AdminUser`](crate::admin::AdminUser) with valid
/// defaults, for struct-update construction. `password_hash` is empty;
/// use [`crate::admin::AdminUser::new_with_password`] when a real hash
/// is needed.
#[cfg(feature = "admin")]
#[must_use]
pub fn admin_user() -> crate::admin::AdminUser {
    crate::admin::AdminUser {
        id: crate::sql::Auto::Unset,
        username: "admin".into(),
        password_hash: String::new(),
        #[cfg(feature = "admin-sso")]
        email: None,
        is_superuser: true,
        active: true,
        created_at: chrono::Utc::now(),
    }
}

#[cfg(all(test, feature = "sqlite", feature = "tenancy", feature = "admin"))]
mod tests {
    use super::*;
    use crate::core::Model as _;

    #[tokio::test]
    async fn framework_tables_and_factories_roundtrip() {
        let path = std::env::temp_dir().join("rustango_testkit_selftest.db");
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = crate::sql::Pool::connect(&url).await.expect("connect");

        // Tables come straight from Model::SCHEMA — no hand-written DDL.
        // Build the specific models this test inserts (not
        // create_framework_tables) because the lib-test build also
        // registers `#[cfg(test)]` fixture models that alias
        // `rustango_users`, which would race the real schema.
        create_tables_for::<crate::tenancy::Org>(&pool)
            .await
            .expect("orgs");
        create_tables_for::<crate::tenancy::User>(&pool)
            .await
            .expect("users");
        create_tables_for::<crate::admin::AdminUser>(&pool)
            .await
            .expect("admin_users");

        // Factories build fully-populated instances; struct-update only
        // overrides what the test cares about.
        let mut org = crate::tenancy::Org {
            slug: "acme".into(),
            ..org()
        };
        org.insert_pool(&pool).await.expect("insert org");
        let mut admin = crate::admin::AdminUser {
            username: "root".into(),
            ..admin_user()
        };
        admin.insert_pool(&pool).await.expect("insert admin");
        let mut user = crate::tenancy::User {
            username: "alice".into(),
            ..user()
        };
        user.insert_pool(&pool).await.expect("insert user");

        // All three inserts succeeding proves the tables exist with the
        // full column set (from SCHEMA) and the factories are
        // insert-valid on every framework model.
        let _ = std::fs::remove_file(&path);
    }
}
