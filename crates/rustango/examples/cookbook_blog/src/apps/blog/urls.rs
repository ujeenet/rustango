//! Tenant-aware HTTP routes for the blog app.
//!
//! `/api/authors` GET / POST exercise the JSON API surface against
//! the per-request tenant pool. `/authors/new` GET / POST exercises
//! the *non-admin* form path: render an HTML form, parse the POST
//! body with `ModelFormFor<Author>`, save via the per-tenant
//! connection. No auto-admin involved.

use axum::extract::Path;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::get;
use axum::Router;

use rustango::extractors::{SessionUser, Tenant};
use rustango::forms::{csrf, ModelFormFor};
use rustango::sql::Auto;
use std::collections::HashMap;

use super::models::Author;

/// Tenant-scoped list + create.
async fn list_or_create(
    mut tenant: Tenant,
) -> Result<Json<Vec<AuthorOut>>, (StatusCode, String)> {
    let rows: Vec<Author> = Author::objects()
        .order_by(&[("id", false)])
        .fetch_on(tenant.conn())
        .await
        .map_err(rustango::server_error)?;
    Ok(Json(rows.into_iter().map(AuthorOut::from).collect()))
}

async fn retrieve(
    mut tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Json<AuthorOut>, (StatusCode, String)> {
    let row: Vec<Author> = Author::objects()
        .filter("id", id)
        .fetch_on(tenant.conn())
        .await
        .map_err(rustango::server_error)?;
    row.into_iter()
        .next()
        .map(|a| Json(AuthorOut::from(a)))
        .ok_or((StatusCode::NOT_FOUND, format!("author {id} not found")))
}

#[derive(serde::Serialize)]
pub struct AuthorOut {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub bio: Option<String>,
}

impl From<Author> for AuthorOut {
    fn from(a: Author) -> Self {
        Self {
            id: match a.id { Auto::Set(v) => v, _ => 0 },
            name: a.name,
            email: a.email,
            bio: a.bio,
        }
    }
}

// ---------------- non-admin form (Chapter 7c) ----------------

/// The form's `_csrf` hidden input, and the cookie to set when it is new.
fn csrf_field(headers: &HeaderMap) -> (String, Option<String>) {
    let (token, set_cookie) = csrf::ensure_token(headers, csrf::CSRF_COOKIE);
    (csrf::csrf_input_html(&token), set_cookie)
}

/// Render a form page with its CSRF cookie, if one was minted.
fn form_page(page: Html<String>, set_cookie: Option<String>) -> Response {
    let mut resp = page.into_response();
    if let Some(v) = set_cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// What the user sees when a save fails. The raw DB error names tables
/// and constraints, so it only goes to the log (#1265).
fn save_error(e: &rustango::sql::ExecError) -> &'static str {
    if let rustango::sql::ExecError::Driver(rustango::sql::sqlx::Error::Database(db)) = e {
        if db.is_unique_violation() {
            return "An author with that email already exists.";
        }
    }
    tracing::warn!(error = %e, "author save failed");
    "Could not save the author."
}

/// GET /authors/new — render the HTML form with optional error banner.
fn new_form(
    csrf_input: &str,
    error: Option<&str>,
    prev: Option<&HashMap<String, String>>,
) -> Html<String> {
    let banner = error
        .map(|e| format!(r#"<p class="error" style="color:#b00">{}</p>"#, html_escape(e)))
        .unwrap_or_default();
    let pre_name  = prev.and_then(|p| p.get("name")).cloned().unwrap_or_default();
    let pre_email = prev.and_then(|p| p.get("email")).cloned().unwrap_or_default();
    let pre_bio   = prev.and_then(|p| p.get("bio")).cloned().unwrap_or_default();
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>New Author</title></head>
<body>
  <h1>New Author</h1>
  {banner}
  <form method="POST" action="/authors/new">
    {csrf_input}
    <p><label>Name <input name="name" type="text" required value="{}"></label></p>
    <p><label>Email <input name="email" type="email" required value="{}"></label></p>
    <p><label>Bio <textarea name="bio">{}</textarea></label></p>
    <p><button type="submit">Create author</button></p>
  </form>
</body></html>
"#,
        html_escape(&pre_name),
        html_escape(&pre_email),
        html_escape(&pre_bio),
    ))
}

use rustango::text::html_escape;

/// The forms write data, so they need a logged-in tenant user (#1265).
fn login_required() -> Response {
    Redirect::to("/login").into_response()
}

async fn show_new_form(SessionUser(user): SessionUser, headers: HeaderMap) -> Response {
    if user.is_none() {
        return login_required();
    }
    let (csrf_input, set_cookie) = csrf_field(&headers);
    form_page(new_form(&csrf_input, None, None), set_cookie)
}

async fn submit_new_form(
    SessionUser(user): SessionUser,
    headers: HeaderMap,
    mut tenant: Tenant,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> Response {
    if user.is_none() {
        return login_required();
    }
    let (csrf_input, set_cookie) = csrf_field(&headers);
    // Parse + validate against Author::SCHEMA. Auto-PK + auto_now_add
    // fields are skipped automatically (post-927c351 fix).
    let mf = match ModelFormFor::<Author>::parse(&form) {
        Ok(mf) => mf,
        Err(errors) => {
            let page = new_form(&csrf_input, Some(&errors.to_string()), Some(&form));
            return form_page(page, set_cookie);
        }
    };
    let query = mf.into_insert_query();
    if let Err(e) = rustango::sql::insert_on(tenant.conn(), &query).await {
        let page = new_form(&csrf_input, Some(save_error(&e)), Some(&form));
        return form_page(page, set_cookie);
    }
    Redirect::to("/api/authors").into_response()
}

// ---------------- non-admin EDIT form (Chapter 7d) ----------------

/// GET /authors/{id}/edit — fetch the row, pre-fill the form.
async fn show_edit_form(
    SessionUser(user): SessionUser,
    headers: HeaderMap,
    mut tenant: Tenant,
    Path(id): Path<i64>,
) -> Result<Response, (StatusCode, String)> {
    if user.is_none() {
        return Ok(login_required());
    }
    let mut rows: Vec<Author> = Author::objects()
        .filter("id", id)
        .fetch_on(tenant.conn())
        .await
        .map_err(rustango::server_error)?;
    let a = rows.pop().ok_or((StatusCode::NOT_FOUND, format!("author {id} not found")))?;
    let (csrf_input, set_cookie) = csrf_field(&headers);
    Ok(form_page(edit_form(id, &a, None, &csrf_input), set_cookie))
}

async fn submit_edit_form(
    SessionUser(user): SessionUser,
    headers: HeaderMap,
    mut tenant: Tenant,
    Path(id): Path<i64>,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> Response {
    use rustango::core::SqlValue;

    if user.is_none() {
        return login_required();
    }
    let (csrf_input, set_cookie) = csrf_field(&headers);
    let mf = match ModelFormFor::<Author>::parse(&form) {
        Ok(mf) => mf,
        Err(errors) => {
            // Synthesize an Author from the form values so the re-render
            // shows what the user typed (not the persisted row).
            let preview = form_preview(&form);
            let page = edit_form(id, &preview, Some(&errors.to_string()), &csrf_input);
            return form_page(page, set_cookie);
        }
    };
    let query = match mf.into_update_query(SqlValue::I64(id)) {
        Some(q) => q,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "Author has no PK").into_response(),
    };
    if let Err(e) = rustango::sql::update_on(tenant.conn(), &query).await {
        let preview = form_preview(&form);
        let page = edit_form(id, &preview, Some(save_error(&e)), &csrf_input);
        return form_page(page, set_cookie);
    }
    Redirect::to(&format!("/api/authors/{id}")).into_response()
}

fn form_preview(form: &HashMap<String, String>) -> Author {
    Author {
        id: Auto::Unset, // unused on render
        name: form.get("name").cloned().unwrap_or_default(),
        email: form.get("email").cloned().unwrap_or_default(),
        bio: form.get("bio").cloned().filter(|s| !s.is_empty()),
        joined_at: Auto::Unset,
    }
}

fn edit_form(id: i64, a: &Author, error: Option<&str>, csrf_input: &str) -> Html<String> {
    let banner = error
        .map(|e| format!(r#"<p class="error" style="color:#b00">{}</p>"#, html_escape(e)))
        .unwrap_or_default();
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Edit Author #{id}</title></head>
<body>
  <h1>Edit Author #{id}</h1>
  {banner}
  <form method="POST" action="/authors/{id}/edit">
    {csrf_input}
    <p><label>Name <input name="name" type="text" required value="{}"></label></p>
    <p><label>Email <input name="email" type="email" required value="{}"></label></p>
    <p><label>Bio <textarea name="bio">{}</textarea></label></p>
    <p>
      <button type="submit">Save changes</button>
      <a href="/api/authors/{id}">Cancel</a>
    </p>
  </form>
</body></html>
"#,
        html_escape(&a.name),
        html_escape(&a.email),
        html_escape(a.bio.as_deref().unwrap_or("")),
    ))
}

// ---------------- user-extraction demo (Chapter 6b) ----------------

/// `GET /whoami` — demonstrates the [`SessionUser`] cookie extractor.
///
/// Anonymous → 401. Logged-in (via the tenant `/login` cookie flow) →
/// 200 with `{ "username": ..., "is_superuser": ... }`. The extractor
/// resolves the request's tenant via [`rustango::extractors::TenantContext`]
/// and validates the cookie against that tenant's slug — a cookie minted
/// on `acme` will not authenticate on `globex`.
async fn whoami(SessionUser(user): SessionUser) -> axum::response::Response {
    match user {
        Some(u) => Json(serde_json::json!({
            "username": u.username,
            "is_superuser": u.is_superuser,
        }))
        .into_response(),
        None => (StatusCode::UNAUTHORIZED, "anonymous").into_response(),
    }
}

#[must_use]
pub fn api() -> Router {
    Router::new()
        .route("/api/authors", get(list_or_create))
        .route("/api/authors/{id}", get(retrieve))
        .route("/authors/new", get(show_new_form).post(submit_new_form))
        .route("/authors/{id}/edit", get(show_edit_form).post(submit_edit_form))
        .route("/whoami", get(whoami))
        // The form POSTs ride the session cookie, so they need a token.
        .layer(csrf::layer())
}
