# HTML views — server-rendered pages

An HTML view turns a model into **server-rendered web pages** — a list page, a
detail page, and create/edit/delete forms — from one declaration. It's the
**sibling of [ViewSets](viewsets.md)**: where a ViewSet emits JSON for API
clients, an HTML view emits a rendered page for a browser. Both are built from
the same `#[derive(Model)]`, and you can serve a model *both* ways at once.

There are five ready-made views — `ListView`, `DetailView`, `CreateView`,
`UpdateView`, `DeleteView` — one per page of a model's create/read/update/delete
flow. They render through [Tera](https://keats.github.io/tera/)
templates.

[![HTML views in Rustango: one model feeds ListView, DetailView and CreateView/UpdateView/DeleteView, each rendering a Tera template into a server-rendered page](img/html-views.png)](img/html-views.png)

> **New to a term here?** If *model*, *template*, *router* or *server-rendered* are
> unfamiliar, the [glossary](glossary.md) explains each in plain language.

> **Source:** `rustango::template_views` (`ListView`, `DetailView`, `CreateView`,
> `UpdateView`, `DeleteView`, `TemplateView`, `RedirectView`) — behind the
> `template_views` feature (on by default).
>
> **Runnable version:** the API-vs-HTML example below is pinned by the
> framework test
> [`html_and_api_contrast_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs)
> (`cargo test -p rustango --features sqlite --test html_and_api_contrast_sqlite_live`).
> The individual views are covered by `template_view.rs` and
> `template_views_context_object_name_sqlite_live.rs`.

## Table of contents

- [API views vs HTML views — which do you want?](#api-views-vs-html-views--which-do-you-want)
- [The five model views](#the-five-model-views)
- [ListView](#listview) · [DetailView](#detailview)
- [CreateView, UpdateView, DeleteView](#createview-updateview-deleteview)
- [The Tera context](#the-tera-context)
- [TemplateView and RedirectView](#templateview-and-redirectview)
- [Single-tenant vs multi-tenant](#single-tenant-vs-multi-tenant)
- [Serving one model both ways](#serving-one-model-both-ways)
- [Error pages](#error-pages)
- [See also](#see-also)

---

## API views vs HTML views — which do you want?

This is the first decision. Both turn a model into endpoints; they differ in
*what comes out* and *who's calling*.

| | **API view** — [ViewSet](viewsets.md) | **HTML view** — this guide |
|---|---|---|
| Module | `rustango::viewset` | `rustango::template_views` |
| Sends back | **JSON data** | a **server-rendered HTML page** |
| Built for | SPAs, mobile apps, other services | browsers, server-rendered sites, admin-style CRUD |
| A "create" | `POST` JSON → `201` + the new object | `POST` a form → `303` redirect to a success page |
| On bad input | `422` + a field-keyed JSON error map | re-render the form with the errors shown |
| Reads a list as | a paginated JSON envelope | a `<table>`/loop in your template |
| Usually authed by | tokens / JWT / API keys | session cookies |

You don't have to choose globally — pick per resource, and you can mount **both
on the same model** (see [below](#serving-one-model-both-ways)). Rules of thumb:

- Building a **JSON backend** for a frontend framework or mobile app → ViewSet.
- Building a **server-rendered site** (the server returns HTML pages) → HTML
  views.
- Need both (a public API *and* internal CRUD pages) → mount both.

> Looking for the JSON side? It has its own deep-dive: [ViewSets — CRUD REST
> APIs](viewsets.md).

---

## The five model views

Each view is `for_model(SCHEMA)` plus a `.router(prefix, tera, pool)`. Mounting
them at the same `prefix` (say `/posts`) gives the classic CRUD URL set:

| View | Renders | Routes mounted | Default template |
|---|---|---|---|
| [`ListView`](#listview) | a paginated list | `GET <prefix>` | `<table>_list.html` |
| [`DetailView`](#detailview) | one row | `GET <prefix>/{pk}` | `<table>_detail.html` |
| [`CreateView`](#createview-updateview-deleteview) | a new-record form | `GET`/`POST <prefix>/new` | `<table>_form.html` |
| [`UpdateView`](#createview-updateview-deleteview) | a prefilled edit form | `GET`/`POST <prefix>/{pk}/edit` | `<table>_form.html` |
| [`DeleteView`](#createview-updateview-deleteview) | a confirm page | `GET`/`POST <prefix>/{pk}/delete` | `<table>_confirm_delete.html` |

`<table>` is the model's table name, so a `Post` (table `posts`) looks for
`posts_list.html`, `posts_detail.html`, and so on. Override any of them with
`.template("my_name.html")`.

---

## ListView

A paginated list page. You provide a template that loops over `object_list`;
the view handles paging, ordering, filtering and search from query params.

```rust
use rustango::template_views::ListView;
use std::sync::Arc;

// Escapes every template, not only `.html` ones.
let tera = rustango::template_extensions::html_tera_from_glob("templates/**/*")?;
let app = ListView::for_model(Post::SCHEMA)
    .page_size(20)                       // rows per page (?page=N to navigate)
    .order_by("published_at", true)      // default sort, true = DESC
    .filter_fields(&["status", "author_id"])  // ?status=published
    .search_fields(&["title", "body"])        // ?search=rust
    .router("/posts", Arc::new(tera), pool);
```

A matching `posts_list.html` — note `object_list` and the pagination variables
the view stamps for you:

```html
<h1>Posts ({{ total }})</h1>
{% for post in object_list %}
  <article>
    <h2><a href="/posts/{{ post.id }}">{{ post.title }}</a></h2>
    <p>{{ post.body }}</p>
  </article>
{% endfor %}

{% if has_prev %}<a href="?page={{ page - 1 }}">← prev</a>{% endif %}
page {{ page }} / {{ total_pages }}
{% if has_next %}<a href="?page={{ page + 1 }}">next →</a>{% endif %}
```

`?page=`, `?status=` and `?search=` work the same as on a ViewSet list — the
difference is purely that the result is a rendered page rather than a JSON
envelope. Use `.context_object_name("posts")` if you'd rather loop over `posts`
than `object_list` in the template.

The page size defaults to 20. A client may ask for `?page_size=N`, capped by
`.max_page_size(n)` (default 100). `.bulk_actions(true)` lets the list accept a
POST with an `action` field and the selected pks: `delete_selected` is built in,
`.action(...)` adds your own, and `.with_delete_confirmation(true)` shows a
confirm page first. `.with_fk_display(true)` adds a `<column>_display` value for
each foreign key, at one extra query per FK column per page.

**`?ordering=` is the exception.** A `ListView`'s allowlist starts *empty*, so
the parameter is ignored until you name the sortable columns yourself:

```rust
fn post_list() -> ListView {
    ListView::for_model(Post::SCHEMA).ordering_fields(&["title", "published_at"])
}
```

Without that call the list falls back to the builder's own `.order_by(...)`, or
to PK-ASC so pagination stays deterministic. Only a single column is accepted,
with a leading `-` for descending; multi-column sorting needs a hand-rolled
handler.

---

## DetailView

One row, looked up from the URL. By default it matches the primary key
(`/posts/42`); point it at another column with `.lookup_field("slug")` for
pretty URLs (`/posts/my-first-post`). A missing row is a `404`.

```rust
use rustango::template_views::DetailView;

let app = DetailView::for_model(Post::SCHEMA)
    .lookup_field("slug")          // GET /posts/{slug} instead of /posts/{id}
    .router("/posts", Arc::new(tera), pool);
```

The template gets the row as `object`:

```html
<h1>{{ object.title }}</h1>
<p>{{ object.body }}</p>
<small>by author #{{ object.author_id }}</small>
```

---

## CreateView, UpdateView, DeleteView

The write side. Each handles a `GET` (render a form / confirm page) and a `POST`
(do the work, then **redirect**). The redirect-after-POST is the standard
**Post/Redirect/Get** pattern — it stops a browser refresh from re-submitting.

**CreateView** — `GET /posts/new` renders an empty form; `POST /posts/new`
inserts the row and `303`s to `success_url`:

```rust
use rustango::template_views::CreateView;

let app = CreateView::for_model(Post::SCHEMA, &["title", "body", "author_id"])  // the only fields a POST can set
    .success_url("/posts")         // where to send the browser after a save
    .router("/posts", Arc::new(tera), pool);
```

The field list is required. It is the write allow-list: the form renders only
these fields, and any other posted key (say `id=1`) is ignored. On
create, a listed field with a column default that the POST leaves out gets
that default.

The form template (`posts_form.html`) is shared with UpdateView. `is_update`
tells the two apart, and **`form`** carries both the fields and any validation
messages:

```html
<form method="post">
  {{ csrf_input | safe }}
  {% if form.errors.__all__ %}<p class="error">{{ form.errors.__all__ }}</p>{% endif %}
  {% for field in form.fields %}
    <label for="{{ field.name }}">{{ field.name }}</label>
    <input id="{{ field.name }}" name="{{ field.name }}" value="{{ field.value }}"
           {% if field.required %}required{% endif %}
           {% if field.max_length %}maxlength="{{ field.max_length }}"{% endif %}>
    {% if form.errors[field.name] %}
      <p class="error">{{ form.errors[field.name] }}</p>
    {% endif %}
  {% endfor %}
  <button>{% if is_update %}Save{% else %}Create{% endif %}</button>
</form>
```

A write the database refuses (a NOT NULL column the form leaves out, a CHECK) is a form-wide error under `form.errors.__all__`, not under a field; show it, or the page comes back with no message.

Two things are easy to get wrong here. `form.errors` maps a field name to **one
string**, not a list — `join` on it is an error. And there is no top-level
`errors` variable; iterating one is a Tera render failure, so the page returns
500 rather than showing the message.

`{{ csrf_input | safe }}` needs the filter: `html_tera` autoescapes every template, so without
it the token renders as text and every POST is rejected.

**Validation.** Schema rules (type, `max_length`, NOT NULL…) are enforced
automatically. Add your own with a closure validator — on `Err`, the form
re-renders with the messages and a `422` status instead of saving:

```rust
use rustango::forms::FormErrors;

CreateView::for_model(Post::SCHEMA, &["title", "body", "author_id"])
    .validator(|data| {
        let mut errs = FormErrors::default();
        if data.get("title").map_or(true, |t| t.len() < 5) {
            errs.add("title", "must be at least 5 characters");
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    })
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

You can also reuse a `#[derive(Form)]` struct's validators with `.form::<F>()`
(validation-only for now — see the API docs).

**UpdateView** — `GET /posts/{pk}/edit` renders the same form prefilled from the
row (`object` is populated, `is_update` is `true`); `POST` updates and `303`s.

```rust
use rustango::template_views::UpdateView;

UpdateView::for_model(Post::SCHEMA, &["title", "body"])
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

A listed field the POST leaves out keeps its stored value. The one exception is
a `bool`: browsers send nothing for an unticked checkbox, so it saves `false`.

**DeleteView** — `GET /posts/{pk}/delete` renders a confirmation page
(`posts_confirm_delete.html`, with `object`); `POST` deletes and `303`s.

```rust
use rustango::template_views::DeleteView;

DeleteView::for_model(Post::SCHEMA)
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

Mount all five at the same prefix and you have full HTML CRUD:

```rust
let app = axum::Router::new()
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(CreateView::for_model(Post::SCHEMA, &["title", "body", "author_id"]).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(UpdateView::for_model(Post::SCHEMA, &["title", "body"]).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(DeleteView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera, pool));
```

---

## The Tera context

Every view stamps a consistent context so templates port cleanly between them:

| View | Variables available in the template |
|---|---|
| `ListView` | `object_list` (the page's rows), `page`, `page_size`, `total`, `total_pages`, `has_next`, `has_prev`, `page_marks`, `prev_page_url` / `next_page_url`, `ordering`, `filters`, `search` (and `bulk_actions` when on) |
| `DetailView` | `object` (the row) |
| `CreateView` | `form` (`.fields`, `.errors`), `is_create` (true), `is_update` (false) — **no `object`** |
| `UpdateView` | `form` (`.fields`, `.errors`), `object` (the row), `pk`, `is_create` (false), `is_update` (true) |
| `DeleteView` | `object` (the row to confirm) |

Rows are exposed as plain maps keyed by column name (`{{ post.title }}`), with
SQL `NULL` rendered as `null`. Use `.context_object_name("posts" / "post")` to
add a friendlier alias alongside `object_list` / `object`.

Each entry in `form.fields` carries `name`, `column`, `ty`, `required`,
`max_length` and `value`. A checkbox and a `blank` string are never `required`. `form.errors` is keyed by field name and holds one
message per field, not a list. Every view also stamps `csrf_token` and
`csrf_input`, and every view router rejects a POST without a matching token.

`CreateView` stamps no `object`, so `{{ object.title }}` on a create form is
undefined rather than empty — use `{{ field.value }}` from `form.fields`, which
is populated on both.

---

## TemplateView and RedirectView

Two model-free helpers for the pages every site has:

**TemplateView** — render a static template with a fixed context (an "about"
page, a landing page). No model, no database:

```rust
use rustango::template_views::TemplateView;

let app = TemplateView::new("about.html")
    .context_value("title", "About us")
    .router("/about", Arc::new(tera));
```

**RedirectView** — a permanent or temporary redirect at a URL (for moved pages):

```rust
use rustango::template_views::RedirectView;

let app = RedirectView::to("/posts").router("/old-posts");
```

It answers `302 Found`; `.permanent()` makes it `301`.

**FormView** — a form with no model. `FormView::<ContactForm>::for_form(handler)`
renders a `#[derive(Form)]` form on GET (template `form.html` by default). On POST
it validates and calls your async handler; `Ok(())` redirects with `303`, and
`Err(msg)` re-renders the form. Invalid input re-renders it with a `422`. Its
context is `errors` (a list per field; form-wide errors under `__all__`) and
`values`. Like every view here, `.success_url` defaults to `/`.

---

## Single-tenant vs multi-tenant

Every model view ships two router constructors — same builder, pick the one that
matches how your app manages database connections:

- **`.router(prefix, tera, pool)`** — single-tenant; captures one pool at mount
  time. This is what the examples above use.
- **`.tenant_router(prefix, tera)`** — multi-tenant; resolves a per-request
  connection from the [`Tenant`](https://docs.rs) extractor. Available with the
  `template_views` + `tenancy` features. Templates port between the two
  unchanged.

This mirrors the ViewSet split (`router` / `router_pool` vs `tenant_router`).

---

## Serving one model both ways

You're not limited to one front door. Mount a JSON API *and* HTML pages over the
same model and pool — a public API for clients, server-rendered pages for
people:

```rust
use rustango::viewset::ViewSet;
use rustango::template_views::{ListView, DetailView};

let app = axum::Router::new()
    // JSON for API clients:
    .merge(ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"]).router_pool("/api/posts", pool.clone()))
    // HTML pages for browsers:
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera, pool));
```

Now `GET /api/posts` returns the paginated JSON envelope and `GET /posts`
returns a rendered HTML list — same rows, same pool, two shapes. This exact
setup is what the [backing test](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs)
asserts.

## Error pages

One layer turns every 4xx/5xx of an app into a page for people, and leaves
API clients alone. It is **off by default**; nothing changes until you turn it
on. Needs the `template_views` feature.

### Turning it on

On the multi-tenant server, the layer wraps everything: your api, the tenant
login, the tenant admin, the operator console and the framework's own layers.

```rust
use std::sync::Arc;
use rustango::errors::ErrorPagesLayer;

let tera = Arc::new(rustango::template_extensions::html_tera_from_glob("templates/**/*.html")?);
let app = rustango::server::Builder::from_env().await?
    .api(my_app::urls::api())
    .error_pages(ErrorPagesLayer::new().templates(tera));
```

`AppBuilder::error_pages` does the same for a single-pool app. On a router of
your own, mount it with `ErrorPagesRouterExt` and add the 404 fallback:

```rust
use rustango::errors::{ErrorPagesLayer, ErrorPagesRouterExt as _};

let app = Router::new()
    .route("/projects", get(projects))
    .fallback(rustango::errors::not_found)
    .error_pages(ErrorPagesLayer::new().templates(tera.clone()));
```

With `Cli` and a config file, use the `[errors]` section instead:

```toml
[errors]
enabled = true            # default false
templates = "templates"   # template root holding errors/404.html etc.; default "templates"
htmx_target = "#flash"    # HX-Retarget for htmx errors; default "#flash", "" sends none
```

A `templates` directory that does not exist is skipped quietly; one that does
not parse logs a warning. Either way the built-in pages are used.

`cargo rustango new` writes `[errors] enabled = true` into
`config/default.toml`, so a new project has the pages on. Older projects stay
off until they add the section.

### What each client gets

| Request | Response |
|---|---|
| Browser navigation (`Accept: text/html`, no `HX-Request`) | Full HTML page |
| htmx (`HX-Request`) | HTML fragment, plus `HX-Retarget: <htmx_target>` and `HX-Reswap: innerHTML show:window:top` |
| Any other client (`fetch`, curl) | The body unchanged; an empty body gets the message as plain text |
| JSON / problem+json body | Unchanged, unless it carries a `Message` and a browser or htmx asked |
| `HEAD` | Unchanged |
| A response marked `rustango::errors::KeepBody` | Unchanged |

**A `text/html` error body is replaced too**, unless the response is marked
`KeepBody`: mark any HTML error page you build yourself.

Only text bodies (`text/plain`, `text/html`, none) and JSON with a message are
replaced; images and other types pass through. A dressed response keeps its
status and headers (`Retry-After` too). Every 4xx/5xx the layer sees, passthroughs
included, gets `Vary: Accept, HX-Request`, merged with the handler's own.

### Templates

The first template that exists in your Tera wins:

1. `errors/<code>.html`, e.g. `errors/404.html`
2. `errors/4xx.html` or `errors/5xx.html`
3. `errors/default.html`
4. the built-in page

htmx fragments use `errors/partial.html`, then the built-in fragment. A
template that fails to render is logged and the built-in page is used; an
error page never comes back empty.

`templates/errors/404.html`:

```html
{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ code }} · {{ title }}</h1>
  <p>{{ message }}</p>
  <p>Nothing at <code>{{ path }}</code>.</p>
  {% if user %}<p>Signed in as {{ user.username }}.</p>{% endif %}
  <a href="/">Back to the start</a>
</section>
{% endblock content %}
```

`templates/errors/5xx.html`:

```html
{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ title }}</h1>
  <p>{{ message }}</p>
  <form method="post" action="/feedback">{{ csrf_input | safe }}
    <button>Tell us what you were doing</button>
  </form>
</section>
{% endblock content %}
```

Context:

| Key | Value |
|---|---|
| `code` | The status, e.g. `404` |
| `title` | A short title per status ("Not found") |
| `message` | What went wrong, for the user (see below) |
| `path` | The request path |
| `user` | `{ username, is_superuser }` of the signed-in tenant user, or null |
| `username`, `is_superuser` | The same, flat |
| `csrf_token`, `csrf_input` | A CSRF token and its hidden input, under your CSRF layer's cookie name. The cookie is set only when the template uses them |
| `csp_nonce` | This request's CSP nonce, or `""` |
| `brand_name`, `brand_logo_url`, `brand_css`, `theme_mode` | From `[brand]` |

`user` needs the `tenancy` feature and a tenant host; elsewhere it is null.
Templates are per app: every tenant gets the same ones.

### The built-in page

Self-contained HTML: no CDN, no script, inline CSS with the request's CSP
nonce, the admin's theme tokens (light and dark). With `[errors]` in settings
it shows `[brand] name` and `logo_url`; `primary_color` and `theme_mode` set
the colours (they need the `tenancy` feature, whose brand code it reuses).
`ErrorPagesLayer::brand(&settings.brand)` does the same in code.

### The message

A handler gives words for the page with `rustango::errors::Message`, a
response extension, or with `PageError`, which carries one:

```rust
use rustango::errors::{Message, PageError};

async fn project(Path(slug): Path<String>) -> Result<Html<String>, PageError> {
    let project = find_project(&slug)
        .await
        .map_err(PageError::server)?            // logged; the page says something generic
        .ok_or_else(|| PageError::not_found("No project with that name."))?;
    Ok(Html(render_project(&project)))
}

async fn archived() -> impl IntoResponse {
    (StatusCode::GONE, Message::new("This project was archived."))
}
```

Without a `Message`, a 4xx `text/plain` body of up to 2 KiB is the message
(axum's rejections, the CSRF check), else the status's default text. The first
letter is upper-cased.

A 5xx never shows its cause: the page uses the generic text, and
`PageError::server(e)` logs `e` and sends the same opaque body as every other
5xx. Only when `RUSTANGO_DISCLOSE_ERRORS` is set does a 5xx page show its
message or body, the same rule as every 5xx body in the framework.

`PageError::forbidden(msg)` and `PageError::new(status, msg)` cover other
statuses. `ErrorPagesLayer::htmx_swap(..)` changes the `HX-Reswap` value.

### What is covered

Mounted by `Builder::error_pages` (or `[errors]`), the layer sits inside the
security headers and outside everything else, so it dresses:

- unknown paths and unknown hosts (404), and 405s;
- the panic guard's 500;
- the CSRF layer's 403, showing its reason;
- `login_required_or_401` / `*_or_403` gates (401, 403);
- the body limit (413), request timeout (504), rate limit and login throttle (429);
- axum's rejections (400, 415, 422);
- the tenant admin's denials and 404s, and unknown paths on the operator console.

Pages that carry their own controls keep their look: every HTML error page of
the admin, the tenant admin, the operator console and the generic views, such
as a form shown again with its error, the "admin access required" page (it has
a sign-out form) and the "table not migrated" page.
Mark any response of your own with `KeepBody` to do the same.

### Migrating from a hand-written error middleware

An app that had its own `error_pages` middleware can delete:

- the middleware (`from_fn(error_pages)`) and its `.fallback(not_found)`;
- its title/message table: `rustango::errors::error_words` is the same table;
- its private message extension: return `PageError` or `Message`;
- the header/extension cloning it did to stamp CSRF and find the user.

Keep `templates/error.html` as `templates/errors/default.html` and the htmx
alert as `templates/errors/partial.html`; the context keys are the same.

---

## See also

- [ViewSets — CRUD REST APIs](viewsets.md) — the JSON/API counterpart, in depth.
- [Admin](admin.md) — the auto-generated admin is built on these same views.
- [URLs & routing](urls.md) — how to compose these routers into your app.
- [Serializers](serializers.md) — shape the JSON when you go the API route.
