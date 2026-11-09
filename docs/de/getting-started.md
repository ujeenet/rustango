# Erste Schritte: einen Blog mit Rustango bauen

Diese Anleitung führt dich von einem leeren Verzeichnis bis zu einem deployten Blog: Beiträge, eine Admin-Oberfläche, eine JSON-API mit Registrierung und Login (JWT) und Tests. Von Anfang bis Ende.

> **Dauer:** ~45 Minuten für die komplette Tour, ~10 Minuten, wenn du es nur laufen sehen willst.
>
> **Lauffähige Version:** Jeder Schritt unten ist in einem getesteten, kompilierbaren Beispiel unter [`crates/rustango/examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog) nachgebildet. Falls ein Schritt jemals seltsam aussieht, vergleiche ihn damit.

[![Einen Blog mit Rustango bauen: die Migration generieren, sie anwenden, den Server starten und die JSON-API abfragen — alles aus einem einzigen Binary](../img/getting-started.png)](../img/getting-started.png)

---

## Was du vorher wissen solltest

Zwei verschiedene Fragen — und die Doku hat bisher nur die zweite beantwortet.

**Rust wird vorausgesetzt.** Kein Experten-Rust, aber du solltest mit Structs,
Traits, `Result` und `?` vertraut sein und genug `async`/`.await` beherrschen, um
eine Funktion zu lesen, ohne etwas nachschlagen zu müssen. Wenn das noch nicht auf
dich zutrifft, kommt zuerst das [Rust Book](https://doc.rust-lang.org/book/);
diese Anleitung bringt dir die Sprache darunter nicht bei.

**Erfahrung mit Web-Backends wird nicht vorausgesetzt.** Wenn du noch nie eine
Web-API gebaut hast, fang mit [Grundlagen der Web-APIs](glossary.md#grundlagen-der-web-apis)
im Glossar an. Das ist eine Fünf-Minuten-Einführung zu Requests, Routen, Handlern
und Migrationen, und sie ist genau für diese Lücke geschrieben. Komm danach hierher
zurück.

**Vorkenntnisse aus einem anderen Web-Framework werden nicht vorausgesetzt.**
Wo diese Doku gelegentlich einen Vergleich zieht, ist das eine Nebenbemerkung,
nie die Erklärung: Sagt dir die Parallele nichts, überspring sie — der Schritt
steht auch für sich. Wo ein Begriff echte Arbeit leistet, erklärt ihn das
[Glossar](glossary.md) in einfachen Worten.

## Was du installiert haben musst

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
| MySQL oder MariaDB | Mit `--backend mysql` generieren | Siehe [MySQL](#mysql). |

Was du auch wählst: Das Einzige, was sich ändert, ist `DATABASE_URL`. So sieht sie jeweils aus:

```bash
DATABASE_URL=postgres://user:password@localhost:5432/myblog_dev
DATABASE_URL=mysql://user:password@localhost:3306/myblog_dev
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

#### SQLite — ohne jede Einrichtung

Generiere das Projekt gleich für SQLite: dann gibt es nichts zu installieren,
nichts zu starten und hinterher nichts zu bearbeiten.

```bash
cargo rustango new myblog --backend sqlite
```

Die generierten `.env.example`, `docker-compose.yml` und Settings-Stufen sind
alle für SQLite geschrieben, die Datenbank ist eine Datei, die das erste
`cargo run -- migrate` anlegt, und du kannst
[Schritt 4](#schritt-4-die-datenbank-starten) komplett überspringen.

Hast du bereits ein Postgres-Projekt generiert und willst umsteigen: jede
Vorlage lässt alle drei Backends verdrahtet — es ist also ein Flag plus eine
URL:

```bash
cargo run --no-default-features --features sqlite
```

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

> **Läuft bei dir schon lokal ein Postgres?** Dann ist Port 5432 belegt, und der
> Container verliert das Rennen stillschweigend. Deine App verbindet sich mit dem
> lokalen Server, der keine deiner Tabellen hat. Der Fehler, der zurückkommt, ist
> nicht lesbar, weil ein nicht englischsprachiger Server seine Meldung in seiner
> eigenen Kodierung schickt. Stoppe entweder den lokalen Dienst oder leg den
> Container auf einen anderen Port.

#### MySQL

Generiere mit `--backend mysql`, dann sind die erzeugten `.env.example`, die
`docker-compose.yml` und die Settings-Stufen alle für MySQL geschrieben:

```bash
cargo rustango new myblog --backend mysql
```

Willst du es nativ statt im Container betreiben, brauchst du eine Datenbank und
einen Benutzer:

```sql
CREATE DATABASE myblog_dev CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs;
CREATE USER 'rustango'@'localhost' IDENTIFIED BY 'rustango';
GRANT ALL PRIVILEGES ON myblog_dev.* TO 'rustango'@'localhost';
```

```bash
DATABASE_URL=mysql://rustango:rustango@localhost:3306/myblog_dev
```

`utf8mb4` lohnt sich bewusst zu setzen: Das ältere `utf8` von MySQL ist drei Bytes
breit und kann kein Emoji speichern, was viel später als Schreibvorgang auftaucht,
der an einer einzigen Zeile scheitert. MariaDB läuft über denselben Treiber und
dasselbe URL-Schema.

Auch die Kollation zählt. Spalten erben den Standard der Datenbank, und der von
MySQL (`utf8mb4_0900_ai_ci`) ignoriert Groß-/Kleinschreibung und Akzente:
`.eq("/About")` trifft `/about`, und eine `unique`-Spalte lehnt `Hero` neben `hero`
ab, anders als PostgreSQL und SQLite. `utf8mb4_0900_as_cs` vergleicht wie diese.
Meide eine `_bin`-Kollation: Der Treiber kann ihre Textspalten nicht als `String`
lesen. `manage check --deploy` warnt vor beidem.

---

## Schritt 1: Den Scaffolder installieren

Der Scaffolder generiert für dich Projekt- und App-Gerüste.

```bash
cargo install cargo-rustango
```

Das ergänzt den `cargo rustango ...`-Unterbefehl global. Bestätige, dass er vorhanden ist:

```bash
cargo rustango --help
```

Die Version des Scaffolders ist die, die dein Projekt pinnt — installierst du den neuesten, bekommst du das neueste rustango. Um ein Projekt auf einem älteren Release zu generieren, installiere stattdessen jenen Generator (`cargo install cargo-rustango --version 0.65.0`) — siehe [Scaffolding](scaffolding.md#die-version-des-generators-ist-die-die-dein-projekt-bekommt).

---

## Schritt 2: Das Projekt erstellen

Das erzeugt ein frisches Projekt mit fertigem Layout, Einstellungen und Migrationen.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Rufst du `cargo rustango new` ganz ohne Argumente auf, fragt es nach Vorlage,
Backend und zusätzlichen Features und gibt die entsprechende Kommandozeile
aus, bevor es etwas anlegt — siehe [Scaffolding](scaffolding.md).

Das wurde generiert:

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

Es gibt ein einziges Binary: `cargo run` startet den HTTP-Server, und jedes Verwaltungsverb (`migrate`, `makemigrations`, `startapp`, `check`, …) läuft über dasselbe Binary via `cargo run -- <verb>`. Es gibt kein separates `manage`-Binary.

Die Module der App liegen im Library-Target (`src/lib.rs`), und `src/main.rs` erreicht sie über den Crate-Namen (`use myblog::urls;`). Dadurch können Tests in `tests/` und zusätzliche Binaries in `src/bin/` deine Modelle nutzen.

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

Die Konfiguration liegt in einer `.env`-Datei. Kopiere die Vorlage:

```bash
cp .env.example .env
```

Die generierte `.env` ist von Haus aus Docker-freundlich. Da wir `cargo` auf dem Host ausführen (nicht im Dev-Container), ändere den Datenbank-Host von `postgres` auf `localhost`:

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

Die Zugangsdaten, der Port und der Datenbankname (`myblog_dev`) passen bereits zum Postgres-Dienst aus der `docker-compose.yml`, du musst sie also nicht anfassen.

`RUSTANGO_SESSION_SECRET` signiert Sessions und Tokens. In der generierten `.env.example` ist es **auskommentiert**, und für die Entwicklung kannst du das so lassen: der erste Start erzeugt einen Schlüssel unter `./var/` und verwendet ihn weiter, sodass Neustarts dich nicht ausloggen.

Für die Produktion setze einen echten — 32 Bytes Base64, aus deinem Secret-Manager oder:

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

Ein Wert, der nicht 32 Bytes Base64 ist, kann nicht verwendet werden. Außerhalb der Prod-Stufe sagt der Server das beim Start und fällt auf den generierten Schlüssel zurück — achte also auf diese Warnung, wenn du einen gesetzt hast und Sessions sich verhalten, als hättest du es nicht. Mit `RUSTANGO_ENV=prod` verhindert ein fehlendes oder ungültiges Secret den Start des Servers.

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

Migrationen erstellen deine Datenbanktabellen. Führe sie einmal aus, um die eigenen Tabellen des Frameworks einzurichten:

```bash
cargo run -- migrate
```

Der erste Kompiliervorgang dauert ~2 Minuten (Rust baut alles aus dem Quellcode). Ein frisches Projekt bringt noch keine eigenen Migrationsdateien mit, aber `migrate` ist kein No-op: Es generiert zuerst die Migrationen des Frameworks aus den kompilierten Modellen und wendet sie an, du siehst also ein paar `applied …`-Zeilen statt `nothing to migrate`. (Diese Meldung erscheint erst, wenn alles — Framework und Projekt — schon auf dem neuesten Stand ist.) Außerdem legt es die Audit-Log-Tabelle an, damit auditierte Modelle sofort funktionieren, sobald du sie hinzufügst. Deine erste Projekt-Migration generierst du in Schritt 9.

Prüfe den Migrationsstatus:

```bash
cargo run -- showmigrations
```

Bei einem frischen Projekt gibt das `(no migrations in ./migrations)` aus, danach die eigene Kette des Frameworks unter `System migrations in ./system/migrations:`. Sobald du ein Modell erstellst und `makemigrations` ausführst (Schritt 9), erscheint hier für jede angewendete Migration ein `[X]`.

---

## Schritt 6: Erster Start

Starte den Server, um sicherzustellen, dass alles verdrahtet ist.

```bash
cargo run
```

Du siehst:

```
INFO rustango::server: listening on http://127.0.0.1:8080
```

`cargo run` wendet außerdem ausstehende Migrationen an, bevor es zu lauschen beginnt.

Öffne <http://localhost:8080> in deinem Browser. Das Gerüst bringt einen einfachen Root-Handler (`views::index`) mit, der dich mit **Hello from Rustango!** und einem Link zum Admin begrüßt — das bestätigt, dass **Rustango** läuft. (Projekte, die keine eigene `/`-Route definieren, bekommen stattdessen eine integrierte Willkommensseite über `Cli::with_welcome()`.)

Drücke Strg-C zum Stoppen.

---

## Schritt 7: Eine App erstellen

Eine „App" ist ein in sich geschlossenes Feature-Modul. Deine Blog-App wird das Post-Modell, seine Routen und seine Templates enthalten.

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

`startapp` verdrahtet das neue Modul für dich: Es deklariert `pub mod blog;` in `src/lib.rs` (oder `mod blog;` in `src/main.rs` bei einem älteren reinen Binary-Projekt) und fügt eine `.merge(crate::blog::urls::api())`-Zeile in den `api()`-Aggregator in `src/urls.rs` ein, sodass sich die Routen des Blogs automatisch in die App einfügen. Keine manuelle Modulregistrierung nötig.

---

## Schritt 8: Ein Modell definieren

Ein Modell ist eine Datenbanktabelle, beschrieben als Rust-Struct — eine Active-Record-Klasse in Rust. Öffne `src/blog/models.rs` und definiere deinen `Post`. (Für die vollständige Referenz — jeden Feldtyp, benutzerdefinierte Primärschlüssel und alle Attribute — siehe den [Modelle-Leitfaden](models.md).)

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

Verwandle dieses Modell nun in eine echte Tabelle. Generiere zunächst die Migration aus deinem Modell:

```bash
cargo run -- makemigrations
```

Du siehst etwa Folgendes:

```
wrote ./migrations/0001_create_item_and_posts.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateIndex { name: "posts_status_published_at_idx", table: "posts", columns: ["status", "published_at"], ... }
```

Diese erste Migration erstellt deine Modelle — `posts`, plus das Starter-Modell `item`, das das Gerüst in `src/models.rs` mitgeliefert hat. Die Tabellen des Frameworks selbst (Admin-Benutzer, Content-Types, Audit-Log, …) sind nicht darin: Sie haben eine eigene Kette in `system/migrations/`, die das erste `migrate` schreibt und vor deiner anwendet. Öffne die JSON, wenn du willst: Sie enthält die Operationen plus einen vollständigen Schema-Snapshot.

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

Lass uns Zeilen aus dem Code lesen und schreiben. Das ORM lässt dich mit Datenbankzeilen als Rust-Structs arbeiten statt mit rohem SQL.

Bearbeite `src/main.rs` vorübergehend, um vor dem Serverstart einen schnellen Erstellen-und-Lesen-Test auszuführen. Ersetze den `Cli`-Rumpf durch einen Ad-hoc-ORM-Smoke-Test (behalte das `#[rustango::main]` des Scaffolders). Die Modelle liegen in der Library, also benennt `main.rs` sie über den Crate-Namen:

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

Was hier geschieht, in einfachen Worten:

- `pool` ist der gemeinsam genutzte Datenbank-Verbindungspool. Du übergibst eine Referenz darauf (`&pool`) an Abfrageaufrufe, statt jedes Mal eine neue Verbindung zu öffnen.
- Datenbankaufrufe sind asynchron, daher endet jeder mit `.await` — das pausiert, bis das Ergebnis zurückkommt, und macht dann weiter. Das `?` nach einem `.await` sagt „falls das einen Fehler ergab, halte an und gib den Fehler zurück".
- `main` gibt ein `Result` zurück, Rusts Erfolg-oder-Fehler-Typ, weshalb `?` und das abschließende `Ok(())` funktionieren.
- Um eine Zeile zu speichern, rufe `.save(&pool)` darauf auf. Um Zeilen zu lesen, baue eine Abfrage mit `Post::objects()` und führe sie mit `.fetch(&pool)` aus — ohne Filter liefert das jede Zeile der Tabelle.
- `.fetch(…)` stammt aus dem `FetcherPool`-Trait, weshalb die Imports es hereinholen. Ohne diese Zeile existiert die Methode nicht, und der Compiler sagt dir das, ohne zu erklären, warum.
- Das sind die Multi-Backend-Aufrufe, und alles oben kompiliert unverändert auf allen drei Datenbanken. Siehe den [ORM-Leitfaden](orm.md).

Führe es aus:

```bash
cargo run
```

Du solltest die ID deines neuen Beitrags und die zurückgelesenen Zeilen sehen. Stelle `src/main.rs` wieder in seine gescaffoldete Server-Form zurück, sobald du bestätigt hast, dass es funktioniert — der nächste Schritt baut darauf auf. Entferne auch die drei `use`-Zeilen oben: Die Server-Form nutzt sie nicht, und der Compiler warnt vor ungenutzten Imports.

---

## Schritt 11: Den Auto-Admin einschalten

**Rustango** bringt eine generierte Admin-Oberfläche für deine Modelle mit — ein fertiges Backoffice zum Durchsuchen, Suchen und Bearbeiten deiner Daten, ganz ohne eigenen Code. Das Fullstack-Gerüst verdrahtet ihn bereits: Ein Helfer in `src/urls.rs` verwandelt einen Pool in einen Admin-Router, und `src/main.rs` hängt ihn unter `/admin` ein.

Der Helfer stellt den Admin hinter ein Login. Er liest seine Optionen aus den geladenen Settings, und `admin_prefix` muss zu dem Pfad passen, unter dem er eingehängt ist, damit die eigenen Links und Formularaktionen des Admins aufgelöst werden. `admin_prefix(settings)` liefert `[admin] url_prefix` (oder `[routes] admin_url`), sonst `/admin`:

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

> **Behalte `.with_session_auth(...)`.** Ohne es hat der Admin kein Login: Jeder, der `/admin` erreicht, kann jedes Modell lesen, bearbeiten und löschen. `check --deploy` warnt, wenn ein Admin ohne es gebaut wird (siehe [admin.md](admin.md)).

`Builder::new` nimmt den Pool jedes Backends, sodass dieser Helfer keinen Treiber nennt und auf allen dreien funktioniert.

`src/main.rs` übergibt den Helfer an das `Cli`, das den Admin aus dem Pool baut, mit dem es serviert; Verben wie `makemigrations` laufen also weiterhin ohne Datenbank. Behalte die `pub mod blog;`-Zeile aus Schritt 7 in `src/lib.rs` — die registriert dein `Post`-Modell beim Admin. Die gescaffoldete `main.rs` sieht so aus (gekürzt):

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

Lege das Konto an, mit dem du dich anmeldest (das Passwort wird abgefragt):

```bash
cargo run -- create-admin alice --superuser
```

`Cli::new()...run()` ist derselbe vereinheitlichte Dispatcher, den der Scaffolder generiert hat — er bedient weiterhin jedes `cargo run -- <verb>`; du hast nur den Router angereichert, den er zur Runserver-Zeit bedient.

Führe es aus:

```bash
cargo run
```

Öffne <http://localhost:8080/admin> (kein abschließender Schrägstrich) und melde dich als `alice` an. Du siehst die Admin-Startseite mit einem `posts`-Link. Klicke ihn an, um deinen Entwurfsbeitrag in der Liste zu sehen, klicke den Beitrag an, um sein Bearbeitungsformular zu öffnen, und speichere. Der Audit-Trail-Tab zeichnet jeden Schreibvorgang auf.

---

## Schritt 12: Die JSON-API bauen

Ein ViewSet stellt ein Modell als REST-API mit List-, Create-, Retrieve-, Update- und Delete-Endpunkten bereit — aus einer einzigen Deklaration, ohne dass du die Routen von Hand schreibst.

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
    read_only,
)]
pub struct PostViewSet;
```

Registriere das neue Modul, indem du `pub mod post_view_set;` zu `src/lib.rs` hinzufügst (`make:viewset` gibt dir diese Zeile aus).

Behalte `read_only` vorerst: Die API bedient nur List und Retrieve. Ohne es würde das ViewSet Create, Update und Delete für jeden bedienen, daher verweigert es das Einhängen, bis du sagst, wer schreiben darf. [Schritt 14](#schritt-14-registrierung-login-und-jwt-authentifizierung) fügt Konten hinzu und schaltet Schreibzugriffe für angemeldete Benutzer frei.

### 12b. Die Routen einhängen

Hänge die Routen des ViewSet an die App an. Der ViewSet-Router braucht den Datenbankpool, und `urls::api()` hat keinen, also füge der `Cli`-Kette in `src/main.rs` ein weiteres `nest_with` hinzu. Es bekommt den Pool des Servers, wie der Admin:

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

Das ViewSet antwortet jetzt unter `/api/posts` und `/api/posts/{pk}`. Der Pfad ist die Tabelle des Modells; der Kommentar am Ende von `src/post_view_set.rs` zeigt dieselbe Zeile, und `make:viewset` gibt die URL aus.

### 12c. Die Endpunkte ausprobieren

Starte den Server:

```bash
cargo run
```

In einem anderen Terminal rufe die API mit `curl` auf:

```bash
curl http://localhost:8080/api/posts                                    # list
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

Ein `POST` antwortet vorerst mit `405 Method Not Allowed`. In Schritt 14 schreibst du Beiträge über die API.

---

## Schritt 13: Die Ausgabe mit einem Serializer formen

Standardmäßig gibt das ViewSet jedes Modellfeld zurück. Ein Serializer lässt dich die Form der Antwort steuern: interne Felder verbergen, sie umbenennen oder einige als read-only markieren. Er ist der Vertrag zwischen deinen Modellen und dem JSON, das deine API ausliefert.

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

    pub author_id: i64,                                 // writable: NOT NULL with no default

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

Sobald ein Serializer angehängt ist, **bilden seine Felder die gesamte Schreiboberfläche**: Alles, was ein Client postet und hier nicht aufgeführt ist, wird vor dem `INSERT` verworfen. Deshalb taucht `author_id` hier auf. Im Modell ist es `NOT NULL` ohne Default — lässt du es weg, scheitert jedes Create an der Not-Null-Beschränkung. `status` darf draußen bleiben, weil das Modell ihm `default = "'draft'"` mitgibt.

Der Typ jedes Serializer-Felds spiegelt das passende Modellfeld wider, sodass `id` und `published_at` ihren `Auto<…>`-Wrapper vom Modell behalten (ein `Auto<i64>` serialisiert weiterhin zu einem schlichten JSON-Integer). Registriere dann das Modul, indem du `pub mod post_serializer;` zu `src/lib.rs` hinzufügst.

Verdrahte den Serializer mit dem ViewSet über das `serializer`-Attribut — List-, Retrieve- und Create-Antworten werden dann durch ihn gerendert. `fields` bleibt Pflicht: Ein Body schreibt nur Felder, die hier und im Serializer stehen:

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

Das funktioniert identisch auf PostgreSQL, MySQL und SQLite. `method`- / `read_only`- / `source`- / `write_only`-Overrides gelten alle für die Antwort, und **Request-Bodies werden ebenfalls durch den Serializer validiert**: `create` / `update` führen sein `validate()` aus (pro Feld und feldübergreifend) und geben bei Fehlschlag einen `422` (`validation_failed`) mit einer Liste von Meldungen pro Feld in `details` zurück (`{field: [messages]}`), und read-only- / berechnete Felder, die ein Client postet, werden ignoriert. (Hinweis: `nested`- / `many`-Serializer-Felder brauchen die zugehörigen Zeilen, geladen via `select_related`; andernfalls werden sie als ihr Default gerendert.) Siehe den [ViewSets-Leitfaden](viewsets.md) für das vollständige Eingabe- und Ausgabeverhalten.

---

## Schritt 14: Registrierung, Login und JWT-Authentifizierung

Bisher kann jeder Beiträge lesen, aber niemand kann sie schreiben. In diesem Schritt legen Leute ein Konto an, melden sich an und bekommen ein Token. Mit dem Token können sie Beiträge schreiben. Lesen bleibt öffentlich.

Zuerst ein paar Begriffe:

- Ein **Passwort-Hash** ist eine Einweg-Verschlüsselung eines Passworts. Du speicherst den Hash, nie das Passwort. Beim Login wird der Versuch erneut gehasht und verglichen.
- Ein **JWT** (JSON Web Token) ist ein kleiner signierter Text. Der Server signiert ihn mit einem geheimen Schlüssel, daher kann ihn niemand ohne den Schlüssel ändern. Er enthält die Benutzer-ID und eine Ablaufzeit.
- Der Client schickt das Token bei jeder Anfrage im Header `Authorization: Bearer <token>` zurück.
- **Middleware** ist Code, der vor deinem Handler läuft. Hier prüft sie das Token.

`rustango::passwords` hasht Passwörter (argon2id). `rustango::jwt` signiert und prüft Tokens (HS256). Beides ist standardmäßig aktiv. Der fertige Code steht im Beispielprojekt [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog), und dessen `tests/auth_flow.rs` prüft jeden Fall unten.

### 14a. Ein Benutzermodell

Füge in `src/blog/models.rs` unter `Post` ein zweites Modell hinzu. Die Imports oben in der Datei bleiben, wie sie sind:

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

`unique` lässt die Datenbank eine zweite Zeile mit demselben Benutzernamen ablehnen. Erzeuge und wende die Migration an, wie in Schritt 9:

```bash
cargo run -- makemigrations     # wrote ./migrations/0002_create_api_users.json
cargo run -- migrate
```

Nimm `password_hash` nie in ein ViewSet oder einen Serializer auf: Das Framework verweigert das Einhängen, wenn eine Credential-Spalte genannt wird, sodass kein Hash über die API nach außen gelangt. Der Admin zeigt die neue Tabelle und maskiert diese Spalte.

### 14b. Der Signierschlüssel

Der Server braucht einen geheimen Schlüssel, um Tokens zu signieren. Erzeuge einen mit mindestens 32 zufälligen Bytes und trage ihn in `.env` ein:

```bash
echo "JWT_SECRET=$(openssl rand -base64 32)" >> .env
```

In PowerShell führst du dieselben drei Zeilen aus, die Schritt 18 für `RUSTANGO_SESSION_SECRET` nutzt, und trägst den Wert als `JWT_SECRET=...` in `.env` ein. Nutze in Produktion einen anderen Schlüssel und halte ihn aus git heraus. `.env` steht bereits in `.gitignore`.

### 14c. Registrieren, anmelden, das Token prüfen

Lege `src/blog/auth.rs` an. Die Datei enthält den gesamten Anmeldecode. Hier kommt sie in drei Teilen; füge sie nacheinander ein. Ergänze dann `pub mod auth;` in `src/blog/mod.rs`.

**Teil 1: Imports, Typen und kleine Helfer.**

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

`ApiError` sendet die übliche JSON-Fehlerform des Frameworks (`{"error": ..., "message": ..., "status": ...}`), daher sehen deine Fehler aus wie die des ViewSets.

**Teil 2: die zwei Handler.** Ein Handler ist eine `async fn`, die eine Route beantwortet. `State(pool)` gibt ihm den Datenbank-Pool; `Json(body)` liest den JSON-Body in `Credentials`.

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

Was `login` der Reihe nach tut:

- `login_throttle::shared().begin(...)` ist dasselbe Tor, das die eingebauten Logins nutzen. Es zählt Fehlversuche pro Client-IP und insgesamt und antwortet bei zu vielen mit `429 Too Many Requests`. Du meldest jedes Ergebnis mit `attempt.failed()` oder `attempt.succeeded()`; ein Erfolg gibt die Zählung zurück. Der Scope `LoginScope::Sso("blog-api")` gibt diesem Login eigene Grenzen.
- Ein unbekannter Benutzername und ein falsches Passwort bekommen dasselbe `401` mit derselben Meldung. Sonst könnte ein Angreifer Namen ausprobieren und erfahren, welche Konten es gibt.
- `verify_dummy_async` macht eine vollständige Passwortprüfung gegen einen Schein-Hash, wenn der Benutzer nicht existiert, sodass beide Fälle etwa gleich lange dauern.
- `Claims::new(id)` legt die Benutzer-ID als *Subject* ins Token. `.ttl(TOKEN_TTL)` lässt es nach 15 Minuten ablaufen.

**Teil 3: der Wächter und die Regel für Beiträge.**

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

- `require_user_for_writes` ist die Middleware. `GET`-Anfragen gehen durch. Für alles andere ruft sie `authenticate` auf, die mit `401` antwortet, wenn der Header fehlt, das Token verändert oder abgelaufen ist oder der Benutzer nicht mehr existiert. Bei Erfolg legt sie einen `CurrentUser` auf die Anfrage, wo der folgende Code ihn lesen kann.
- `AuthorFromToken` ist ein *Filter-Backend* des ViewSets: Es kann einschränken, welche Zeilen eine Anfrage sieht, und Werte beim Schreiben festlegen. `write_pins` speichert beim Anlegen die ID des angemeldeten Benutzers in `author_id`, egal was der Body sagt, und ein Update kann sie nie ändern. `filter_with` lässt einen Benutzer nur seine eigenen Beiträge ändern oder löschen; der Beitrag eines anderen antwortet mit `404`.

### 14d. Die Routen einhängen

Füge die Anmelderouten zu `src/blog/urls.rs` hinzu. Sie brauchen den Datenbank-Pool, daher bekommen sie neben `api()` eine eigene Funktion:

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

Ergänze dann in `src/urls.rs` eine Funktion, die alles unter `/api` zusammenführt:

```rust
/// Everything `main.rs` mounts under `/api`: the posts and the sign-in routes (Step 14).
pub fn rest_api(pool: Pool) -> Router {
    crate::post_view_set::PostViewSet::router("/posts", pool.clone())
        .merge(crate::blog::urls::auth_routes(pool))
}
```

Ersetze in `src/main.rs` die `/api`-Zeile aus Schritt 12b durch:

```rust
.nest_with("/api", urls::rest_api)
```

### 14e. Post-Schreibzugriffe schützen

Ersetze `src/post_view_set.rs` durch die Version unten. Die Attribute von `#[derive(ViewSet)]` haben keine Option für ein Filter-Backend, daher nutzt diese Datei dieselben Optionen über den `ViewSet`-Builder. `PostViewSet::router(prefix, pool)` behält seinen Namen, sodass sich der Rest deines Codes nicht ändert.

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

`allow_anonymous()` sagt dem Framework, dass die Schreibzugriffe nicht aus Versehen offen sind: Deine eigene Middleware, mit `.layer(...)` hinzugefügt, prüft jeden Schreibzugriff, bevor das ViewSet ihn sieht. Beide Zeilen zählen: Ohne `allow_anonymous()` verweigert der Router das Einhängen, und ohne `.layer(...)` könnte jeder schreiben.

Der Serializer aus Schritt 13 führt `author_id` weiterhin auf, daher zeigen die Antworten das Feld. Ein Client darf es noch senden, aber der Pin ersetzt den Wert.

### 14f. Mit curl ausprobieren

Starte den Server mit `cargo run`, dann in einem anderen Terminal:

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

Ein falsches Passwort oder ein Benutzername, den es nicht gibt, ergibt dasselbe `401 {"error":"unauthorized","message":"invalid username or password",...}`. Nach 15 Minuten läuft das Token ab und Schreibzugriffe antworten wieder mit `401`: Melde dich erneut an.

> **Gute Praxis von Anfang an**
>
> - **Geheimnis aus der Umgebung.** `JWT_SECRET` kommt aus `.env` oder der Umgebung des Servers, nie aus dem Code oder aus git. Nutze 32 oder mehr zufällige Bytes (`openssl rand -base64 32`) und pro Umgebung einen anderen Schlüssel.
> - **Kurzlebige Tokens.** 15 Minuten begrenzen den Schaden eines gestohlenen Tokens. Erhöhe das nicht auf Tage, um einen Login zu sparen.
> - **Nie Geheimnisse loggen.** Logge oder drucke keine Passwörter, Tokens oder den `Authorization`-Header, und setze ein Token nie in eine URL. Der `AccessLogLayer` aus Schritt 15 loggt keine Header oder Bodies und schwärzt Query-Werte.
> - **Allgemeine Login-Fehler.** Dasselbe `401` für einen unbekannten Benutzer und ein falsches Passwort, wie oben.
> - **Login-Versuche begrenzen.** Behalte das `login_throttle`-Tor in `login` und das Rate-Limit auf `/register`. Hinter einem Proxy setze vertrauenswürdige Proxies (Schritt 18), sonst teilen sich alle Clients die IP des Proxys.
> - **HTTPS in Produktion.** Ein Token, das über reines HTTP geht, kann unterwegs gelesen werden. Terminiere HTTPS an deinem Proxy (Schritt 18).
> - **Nur auf dem Server hashen, nur mit argon2.** Nutze `passwords::hash_async` / `verify_async`; speichere nie ein Passwort oder einen schnellen Hash wie SHA-256.
> - **Keine offenen Schreibzugriffe.** Jede Schreibroute braucht eine Prüfung, wie die Schicht in 14e. Ein ViewSet mit `allow_anonymous` und ohne Schicht lässt jeden deine Daten ändern.

### 14g. Nächster Schritt: Refresh-Tokens

`rustango::jwt` stellt einzelne, zustandslose Tokens aus: Läuft eines ab, meldet sich der Benutzer erneut an, und du kannst ein Token nicht vor seinem Ablauf widerrufen. Brauchst du längere Sitzungen oder ein Logout, das sofort wirkt, wechsle zum vollständigen Muster: kurzlebige **Access**-Tokens, ein langlebiges **Refresh**-Token in einem HttpOnly-Cookie, Rotation und eine JTI-Blacklist für den Widerruf. Aktiviere die `tenancy`-Feature und nutze `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, dessen Methoden `issue_pair_with` / `verify_access` / `refresh` das Paar für dich verwalten.

---

## Schritt 15: Security-Middleware hinzufügen

Middleware umschließt jede Anfrage, um querschnittliches Verhalten zu ergänzen. Dieser Code gehört in `src/main.rs`: Er ersetzt die `/api`-`nest_with`-Zeile aus Schritt 14d, sodass die Schichten jede Route unter `/api` umschließen. Hier stapelst du Request-IDs, Access-Logging, Rate-Limiting, CORS und Security-Header in einer Kette. Jedes `.method(...)` fügt eine Schicht hinzu; die Reihenfolge der Aufrufe bestimmt die Reihenfolge im Stack. Siehe den [Middleware-Leitfaden](middleware.md) für den vollständigen Schichtenkatalog und die Reihenfolgeregeln.

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

Der Rest der `Cli`-Kette bleibt, wie er war. Jede `/api`-Anfrage fließt nun durch den vollständigen Middleware-Stack. `.with_welcome()` bewirkt hier nichts: Das Gerüst routet `/` bereits, also wird die Willkommensseite mit einer Warnung übersprungen.

---

## Schritt 16: Tests schreiben

**Rustango** enthält einen Testclient, der deinen Router in-process ansteuert, sodass du reale HTTP-Antworten prüfen kannst, ohne einen Server zu starten und ohne das Netzwerk zu berühren. Erstelle das Gerüst einer Testdatei:

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

Die `make:*`-Generatoren nehmen einen PascalCase-Namen; `PostSmoke` wird zur snake_case-Datei `tests/post_smoke.rs`.

Bearbeite `tests/post_smoke.rs`. Integrationstests leben in einer separaten Crate, daher bauen sie den zu testenden Router selbst, mit demselben `urls::rest_api`, das `main.rs` in Schritt 14d einhängt:

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

Zwei Dinge in diesem Ausschnitt macht man leicht falsch:

- **`dotenvy::dotenv()`** — lässt du es weg, scheitern *beide* Tests, noch bevor eine einzige Anfrage gestellt wird.
- **`content`, nicht `body`** — der Serializer akzeptiert hier beides, da `source = "body"` den Modellnamen bei der Eingabe weiterhin funktionieren lässt, aber `content` ist der Name, den deine API tatsächlich veröffentlicht.

Der zweite Test beweist, dass der Wächter aus Schritt 14 greift. Um einen angemeldeten Schreibzugriff zu testen, registriere dich zuerst über denselben Client, melde dich an und sende das Token in einem `authorization`-Header. Das [`tests/auth_flow.rs`](https://github.com/ujeenet/rustango/blob/develop/crates/rustango/examples/getting_started_blog/tests/auth_flow.rs) des Beispielprojekts tut das für jeden Fall aus Schritt 14: Registrierung, doppelter Name, Login, falsches Passwort, unbekannter Benutzer, fehlende, veränderte und abgelaufene Tokens und der Autor aus dem Token.

`Pool` ist der Multi-Backend-Pool, und `router` nimmt den Pool jedes Backends, sodass diese Datei auf PostgreSQL, MySQL und SQLite unverändert kompiliert.

> **Achtung:** Integrationstests in `tests/` können `use myblog::…` nur für Module, die in `src/lib.rs` als `pub mod` deklariert sind. Ein frisches Gerüst hat dieses Library-Target; stell sicher, dass `pub mod post_view_set;` und `pub mod urls;` dort stehen. Ein Projekt aus einem älteren `cargo-rustango` ist eventuell reines Binary: Füge eine `src/lib.rs` mit den benötigten `pub mod …;`-Zeilen hinzu, oder baue den Router inline im Test, so wie `make:test` sein `app()` scaffoldet.

Führe die Tests aus:

```bash
cargo test --test post_smoke
```

---

## Schritt 17: Den Systemcheck ausführen

Bevor du deployst, führe den integrierten Prüfer aus. Er meldet gängige Fehlkonfigurationen (wie ein schwaches `RUSTANGO_SESSION_SECRET` oder eine nicht erreichbare Datenbank), bevor sie in Produktion auffallen.

```bash
cargo run -- check --deploy
```

`--deploy` wertet ein fehlendes `RUSTANGO_SESSION_SECRET` als Fehler, also setze zuerst eines (Schritt 3), auch lokal. In deiner lokalen Dev-Umgebung siehst du dann etwa Folgendes:

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

(Die genauen Modell-/Migrationszahlen und zusätzliche Hinweise hängen von deinem Projekt und seinen Features ab.) Diese zwei Warnungen sind die erwarteten Dev-Umgebungs-Warnungen. In einem Produktions-Setup — `RUSTANGO_ENV=prod` und eine managed-Database-`DATABASE_URL` — verschwinden sie und du siehst `all checks passed`. Behebe verbleibende Warnungen oder Fehler, bevor du in die Produktion pushst.

---

## Schritt 18: In die Produktion deployen

Wie du deployst, hängt von deiner Plattform ab (Fly, Railway, Kubernetes, blankes ECS usw.). Die framework-seitigen Schritte sind überall gleich; das `--release`-Flag baut ein optimiertes Binary:

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

> **Prüfe zuerst deine Schreibrouten.** Ist ein ViewSet oder eine View noch `allow_anonymous`, ohne Auth-Schicht davor? Dann kann jeder im Internet diese Daten ändern. Mach es `read_only` oder schütze es wie in [Schritt 14e](#14e-post-schreibzugriffe-schützen).

Stelle sicher, dass dein Reverse-Proxy:
- HTTPS terminiert
- `X-Forwarded-For` weiterleitet und die App `RealIpLayer::trust_proxies([...])` mit diesem Proxy mountet (`Cli::with_trusted_proxies([...])?` oder `server::Builder::real_ip`), für akkurate IPs im `AccessLogLayer` und in den Throttles (siehe [security.md](security.md))
- den originalen `Host`-Header unverändert weitergibt: Tenant-Routing, Apex-Erkennung und `AllowedHostsLayer` lesen nur `Host`
- `X-Forwarded-Proto` weiterleitet (und `X-Forwarded-Host`, das nur die SSO-Redirect-URL hinter einem vertrauenswürdigen Proxy setzt)

`Cli::run()` befüllt `ConnectInfo` für Rate-Limiting und IP-Filterung bereits. Wenn du den Router selbst servierst, verwende `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`.

---

## Wie es weitergeht

| Thema | Doku |
|---|---|
| Lauffähige Version dieses Leitfadens | [`examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog) |
| Jeder `manage`-Unterbefehl | [`docs/manage.md`](manage.md) |
| ORM-Kochbuch (fortgeschrittene Filter, Aggregationen, M2M, Soft-Delete) | [`docs/orm.md`](orm.md) |
| Middleware (der vollständige Schichtenkatalog + Reihenfolge) | [`docs/middleware.md`](middleware.md) |
| Performance-Benchmarks (vs. Go) | [`docs/benchmarks.md`](benchmarks.md) |
| API-Konventionen (Benennung, Builder-Muster, Feature-Gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Security-Features im Detail | [`docs/security.md`](security.md) |
| Multi-Tenancy | [README — Abschnitt Multi-tenancy](https://github.com/ujeenet/rustango/blob/develop/README.md#multi-tenancy) |
| API-Doku | <https://docs.rs/rustango> |

Wenn du auf etwas stößt, das nicht funktioniert oder unklar ist, öffne ein Issue.
