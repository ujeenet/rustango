//! `edit-tenant` — change a tenant's routing and display config (#1344).
//!
//! The console has edited these since v0.25; the CLI could not, so moving
//! a tenant to a new hostname or rotating its credential meant a browser.
//!
//! The work is in [`crate::tenancy::org_edit`], which both surfaces call:
//! the write has to be followed by dropping the cached `Org` and, only on
//! a real URL change, evicting the pool. Leaving out the cache drop is
//! invisible in testing and reports success while the next request still
//! uses the old row.

use std::io::Write;

use sqlx::Database;

use crate::manage_interactive;
use crate::tenancy::error::TenancyError;
use crate::tenancy::org_edit::{apply, OrgPatch};
use crate::tenancy::pools::TenantPools;

use super::args::next_value;

/// The slug and the patch, from argv.
fn parse_args(args: &[String]) -> Result<(Option<String>, OrgPatch), TenancyError> {
    let mut patch = OrgPatch::default();
    let mut slug: Option<String> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--display-name" => patch.display_name = Some(next_value(&mut iter, arg)?),
            "--host-pattern" => patch.host_pattern = Some(next_value(&mut iter, arg)?),
            "--path-prefix" => patch.path_prefix = Some(next_value(&mut iter, arg)?),
            "--port" => patch.port = Some(next_value(&mut iter, arg)?),
            "--database-url" => patch.database_url = Some(next_value(&mut iter, arg)?),
            "--activate" => patch.active = Some(true),
            "--deactivate" => patch.active = Some(false),
            // An explicit empty value is how you clear an optional column;
            // `--clear <field>` says so without relying on `--x ""`, which
            // some shells and CI runners eat.
            "--clear" => {
                let field = next_value(&mut iter, "--clear")?;
                match field.as_str() {
                    "host-pattern" => patch.host_pattern = Some(String::new()),
                    "path-prefix" => patch.path_prefix = Some(String::new()),
                    "port" => patch.port = Some(String::new()),
                    // `display_name` is NOT NULL, so there is no "cleared"
                    // state to ask for — `--display-name ""` is the whole
                    // of what clearing it could mean.
                    other => {
                        return Err(TenancyError::Validation(format!(
                            "cannot clear `{other}` — the clearable fields are host-pattern, \
                             path-prefix, port"
                        )))
                    }
                }
            }
            other if other.starts_with("--") => {
                return Err(TenancyError::Validation(format!(
                    "unknown flag `{other}` — run `edit-tenant --help`"
                )))
            }
            positional => {
                if slug.is_some() {
                    return Err(TenancyError::Validation(format!(
                        "unexpected argument `{positional}`"
                    )));
                }
                slug = Some(positional.to_owned());
            }
        }
    }
    Ok((slug, patch))
}

pub(super) async fn edit_tenant<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (slug, patch) = parse_args(args)?;

    let slug = match slug {
        Some(s) => s,
        None => manage_interactive::ask("Tenant slug: ")
            .map_err(TenancyError::Io)?
            .ok_or_else(|| TenancyError::Validation("edit-tenant requires a slug".into()))?,
    };

    let registry = pools.registry_pool();
    let applied = apply(&registry, &slug, &patch).await?;

    // Only on a real change — evicting for a display name throws away warm
    // connections for nothing.
    if applied.database_url_rotated {
        pools.invalidate(&slug).await;
    }

    writeln!(w, "updated `{slug}`: {}", applied.touched.join(", "))?;
    if applied.database_url_rotated {
        writeln!(
            w,
            "  pool evicted — the next request rebuilds with the new URL"
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<(Option<String>, OrgPatch), TenancyError> {
        let args: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        parse_args(&args)
    }

    fn patch(argv: &[&str]) -> OrgPatch {
        parse(argv).expect("parse").1
    }

    /// "Leave alone" and "clear it" are different instructions, and a patch
    /// that cannot tell them apart would wipe fields on every edit.
    #[test]
    fn unmentioned_is_not_the_same_as_cleared() {
        let only_name = patch(&["acme", "--display-name", "Acme"]);
        assert!(only_name.host_pattern.is_none(), "not mentioned");

        let cleared = patch(&["acme", "--clear", "host-pattern"]);
        assert_eq!(cleared.host_pattern.as_deref(), Some(""), "cleared");
        assert!(cleared.display_name.is_none());
    }

    #[test]
    fn activate_and_deactivate_are_opposite_flags() {
        assert_eq!(patch(&["acme", "--activate"]).active, Some(true));
        assert_eq!(patch(&["acme", "--deactivate"]).active, Some(false));
        assert_eq!(patch(&["acme", "--display-name", "x"]).active, None);
    }

    #[test]
    fn the_slug_is_the_only_positional() {
        let (slug, _) = parse(&["acme", "--activate"]).expect("parse");
        assert_eq!(slug.as_deref(), Some("acme"));

        // Flags before the slug still work.
        let (slug, _) = parse(&["--activate", "acme"]).expect("parse");
        assert_eq!(slug.as_deref(), Some("acme"));

        let err = parse(&["acme", "globex"]).expect_err("two slugs");
        assert!(err.to_string().contains("globex"), "{err}");
    }

    #[test]
    fn an_unknown_flag_or_clear_target_is_named() {
        let err = parse(&["acme", "--nope"]).expect_err("unknown flag");
        assert!(err.to_string().contains("--nope"), "{err}");

        let err = parse(&["acme", "--clear", "slug"]).expect_err("not clearable");
        assert!(err.to_string().contains("slug"), "{err}");
        assert!(
            err.to_string().contains("host-pattern"),
            "should list what is: {err}"
        );
    }
}
