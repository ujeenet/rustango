# Getting Started: build a blog with Rustango

This walkthrough takes you from an empty directory to a deployed blog: posts, an admin UI, a JSON API with sign-up and login (JWT), and tests. End to end.

> **Time:** ~45 minutes for the full tour, ~10 minutes if you just want to see it running.
>
> **Runnable version:** every step below is mirrored in a tested, compilable example at [`crates/rustango/examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog). If a step ever looks off, diff against it.

[![Build a blog with Rustango: generate the migration, apply it, boot the server, and hit the JSON API — all from one binary](img/getting-started.png)](img/getting-started.png)

---

## What you need to know first

Two different questions, and the docs used to answer only the second one.

**Rust is assumed.** Not expert Rust, but you should be comfortable with structs,
traits, `Result` and `?`, and enough `async`/`.await` to read a function without
looking things up. If that is not you yet, the [Rust Book](https://doc.rust-lang.org/book/)
comes first; this guide will not teach the language underneath it.

**Web backend experience is not assumed.** If you have never built a web API,
start with [Web API basics](glossary.md#web-api-basics) in the glossary. It is a
five-minute primer on requests, routes, handlers and migrations, and it is
written for exactly this gap. Come back here afterwards.

**No other framework is assumed.** Every step explains itself, and where a term
is doing real work, the [glossary](glossary.md) defines it in plain language.

## What you need installed

| Tool | Why | Install |
|---|---|---|
| Rust 1.88+ | Compiler | <https://rustup.rs> |
| A database | This guide uses Postgres | see [Choosing a database](#choosing-a-database) below |
| `psql` (optional) | Inspect DB | `brew install libpq` / `apt install postgresql-client` |

```bash
rustc --version    # should print 1.88+
```

On an older toolchain a fresh `Cargo.lock` can pick a dependency release
that needs a newer Rust. Set `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback`
when you generate the lock file, and Cargo picks the newest releases your
toolchain can build.

### Choosing a database

**Docker is not required.** This guide reaches for it because one command gives
you a throwaway Postgres, but nothing in rustango depends on it. Pick whichever
row fits your machine — everything after this step is identical:

| You want | Do this | Notes |
|---|---|---|
| **No database server at all** | Run with SQLite (below) | Nothing to install. Best for learning the framework. |
| Postgres **without** Docker | Install Postgres natively, then point `DATABASE_URL` at `localhost` | See [Native Postgres](#native-postgres-no-docker). |
| Postgres **with** Docker | `docker compose up -d` in the generated project | What the rest of this guide assumes. |
| MySQL or MariaDB | Scaffold with `--backend mysql` | See [MySQL](#mysql). |

Whichever you pick, the only thing that changes is `DATABASE_URL`. Here is the shape of each:

```bash
DATABASE_URL=postgres://user:password@localhost:5432/myblog_dev
DATABASE_URL=mysql://user:password@localhost:3306/myblog_dev
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

#### SQLite — zero setup

Scaffold the project for SQLite and there is nothing to install, nothing to
start, and nothing to edit afterwards:

```bash
cargo rustango new myblog --backend sqlite
```

The generated `.env.example`, `docker-compose.yml` and settings tiers are all
written for SQLite, the database is a file created by the first
`cargo run -- migrate`, and you can skip
[Step 4](#step-4-start-the-database) entirely.

If you already generated a Postgres project and want to switch it, every
template keeps all three backends wired up — so it is one flag plus a URL:

```bash
cargo run --no-default-features --features sqlite
```

```bash
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

`mode=rwc` tells SQLite to create the file if it does not exist. Everything in
this guide — models, migrations, the admin, the ORM — works unchanged; only
Postgres-specific features (`JSONB` operators, schema-mode multi-tenancy) do
not apply.

#### Native Postgres (no Docker)

Install Postgres from your package manager (`brew install postgresql@16`,
`apt install postgresql`, or the Windows installer at
<https://www.postgresql.org/download/windows/>), then create the role and
database the generated config expects:

```bash
createuser -s rustango          # or: CREATE ROLE rustango LOGIN SUPERUSER PASSWORD 'rustango';
createdb myblog_dev -O rustango
```

The generated `.env.example` points at the Docker service hostname. Change the
host to `localhost`:

```bash
# .env  —  `postgres` is the docker-compose service name; use localhost natively
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

> **On Windows?** Docker Desktop's Hyper-V / WSL2 backend is a common source of
> start-up failures. If it is fighting you, use the SQLite path above to learn
> the framework and come back to Docker when you are packaging for deployment —
> that is what the container setup is really for.

> **Already running Postgres locally?** Then port 5432 is taken, and the
> container quietly loses the race. Your app connects to the local server, which
> has none of your tables. The error that comes back is not readable, because a
> non-English server sends its message in its own encoding. Either stop the local
> service or move the container to another port.

#### MySQL

Scaffold with `--backend mysql` and the generated `.env.example`,
`docker-compose.yml` and settings tiers are all written for MySQL:

```bash
cargo rustango new myblog --backend mysql
```

Running it natively instead of in the container takes a database and a user:

```sql
CREATE DATABASE myblog_dev CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs;
CREATE USER 'rustango'@'localhost' IDENTIFIED BY 'rustango';
GRANT ALL PRIVILEGES ON myblog_dev.* TO 'rustango'@'localhost';
```

```bash
DATABASE_URL=mysql://rustango:rustango@localhost:3306/myblog_dev
```

`utf8mb4` is worth setting deliberately: MySQL's older `utf8` is three bytes and
cannot store an emoji, which surfaces much later as a write that fails on one
row. MariaDB works through the same driver and the same URL scheme.

The collation matters too. Columns inherit the database default, and MySQL's own
(`utf8mb4_0900_ai_ci`) ignores case and accents, so `.eq("/About")` matches
`/about` and a `unique` column refuses `Hero` next to `hero`, unlike PostgreSQL
and SQLite. `utf8mb4_0900_as_cs` compares like they do. Avoid a `_bin` collation:
the driver cannot read its text columns as `String`. `manage check --deploy` warns
about both.

---

## Step 1: Install the scaffolder

The scaffolder generates project and app skeletons for you.

```bash
cargo install cargo-rustango
```

This adds the `cargo rustango ...` subcommand globally. Confirm it's there:

```bash
cargo rustango --help
```

The scaffolder's own version is the one your project pins, so installing the newest gives you the newest rustango. To generate a project on an older release, install that generator instead (`cargo install cargo-rustango --version 0.65.0`) — see [Scaffolding](scaffolding.md#the-generators-own-version-is-the-one-your-project-gets).

---

## Step 2: Create the project

This scaffolds a fresh project with a working layout, settings and migrations.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Run `cargo rustango new` with no arguments at all and it asks for the
template, the backend and any extra features, then prints the equivalent
command line before creating anything — see [Scaffolding](scaffolding.md).

Here's what was generated:

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── Dockerfile                  # release image; Dockerfile.dev is the dev one
├── README.md                   # project-specific
├── config/                     # tiered settings (default + dev/staging/prod)
├── migrations/                 # empty — `cargo run -- makemigrations` populates
├── system/migrations/          # the framework's own tables
└── src/
    ├── lib.rs                  # the app as a library: `pub mod models; …`
    ├── main.rs                 # entry point: `Cli::new().api(urls::api()).run()`
    ├── models.rs               # every #[derive(Model)] lives here
    ├── views.rs                # axum request handlers
    └── urls.rs                 # `pub fn api()` route aggregator
```

There is a single binary: `cargo run` boots the HTTP server, and every management verb (`migrate`, `makemigrations`, `startapp`, `check`, …) flows through the same binary via `cargo run -- <verb>`. There's no separate `manage` binary.

The app's modules live in the library target (`src/lib.rs`), and `src/main.rs` reaches them by crate name (`use myblog::urls;`). That is what lets tests in `tests/` and extra binaries in `src/bin/` use your models.

`Cargo.toml` is the dependency manifest (like `composer.json` or a `Gemfile`). Open it and confirm `rustango` is listed under `[dependencies]`.

> **Confirm the `[features]` block — pick a database backend.** `#[derive(Model)]`
> cfg-gates its generated `FromRow` / `LoadRelated` impls on **your** crate's
> features (a `cfg` inside a derive macro resolves against the destination crate,
> not **Rustango**), so a backend feature must be enabled here or the first model
> won't compile. A current scaffold includes:
>
> ```toml
> [features]
> default  = ["postgres"]            # the backend `cargo run` uses
> postgres = ["rustango/postgres"]
> sqlite   = ["rustango/sqlite"]
> mysql    = ["rustango/mysql"]
> ```
>
> If your generated `Cargo.toml` has **no** `[features]` block (an older
> `cargo-rustango`), add the one above by hand — that always fixes it. Without it
> the build fails with *"the trait bound `…: MaybePgFromRow` is not satisfied"*
> plus a tell-tale `warning: unexpected cfg condition value: postgres`.

---

## Step 3: Set up your environment

Configuration lives in a `.env` file. Copy the template:

```bash
cp .env.example .env
```

The generated `.env` is Docker-friendly out of the box. Because we'll run `cargo` on the host (not inside the dev container), change the database host from `postgres` to `localhost`:

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

The credentials, port, and database name (`myblog_dev`) already match the `docker-compose.yml` Postgres service, so you don't need to touch those.

`RUSTANGO_SESSION_SECRET` signs sessions and tokens. It is **left commented out** in the generated `.env.example`, and for development you can leave it that way: the first boot generates a key into `./var/` and reuses it, so restarts don't sign you out.

For production set a real one — 32 bytes of base64, from your secret manager or:

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

A value that is not 32 bytes of base64 cannot be used. Off the prod tier the server says so on boot and falls back to the generated key, so watch for that warning if you set one and sessions behave as though you had not. With `RUSTANGO_ENV=prod` a missing or invalid secret stops the server from starting.

---

## Step 4: Start the database

> **Using SQLite?** Skip this step — there is no server to start. Make sure
> `.env` has `DATABASE_URL=sqlite://myblog_dev.db?mode=rwc` and add
> `--no-default-features --features sqlite` to every `cargo run` below.
>
> **Using a native Postgres?** It is already running as a service; just confirm
> `psql "$DATABASE_URL" -c "SELECT version();"` succeeds, then skip ahead.

The project ships with a `docker-compose.yml` that runs Postgres in a container, so you don't install a database by hand. We'll run the app itself with `cargo` on the host, so start just the `postgres` service in the background (the compose file also defines an optional `rust` dev-container that would otherwise bind port 8080):

```bash
docker compose up -d postgres
```

Confirm it's running:

```bash
docker compose ps
psql "$DATABASE_URL" -c "SELECT version();"   # should print Postgres version
```

---

## Step 5: Run the built-in migrations

Migrations create your database tables. Run them once to set up the framework's own tables:

```bash
cargo run -- migrate
```

The first compile takes ~2 minutes (Rust builds everything from source). A fresh project ships no migration files of its own yet, but `migrate` is not a no-op: it first generates the framework's own migrations from the compiled models and applies them, so you'll see a few `applied …` lines rather than `nothing to migrate`. (That message appears only once everything — framework and project — is already up to date.) It also creates the audit-log table, so audited models work the moment you add them. You generate your first project migration in Step 9.

Check the migration state:

```bash
cargo run -- showmigrations
```

On a fresh project this prints `(no migrations in ./migrations)`, then the framework's own chain under `System migrations in ./system/migrations:`. Once you create a model and run `makemigrations` (Step 9), each applied migration shows an `[X]` here.

---

## Step 6: First boot

Start the server to make sure everything is wired up.

```bash
cargo run
```

You'll see:

```
INFO rustango::server: listening on http://127.0.0.1:8080
```

`cargo run` also applies pending migrations before it starts listening.

Open <http://localhost:8080> in your browser. The scaffold ships a simple root handler (`views::index`) that greets you with **Hello from Rustango!** and a link to the admin — that confirms **Rustango** is running. (Projects that don't define their own `/` route get a built-in welcome page instead, via `Cli::with_welcome()`.)

Press Ctrl-C to stop.

---

## Step 7: Create an app

An "app" is a self-contained feature module. Your blog app will hold the Post model, its routes, and its templates.

```bash
cargo run -- startapp blog
```

This writes:

```
src/blog/
├── mod.rs
├── models.rs              # a starter model named after the app (you'll replace it)
├── views.rs               # axum handlers
├── urls.rs                # blog-specific routes (pub fn api())
└── tests.rs               # in-process router + inventory smoke tests
```

`startapp` wires the new module in for you: it declares `pub mod blog;` in `src/lib.rs` (or `mod blog;` in `src/main.rs` on an older binary-only project) and inserts a `.merge(crate::blog::urls::api())` line into the `api()` aggregator in `src/urls.rs`, so the blog's routes compose into the app automatically. No manual module registration needed.

---

## Step 8: Define a model

A model is a database table described as a Rust struct. Open `src/blog/models.rs` and define your `Post`. (For the full reference — every field type, custom primary keys, and all attributes — see the [Models guide](models.md).)

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display  = "id, title, status, published_at",
        search_fields = "title, body",
        list_filter   = "status, author_id",
        ordering      = "-published_at",
    ),
    audit(track = "title, body, status"),
    index("status, published_at"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,                  // draft | published

    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,

    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}
```

A few Rust things to note:

- `#[derive(Model, ...)]` is a **derive macro**: it auto-generates code for the struct, the way a class decorator or base class would in other frameworks. Deriving `Model` is what gives the struct its query methods.
- `Auto<i64>` marks a field the database fills in for you (an auto-incrementing `i64` integer), like an auto primary key.
- `Option<...>` means "this value may be absent." `Option<DateTime<Utc>>` is a timestamp that can be null, so `deleted_at` is empty until the row is soft-deleted.
- The `#[rustango(...)]` attributes configure each field (max length, defaults, indexes) and the `admin(...)` block sets up the admin UI columns and filters.

---

## Step 9: Create and apply the migration

Now turn that model into a real table. First, generate the migration from your model:

```bash
cargo run -- makemigrations
```

You'll see something like:

```
wrote ./migrations/0001_create_item_and_posts.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateIndex { name: "posts_status_published_at_idx", table: "posts", columns: ["status", "published_at"], ... }
```

This first migration creates your models — `posts`, plus the starter `item` model the scaffold shipped in `src/models.rs`. The framework's own tables (admin users, content types, audit log, …) are not in it: they have their own chain in `system/migrations/`, which the first `migrate` writes and applies before yours. Open the JSON if you like: it carries the operations plus a full schema snapshot.

Apply it to the database:

```bash
cargo run -- migrate
```

Confirm the table exists:

```bash
psql "$DATABASE_URL" -c "\d posts"
```

---

## Step 10: Try the ORM

Let's read and write rows from code. The ORM lets you work with database rows as Rust structs instead of raw SQL.

Temporarily edit `src/main.rs` to run a quick create-and-read test before booting the server. Replace the `Cli` body with an ad-hoc ORM smoke test (keep the scaffolder's `#[rustango::main]`). The models live in the library, so `main.rs` names them by crate:

```rust
use myblog::blog::models::Post;
use rustango::sql::{FetcherPool, Pool};
use rustango::{Auto, Model};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = Pool::connect(&std::env::var("DATABASE_URL")?).await?;

    // CREATE
    let mut p = Post {
        id: Auto::default(),
        title: "First post".into(),
        body: "Hello, world.".into(),
        status: "draft".into(),
        author_id: 1,
        published_at: Auto::default(),
        deleted_at: None,
    };
    p.save(&pool).await?;
    println!("created post id = {}", p.id.get().copied().unwrap());

    // READ
    let posts = Post::objects().fetch(&pool).await?;
    for post in &posts {
        println!("- {}", post.title);
    }

    Ok(())
}
```

What's happening here, in plain terms:

- `pool` is the shared database connection pool. You pass a reference to it (`&pool`) into query calls instead of opening a new connection each time.
- Database calls are asynchronous, so each one ends in `.await` — that pauses until the result comes back, then continues. The `?` after an `.await` says "if this errored, stop and return the error."
- `main` returns a `Result`, Rust's success-or-error type, which is why `?` and the closing `Ok(())` work.
- To save a row, call `.save(&pool)` on it. To read rows, build a query with `Post::objects()` and run it with `.fetch(&pool)` — with no filters, that reads every row.
- `.fetch(…)` comes from the `FetcherPool` trait, which is why the imports bring it in. Without that line the method does not exist and the compiler says so without explaining why.
- These are the multi-backend calls, and everything above compiles unchanged on all three databases. See the [ORM guide](orm.md).

Run it:

```bash
cargo run
```

You should see your new post id and the rows read back. Restore `src/main.rs` to its scaffolded server shape once you've confirmed it works — the next step builds on that. Remove the three `use` lines above as well: the server shape doesn't use them, and the compiler warns about unused imports.

---

## Step 11: Turn on the auto-admin

**Rustango** ships a generated admin UI for your models — browse, search and edit rows with no code of your own. The fullstack scaffold already wires it: a helper in `src/urls.rs` turns a pool into an admin router, and `src/main.rs` nests it under `/admin`.

The helper puts the admin behind a login. It reads its options from the loaded settings, and `admin_prefix` must match the path it is nested under so the admin's own links and form actions resolve. `admin_prefix(settings)` returns `[admin] url_prefix` (or `[routes] admin_url`), else `/admin`:

```rust
use rustango::admin;
use rustango::config::Settings;
use rustango::session::{tier_from_env, try_load_session_secret_for_tier, SessionSecretError};
use rustango::sql::Pool;

pub fn admin_router(pool: Pool, settings: &Settings) -> Result<Router, SessionSecretError> {
    let secret = try_load_session_secret_for_tier(
        &tier_from_env(),
        std::path::Path::new("./var/.rustango_session.key"),
    )?;
    Ok(admin::Builder::from_settings(pool, settings)
        .admin_prefix(admin_prefix(settings)) // the path main.rs nests it at
        .with_session_auth(secret)
        .build())
}
```

> **Keep `.with_session_auth(...)`.** Without it the admin has no login: anyone who can reach `/admin` can read, edit and delete every model. `check --deploy` warns when an admin is built without it (see [admin.md](admin.md)).

`Builder::new` takes any backend's pool, so this helper names no driver and works on all three.

`src/main.rs` hands the helper to the `Cli`, which builds the admin from the pool it serves with, so verbs like `makemigrations` still run without a database. Keep the `pub mod blog;` line from Step 7 in `src/lib.rs` — that's what registers your `Post` model with the admin. The scaffolded `main.rs` looks like this (trimmed):

```rust
use myblog::urls;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let loaded = rustango::config::Settings::load_from_env();
    let settings = loaded.as_ref().cloned().unwrap_or_default();
    let admin_settings = settings.clone();
    rustango::manage::Cli::new()
        .api(urls::api())
        .try_nest_with(urls::admin_prefix(&settings), move |pool| {
            urls::admin_router(pool, &admin_settings)
        })
        .with_health() // /health + /ready endpoints
        .with_loaded_settings(loaded)
        .run()
        .await
}
```

Create the account you will log in with (it prompts for a password):

```bash
cargo run -- create-admin alice --superuser
```

`Cli::new()...run()` is the same unified dispatcher the scaffolder generated — it still serves every `cargo run -- <verb>`; you've only enriched the router it serves at runserver time.

Run it:

```bash
cargo run
```

Open <http://localhost:8080/admin> (no trailing slash) and log in as `alice`. You'll see the admin home with a `posts` link. Click it to see your draft post in the list, click the post to open its edit form, and save. The audit-trail tab records every write.

---

## Step 12: Build the JSON API

A ViewSet exposes a model as a REST API with list, create, retrieve, update, and delete endpoints, all from one declaration.

### 12a. Generate the ViewSet

Scaffold the file, then fill in which fields and behaviors to expose:

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Edit `src/post_view_set.rs`:

```rust
use rustango::ViewSet;
use crate::blog::models::Post;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    fields        = "id, title, body, status, author_id, published_at",
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
    read_only,
)]
pub struct PostViewSet;
```

Register the new module by adding `pub mod post_view_set;` to `src/lib.rs` (`make:viewset` prints this line for you).

Keep `read_only` for now: the API serves list and retrieve only. Without it the ViewSet would also serve create, update and delete to anyone, so it refuses to mount until you say who may write. [Step 14](#step-14-add-sign-up-login-and-jwt-authentication) adds accounts and turns writes on for signed-in users.

### 12b. Mount the routes

Attach the ViewSet's routes to the app. The ViewSet router needs the database pool, and `urls::api()` has none, so add one more `nest_with` to the `Cli` chain in `src/main.rs`. It gets the server's pool, like the admin:

```rust
rustango::manage::Cli::new()
    .api(urls::api())
    .try_nest_with(urls::admin_prefix(&settings), move |pool| {
        urls::admin_router(pool, &admin_settings)
    })
    .nest_with("/api", |pool| myblog::post_view_set::PostViewSet::router("/posts", pool))
    .with_health()
    .with_loaded_settings(loaded)
    .run()
    .await
```

The ViewSet now answers at `/api/posts` and `/api/posts/{pk}`. The path is the model's table; the comment at the end of `src/post_view_set.rs` shows this same line, and `make:viewset` prints the URL.

### 12c. Try the endpoints

Start the server:

```bash
cargo run
```

In another terminal, hit the API with `curl`:

```bash
curl http://localhost:8080/api/posts                                    # list
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

A `POST` answers `405 Method Not Allowed` for now. You will write posts through the API in Step 14.

---

## Step 13: Shape the output with a Serializer

By default the ViewSet returns every model field. A Serializer lets you control the response shape: hide internal fields, rename them, or mark some read-only.

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Edit `src/post_serializer.rs`:

```rust
use rustango::{Auto, Serializer};
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,

    #[serializer(source = "body")]                      // rename in API
    pub content: String,

    pub author_id: i64,                                 // writable: NOT NULL with no default

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

Once a serializer is attached, **its fields are the whole write surface**: anything a client posts that is not listed here is dropped before the `INSERT`. That is why `author_id` appears. It is `NOT NULL` with no default on the model, so leaving it out makes every create fail on the not-null constraint. `status` can stay out because the model gives it `default = "'draft'"`.

Each serializer field's type mirrors the matching model field, so `id` and `published_at` keep their `Auto<…>` wrapper from the model (an `Auto<i64>` still serializes to a plain JSON integer). Then register the module by adding `pub mod post_serializer;` to `src/lib.rs`.

Wire the serializer into the ViewSet with the `serializer` attribute — list, retrieve, and create responses are then rendered through it. `fields` stays required: a body can write only fields both it and the serializer list:

```rust
#[derive(ViewSet)]
#[viewset(
    model = Post,
    fields = "id, title, body, author_id, published_at",
    serializer = crate::post_serializer::PostSerializer,
    ordering = "-published_at",
    read_only,
)]
pub struct PostViewSet;
```

This works identically on PostgreSQL, MySQL, and SQLite. `method` / `read_only` / `source` / `write_only` overrides all apply to the response, and **request bodies are validated through the serializer too**: `create` / `update` run its `validate()` (per-field and cross-field), returning a `422` (`validation_failed`) with one list of messages per field in `details` (`{field: [messages]}`) on failure, and read-only / computed fields a client posts are ignored. (Note: `nested` / `many` serializer fields need the related rows loaded via `select_related`; otherwise they render as their default.) See the [ViewSets guide](viewsets.md) for the full input + output behavior.

---

## Step 14: Add sign-up, login and JWT authentication

So far anyone can read posts and nobody can write them. In this step people create an account, log in, and get a token. With the token they can write posts. Reads stay public.

A few words first:

- A **password hash** is a one-way scramble of a password. You store the hash, never the password. At login you hash the attempt again and compare.
- A **JWT** (JSON Web Token) is a small signed text. The server signs it with a secret key, so nobody can change it without the key. It holds the user id and an expiry time.
- The client sends the token back on every request in the `Authorization: Bearer <token>` header.
- **Middleware** is code that runs before your handler. Here it checks the token.

`rustango::passwords` hashes passwords (argon2id). `rustango::jwt` signs and checks tokens (HS256). Both are on by default. The finished code is in the example project [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog), and its `tests/auth_flow.rs` runs every case below.

### 14a. A user model

Add a second model to `src/blog/models.rs`, below `Post`. The imports at the top of the file stay as they are:

```rust
/// Someone who can sign in to the API (Step 14).
#[derive(Model, Clone, Debug)]
#[rustango(table = "api_users", display = "username")]
pub struct ApiUser {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 150, unique)]
    pub username: String,

    // An argon2 hash, never the password itself.
    #[rustango(max_length = 255)]
    pub password_hash: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}
```

`unique` makes the database refuse a second row with the same username. Create and apply the migration, as in Step 9:

```bash
cargo run -- makemigrations     # wrote ./migrations/0002_create_api_users.json
cargo run -- migrate
```

Never list `password_hash` in a ViewSet or a serializer: the framework refuses to mount one that names a credential column, so a hash cannot leak through the API. The admin shows the new table and masks that column.

### 14b. The signing key

The server needs a secret key to sign tokens. Make one with at least 32 random bytes and put it in `.env`:

```bash
echo "JWT_SECRET=$(openssl rand -base64 32)" >> .env
```

In PowerShell, run the same three lines Step 18 uses for `RUSTANGO_SESSION_SECRET`, then add the value to `.env` as `JWT_SECRET=...`. Use a different key in production, and keep it out of git. `.env` is already in `.gitignore`.

### 14c. Register, log in, check the token

Create `src/blog/auth.rs`. It holds all the sign-in code. The file comes in three parts here; paste them one after another. Then add `pub mod auth;` to `src/blog/mod.rs`.

**Part 1: imports, types and small helpers.**

```rust
//! Sign-up, sign-in and the Bearer-token guard for the blog API (Step 14).

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::api_errors::ApiError;
use rustango::core::{ModelSchema, WhereExpr};
use rustango::jwt::{self, Claims};
use rustango::login_throttle::{self, ClientIp, LoginRefused, LoginScope};
use rustango::passwords::{self, PasswordError};
use rustango::prelude::*;
use rustango::sql::{sqlx, ExecError, Pool};
use rustango::viewset::{match_nothing, ViewSetFilter, WritePin};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::blog::models::{ApiUser, Post};

/// How long a token from `/api/login` stays valid.
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// What `/api/register` and `/api/login` read from the request body.
#[derive(Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// What `/api/login` sends back.
#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

/// The signed-in user. The guard puts it on the request for the handlers.
#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
}

/// The signing key, from the `JWT_SECRET` environment variable.
fn jwt_secret() -> Result<Vec<u8>, ApiError> {
    match std::env::var("JWT_SECRET") {
        Ok(secret) if secret.len() >= 32 => Ok(secret.into_bytes()),
        _ => Err(ApiError::logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth",
            "JWT_SECRET is unset or shorter than 32 bytes",
        )),
    }
}

/// Usernames are stored lower-case, so `Alice` and `alice` are one account.
fn normalize(username: &str) -> String {
    username.trim().to_lowercase()
}

/// A 500 that logs the cause and tells the client nothing about it.
fn server_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, "auth", e)
}

/// The same answer for an unknown user and a wrong password.
fn bad_credentials() -> ApiError {
    ApiError::unauthorized("invalid username or password")
}
```

`ApiError` sends the framework's usual JSON error shape (`{"error": ..., "message": ..., "status": ...}`), so your errors look like the ViewSet's.

**Part 2: the two handlers.** A handler is an `async fn` that answers one route. `State(pool)` hands it the database pool; `Json(body)` reads the JSON body into `Credentials`.

```rust
/// `POST /api/register` — create an account. 201 on success, 409 if the name is taken.
pub async fn register(
    State(pool): State<Pool>,
    Json(body): Json<Credentials>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let username = normalize(&body.username);
    let name_ok = (3..=150).contains(&username.len())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !name_ok {
        return Err(ApiError::validation(
            "username: 3-150 characters, letters, digits, '_', '-' or '.'",
        )
        .with_field("username"));
    }
    // An upper limit too: hashing a huge password costs the server.
    if body.password.len() > 128 || !passwords::strength_score(&body.password).is_empty() {
        return Err(ApiError::validation(
            "password: 12-128 characters, with a digit or a symbol, not a common one",
        )
        .with_field("password"));
    }

    // argon2 is slow on purpose; the `_async` call keeps the server responsive.
    let password_hash = passwords::hash_async(&body.password)
        .await
        .map_err(server_error)?;

    let mut user = ApiUser {
        id: Auto::default(),
        username,
        password_hash,
        created_at: Auto::default(),
    };
    match user.save(&pool).await {
        Ok(_) => {}
        // The unique index on `username` decides, so two parallel sign-ups cannot both win.
        Err(ExecError::Driver(sqlx::Error::Database(db))) if db.is_unique_violation() => {
            return Err(ApiError::conflict("username already taken").with_field("username"));
        }
        Err(e) => return Err(server_error(e)),
    }

    let id = user.id.get().copied().unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "username": user.username })),
    ))
}

/// `POST /api/login` — check the password and hand out a short-lived token.
pub async fn login(
    State(pool): State<Pool>,
    ip: ClientIp,
    Json(body): Json<Credentials>,
) -> Result<Json<TokenResponse>, Response> {
    let username = normalize(&body.username);

    // Before any lookup: 429 once this IP (or everyone) failed too often.
    // `Sso(name)` is the scope an app can name: own limits, no account lock.
    let scope = LoginScope::Sso("blog-api".to_owned());
    let attempt = login_throttle::shared()
        .begin(&scope, &ip, &username)
        .await
        .map_err(IntoResponse::into_response)?;

    let found = ApiUser::objects()
        .where_(ApiUser::username.eq(username))
        .first(&pool)
        .await
        .map_err(|e| server_error(e).into_response())?;

    let Some(user) = found else {
        // Do the same slow work as a real check, so timing does not reveal the name is unknown.
        if passwords::verify_dummy_async(&body.password).await.is_err() {
            return Err(LoginRefused::Busy.into_response()); // 503: no hashing slot free
        }
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    };
    let password_ok = match passwords::verify_async(&body.password, &user.password_hash).await {
        Ok(ok) => ok,
        Err(PasswordError::Busy) => return Err(LoginRefused::Busy.into_response()),
        // A corrupt stored hash counts as a wrong password, not a server error.
        Err(_) => false,
    };
    if !password_ok {
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    }
    attempt.succeeded().await;

    let id = user.id.get().copied().unwrap_or_default();
    let claims = Claims::new(id.to_string()).ttl(TOKEN_TTL); // subject = user id
    let secret = jwt_secret().map_err(IntoResponse::into_response)?;
    let access_token =
        jwt::encode(&claims, &secret).map_err(|e| server_error(e).into_response())?;
    Ok(Json(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: TOKEN_TTL.as_secs(),
    }))
}
```

What `login` does, in order:

- `login_throttle::shared().begin(...)` is the same gate the built-in logins use. It counts failed attempts per client IP and in total, and answers `429 Too Many Requests` when there are too many. You report each result with `attempt.failed()` or `attempt.succeeded()`; a success gives the count back. The `LoginScope::Sso("blog-api")` scope gives this login its own limits.
- An unknown username and a wrong password get the same `401` with the same message. Otherwise an attacker could try names and learn which accounts exist.
- `verify_dummy_async` does a full password check on a fake hash when the user does not exist, so both cases take about the same time.
- `Claims::new(id)` puts the user id in the token as its *subject*. `.ttl(TOKEN_TTL)` makes it expire after 15 minutes.

**Part 3: the guard and the post rule.**

```rust
/// The user behind an `Authorization: Bearer <token>` header, or a 401.
pub async fn authenticate(pool: &Pool, parts: &Parts) -> Result<CurrentUser, ApiError> {
    let unauthorized = || ApiError::unauthorized("missing, invalid or expired token");

    let token = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .ok_or_else(unauthorized)?;
    // `decode` checks the signature and the expiry time.
    let claims = jwt::decode(token, &jwt_secret()?).map_err(|_| unauthorized())?;
    let id: i64 = claims
        .subject()
        .and_then(|sub| sub.parse().ok())
        .ok_or_else(unauthorized)?;

    // A deleted account's tokens stop working at once.
    let user = ApiUser::objects()
        .where_(ApiUser::id.eq(id))
        .first(pool)
        .await
        .map_err(server_error)?
        .ok_or_else(unauthorized)?;
    Ok(CurrentUser {
        id,
        username: user.username,
    })
}

/// Middleware: reads pass, every other method needs a valid token.
pub async fn require_user_for_writes(
    State(pool): State<Pool>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(next.run(request).await);
    }
    let (mut parts, body) = request.into_parts();
    let user = authenticate(&pool, &parts).await?;
    parts.extensions.insert(user);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// ViewSet rule: a new post's author is the signed-in user, and only the
/// author may change or delete it. Reads see every post.
pub struct AuthorFromToken;

impl ViewSetFilter for AuthorFromToken {
    fn filter(&self, _: &HashMap<String, String>, _: &'static ModelSchema) -> Vec<WhereExpr> {
        Vec::new()
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        if matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return Vec::new();
        }
        match parts.extensions.get::<CurrentUser>() {
            Some(user) => vec![Post::author_id.eq(user.id).into()],
            // No user: match no row. An empty Vec would mean "every row".
            None => vec![match_nothing(schema)],
        }
    }

    fn write_pins(&self, parts: &Parts, _: &'static ModelSchema) -> Vec<WritePin> {
        match parts.extensions.get::<CurrentUser>() {
            // Whatever `author_id` the body sends, this value is stored.
            Some(user) => vec![WritePin::field("author_id", user.id)],
            None => vec![WritePin::Deny],
        }
    }
}
```

- `require_user_for_writes` is the middleware. `GET` requests pass. For anything else it calls `authenticate`, which answers `401` when the header is missing, the token is changed or expired, or the user no longer exists. On success it puts a `CurrentUser` on the request, where the next code can read it.
- `AuthorFromToken` is a ViewSet *filter backend*: it can narrow which rows a request sees and fix values on writes. `write_pins` stores the signed-in user's id in `author_id` on create, whatever the body says, and an update can never change it. `filter_with` lets a user change or delete only their own posts; someone else's post answers `404`.

### 14d. Mount the routes

Add the sign-in routes to `src/blog/urls.rs`. They need the database pool, so they get their own function next to `api()`:

```rust
use std::time::Duration;

use axum::routing::post;
use axum::Router;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::sql::Pool;

use super::auth;

/// `/register` and `/login` (Step 14). They need the pool, so `main.rs` mounts them.
pub fn auth_routes(pool: Pool) -> Router {
    // Each sign-up costs a password hash: 10 a minute per client IP.
    let register = Router::new()
        .route("/register", post(auth::register))
        .rate_limit(RateLimitLayer::per_ip(10, Duration::from_secs(60)));
    Router::new()
        .route("/login", post(auth::login)) // throttled inside the handler
        .merge(register)
        .with_state(pool)
}
```

Then add one function to `src/urls.rs` that puts everything under `/api` together:

```rust
/// Everything `main.rs` mounts under `/api`: the posts and the sign-in routes (Step 14).
pub fn rest_api(pool: Pool) -> Router {
    crate::post_view_set::PostViewSet::router("/posts", pool.clone())
        .merge(crate::blog::urls::auth_routes(pool))
}
```

In `src/main.rs`, replace the `/api` line from Step 12b with:

```rust
.nest_with("/api", urls::rest_api)
```

### 14e. Protect the post writes

Replace `src/post_view_set.rs` with the version below. The `#[derive(ViewSet)]` attributes have no option for a filter backend, so this file uses the same options through the `ViewSet` builder. `PostViewSet::router(prefix, pool)` keeps its name, so the rest of your code does not change.

```rust
use axum::middleware::from_fn_with_state;
use axum::Router;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::viewset::ViewSet;

use crate::blog::auth::{require_user_for_writes, AuthorFromToken};
use crate::blog::models::Post;
use crate::post_serializer::PostSerializer;

/// The Post API: anyone can read, writes need a Bearer token (Step 14).
pub struct PostViewSet;

impl PostViewSet {
    pub fn router(prefix: &str, pool: impl Into<Pool>) -> Router {
        let pool = pool.into();
        ViewSet::for_model(
            Post::SCHEMA,
            &["id", "title", "body", "author_id", "published_at"],
        )
        .serializer::<PostSerializer>()
        .filter_fields(&["author_id", "status"])
        .search_fields(&["title", "body"])
        .ordering(&[("published_at", true)])
        .page_size(20)
        .filter_backend(AuthorFromToken)
        // Writes pass the framework's gate because the layer below guards them.
        .allow_anonymous()
        .router(prefix, pool.clone())
        .layer(from_fn_with_state(pool, require_user_for_writes))
    }
}
```

`allow_anonymous()` tells the framework that the writes are not open by accident: your own middleware, added with `.layer(...)`, checks every write before the ViewSet sees it. Both lines matter: without `allow_anonymous()` the router refuses to mount, and without `.layer(...)` anyone could write.

The serializer from Step 13 still lists `author_id`, so responses show it. A client may still send it, but the pin replaces the value.

### 14f. Try it with curl

Start the server with `cargo run`, then in another terminal:

```bash
# 1. Create an account: 201
curl -i -X POST http://localhost:8080/api/register \
     -H "content-type: application/json" \
     -d '{"username":"alice","password":"correct-Horse-battery-9"}'

# 2. Log in: 200 with {"access_token":"eyJ...","token_type":"Bearer","expires_in":900}
curl -X POST http://localhost:8080/api/login \
     -H "content-type: application/json" \
     -d '{"username":"alice","password":"correct-Horse-battery-9"}'

# 3. Keep the token in a shell variable (paste your own value)
TOKEN=eyJ...

# 4. Write a post with the token: 201, and "author_id" is alice's id
curl -X POST http://localhost:8080/api/posts \
     -H "authorization: Bearer $TOKEN" \
     -H "content-type: application/json" \
     -d '{"title":"Hello","content":"My first signed-in post"}'

# 5. The same request without the token: 401
curl -i -X POST http://localhost:8080/api/posts \
     -H "content-type: application/json" \
     -d '{"title":"Nope","content":"x"}'

# 6. Reading stays public: 200
curl http://localhost:8080/api/posts
```

A wrong password, or a username that does not exist, gives the same `401 {"error":"unauthorized","message":"invalid username or password",...}`. After 15 minutes the token expires and writes answer `401` again: log in once more.

> **Good practices from the start**
>
> - **Secret from the environment.** `JWT_SECRET` comes from `.env` or the server's environment, never from code or git. Use 32 or more random bytes (`openssl rand -base64 32`) and a different key per environment.
> - **Short-lived tokens.** 15 minutes limits the damage of a stolen token. Do not raise it to days to save a login.
> - **Never log secrets.** Do not log or print passwords, tokens or the `Authorization` header, and never put a token in a URL. The `AccessLogLayer` from Step 15 logs no headers or bodies and redacts query values.
> - **Generic login errors.** The same `401` for an unknown user and a wrong password, as above.
> - **Limit login attempts.** Keep the `login_throttle` gate in `login` and the rate limit on `/register`. Behind a proxy, set trusted proxies (Step 18), or every client shares the proxy's IP.
> - **HTTPS in production.** A token sent over plain HTTP can be read on the way. Terminate HTTPS at your proxy (Step 18).
> - **Hash only on the server, only with argon2.** Use `passwords::hash_async` / `verify_async`; never store a password or a fast hash such as SHA-256.
> - **No open writes.** Every write route needs a check, like the layer in 14e. A ViewSet with `allow_anonymous` and no layer lets anyone change your data.

### 14g. Next step: refresh tokens

`rustango::jwt` issues single stateless tokens: when one expires, the user logs in again, and you cannot cancel a token before it expires. When you need longer sessions or logout that takes effect at once, move to the full pattern: short-lived **access** tokens, a long-lived **refresh** token in an HttpOnly cookie, rotation, and a JTI blacklist for revocation. Enable the `tenancy` feature and use `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, whose `issue_pair_with` / `verify_access` / `refresh` methods manage the pair for you.

---

## Step 15: Add security middleware

Middleware wraps every request to add cross-cutting behavior. This one goes in `src/main.rs`: it replaces the `/api` `nest_with` line from Step 14d, so the layers wrap every route under `/api`. Here you stack request IDs, access logging, rate limiting, CORS, and security headers in one chain. Each `.method(...)` adds one layer. See the [Middleware guide](middleware.md) for the full layer catalog and ordering rules.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use std::time::Duration;

.nest_with("/api", |pool| urls::rest_api(pool)
    .request_id(RequestIdLayer::default())
    .access_log(AccessLogLayer::default())                      // PII-redacted
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
    .cors(CorsLayer::new()
        .allow_origins(vec!["https://app.example.com"])
        .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]))
    .security_headers(
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    ))
```

The rest of the `Cli` chain stays as it was. Every `/api` request now flows through the full middleware stack. `.with_welcome()` does nothing here: the scaffold already routes `/`, so the welcome page is skipped with a warning.

---

## Step 16: Write tests

**Rustango** includes a test client that drives your router in-process, so you can assert on real HTTP responses without starting a server. Scaffold a test file:

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

The `make:*` generators take a PascalCase name; `PostSmoke` becomes the snake_case file `tests/post_smoke.rs`.

Edit `tests/post_smoke.rs`. Integration tests live in a separate crate, so they build the router under test themselves, with the same `urls::rest_api` that `main.rs` mounts in Step 14d:

```rust
use myblog::urls;
use rustango::sql::Pool;
use rustango::test_client::TestClient;
use serde_json::json;

async fn app() -> axum::Router {
    // A test in `tests/` is a separate crate and never runs `main`, so
    // nothing has loaded `.env` for it. Without this line `DATABASE_URL`
    // is unset and both tests panic before reaching the database.
    let _ = dotenvy::dotenv();

    let pool = Pool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    axum::Router::new().nest("/api", urls::rest_api(pool))
}

#[tokio::test]
async fn list_posts_returns_200() {
    let client = TestClient::new(app().await);
    let response = client.get("/api/posts").send().await;
    assert_eq!(response.status, 200);
    let v = response.json_value();
    assert!(v["results"].is_array());
}

#[tokio::test]
async fn create_post_without_a_token_returns_401() {
    let client = TestClient::new(app().await);
    let response = client.post("/api/posts")
        // The serializer's field names: `content` is `Post.body` renamed.
        .json(&json!({ "title": "Test", "content": "x" }))
        .send().await;
    assert_eq!(response.status, 401);
}
```

Two things in that snippet are easy to get wrong:

- **`dotenvy::dotenv()`** — omit it and *both* tests fail, before any request is made.
- **`content`, not `body`** — the serializer accepts either here, since `source = "body"` keeps the model's name working on input, but `content` is the name your API actually publishes.

The second test proves the Step 14 guard is in place. To test a signed-in write, register and log in through the same client first, then send the token in an `authorization` header. The example project's [`tests/auth_flow.rs`](https://github.com/ujeenet/rustango/blob/develop/crates/rustango/examples/getting_started_blog/tests/auth_flow.rs) does that for every case of Step 14: register, duplicate name, login, wrong password, unknown user, missing, changed and expired tokens, and the author taken from the token.

`Pool` is the multi-backend pool, and `router` takes any backend's pool, so this file compiles unchanged on PostgreSQL, MySQL and SQLite.

> **Heads-up:** integration tests in `tests/` can only `use myblog::…` for modules declared `pub mod` in `src/lib.rs`. A fresh scaffold has that library target; make sure `pub mod post_view_set;` and `pub mod urls;` are there. A project from an older `cargo-rustango` may be binary-only: add a `src/lib.rs` with the `pub mod …;` lines you need, or build the router inline in the test, the way `make:test` scaffolds its `app()`.

Run the tests:

```bash
cargo test --test post_smoke
```

---

## Step 17: Run the system check

Before you deploy, run the built-in checker. It flags common misconfigurations, like a weak `RUSTANGO_SESSION_SECRET` or an unreachable database.

```bash
cargo run -- check --deploy
```

`--deploy` treats a missing `RUSTANGO_SESSION_SECRET` as an error, so set one first (Step 3), even locally. In your local dev environment you'll then see something like:

```
running rustango system check (deploy mode)...
  [info]    6 models registered via inventory
  [info]    database reachable
  [info]    1 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET decodes to a valid ≥32-byte key
  [info]    config tier resolved to `dev`
  [warning] RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier
  [warning] DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in production (…)
```

(The exact model/migration counts and extra notes depend on your project and features.) Those two warnings are the expected dev-environment ones. In a production setup — `RUSTANGO_ENV=prod` and a managed-database `DATABASE_URL` — they clear and you'll see `all checks passed`. Fix any remaining warnings or errors before pushing to production.

---

## Step 18: Deploy to production

How you deploy depends on your platform (Fly, Railway, Kubernetes, bare ECS, and so on). The framework-side steps are the same everywhere; the `--release` flag builds an optimized binary:

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export RUSTANGO_SESSION_SECRET=$(openssl rand -base64 32)

# 2. Run migrations (ship `migrations/` AND `system/migrations/` with the binary)
cargo run --release -- migrate

# 3. Audit
cargo run --release -- check --deploy

# 4. Build binary
cargo build --release

# 5. Run with a process supervisor (systemd / docker / k8s)
./target/release/myblog
```

In PowerShell:

```powershell
$env:RUSTANGO_ENV = "prod"
$env:DATABASE_URL = "postgres://prod-host/myblog"
$bytes = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$env:RUSTANGO_SESSION_SECRET = [Convert]::ToBase64String($bytes)
cargo run --release -- migrate
```

> **Check your write routes first.** Is any ViewSet or view still `allow_anonymous` with no auth layer in front? Then anyone on the internet can change that data. Make it `read_only`, or guard it as in [Step 14e](#14e-protect-the-post-writes).

Make sure your reverse proxy:
- Terminates HTTPS
- Forwards `X-Forwarded-For`, and the app mounts `RealIpLayer::trust_proxies([...])` naming that proxy (`Cli::with_trusted_proxies([...])?` or `server::Builder::real_ip`), for accurate IPs in `AccessLogLayer` and the throttles (see [security.md](security.md))
- Passes the original `Host` header unchanged: tenant routing, apex detection and `AllowedHostsLayer` read only `Host`
- Forwards `X-Forwarded-Proto` (and `X-Forwarded-Host`, which only sets the SSO redirect URL behind a trusted proxy)


`Cli::run()` already fills `ConnectInfo` for rate limiting and IP filtering. If you serve the router yourself, use `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`.

---

## Where to go next

| Topic | Doc |
|---|---|
| Runnable version of this guide | [`examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog) |
| Every `manage` subcommand | [`docs/manage.md`](manage.md) |
| ORM cookbook (advanced filters, aggregations, M2M, soft delete) | [`docs/orm.md`](orm.md) |
| Middleware (the full layer catalog + ordering) | [`docs/middleware.md`](middleware.md) |
| Performance benchmarks (vs Go) | [`docs/benchmarks.md`](benchmarks.md) |
| API conventions (naming, builder patterns, feature gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Security features in depth | [`docs/security.md`](security.md) |
| Multi-tenancy | [README — Multi-tenancy section](https://github.com/ujeenet/rustango/blob/develop/README.md#multi-tenancy) |
| API docs | <https://docs.rs/rustango> |

If you hit something that doesn't work or is unclear, open an issue.
