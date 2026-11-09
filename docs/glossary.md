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
- [Sign-in and security](#sign-in-and-security) — sessions, tokens, passwords, signatures
- [Caching, files, email and jobs](#caching-files-email-and-jobs)
- [Languages](#languages) — translating your app
- [Real-time and AI agents](#real-time-and-ai-agents) — live updates, MCP
- [Multi-tenancy](#multi-tenancy) — only if you serve several customers from one deployment
- [A few Rust words](#a-few-rust-words) — so the code blocks aren't scary

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
| `422` | Unprocessable — the data failed validation (the body names the field) |
| `401` / `403` | Not logged in / not allowed |
| `404` | Not found |
| `429` | Too many requests — slow down |
| `500` | The server hit an error |

**401 / 403** — `401` means "we don't know who you are": log in or send a
token. `403` means "we know who you are, but you may not do this".

**Route** — one URL pattern plus the verb it answers, like `GET /api/posts/{id}`,
tied to the code that handles it. See [URLs & routing](urls.md).

**Handler** — the function that runs for a route. It gets the request and
returns the response.

**Cookie** — a small piece of text the server asks the browser to store and
send back with every later request to the same site. It is how a browser stays
logged in.

**Multipart (form upload)** — the request format a browser uses to send a form
that has files in it. Each field and each file travels as its own part. See
[Files](files.md#step-3--accept-an-upload).

**Server-rendered page** — an HTML page the server builds and sends complete,
so the browser just shows it. See [HTML views](html-views.md).

**CRUD** — *Create, Read, Update, Delete.* The four basic things you do to data.
A "CRUD API" just means an API that lets you do all four. See
[ViewSets](viewsets.md), which build a full CRUD API from one declaration.

**Query string / query parameter** — the `?key=value` part on the end of a URL,
used to filter, search, sort, or page through results — e.g.
`/api/posts?status=published&page=2`. Each `key=value` is one parameter.

**Pagination** — splitting a long list of results into pages so a response isn't
huge. The **envelope** is the wrapper around the page that also tells you the
totals — e.g. `{"count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ]}`. See
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

**Template** — a file with placeholders (Rustango uses [Tera](https://keats.github.io/tera/))
that the server fills with data to produce
an HTML page. `{{ post.title }}` drops in a value; `{% for … %}` loops.

**Router / mount** — the router maps incoming URLs to the code that handles
them. To *mount* a ViewSet means "attach its endpoints to your app at a given
path", e.g. mount the posts API at `/api/posts`. See [URLs & routing](urls.md).

**Middleware (a "layer")** — code that runs *around* every request — before your
handler and after it — for cross-cutting concerns like logging, rate limiting,
security headers, or CSRF. "Layer" is Rustango's word for one piece of
middleware. See the [Middleware guide](middleware.md).

**Extractor** — a handler argument that pulls one thing out of the request for
you, such as the logged-in user, the JSON body or the current tenant. If the
thing is missing, the extractor rejects the request before your code runs.

**Pool** — the database handle your code uses to read and write. Rustango asks
you to pass it into each database call explicitly (rather than hiding it in a
global), so it's always clear what touches the database. You'll see `&pool` as
the last argument to ORM calls. `rustango::sql::Pool` is an enum over the three
backends, and it is what the everyday methods take.

**Executor** — *not* a synonym for pool. An executor is a single connection or
an open transaction: the thing a statement actually runs on. Methods ending
`_on` take one, which is how you put several statements inside one transaction.
`fetch_on`, `first_on`, `count_on` and `exists_on` take any executor on all three
backends; the rest of the `_on` family is still Postgres-only. The distinction
decides which methods exist on your build, so
[api-conventions](api-conventions.md#functions) spells the rule out in full.

**QuerySet** — a database query you build up step by step in Rust
(`Post::objects().filter(...).order_by(...)`) before running it. It's lazy:
nothing hits the database until you `fetch` it.

**Feature flag** — an on/off switch, set in `Cargo.toml`, that includes or
excludes a chunk of the framework at build time. It lets you keep your app small
by compiling only what you use. Most features are on by default.

**Reverse (a URL)** — build a URL from its name instead of typing the path, so
renaming a path does not break links. See [URL names & reverse](urls.md).

**Namespace / namespacing** — a prefix in a URL name, like `blog:post-detail`,
that keeps names from different parts of the app apart. See
[Namespacing](urls.md#namespacing).

**Fieldset** — a titled group of fields on an admin edit form, such as
"Content" and "Publishing". See [Admin](admin.md#the-change-form).

**Audit trail** — a log of who changed which record, when, and what the old and
new values were. See [Admin](admin.md#audit-trail).

**OpenAPI** — a standard file format that describes every endpoint of an API.
Tools read it to draw interactive API docs and to generate client code. See
[OpenAPI](openapi.md).

**Schema** — a description of the shape of data. In OpenAPI it describes a JSON
body. In a database it means the set of tables, and in Postgres it is also a
named group of tables inside one database.

**Fixture** — test data you set up before a test runs, so every test starts
from a known state. See [Testing](testing.md).

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

**ORM** — *Object-Relational Mapper.* The part of the framework that turns your
Rust structs into database rows and back, so you query in Rust instead of
writing SQL by hand. See the [ORM guide](orm.md).

**Transaction** — a group of database changes that succeed or fail together.
Either all of them are saved, or none are.

**Rollback** — undoing every change made inside a transaction, as if it never
happened. Tests use it to leave the database clean. See
[Database tests with rollback](testing.md#database-tests-with-rollback).

**Tri-dialect** — "works the same on all three supported databases" —
PostgreSQL, MySQL, and SQLite. When a feature is tri-dialect you can switch
databases without changing your code.

---

## Sign-in and security

**Session** — the server's memory of a logged-in user between requests. The
browser holds only an id in a cookie; the data stays on the server. See
[Sessions](auth-sessions.md).

**Opaque id** — an id that means nothing on its own, just a long random string.
Only the server can look up what it points to.

**Token** — a string a client sends to prove who it is, usually in the
`Authorization` header. Treat it like a password.

**Bearer token** — a token sent as `Authorization: Bearer <token>`. Whoever holds
("bears") it is let in, so keep it secret. See [API keys](auth-api-keys.md).

**JWT** — *JSON Web Token.* A token that carries signed data (who the user is,
when it expires). The server can check it without a database lookup. See
[JWT](auth-jwt.md).

**Claims** — the pieces of data inside a JWT, such as the user id and the expiry
time.

**Stateless** — the server keeps nothing between requests. Everything it needs
comes in the request itself, as with a JWT.

**Access token / refresh token** — a short-lived token used on every request,
plus a long-lived one used only to get a new access token when the old one
expires. See [JWT API](auth-jwt-api.md).

**Token rotation** — each time a refresh token is used, it is replaced by a new
one and the old one stops working. A stolen refresh token then works only once.

**Revocation** — cancelling a token or session before it expires, for example
on logout. See [Revocation](auth-jwt-api.md#revocation-and-the-jti-store).

**Expiry** — the moment a token, link or session stops working.

**Secret / shared secret** — a private value that signs or checks data. A shared
secret is known to both sides, such as the client and the server. Never put it
in code or in logs.

**Hash** — a one-way fingerprint of some data. You can check a password against
its hash, but you cannot get the password back from it. See
[Passwords](auth-passwords.md).

**Salt** — random data mixed into each password before hashing, so two equal
passwords get different hashes.

**Argon2id** — the password-hashing method Rustango uses. It is slow on purpose,
which makes guessing passwords expensive.

**PHC string** — the standard text form of a stored password hash. It holds the
method, its settings, the salt and the hash, all in one string that starts with
`$argon2id$`.

**HMAC** — a signature made from some data plus a secret key. Anyone with the
key can check that the data was not changed. See
[HMAC request signing](auth-hmac.md) and [Account flows](auth-flows.md).

**Constant-time compare** — comparing two secrets in a way that always takes the
same time, so an attacker cannot learn anything from how long the check took.

**Replay** — an attacker sends a request they captured earlier a second time.
A time limit on signed requests blocks it. See
[Clock skew and replay](auth-hmac.md#clock-skew-and-replay).

**Presigned URL** — a link to a file with a signature and an expiry built in, so
anyone with the link can download (or upload) that one file for a short time.
See [Files](files.md).

**CSRF** — *Cross-Site Request Forgery.* Another site tricks a logged-in browser
into sending a request to your app. A CSRF token in each form blocks it. See
[CSRF protection](middleware.md#csrf-protection).

**Permission codename** — the name of one permission, written as
`{table}.{action}`, for example `post.add`. Routes check it to decide who may do
what. See [Auth backends](auth-backends.md#permissions-require_perm).

**OAuth** — a standard way for one app to get limited access on a user's behalf
without seeing their password. It is the basis of "Sign in with …" buttons. See
[SSO](sso.md).

---

## Caching, files, email and jobs

**Backend (storage backend, mailer backend, cache backend)** — the actual place
or service behind a common interface: local disk or S3 for files, SMTP or the
console for email, memory or Redis for a cache. You can swap it without changing
the calling code. Authentication backends are the same idea: each one is a way
to recognise a user. See [Auth backends](auth-backends.md).

**Cache** — a fast store that keeps copies of data you read often, so you don't
compute or fetch it again each time. See [Caching](caching.md).

**Cache key** — the name a value is stored under in the cache, like
`post:42:summary`.

**TTL** — *time to live.* How long a cached value is kept before it expires. See
[TTL and expiry](caching.md#ttl-and-expiry).

**Object storage** — a service that stores files ("objects") over HTTP, such as
Amazon S3 or Azure Blob Storage. See [Files](files.md).

**Mailer** — the part of your app that sends email. In development it prints
the email to the terminal; in production it sends it for real. See
[Email](email.md).

**SMTP** — the standard protocol for sending email to a mail server.

**Transactional email** — an email your app sends because of something a user
did, such as a password reset or a receipt.

**Job queue** — a list of tasks waiting to run in the background, so a request
can return right away. See [Background jobs](jobs.md).

**Worker** — a background task that takes jobs off the queue and runs them.

**Retry and backoff** — run a failed job again, waiting a little longer after
each failure. See [Retries and backoff](jobs.md#retries-and-backoff).

**Dead-letter** — where a job goes after it has failed for good, so you can look
at it later. See [The dead-letter handler](jobs.md#the-dead-letter-handler).

---

## Languages

**Locale** — a language, sometimes with a region, such as `en`, `de` or `pt-BR`.
See [i18n](i18n.md).

**Message catalog** — the file of translated strings for one locale.

**Pluralization** — picking the right form of a word for a number, such as
"1 file" and "2 files". Some languages have more than two forms. See
[Pluralization](i18n.md#pluralization).

**Accept-Language** — a header the browser sends that lists the languages the
user prefers. See [Negotiating the language](i18n.md#negotiating-the-language).

**RTL** — *right-to-left.* Languages such as Arabic and Hebrew that are written
from right to left. See [Right-to-left languages](i18n.md#right-to-left-languages).

---

## Real-time and AI agents

**WebSocket** — a connection that stays open so the browser and the server can
both send messages at any time. See [WebSockets & SSE](websockets.md).

**SSE** — *Server-Sent Events.* A connection that stays open so the server can
push updates to the browser. Messages go one way only, server to browser.

**Broadcast** — sending one message to every connected listener at once.

**Keep-alive / ping** — a small message sent now and then so an idle connection
is not closed by a proxy, and so a dead one is noticed.

**Back-pressure** — what happens when a listener reads slower than messages
arrive. The sender must wait, buffer or drop messages.

**MCP** — *Model Context Protocol.* A standard way for an AI agent to discover
and call the tools your app offers. See [MCP server](mcp.md).

**JSON-RPC** — a simple protocol for calling a named function on a server, with
the request and the reply written in JSON. MCP uses it.

**Agent** — an AI program, usually driven by a large language model, that
calls tools to get work done.

**Tool, resource, prompt (MCP)** — what an MCP server offers an agent: a tool is
an action it can call, a resource is data it can read, and a prompt is a
ready-made instruction.

---

## Multi-tenancy

Behind the `tenancy` feature. Skip this section if you are building an
ordinary single-customer app — none of it applies.

**Multi-tenancy** — running one deployment that serves several customers, each
seeing only their own data. Rustango resolves which customer a request belongs
to from its hostname, then routes it to that customer's data for the rest of
the request.

**Tenant (or org)** — one customer in such a deployment. A tenant has a slug
(`acme`), a host pattern (`acme.example.com`) and its own users. `Org` is the
registry row; "tenant" is the thing it describes.

**Registry** — the small database that lists the tenants: who they are, where
their data lives, whether they are active. Distinct from any tenant's own data,
and the one database the framework always needs.

**Storage mode** — how a tenant's data is kept apart from its neighbours':
a separate database, or a separate schema inside a shared one. Chosen per
tenant at provisioning time.

**Operator** — an administrator of the *deployment* rather than of a tenant.
Operators create tenants and bind hostnames; they are not users of any tenant
and live in the registry. See [operator console](operator-console.md).

**Operator console** — the web interface operators use: provisioning tenants,
binding hostnames, managing other operators, reading the audit trail. Nearly
every action in it is also a `manage` verb, so it can be scripted.

**Provisioning** — creating a tenant: making its database or schema, running
migrations into it, and recording it in the registry.

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
