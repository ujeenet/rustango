//! Tenant sign-in by the `[auth] login_field` column: one lookup and one
//! lock key for the sign-in form, HTTP Basic and `/auth/login`.

use super::auth::User;
use crate::login_field::LoginField;

static FIELD: crate::boot_slot::BootSlot<LoginField> = crate::boot_slot::BootSlot::new();

/// The field tenant users sign in with: [`LoginField::Username`] unless
/// [`configure_login_field`] or `[auth] login_field` set it.
#[must_use]
pub fn login_field() -> LoginField {
    *FIELD.get(LoginField::default)
}

/// Set the sign-in field at boot. It beats `[auth] login_field`; `false`
/// if an earlier call already set it.
pub fn configure_login_field(field: LoginField) -> bool {
    FIELD.set_explicit(field)
}

/// The `[auth] login_field` value; `false` if app code already set one.
#[cfg(all(feature = "config", feature = "manage"))]
pub(crate) fn configure_login_field_from_settings(field: LoginField) -> bool {
    FIELD.set_from_settings(field)
}

/// The user `identifier` signs in as under `field`. An email matches
/// ignoring case; one that matches more than one user signs nobody in.
///
/// # Errors
/// A driver failure.
pub async fn find_login_user(
    pool: &crate::sql::Pool,
    field: LoginField,
    identifier: &str,
) -> Result<Option<User>, crate::sql::ExecError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    match field {
        LoginField::Username => Ok(User::objects()
            .where_(User::username.eq(identifier.to_owned()))
            .fetch(pool)
            .await?
            .into_iter()
            .next()),
        #[cfg(feature = "sso")]
        LoginField::Email => {
            let wanted = identifier.trim();
            if wanted.is_empty() {
                return Ok(None);
            }
            let rows = User::objects()
                .where_(User::email.iexact(wanted))
                .fetch(pool)
                .await?;
            match crate::sso::link::EmailLookup::pick(&rows, wanted, |u| u.email.as_deref()) {
                Ok(user) => Ok(user.cloned()),
                Err(()) => {
                    tracing::warn!(
                        target: "rustango::tenancy::login",
                        "sign-in refused: the email matches more than one user"
                    );
                    Ok(None)
                }
            }
        }
    }
}

/// The stored identifier the login lock follows under `field`, so every
/// spelling that finds `user` shares one lock.
#[must_use]
pub fn login_key(field: LoginField, user: &User) -> &str {
    match field {
        LoginField::Username => &user.username,
        #[cfg(feature = "sso")]
        LoginField::Email => user.email.as_deref().unwrap_or(&user.username),
    }
}

/// Users of one tenant who cannot sign in by email.
#[cfg(feature = "sso")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EmailGaps {
    /// Active users with no email.
    pub without_email: usize,
    /// Users whose email another user also has, ignoring case.
    pub shared_email: usize,
}

#[cfg(feature = "sso")]
impl EmailGaps {
    /// Nobody is locked out.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.without_email == 0 && self.shared_email == 0
    }
}

/// Count the tenant users email sign-in leaves out, in two aggregate reads.
///
/// # Errors
/// A driver failure.
#[cfg(feature = "sso")]
pub async fn email_gaps(pool: &crate::sql::Pool) -> Result<EmailGaps, crate::sql::ExecError> {
    use crate::core::aggregates::count_all;
    use crate::core::funcs::{lower, trim};
    use crate::core::{
        AggregateExpr, AggregateQuery, Expr, Filter, Model as _, Op, SqlValue, WhereExpr,
    };
    use crate::sql::fetch_aggregate_pool;
    let email = || trim(Expr::Column("email"));
    let blank = WhereExpr::Or(vec![
        WhereExpr::Predicate(Filter::new("email", Op::IsNull, SqlValue::Bool(true))),
        WhereExpr::ExprCompare {
            lhs: email(),
            op: Op::Eq,
            rhs: Expr::Literal(SqlValue::String(String::new())),
        },
    ]);
    let active = WhereExpr::Predicate(Filter::new("active", Op::Eq, SqlValue::Bool(true)));
    let without = count_all().filter(WhereExpr::And(vec![active, blank.clone()]));
    let q = AggregateQuery::new(User::SCHEMA, vec![("n".into(), without.into())]);
    let rows: Vec<(i64,)> = fetch_aggregate_pool(pool, &q).await?;
    let without_email = rows.first().map_or(0, |r| r.0);
    // One row per email two or more users share, ignoring case and spaces.
    let mut q = AggregateQuery::new(User::SCHEMA, vec![("n".into(), count_all().into())]);
    q.where_clause = WhereExpr::Not(Box::new(blank));
    q.group_by_exprs = vec![lower(email())];
    q.having = Some(WhereExpr::ExprCompare {
        lhs: Expr::Aggregate(Box::new(AggregateExpr::Count(None))),
        op: Op::Gt,
        rhs: Expr::Literal(SqlValue::I64(1)),
    });
    let groups: Vec<(i64,)> = fetch_aggregate_pool(pool, &q).await?;
    let count = |n: i64| usize::try_from(n).unwrap_or(0);
    Ok(EmailGaps {
        without_email: count(without_email),
        shared_email: groups.iter().map(|g| count(g.0)).sum(),
    })
}

/// Warn, once per tenant, about users email sign-in leaves out. A warning
/// and not a refusal: one tenant's data must not stop every tenant.
#[cfg(feature = "sso")]
pub(crate) async fn warn_email_gaps<DB>(pools: &super::TenantPools<DB>)
where
    DB: sqlx::Database,
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // Own caches, each pool dropped after its tenant: a boot check must
    // not fill the serving cache or hold every tenant's pool at once.
    let own = pools.fresh();
    let sweep = super::for_each_tenant(&own, |org, pool| {
        let own = &own;
        async move {
            let gaps = email_gaps(&pool).await;
            drop(pool);
            own.invalidate(&org.slug).await;
            gaps
        }
    })
    .await;
    let sweep = match sweep {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "rustango::tenancy::login",
                error = %e,
                "login_field = \"email\": could not list tenants to check user emails"
            );
            return;
        }
    };
    for (slug, gaps) in sweep.values().filter(|(_, g)| !g.is_empty()) {
        tracing::warn!(
            target: "rustango::tenancy::login",
            slug,
            without_email = gaps.without_email,
            shared_email = gaps.shared_email,
            "login_field = \"email\": these users cannot sign in until each has its own email"
        );
    }
    for (slug, e) in sweep.errors() {
        tracing::warn!(
            target: "rustango::tenancy::login",
            slug,
            error = %e,
            "login_field = \"email\": could not check user emails"
        );
    }
}

#[cfg(all(test, feature = "sqlite", feature = "sso"))]
mod sweep_tests {
    use super::super::provision::{DatabaseSource, ProvisionRequest, Provisioner, TenantStorage};
    use super::super::TenantPools;
    use crate::sql::sqlx;

    /// The boot sweep opens each tenant's pool on its own and drops it,
    /// so the serving cache is as it was.
    #[tokio::test]
    async fn the_email_sweep_leaves_the_pool_cache_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        let pools = std::sync::Arc::new(TenantPools::<sqlx::Sqlite>::new(pool));
        let migrations = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        super::super::manage::run_with_writer(
            pools.as_ref(),
            &url,
            migrations.path(),
            vec!["migrate-registry".to_owned()],
            &mut out,
        )
        .await
        .unwrap();
        let p = Provisioner::new(pools.clone(), url.clone(), migrations.path()).erased();
        let db = tmp.path().join("acme.db");
        let req = ProvisionRequest::new(
            "acme",
            TenantStorage::Database(DatabaseSource::Url(format!(
                "sqlite://{}?mode=rwc",
                db.display()
            ))),
        );
        p.provision(&req, None, None).await.unwrap();
        pools.invalidate("acme").await;
        assert_eq!(pools.cached_database_pool_count().await, 0);
        super::warn_email_gaps(pools.as_ref()).await;
        assert_eq!(pools.cached_database_pool_count().await, 0);
    }
}
