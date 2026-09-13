# Erste Schritte: einen Blog mit Rustango bauen

Diese Anleitung führt dich von einem leeren Verzeichnis bis zu einem deployten Blog: Beiträge, eine Admin-Oberfläche, eine JSON-API, JWT-Authentifizierung und Tests. Von Anfang bis Ende. Wenn du schon einmal Django, Laravel oder Rails verwendet hast, werden dir die meisten Schritte vertraut vorkommen; wir weisen unterwegs auf die Parallelen hin.

> **Dauer:** ~45 Minuten für die komplette Tour, ~10 Minuten, wenn du es nur laufen sehen willst.
>
> **Lauffähige Version:** Jeder Schritt unten ist in einem getesteten, kompilierbaren Beispiel unter [`crates/rustango/examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog) nachgebildet. Falls ein Schritt jemals seltsam aussieht, vergleiche ihn damit.

[![Einen Blog mit Rustango bauen: die Migration generieren, sie anwenden, den Server starten und die JSON-API abfragen — alles aus einem einzigen Binary](../img/getting-started.png)](../img/getting-started.png)

---

## Was du zuerst brauchst

| Werkzeug | Wofür | Installation |
|---|---|---|
| Rust 1.88+ | Compiler | <https://rustup.rs> |
| Eine Datenbank | Diese Anleitung nutzt Postgres | siehe [Datenbank wählen](#datenbank-wählen) unten |
| `psql` (optional) | DB inspizieren | `brew install libpq` / `apt install postgresql-client` |

```bash
rustc --version    # should print 1.88+
```

### Datenbank wählen

**Docker ist nicht erforderlich.** Diese Anleitung greift darauf zurück, weil ein
einziger Befehl ein wegwerfbares Postgres liefert — aber nichts in rustango hängt
davon ab. Wähle die Zeile, die zu deinem Rechner passt; alles Weitere ist identisch:

| Du willst | Mach das | Hinweise |
|---|---|---|
| **Gar keinen Datenbankserver** | Mit SQLite laufen lassen (siehe unten) | Nichts zu installieren. Ideal zum Lernen. |
| Postgres **ohne** Docker | Postgres nativ installieren, `DATABASE_URL` auf `localhost` zeigen lassen | Siehe [Natives Postgres](#natives-postgres-ohne-docker). |
| Postgres **mit** Docker | `docker compose up -d` im generierten Projekt | Wovon der Rest dieser Anleitung ausgeht. |

#### SQLite — ohne jede Einrichtung

Generierte Projekte bringen ein `sqlite`-Feature mit, du kannst also loslegen,
ohne irgendetwas zu installieren oder zu starten:

```bash
cargo run --no-default-features --features sqlite
```

mit einer dateibasierten URL in `.env` statt der Postgres-Variante:

```bash
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

`mode=rwc` weist SQLite an, die Datei anzulegen, falls sie fehlt. Alles in dieser
Anleitung — Modelle, Migrationen, das Admin, das ORM — funktioniert unverändert;
nur Postgres-spezifische Funktionen (`JSONB`-Operatoren, Schema-Mode-Mandanten)
entfallen.

#### Natives Postgres (ohne Docker)

Installiere Postgres über deinen Paketmanager (`brew install postgresql@16`,
`apt install postgresql` oder das Windows-Installationsprogramm unter
<https://www.postgresql.org/download/windows/>) und lege dann Rolle und Datenbank
an, die die generierte Konfiguration erwartet:

```bash
createuser -s rustango          # oder: CREATE ROLE rustango LOGIN SUPERUSER PASSWORD 'rustango';
createdb myblog_dev -O rustango
```

Die generierte `.env.example` zeigt auf den Docker-Servicenamen. Ersetze den Host
durch `localhost`:

```bash
# .env  —  `postgres` ist der docker-compose-Servicename; nativ ist es localhost
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

> **Unter Windows?** Das Hyper-V-/WSL2-Backend von Docker Desktop ist eine
> häufige Ursache für Startprobleme. Wenn es sich querstellt, nimm den
> SQLite-Weg oben, um das Framework zu lernen, und komm zu Docker zurück, wenn
> es ans Deployment geht — dafür ist das Container-Setup eigentlich da.

---

## Schritt 1: Den Scaffolder installieren

Der Scaffolder generiert für dich Projekt- und App-Gerüste, ähnlich wie `django-admin` oder `rails new`.

```bash
cargo install cargo-rustango
```

Das ergänzt den `cargo rustango ...`-Unterbefehl global. Bestätige, dass er vorhanden ist:

```bash
cargo rustango --help
```

---

## Schritt 2: Das Projekt erstellen

Das erzeugt ein frisches Projekt, das **Rustango**-Äquivalent zu `rails new` oder `composer create-project`.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Das wurde generiert:

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── README.md                   # project-specific
├── config/                     # tiered settings (default + dev/staging/prod)
├── migrations/                 # empty — `cargo run -- makemigrations` populates
└── src/
    ├── main.rs                 # entry point: `Cli::new().api(urls::api()).run()`
    ├── models.rs               # every #[derive(Model)] lives here
    ├── views.rs                # axum request handlers
    └── urls.rs                 # `pub fn api()` route aggregator + `admin_router(pool)`
```

Es gibt ein einziges Binary: `cargo run` startet den HTTP-Server, und jedes Django-artige Verb (`migrate`, `makemigrations`, `startapp`, `check`, …) läuft über dasselbe Binary via `cargo run -- <verb>`. Es gibt kein separates `manage`-Binary.

`Cargo.toml` ist das Abhängigkeits-Manifest (wie `composer.json` oder ein `Gemfile`). Öffne es und bestätige, dass `rustango` unter `[dependencies]` aufgeführt ist.

> **Bestätige den `[features]`-Block — wähle ein Datenbank-Backend.** `#[derive(Model)]`
> cfg-gated seine generierten `FromRow`- / `LoadRelated`-Impls anhand der Features
> **deiner** Crate (ein `cfg` innerhalb eines Derive-Makros wird gegen die Ziel-Crate
> aufgelöst, nicht gegen **Rustango**), also muss hier ein Backend-Feature aktiviert sein,
> sonst kompiliert das erste Modell nicht. Ein aktuelles Gerüst enthält:
>
> ```toml
> [features]
> default  = ["postgres"]            # the backend `cargo run` uses
> postgres = ["rustango/postgres"]
> sqlite   = ["rustango/sqlite"]
> mysql    = ["rustango/mysql"]
> ```
>
> Wenn deine generierte `Cargo.toml` **keinen** `[features]`-Block hat (ein älteres
> `cargo-rustango`), füge den obigen von Hand hinzu — das behebt es immer. Ohne ihn
> schlägt der Build fehl mit *"the trait bound `…: MaybePgFromRow` is not satisfied"*
> plus einem verräterischen `warning: unexpected cfg condition value: postgres`.

---

## Schritt 3: Deine Umgebung einrichten

Die Konfiguration liegt in einer `.env`-Datei, genau wie bei Django oder Laravel. Kopiere die Vorlage:

```bash
cp .env.example .env
```

Die generierte `.env` ist von Haus aus Docker-freundlich. Da wir `cargo` auf dem Host ausführen (nicht im Dev-Container), ändere den Datenbank-Host von `postgres` auf `localhost`:

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
RUSTANGO_BIND=0.0.0.0:8080
RUSTANGO_APEX_DOMAIN=localhost
RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more
```

Die Zugangsdaten, der Port und der Datenbankname (`myblog_dev`) passen bereits zum Postgres-Dienst aus der `docker-compose.yml`, du musst sie also nicht anfassen.

`RUSTANGO_SESSION_SECRET` signiert Sessions und Tokens, also liefere nicht den Platzhalter aus. Generiere ein echtes und füge es ein:

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

---

## Schritt 4: Die Datenbank starten

> **Du nutzt SQLite?** Überspring diesen Schritt — es gibt keinen Server zu
> starten. Stell sicher, dass in `.env` `DATABASE_URL=sqlite://myblog_dev.db?mode=rwc`
> steht, und häng an jedes `cargo run` unten `--no-default-features --features sqlite` an.
>
> **Du nutzt ein natives Postgres?** Es läuft bereits als Dienst; prüf nur, dass
> `psql "$DATABASE_URL" -c "SELECT version();"` durchgeht, und spring weiter.


Das Projekt bringt eine `docker-compose.yml` mit, die Postgres in einem Container ausführt, sodass du keine Datenbank von Hand installieren musst. Die App selbst führen wir mit `cargo` auf dem Host aus, also starte nur den `postgres`-Dienst im Hintergrund (die Compose-Datei definiert außerdem einen optionalen `rust`-Dev-Container, der andernfalls Port 8080 belegen würde):

```bash
docker compose up -d postgres
```

Bestätige, dass er läuft:

```bash
docker compose ps
psql "$DATABASE_URL" -c "SELECT version();"   # should print Postgres version
```

---

## Schritt 5: Die integrierten Migrationen ausführen

Migrationen erstellen deine Datenbanktabellen, dieselbe Idee wie `php artisan migrate` oder `rails db:migrate`. Führe sie einmal aus, um die eigenen Tabellen des Frameworks einzurichten:

```bash
cargo run -- migrate
```

Der erste Kompiliervorgang dauert ~2 Minuten (Rust baut alles aus dem Quellcode). Ein frisches Projekt bringt noch keine Migrationsdateien mit, du siehst also `nothing to migrate (already up to date)` — `migrate` richtet dennoch die Audit-Log-Tabelle des Frameworks ein, damit auditierte Modelle sofort funktionieren, sobald du sie hinzufügst. Deine erste echte Migration generierst du in Schritt 9.

Prüfe den Migrationsstatus:

```bash
cargo run -- showmigrations
```

Bei einem frischen Projekt gibt das `(no migrations in ./migrations)` aus. Sobald du ein Modell erstellst und `makemigrations` ausführst (Schritt 9), erscheint hier für jede angewendete Migration ein `[X]`.

---

## Schritt 6: Erster Start

Starte den Server, um sicherzustellen, dass alles verdrahtet ist.

```bash
cargo run
```

Du siehst:

```
listening on http://0.0.0.0:8080
```

Öffne <http://localhost:8080> in deinem Browser. Das Gerüst bringt einen einfachen Root-Handler (`views::index`) mit, der dich mit **Hello from Rustango!** und einem Link zum Admin begrüßt — das bestätigt, dass **Rustango** läuft. (Projekte, die keine eigene `/`-Route definieren, bekommen stattdessen eine integrierte Willkommensseite über `Cli::with_welcome()`.)

Drücke Strg-C zum Stoppen.

---

## Schritt 7: Eine App erstellen

Eine „App" ist ein in sich geschlossenes Feature-Modul, genau wie eine Django-App. Deine Blog-App wird das Post-Modell, seine Routen und seine Templates enthalten.

```bash
cargo run -- startapp blog
```

Das schreibt:

```
src/blog/
├── mod.rs
├── models.rs              # a starter model named after the app (you'll replace it)
├── views.rs               # axum handlers
├── urls.rs                # blog-specific routes (pub fn api())
└── tests.rs               # in-process router + inventory smoke tests
```

`startapp` verdrahtet das neue Modul für dich (ähnlich wie das Hinzufügen zu Djangos `INSTALLED_APPS`): Es deklariert `mod blog;` in `src/main.rs` und fügt eine `.merge(crate::blog::urls::api())`-Zeile in den `api()`-Aggregator in `src/urls.rs` ein, sodass sich die Routen des Blogs automatisch in die App einfügen. Keine manuelle Modulregistrierung nötig.

---

## Schritt 8: Ein Modell definieren

Ein Modell ist eine Datenbanktabelle, beschrieben als Rust-Struct, wie ein Django-Modell oder eine Eloquent-/Active-Record-Klasse. Öffne `src/blog/models.rs` und definiere deinen `Post`. (Für die vollständige Referenz — jeden Feldtyp, benutzerdefinierte Primärschlüssel und alle Attribute — siehe den [Modelle-Leitfaden](models.md).)

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

Ein paar Rust-Dinge, die zu beachten sind:

- `#[derive(Model, ...)]` ist ein **Derive-Makro**: Es generiert automatisch Code für das Struct, so wie es ein Klassendekorator oder eine Basisklasse in anderen Frameworks täte. Das Ableiten von `Model` verleiht dem Struct seine Abfragemethoden.
- `Auto<i64>` markiert ein Feld, das die Datenbank für dich befüllt (ein automatisch hochzählender `i64`-Integer), wie ein Auto-Primärschlüssel.
- `Option<...>` bedeutet „dieser Wert kann fehlen". `Option<DateTime<Utc>>` ist ein Zeitstempel, der null sein kann, sodass `deleted_at` leer ist, bis die Zeile per Soft-Delete gelöscht wird.
- Die `#[rustango(...)]`-Attribute konfigurieren jedes Feld (maximale Länge, Defaults, Indizes), und der `admin(...)`-Block richtet die Spalten und Filter der Admin-Oberfläche ein.

---

## Schritt 9: Die Migration erstellen und anwenden

Verwandle dieses Modell nun in eine echte Tabelle. Generiere zunächst die Migration aus deinem Modell (wie `makemigrations` in Django):

```bash
cargo run -- makemigrations
```

Du siehst etwa Folgendes:

```
wrote ./migrations/0001_create_item_and_posts_and_rustango_admin_users_etc.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateTable("rustango_admin_users")
    + CreateTable("rustango_content_types")
    + CreateIndex { table: "posts", columns: ["status", "published_at"], ... }
```

Diese erste Migration erstellt deine Modelle — `posts`, plus das Starter-Modell `item`, das das Gerüst in `src/models.rs` mitgeliefert hat — zusammen mit den Admin- und Content-Type-Tabellen des Frameworks. Öffne die JSON, wenn du willst: Sie enthält die Operationen plus einen vollständigen Schema-Snapshot.

Wende sie auf die Datenbank an:

```bash
cargo run -- migrate
```

Bestätige, dass die Tabelle existiert:

```bash
psql "$DATABASE_URL" -c "\d posts"
```

---

## Schritt 10: Das ORM ausprobieren

Lass uns Zeilen aus dem Code lesen und schreiben. Das ORM lässt dich mit Datenbankzeilen als Rust-Structs arbeiten statt mit rohem SQL, wie Djangos ORM, Eloquent oder Active Record.

Bearbeite `src/main.rs` vorübergehend, um vor dem Serverstart einen schnellen Erstellen-und-Lesen-Test auszuführen. Ersetze den `Cli`-Rumpf durch einen Ad-hoc-ORM-Smoke-Test (behalte das `#[rustango::main]` des Scaffolders und die `mod`-Deklarationen am Anfang der Datei):

```rust
mod blog;
mod models;
mod urls;
mod views;

use crate::blog::models::Post;
use rustango::{Auto, Model};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

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
    let posts = Post::objects().fetch_on(&pool).await?;
    for post in &posts {
        println!("- {}", post.title);
    }

    Ok(())
}
```

Was hier geschieht, in einfachen Worten:

- `pool` ist der gemeinsam genutzte Datenbank-Verbindungspool. Du übergibst eine Referenz darauf (`&pool`) an Abfrageaufrufe, statt jedes Mal eine neue Verbindung zu öffnen.
- Datenbankaufrufe sind asynchron, daher endet jeder mit `.await` — das pausiert, bis das Ergebnis zurückkommt, und macht dann weiter. Das `?` nach einem `.await` sagt „falls das einen Fehler ergab, halte an und gib den Fehler zurück".
- `main` gibt ein `Result` zurück, Rusts Erfolg-oder-Fehler-Typ, weshalb `?` und das abschließende `Ok(())` funktionieren.
- Um eine Zeile zu speichern, rufe `.save(&pool)` darauf auf. Um Zeilen zu lesen, baue eine Abfrage mit `Post::objects()` und führe sie mit `.fetch_on(&pool)` aus — das grobe Äquivalent zu Djangos `Post.objects.all()`. (`.save(&pool)` / `.fetch_on(&pool)` nehmen einen `sqlx::PgPool`; die schlichte `.fetch(&pool)`-Variante nimmt stattdessen einen Multi-Backend-`rustango::sql::Pool` — siehe den [ORM-Leitfaden](orm.md).)

Führe es aus:

```bash
cargo run
```

Du solltest die ID deines neuen Beitrags und die zurückgelesenen Zeilen sehen. Stelle `src/main.rs` wieder in seine gescaffoldete Server-Form zurück, sobald du bestätigt hast, dass es funktioniert — der nächste Schritt baut darauf auf.

---

## Schritt 11: Den Auto-Admin einschalten

**Rustango** bringt eine generierte Admin-Oberfläche für deine Modelle mit, genau wie Djangos Admin. Der Scaffolder hat dir bereits einen `admin_router(pool)`-Helfer in `src/urls.rs` gegeben, der den Auto-Admin aus einem Pool baut — du musst ihn nur unter `/admin` einhängen und in das `Cli` einspeisen.

Gib dem Admin zunächst einen Titel in `src/urls.rs`. Das `admin_prefix` muss zu dem Pfad passen, unter dem du ihn im nächsten Schritt einhängst (`/admin`), damit die eigenen Links und Formularaktionen des Admins aufgelöst werden:

```rust
pub fn admin_router(pool: PgPool) -> Router {
    admin::Builder::new(pool)
        .title("Myblog Admin")
        .admin_prefix("/admin") // must match the `.nest("/admin", …)` below
        .build()
}
```

Verbinde dann einen Pool in `src/main.rs` und hänge den Admin in den API-Router ein, bevor du ihn an das `Cli` übergibst. Behalte die `mod blog;`-Zeile aus Schritt 7 — die registriert dein `Post`-Modell beim Admin:

```rust
mod blog;
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    let api = urls::api().nest("/admin", urls::admin_router(pool));

    rustango::manage::Cli::new()
        .api(api)
        .with_health() // /health + /ready endpoints
        .run()
        .await
}
```

`Cli::new()...run()` ist derselbe vereinheitlichte Dispatcher, den der Scaffolder generiert hat — er bedient weiterhin jedes `cargo run -- <verb>`; du hast nur den Router angereichert, den er zur Runserver-Zeit bedient.

Führe es aus:

```bash
cargo run
```

Öffne <http://localhost:8080/admin> (kein abschließender Schrägstrich). Du siehst die Admin-Startseite mit einem `posts`-Link. Klicke ihn an, um deinen Entwurfsbeitrag in der Liste zu sehen, klicke den Beitrag an, um sein Bearbeitungsformular zu öffnen, und speichere. Der Audit-Trail-Tab zeichnet jeden Schreibvorgang auf.

---

## Schritt 12: Die JSON-API bauen

Ein ViewSet stellt ein Modell als REST-API mit List-, Create-, Retrieve-, Update- und Delete-Endpunkten bereit, ganz ähnlich einem ViewSet des Django REST Framework oder einem API-Resource-Controller von Laravel.

### 12a. Das ViewSet generieren

Erstelle das Gerüst der Datei, fülle dann aus, welche Felder und Verhaltensweisen exponiert werden sollen:

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Bearbeite `src/post_view_set.rs`:

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
)]
pub struct PostViewSet;
```

Registriere das neue Modul, indem du `mod post_view_set;` zu den anderen `mod`-Deklarationen am Anfang von `src/main.rs` hinzufügst.

### 12b. Die Routen einhängen

Hänge die Routen des ViewSet an den Router der App an (die **Rustango**-Version einer `urls.py`- oder `routes/api.php`-Datei). Der ViewSet-Router braucht den Datenbankpool, also baue ihn in `src/main.rs`, wo der Pool lebt, und merge ihn in den `urls::api()`-Aggregator:

```rust
let api = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool));

rustango::manage::Cli::new()
    .api(api)
    .with_health()
    .run()
    .await
```

(`urls::api()` ist der Aggregator, den der Scaffolder generiert hat; `manage startapp` merged die Routen jeder Unter-App auf dieselbe Weise hinein.)

### 12c. Die Endpunkte ausprobieren

Starte den Server:

```bash
cargo run
```

In einem anderen Terminal rufe die API mit `curl` auf:

```bash
curl http://localhost:8080/api/posts                                    # list
curl -X POST http://localhost:8080/api/posts \
     -H "content-type: application/json" \
     -d '{"title":"From API","body":"Yo","status":"published","author_id":1}'
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

---

## Schritt 13: Die Ausgabe mit einem Serializer formen

Standardmäßig gibt das ViewSet jedes Modellfeld zurück. Ein Serializer lässt dich die Form der Antwort steuern: interne Felder verbergen, sie umbenennen oder einige als read-only markieren. Es ist dieselbe Rolle wie ein DRF-Serializer oder eine API-Resource von Laravel.

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Bearbeite `src/post_serializer.rs`:

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

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

Der Typ jedes Serializer-Felds spiegelt das passende Modellfeld wider, sodass `id` und `published_at` ihren `Auto<…>`-Wrapper vom Modell behalten (ein `Auto<i64>` serialisiert weiterhin zu einem schlichten JSON-Integer). Registriere dann das Modul, indem du `mod post_serializer;` zu den anderen `mod`-Deklarationen in `src/main.rs` hinzufügst.

Verdrahte den Serializer mit dem ViewSet über das `serializer`-Attribut — List-, Retrieve- und Create-Antworten werden dann durch ihn gerendert (die feldbasierte `fields`-Projektion wird zugunsten der Form des Serializers umgangen):

```rust
#[derive(ViewSet)]
#[viewset(
    model = Post,
    serializer = crate::post_serializer::PostSerializer,
    ordering = "-published_at",
)]
pub struct PostViewSet;
```

Das funktioniert identisch auf PostgreSQL, MySQL und SQLite. `method`- / `read_only`- / `source`- / `write_only`-Overrides gelten alle für die Antwort, und **Request-Bodies werden ebenfalls durch den Serializer validiert**: `create` / `update` führen sein `validate()` aus (pro Feld und feldübergreifend) und geben bei Fehlschlag einen `400` in DRF-Form zurück (`{field: [messages]}`), und read-only- / berechnete Felder, die ein Client postet, werden ignoriert. (Hinweis: `nested`- / `many`-Serializer-Felder brauchen die zugehörigen Zeilen, geladen via `select_related`; andernfalls werden sie als ihr Default gerendert.) Siehe den [ViewSets-Leitfaden](viewsets.md) für das vollständige Eingabe- und Ausgabeverhalten.

---

## Schritt 14: JWT-Authentifizierung hinzufügen

JWTs sind signierte Tokens, die du einem Client nach dem Login übergibst und bei jeder Anfrage prüfst, ein gängiges Muster für API-Auth. Das `rustango::jwt`-Modul von **Rustango** stellt sie aus und verifiziert sie (HS256) und ist standardmäßig aktiv — kein zusätzliches Feature-Flag.

### 14a. Beim Login ein Token ausstellen

Backe die Benutzer-ID (das „Subject" des Tokens) und beliebige benutzerdefinierte Claims, wie Rollen, in ein signiertes Token und übergib es dann dem Client:

```rust
use rustango::jwt::{encode, Claims};
use std::time::Duration;

// Derive the signing key from your session secret.
let secret = std::env::var("RUSTANGO_SESSION_SECRET")?.into_bytes();

let mut claims = Claims::new(user_id.to_string());   // subject = user id
claims.set("roles", vec!["editor"]);
let token = encode(&claims.ttl(Duration::from_secs(900)), &secret)?;

// Send `token` to the client (e.g. in the login response body).
```

### 14b. Das Token bei jeder Anfrage verifizieren

Dekodiere das Token — das prüft die Signatur und die Ablaufzeit — und lies dann die Claims zurück. Falls es fehlt oder ungültig ist, weise die Anfrage als nicht autorisiert ab:

```rust
use rustango::jwt::decode;

let claims = decode(&access_token, &secret)
    .map_err(|_| StatusCode::UNAUTHORIZED)?;

let user_id = claims.subject().ok_or(StatusCode::UNAUTHORIZED)?;
let roles: Vec<String> = claims.get("roles").unwrap_or_default();
```

### 14c. Access- + Refresh-Lebenszyklus

`rustango::jwt` stellt zustandslose Einzeltokens aus. Für das vollständige Muster — kurzlebige **Access**-Tokens, ein langlebiges **Refresh**-Token in einem HttpOnly-Cookie, Rotation und eine JTI-Blacklist zum Widerruf — aktiviere das `tenancy`-Feature und verwende `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, dessen Methoden `issue_pair_with` / `verify_access` / `refresh` das Paar für dich verwalten.

---

## Schritt 15: Security-Middleware hinzufügen

Middleware umschließt jede Anfrage, um querschnittliches Verhalten zu ergänzen. Hier stapelst du Request-IDs, Access-Logging, Rate-Limiting, CORS und Security-Header in einer Kette. Jedes `.method(...)` fügt eine Schicht hinzu, ähnlich wie Django-Middleware oder Laravels Middleware-Stack. Siehe den [Middleware-Leitfaden](middleware.md) für den vollständigen Schichtenkatalog und die Reihenfolgeregeln.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use rustango::health::health_router;
use std::time::Duration;

let app = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool.clone()))
    .merge(health_router(pool.clone()))                        // /health, /ready
    .request_id(RequestIdLayer::default())
    .access_log(AccessLogLayer::default())                      // PII-redacted
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
    .cors(CorsLayer::new()
        .allow_origins(vec!["https://app.example.com"])
        .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]))
    .security_headers(
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    );
```

Übergib die fertige `app` genau wie zuvor an das `Cli` — `rustango::manage::Cli::new().api(app).with_welcome().run().await` — und jede Anfrage fließt nun durch den vollständigen Middleware-Stack.

---

## Schritt 16: Tests schreiben

**Rustango** enthält einen Testclient, der deinen Router in-process ansteuert, sodass du reale HTTP-Antworten prüfen kannst, ohne einen Server zu starten, ganz ähnlich Djangos Testclient oder Laravels HTTP-Tests. Erstelle das Gerüst einer Testdatei:

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

Die `make:*`-Generatoren nehmen einen PascalCase-Namen; `PostSmoke` wird zur snake_case-Datei `tests/post_smoke.rs`.

Bearbeite `tests/post_smoke.rs`. Integrationstests leben in einer separaten Crate, daher bauen sie den zu testenden Router direkt aus dem ViewSet (derselbe `router(...)`-Aufruf, den du in Schritt 12b eingehängt hast):

```rust
use rustango::test_client::TestClient;
use myblog::post_view_set::PostViewSet;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
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
async fn create_post_returns_the_new_object() {
    let client = TestClient::new(app().await);
    let response = client.post("/api/posts")
        .json(&json!({
            "title": "Test",
            "body":  "x",
            "status": "draft",
            "author_id": 1,
        }))
        .send().await;
    assert_eq!(response.status, 201);
    let v: serde_json::Value = response.json();
    assert_eq!(v["title"], "Test");
}
```

> **Achtung:** Integrationstests in `tests/` können nur dann `use myblog::…`, wenn die Crate ein Library-Target bereitstellt. Ein frisches Gerüst ist reines Binary (`src/main.rs`, kein `src/lib.rs`), also füge eine einzeilige `src/lib.rs` hinzu, die die Module re-exportiert, die du testen willst — `pub mod models; pub mod post_view_set; pub mod urls;` — und behalte die passenden `mod …;`-Zeilen in `src/main.rs`. (Wenn du lieber kein Library-Target hinzufügen möchtest, baue den Router stattdessen vollständig inline im Test, so wie `make:test` sein `app()` scaffoldet.)

Führe die Tests aus:

```bash
cargo test --test post_smoke
```

---

## Schritt 17: Den Systemcheck ausführen

Bevor du deployst, führe den integrierten Prüfer aus. Er meldet gängige Fehlkonfigurationen (wie ein schwaches `RUSTANGO_SESSION_SECRET` oder eine nicht erreichbare Datenbank), ähnlich Djangos `check --deploy`.

```bash
cargo run -- check --deploy
```

In deiner lokalen Dev-Umgebung siehst du etwa Folgendes:

```
running rustango system check (deploy mode)...
  [info]    6 models registered via inventory
  [info]    database reachable
  [info]    1 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET length OK
  [info]    config tier resolved to `dev`
  [warning] RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier
  [warning] DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in production
  [warning] RUSTANGO_APEX_DOMAIN is unset / `localhost` — set it for tenancy projects
```

(Die genauen Modell-/Migrationszahlen hängen von deinem Projekt ab.) Diese drei Warnungen sind die erwarteten Dev-Umgebungs-Warnungen. In einem Produktions-Setup — `RUSTANGO_ENV=prod`, eine managed-Database-`DATABASE_URL`, eine gesetzte Apex-Domain — verschwinden sie und du siehst `all checks passed`. Behebe verbleibende Warnungen oder Fehler, bevor du in die Produktion pushst.

---

## Schritt 18: In die Produktion deployen

Wie du deployst, hängt von deiner Plattform ab (Fly, Railway, Kubernetes, blankes ECS usw.). Die framework-seitigen Schritte sind überall gleich; das `--release`-Flag baut ein optimiertes Binary:

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export RUSTANGO_SESSION_SECRET=$(openssl rand -base64 32)

# 2. Run migrations
cargo run --release -- migrate

# 3. Audit
cargo run --release -- check --deploy

# 4. Build binary
cargo build --release

# 5. Run with a process supervisor (systemd / docker / k8s)
./target/release/myblog
```

Stelle sicher, dass dein Reverse-Proxy:
- HTTPS terminiert
- `X-Forwarded-For` weiterleitet für akkurate IPs im `AccessLogLayer`
- `X-Forwarded-Host`, `X-Forwarded-Proto` weiterleitet
- `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())` verwendet, damit `ConnectInfo` für Rate-Limiting + IP-Filterung befüllt ist

---

## Wie es weitergeht

| Thema | Doku |
|---|---|
| Lauffähige Version dieses Leitfadens | [`examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog) |
| Jeder `manage`-Unterbefehl | [`docs/manage.md`](manage.md) |
| ORM-Kochbuch (fortgeschrittene Filter, Aggregationen, M2M, Soft-Delete) | [`docs/orm.md`](orm.md) |
| Middleware (der vollständige Schichtenkatalog + Reihenfolge) | [`docs/middleware.md`](middleware.md) |
| Performance-Benchmarks (vs. Go) | [`docs/benchmarks.md`](benchmarks.md) |
| API-Konventionen (Benennung, Builder-Muster, Feature-Gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Security-Features im Detail | [`docs/security.md`](security.md) |
| Django-Parity-Audit | [`docs/django-parity-audit-2026-05-21.md`](https://github.com/ujeenet/rustango/blob/main/docs/django-parity-audit-2026-05-21.md) |
| Multi-Tenancy | [README — Abschnitt Multi-tenancy](https://github.com/ujeenet/rustango/blob/main/README.md#multi-tenancy) |
| API-Doku | <https://docs.rs/rustango> |

Wenn du auf etwas stößt, das nicht funktioniert oder unklar ist, öffne ein Issue.
