//! Admin hooks for invites and links: every write here is superuser-only.

use crate::admin::derived_fields::DeriveFuture;
use crate::core::SqlValue;
use crate::sql::Pool;

use super::signup::{InviteTarget, RoleNames};
use super::SsoInvite;

/// How long a re-link invite stays good.
#[cfg(feature = "tenancy")]
const RELINK_DAYS: i64 = 7;

/// The signed-in admin user, if a real one (not an impersonating operator).
fn current_user_id() -> Option<i64> {
    crate::admin::session::current()
        .filter(|s| s.impersonated_by.is_none() && s.user_id != 0)
        .map(|s| s.user_id)
}

/// Admin hook: store the target and roles in canonical form, refuse what does
/// not parse, and stamp `invited_by` on a new invite.
pub(crate) fn admin_invite<'a>(
    values: &'a mut Vec<(&'static str, SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> DeriveFuture<'a> {
    let out = (|| {
        for (col, v) in values.iter_mut() {
            match (*col, &*v) {
                ("target", SqlValue::String(raw)) => {
                    let t: InviteTarget = raw.parse().map_err(|e| format!("target: {e}"))?;
                    *v = SqlValue::String(t.to_string());
                }
                ("target", _) => return Err("target: expected an email or a domain".into()),
                ("roles", SqlValue::String(raw)) => {
                    let r: RoleNames = raw.parse().map_err(|e| format!("roles: {e}"))?;
                    *v = if r.is_empty() {
                        SqlValue::Null
                    } else {
                        SqlValue::String(r.to_string())
                    };
                }
                _ => {}
            }
        }
        if before.is_none() {
            values.retain(|(c, _)| *c != "invited_by" && *c != "uses");
            if let Some(id) = current_user_id() {
                values.push(("invited_by", SqlValue::I64(id)));
            }
        }
        Ok(())
    })();
    Box::pin(async move { out })
}

inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_invites",
        derive: admin_invite,
    }
}

/// The invites a bulk form asks for, one per target.
///
/// # Errors
/// The first target or field that does not parse.
pub(crate) fn bulk_invites(
    form: &[(String, String)],
    invited_by: Option<i64>,
) -> Result<Vec<SsoInvite>, String> {
    let field = |name: &str| {
        form.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    let roles: Option<RoleNames> = field("roles")
        .map(str::parse)
        .transpose()
        .map_err(|e| format!("roles: {e}"))?;
    let max_uses = field("max_uses")
        .map(|v| v.parse::<i32>().ok().filter(|n| *n > 0))
        .map(|n| n.ok_or("max uses: a positive number"))
        .transpose()?;
    let days = field("expires_in_days")
        .map(|v| v.parse::<i64>().ok().filter(|n| (1..=3650).contains(n)))
        .map(|n| n.ok_or("expires in days: 1 to 3650"))
        .transpose()?;
    let provider = field("provider_slug").map(str::to_owned);
    let mut seen = Vec::new();
    let mut out = Vec::new();
    // Paste or CSV: one target per line or cell; a header line is skipped.
    for raw in field("targets")
        .unwrap_or_default()
        .split([',', ';', '\n', '\r', '\t', ' '])
        .map(|t| t.trim().trim_matches('"'))
        .filter(|t| {
            !t.is_empty() && !t.eq_ignore_ascii_case("email") && !t.eq_ignore_ascii_case("target")
        })
    {
        let target: InviteTarget = raw.parse().map_err(|e| format!("`{raw}`: {e}"))?;
        let target = target.to_string();
        if seen.contains(&target) {
            continue;
        }
        seen.push(target.clone());
        out.push(SsoInvite {
            id: crate::sql::Auto::Unset,
            provider_slug: provider.clone(),
            target,
            roles: roles
                .as_ref()
                .filter(|r| !r.is_empty())
                .map(ToString::to_string),
            expires_at: days.map(|d| chrono::Utc::now() + chrono::TimeDelta::days(d)),
            max_uses,
            uses: 0,
            used_at: None,
            used_by: None,
            invited_by,
            link_user_id: None,
            link_key: None,
            created_at: crate::sql::Auto::Unset,
        });
    }
    if out.is_empty() {
        return Err("no targets".into());
    }
    Ok(out)
}

fn bulk_page(error: Option<&str>) -> String {
    let esc = crate::text::html_escape;
    let error = error
        .map(|e| format!("<p class=\"error\">{}</p>", esc(e)))
        .unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Add invites</title></head><body>\
         <h1>Add invites</h1>{error}<form method=\"post\">{csrf}\
         <p><label>Emails or domains (paste, or a CSV column)<br>\
         <textarea name=\"targets\" rows=\"12\" cols=\"60\"></textarea></label></p>\
         <p><label>Roles <input name=\"roles\"></label></p>\
         <p><label>Provider slug (empty: any) <input name=\"provider_slug\"></label></p>\
         <p><label>Max uses each (empty: unlimited) <input name=\"max_uses\" inputmode=\"numeric\"></label></p>\
         <p><label>Expires in days (empty: never) <input name=\"expires_in_days\" inputmode=\"numeric\"></label></p>\
         <p><button type=\"submit\">Add invites</button></p></form></body></html>",
        csrf = crate::admin::current_csrf_input(),
    )
}

/// Only a superuser adds invites; an admin mounted without auth is open, as elsewhere.
fn may_invite(req: &axum::http::Request<axum::body::Body>) -> bool {
    match crate::admin::session::from_extensions(req.extensions()) {
        Some(s) => s.is_superuser,
        None => true,
    }
}

async fn bulk_get(
    _pool: Pool,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::{Html, IntoResponse};
    if !may_invite(&req) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    Html(bulk_page(None)).into_response()
}

async fn bulk_post(
    pool: Pool,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::{Html, IntoResponse, Redirect};
    if !may_invite(&req) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let invited_by = crate::admin::session::from_extensions(req.extensions())
        .filter(|s| s.impersonated_by.is_none() && s.user_id != 0)
        .map(|s| s.user_id);
    let Ok(body) = axum::body::to_bytes(req.into_body(), 1 << 20).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let form: Vec<(String, String)> = serde_urlencoded::from_bytes(&body).unwrap_or_default();
    let invites = match bulk_invites(&form, invited_by) {
        Ok(i) => i,
        Err(e) => return (StatusCode::BAD_REQUEST, Html(bulk_page(Some(&e)))).into_response(),
    };
    if let Err(e) = insert_all(&pool, invites).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::error::server_error_body("add invites", &e),
        )
            .into_response();
    }
    Redirect::to(".").into_response()
}

/// Insert every invite, or none.
async fn insert_all(pool: &Pool, invites: Vec<SsoInvite>) -> Result<(), String> {
    let mut tx = crate::sql::transaction_pool(pool)
        .await
        .map_err(|e| e.to_string())?;
    for mut i in invites {
        i.insert_tx(&mut tx).await.map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())
}

crate::register_admin_view!(
    "rustango_sso_invites",
    "bulk",
    axum::http::Method::GET,
    "Add invites in bulk",
    bulk_get,
    perm = "add",
);

crate::register_admin_view!(
    "rustango_sso_invites",
    "bulk",
    axum::http::Method::POST,
    "",
    bulk_post,
    perm = "add",
);

fn superuser_only(parts: &axum::http::request::Parts, _: Option<&serde_json::Value>) -> bool {
    crate::admin::session::is_superuser(parts)
}

crate::register_admin_object_permission!("rustango_sso_links", "unlink", superuser_only);
crate::register_admin_object_permission!("rustango_sso_links", "relink", superuser_only);

/// The `SsoLink` admin actions: unlink, and re-link on the next sign-in.
pub(crate) fn register_link_actions(b: crate::admin::Builder) -> crate::admin::Builder {
    use crate::admin::ActionPerm;
    b.register_action_with_perm(
        "rustango_sso_links",
        "unlink",
        ActionPerm::Delete,
        |pool, pks| {
            Box::pin(async move {
                unlink(pool, pks)
                    .await
                    .map_err(crate::admin::AdminError::Internal)
            })
        },
    )
    .register_action_with_perm(
        "rustango_sso_links",
        "relink",
        ActionPerm::Delete,
        |pool, pks| {
            Box::pin(async move {
                relink(pool, pks)
                    .await
                    .map_err(crate::admin::AdminError::Internal)
            })
        },
    )
}

async fn unlink(pool: &Pool, pks: &[SqlValue]) -> Result<(), String> {
    for pk in pks {
        let query = crate::sso::SsoLink::objects()
            .filter("id", pk.clone())
            .compile_delete()
            .map_err(|e| e.to_string())?;
        crate::sql::delete_pool(pool, &query)
            .await
            .map_err(|e| format!("unlink: {e}"))?;
    }
    Ok(())
}

/// Delete each link and leave a one-use invite that links its user again on
/// the next sign-in by the user's verified email. Never for a superuser.
async fn relink(pool: &Pool, pks: &[SqlValue]) -> Result<(), String> {
    #[cfg(feature = "tenancy")]
    {
        use crate::sql::FetcherPool as _;
        let invited_by = current_user_id();
        for pk in pks {
            let Some(link) = crate::sso::SsoLink::objects()
                .filter("id", pk.clone())
                .fetch(pool)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
            else {
                continue;
            };
            if link.provider_source == crate::sso::LinkSource::Admin.as_str() {
                return Err(
                    "admin links are never re-linked; unlink and add a link instead".into(),
                );
            }
            let user = crate::tenancy::member_auth::find_tenant_user(pool, link.user_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the linked user is gone; unlink instead")?;
            if user.is_superuser {
                return Err("a superuser is never re-linked by email; add the link by hand".into());
            }
            let email: super::EmailAddr = user
                .email
                .as_deref()
                .ok_or("the user has no email to re-link by")?
                .parse()
                .map_err(|e| format!("the user's email: {e}"))?;
            // Only a sign-in through the old link's provider may use the invite.
            let key = crate::sso::ProviderKey::of_link(&link)
                .ok_or("the link names an unknown provider source")?;
            let mut invite = SsoInvite {
                id: crate::sql::Auto::Unset,
                provider_slug: None,
                target: email.as_str().to_owned(),
                roles: None,
                expires_at: Some(chrono::Utc::now() + chrono::TimeDelta::days(RELINK_DAYS)),
                max_uses: Some(1),
                uses: 0,
                used_at: None,
                used_by: None,
                invited_by,
                link_user_id: Some(link.user_id),
                link_key: Some(key.fingerprint()),
                created_at: crate::sql::Auto::Unset,
            };
            let mut tx = crate::sql::transaction_pool(pool)
                .await
                .map_err(|e| e.to_string())?;
            let query = crate::sso::SsoLink::objects()
                .filter("id", pk.clone())
                .compile_delete()
                .map_err(|e| e.to_string())?;
            crate::sql::delete_tx(&mut tx, &query)
                .await
                .map_err(|e| format!("unlink: {e}"))?;
            invite
                .insert_tx(&mut tx)
                .await
                .map_err(|e| format!("invite: {e}"))?;
            tx.commit().await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    #[cfg(not(feature = "tenancy"))]
    {
        let _ = (pool, pks);
        Err("re-link needs tenant users".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_invite_hook_canonicalises_and_refuses() {
        let mut ok = vec![
            ("target", SqlValue::String(" Ann@Client.org ".into())),
            ("roles", SqlValue::String("Viewer, ,Editor".into())),
            ("uses", SqlValue::I32(5)),
        ];
        admin_invite(&mut ok, None).await.unwrap();
        assert_eq!(ok[0].1, SqlValue::String("ann@client.org".into()));
        assert_eq!(ok[1].1, SqlValue::String("Viewer, Editor".into()));
        assert!(
            !ok.iter().any(|(c, _)| *c == "uses"),
            "a new invite starts unused"
        );
        let mut bad = vec![("target", SqlValue::String("gmail".into()))];
        assert!(admin_invite(&mut bad, None).await.is_err());
    }

    #[test]
    fn bulk_form_takes_a_paste_or_a_csv_column() {
        let form = |targets: &str, extra: &[(&str, &str)]| {
            let mut f = vec![("targets".to_owned(), targets.to_owned())];
            f.extend(
                extra
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
            );
            f
        };
        let got = bulk_invites(
            &form(
                "email\n\"Ann@client.org\"\nbob@client.org, @Partner.org\nann@client.org",
                &[("roles", "Viewer"), ("max_uses", "1")],
            ),
            Some(3),
        )
        .unwrap();
        let targets: Vec<&str> = got.iter().map(|i| i.target.as_str()).collect();
        assert_eq!(
            targets,
            ["ann@client.org", "bob@client.org", "@partner.org"]
        );
        assert!(got
            .iter()
            .all(|i| i.max_uses == Some(1) && i.invited_by == Some(3)));
        assert_eq!(got[0].roles.as_deref(), Some("Viewer"));
        assert!(bulk_invites(&form("gmail", &[]), None).is_err());
        assert!(bulk_invites(&form("a@b.org", &[("max_uses", "0")]), None).is_err());
        assert!(bulk_invites(&form("", &[]), None).is_err());
    }
}
