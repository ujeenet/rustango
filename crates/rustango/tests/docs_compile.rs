//! The `rust` fences of `docs/orm.md` and the cookbook compile.
//!
//! Each fence not in [`KNOWN_FAILING`] is generated into
//! `tests/docs_compile/<page>_blocks.rs`, which the `docs_compile_orm` and
//! `docs_compile_cookbook` targets compile on SQLite. This test fails when a
//! page and its generated file differ. Regenerate with
//! `DOCS_COMPILE_BLESS=1 cargo test -p rustango --test docs_compile`.
//!
//! `rust,ignore` fences are not compiled; their count per page may only
//! shrink. A fence is keyed by a hash of its text, so editing a listed fence
//! takes it off the list: it then has to compile. The list may only shrink.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// `(page, hash, first line)`: fences that do not compile yet. Lower
/// [`KNOWN_FAILING_MAX`] with every entry you remove.
const KNOWN_FAILING: &[(&str, &str, &str)] = &[
    ("COOKBOOK.md", "04121ecc", "// One-liner that loads via load_from_env() and applies the entire"),
    ("COOKBOOK.md", "1e75dae3", "use rustango::template_views::{ListView, DetailView};"),
    ("COOKBOOK.md", "2a547261", "use rustango::mcp::{McpContext, McpError};"),
    ("COOKBOOK.md", "3e426ec1", "#[rustango("),
    ("COOKBOOK.md", "4483bd22", "#[derive(Model)]"),
    ("COOKBOOK.md", "4ab25361", "#[derive(Model)]"),
    ("COOKBOOK.md", "4da20f6e", "#[rustango(table = \"geo_audit\", required_db_vendor = \"postgres\")]"),
    ("COOKBOOK.md", "53f7e77d", "#[rustango(max_length = 200, citext)] pub email: String,"),
    ("COOKBOOK.md", "56c2889d", "let mut act = Activity {"),
    ("COOKBOOK.md", "57d39bb3", "// .when(cond, |qs| ...) / .unless(cond, |qs| ...) / .tap(|&qs| ...)"),
    ("COOKBOOK.md", "5afba9fe", "#[rustango(fk = \"post\", on = \"id\", on_delete = \"cascade\")]"),
    ("COOKBOOK.md", "5cfbadc3", "#[rustango(table = \"post\", permissions, extra_permissions = \"approve:Can approve posts, archive:Can archive posts\")]"),
    ("COOKBOOK.md", "60f5af98", "// Reads RUSTANGO_ENV (defaults to \"dev\"), runs the layered load:"),
    ("COOKBOOK.md", "61c2fffd", "// Model::paginate(page, per_page, &pool) -> (rows, total) —"),
    ("COOKBOOK.md", "63044793", "const EMBEDDED: &[rustango::migrate::Migration] ="),
    ("COOKBOOK.md", "656c4d73", "#[rustango("),
    ("COOKBOOK.md", "671529dc", "#[rustango::main]"),
    ("COOKBOOK.md", "6e51c991", "ensure_seeded(&pool).await?;"),
    ("COOKBOOK.md", "739df98f", "#[rustango("),
    ("COOKBOOK.md", "77582d1d", "let mut tera = rustango::template_extensions::html_tera_from_glob(&templates_glob)?;"),
    ("COOKBOOK.md", "811c5aef", "#[derive(Model)]"),
    ("COOKBOOK.md", "86ab9773", "#[rustango("),
    ("COOKBOOK.md", "8ff59bc0", "rustango::register_mcp_resource!("),
    ("COOKBOOK.md", "95943ed7", "let api = axum::Router::new()"),
    ("COOKBOOK.md", "9781d831", "#[rustango(table = \"country\", permissions, default_permissions = \"view\")]"),
    ("COOKBOOK.md", "98c57dc3", "#[rustango(table = \"orders\", db_table_comment = \"Customer purchase records — see /docs/orders.md\")]"),
    ("COOKBOOK.md", "997fff7c", "let cfg = rustango::config::Settings::load_from_env()?;"),
    ("COOKBOOK.md", "9e08df0d", "#[rustango("),
    ("COOKBOOK.md", "b090bc44", "use rustango::core::{Filter, Op, SqlValue, WhereExpr};"),
    ("COOKBOOK.md", "b6abb412", "use rustango::tenancy::RouteConfig;"),
    ("COOKBOOK.md", "bb85f197", "#[rustango("),
    ("COOKBOOK.md", "bd955c96", "#[derive(Model)]"),
    ("COOKBOOK.md", "c6640b53", "// Find-or-default in one call. Returns (row, exists: bool) so"),
    ("COOKBOOK.md", "cc7e33a1", "#[derive(Model)]"),
    ("COOKBOOK.md", "d13b66a5", "use rustango::tenancy::TenantPoolsConfig;"),
    ("COOKBOOK.md", "e184aeeb", "use rustango::template_views::{ListView, DetailView, CreateView, UpdateView, DeleteView};"),
    ("COOKBOOK.md", "e70be996", "#[rustango(table = \"post\", get_latest_by = \"-created_at\")]"),
    ("COOKBOOK.md", "eae5f028", "#[derive(Model)]"),
    ("COOKBOOK.md", "ee7736d7", "#[derive(Model)]"),
    ("COOKBOOK.md", "f8c02602", "#[rustango::main]"),
    ("orm.md", "011539ad", "// 1. HTTP query string (set via ViewSet filter_fields)"),
    ("orm.md", "0a6d5e63", "use axum::Router;"),
    ("orm.md", "15f99a0f", "post.soft_delete(&pool).await?;        // sets deleted_at = NOW()"),
    ("orm.md", "160884cc", "use rustango::core::aggregates::max;"),
    ("orm.md", "26a69b5d", "rustango::audit::cleanup_older_than_pool(&pool, 90).await?;  // delete > 90 days"),
    ("orm.md", "271016ec", "concat([F(\"first\").into(), \" \".into(), F(\"last\").into()])"),
    ("orm.md", "291ebd0a", "use rustango::core::aggregates::{count_all, sum};"),
    ("orm.md", "2b22a706", "use rustango::core::F;"),
    ("orm.md", "2e031647", "let mut post = Post::objects().find_or_fail(1, &pool).await?;"),
    ("orm.md", "35a7271f", "Post::objects().where_(Post::view_count.gt(100)).fetch(&pool).await?;"),
    ("orm.md", "3b596b1c", "#[derive(Model)]"),
    ("orm.md", "3daac75f", "// Writer A — flips title."),
    ("orm.md", "4a77e5c5", "use rustango::sql::CounterPool as _;"),
    ("orm.md", "5517956d", "use rustango::extractors::Tenant;"),
    ("orm.md", "56c98699", "// Upsert on the PK — INSERT, or UPDATE every non-PK column if the"),
    ("orm.md", "5874077e", "#[rustango("),
    ("orm.md", "5d2abf75", "let author_posts = author.post_set(&pool).await?;"),
    ("orm.md", "63ee9b70", "use rustango::core::funcs::{lower, upper, concat, coalesce, trim, abs, round};"),
    ("orm.md", "6de0b562", "// Outer .order_by() / .limit() / .offset() / .select_for_update()"),
    ("orm.md", "7b2e5d58", "let mut tx = pool.begin().await?;"),
    ("orm.md", "7b36c432", "// ON CONFLICT (external_id) DO UPDATE SET title = EXCLUDED.title"),
    ("orm.md", "7ceb5218", "let tag_ids: Vec<i64> = post.tags_m2m().all(&pool).await?;"),
    ("orm.md", "83077892", "// Bulk INSERT — rows FIRST (a `&mut [Self]`), executor/pool second."),
    ("orm.md", "855509e3", "use rustango::audit::{with_source, AuditSource};"),
    ("orm.md", "861e148c", "use rustango::core::{Expr, Op, SqlValue, WhereExpr};"),
    ("orm.md", "8a0b4213", "let mut tx = pool.begin().await?;"),
    ("orm.md", "a0231746", "use sqlx::sqlite::SqliteConnectOptions;"),
    ("orm.md", "a1ebe5f5", "use rustango::core::{FrameBoundary, FrameKind, WindowFrame};"),
    ("orm.md", "aee99193", "use rustango::core::{Op, SqlValue};"),
    ("orm.md", "b3141bbc", "use rustango::core::funcs::{"),
    ("orm.md", "c2aca856", "use rustango::core::Column as _;"),
    ("orm.md", "ceb8de63", "post.save_partial_typed((Post::title, Post::slug), &pool).await?;"),
    ("orm.md", "d3d426f9", "// Canonical \"claim next available row\" pattern. Worker A grabs the"),
    ("orm.md", "d91565ef", "let posts = Post::objects()"),
    ("orm.md", "db8435cb", "use rustango::sql::sqlx;"),
    ("orm.md", "e115b399", "use rustango::core::joins::aliased;"),
    ("orm.md", "ee6e61e1", "// Page-number — page 2 of 50-row pages = LIMIT 50 OFFSET 50."),
    ("orm.md", "f0c6d25d", "// .only(...) — fetch only the named columns."),
];

/// The size of [`KNOWN_FAILING`]. It may only go down.
const KNOWN_FAILING_MAX: usize = 78;

struct Page {
    name: &'static str,
    /// Relative to this crate's manifest.
    source: &'static str,
    generated: &'static str,
    /// `rust,ignore` fences, which are not compiled. May only go down.
    uncompiled_max: usize,
}

const PAGES: &[Page] = &[
    Page {
        name: "orm.md",
        source: "../../docs/orm.md",
        generated: "tests/docs_compile/orm_blocks.rs",
        uncompiled_max: 0,
    },
    Page {
        name: "COOKBOOK.md",
        source: "examples/cookbook_blog/COOKBOOK.md",
        generated: "tests/docs_compile/cookbook_blocks.rs",
        uncompiled_max: 25,
    },
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A fenced block whose info string starts with the `rust` tag.
struct Fence {
    /// The info string, e.g. `rust` or `rust,ignore`.
    tags: String,
    /// De-indented.
    body: String,
}

impl Fence {
    /// Only a bare `rust` fence is compiled; `rust,ignore` is budgeted.
    fn compiled(&self) -> bool {
        self.tags == "rust"
    }
}

fn fences(markdown: &str) -> Vec<Fence> {
    let mut out = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        let Some(tags) = line.trim_start().strip_prefix("```") else {
            continue;
        };
        if tags.split(',').next().map(str::trim) != Some("rust") {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let mut body = String::new();
        for l in lines.by_ref() {
            if l.trim_start() == "```" {
                break;
            }
            body.push_str(l.get(indent..).unwrap_or(l.trim_start()));
            body.push('\n');
        }
        out.push(Fence {
            tags: tags.trim().to_owned(),
            body,
        });
    }
    out
}

/// FNV-1a: stable across toolchains, unlike `DefaultHasher`.
fn hash(text: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in text.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// A fence of items (models, fns) rather than statements.
fn is_items(body: &str) -> bool {
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("use "))
        .unwrap_or("");
    [
        "#[",
        "pub ",
        "fn ",
        "async fn ",
        "struct ",
        "enum ",
        "impl",
        "const ",
        "static ",
        "mod ",
        "trait ",
        "type ",
    ]
    .iter()
    .any(|p| first.starts_with(p))
}

fn indent(body: &str, by: usize) -> String {
    body.lines()
        .map(|l| {
            if l.is_empty() {
                "\n".to_owned()
            } else {
                format!("{:by$}{l}\n", "")
            }
        })
        .collect()
}

fn is_known_failing(page: &str, h: &str) -> bool {
    KNOWN_FAILING.iter().any(|(p, k, _)| *p == page && *k == h)
}

fn generate(page: &Page) -> String {
    let markdown = std::fs::read_to_string(root().join(page.source)).unwrap();
    let mut out = format!(
        "// @generated by tests/docs_compile.rs from {}; do not edit.\n",
        page.name
    );
    let mut seen = std::collections::BTreeSet::new();
    for body in fences(&markdown)
        .into_iter()
        .filter(Fence::compiled)
        .map(|f| f.body)
    {
        let h = hash(&body);
        if !seen.insert(h.clone()) || is_known_failing(page.name, &h) {
            continue;
        }
        writeln!(
            out,
            "\n// {}\nmod b_{h} {{\n    use crate::prelude::*;",
            first_line(&body)
        )
        .unwrap();
        if is_items(&body) {
            write!(out, "{}}}\n", indent(&body, 4)).unwrap();
        } else {
            write!(
                out,
                "    pub async fn run(pool: Pool) -> Result<(), BoxError> {{\n        let _ = {{\n{}        }};\n        Ok(())\n    }}\n}}\n",
                indent(&body, 12)
            )
            .unwrap();
        }
    }
    out
}

#[test]
fn generated_files_match_the_pages() {
    let bless = std::env::var_os("DOCS_COMPILE_BLESS").is_some();
    for page in PAGES {
        let want = generate(page);
        let path = root().join(page.generated);
        if bless {
            std::fs::write(&path, &want).unwrap();
            continue;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "{} is stale for {}; rerun with DOCS_COMPILE_BLESS=1",
            page.generated,
            page.name
        );
    }
}

#[test]
fn every_known_failing_entry_names_a_current_fence() {
    for (page, h, line) in KNOWN_FAILING {
        let source = PAGES.iter().find(|p| p.name == *page).expect(page).source;
        let markdown = std::fs::read_to_string(root().join(source)).unwrap();
        let found = fences(&markdown)
            .iter()
            .any(|f| f.compiled() && hash(&f.body) == *h);
        assert!(
            found,
            "{page} has no fence {h} ({line}); drop it from KNOWN_FAILING"
        );
    }
}

#[test]
fn the_known_failing_list_only_shrinks() {
    assert!(
        KNOWN_FAILING.len() <= KNOWN_FAILING_MAX,
        "KNOWN_FAILING grew past {KNOWN_FAILING_MAX}: new fences must compile"
    );
    assert_eq!(
        KNOWN_FAILING.len(),
        KNOWN_FAILING_MAX,
        "KNOWN_FAILING shrank: lower KNOWN_FAILING_MAX to match"
    );
}

#[test]
fn the_fence_parser_sees_every_rust_fence() {
    for page in PAGES {
        let markdown = std::fs::read_to_string(Path::new(&root()).join(page.source)).unwrap();
        // Counted apart from `fences`: every opening line whose first tag is `rust`.
        let opened = markdown
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("```rust"))
            .filter(|rest| rest.is_empty() || rest.starts_with(','))
            .count();
        assert_eq!(fences(&markdown).len(), opened, "{}", page.name);
    }
}

#[test]
fn uncompiled_rust_fences_only_shrink() {
    for page in PAGES {
        let markdown = std::fs::read_to_string(root().join(page.source)).unwrap();
        let n = fences(&markdown).iter().filter(|f| !f.compiled()).count();
        assert!(
            n <= page.uncompiled_max,
            "{}: {n} `rust,ignore` fences, budget {}: make the new one compile",
            page.name,
            page.uncompiled_max
        );
        assert_eq!(
            n, page.uncompiled_max,
            "{}: `rust,ignore` fences went down to {n}: lower its `uncompiled_max`",
            page.name
        );
    }
}
