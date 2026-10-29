# Scaffolding

**Rustango** hat zwei Ebenen der Codegenerierung — sodass du selten Boilerplate von Hand verdrahtest:

1. **Der Projektgenerator** — `cargo rustango new` erstellt ein komplett neues Projekt aus einer Vorlage.
2. **Projektinterne Generatoren** — `manage startapp` und die `manage make:*`-Familie fügen Apps, Views, Serializer, Jobs und mehr in einem bestehenden Projekt hinzu.

[![`cargo rustango new` scaffoldet ein komplettes, sofort lauffähiges Projekt — Cargo-Manifest, Config-Tiers, Docker, Migrationen und src — mit einem einzigen Befehl](../img/scaffolding.png)](../img/scaffolding.png)

## Inhaltsverzeichnis

- [Den Generator installieren](#den-generator-installieren)
- [Ein Projekt erstellen: `cargo rustango new`](#ein-projekt-erstellen-cargo-rustango-new)
- [Was generiert wird](#was-generiert-wird)
- [Ein Feature-Modul hinzufügen: `manage startapp`](#ein-feature-modul-hinzufügen-manage-startapp)
- [Einzelne Dateien generieren: die `make:*`-Befehle](#einzelne-dateien-generieren-die-make-befehle)
- [Ein typischer Ablauf](#ein-typischer-ablauf)

---

## Den Generator installieren

`cargo rustango` ist ein Cargo-Unterbefehl. Installiere ihn einmal, global:

```sh
cargo install cargo-rustango
```

Das legt ein `cargo-rustango`-Binary in deinem `PATH` ab; Cargo stellt es dann als `cargo rustango` bereit — ein globaler Befehl, der überall verfügbar ist, auch außerhalb eines Projekts. Dieselbe Installation bringt `cargo-rgo` mit, also ist `cargo rgo …` eine kürzere Schreibweise für jeden `cargo rustango …`-Befehl.

### Die Version des Generators ist die, die dein Projekt bekommt

`cargo rustango new` schreibt `rustango = "MAJOR.MINOR"` in die generierte `Cargo.toml` — übernommen aus **der Version des Generators**, nicht aus dem, was gerade auf crates.io am neuesten ist. Welchen Generator du installierst, dessen Version pinnt dein Projekt.

Das ist fast immer das, was du willst, und genau deshalb ist der Installationsbefehl oben ungepinnt: Der neueste Generator schreibt den neuesten Pin, und die beiden können nicht auseinanderdriften.

Wissen solltest du es, wenn du bewusst ein älteres Release willst — um zu einem Projekt zu passen, das bereits darauf läuft, oder um einen Fehlerbericht zu reproduzieren. Pinne den Generator, nicht das Projekt:

```sh
cargo install cargo-rustango --version 0.57.0
```

Welchen du hast, prüfst du mit `cargo rustango --version`. Später aktualisieren ist derselbe Befehl mit `--force`, und das wirkt sich nur auf Projekte aus, die du danach generierst — der Pin eines bestehenden Projekts ist eine Zeile in dessen eigener `Cargo.toml`, die du selbst änderst.

---

## Ein Projekt erstellen: `cargo rustango new`

```sh
cargo rustango new <name> [--template api|fullstack|tenant]
                          [--backend postgres|sqlite|mysql]
                          [--features <liste>]
```

- **`<name>`** — der Projekt- (und Crate-)Name. Er muss ein gültiger Cargo-Crate-Name sein (`[A-Za-z_][A-Za-z0-9_-]*`), und das Zielverzeichnis darf noch nicht existieren.
- **`--template` / `-t`** — welcher Starter gescaffoldet wird (Default: **fullstack**).
- **`--backend` / `-b`** — auf welcher Datenbank das Projekt läuft (Default: **postgres**).
- **`--features` / `-F`** — zusätzliche **Rustango**-Features, durch Komma oder Leerzeichen getrennt.
- **`--alias rgo`** — die Abhängigkeit `rgo` statt `rustango` nennen (siehe [Ein kürzerer Name](#ein-kürzerer-name-rgo)).
- **`--interactive` / `-i`** — stattdessen aus Menüs wählen. Ein nacktes `cargo rustango new` auf einem Terminal macht dasselbe.
- **`--help` / `-h`**, **`--version`** — Verwendung und Version.

Ohne Argumente aufgerufen, fragt es nach:

```text
  rustango — new project
  (enter accepts the default; ctrl-c aborts)

  Project name: shop

  Template
    1) fullstack  ORM + auto-admin + forms — the usual starting point  (default)
    2) api        bare ORM + axum, no admin UI — for JSON-only services
    3) tenant     multi-tenancy: tenant registry + operator console
  > 3

  Database
    1) postgres  every feature, including schema-mode tenancy  (default)
    2) sqlite    a file beside the project — no server to run
    3) mysql     MySQL 8.0+ / MariaDB
  > 1

  Extra features  (numbers, e.g. `1 3 4` — enter for none)
     1) csrf         CSRF protection middleware for form POSTs
     2) sso          OIDC single sign-on for application users
     …
  > 1 2

  Same thing without the wizard:
    cargo rustango new shop --template tenant --backend postgres --features csrf,sso

  Create it? [Y/n]
```

Der Wizard setzt genau die Felder, die auch die Flags setzen, und gibt die
entsprechende Kommandozeile aus, bevor er irgendetwas schreibt — einmal
benutzt, kennst du die Flags, und es gibt nur einen Codepfad, der entscheidet,
was ein Projekt enthält. Ohne Terminal (Skript, CI-Job) bricht er mit einer
Meldung ab, statt auf eine Antwort zu warten, die niemand geben kann.

### Die drei Vorlagen

Jede entspricht einer der drei App-Formen von **Rustango**:

| Vorlage | Was du bekommst | Wähle sie, wenn |
|---|---|---|
| `api` | Blankes ORM + Axum, **kein Admin** | JSON-only-Dienste und Microservices |
| `fullstack` *(Default)* | ORM + der **Auto-Admin** | Eine typische Webapp mit Backoffice |
| `tenant` | Multi-Tenancy + Operator-Konsole + Apps pro Tenant | SaaS, das viele isolierte Tenants hostet |

```sh
cargo rustango new myblog                      # fullstack (the default)
cargo rustango new api_demo  --template api
cargo rustango new shop      --template tenant
```

### Die Datenbank wählen: `--backend`

```sh
cargo rustango new edge --backend sqlite
cargo rustango new shop --backend mysql
```

`--backend` entscheidet, was `cargo run` benutzt, und formt das ganze Projekt passend dazu — die `DATABASE_URL` in `.env.example`, die Services in `docker-compose.yml`, die `url` in jeder Settings-Stufe und die Startanleitung im README. Nimm `sqlite`, und es gibt überhaupt keinen Datenbank-Service: sie ist eine Datei neben dem Projekt, die das erste `cargo run -- migrate` anlegt.

Alle drei Backends bleiben in den generierten `[features]` verdrahtet, die anderen beiden sind also ein Flag entfernt:

```sh
cargo run --no-default-features --features sqlite
```

### Mehr vom Framework einschalten: `--features`

```sh
cargo rustango new saas --template tenant --features csrf,sso,cache-redis
```

Eine Vorlage schaltet einen sinnvollen Satz ein; `--features` ergänzt die Opt-ins, die keine von ihnen erreicht:

| Feature | Was es hinzufügt |
|---|---|
| `tenancy` | Multi-Tenancy: Tenant-Registry, Datenbanken pro Tenant, Operator-Konsole |
| `csrf` | CSRF-Schutz-Middleware für Formular-POSTs |
| `sso` / `admin-sso` | OIDC Single Sign-on, für App-Benutzer / für die Admin-Site |
| `passkey` | WebAuthn-/Passkey-Authentifizierung |
| `cache-redis` / `cache-page` | Redis-Cache-Backend / Caching ganzer Seiten |
| `jobs` / `jobs-postgres` | Hintergrund-Job-Queue, prozessintern / datenbankgestützt und damit neustartfest |
| `scheduler` | Hintergrundaufgaben in festen Intervallen |
| `email-smtp` | SMTP-Transport für das E-Mail-Framework |
| `mcp` | Model-Context-Protocol-Server für KI-Agenten |
| `http2` | Klartext-HTTP/2 (h2c) neben HTTP/1 bereitstellen |
| `testkit` / `test_utils` | Nur-Test-Schema-Builder, Factories und Konstruktoren |

`cargo rustango new --help` gibt diese Liste aus. Backends sind hier **nicht** gültig — nimm dafür `--backend`. Eines als Feature zu nennen wird abgelehnt, weil es das Backend des Frameworks festnageln würde, während das eigene Feature des Projekts aus bliebe, und `#[derive(Model)]` seine Ausgaben an den Features des Projekts ausrichtet.

### Ein kürzerer Name: `rgo`

```sh
cargo rgo new shop --alias rgo
```

`--alias rgo` trägt die Abhängigkeit unter dem Namen `rgo` ein, und der generierte Code verwendet `rgo::`-Pfade und `#[rgo(...)]`-Attribute. Die Crate `rgo` auf crates.io gehört jemand anderem, darum ist das eine Umbenennung des Pakets `rustango`:

```toml
[dependencies]
rgo = { package = "rustango", version = "0.61", default-features = false, features = ["batteries"] }

[features]
postgres = ["rgo/postgres"]
```

```rust
use rgo::prelude::*;

#[derive(Model, Debug, Clone)]
#[rgo(table = "item", display = "name")]
pub struct Item {
    #[rgo(primary_key)]
    pub id: Auto<i64>,
    #[rgo(max_length = 64)]
    pub name: String,
}
```

`#[rgo(...)]` bedeutet genau dasselbe wie `#[rustango(...)]` und funktioniert unabhängig davon, wie du die Abhängigkeit nennst. Ein Struct oder Feld verwendet einen Namen: `#[rustango(...)]` und `#[rgo(...)]` am selben Element zu mischen ist ein Compile-Fehler. `manage startapp` und die `make:*`-Generatoren lesen die Cargo.toml und schreiben ebenfalls `rgo::`-Pfade und `#[rgo(...)]`.

---

## Was generiert wird

Jede Vorlage schreibt ein in sich geschlossenes Cargo-Projekt:

```text
<name>/
  Cargo.toml            # the rustango dependency + features for this template
  .env.example          # copy to .env (DATABASE_URL, RUSTANGO_SESSION_SECRET, …)
  .gitignore
  rust-toolchain.toml   # selects the `stable` toolchain + rustfmt/clippy/rust-analyzer
  docker-compose.yml    # the database service (none for sqlite) + a cargo-watch dev container
  Dockerfile            # production image (multi-stage, release, non-root)
  Dockerfile.dev        # the dev image docker-compose.yml builds
  .dockerignore
  README.md
  config/
    default.toml        # settings shared across every environment
    dev_settings.toml   # per-tier overrides …
    staging_settings.toml
    prod_settings.toml
  migrations/           # JSON migration files (committed to git)
  system/migrations/    # the framework's own tables, generated on first migrate (commit them)
  src/
    lib.rs              # the app as a library: `pub mod models; pub mod urls; pub mod views;`
    main.rs             # the single binary — HTTP server + every manage verb
    models.rs           # your #[derive(Model)] structs
    views.rs            # request handlers ("views")
    urls.rs             # pub fn api() -> Router that aggregates your routes
```

### Ein Binary für alles

`src/main.rs` ist der einzige Einstiegspunkt. Es startet den HTTP-Server **und** dispatcht jedes `manage`-Verb — es gibt kein separates manage-Skript und keine `src/bin/manage.rs`. Die Module liegen im Library-Target (`src/lib.rs`), daher erreicht `main.rs` sie über den Crate-Namen, und ebenso ein Worker-Binary unter `src/bin/`. Die `main.rs` der api-Vorlage, gekürzt:

```rust
use myapp::urls;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .api(urls::api())
        .with_health()   // /health + /ready endpoints (every template)
        .run()
        .await
}
```

Also startet `cargo run` den Server, und `cargo run -- <verb>` führt Migrationen, Generatoren und den Rest aus.

Wie sich die Vorlagen innerhalb von `main.rs` / `urls.rs` unterscheiden:

- **api** — kein Admin; `urls::api()` aggregiert schlicht deine eigenen Routen.
- **fullstack** — dieselbe `urls.rs`, plus `admin_prefix(&settings)` und ein `admin_router(pool, &settings)`-Helfer, den `main.rs` mit `Cli::nest_with(urls::admin_prefix(&settings), …)` einhängt, gebaut aus dem Pool des Servers. Das Präfix ist `[admin] url_prefix` (oder `[routes] admin_url`), sonst `/admin`. `main.rs` lädt die Einstellungen einmal und gibt sie mit `with_loaded_settings` an die `Cli` weiter. Er nimmt `rustango::sql::Pool` entgegen, benennt also keinen Treiber, und liegt hinter einem Login: Lege das Konto mit `cargo run -- create-admin <username> --superuser` an (siehe [Erste Schritte, Schritt 11](getting-started.md#schritt-11-den-auto-admin-einschalten)).
- **tenant** — `main.rs` ergänzt `.tenancy()`, bedient die Operator-Konsole auf der Apex-Domain und jeden Tenant unter seiner eigenen Subdomain, sowie `.with_tenant_provisioning("migrations")` für die Konsolenseiten **New tenant** und **Database servers** (lösche es, um sie abzuschalten; das Speichern eines Servers braucht `RUSTANGO_SECRET_KEY`). Die eigenen Tabellen des Frameworks werden beim ersten `cargo run -- migrate` aus den kompilierten Modellen in einen **`system/migrations/`**-Ordner generiert — kein handgeliefertes Bootstrap-JSON, sodass das allererste migrate ohne zusätzliche Einrichtung funktioniert.

### Geschichtete Konfiguration

Die Einstellungen laden zuerst `config/default.toml`, dann `config/<RUSTANGO_ENV>_settings.toml` darüber. `RUSTANGO_ENV` ist standardmäßig `dev`, sodass ein frisch gescaffoldetes `cargo run` ohne Änderungen funktioniert; setze `RUSTANGO_ENV=prod` in der Produktion, um `prod_settings.toml` aufzunehmen.

Die Dev- und Staging-Dateien setzen eine `[brand] tagline` von `dev` bzw. `staging`. Um sie zu ändern oder deinen eigenen Namen und dein Logo zu setzen, siehe [Branding](operator-console.md#branding) und die [Konfigurationsschlüssel](admin.md#konfiguration-admin-und-brand) des Admins.

### Einstellungen, die nicht von selbst wirken

Nicht jeden Schlüssel liest das Framework. `Cli::with_settings` warnt beim Start, wenn einer aus der ersten Gruppe gesetzt ist.

| Schlüssel | Status |
|---|---|
| `[sso]` (jeder Schlüssel) | Noch ohne Wirkung. Richte Admin-SSO in der Admin-UI ein. |
| `[auth.jwt] issuer`, `audience` | Noch ohne Wirkung. |
| `[admin] primary_color`, `csrf_cookie_secure`, `session_timeout_minutes` | Noch ohne Wirkung; nur `check --deploy` liest sie. |
| `[database] url` | Wird nicht zum Verbinden genutzt: Jeder Pool liest die Umgebungsvariable `DATABASE_URL`. |
| `[audit] retention_days` | Nichts räumt danach auf; plane `audit-cleanup --days <N>` ein. |
| `[jobs]`, `[i18n]`, `[cache]`, `[mail]`, `[mcp]`, `[auth.jwt]`-TTLs, `[admin]` | Vom Benutzer verdrahtet: nur vom Konstruktor gelesen, den du aufrufst (`jobs::inmemory_from_settings`, `Translator::from_settings`, `cache::from_settings_async`, `email::from_settings`, `mcp::secure_tenant_router_from_settings`, `auth_routes::Config::with_jwt_settings`, `admin::Builder::from_settings`). |

### Erster Lauf

```sh
cd <name>
cp .env.example .env                     # then change `postgres` -> `localhost` in DATABASE_URL
docker compose up -d --no-deps postgres  # only the database: the rust service holds 8080
cargo run -- makemigrations              # generate the framework's (and your) tables
cargo run -- migrate                     # apply them
cargo run -- create-admin <username> --superuser  # fullstack: the admin login
cargo run                                # serve
cargo run -- --help                      # see every manage verb
```

---

## Ein Feature-Modul hinzufügen: `manage startapp`

Scaffolde ein in sich geschlossenes Modul aus zusammengehörigen Modellen, Views und Routen:

```sh
cargo run -- startapp blog
```

Es schreibt `src/blog/` mit `mod.rs`, `models.rs` (ein Starter-Modell, benannt nach der Singular-Form der App — `blog` → `Blog`), `views.rs`, `urls.rs` und `tests.rs`, deklariert dann das Modul in `src/lib.rs` (`pub mod blog;`; `mod blog;` in `src/main.rs` bei einem Projekt ohne `lib.rs`) und merged seine Routen in `urls::api()`.

Optionen:

- **`--into <dir>`** — scaffolde unter einem anderen Basisverzeichnis als `src/` (z. B. einem Workspace-Member).
- **`--with-manage-bin`** — gib zusätzlich eine `src/bin/manage.rs` aus (für Layouts, die ein separates manage-Binary bevorzugen).

---

## Einzelne Dateien generieren: die `make:*`-Befehle

Innerhalb eines Projekts scaffolden die `make:*`-Verben jeweils eine Datei. Die vollständige Referenz pro Flag findest du in der [manage-CLI-Referenz](manage.md); die gängigen Formen sind:

| Befehl | Generiert |
|---|---|
| `make:viewset <Name> [--model <M>]` | Ein CRUD-ViewSet |
| `make:serializer <Name> [--model <M>]` | Ein Serializer zum Formen von Request/Response |
| `make:api_routes <app>` | Ein API-Routen-Aggregator für eine App |
| `make:form <Name>` | Ein HTML-Formular mit Validierung |
| `make:job <Name>` | Ein Handler für einen Hintergrund-Job |
| `make:scheduled <Name>` | Eine Aufgabe mit festem Intervall für `scheduler::Scheduler` |
| `make:worker <Name>` | Ein Worker-Binary unter `src/bin/`, das die Job-Queue abarbeitet |
| `make:notification <Name>` | Eine Mehrkanal-Benachrichtigung |
| `make:middleware <Name>` | Ein Middleware-Gerüst |
| `make:test <Name>` | Ein Testmodul mit dem In-process-Testclient |

```sh
cargo run -- make:viewset PostViewSet --model Post
cargo run -- make:serializer PostSerializer --model Post
cargo run -- make:test post_smoke
```

---

## Ein typischer Ablauf

```sh
cargo rustango new myblog                              # 1. scaffold the project
cd myblog
cargo run -- startapp blog                             # 2. add a feature module
# …add fields to src/blog/models.rs…
cargo run -- makemigrations                            # 3. generate a migration
cargo run -- migrate                                   # 4. apply it
cargo run -- make:viewset PostViewSet --model Post     # 5. expose a JSON API
cargo run                                              # 6. serve
```
