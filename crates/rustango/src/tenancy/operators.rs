//! Operator account state, shared by the console and the CLI (#1344).
//!
//! Activating and deactivating an operator carries rules that are not
//! obvious from the column: you cannot deactivate yourself, and you cannot
//! deactivate the last active operator. Both were written inside the console
//! handler, so a CLI verb would have had to restate them — and the copy that
//! drifted would be the one that locked everybody out of the console with
//! only a shell on the registry to undo it.
//!
//! So the rules live here and both surfaces call in. The console passes the
//! signed-in operator as `actor`; the CLI passes `None`, because a shell has
//! no session to lock itself out of. The last-active rule applies to both:
//! it is the invariant, not a UI courtesy.

use crate::core::Column as _;
use crate::sql::{CounterPool as _, FetcherPool as _, Pool, UpdaterPool as _};
use crate::tenancy::auth::Operator;

/// Why an activation change was refused.
#[derive(Debug)]
pub enum OperatorError {
    NotFound(String),
    /// The signed-in operator tried to deactivate themselves.
    SelfDeactivation,
    /// Deactivating this one would leave no active operator at all.
    LastActive,
    Driver(crate::sql::ExecError),
}

impl std::fmt::Display for OperatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(u) => write!(f, "no operator named `{u}`"),
            Self::SelfDeactivation => write!(
                f,
                "you cannot deactivate yourself — your next request would be rejected. \
                 Ask another operator to do it."
            ),
            Self::LastActive => write!(
                f,
                "this is the last active operator. Deactivating it would lock everyone \
                 out of the console, and only a shell on the registry could undo it."
            ),
            Self::Driver(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OperatorError {}

impl From<crate::sql::ExecError> for OperatorError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::Driver(e)
    }
}

/// What [`set_active`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Changed,
    /// Already in the requested state — reported, not an error, so a retried
    /// request or a re-run script is not a failure.
    AlreadySo,
}

/// Look an operator up by username.
///
/// # Errors
/// [`OperatorError::NotFound`] when there is no such operator; driver errors
/// otherwise.
pub async fn by_username(registry: &Pool, username: &str) -> Result<Operator, OperatorError> {
    Operator::objects()
        .where_(Operator::username.eq(username.to_owned()))
        .fetch(registry)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| OperatorError::NotFound(username.to_owned()))
}

/// Every operator, username order — the console's list and the CLI's.
///
/// # Errors
/// Driver / query failures.
pub async fn list(registry: &Pool) -> Result<Vec<Operator>, OperatorError> {
    Ok(Operator::objects()
        .order_by(&[("username", false)])
        .fetch(registry)
        .await?)
}

/// How many operators are active, ignoring `except`.
///
/// # Errors
/// Driver / query failures.
pub async fn active_count(registry: &Pool, except: Option<i64>) -> Result<i64, OperatorError> {
    let mut q = Operator::objects().where_(Operator::active.eq(true));
    if let Some(id) = except {
        q = q.where_(Operator::id.ne(id));
    }
    Ok(q.count(registry).await?)
}

/// Activate or deactivate `target`, enforcing both lockout rules.
/// Deactivating also ends the operator's sessions.
///
/// `actor` is the operator performing the change, when there is one — the
/// console has a session, a shell does not.
///
/// # Errors
/// [`OperatorError::SelfDeactivation`], [`OperatorError::LastActive`], or a
/// driver error. Ordering matters: the no-op check runs first, so
/// deactivating an already-inactive operator reports "already so" instead of
/// claiming a lockout about an operator who was not active to begin with.
pub async fn set_active(
    registry: &Pool,
    target: &mut Operator,
    active: bool,
    actor: Option<i64>,
) -> Result<Outcome, OperatorError> {
    let id = target.id.get().copied().unwrap_or_default();

    if target.active == active {
        return Ok(Outcome::AlreadySo);
    }

    if !active {
        if actor == Some(id) {
            return Err(OperatorError::SelfDeactivation);
        }
        return deactivate_unless_last(registry, target, id).await;
    }

    // Only `active`: a whole-row save would undo a password reset that
    // landed since `target` was read (#2467).
    Operator::objects()
        .where_(Operator::id.eq(id))
        .update()
        .set_typed(Operator::active.set(true))
        .execute_pool(registry)
        .await?;
    target.active = active;
    Ok(Outcome::Changed)
}

/// Deactivate `id` and end its sessions, unless no other operator would
/// stay active. Every active row is locked first, in id order, so two
/// operators deactivating each other run one after the other.
async fn deactivate_unless_last(
    registry: &Pool,
    target: &mut Operator,
    id: i64,
) -> Result<Outcome, OperatorError> {
    use crate::sql::FetcherTx as _;
    let mut tx = crate::sql::write_transaction(registry).await?;
    // A locking read: MySQL sees the latest rows, PG re-checks `active`.
    let active = Operator::objects()
        .where_(Operator::active.eq(true))
        .order_by(&[("id", false)])
        .select_for_update()
        .silent_on_sqlite()
        .fetch_tx(&mut tx)
        .await?;
    let Some(current) = active.iter().find(|o| o.id.get() == Some(&id)) else {
        target.active = false;
        return Ok(Outcome::AlreadySo);
    };
    if active.len() == 1 {
        return Err(OperatorError::LastActive);
    }
    // Only these two columns, so a concurrent password reset stands; the
    // stamp is read under the lock, so it only moves forward.
    let cutoff = crate::session::logout_cutoff_at(current.sessions_revoked_at, 0);
    let update = Operator::objects()
        .where_(Operator::id.eq(id))
        .update()
        .set_typed(Operator::active.set(false))
        .set_typed(Operator::sessions_revoked_at.set(Some(cutoff)))
        .compile()
        .map_err(crate::sql::ExecError::from)?;
    crate::sql::update_tx(&mut tx, &update).await?;
    tx.commit().await.map_err(crate::sql::ExecError::from)?;
    target.active = false;
    Ok(Outcome::Changed)
}

#[cfg(all(test, feature = "sqlite", feature = "testkit"))]
mod tests {
    use super::*;
    use crate::sql::Auto;

    async fn operator(pool: &Pool, username: &str) -> Operator {
        let mut op = Operator {
            id: Auto::default(),
            username: username.into(),
            password_hash: "OLD".into(),
            active: true,
            created_at: chrono::Utc::now(),
            password_changed_at: None,
            sessions_revoked_at: None,
        };
        op.insert_pool(pool).await.unwrap();
        op
    }

    /// #2467 — deactivating writes only `active`, so a password reset that
    /// landed since the row was read stands.
    #[tokio::test]
    async fn set_active_keeps_a_concurrent_password_reset() {
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::create_tables_for::<Operator>(&pool)
            .await
            .unwrap();
        operator(&pool, "keeper").await;
        let mut stale = operator(&pool, "target").await;
        let id = *stale.id.get().unwrap();
        Operator::objects()
            .where_(Operator::id.eq(id))
            .update()
            .set_typed(Operator::password_hash.set("NEW".to_owned()))
            .execute_pool(&pool)
            .await
            .unwrap();

        let out = set_active(&pool, &mut stale, false, None).await.unwrap();
        assert!(matches!(out, Outcome::Changed));
        let now = Operator::objects()
            .where_(Operator::id.eq(id))
            .fetch(&pool)
            .await
            .unwrap()
            .remove(0);
        assert!(!now.active);
        assert_eq!(now.password_hash, "NEW", "deactivate undid the reset");
    }

    /// An admin edit that deactivates an operator ends its
    /// sessions, as `set_active` does; other edits leave them alone.
    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn admin_deactivation_revokes_sessions() {
        use crate::core::SqlValue;
        let before = serde_json::json!({"active": true, "sessions_revoked_at": null});
        let mut values = vec![("active", SqlValue::Bool(false))];
        crate::admin::derived_fields::apply("rustango_operators", &mut values, Some(&before))
            .await
            .unwrap();
        let stamp = values.iter().find(|(c, _)| *c == "sessions_revoked_at");
        assert!(
            matches!(stamp, Some((_, SqlValue::DateTime(t))) if *t >= chrono::Utc::now() - chrono::Duration::seconds(5)),
            "{values:?}"
        );

        let mut values = vec![("active", SqlValue::Bool(true))];
        crate::admin::derived_fields::apply("rustango_operators", &mut values, Some(&before))
            .await
            .unwrap();
        assert_eq!(values.len(), 1, "{values:?}");

        // An int bool (SQLite/MySQL rows) counts; an inactive row is not re-stamped.
        for (was, stamps) in [
            (serde_json::json!(1), true),
            (serde_json::json!(false), false),
        ] {
            let before = serde_json::json!({"active": was, "sessions_revoked_at": null});
            let mut values = vec![("active", SqlValue::Bool(false))];
            crate::admin::derived_fields::apply("rustango_operators", &mut values, Some(&before))
                .await
                .unwrap();
            let stamped = values.iter().any(|(c, _)| *c == "sessions_revoked_at");
            assert_eq!(stamped, stamps, "active was {was}: {values:?}");
        }
    }
}
