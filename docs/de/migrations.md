# Migrationen & die Migrations-Engine

**Rustango** liefert eine Migrations-Engine im Django-Stil: Sie bearbeiten Ihre Modelle,
führen `makemigrations` aus, um eine versionierte JSON-Datei zu generieren, die die
Schemaänderung beschreibt, und `migrate`, um sie anzuwenden. Seit **0.48** migriert das Framework
sogar **seine eigenen** `rustango_*`-Tabellen über dieselbe Engine —
kein handausgeliefertes Bootstrap-DDL. Diese Seite erklärt die beweglichen Teile, die
ein Upgrade sicher machen: die beiden Migrationsketten, die Squash-Reconciliation
und das abgesicherte Fake-Initial, das einer vorbestehenden Datenbank erlaubt, die
Engine ohne Kollisionen zu übernehmen.

> **Neu bei Migrationen?** Die alltäglichen CLI-Verben — `makemigrations`,
> `migrate`, `migrate --squash`, `migrate --fake`, `downgrade`,
> `showmigrations` — werden Befehl für Befehl im
> [manage-Leitfaden](manage.md#migrations) behandelt. Diese Seite ist das konzeptionelle
> Modell dahinter.

> **Quelle:** `rustango::migrate` (`runner`, `make`, `file`, `manage`)
> und `rustango::tenancy::migrate` — die Reconciliation des Runners lebt in
> `migrate::runner::reconcile`.

---

## Zwei Migrationsketten

Eine Migration gehört zu einer von zwei unabhängigen Ketten, jede mit ihrer eigenen
Ledger-Tabelle (der Zeile angewandter Namen, die der Runner konsultiert, um Arbeit zu überspringen):

| Kette | Was sie verwaltet | Ledger-Tabelle |
|---|---|---|
| **Project** | Ihre `#[derive(Model)]`-Tabellen, in `migrations/` | `__rustango_migrations__` |
| **System** | die eigenen `rustango_*`-Tabellen des Frameworks (`Org`, `User`, Rollen/Berechtigungen, Agenten, Media, …), in `system/migrations/` | `__rustango_system_migrations__` |

Die **System-Kette** ist das, was das Framework-Schema selbstbeschreibend macht.
Ihre Dateien werden aus den kompilierten Framework-Modellen generiert — und sie sind
**`#[cfg(feature = …)]`-bewusst**: eine feature-gegatete Spalte oder Tabelle wird vom
Compiler entfernt, wenn das Feature aus ist, sodass das Aktivieren eines Features
`makemigrations` ein `AddColumn` / `CreateTable` emittieren lässt und das Deaktivieren
ein `DropColumn` / `DropTable` emittiert. Aufgescaffoldete Tenant-Projekte liefern ein
**leeres** `system/migrations/`; das erste `cargo run -- migrate`
generiert und wendet es an (siehe [Scaffolding](scaffolding.md)).

`migrate` wendet die System-Kette **vor** den Migrationen Ihres Projekts an.
Im Tenancy-Modus überlappen sich die beiden Scopes absichtlich bei den geteilten
Framework-Tabellen, sodass die Tenant-Scope-Kette diejenige ist, die läuft; die
reinen Registry-Tabellen (`rustango_orgs`, `rustango_operators`) bedeuten nichts
ohne Tenancy. Nicht-Tenancy-Anwendungen, die ein Framework-Subsystem verwenden (z. B.
`media`), erhalten die System-Kette ebenfalls angewandt.

---

## Squash-Reconciliation — `Migration.replaces`

Ein **Squash** kollabiert eine Reihe historischer Migrationen in eine einzige frisch
generierte Datei, die denselben Endzustand nachbildet — praktisch, wenn ein Stapel
halbfertiger Migrationen leichter neu zu generieren als zu reparieren ist. Der Haken:
die `CREATE TABLE`s der Datei würden auf jeder Datenbank kollidieren, die die Migrationen,
die sie kollabiert hat, bereits angewandt hat (der Checkout eines Kollegen, Staging,
CI).

`migrate --squash` löst das, indem es die **`replaces`**-Liste der neuen Datei mit
den Namen stempelt, die sie kollabiert hat:

```jsonc
{
  "name": "0007_squashed_0001_0006",
  "replaces": ["0001_initial", "0002_add_status", "0003_add_slug", "…"],
  "forward": [ /* recreates the end state */ ]
}
```

Mit gesetztem `replaces` **reconciliiert** der Runner den Squash gegen den tatsächlichen
Zustand der Datenbank, anstatt ihn blind auszuführen. Die Entscheidung ist
automatisch und hängt ganz davon ab, was bereits vorhanden ist:

| Datenbankzustand | Was der Runner tut |
|---|---|
| frisch — keine Historie, keine Tabellen | führt den Squash tatsächlich aus |
| jede ersetzte Migration ist im Ledger | verzeichnet ihn, setzt Tombstones auf die Vorgänger, **kein DDL** |
| Tabellen existieren, aber das Ledger hat keine Historie | verzeichnet ihn, **kein DDL** (Djangos kettenübergreifendes `--fake-initial`) |
| nur *einige* ersetzte Zeilen / Tabellen vorhanden | **verweigert** — benennt, was fehlt, weist Sie an, von Hand aufzulösen |

Der **partielle** Fall ist absichtlich ein harter Fehler: keine automatische Wahl ist
dort sicher, also stoppt der Runner und meldet, was er gefunden hat, statt zu
raten. Lösen Sie ihn mit `migrate --fake` (unten) auf.

Migrationen, die von einem angewandten Squash abgelöst werden, zählen als angewandt, sodass Sie
die kollabierten Dateien für ein oder zwei Releases auf der Platte lassen können — Deployments, die
sie nie ausgeführt haben, migrieren trotzdem korrekt vorwärts. Gewöhnliche (Nicht-Squash-)
Migrationen sind unberührt: eine schlichte Migration, deren Tabelle bereits existiert,
schlägt trotzdem lautstark fehl, weil das ein echter Konflikt ist, keine
bekannt-äquivalente Historie.

---

## Die abgesicherte Fake-Initial-Reconciliation

Dies ist der Mechanismus, der einer **bestehenden** Datenbank erlaubt, die System-
Kette nahtlos zu übernehmen. Vor 0.51 baute das Framework einige seiner Tabellen über
faules `ensure_table`-Roh-DDL; diese Tabellen existieren, sind aber nicht im
`__rustango_system_migrations__`-Ledger verzeichnet, sodass ein frisches `CREATE TABLE`
aus der neuen System-Migration kollidieren würde (`relation "rustango_media"
already exists`, MySQL 1050, …).

Die System-Kette reconciliiert das selbst. Eine ausstehende **System**-Migration wird
inspiziert: die Operationen, die *das Erstellen ihrer Tabellen* ausmachen —
`CreateTable`, plus `CreateIndex` / `CreateM2MTable`, die auf eine Tabelle zielen, die
dieselbe Migration erstellt — sind die akzeptierte Menge. Wenn **jede** Tabelle, die sie
erstellt, bereits existiert, wird die Migration **ins Ledger verzeichnet, ohne
irgendein DDL auszuführen**, und bestehende Daten bleiben unberührt. Wenn nur
*einige* ihrer Tabellen existieren, erstellt die Kette nur die fehlenden
(`CREATE TABLE IF NOT EXISTS`-Semantik) und lässt den Rest in Ruhe.

Der Schutz ist bewusst eng:

- **Auf die eigene System-Kette des Frameworks beschränkt.** Benutzermigrationen verwenden den
  schlichten Runner und faken niemals automatisch — Tabellen-Existenz-Faking ist opt-in
  ausschließlich für den System-Pfad.
- **Alles, was keine Tabellenerstellung ist, disqualifiziert das Faken** — ein Index
  auf einer vorbestehenden Tabelle, ein Alter / Drop / Datenop / Callback fällt
  durch zu einem echten Lauf, sodass echte Arbeit nie übersprungen wird.
- **Existenz wird nur im aktuellen Namespace abgefragt** — Postgres
  `current_schema()`, MySQL `DATABASE()`, SQLite `sqlite_master` — nicht
  über den `search_path`, sodass in schema-modus-Multi-Tenancy eine gleichnamige
  Tabelle in `public` einen Tenant nicht dazu verleiten kann, seine eigenen Tabellen zu überspringen.

Der partielle Zustand eines Squash wird weiterhin verweigert (siehe oben); nur die
System-Kette des Frameworks führt die stückweise „erstelle die fehlenden“-Reparatur durch.

---

## Drift von Hand reparieren — `migrate --fake`

Wenn die Datenbank bereits im Zielzustand ist, aber das Ledger es nicht
weiß (eine außerhalb der Reihe aufgesetzte DB, ein gelöschtes Ledger, eine teilweise
erfolgreiche Migration, ein verweigerter partieller Squash), stempeln Sie eine Migration als angewandt
**ohne ihr SQL auszuführen**:

```bash
cargo run -- migrate --fake 0004_add_indexes
cargo run -- migrate --fake 0001_rustango_registry_initial --system       # framework's own chain
cargo run -- migrate --fake 0001_rustango_registry_initial --all-tenants  # every active tenant
```

- `--system` stempelt die System-Kette des Frameworks
  (`system/migrations/` → `__rustango_system_migrations__`) statt
  der Ihres Projekts.
- `--all-tenants` fächert den Stempel über jeden aktiven Tenant auf, meldet
  jeden und fährt über Fehlschläge hinweg fort — die Framework-Tabellen leben pro
  Tenant, sodass ihre Reparatur eine Aufgabe pro Tenant ist. Kombinieren Sie mit `--system`
  für die Framework-Tabellen über alle Tenants.

Der Name wird zuerst gegen das Migrationsverzeichnis validiert, sodass ein Tippfehler
keine falsche Zeile landen kann; das Stempeln ist idempotent, und das Flag kann
wiederholt werden, um eine Reihe von Zeilen in einem Befehl zu reparieren.

---

## Upgrade auf 0.51.2

> **0.51.0 und 0.51.1 wurden zurückgezogen (yanked)** — die Reconciliation, die sie versprachen, feuerte nie
> tatsächlich gegen echte 0.46–0.50-Datenbanken (0.51.0 verlagerte die Media-
> Tabellen auf System-Migrationen und kollidierte; der Schutz von 0.51.1 verlangte, dass eine
> Migration *rein* `CreateTable` sei, was keine generierte Migration ist).
> **Upgraden Sie direkt auf 0.51.2**, das beides behebt.

Für ein bestehendes Deployment ist das Upgrade ein schlichtes Deploy — kein
Reprovisionieren, kein manuelles DDL:

```bash
cargo run -- migrate
```

Das abgesicherte Fake-Initial handhabt die vorbestehenden Framework-Tabellen: das
erste `migrate` verzeichnet die System-Migration, deren Tabellen bereits existieren,
ins Ledger, ohne sie zu berühren, erstellt nur das, was wirklich fehlt,
und lässt Ihre Daten in Ruhe. Wenn eine Datenbank in einem wahrhaft
inkonsistenten partiellen Zustand ist, stoppt der Runner und sagt Ihnen, was er gefunden hat;
lösen Sie es mit `migrate --fake` auf, statt zu erzwingen.

---

## Siehe auch

- [`manage`-Leitfaden](manage.md#migrations) — jedes Migrations-CLI-Verb, mit
  Beispielen.
- [Scaffolding](scaffolding.md) — woher `migrations/` und
  `system/migrations/` kommen.
- [Models](models.md) — das Derive, aus dem die Migrationen generiert werden.
