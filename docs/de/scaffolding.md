# Scaffolding

**Rustango** hat zwei Ebenen der Codegenerierung, beide nach den Generatoren modelliert, die du von Django und Laravel kennst — sodass du selten Boilerplate von Hand verdrahtest:

1. **Der Projektgenerator** — `cargo rustango new` erstellt ein komplett neues Projekt aus einer Vorlage.
2. **Projektinterne Generatoren** — `manage startapp` und die `manage make:*`-Familie fügen Apps, Views, Serializer, Jobs und mehr in einem bestehenden Projekt hinzu.

[![`cargo rustango new` scaffoldet ein komplettes, sofort lauffähiges Projekt — Cargo-Manifest, Config-Tiers, Docker, Migrationen und src — mit einem einzigen Befehl](../img/scaffolding.png)](../img/scaffolding.png)

## Inhaltsverzeichnis

- [Den Generator installieren](#install-the-generator)
- [Ein Projekt erstellen: `cargo rustango new`](#create-a-project-cargo-rustango-new)
- [Was generiert wird](#what-gets-generated)
- [Ein Feature-Modul hinzufügen: `manage startapp`](#add-a-feature-module-manage-startapp)
- [Einzelne Dateien generieren: die `make:*`-Befehle](#generate-single-files-the-make-commands)
- [Ein typischer Ablauf](#a-typical-flow)

---

## Den Generator installieren

`cargo rustango` ist ein Cargo-Unterbefehl. Installiere ihn einmal, global:

```sh
cargo install cargo-rustango
```

Das legt ein `cargo-rustango`-Binary in deinem `PATH` ab; Cargo stellt es dann als `cargo rustango` bereit (auf dieselbe Weise, wie `django-admin` oder der `laravel`-Installer dir einen globalen Befehl geben).

---

## Ein Projekt erstellen: `cargo rustango new`

```sh
cargo rustango new <name> [--template api|fullstack|tenant]
```

- **`<name>`** — der Projekt- (und Crate-)Name. Er muss ein gültiger Cargo-Crate-Name sein (`[A-Za-z_][A-Za-z0-9_-]*`), und das Zielverzeichnis darf noch nicht existieren.
- **`--template` / `-t`** — welcher Starter gescaffoldet wird (Default: **fullstack**).
- **`--help` / `-h`**, **`--version`** — Verwendung und Version.

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

---

## Was generiert wird

Jede Vorlage schreibt ein in sich geschlossenes Cargo-Projekt:

```text
<name>/
  Cargo.toml            # the rustango dependency + features for this template
  .env.example          # copy to .env (DATABASE_URL, RUSTANGO_SESSION_SECRET, …)
  .gitignore
  rust-toolchain.toml   # pins the Rust toolchain
  docker-compose.yml    # a Postgres service to develop against
  Dockerfile            # production image
  README.md
  config/
    default.toml        # settings shared across every environment
    dev_settings.toml   # per-tier overrides …
    staging_settings.toml
    prod_settings.toml
  migrations/           # JSON migration files (committed to git)
  src/
    main.rs             # the single binary — HTTP server + every manage verb
    models.rs           # your #[derive(Model)] structs
    views.rs            # request handlers ("views")
    urls.rs             # pub fn api() -> Router that aggregates your routes
```

### Ein Binary für alles

`src/main.rs` ist der einzige Einstiegspunkt. Es startet den HTTP-Server **und** dispatcht jedes `manage`-Verb — es gibt keine separate `manage.py` oder `src/bin/manage.rs`:

```rust
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .api(urls::api())
        .with_welcome()  // friendly `/` page until you add a root handler
        .with_health()   // /health + /ready endpoints (fullstack & tenant)
        .run()
        .await
}
```

Also startet `cargo run` den Server, und `cargo run -- <verb>` führt Migrationen, Generatoren und den Rest aus.

Wie sich die Vorlagen innerhalb von `main.rs` / `urls.rs` unterscheiden:

- **api** — kein Admin; `urls::api()` aggregiert schlicht deine eigenen Routen.
- **fullstack** — `urls.rs` exponiert zusätzlich `admin_router(pool)` (gebaut aus `admin::Builder::new(pool).build()`), sodass der Auto-Admin unter `/admin` eingehängt wird.
- **tenant** — `main.rs` ergänzt `.tenancy()`, bedient die Operator-Konsole auf der Apex-Domain und jeden Tenant unter seiner eigenen Subdomain. Die eigenen Tabellen des Frameworks werden beim ersten `cargo run -- migrate` aus den kompilierten Modellen (Django-Stil) in einen **`system/migrations/`**-Ordner generiert — kein handgeliefertes Bootstrap-JSON, sodass das allererste migrate ohne zusätzliche Einrichtung funktioniert.

### Geschichtete Konfiguration

Die Einstellungen laden zuerst `config/default.toml`, dann `config/<RUSTANGO_ENV>_settings.toml` darüber. `RUSTANGO_ENV` ist standardmäßig `dev`, sodass ein frisch gescaffoldetes `cargo run` ohne Änderungen funktioniert; setze `RUSTANGO_ENV=prod` in der Produktion, um `prod_settings.toml` aufzunehmen.

### Erster Lauf

```sh
cd <name>
cp .env.example .env
docker compose up -d        # start Postgres
cargo run -- migrate        # apply migrations
cargo run                   # serve
cargo run -- --help         # see every manage verb
```

---

## Ein Feature-Modul hinzufügen: `manage startapp`

Das ist Djangos `startapp` — scaffolde ein in sich geschlossenes Modul aus zusammengehörigen Modellen, Views und Routen:

```sh
cargo run -- startapp blog
```

Es schreibt `src/blog/` mit `mod.rs`, `models.rs` (ein Starter-Modell, benannt nach der Singular-Form der App — `blog` → `Blog`), `views.rs`, `urls.rs` und `tests.rs`, deklariert dann das Modul in `src/main.rs` und merged seine Routen in `urls::api()`.

Optionen:

- **`--into <dir>`** — scaffolde unter einem anderen Basisverzeichnis als `src/` (z. B. einem Workspace-Member).
- **`--with-manage-bin`** — gib zusätzlich eine `bin/manage.rs` aus (für Layouts, die ein separates manage-Binary bevorzugen).

---

## Einzelne Dateien generieren: die `make:*`-Befehle

Innerhalb eines Projekts scaffolden die `make:*`-Verben jeweils eine Datei. Die vollständige Referenz pro Flag findest du in der [manage-CLI-Referenz](manage.md); die gängigen Formen sind:

| Befehl | Generiert | Vergleichbar mit |
|---|---|---|
| `make:viewset <Name> [--model <M>]` | Ein DRF-artiges CRUD-ViewSet | DRF `ViewSet` |
| `make:serializer <Name> [--model <M>]` | Ein Serializer zum Formen von Request/Response | DRF-Serializer |
| `make:api_routes <app>` | Ein API-Routen-Aggregator für eine App | — |
| `make:form <Name>` | Ein HTML-Formular mit Validierung | Django `Form` |
| `make:job <Name>` | Ein Handler für einen Hintergrund-Job | Laravel-/Celery-Job |
| `make:notification <Name>` | Eine Mehrkanal-Benachrichtigung | Laravel-Notification |
| `make:middleware <Name>` | Ein Middleware-Gerüst | Django-/Laravel-Middleware |
| `make:test <Name>` | Ein Testmodul mit dem In-process-Testclient | — |

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
