# The admin

**Rustango** generates a complete admin UI from your models — the same idea as
Django's admin or a Laravel Nova/Filament panel, but with **zero per-model
boilerplate**. Add `#[derive(Model)]`, mount the admin once, and every model gets
a list view with search, filters, sorting, pagination and bulk actions; a
create/edit form grouped into fieldsets; inline child editing; a per-row audit
trail; and a live model reference. Everything below is configured declaratively
in an `admin(...)` block on the derive, or with a handful of `Builder` methods
and module-scope registration macros.

[![The auto-generated admin: a posts list with filter facets, search, bulk actions, and pagination — all from one `admin(...)` block](img/admin.png)](img/admin.png)

> **Runnable version:** every feature on this page is exercised in a tested,
> compilable example at
> [`crates/rustango/examples/admin_demo`](../crates/rustango/examples/admin_demo).
> The screenshots on this page are that example. If a snippet looks off, diff
> against it.

---

## Contents

- [Mount it](#mount-it) · [The home page](#the-home-page)
- [Configure a model: the `admin(...)` block](#configure-a-model-the-admin-block)
- [The list view](#the-list-view) — columns, search, filters, date hierarchy, sorting, pagination
- [The change form](#the-change-form) — fieldsets, widgets, FK editing, prepopulated & readonly fields
- [Inlines](#inlines) · [Bulk actions](#bulk-actions) · [Audit trail](#audit-trail)
- [Computed columns & custom filters](#computed-columns-and-custom-filters)
- [Custom views, querysets & permissions](#custom-views-querysets-and-permissions)
- [Authentication](#authentication) · [Theming & branding](#theming-and-branding)
- [`Builder` reference](#builder-reference) · [Routes reference](#routes-reference)
- [The model reference (`__docs`)](#the-model-reference) · [Try the example](#try-the-example)

---

## Mount it

The admin is an `axum::Router` you build from a database pool and nest under a
path:

```rust
use rustango::admin;

let admin_router = admin::Builder::new(pool.clone())
    .title("Admin Demo")
    .subtitle("rustango auto-admin showcase")
    .admin_prefix("/admin")          // MUST match the nest path below
    .build();

let api = axum::Router::new().nest("/admin", admin_router);
```

The auto-admin discovers every `#[derive(Model)]` in your binary **automatically**
via the `inventory` registry — you don't register models one by one. Open
`http://localhost:8080/admin` and they appear grouped in the sidebar.

> **`admin_prefix` must equal the nest path.** The admin builds its links and
> form actions from `admin_prefix` (default `/__admin`). If you nest at `/admin`
> but leave the default prefix, every link 404s. Set them the same.

> **Linking the registry in.** Inventory registration only links into the final
> binary if the model types are referenced somewhere. A library crate whose
> models aren't otherwise used may need a `let _ = std::any::type_name::<Post>();`
> nudge in `main` (the example does this) — otherwise the linker drops them and
> they never appear.

### The home page

The admin root (`GET /<prefix>`) lists every registered model, grouped by app,
with each model's table name and field count — plus a **Recent actions** feed
of the latest audited changes.

[![The admin home: every registered model grouped by app with table and field counts, and a recent-actions activity feed](img/admin-home.png)](img/admin-home.png)

---

## Configure a model: the `admin(...)` block

Everything about how a model appears is set in an `admin(...)` block on the
derive. Here's the showcase `Post` from the example, which exercises almost
every knob:

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

`display = "title"` (on the model, outside `admin(...)`) sets the human label
used wherever a row is referenced — FK columns in other models' lists, the
breadcrumb, the detail-page title.

### Every `admin(...)` option

| Key | Example | What it does |
|---|---|---|
| `list_display` | `"id, title, status"` | Columns shown in the list, in order. FK columns render the target's `display` value. Computed columns (see below) can be named here. Empty = every scalar field. |
| `list_display_links` | `"id, title"` | Which `list_display` cells link to the detail page. Must be a subset of `list_display`. |
| `list_filter` | `"status, author_id"` | Right-rail facet cards — distinct values + counts, click to filter. Works on scalar and FK columns. |
| `search_fields` | `"title, body"` | Fields the `?q=` search box matches (case-insensitive `ILIKE`/`LIKE`). |
| `search_help_text` | `"Search by title"` | Caption rendered beside the search box. |
| `ordering` | `"-published_at, id"` | Default sort. `-` prefix = DESC; bare = ASC. Multiple keys comma-separated. |
| `list_per_page` | `10` | Page size (default 50). |
| `date_hierarchy` | `"published_at"` | Year → month → day drill-down strip above the list, on a Date/DateTime column. |
| `fieldsets` | `"Content: title, body \| Meta: status"` | Group the change form into titled sections. Pipe `\|` separates sections, comma separates fields; the `Title:` legend is optional. |
| `actions` | `"publish, archive"` | Bulk actions offered in the list's action picker (each needs a registered handler — see [Bulk actions](#bulk-actions)). |
| `readonly_fields` | `"created_at"` | Fields rendered as text (no input) on the change form. |
| `raw_id_fields` | `"author_id"` | FK fields edited via a raw id input + lookup link (good for large target tables). |
| `autocomplete_fields` | `"author_id"` | FK fields edited via an Ajax typeahead backed by the target's `__autocomplete` endpoint. |
| `prepopulated_fields` | `"slug:title"` | Auto-fill a field by slugifying another as you type (`target:source`; combine sources with `+`). |
| `list_select_related` | `"all"` / `"none"` / `"author_id"` | Controls auto-JOIN of FK columns in the list query. `"all"` (default) joins every FK; `"none"` disables; a CSV restricts to named FKs. |
| `formfield_overrides` | `"status:textarea"` | Override a field's form widget (`field:widget`) — see the [widget table](#form-widgets). |
| `actions_on_top` | `true` | Render the bulk-action bar above the list (default `true`). |
| `actions_on_bottom` | `false` | Render a second action bar below the list (default `false`). |

---

## The list view

`GET /<prefix>/<table>` renders the list. From the single `admin(...)` block
above you get sortable columns, a search box with help text, the status/author
facet cards with live counts, the date drill-down, pagination at 10/page, and
the publish/archive action picker.

**Filtering.** Click any value in a `list_filter` facet card to scope the list;
the active filter shows as a chip with a **clear** link, and the row count and
facet counts update. Filters, search, sorting and the date hierarchy all
compose in the query string and can be combined.

[![The posts list filtered by status=published: an active filter chip, the matching facet highlighted, search box, and the bulk-action picker](img/admin-list-filtered.png)](img/admin-list-filtered.png)

**Sorting.** Click a column header to sort; click again to flip direction
(`?sort=col&order=asc|desc`). The default comes from `ordering`.

**Pagination.** `list_per_page` sets the page size; navigate with `?page=N`.
For very large tables, register them with `Builder::skip_count_for([...])` to
skip the `SELECT COUNT(*)` (the pager then shows "Page N" without a grand
total); a per-request `?count=skip` does the same ad hoc.

**Search.** When `search_fields` is set, a search box appears and matches those
fields with `ILIKE` (Postgres) / `LIKE` (MySQL, SQLite). `search_help_text`
renders as its caption.

**Date hierarchy.** With `date_hierarchy` set, a year → month → day breadcrumb
sits above the table; drilling in adds half-open range filters on that column
using tri-dialect date extraction (Postgres `EXTRACT`, MySQL, SQLite `strftime`).

---

## The change form

`GET /<prefix>/<table>/new` (create) and `GET /<prefix>/<table>/<pk>/edit`
(edit) render the form. `fieldsets` groups the inputs into titled sections;
without it, all editable fields appear in one block.

[![The Post change form grouped into Content and Publishing fieldsets, each field with the right input widget for its type](img/admin-fieldsets.png)](img/admin-fieldsets.png)

Submitting a form validates the input, writes the row, records an audit entry,
and redirects to the read-only **detail** view
(`GET /<prefix>/<table>/<pk>`), which shows every field plus inlines and the
audit card (below). The detail page's **Edit** and **Delete** buttons lead to
the form and the delete confirmation.

### Form widgets

Each field renders an input matching its type by default — `<input type="number">`
for integers, `type="date"`/`datetime-local` for dates, `type="checkbox"` for
bools, a `<textarea>` for long strings, a `<select>` for FK columns, and so on.
Override per field with `formfield_overrides = "field:widget"`:

| Widget | Applies to | Renders |
|---|---|---|
| `textarea` | String | multi-line `<textarea>` |
| `password` | String | `<input type="password">` |
| `email` | String | `<input type="email">` |
| `url` | String | `<input type="url">` |
| `color` | String | `<input type="color">` |
| `slug` | String | text input with a slug pattern |
| `ipaddress` | String | text input with an IP pattern |
| `json` | Json | monospace `<textarea>` |
| `hidden` | any | `<input type="hidden">` |

### Editing foreign keys

FK columns have three editing modes:

- **Default** — a `<select>` populated from the target table, showing each row's
  `display` value.
- **`raw_id_fields`** — a plain id input plus a lookup link; best when the target
  table is too large to enumerate in a dropdown.
- **`autocomplete_fields`** — an Ajax typeahead that queries the target model's
  `search_fields` via `GET /<prefix>/<target>/__autocomplete?q=…`.

### Prepopulated & read-only fields

`prepopulated_fields = "slug:title"` emits client-side JS that slugifies the
source field into the target as you type (combine multiple sources with
`+`, e.g. `"slug:section+title"`). `readonly_fields` renders the named fields as
escaped text on the form instead of inputs.

---

## Inlines

Inlines show a child model's rows on the parent's page (Django inlines).
Register one at module scope:

```rust
rustango::register_admin_inline!(
    parent = "posts",
    child  = "comments",
    fk     = "post_id",                                     // child column → parent PK
    kind   = rustango::admin::inlines::InlineKind::Tabular, // or Stacked
    label  = "Comments",
    fields = &["author_name", "body", "created_at"],
);
```

On the parent's **detail** page the children render as a read-only table; on the
**edit** page they become an editable FormSet (add / change / delete rows in
place). Options: `kind` (`Tabular` — one table row per child, or `Stacked` — a
fieldset per child), `label`, `fields` (default: every scalar except the FK),
`extra` (blank rows offered for adding), `max_num`, and `readonly_fields`.

[![A post's detail page: read-only fields, the Comments inline table, and the audit-trail card showing the create entry as a JSON diff](img/admin-detail.png)](img/admin-detail.png)

For child rows attached by a generic foreign key (content-type + object-pk pair)
rather than a single FK column, use `register_admin_inline_generic!(parent, child,
ct = "content_type_id", pk = "object_pk", …)` — same options otherwise.

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
                SqlValue::I32(n) => Some(n.to_string()),
                _ => None,
            }).collect();
            if !ids.is_empty() {
                let sql = format!("UPDATE posts SET status='published' WHERE id IN ({})", ids.join(","));
                rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
            }
            Ok(())
        })
    })
    .register_action("posts", "archive", /* … */)
    .build();
```

Pick rows with the checkboxes, choose the action in the picker, and submit
(`POST /<prefix>/<table>/__action`). `delete_selected` is built in — you don't
register it. An action name listed in `admin(actions = ...)` without a
registered handler simply won't appear.

---

## Audit trail

Add `audit(track = "field1, field2")` to a model and every create, update and
delete is recorded to the `rustango_audit_log` table (created for you when you
run `migrate`). Only models with an `audit(...)` attribute are logged; `track`
selects which fields are captured in the diff (omit it to track all scalars).

```rust
#[rustango(table = "posts", audit(track = "title, body, status"))]
```

Each entry stores the table, primary key, operation, source, a per-field diff
(`{before, after}`) as JSON, the actor, a timestamp, and a tamper-evident
fingerprint. Two places surface it:

- The model's **detail page** grows an **Audit trail** card listing recent
  changes (who, when, and the diff), with a **View full history** link
  (shown in the [detail screenshot](#inlines) above).
- The sidebar's **Activity** view (`GET /<prefix>/__audit`) is a cross-row feed,
  newest first, with facet cards for entity / operation / source and a cleanup
  form to purge entries older than N days (itself recorded as an audit entry).

[![The Activity feed: every audited change across models with JSON diffs, facet cards by table/operation/source, and a cleanup form](img/admin-audit.png)](img/admin-audit.png)

---

## Computed columns and custom filters

When the declarative block isn't enough, two module-scope macros extend the list
view:

**Computed columns** — a derived, non-database column:

```rust
rustango::register_admin_computed!(
    "posts", "word_count", "Words",
    |row| row.get("body").and_then(|v| v.as_str())
             .unwrap_or_default().split_whitespace().count().to_string(),
);
// then add `word_count` to admin(list_display = "...").
```

The closure receives the row as `serde_json::Value` and returns pre-escaped
HTML. A 5-argument form adds `link = |row| Option<String>` to wrap the cell in
an `<a>`.

**Custom list filters** — filter logic the auto-facets can't express:

```rust
fn by_status(value: &str) -> Vec<rustango::admin::Filter> { /* map value → predicates */ }

rustango::register_admin_list_filter!(
    "posts", "status", "Status",
    &[("draft", "Drafts"), ("published", "Published")],   // (value, label) choices
    by_status,                                            // fn(&str) -> Vec<Filter>
);
```

---

## Custom views, querysets and permissions

Three more registration macros mirror Django's `ModelAdmin` hooks:

- **Custom admin pages** —
  `register_admin_view!("posts", "duplicate", Method::POST, "Duplicate", handler)`
  mounts an extra page/action at `/<prefix>/posts/duplicate`. The handler is an
  async `fn(Pool, Request) -> Response`. (Reserved suffixes like `new`,
  `__action`, `__autocomplete`, `{pk}`, `{pk}/edit`, `{pk}/delete` are skipped
  with a warning.)
- **Queryset scoping** —
  `register_admin_queryset!("posts", hook)` where `hook: fn(&Parts) -> Vec<Filter>`
  narrows what a request can see (e.g. only the current user's rows). Multiple
  hooks on a table compose.
- **Row-level permissions** —
  `register_admin_object_permission!("posts", "change", check)` where
  `check: fn(&Parts, Option<&Value>) -> bool` allows or denies per row. Built-in
  handlers consult the `add`, `change`, `delete` and `view` actions; multiple
  hooks AND together.

For coarser, codename-based access control, `Builder::with_user_perms([...])`
gates each table on `{table}.view` / `.add` / `.change` / `.delete`: missing
`view` hides the model and 404s direct hits, missing `change` renders it
read-only, and missing `add` / `delete` removes those buttons.

---

## Authentication

By default the admin is **open** — anyone who can reach it can use it. Gate it in
one of two ways:

- **Session auth (built in).** `Builder::with_session_auth(secret)` mounts
  `/login` + `/logout` (and an optional `/account/password` change page) and
  wraps every other route in middleware that redirects anonymous requests to the
  login form. Credentials live in the `rustango_admin_users` table
  (`username`, argon2 `password_hash`, `is_superuser`, `active`, `created_at`);
  changing a password revokes that user's other sessions. Optional TOTP
  two-factor is available behind the `totp` feature, with enrollment under
  `/account/totp`.

  ```rust
  let admin = admin::Builder::new(pool)
      .with_session_auth(session_secret)
      .secure_cookies(true)              // HTTPS-only cookie in production
      .build();
  ```

- **Front it with your own auth.** Leave the admin open and put HTTP Basic auth,
  OAuth2, or corporate SSO in front of the nest path with your own middleware.

---

## Theming and branding

| Method | Effect |
|---|---|
| `.theme_mode("light" \| "dark" \| "auto")` | Default colour theme (sets `data-theme` on `<html>`). |
| `.title(s)` / `.subtitle(s)` | Sidebar header text. |
| `.brand_logo_url(url)` | Logo rendered above the title. |
| `.brand_name(s)` / `.brand_tagline(s)` | Per-tenant overrides of title/subtitle. |
| `.tenant_brand_css(css)` | A pre-built `:root{…}` CSS-variable block, inlined for per-tenant palettes. |
| `.from_settings(pool, &settings)` | Build branding + visibility from your config file's `[admin]` / `[brand]` sections. |

`from_settings` reads `admin.title`, `admin.subtitle`, `admin.logo_url`,
`admin.theme_mode`, `admin.url_prefix`, `admin.allowed_tables`,
`admin.read_only_tables`, falling back to the `[brand]` section, and defaults
`secure_cookies` to `true`. Imperative `Builder` calls after it still win.

---

## `Builder` reference

Every method on `admin::Builder` (each returns `Self` for chaining unless noted):

| Method | Purpose |
|---|---|
| `new(pool)` | Construct from any pool (Postgres / MySQL / SQLite). Defaults: prefix `/__admin`, dev cookies. |
| `from_settings(pool, &settings)` | Construct from parsed config (`config` feature). |
| `title(s)` / `subtitle(s)` | Sidebar header / subheader. |
| `admin_prefix(p)` | URL prefix — **must match the nest path**. Default `/__admin`. |
| `audit_url(u)` | Path of the activity/audit view. Default `/__audit`. |
| `static_url(u)` | Prefix for embedded assets (favicon, logo). Default `/__static__`. |
| `change_password_url(u)` | Path of the self-serve password-change page (adds the sidebar link). |
| `show_only([tables])` | Whitelist which tables appear; others 404 and are hidden. |
| `read_only([tables])` | Render those tables but forbid create/edit/delete. |
| `read_only_all()` | Mark **every** table read-only. |
| `skip_count_for([tables])` | Skip `COUNT(*)` on huge tables (pager shows "Page N"). |
| `with_user_perms([codenames])` | Gate tables on `{table}.view/add/change/delete`. |
| `register_action(table, name, handler)` | Register a bulk-action handler. |
| `with_session_auth(secret)` | Require cookie login (`/login` + `/logout`). |
| `secure_cookies(bool)` | Set the `Secure` (HTTPS-only) flag on the session cookie. |
| `theme_mode(m)` | `"light"` / `"dark"` / `"auto"`. |
| `brand_logo_url(url)` | Logo above the title. |
| `brand_name(s)` / `brand_tagline(s)` | Per-tenant brand overrides. |
| `tenant_brand_css(css)` | Per-tenant CSS-variable block. |
| `impersonated_by(operator_id)` | Render an impersonation banner (operator console). |
| `tenant_mode()` | Hide registry-scoped models (set automatically for tenant admins). |
| `build()` | Finalize and return the `axum::Router`. |

---

## Routes reference

All paths are relative to `admin_prefix`:

| Path | Method | What |
|---|---|---|
| `/` | GET | Home — model index + recent actions. |
| `/<table>` | GET | List view (search, filters, sort, pagination). |
| `/<table>` | POST | Create-submit. |
| `/<table>/new` | GET | Create form. |
| `/<table>/<pk>` | GET | Detail (read-only) view, with inlines + audit card. |
| `/<table>/<pk>` | POST | Update-submit. |
| `/<table>/<pk>/edit` | GET | Edit form. |
| `/<table>/<pk>/delete` | POST | Delete (after confirmation). |
| `/<table>/__action` | POST | Run a bulk action on selected PKs. |
| `/<table>/__autocomplete` | GET | FK typeahead JSON (`?q=`). |
| `/__docs` | GET | Model reference. |
| `/__audit` (or `audit_url`) | GET | Activity feed + cleanup. |
| `/login`, `/logout` | GET/POST | Session auth (when enabled). |
| `/account/password`, `/account/totp` | GET/POST | Self-serve password change / TOTP enrollment. |

Custom routes registered with `register_admin_view!` mount at
`/<table>/<suffix>`.

---

## The model reference

Every admin ships a live model reference (Django's admindocs) at
`<prefix>/__docs` — a read-only catalogue of every registered model with its
fields, columns, types, flags (PK, unique, …) and relations. Nothing to
configure; it's generated from your models, so it never drifts from the schema.

[![The model reference: every model's fields with column name, Rust type, flags, and relations — generated from the models](img/admin-model-reference.png)](img/admin-model-reference.png)

---

## Try the example

```bash
cd crates/rustango/examples/admin_demo
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/admin_demo
cargo run -- migrate     # tables + the audit-log table
cargo run                # seeds demo data, serves the admin at /admin
```

Then open <http://localhost:8080/admin> and click into **Posts** to see the
filters, search, date hierarchy, actions, fieldsets, inline comments, audit
trail, and model reference — every screenshot on this page — in one place.
