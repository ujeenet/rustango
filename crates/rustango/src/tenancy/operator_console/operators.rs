//! Managing who can sign in to the operator console, from the console.
//!
//! The page used to be a table with a paragraph telling the reader to
//! leave: *"Mutations go through `cargo run -- create-operator`"*. So
//! onboarding a colleague, locking out someone who left, or rescuing an
//! operator who forgot their password all required shell access to the
//! production host — for the one surface whose entire job is
//! administering the deployment.
//!
//! ## Deactivate, never delete
//!
//! `active = false` is what the CLI offers and what the audit trail
//! wants: the row stays, so a later "who was this?" still resolves.
//! [`super::require_session`] re-reads the row on **every** request and
//! filters on `active`, so a deactivation takes effect on the target's
//! next click rather than whenever their cookie happens to expire.
//!
//! ## The two locks this page must not let you turn
//!
//! Deactivating **yourself** ends your own session on the next request.
//! Deactivating the **last active operator** ends everyone's, and there
//! is no console path back — recovery means a shell on the registry.
//! Both are refused here, for the same reason the base host has no
//! delete button: an action whose only outcome is losing access to the
//! thing you are using should not be one click away.
//!
//! ## Passwords
//!
//! A reset rotates `password_changed_at`, and `require_session` rejects
//! any session issued before it, so a reset signs that operator out
//! everywhere. The page says so, because an operator resetting a
//! colleague's password should know it will interrupt them.
//!
//! A generated password is rendered **directly in the POST response**
//! rather than carried through a redirect. It is a secret, and a
//! redirect would put it in the URL bar, the history, the referrer and
//! every access log between here and the browser.

use axum::body::Body;
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, Response, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::Extension;
use serde::Deserialize;
use tera::Context;

use super::super::auth;
use super::super::password;
use super::{inject_op_brand, render, ConsoleState};
use crate::core::Column as _;
use crate::sql::{Auto, FetcherPool as _};
use crate::tenancy::operators as ops;

/// The column's limit. A longer username is truncated by some backends
/// and rejected by others; neither is a good way to find out.
const USERNAME_MAX: usize = 64;

/// Matches the console's own change-password rule, so the two places
/// that set an operator password cannot disagree about what is
/// acceptable.
const PASSWORD_MIN: usize = 8;

#[derive(Deserialize)]
pub(super) struct OperatorsQuery {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    notice: Option<String>,
    #[serde(default)]
    page: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct CreateForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm_password: String,
    /// Present when the operator asked for a random password instead of
    /// typing one.
    #[serde(default)]
    generate: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ResetForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm_password: String,
    #[serde(default)]
    generate: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ActiveForm {
    /// Present and `"on"` to activate; absent to deactivate — the shape
    /// a checkbox posts.
    #[serde(default)]
    active: Option<String>,
}

// ------------------------------------------------------------- listing

pub(super) async fn operators_list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<OperatorsQuery>,
) -> Response<Body> {
    page(
        &state,
        &op,
        q.error.as_deref(),
        q.notice.as_deref(),
        None,
        q.page,
    )
    .await
}

/// Render the list, optionally with a freshly generated password to
/// show once.
async fn page(
    state: &ConsoleState,
    op: &auth::Operator,
    error: Option<&str>,
    notice: Option<&str>,
    secret: Option<(&str, &str)>,
    requested_page: Option<i64>,
) -> Response<Body> {
    use crate::core::Model as _;

    let paged =
        match super::Paged::of_model(&state.registry, auth::Operator::SCHEMA, requested_page).await
        {
            Ok(p) => p,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    let rows: Vec<auth::Operator> = match auth::Operator::objects()
        .order_by(&[("username", false)])
        .limit(paged.limit)
        .offset(paged.offset)
        .fetch(&state.registry)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read the operator list: {e}"),
            )
                .into_response();
        }
    };
    let me = id_of(op);

    // Counted across the whole table, not this page. "Is this the last
    // active operator?" is a question about the registry, and a page of
    // rows cannot answer it — on page 2 of a long list every row would
    // have looked like the last one.
    let active_count = match super::count_where(
        &state.registry,
        auth::Operator::SCHEMA,
        auth::Operator::active.eq(true).into(),
    )
    .await
    {
        Ok(n) => usize::try_from(n).unwrap_or(usize::MAX),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let view: Vec<_> = rows
        .iter()
        .map(|o| {
            let id = o.id.get().copied().unwrap_or_default();
            serde_json::json!({
                "id": id,
                "username": o.username,
                "active": o.active,
                "created_at": o.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                "password_changed_at": o.password_changed_at
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()),
                "is_self": id == me,
                // Why the control is absent, so the page can say it
                // rather than just omit a button.
                "last_active": o.active && active_count <= 1,
            })
        })
        .collect();

    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "operators");
    ctx.insert("operator_username", &op.username);
    ctx.insert("operators", &view);
    ctx.insert("manage_enabled", &state.pools.is_some());
    paged.inject(&mut ctx, "/operators", "");
    ctx.insert("error", &error);
    ctx.insert("notice", &notice);
    if let Some((username, plain)) = secret {
        ctx.insert("new_username", username);
        ctx.insert("new_password", plain);
    }

    let mut resp = render(state, "op_operators.html", &ctx);
    if secret.is_some() {
        // A page body holding a plaintext password must not sit in a
        // disk cache or come back on a Back-button replay.
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store, max-age=0"),
        );
    }
    resp
}

// -------------------------------------------------------------- create

pub(super) async fn operator_create(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<CreateForm>,
) -> Response<Body> {
    let username = form.username.trim().to_owned();
    if username.is_empty() {
        return back_err(&state, &op, "A username is required.").await;
    }
    if username.chars().count() > USERNAME_MAX {
        return back_err(
            &state,
            &op,
            &format!("A username may be at most {USERNAME_MAX} characters."),
        )
        .await;
    }

    // Up front, so a duplicate reads as a duplicate rather than as
    // whatever the unique index says.
    match auth::Operator::objects()
        .where_(auth::Operator::username.eq(username.clone()))
        .fetch(&state.registry)
        .await
    {
        Ok(existing) => {
            if !existing.is_empty() {
                return back_err(
                    &state,
                    &op,
                    &format!("An operator named `{username}` already exists."),
                )
                .await;
            }
        }
        Err(e) => return back_err(&state, &op, &format!("Registry lookup failed: {e}")).await,
    }

    let generated = form.generate.is_some();
    let plain = match chosen_password(generated, &form.password, &form.confirm_password) {
        Ok(p) => p,
        Err(msg) => return back_err(&state, &op, &msg).await,
    };

    let hash = match password::hash(&plain) {
        Ok(h) => h,
        Err(e) => return back_err(&state, &op, &format!("Could not hash password: {e}")).await,
    };

    let mut row = auth::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: hash,
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
    };
    if let Err(e) = row.insert_pool(&state.registry).await {
        return back_err(&state, &op, &format!("Could not create the operator: {e}")).await;
    }

    audit(&state, &op, id_of(&row), "operator_create", &username).await;

    if generated {
        // Rendered here, not redirected to — see the module docs.
        page(&state, &op, None, None, Some((&username, &plain)), None).await
    } else {
        back_ok(&format!("created operator `{username}`"))
    }
}

// ------------------------------------------------------ activate / not

pub(super) async fn operator_set_active(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(id): Path<i64>,
    Form(form): Form<ActiveForm>,
) -> Response<Body> {
    let activate = form.active.is_some();
    let mut target = match one_operator(&state, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return back_err(&state, &op, "No such operator.").await,
        Err(e) => return back_err(&state, &op, &e).await,
    };

    // The rules — no-op first, then self-deactivation, then last-active —
    // live in `tenancy::operators` so the CLI enforces the same ones (#1344).
    // Ordering is load-bearing and documented there.
    match ops::set_active(&state.registry, &mut target, activate, Some(id_of(&op))).await {
        Ok(ops::Outcome::AlreadySo) => {
            return back_ok(&format!(
                "`{}` was already {}",
                target.username,
                if activate { "active" } else { "inactive" }
            ));
        }
        Ok(ops::Outcome::Changed) => {}
        Err(e) => return back_err(&state, &op, &sentence(&e.to_string())).await,
    }

    let verb = if activate {
        "operator_activate"
    } else {
        "operator_deactivate"
    };
    audit(&state, &op, id, verb, &target.username).await;
    back_ok(&format!(
        "{} `{}`",
        if activate { "activated" } else { "deactivated" },
        target.username
    ))
}

// ------------------------------------------------------ reset password

pub(super) async fn operator_reset_password(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(id): Path<i64>,
    Form(form): Form<ResetForm>,
) -> Response<Body> {
    let mut target = match one_operator(&state, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return back_err(&state, &op, "No such operator.").await,
        Err(e) => return back_err(&state, &op, &e).await,
    };

    let generated = form.generate.is_some();
    let plain = match chosen_password(generated, &form.password, &form.confirm_password) {
        Ok(p) => p,
        Err(msg) => return back_err(&state, &op, &msg).await,
    };
    let hash = match password::hash(&plain) {
        Ok(h) => h,
        Err(e) => return back_err(&state, &op, &format!("Could not hash password: {e}")).await,
    };

    target.password_hash = hash;
    // What actually signs them out: `require_session` rejects any
    // session whose `iat` predates this.
    target.password_changed_at = Some(chrono::Utc::now());
    if let Err(e) = target.save_pool(&state.registry).await {
        return back_err(&state, &op, &format!("Could not save: {e}")).await;
    }

    audit(&state, &op, id, "operator_reset_password", &target.username).await;

    if generated {
        let username = target.username.clone();
        page(&state, &op, None, None, Some((&username, &plain)), None).await
    } else {
        back_ok(&format!(
            "reset the password for `{}` — they are now signed out everywhere",
            target.username
        ))
    }
}

// ------------------------------------------------------------- helpers

/// The password to store, or why neither route produced one.
fn chosen_password(generate: bool, typed: &str, confirm: &str) -> Result<String, String> {
    if generate {
        if !typed.is_empty() {
            return Err("Choose one: generate a password, or type one. Both were supplied.".into());
        }
        return Ok(password::generate(20));
    }
    if typed.is_empty() {
        return Err("A password is required.".into());
    }
    if typed != confirm {
        return Err("The password and its confirmation did not match.".into());
    }
    if typed.chars().count() < PASSWORD_MIN {
        return Err(format!(
            "A password must be at least {PASSWORD_MIN} characters."
        ));
    }
    Ok(typed.to_owned())
}

async fn one_operator(state: &ConsoleState, id: i64) -> Result<Option<auth::Operator>, String> {
    auth::Operator::objects()
        .where_(auth::Operator::id.eq(id))
        .fetch(&state.registry)
        .await
        .map(|rows: Vec<auth::Operator>| rows.into_iter().next())
        .map_err(|e| format!("could not read the operator: {e}"))
}

fn id_of(op: &auth::Operator) -> i64 {
    op.id.get().copied().unwrap_or_default()
}

/// Capitalize an engine message so it reads as a sentence in the flash bar.
/// The engine wording is shared with the CLI, which prints it lowercase
/// after a `error: ` prefix.
fn sentence(msg: &str) -> String {
    let mut chars = msg.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().to_string() + chars.as_str()
    })
}

/// Post/Redirect/Get on success, so a reload does not repeat the write.
fn back_ok(notice: &str) -> Response<Body> {
    Redirect::to(&format!(
        "/operators?notice={}",
        crate::url_codec::url_encode(notice)
    ))
    .into_response()
}

/// Re-render with the reason rather than redirecting, so a long message
/// need not survive a URL.
async fn back_err(state: &ConsoleState, op: &auth::Operator, msg: &str) -> Response<Body> {
    page(state, op, Some(msg), None, None, None).await
}

async fn audit(
    state: &ConsoleState,
    actor: &auth::Operator,
    subject_id: i64,
    verb: &str,
    subject_username: &str,
) {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "target_username".into(),
        serde_json::Value::String(subject_username.to_owned()),
    );
    super::emit_registry_audit(
        &state.registry,
        "rustango_operators",
        &subject_id.to_string(),
        id_of(actor),
        verb,
        extra,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_password_needs_no_typing_and_refuses_both() {
        let p = chosen_password(true, "", "").expect("generate");
        assert!(p.chars().count() >= 20, "should be long: {p}");
        assert!(
            chosen_password(true, "typed", "typed").is_err(),
            "supplying both is ambiguous and should be refused"
        );
    }

    #[test]
    fn a_typed_password_must_be_confirmed_and_long_enough() {
        assert!(chosen_password(false, "", "").is_err(), "empty");
        assert!(
            chosen_password(false, "longenough", "different").is_err(),
            "mismatched confirmation"
        );
        assert!(
            chosen_password(false, "short", "short").is_err(),
            "too short"
        );
        assert_eq!(
            chosen_password(false, "longenough", "longenough").as_deref(),
            Ok("longenough")
        );
    }

    /// The console's own change-password form refuses anything under 8,
    /// and two places that set the same field must not disagree.
    #[test]
    fn the_minimum_matches_the_change_password_form() {
        assert_eq!(PASSWORD_MIN, 8);
    }
}
