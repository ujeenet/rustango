# Glossary

A plain-language reference for the words used across these docs. If a term in a
guide is unfamiliar, look it up here first. Definitions are deliberately
informal — the deep-dive guides have the precise details.

If you've never built a web API before, read [Web API basics](#web-api-basics)
top to bottom; it's a five-minute primer. Everything else is for looking things
up as you go.

## Table of contents

- [Web API basics](#web-api-basics) — what an API is, in everyday terms
- [Rustango building blocks](#rustango-building-blocks) — the pieces you assemble
- [Data and the database](#data-and-the-database)
- [A few Rust words](#a-few-rust-words) — so the code blocks aren't scary
- [Frameworks we compare to](#frameworks-we-compare-to)

---

## Web API basics

**API** — *Application Programming Interface.* A way for one program to talk to
another. A **web API** does it over the internet: your app sends a message, the
server sends one back. Think of it as a waiter — you order from a menu, the
kitchen sends food back.

**REST API** — the most common style of web API. "REST" is just a set of
conventions: you act on **resources** (like "posts" or "users") using standard
web verbs. You don't need to know the theory — in practice it means *predictable
URLs and a handful of verbs*, described next.

**Endpoint** — one specific URL your API answers, like `/api/posts` (all posts)
or `/api/posts/42` (the post with id 42). An API is a collection of endpoints.

**HTTP verb (or method)** — *what* you want to do at an endpoint. There are five
you'll see constantly:

| Verb | Means | Example |
|---|---|---|
| `GET` | read / fetch | "give me all posts" |
| `POST` | create | "add a new post" |
| `PUT` | replace | "overwrite post 42 entirely" |
| `PATCH` | partially update | "just change post 42's title" |
| `DELETE` | remove | "delete post 42" |

**Request / Response** — a request is the message you send (a verb + an endpoint
+ optionally a body of data). The response is what comes back (a status code +
usually a body of data).

**JSON** — the text format APIs use to carry data. It looks like
`{"title": "Hello", "published": true}` — labelled values, human-readable. Both
requests and responses are usually JSON.

**Status code** — a three-digit number in every response saying how it went:

| Code | Meaning |
|---|---|
| `200` | OK — here's your data |
| `201` | Created — your new thing was saved |
| `204` | Done — nothing to send back (e.g. after a delete) |
| `400` | Bad request — you sent something invalid (the body says what) |
| `401` / `403` | Not logged in / not allowed |
| `404` | Not found |
| `429` | Too many requests — slow down |
| `500` | The server hit an error |

**CRUD** — *Create, Read, Update, Delete.* The four basic things you do to data.
A "CRUD API" just means an API that lets you do all four. See
[ViewSets](viewsets.md), which build a full CRUD API from one declaration.

**Query string / query parameter** — the `?key=value` part on the end of a URL,
used to filter, search, sort, or page through results — e.g.
`/api/posts?status=published&page=2`. Each `key=value` is one parameter.

**Pagination** — splitting a long list of results into pages so a response isn't
huge. The **envelope** is the wrapper around the page that also tells you the
totals — e.g. `{"count": 137, "page": 2, "results": [ … ]}`. See
[Pagination](viewsets.md#pagination).

**`curl`** — a command-line tool for sending API requests by hand. The
`curl ...` examples in these docs let you try an endpoint from a terminal
without writing any code.

---

## Rustango building blocks

These are the pieces you assemble to build an app. Each links to its full guide.

**Model** — a description of one kind of thing your app stores, like a `Post` or
a `User`. You write it as a Rust `struct`; Rustango turns it into a database
table. See the [ORM guide](orm.md).

**Migration** — a recorded change to your database's shape (adding a table,
a column…). You generate one with `makemigrations` and apply it with `migrate`,
so every environment ends up with the same database structure.

**Serializer** — the translator between your database rows and the JSON your API
sends and receives. It decides which fields are visible, renames or computes
fields for output, and validates incoming data. It *shapes* data; it doesn't
save it (the model does that). See the [Serializers guide](serializers.md).

**ViewSet** — takes a model and a serializer and produces a complete CRUD
**JSON API** (all five verbs above) automatically, so you don't hand-write each
endpoint. The *API view*. See the [ViewSets guide](viewsets.md).

**HTML view (template view, class-based view)** — the server-rendered
counterpart to a ViewSet: turns a model into HTML **pages** — a list page, a
detail page, and create/edit/delete forms — rendered through Tera templates,
instead of JSON. The *HTML view*. See [HTML views](html-views.md).

**Template** — a file with placeholders (Rustango uses [Tera](https://keats.github.io/tera/),
much like Django templates or Jinja) that the server fills with data to produce
an HTML page. `{{ post.title }}` drops in a value; `{% for … %}` loops.

**Router / mount** — the router maps incoming URLs to the code that handles
them. To *mount* a ViewSet means "attach its endpoints to your app at a given
path", e.g. mount the posts API at `/api/posts`. See [URLs & routing](urls.md).

**Middleware (a "layer")** — code that runs *around* every request — before your
handler and after it — for cross-cutting concerns like logging, rate limiting,
security headers, or CSRF. "Layer" is Rustango's word for one piece of
middleware. See the [Middleware guide](middleware.md).

**Pool (or executor)** — the database connection your code uses to read and
write. Rustango asks you to pass the pool into each database call explicitly
(rather than hiding it in a global), so it's always clear what touches the
database. You'll see `&pool` as the last argument to ORM calls.

**QuerySet** — a database query you build up step by step in Rust
(`Post::objects().filter(...).order_by(...)`) before running it. It's lazy:
nothing hits the database until you `fetch` it.

**Feature flag** — an on/off switch, set in `Cargo.toml`, that includes or
excludes a chunk of the framework at build time. It lets you keep your app small
by compiling only what you use. Most features are on by default.

**Scaffolding** — generator commands (`startapp`, `make:serializer`,
`make:viewset`…) that write starter files for you so you don't begin from a
blank page. See [Scaffolding](scaffolding.md).

---

## Data and the database

**Field / column** — one piece of data on a model, like a post's `title` or
`published_at`. "Field" is the Rust side; "column" is the database side; they
line up one-to-one.

**Primary key** — the unique id that identifies one row, usually an
auto-incrementing number called `id`.

**Foreign key (FK)** — a field on one model that points at another model's row,
modelling a relationship — e.g. a `Post` has an `author_id` foreign key pointing
at an `Author`. It's how rows reference each other.

**NULL / nullable** — `NULL` is the database's word for "no value / empty". A
**nullable** field is allowed to be empty; a non-nullable one is required.

**Tri-dialect** — "works the same on all three supported databases" —
PostgreSQL, MySQL, and SQLite. When a feature is tri-dialect you can switch
databases without changing your code.

---

## A few Rust words

You don't need to know Rust to *read* most examples, but these four words show
up everywhere.

**`struct`** — a named bundle of fields, like a record or a class with only
data. Models and serializers are structs.

**Derive macro (`#[derive(Model)]`, `#[derive(Serializer)]`…)** — a one-line
annotation above a struct that tells the compiler to auto-generate a pile of
code for you (the database mapping, the JSON conversion, …). It's the magic that
turns a plain struct into a working model or serializer.

**`async` / `.await`** — Rust's way of handling work that involves waiting (a
database query, a network call). A function marked `async` is "awaitable"; the
`.await` after a call means "wait here for the result". Anything touching the
database is `async`.

**`Result` / `Option`** — how Rust reports outcomes instead of throwing
exceptions. A `Result` is "success *or* an error"; an `Option` is "a value *or*
nothing". The `?` you see after some calls means "if this failed, stop and
return the error".

---

## Frameworks we compare to

These docs occasionally say "like X" to help readers coming from other
ecosystems. The comparisons are a bonus — you never need them to follow a guide.

**Django** — a popular Python web framework. Rustango borrows much of its shape
(models, migrations, an admin UI, the `manage` commands).

**DRF (Django REST Framework)** — Django's add-on for building REST APIs.
Rustango's serializers and ViewSets are modelled on it, so "DRF-shape" means
"laid out the way DRF does it" — e.g. validation errors returned as a JSON
object keyed by field name.

**Laravel / Rails** — popular PHP and Ruby web frameworks, mentioned for the same
"if you've used this, this will feel familiar" reason.
