//! Admin UI to edit translations — issue #532 Slices 2 + 3.
//!
//! A pivoted editor over the [`crate::i18n::db`] override layer: one row
//! per translation key, one editable column per locale, plus a per-locale
//! coverage summary. Saving upserts each changed `(locale, key)` into
//! `rustango_translations` (the editable DB layer from Slice 1) so
//! operators fix typos — and fill coverage gaps (a blank cell for an
//! existing key) — without a redeploy.
//!
//! Slice 2 (#1006): view + edit + coverage + persist. Slice 3 (this
//! change): add a brand-new key (footer row), delete a key (per-row
//! checkbox → all locales), export the whole catalog as JSON
//! (`…/export.json`), and gate **writes** on superuser — the POST handler
//! reads the live [`crate::admin::AdminSession`] from request extensions,
//! rejects non-superusers with 403, and records the operator's username
//! as `updated_by`. Reads (the grid + export) stay at the admin login
//! gate.
//!
//! Still tracked on #532: the `seed-translations` manage verb (needs a
//! locales-dir wired into the `Cli`) and a fine-grained
//! `auth.edit_translations` permission codename (superuser is the gate
//! for now).
//!
//! Mounted as an admin custom view on the `rustango_translations` model
//! (so it inherits the admin's login gate):
//! `GET`/`POST {admin_prefix}/rustango_translations/editor`. The app must
//! register the `Translation` model with its admin Builder for the route
//! to mount (the editor is otherwise inert).
//!
//! The render + apply logic are free functions so they're unit-testable
//! without an HTTP harness; the registered handlers are thin glue.
//!
//! After a save the in-memory override cache is stale until the app calls
//! [`crate::i18n::db::refresh_overrides_pool`] — wire that on a signal or
//! a short interval; the persisted edit is authoritative regardless.

#[cfg(feature = "admin")]
use crate::i18n::db::{all_pool, delete_key_pool, upsert_pool, Translation};
use crate::sql::Pool;

/// A single key's value across the active locales, for the pivoted grid.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyRow {
    pub key: String,
    /// `value` per locale, aligned with the column order passed to
    /// [`pivot`]. `None` = no row for that (locale, key) (a coverage gap).
    pub values: Vec<Option<String>>,
}

/// Group flat `Translation` rows into the pivoted `(locales, key-rows)`
/// shape the editor renders. Locales are sorted; keys are sorted.
#[must_use]
pub fn pivot(rows: &[(String, String, String)]) -> (Vec<String>, Vec<KeyRow>) {
    use std::collections::{BTreeMap, BTreeSet};
    let mut locales: BTreeSet<String> = BTreeSet::new();
    // key -> locale -> value
    let mut by_key: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (locale, key, value) in rows {
        locales.insert(locale.clone());
        by_key
            .entry(key.clone())
            .or_default()
            .insert(locale.clone(), value.clone());
    }
    let locales: Vec<String> = locales.into_iter().collect();
    let key_rows = by_key
        .into_iter()
        .map(|(key, per_locale)| KeyRow {
            values: locales
                .iter()
                .map(|loc| per_locale.get(loc).cloned())
                .collect(),
            key,
        })
        .collect();
    (locales, key_rows)
}

/// A coverage summary: per-locale count of keys missing a (non-empty)
/// value, given the pivoted grid.
#[must_use]
pub fn coverage_gaps(locales: &[String], rows: &[KeyRow]) -> Vec<(String, usize)> {
    locales
        .iter()
        .enumerate()
        .map(|(i, loc)| {
            let missing = rows
                .iter()
                .filter(|r| r.values.get(i).map_or(true, |v| v.is_none()))
                .count();
            (loc.clone(), missing)
        })
        .collect()
}

/// Render the editor page HTML. Inputs are named `tr:<locale>:<key>` so
/// the POST handler can recover the `(locale, key)` pair. `escape` is the
/// admin's HTML escaper.
#[must_use]
pub fn render_editor(
    locales: &[String],
    rows: &[KeyRow],
    action: &str,
    escape: impl Fn(&str) -> String,
) -> String {
    let mut h = String::with_capacity(1024 + rows.len() * 128);
    h.push_str("<h1>Translations</h1>");
    // Coverage panel.
    let gaps = coverage_gaps(locales, rows);
    h.push_str("<p class=\"coverage\">");
    for (loc, missing) in &gaps {
        h.push_str(&format!(
            "<span>{}: {}</span> ",
            escape(loc),
            if *missing == 0 {
                "complete".to_owned()
            } else {
                format!("{missing} missing")
            }
        ));
    }
    h.push_str("</p>");
    h.push_str(r#"<p class="export"><a href="export.json">Export JSON</a></p>"#);

    h.push_str(&format!(
        "<form method=\"post\" action=\"{}\"><table><thead><tr><th>key</th>",
        escape(action)
    ));
    for loc in locales {
        h.push_str(&format!("<th>{}</th>", escape(loc)));
    }
    h.push_str("<th>delete</th></tr></thead><tbody>");
    for row in rows {
        h.push_str(&format!("<tr><td>{}</td>", escape(&row.key)));
        for (i, loc) in locales.iter().enumerate() {
            let val = row.values.get(i).and_then(Option::as_deref).unwrap_or("");
            h.push_str(&format!(
                "<td><input name=\"tr:{}:{}\" value=\"{}\"></td>",
                escape(loc),
                escape(&row.key),
                escape(val)
            ));
        }
        // Per-row "delete this key" (removes every locale's override).
        h.push_str(&format!(
            "<td><input type=\"checkbox\" name=\"delkey:{}\"></td></tr>",
            escape(&row.key)
        ));
    }
    h.push_str("</tbody>");
    // Footer: add a brand-new key across the active locales. The key goes
    // in `newkey`; each locale's value in `newval:<locale>`.
    h.push_str("<tfoot><tr><td><input name=\"newkey\" placeholder=\"new key\"></td>");
    for loc in locales {
        h.push_str(&format!(
            "<td><input name=\"newval:{}\" placeholder=\"{}\"></td>",
            escape(loc),
            escape(loc)
        ));
    }
    h.push_str("<td></td></tr></tfoot>");
    h.push_str("</table><button type=\"submit\">Save</button></form>");
    h
}

/// Parse the editor POST body (`tr:<locale>:<key>=<value>` pairs) into
/// `(locale, key, value)` edits. Form keys not matching the `tr:` prefix
/// are ignored. The locale segment can't contain `:`; the key may.
#[must_use]
pub fn parse_edits(form: &[(String, String)]) -> Vec<(String, String, String)> {
    form.iter()
        .filter_map(|(name, value)| {
            let rest = name.strip_prefix("tr:")?;
            let (locale, key) = rest.split_once(':')?;
            if locale.is_empty() || key.is_empty() {
                return None;
            }
            Some((locale.to_owned(), key.to_owned(), value.clone()))
        })
        .collect()
}

/// Parse the "add new key" footer of the editor POST body: `newkey` names
/// the key, each `newval:<locale>` carries that locale's value. Returns
/// one `(locale, key, value)` edit per **non-empty** `newval`. An empty
/// `newkey` (or no non-empty values) yields nothing.
#[must_use]
pub fn parse_new_key(form: &[(String, String)]) -> Vec<(String, String, String)> {
    let key = form
        .iter()
        .find(|(n, _)| n == "newkey")
        .map_or("", |(_, v)| v.trim());
    if key.is_empty() {
        return Vec::new();
    }
    form.iter()
        .filter_map(|(name, value)| {
            let locale = name.strip_prefix("newval:")?;
            if locale.is_empty() || value.is_empty() {
                return None;
            }
            Some((locale.to_owned(), key.to_owned(), value.clone()))
        })
        .collect()
}

/// Parse the per-row delete checkboxes (`delkey:<key>`) from the editor
/// POST body. An HTML checkbox only submits when ticked, so any present
/// `delkey:<key>` marks that key for deletion (all locales). Returns the
/// keys to drop.
#[must_use]
pub fn parse_deletes(form: &[(String, String)]) -> Vec<String> {
    form.iter()
        .filter_map(|(name, _)| name.strip_prefix("delkey:").map(str::to_owned))
        .filter(|k| !k.is_empty())
        .collect()
}

/// Render the override catalog as a locale-keyed JSON object —
/// `{"<locale>": {"<key>": "<value>", …}, …}` — for the editor's
/// "Export JSON" link / backup. Locales and keys are sorted (`BTreeMap`)
/// so the output is deterministic; pretty-printed.
#[must_use]
pub fn export_json(rows: &[(String, String, String)]) -> String {
    use std::collections::BTreeMap;
    let mut by_locale: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
    for (locale, key, value) in rows {
        by_locale
            .entry(locale.as_str())
            .or_default()
            .insert(key.as_str(), value.as_str());
    }
    serde_json::to_string_pretty(&by_locale).unwrap_or_else(|_| "{}".to_owned())
}

/// Upsert each parsed edit into the override layer. Returns the count
/// written. `updated_by` records the operator.
///
/// # Errors
/// As the ORM upsert path ([`crate::sql::ExecError`]).
#[cfg(feature = "admin")]
pub async fn apply_edits(
    pool: &Pool,
    edits: &[(String, String, String)],
    updated_by: &str,
) -> Result<usize, crate::sql::ExecError> {
    for (locale, key, value) in edits {
        upsert_pool(pool, locale, key, value, updated_by).await?;
    }
    Ok(edits.len())
}

/// Delete each listed key (all locales) from the override layer. Returns
/// the total override rows removed.
///
/// # Errors
/// As the ORM delete path ([`crate::sql::ExecError`]).
#[cfg(feature = "admin")]
pub async fn apply_deletes(pool: &Pool, keys: &[String]) -> Result<u64, crate::sql::ExecError> {
    let mut removed = 0;
    for key in keys {
        removed += delete_key_pool(pool, key).await?;
    }
    Ok(removed)
}

/// Fetch the current override rows as `(locale, key, value)` triples for
/// [`pivot`].
///
/// # Errors
/// As the ORM fetch path ([`crate::sql::ExecError`]).
#[cfg(feature = "admin")]
pub async fn editor_rows(
    pool: &Pool,
) -> Result<Vec<(String, String, String)>, crate::sql::ExecError> {
    let rows: Vec<Translation> = all_pool(pool).await?;
    Ok(rows
        .into_iter()
        .map(|t| (t.locale, t.key, t.value))
        .collect())
}

// ---- admin custom-view registration (GET grid + POST save) ----------
//
// Mounts at `{admin_prefix}/rustango_translations/editor`. Inert unless
// the app registers the `Translation` model with its admin Builder.

#[cfg(feature = "admin")]
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(feature = "admin")]
async fn editor_get(
    pool: Pool,
    _req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::{Html, IntoResponse};
    match editor_rows(&pool).await {
        Ok(triples) => {
            let (locales, rows) = pivot(&triples);
            Html(render_editor(&locales, &rows, "editor", html_escape)).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("translations editor: {e}"),
        )
            .into_response(),
    }
}

#[cfg(feature = "admin")]
async fn editor_post(
    pool: Pool,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Redirect};
    // Writes require a superuser. The admin login gate inserts the live
    // `AdminSession` into request extensions (see `admin::login_view`);
    // read it before consuming the body.
    let Some(session) = req
        .extensions()
        .get::<crate::admin::AdminSession>()
        .cloned()
    else {
        return (
            StatusCode::FORBIDDEN,
            "translations editor: no admin session",
        )
            .into_response();
    };
    if !session.is_superuser {
        return (
            StatusCode::FORBIDDEN,
            "translations editor: editing requires a superuser",
        )
            .into_response();
    }

    let body = match axum::body::to_bytes(req.into_body(), 1 << 20).await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let form: Vec<(String, String)> = serde_urlencoded::from_bytes(&body).unwrap_or_default();

    // Edits + the new key are upserts; deletes run last so a key that's
    // both edited and delete-checked in the same submit ends up deleted.
    let mut edits = parse_edits(&form);
    edits.extend(parse_new_key(&form));
    let deletes = parse_deletes(&form);
    let result = async {
        apply_edits(&pool, &edits, &session.username).await?;
        apply_deletes(&pool, &deletes).await
    }
    .await;
    if let Err(e) = result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("save translations: {e}"),
        )
            .into_response();
    }
    Redirect::to("editor").into_response()
}

#[cfg(feature = "admin")]
async fn export_get(
    pool: Pool,
    _req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    match editor_rows(&pool).await {
        Ok(triples) => (
            [(header::CONTENT_TYPE, "application/json")],
            export_json(&triples),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("export translations: {e}"),
        )
            .into_response(),
    }
}

#[cfg(feature = "admin")]
crate::register_admin_view!(
    "rustango_translations",
    "editor",
    axum::http::Method::GET,
    "Edit translations",
    editor_get,
);

#[cfg(feature = "admin")]
crate::register_admin_view!(
    "rustango_translations",
    "editor",
    axum::http::Method::POST,
    "",
    editor_post,
);

#[cfg(feature = "admin")]
crate::register_admin_view!(
    "rustango_translations",
    "export.json",
    axum::http::Method::GET,
    "Export translations (JSON)",
    export_get,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<(String, String, String)> {
        vec![
            ("en".into(), "greeting".into(), "Hello".into()),
            ("fr".into(), "greeting".into(), "Bonjour".into()),
            ("en".into(), "bye".into(), "Bye".into()),
            // fr.bye missing → coverage gap.
        ]
    }

    #[test]
    fn pivot_groups_by_key_with_aligned_locale_columns() {
        let (locales, key_rows) = pivot(&rows());
        assert_eq!(locales, vec!["en", "fr"]);
        let greeting = key_rows.iter().find(|r| r.key == "greeting").unwrap();
        assert_eq!(
            greeting.values,
            vec![Some("Hello".into()), Some("Bonjour".into())]
        );
        let bye = key_rows.iter().find(|r| r.key == "bye").unwrap();
        assert_eq!(bye.values, vec![Some("Bye".into()), None]); // fr gap
    }

    #[test]
    fn coverage_counts_missing_per_locale() {
        let (locales, key_rows) = pivot(&rows());
        let gaps = coverage_gaps(&locales, &key_rows);
        assert_eq!(gaps, vec![("en".into(), 0), ("fr".into(), 1)]);
    }

    #[test]
    fn render_has_inputs_named_for_locale_and_key() {
        let (locales, key_rows) = pivot(&rows());
        let html = render_editor(&locales, &key_rows, "editor", |s| s.to_owned());
        assert!(
            html.contains(r#"name="tr:fr:greeting" value="Bonjour""#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="tr:fr:bye" value="""#),
            "missing-value input: {html}"
        );
        assert!(html.contains("fr: 1 missing"));
    }

    #[test]
    fn parse_edits_recovers_locale_and_key() {
        let form = vec![
            ("tr:fr:greeting".into(), "Salut".into()),
            ("tr:en:bye".into(), "Goodbye".into()),
            ("csrf".into(), "token".into()), // ignored (no tr: prefix)
            ("tr:bad".into(), "x".into()),   // ignored (no second colon)
        ];
        let edits = parse_edits(&form);
        assert_eq!(
            edits,
            vec![
                ("fr".into(), "greeting".into(), "Salut".into()),
                ("en".into(), "bye".into(), "Goodbye".into()),
            ]
        );
    }

    #[test]
    fn parse_edits_handles_keys_with_dots_and_underscores() {
        let form = vec![("tr:en:nav.home_link".into(), "Home".into())];
        assert_eq!(
            parse_edits(&form),
            vec![("en".into(), "nav.home_link".into(), "Home".into())]
        );
    }

    #[test]
    fn render_includes_delete_export_and_add_key_controls() {
        let (locales, key_rows) = pivot(&rows());
        let html = render_editor(&locales, &key_rows, "editor", |s| s.to_owned());
        assert!(
            html.contains(r#"href="export.json""#),
            "export link: {html}"
        );
        assert!(
            html.contains(r#"name="delkey:greeting""#),
            "per-row delete: {html}"
        );
        assert!(html.contains(r#"name="newkey""#), "add-key field: {html}");
        assert!(
            html.contains(r#"name="newval:fr""#),
            "add-key per-locale: {html}"
        );
    }

    #[test]
    fn parse_new_key_builds_edits_for_nonempty_values_only() {
        let form = vec![
            ("newkey".into(), " farewell ".into()), // trimmed
            ("newval:en".into(), "Goodbye".into()),
            ("newval:fr".into(), String::new()), // empty → skipped
            ("tr:en:greeting".into(), "Hi".into()), // not a newval
        ];
        assert_eq!(
            parse_new_key(&form),
            vec![("en".into(), "farewell".into(), "Goodbye".into())]
        );
    }

    #[test]
    fn parse_new_key_empty_key_yields_nothing() {
        let form = vec![
            ("newkey".into(), "   ".into()),
            ("newval:en".into(), "x".into()),
        ];
        assert!(parse_new_key(&form).is_empty());
    }

    #[test]
    fn parse_deletes_collects_checked_keys() {
        let form = vec![
            ("delkey:greeting".into(), "on".into()),
            ("delkey:nav.home".into(), "on".into()),
            ("tr:en:bye".into(), "Bye".into()), // ignored
        ];
        assert_eq!(parse_deletes(&form), vec!["greeting", "nav.home"]);
    }

    #[test]
    fn export_json_is_locale_keyed_and_sorted() {
        let json = export_json(&rows());
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["en"]["greeting"], "Hello");
        assert_eq!(v["fr"]["greeting"], "Bonjour");
        assert_eq!(v["en"]["bye"], "Bye");
        assert!(v["fr"].get("bye").is_none(), "fr.bye absent (coverage gap)");
    }
}
