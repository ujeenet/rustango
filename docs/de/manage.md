# `manage`-CLI-Referenz

Dies ist **Rustango**s Kommandozeilenwerkzeug, wie Djangos `manage.py`, Laravels
`artisan` oder Rails' `rails`-Befehl. In einem via `cargo rustango new`
generierten Projekt führt ein einziges Binary jeden Befehl aus („Verb"):

```bash
cargo run                          # runserver (no args = boot the HTTP server)
cargo run -- migrate               # any other verb
cargo run -- --help                # full subcommand list
```

[![One binary runs every manage verb — server, migrations, scaffolders, database utilities, and system commands — like Django's manage.py or Laravel's artisan](../img/manage.png)](../img/manage.png)

> **Quelle:** `rustango::manage` (`Cli`, der Verb-Dispatcher) — hinter dem
> `manage`-Feature (standardmäßig aktiviert).
>
> **Ausführbare Version:** jedes Verb hier läuft in einem generierten Projekt;
> das Beispiel
> [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog)
> wird von `cargo run -- migrate` und Konsorten angetrieben.

> **Neu bei einem Begriff hier?** *scaffold*, *migration*, *tenant* — siehe das
> [Glossar](glossary.md).

Der Befehls-Router liegt in [`rustango::manage::Cli`](https://docs.rs/rustango/latest/rustango/manage/struct.Cli.html);
Ihre `src/main.rs` verdrahtet ihn so:

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

Multi-Tenant-Projekte fügen der Kette `.tenancy()` hinzu. Das schaltet den
Router auf [`rustango::tenancy::manage`](https://docs.rs/rustango/latest/rustango/tenancy/manage/index.html)
um und schaltet die Multi-Tenant-Befehle frei.

> **Ältere Form** — Projekte, die von `manage startapp --with-manage-bin`
> (oder vor v0.16) generiert wurden, liefern weiterhin `src/bin/manage.rs`.
> Diese verwenden `cargo run --bin manage -- <verb>`. Beide Formen akzeptieren
> dieselben Verben.

Jeder Befehl gibt auf stdout aus und beendet sich bei Validierungs- oder
E/A-Fehlern mit einem Exit-Code ungleich null. Führen Sie
`cargo run -- --help` (oder `<verb> --help`) für die eingebaute
Nutzungshilfe aus.

---

## Inhaltsverzeichnis

- [Migrationen](#migrations)
- [Datenmigrationen](#data-migrations)
- [Projekt-/App-Scaffolder](#project--app-scaffolders)
- [Dateigeneratoren (`make:*`)](#file-generators-make)
- [Datenbank-Werkzeuge](#database-utilities)
- [Systembefehle](#system-commands)
- [Tenancy-Befehle](#tenancy-commands)
- [Eigene Unterbefehle](#custom-subcommands)
- [Häufige Arbeitsabläufe](#common-workflows)

---

## Migrationen

### `makemigrations [name]`

Generiert eine Migrationsdatei aus Änderungen an Ihren Modellen — wie Djangos
`makemigrations`. Es vergleicht Ihre registrierten Modelle mit dem letzten
gespeicherten Schema-Snapshot in `migrations/` und schreibt eine neue
JSON-Datei mit allen Änderungen.

```bash
cargo run -- makemigrations                          # auto-name (e.g. 0004_add_slug_to_posts)
cargo run -- makemigrations rename_status_to_state   # custom suffix
```

**Automatisch erkannte Änderungen:**
- `CreateTable` / `DropTable`
- `AddColumn` / `DropColumn`
- `AlterColumnType` / `AlterColumnNullable` / `AlterColumnDefault` / `AlterColumnMaxLength`
- `AlterColumnUnique`
- `CreateIndex` / `DropIndex`
- `AddCheckConstraint` / `DropCheckConstraint`
- `CreateM2MTable` / `DropM2MTable`

**NICHT automatisch erkannt** (Umbenennen vs. Löschen+Hinzufügen ist mehrdeutig):
- `RenameTable`, `RenameColumn` — verwenden Sie `--empty` und bearbeiten Sie das JSON.

### `makemigrations --app <app>`

Beschränkt die Migration auf eine einzelne App. Sie schreibt in das eigene
Verzeichnis `<project_root>/<app>/migrations/` dieser App und betrachtet nur
Modelle, die zu dieser App gehören.

```bash
cargo run -- makemigrations --app blog
cargo run -- makemigrations --app blog backfill_slugs
```

### `makemigrations --scope <registry|tenant>`

Nur Multi-Tenant. Schreibt eine einzige Migration für nur die Modelle in einem
Scope — jene, deren `#[rustango(scope = "...")]`-Attribut übereinstimmt.
(„Registry"-Tabellen werden von allen Tenants gemeinsam genutzt;
„Tenant"-Tabellen leben pro Tenant.) Ohne dieses Flag teilt ein einfaches
`makemigrations` in einem Tenancy-Projekt die Änderungen automatisch in ZWEI
Dateien auf — eine für Registry-Modelle, eine für Tenant-Modelle — damit
gemeinsame Framework-Tabellen (`Org`, `Operator`) nicht in die
Per-Tenant-Migrationen durchsickern, die `migrate-tenants` ausführt.

```bash
cargo run -- makemigrations                       # tenancy: writes 0NN_<auto>.json (registry) + 0MM_<auto>.json (tenant) as needed
cargo run -- makemigrations --scope tenant        # explicit single-scope diff
cargo run -- makemigrations --scope registry      # explicit single-scope diff
```

Warum die Aufteilung wichtig ist: vor v0.24.2 bündelte ein einfaches
`makemigrations` in einem Tenancy-Projekt Operationen auf
`rustango_operators` (einer Registry-Tabelle) in eine Tenant-Migration. Als
`migrate-tenants` diese Datei ausführte, wurde `rustango_operators` über den
`search_path` auf die Registry-Kopie aufgelöst und kollidierte mit dem dort
bereits vorhandenen Constraint.

### `makemigrations --empty <name>`

Erstellt eine leere Migration (keine `forward`-Operationen), die Sie von Hand
ausfüllen — wie Djangos `makemigrations --empty`. Verwenden Sie sie, wenn Sie
Datenoperationen oder Umbenennungsoperationen schreiben müssen, die der
Auto-Detektor nicht generieren kann. Bearbeiten Sie das resultierende JSON
selbst.

```bash
cargo run -- makemigrations --empty rename_status_to_state
# Then edit migrations/0005_rename_status_to_state.json:
#   "forward": [
#     {"schema": {"RenameColumn": {"table": "posts", "old_column": "status", "new_column": "state"}}}
#   ]
```

### `makemigrations --merge`

Repariert eine Migrationshistorie, die sich in zwei Zweige aufgeteilt hat —
dieselbe Idee wie Djangos `makemigrations --merge` (Issue #346). Das passiert,
wenn zwei Personen jeweils `makemigrations` auf ihrem eigenen Feature-Branch
ausführen, sodass beide neuen Dateien auf denselben Elternknoten zeigen.
Nachdem beide Branches gemergt sind, hat die Historie zwei „Blätter"
(Endpunkte), und das nächste `makemigrations` würde willkürlich eines davon
als Elternknoten wählen.

`--merge` erkennt dies und schreibt eine leere `NNNN_merge.json`, deren
Elternknoten auf das alphabetisch letzte Blatt zeigt und die Historie wieder
zu einer einzigen Kette vereint. Ihr Schema-Snapshot spiegelt den kombinierten
Zustand wider, gelesen aus der lebendigen Modell-Registry — die Modelle beider
Branches sind zu diesem Zeitpunkt einkompiliert, sodass der Snapshot genau ist.

```bash
cargo run -- makemigrations --merge
# wrote migrations/0004_merge.json
#     merge node — empty `forward`, anchors the chain after divergent leaves
```

- **Bereits eine einzelne Kette** → gibt `no merge needed` aus und beendet sich
  sauber. Sicher auf einer gesunden Historie auszuführen.
- **Wirklich getrennte Historien** (keine Branch-Kollision) → bricht mit einem
  Fehler ab, statt einen Elternknoten zu erfinden. Dieselbe Absicherung, die
  Django verwendet.
- **Nicht kombinierbar** mit `--empty`, `--app`, `--scope` oder einem
  positionalen Namen.

### `migrate`

Wendet alle ausstehenden Migrationen der Reihe nach auf die Datenbank an — wie
Djangos `migrate` oder Laravels `php artisan migrate`. Dies ist der Befehl, den
Sie nach `makemigrations` ausführen, um Ihr Schema tatsächlich zu ändern.

```bash
cargo run -- migrate
cargo run -- migrate --dry-run                       # print SQL without writing
```

Jede Datei läuft standardmäßig innerhalb einer Transaktion, sodass ein Fehler
die gesamte Datei zurückrollt. Setzen Sie `"atomic": false` im JSON, um sich
abzumelden — das brauchen Sie für Anweisungen wie
`CREATE INDEX CONCURRENTLY`, die nicht in einer Transaktion laufen können.

Im **Tenancy-Modus** (`Cli::tenancy()`) ist `migrate` scope-bewusst: es wendet
zuerst Registry-Migrationen auf die gemeinsame Registry-Datenbank an, dann
Tenant-Migrationen über jeden aktiven Tenant. Für feinere Kontrolle verwenden
Sie [`migrate-registry`](#migrate-registry) /
[`migrate-tenants`](#migrate-tenants).

### `migrate <target>`

Migriert zu einem bestimmten Punkt in der Historie, vorwärts oder rückwärts —
wie Djangos `migrate <app> <name>`. Nennen Sie eine Migration, um zu ihr zu
wechseln; das spezielle Ziel `zero` macht alles rückgängig.

```bash
cargo run -- migrate 0003_add_slug      # forward to 0003
cargo run -- migrate 0001_initial       # roll back to 0001 (unapply 0002+)
cargo run -- migrate zero               # unapply EVERY migration
```

### `migrate --squash`

Fasst jede **ausstehende** (nicht angewendete) Migration in einem frisch
generierten Diff zusammen — der Dev-Iterations-Notausgang, wenn ein Stapel
halbfertiger Migrationen leichter neu zu generieren als zu reparieren ist. Es
weigert sich, an bereits Angewendetem etwas zu ändern.

```bash
cargo run -- migrate --squash
```

Die neu generierte Datei zeichnet die Namen, die sie zusammengefasst hat, in
ihrer `replaces`-Liste auf. Das ist in dem Moment wichtig, in dem eine andere
Datenbank beteiligt ist: der Checkout Ihres Kollegen, Staging oder CI hat
möglicherweise bereits einige der Dateien angewendet, die Sie gerade gelöscht
haben. Ohne `replaces` würde das `CREATE TABLE` der neuen Datei dort
kollidieren; mit ihr **versöhnt** der Runner stattdessen (siehe unten).

### Squash-Versöhnung

Ein Squash stellt den Endzustand der Migrationen wieder her, die er ersetzt,
sodass das, was der Runner tun sollte, vollständig davon abhängt, was die
Zieldatenbank bereits enthält. Er entscheidet automatisch:

| Datenbankzustand | was passiert |
|---|---|
| frisch — keine Historie, keine Tabellen | der Squash läuft echt |
| jede ersetzte Migration steht im Ledger | verzeichnet, Vorgänger als erledigt markiert, **kein DDL** |
| Tabellen existieren, aber das Ledger hat keine Historie | verzeichnet, **kein DDL** (Djangos `--fake-initial`) |
| nur *einige* ersetzte Zeilen / Tabellen vorhanden | **verweigert**, benennt, was fehlt |

Der partielle Fall ist bewusst ein harter Fehler: keine automatische Wahl ist
dort sicher, also stoppt der Runner und sagt Ihnen, was er gefunden hat, statt
zu raten. Lösen Sie ihn mit `migrate --fake` (unten).

Migrationen, die von einem angewendeten Squash abgelöst wurden, werden als
angewendet behandelt, sodass Sie die alten Dateien für ein oder zwei Releases
auf der Platte lassen können — Deployments, die sie nie ausgeführt haben,
migrieren trotzdem korrekt vorwärts.

Gewöhnliche (Nicht-Squash-)Migrationen bleiben unberührt: eine einfache
Migration, deren Tabelle bereits existiert, scheitert weiterhin lautstark,
denn das ist ein echter Konflikt und keine bekannt-äquivalente Historie.

### `migrate --fake <name>`

Stempelt eine Migration als angewendet **ohne ihr SQL auszuführen** — der
Betreiber-Notausgang, wenn die Datenbank bereits im Zielzustand ist, das
Ledger es aber nicht weiß (eine außerhalb des Kanals eingerichtete DB, eine
gelöschte Ledger-Tabelle, eine teilweise gelungene Migration, ein verweigerter
partieller Squash). Wiederholen Sie das Flag, um mehrere Zeilen auf einmal zu
reparieren.

```bash
cargo run -- migrate --fake 0004_add_indexes
cargo run -- migrate --fake 0004_add_indexes --system        # framework's own chain
cargo run -- migrate --fake 0004_add_indexes --all-tenants   # every active tenant
```

Der Name wird zuerst gegen das Migrationsverzeichnis validiert, sodass ein
Tippfehler keine unechte Zeile einschleusen kann. Das Stempeln ist idempotent.

`--system` zielt auf die eigene Migrationskette des Frameworks
(`system/migrations/`, verzeichnet in `__rustango_system_migrations__`) statt
auf die Ihres Projekts. `--all-tenants` fächert den Stempel über jeden aktiven
Tenant auf, berichtet über jeden einzelnen und fährt über Fehler hinweg fort —
die Tabellen des Frameworks leben pro Tenant, ihre Reparatur ist also eine
Per-Tenant-Aufgabe.

### `downgrade [N]`

Rollt die letzten N angewendeten Migrationen zurück (Standard 1) — Laravels
`migrate:rollback`. Jede Migration muss reversibel sein: Schemaänderungen
kehren sich automatisch um, aber Datenoperationen brauchen ein definiertes
`reverse_sql`, sonst scheitert der Rollback.

```bash
cargo run -- downgrade                  # one step
cargo run -- downgrade 3                # three steps
```

### `showmigrations` / `status`

Listet jede Migration und ob sie angewendet wurde — wie Djangos
`showmigrations`. `[X]` bedeutet angewendet, `[ ]` bedeutet noch ausstehend.

```bash
cargo run -- showmigrations
cargo run -- status                     # alias
```

Ausgabe:

```
[X] 0001_initial
[X] 0002_add_status
[ ] 0003_add_slug
```

---

## Datenmigrationen

### `add-data-op`

Fügt einer Migration einen Roh-SQL-Datenschritt hinzu, ohne das JSON von Hand
zu bearbeiten. Greifen Sie dazu, wenn Sie vorhandene Zeilen transformieren
müssen — eine Spalte nachfüllen, Daten bereinigen — als Teil einer Migration.
Es ist das Äquivalent zu Djangos `RunSQL`-Datenmigration, für Sie von der
Kommandozeile aus generiert.

```bash
# New migration with up + down
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

# Append to an existing migration
cargo run -- add-data-op \
    --to 0003_add_slug \
    --sql "UPDATE posts SET slug = id::text"

# Irreversible (no rollback)
cargo run -- add-data-op \
    --sql "DELETE FROM legacy_data" \
    --name purge_legacy
```

| Flag | Erforderlich | Beschreibung |
|---|:-:|---|
| `--sql <SQL>` | ja | Vorwärts-SQL, das bei `migrate` läuft |
| `--reverse-sql <SQL>` | nein | Rollback-SQL bei `unapply`; weglassen für irreversibel |
| `--name <name>` | nein | Namenssuffix der neuen Migration; Standard ist `data_op` |
| `--to <migration>` | nein | An eine bestehende Migration anhängen, statt eine neue zu erstellen |

Lassen Sie `--reverse-sql` weg, und der Schritt wird als `reversible: false`
markiert — jeder Versuch, ihn zurückzurollen, scheitert sofort.

---

## Projekt-/App-Scaffolder

### `cargo rustango new <name>` *(separates Binary)*

Erstellt ein brandneues **Rustango**-Projekt — wie `django-admin startproject`
oder `laravel new`. Dies ist ein separates Werkzeug, installieren Sie es also
zuerst mit `cargo install cargo-rustango`. Wählen Sie aus drei Vorlagen:

```bash
cargo rustango new myblog                          # default = fullstack (ORM + admin)
cargo rustango new myapi --template api            # JSON-only, no admin
cargo rustango new shop --template tenant          # multi-tenancy
```

Schreibt:

```
<name>/
  Cargo.toml
  .env.example
  .gitignore
  rust-toolchain.toml
  docker-compose.yml
  README.md
  migrations/                               (your app's migrations)
  system/migrations/                        (tenant template — framework tables, generated)
  src/{main,models,views,urls}.rs
```

Die Tenant-Vorlage liefert einen **leeren** `system/migrations/`-Ordner. Die
eigenen Tabellen des Frameworks (`rustango_orgs`, `rustango_users`,
Rollen/Berechtigungen, …) werden beim ersten `cargo run -- migrate` aus den
kompilierten Modellen dorthin generiert — es gibt kein handgeliefertes
Bootstrap-JSON. Siehe [`migrate`](#migrate) /
[`migrate-registry`](#migrate-registry).

### `startapp <name> [flags]`

Erstellt eine neue App (ein Feature-Modul) unter `src/<name>/` — genau wie
Djangos `startapp`. Verwenden Sie es, um Modelle, Views und URLs für einen Teil
Ihres Projekts gruppiert zu halten.

```bash
cargo run -- startapp blog
cargo run -- startapp shop --with-manage-bin             # also writes src/bin/manage.rs
cargo run -- startapp shop --into apps                   # write under src/apps/shop/ instead
```

Erstellt:

```
src/<name>/
  mod.rs
  models.rs
  views.rs
  urls.rs
```

Sicher erneut ausführbar — bestehende Dateien bleiben unberührt. Ein manueller
Schritt: fügen Sie `pub mod <name>;` zu `src/lib.rs` hinzu, damit Rust das neue
Modul kompiliert.

---

## Dateigeneratoren (`make:*`)

Diese erstellen Startdateien für gängige Bausteine — sehr ähnlich zu Laravels
`make:*`-Befehlen (`make:controller`, `make:model`, …). Jeder Generator
schreibt nach `src/<snake_name>.rs` (oder `tests/<snake_name>.rs` für
`make:test`) und:

- Prüft, ob der Name gültig ist (PascalCase, Buchstaben/Ziffern/Unterstrich).
- Wandelt ihn für den Dateinamen in snake_case um (`PostViewSet` →
  `post_view_set.rs`).
- Überschreibt keine bestehende Datei.
- Erinnert Sie daran, `pub mod X;` zu Ihrer `lib.rs` hinzuzufügen.

### `make:viewset <Name> [--model <Model>]`

Generiert eine `#[derive(ViewSet)]`-Struktur — einen REST-Endpunkt für ein
Modell, wie ein Django-REST-Framework-ViewSet. Die Feldlisten kommen für Sie
vorgestubbt zum Ausfüllen.

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Generierte `src/post_view_set.rs`:

```rust
#[derive(ViewSet)]
#[viewset(model = Post, fields = "id, ", filter_fields = "", search_fields = "", page_size = 20)]
pub struct PostViewSet;
```

Einbinden mit: `.merge(PostViewSet::router("/api/posts", pool.clone()))`.

### `make:serializer <Name> [--model <Model>]`

Generiert eine `#[derive(Serializer)]`-Struktur — steuert, wie ein Modell nach
und von JSON konvertiert wird (wie ein DRF-Serializer).

```bash
cargo run -- make:serializer PostSerializer --model Post
```

### `make:form <Name>`

Generiert eine `#[derive(Form)]`-Struktur zum Validieren und Verarbeiten von
Formular-Eingaben — wie ein Django-`Form`.

```bash
cargo run -- make:form ContactForm
```

### `make:job <Name>`

Generiert ein Hintergrund-Job-Gerüst (Arbeit, die außerhalb des Requests läuft,
wie eine Celery-Task oder ein Laravel-Job), mit einem auskommentierten Beispiel,
wie man es einplant.

```bash
cargo run -- make:job EmailDigestJob
```

### `make:notification <Name>`

Generiert eine Notification-Struktur, die eine E-Mail aufbaut — wie Laravels
`make:notification`.

```bash
cargo run -- make:notification WelcomeEmail
```

### `make:middleware <Name>`

Generiert eine Middleware-Funktion — Code, der vor und nach jedem Request läuft
(Auth-Prüfungen, Logging und so weiter). „axum" ist das Web-Framework, auf dem
**Rustango** aufbaut, sodass der Stub der Middleware-Form von axum entspricht.

```bash
cargo run -- make:middleware AuditLog
```

### `make:test <Name>`

Generiert einen Integrationstest in `tests/`, der `TestClient` verwendet, um
Requests gegen Ihre App zu stellen.

```bash
cargo run -- make:test post_smoke
```

---

## Datenbank-Werkzeuge

### `db:info`

Zeigt, mit welcher Datenbank dieser Build zu sprechen konfiguriert ist, ohne
sich zu verbinden. Es gibt die Framework-Version aus, welche
Datenbanktreiber (`postgres`/`mysql` Cargo-Features) einkompiliert sind, die
Verbindungs-URL mit verstecktem Passwort und das erkannte Backend. Da es nie
eine Verbindung öffnet, ist es praktisch in CI oder Containern, in denen die
Datenbank noch nicht läuft, Sie aber bestätigen wollen, dass die Einstellungen
stimmen.

```bash
cargo run -- db:info
```

### `db:dump [--out <path>] [--data-only|--schema-only] [--no-owner]`

Sichert Ihre Datenbank, indem `pg_dump` gegen `DATABASE_URL` ausgeführt wird —
wie `php artisan db:dump`. Standardmäßig geht das SQL nach stdout (damit Sie es
pipen können); übergeben Sie `--out <path>` (`-o`), um stattdessen eine Datei
zu schreiben. `--data-only` und `--schema-only` bilden direkt auf die Flags von
`pg_dump` ab, und `--no-owner` lässt die OWNER-Zeilen weg. Sie brauchen
`pg_dump` installiert und in Ihrem `PATH`.

```bash
cargo run -- db:dump > backups/before-migrate.sql    # stdout → file
cargo run -- db:dump --out backups/before-migrate.sql
```

### `db:restore <path> [--clean]`

Lädt eine Dump-Datei zurück in Ihre Datenbank — das Gegenstück zu `db:dump`. Es
lässt die Datei durch `psql` gegen `DATABASE_URL` mit `ON_ERROR_STOP=1` laufen,
sodass es beim ersten Fehler stoppt. Fügen Sie `--clean` hinzu, um zuerst das
bestehende Schema zu löschen (es stellt
`DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;` voran), damit die
Wiederherstellung auf einer leeren Datenbank landet. Sie brauchen `psql` in
Ihrem `PATH`.

```bash
cargo run -- db:restore backups/before-migrate.sql
cargo run -- db:restore backups/before-migrate.sql --clean
```

---

## Systembefehle

### `version` / `--version`

Gibt die Version des **Rustango**-Frameworks aus.

```bash
$ cargo run -- version
rustango 0.44.0
```

### `about`

Gibt eine Momentaufnahme Ihrer Umgebung aus: Framework-Version, registrierte
Modelle und Apps, ob die Datenbank erreichbar ist, und wichtige
Umgebungsvariablen. Legen Sie dies in Support-Tickets, wenn etwas nicht stimmt.

```bash
$ cargo run -- about
rustango
  version:        0.44.0
  models:         3 registered
  apps:           1 (blog)
  RUSTANGO_ENV:   local
  DATABASE_URL:   postgres://***@localhost:5433/myblog
  db_connect:     ok
```

### `check [--deploy]`

Führt Gesundheitsprüfungen an Ihrem Projekt durch — wie Djangos `check`. Fügen
Sie `--deploy` für die strengeren Produktionsreife-Prüfungen hinzu, genau wie
Djangos `check --deploy` funktioniert.

**Immer aktive Prüfungen:**
- ≥ 1 Modell via `inventory` registriert
- DB erreichbar (`SELECT 1`)
- Migrationsanzahl vs. Modellanzahl

**Mit `--deploy`:**
- `RUSTANGO_ENV` ist `prod` oder `production`
- `RUSTANGO_SESSION_SECRET` gesetzt und ≥ 32 Byte (der HMAC-Schlüssel für
  Cookies + JWTs; `SECRET_KEY` wird vom Framework nie gelesen)
- `DATABASE_URL` gesetzt
- `RUSTANGO_APEX_DOMAIN` gesetzt (Tenancy-Projekte)

```bash
$ cargo run -- check --deploy
running rustango system check (deploy mode)...
  [info]    3 models registered via inventory
  [info]    database reachable
  [info]    4 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET length OK
all checks passed
```

Beendet sich mit Exit-Code ungleich null, wenn eine Prüfung auf Fehler-Ebene
scheitert. Warnungen allein verursachen kein Scheitern.

### `docs`

Öffnet die **Rustango**-Dokumentation (<https://docs.rs/rustango>) in Ihrem
Browser. Es gibt immer auch die URL aus, sodass es auch auf einem Headless-Server
funktioniert.

```bash
cargo run -- docs
```

### `--help` / `help`

Listet jeden Befehl mit einer einzeiligen Beschreibung. Im Tenancy-Modus werden
auch die unten aufgeführten Multi-Tenant-Befehle hinzugefügt.

---

## Tenancy-Befehle

Diese Befehle existieren nur in Multi-Tenant-Projekten (eine App, die viele
isolierte Kunden/Orgs bedient). Sie erscheinen nur, wenn das Projekt mit
`features = ["tenancy"]` gebaut wird UND `Cli::new()` mit `.tenancy()`
verkettet ist.

### `init-tenancy`

**No-op — aus Kompatibilitätsgründen beibehalten.** Das Framework liefert keine
handgebauten Bootstrap-Migrationen mehr. Seine eigenen Tabellen
(`rustango_orgs`, `rustango_operators`, `rustango_users`, Rollen/Berechtigungen,
…) werden aus den kompilierten Modellen in `system/migrations/` generiert — der
normale Django-Fluss (Modelle → `makemigrations` → `migrate`) — und von
[`migrate`](#migrate) / [`migrate-registry`](#migrate-registry) angewendet, die
sie bei Bedarf generieren, falls die Dateien fehlen.

```bash
cargo run -- init-tenancy   # does nothing now; kept so old scripts don't break
```

Ältere Versionen schrieben hier `0001_rustango_*_initial.json`; dieser
hartkodierte Fluss ist verschwunden. **Zum Bereitstellen führen Sie einfach
`cargo run -- migrate` aus.** Ein eigenes Benutzermodell
(`.user_model::<AppUser>()`) fließt durch dieselben generierten
`system/migrations/` — siehe
[Eigenes Benutzermodell](#custom-user-model-extra-columns-on-rustango_users).

### `migrate-registry`

Wendet nur die Registry-Migrationen an — die gemeinsamen, tenant-übergreifenden
Tabellen. Die Registry hält `rustango_orgs` und `rustango_operators` sowie alle
registry-scoped Tabellen, die Sie definieren. Tenant-Tabellen bleiben unberührt.

```bash
cargo run -- migrate-registry
```

### `migrate-tenants`

Wendet Tenant-Migrationen auf jeden aktiven Tenant nacheinander an. Jeder Tenant
verwendet seine eigene Verbindung (sein eigenes Schema oder seine eigene
Datenbank), und wenn ein Tenant scheitert, laufen die übrigen trotzdem weiter —
der Befehl berichtet am Ende das Ergebnis pro Tenant.

```bash
cargo run -- migrate-tenants
```

Für den häufigen Fall macht ein einfaches `migrate` bereits zuerst die Registry,
dann die Tenants — greifen Sie zu `migrate-tenants` nur, wenn Sie diesen Schritt
für sich allein brauchen.

### `runserver` / `run-server`

Startet den Multi-Tenant-Webserver — Djangos `runserver`. In einem
Tenancy-Projekt ist dies dasselbe wie ein nacktes `cargo run`; die benannte Form
existiert, damit eigene Binaries, die ihre eigenen Argumente parsen, es trotzdem
auslösen können.

```bash
cargo run                        # implicit
cargo run -- runserver           # explicit
```

### `create-tenant <slug> [options]`

Richtet einen neuen Tenant (Kunde/Org) ein und wendet die Tenant-Migrationen
darauf an. Der `<slug>` ist sein kurzer Bezeichner. Sicher erneut ausführbar —
ein erneuter Aufruf auf einem bestehenden Tenant dupliziert nichts.

```bash
cargo run -- create-tenant acme --display-name "ACME Corp"
cargo run -- create-tenant beta --mode database --database-url postgres://...
```

| Flag | Beschreibung |
|---|---|
| `--display-name <name>` | Menschenlesbares Label, das in Admin-Seitenleisten angezeigt wird |
| `--mode schema \| database` | Speichermodus (Standard: schema) |
| `--database-url <url>` | Tenant-spezifische DB-URL (erforderlich für den database-Modus) |
| `--host-pattern <pattern>` | Überschreibt das vom `SubdomainResolver` verwendete Host-Muster |
| `--no-migrate` | Überspringt das Anwenden tenant-scoped Migrationen nach dem Provisioning |

### `drop-tenant <slug> [--confirm <slug>]`

Deaktiviert einen Tenant, indem `active = false` gesetzt wird. Dies ist die
weiche, umkehrbare Option — die Daten des Tenants bleiben auf der Platte, und
ein erneutes Ausführen von `create-tenant` bringt ihn zurück. Wenn Sie nicht
interaktiv laufen (kein Terminal angehängt), müssen Sie `--confirm <slug>` mit
dem erneut getippten Slug zur Bestätigung übergeben.

```bash
cargo run -- drop-tenant acme --confirm acme
```

### `purge-tenant <slug> [--confirm <slug>] [--purge-database]`

**Löscht einen Tenant dauerhaft.** Es löscht das Schema des Tenants und entfernt
seine Zeile aus `rustango_orgs`, ohne Rückgängig-Machen. Wenn Sie nicht
interaktiv laufen (kein Terminal angehängt), müssen Sie `--confirm <slug>` mit
dem erneut getippten Slug übergeben. Bei Tenants im database-Modus bleibt die
zugrunde liegende Datenbank an Ort und Stelle, es sei denn, Sie übergeben
zusätzlich `--purge-database`.

```bash
cargo run -- purge-tenant acme --confirm acme
cargo run -- purge-tenant beta --confirm beta --purge-database   # database-mode: also DROP DATABASE
```

### `list-tenants`

Listet jeden Tenant mit seinem Speichermodus und aktiv/inaktiv-Status.

```bash
cargo run -- list-tenants
```

### `create-operator <username> --password <pwd>`

Erstellt einen Operator — einen globalen Admin, der jeden Tenant von einer
tenant-übergreifenden Konsole aus verwalten kann. Operatoren leben in der
gemeinsamen Registry, nicht innerhalb eines einzelnen Tenants.

```bash
cargo run -- create-operator admin --password letmein
```

### `create-user <tenant> <username> --password <pwd> [--superuser]`

Erstellt einen Benutzer innerhalb eines Tenants — ungefähr Djangos
`createsuperuser`, aber auf einen einzelnen Tenant beschränkt.

```bash
cargo run -- create-user acme alice --password hunter2 --superuser
```

`--superuser` setzt `is_superuser = true` für diesen Benutzer innerhalb des
Tenants. Das macht ihn zu einem Admin des Tenants (voller Schreibzugriff im
Tenant-Admin), gewährt aber nie Zugang zur tenant-übergreifenden
Operator-Konsole.

### `create-role <tenant> <name>`

Erstellt eine Rolle (ein benanntes Bündel von Berechtigungen, wie eine
Django-Gruppe) innerhalb eines Tenants.

```bash
cargo run -- create-role acme editor
```

### `list-roles <tenant>`

Listet die in einem bestimmten Tenant definierten Rollen.

```bash
cargo run -- list-roles acme
```

### `assign-role <tenant> <username> <role>`

Gibt einem Benutzer eine der Rollen des Tenants.

```bash
cargo run -- assign-role acme alice editor
```

### `revoke-role <tenant> <username> <role>`

Entfernt eine Rolle von einem Benutzer — die Umkehrung von `assign-role`.

```bash
cargo run -- revoke-role acme alice editor
```

### `grant-perm <tenant> <role-name|username> <codename> [--role]`

Gewährt eine einzelne Berechtigung. Standardmäßig ist das zweite Argument ein
**Benutzername**, sodass die Berechtigung direkt an diesen Benutzer geht; fügen
Sie `--role` hinzu, um sie stattdessen einer Rolle zu gewähren.
Berechtigungs-Codenames verwenden Djangos Format `<app>.<action>_<model>`
(`blog.add_post`, `blog.change_post`, …). Das Feature
`auto_create_permissions` erstellt die vier standardmäßigen CRUD-Codenames
automatisch für jedes Modell, das mit `#[rustango(permissions)]` markiert ist.

```bash
cargo run -- grant-perm acme alice blog.change_post           # grant to user alice
cargo run -- grant-perm acme editor blog.change_post --role   # grant to role editor
```

### `revoke-perm <tenant> <role-name|username> <codename> [--role]`

Entfernt eine Berechtigung — die Umkehrung von `grant-perm`. Zielt standardmäßig
auf einen Benutzer; fügen Sie `--role` hinzu, um sie stattdessen von einer Rolle
zu entziehen.

```bash
cargo run -- revoke-perm acme alice blog.change_post
cargo run -- revoke-perm acme editor blog.change_post --role
```

### `create-api-key <tenant> <username> [--label <s>]`

Stellt einen API-Schlüssel für einen Tenant-Benutzer aus. Das vollständige Token
wird **einmal** ausgegeben und nie wieder — kopieren Sie es jetzt, denn nur sein
Präfix und ein Hash werden gespeichert.

```bash
cargo run -- create-api-key acme alice --label "ci-bot"
```

### `audit-cleanup`

Beschneidet alte Einträge aus dem Audit-Log (`rustango_audit_log`), damit es
nicht ewig wächst. Kürzen Sie nach Alter (`--days`) oder nach Anzahl
(`--keep-last`), und optional auf einen Tenant beschränkt.

```bash
cargo run -- audit-cleanup --days 90                       # delete > 90 days old
cargo run -- audit-cleanup --keep-last 50                  # keep most recent 50 per row
cargo run -- audit-cleanup --keep-last 50 --tenant acme    # scoped
```

---

## Eigenes Benutzermodell (zusätzliche Spalten auf `rustango_users`)

Dies ist **Rustango**s Version von Djangos „custom user model" — wie Sie Ihre
eigenen Felder zur Benutzertabelle hinzufügen. Der eingebaute Tenant-`User` hat
sieben feste Spalten: `id`, `username`, `password_hash`, `is_superuser`,
`active`, `created_at`, plus eine `data`-JSONB-Spalte (ein flexibler JSON-Blob)
für beliebige zusätzliche Metadaten pro Benutzer. **Für die meisten Apps ist
diese JSONB-Spalte alles, was Sie brauchen** — keine Migration, kein Override,
keine Überraschungen.

Wenn Sie stattdessen **typisierte, indexierbare** Spalten auf `rustango_users`
wollen, gibt es zwei Ansätze. Sie sind nicht austauschbar; wählen Sie den, der
dazu passt, wo Ihr Projekt in seinem Leben steht.

### Option 1 — Geschwister-Profilmodell mit FK *(funktioniert bei jedem Projekt)*

Am besten, wenn das Projekt bereits existiert, oder wenn Sie die `User`-Tabelle
des Frameworks lieber als einzige Quelle der Wahrheit belassen möchten.

```rust
#[derive(rustango::Model)]
pub struct UserProfile {
    #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>,
    #[rustango(fk = "rustango_users")] pub user_id: i64,
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
```

Führen Sie `cargo run -- makemigrations` dann `cargo run -- migrate` aus, und
Sie haben eine typisierte Extra-Tabelle, die per Fremdschlüssel mit dem Benutzer
verknüpft ist. Lesen Sie sie mit dem ORM:

```rust
let profile = UserProfile::objects()
    .where_(UserProfile::user_id.eq(user.id.get().copied().unwrap()))
    .first(&pool).await?;            // Option<UserProfile>
```

Kompromiss: eine zusätzliche Zeile und ein JOIN bei jedem Zugriff. Vorteil: null
Risiko, die Framework-Auth zu brechen.

### Option 2 — `Cli::user_model::<AppUser>()` *(nur auf der grünen Wiese)*

Verwenden Sie dies nur auf einem frischen Projekt, bei dem Sie die
zusätzlichen Felder direkt auf der `rustango_users`-Tabelle selbst wollen. Da
`AppUser` das `rustango_users`-Modell *ist*, fließen seine Spalten durch die
gewöhnliche `makemigrations` → `migrate`-Engine: die Tabellen des Frameworks
werden in `system/migrations/` generiert, sodass die Spalten von `AppUser` im
generierten `CREATE TABLE rustango_users` landen.

**Schritt 1.** Definieren Sie Ihr Modell. Es muss jede vom Framework geforderte
Spalte exakt deklarieren (`id`, `username`, `password_hash`, `is_superuser`,
`active`, `created_at`, `data`), plus Ihre Extras. Jede zusätzliche Spalte muss
entweder `NULL` erlauben oder einen `default = "…"` haben.

```rust
use rustango::sql::Auto;

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "rustango_users")]
pub struct AppUser {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)] pub username: String,
    #[rustango(max_length = 255)] pub password_hash: String,
    pub is_superuser: bool,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[rustango(default = "'{}'")] pub data: serde_json::Value,
    // extras —
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
impl rustango::tenancy::TenantUserModel for AppUser {}
```

**Schritt 2.** Verdrahten Sie den Override in `main.rs`:

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::manage::Cli::new()
        .api(my_app::urls::router())
        .tenancy()
        .user_model::<AppUser>()
        .run().await
}
```

**Schritt 3.** Registrieren Sie `AppUser` **anstelle** des Framework-`User` —
nur ein Modell darf `table = "rustango_users"` beanspruchen. Der Scaffolder
liefert kein statisches Bootstrap-JSON (nur ein leeres `system/migrations/`),
es gibt also nichts zu löschen; registrieren Sie einfach nicht auch den
Framework-`User`.

**Schritt 4.** Generieren + anwenden:

```bash
cargo run -- makemigrations       # generates system/migrations/ with AppUser's columns
cargo run -- migrate              # creates rustango_users with your extras
```

**Vorbehalte:**

- `AppUser` später zu ändern ist eine normale Schemaänderung: führen Sie
  `makemigrations` erneut aus, um die `AddColumn`-Migration zu emittieren, dann
  `migrate`.
- Nur ein Modell darf auf `rustango_users` abbilden. Das Registrieren **beider**,
  des Framework-`User` und Ihres `AppUser`, macht `makemigrations` mehrdeutig —
  registrieren Sie `AppUser` allein. Das ist der Hauptgrund, warum Option 2 nur
  für frische Projekte ist; bei einem bestehenden Projekt vermeidet Option 1 das
  Problem.
- Framework-Auth- und Admin-Code liest die sieben Kernspalten namentlich; Ihre
  zusätzlichen Spalten sind nur über `AppUser::objects().fetch(...)` erreichbar.

`Builder::user_model::<AppUser>()` macht dasselbe für Code, der den
Server-`Builder` direkt baut, ohne über `Cli` zu gehen.

---

## Eigene Unterbefehle

Sie können Ihre eigenen Befehle hinzufügen — **Rustango**s Interpretation von
Djangos eigenen Management-Befehlen. Der Trick besteht darin, die Argumente
selbst zu inspizieren und Ihren Befehl zu behandeln, bevor Sie den Rest an
`Cli::run` weitergeben. Zwei Wege, es zu tun:

**Inline in `src/main.rs`** (kein zusätzliches Binary):

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("import-csv")) {
        let url = std::env::var("DATABASE_URL")?;
        let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;
        return my_csv_importer::run(&pool, &args[1..]).await;
    }
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

**Via `--with-manage-bin`** (separates `src/bin/manage.rs`):

```bash
cargo run -- startapp app --with-manage-bin
```

Dann in `src/bin/manage.rs`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = std::env::var("DATABASE_URL")?;
    let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;

    match args.first().map(String::as_str) {
        Some("import-csv") => my_csv_importer::run(&pool, &args[1..]).await,
        _ => rustango::migrate::manage::run(&pool, "./migrations".as_ref(), args)
            .await
            .map_err(Into::into),
    }
}
```

Führen Sie Ihre eigenen Befehle genau wie die eingebauten aus:
`cargo run -- import-csv path/to/file.csv` (oder
`cargo run --bin manage -- import-csv …` bei Verwendung von
`--with-manage-bin`).

---

## Häufige Arbeitsabläufe

### Erstmalige Projekteinrichtung (single-tenant)

```bash
cargo rustango new myapp
cd myapp
cp .env.example .env             # edit DATABASE_URL
docker compose up -d
cargo run -- migrate
cargo run                        # serve at :8080
```

### Erstmalige Projekteinrichtung (tenancy)

```bash
cargo rustango new myapp --template tenant
cd myapp
cp .env.example .env             # edit DATABASE_URL + RUSTANGO_APEX_DOMAIN
docker compose up -d
cargo run -- migrate                                      # registry + tenants
cargo run -- create-operator admin --password letmein
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
cargo run                        # serve at :8080
```

### Tenants hinzufügen, nachdem die App bereits läuft

Eine echte Tenancy-App baut in der Regel lange vor der Anmeldung ihres ersten
Tenants Modelle und Migrationen auf. Dieser Ablauf funktioniert zu jedem
Zeitpunkt im Leben des Projekts:

```bash
# 1. (any time) develop user models — define structs with #[derive(Model)],
#    add `pub mod ...;` to src/lib.rs.
# 2. Generate scope-aware migrations. In a tenancy project this writes
#    up to TWO files: one tagged registry-scope (touches Org/Operator),
#    one tagged tenant-scope (touches User + your models). Pre-v0.24.2
#    this used to dump everything into one tenant-scoped file and
#    crash on `create-tenant` — see the changelog.
cargo run -- makemigrations

# 3. Apply migrations. `migrate` is scope-aware: it runs registry-
#    scoped files once against the registry pool first, then fans
#    tenant-scoped files across every active tenant.
cargo run -- migrate

# 4. Provision a NEW tenant whenever (could be days, weeks, many
#    migrations later). The tenancy code applies every accumulated
#    tenant-scoped migration to the new tenant's schema in one pass —
#    the new tenant arrives at the same schema state as existing ones.
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
```

Warum das sicher ist:
- `#[rustango(scope = "registry")]` auf `Org`/`Operator` hält Änderungen an
  gemeinsamen Tabellen aus den Per-Tenant-Migrationen heraus.
- `migrate-tenants` besucht jeden aktiven Tenant und wendet nur die
  Tenant-Migrationen an — Registry-Dateien werden übersprungen.
- `create-tenant` führt denselben `migrate-tenants`-Durchlauf gegen das Schema
  des neuen Tenants aus, sodass er vollständig auf dem neuesten Stand startet,
  ohne manuelle Nachbesserung.

### Ein Modell hinzufügen

```bash
cargo run -- startapp blog        # if not done yet
# Edit src/blog/models.rs — add #[derive(Model)]
# Add `pub mod blog;` to src/lib.rs
cargo run -- makemigrations
cargo run -- migrate
```

### Eine JSON-API für dieses Modell hinzufügen

```bash
cargo run -- make:viewset PostViewSet --model Post
# Edit src/post_view_set.rs — fill in field lists
# Mount in src/urls.rs
cargo run                        # GET /api/posts now works
```

### Ein Daten-Backfill hinzufügen

```bash
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title) WHERE slug IS NULL" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs
cargo run -- migrate
```

### Pre-Deploy-Audit

```bash
cargo run --release -- check --deploy
```

### Die letzte Migration zurückrollen

```bash
cargo run -- downgrade 1
```

### Eine Tenancy-Migration auf einen bestimmten Scope anwenden

```bash
cargo run -- migrate-registry            # registry-scoped only
cargo run -- migrate-tenants             # tenant-scoped, fan-out across orgs
```

### Einen Tenant außer Betrieb nehmen

```bash
cargo run -- drop-tenant acme            # soft (reversible)
cargo run -- purge-tenant acme           # hard (drops schema/db)
```

---

## Tenant-Pool-Feinabstimmung (v0.27.7+)

Tenants im database-Modus erhalten ihren eigenen Verbindungspool (einen
`PgPool` — eine Menge wiederverwendeter Datenbankverbindungen), zwischengespeichert
nach Slug in [`TenantPools`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/src/tenancy/pools.rs).
Standardmäßig wird ein Pool **faul, beim ersten Request des Tenants** gebaut, es
sei denn, Sie schalten das Pre-Warming ein. Die Einstellungen leben auf
`TenantPoolsConfig`:

| Feld | Standard | Zweck |
|---|---|---|
| `max_cached_database_pools` | 64 | Obergrenze für den Pool-Cache. Ist er voll, scheitert der nächste nicht-gecachte Tenant (keine stille Verdrängung). |
| `database_pool_max_connections` | 4 | `max_connections` pro Pool. Halten Sie es klein, damit ein Tenant-Fan-out PGs `max_connections` nicht erschöpft. |
| `database_pool_min_connections` | 0 | Hält jederzeit N Verbindungen warm. `≥1` senkt die Latenz des ersten Requests, indem der TCP/TLS/Auth-Roundtrip beim Boot bezahlt wird. |
| `database_pool_acquire_timeout` | 30s | Wie lange `pool.acquire()` wartet, bevor es mit `PoolTimedOut` scheitert. |
| `database_pool_idle_timeout` | 10 min | Schließt untätige Verbindungen nach dieser Dauer. Wehrt Kappungen durch Load-Balancer / `idle_in_transaction_session_timeout` ab. |
| `database_pool_max_lifetime` | 30 min | Erzwingt das Rotieren von Verbindungen, damit vault-geleaste Anmeldedaten aufgefrischt werden. |
| `prewarm_active_tenants` | false | Wenn true, ruft `Server::Builder::serve` beim Boot `prewarm_database_tenants()` auf. |

### Beim Boot vorwärmen

Zwei Wege zum Auslösen:

1. **Automatisch** — setzen Sie `prewarm_active_tenants = true` auf der
   `TenantPoolsConfig`, die Sie `TenantPools::new(...).config(...)` übergeben.
   `Server::Builder::serve` führt das Pre-Warming vor dem Binden aus.

2. **CLI-Verb** — `cargo run -- prewarm-pools` baut Pools für jeden aktiven
   Tenant im database-Modus und beendet sich. Nützlich als Post-Deploy-Hook
   (z. B. nach einer Anmeldedaten-Rotation) oder um zu validieren, dass jeder
   Tenant erreichbar ist, bevor ein Load-Balancer umgeschaltet wird.

Das Pre-Warming durchläuft `Org::objects().where(active = true, storage_mode =
"database")` und bricht kurz, wenn die Cache-Obergrenze erreicht ist (gemeldet
als `skipped_cap` im [`PrewarmReport`]). Per-Tenant-Build-Fehler loggen ein
`tracing::warn!`, brechen die Schleife aber nicht ab.

### Tracing

`crate::tenancy::pools::tenant_pool_init` ist ein `tracing::info_span!`, der den
Cold-Path-Pool-Build umschließt. Abonnieren Sie ihn, um die Per-Tenant-Build-
Latenz zu sehen:

```text
INFO crate::tenancy::pools: tenant pool connected (database mode)
     slug=acme elapsed_ms=42 min_conn=1 max_conn=4
```

### Einrichtungs-Falle — macOS `.local`-TLDs

Wenn Sie den Tenant-Admin über `http://acme.local:8080/admin/` auf macOS
erreichen und bei jedem Request eine 5-Sekunden-Pause sehen: das ist
**Bonjour / mDNS**, nicht **Rustango**. Der Resolver von macOS behandelt
`.local` speziell und wartet den vollen mDNS-Timeout ab, bevor er auf
`/etc/hosts` zurückfällt. Zwei Lösungen:

1. **Verwenden Sie eine andere TLD**: `127.0.0.1 acme.localhost` funktioniert
   ohne Verzögerung. `localhost` ist reserviert (RFC 6761) und überspringt
   mDNS.
2. **Betreiben Sie dnsmasq** mit einer `.local`-Zone, die auf 127.0.0.1 zeigt,
   damit das OS eine sofortige Antwort erhält.

Bestätigen Sie mit `curl -w "%{time_connect}\n"`: wenn `time_connect` ~5s zeigt,
aber mit `--resolve acme.local:8080:127.0.0.1` auf Millisekunden fällt, treffen
Sie auf mDNS.


---

## Siehe auch

- [ORM-Kochbuch](orm.md)
- [Scaffolding](scaffolding.md)
- [ViewSets](viewsets.md)
- [Serializer](serializers.md)
- [Sicherheitsleitfaden](security.md)
