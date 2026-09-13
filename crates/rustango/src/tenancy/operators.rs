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
use crate::sql::{CounterPool as _, FetcherPool as _, Pool};
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
        // Everyone active *except* the target. With the self-check above this
        // is unreachable single-handed; it covers two operators deactivating
        // each other at once, where both read "two active". A narrow window,
        // not a closed one.
        if active_count(registry, Some(id)).await? == 0 {
            return Err(OperatorError::LastActive);
        }
    }

    target.active = active;
    target.save_pool(registry).await?;
    Ok(Outcome::Changed)
}
