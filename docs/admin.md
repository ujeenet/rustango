# The admin

**Rustango** generates a full admin UI from your models — the same idea as
Django's admin or a Laravel Nova/Filament panel, but with zero per-model
boilerplate. Add `#[derive(Model)]`, mount the admin once, and every model gets
list, search, filter, create, edit, delete, and an audit trail.

> **Runnable version:** every feature on this page is exercised in a tested
> example at [`crates/rustango/examples/admin_demo`](../crates/rustango/examples/admin_demo).
> If a snippet looks off, diff against it.

---

## Mount it

The admin is an `axum::Router` you build from a pool and nest under a path:

```rust
use rustango::admin;

let admin_router = admin::Builder::new(pool.clone())
    .title("Admin Demo")
    .admin_prefix("/admin")          // MUST match the nest path below
    .build();

let api = urls::api().nest("/admin", admin_router);
```

The auto-admin discovers every `#[derive(Model)]` in your binary automatically
(via the `inventory` registry) — you don't register models one by one. Open
`http://localhost:8080/admin` and you'll see them grouped in the sidebar.

> **`admin_prefix` must equal the nest path.** The admin builds its links and
> form actions from `admin_prefix` (default `/__admin`). If you nest at `/admin`
> but leave the default prefix, every link 404s. Set them the same.

---

## Configure a model: the `admin(...)` block

Everything about how a model appears is set in an `admin(...)` block on the
derive. Here's the showcase `Post` from the example:

```rust
#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display       = "id, title, author_id, status, view_count, published_at",
        list_display_links = "id, title",
        list_filter        = "status, author_id",
        search_fields      = "title, body",
        search_help_text   = "Search posts by title or body",
        ordering           = "-published_at",
        list_per_page      = 10,
        date_hierarchy     = "published_at",
        fieldsets          = "Content: title, body, status | Publishing: author_id, published_at, view_count",
        actions            = "publish, archive",
    ),
    audit(track = "title, body, status"),
)]
pub struct Post { /* … */ }
```

### Every `admin(...)` option

| Key | Example | What it does |
|---|---|---|
| `list_display` | `"id, title, status"` | Columns shown in the list, in order. FK columns render the target's display value. Empty = every scalar field. |
| `list_display_links` | `"id, title"` | Which `list_display` cells link to the detail/edit page. |
| `list_filter` | `"status, author_id"` | Right-rail facet cards — distinct values + counts, click to filter. |
| `search_fields` | `"title, body"` | Fields the `?q=` search box matches. |
| `search_help_text` | `"Search by title"` | Caption beside the search box. |
| `ordering` | `"-published_at"` | Default sort (`-` = DESC). |
| `list_per_page` | `10` | Page size (0 = admin default of 50). |
| `date_hierarchy` | `"published_at"` | Year → month → day drill-down strip above the list. |
| `fieldsets` | `"Content: title, body \| Meta: status"` | Group the edit form into titled sections (pipe-separated; `Title:` legend optional). |
| `actions` | `"publish, archive"` | Bulk actions in the list's action picker (see below). |
| `readonly_fields` | `"created_at"` | Fields shown as text on the edit form. |
| `raw_id_fields` | `"author_id"` | FK fields edited via an id + lookup link (good for large tables). |
| `autocomplete_fields` | `"author_id"` | FK fields edited via an Ajax typeahead. |
| `prepopulated_fields` | `"slug:title"` | Auto-fill a field by slugifying another (`target:source`). |
| `list_select_related` | `"all"` / `"author_id"` | Controls auto-JOIN of FK columns in the list. |
| `formfield_overrides` | `"status:textarea"` | Override a field's form widget (`field:widget`). |
| `actions_on_top` / `actions_on_bottom` | `true` / `false` | Where the action bar renders (default top). |

That single block produces a list view with sortable columns, the search box +
help text, the status/author facet cards with live counts, the date drill-down,
pagination at 10/page, and the publish/archive action picker — all visible in
the example's `/admin/posts`.

---

## Bulk actions

Name the actions in `admin(actions = "...")`, then register a handler per action
on the `Builder`. The handler receives the pool and the selected rows' primary
keys:

```rust
use rustango::core::SqlValue;

let admin_router = admin::Builder::new(pool)
    .register_action("posts", "publish", |pool, pks| {
        Box::pin(async move {
            let ids: Vec<String> = pks.iter().filter_map(|v| match v {
                SqlValue::I64(n) => Some(n.to_string()),
                _ => None,
            }).collect();
            if !ids.is_empty() {
                let sql = format!("UPDATE posts SET status='published' WHERE id IN ({})", ids.join(","));
                rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
            }
            Ok(())
        })
    })
    .build();
```

`delete_selected` is built in — you don't register it. Action names in
`admin(actions = ...)` without a registered handler simply won't appear.

---

## Inlines — edit related rows on the parent page

Show a child model's rows on the parent's detail page (Django inlines). Register
once, at module scope:

```rust
rustango::register_admin_inline!(
    parent = "posts",
    child  = "comments",
    fk     = "post_id",
    kind   = rustango::admin::inlines::InlineKind::Tabular,  // or Stacked
    label  = "Comments",
    fields = &["author_name", "body", "created_at"],
);
```

In the example, opening any post shows its comments in a "Comments" table below
the fields. (Inlines are read-only display today; an inline editor is on the
roadmap.)

---

## Audit trail

Add `audit(track = "field1, field2")` to a model and every create/update/delete
is recorded. The model's detail page grows an **Audit trail** card showing each
change (who, when, and a diff of the tracked fields), with a link to the full
history. The audit-log table is created for you when you run `migrate`.

```rust
#[rustango(table = "posts", audit(track = "title, body, status"))]
```

---

## `Builder` options

| Method | Purpose |
|---|---|
| `.title(s)` / `.subtitle(s)` | Sidebar header text. |
| `.admin_prefix(p)` | URL prefix (match your nest path). |
| `.theme_mode("light"\|"dark"\|"auto")` | Default colour theme. |
| `.brand_logo_url(url)` | Logo above the title. |
| `.show_only(["posts", "tags"])` | Whitelist which tables appear. |
| `.read_only(["audit_log"])` | Render a table but forbid writes. |
| `.with_session_auth(secret)` | Gate the admin behind cookie login (`/login` + `/logout`). |
| `.from_settings(pool, &settings)` | Build from your config file's `[admin]` / `[brand]` sections. |

By default the admin is **open** (anyone who can reach it can use it) — call
`.with_session_auth(...)` to require a login, or front it with your own auth
middleware.

---

## Beyond the basics

These extension points are registered with macros at module scope (each takes a
table name + a function); reach for them when the declarative `admin(...)` block
isn't enough:

- **Computed columns** — `register_admin_computed!("posts", "word_count", "Words", |row| …)` then add `word_count` to `list_display`.
- **Custom list filters** — `register_admin_list_filter!("posts", "by_status", "Status", &[("draft","Drafts")], to_filters_fn)` for filter logic the auto-facets can't express.
- **Custom admin pages** — `register_admin_view!("posts", "duplicate", Method::POST, "Duplicate", handler)` mounts an extra page/action under `/<admin>/posts/duplicate`.
- **Row-level permissions** — `register_admin_object_permission!("posts", "change", check_fn)` to allow/deny per row.
- **Queryset scoping** — `register_admin_queryset!("posts", hook_fn)` to filter what a request can see (e.g. only the current user's rows).

## In-admin model reference

Every admin ships a live model reference (Django's admindocs) at
`<admin_prefix>/__docs` — a read-only catalogue of every registered model with
its fields, types, and relations. Nothing to configure; it's built from your
models.

---

## Try the example

```bash
cd crates/rustango/examples/admin_demo
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/admin_demo
cargo run -- migrate     # tables + the audit-log table
cargo run                # seeds demo data, serves the admin at /admin
```

Then open <http://localhost:8080/admin> and click into **Posts** to see the
filters, search, date hierarchy, actions, inline comments, and audit trail in
one place.
