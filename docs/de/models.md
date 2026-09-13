# Modelle

Ein Modell ist ein Rust-Struct, das auf eine Datenbanktabelle abgebildet wird. Füge
`#[derive(Model)]` hinzu, annotiere die Felder, und **Rustango** generiert das Schema,
einen typsicheren Abfrage-Einstiegspunkt sowie `save`/`find`/`delete`-Methoden — Djangos
Modelle oder Laravels Eloquent, mit dem Compiler, der deine Spalten prüft. Dies ist die
**Deklarations**-Referenz: jeder Feldtyp, jede Primärschlüssel-Option und jedes
`#[rustango(...)]`-Attribut. Für das *Abfragen* von Modellen, sobald sie deklariert sind,
siehe das [ORM-Kochbuch](orm.md).

[![Modelle in Rustango: ein #[derive(Model)]-Struct bildet Rust-Feldtypen auf dialektspezifische Spalten ab, der Primärschlüssel kann ein auto-inkrementierender Auto<i64> oder ein benutzerdefinierter, anwendungsseitig zugewiesener Schlüssel sein, und das Derive generiert SCHEMA + objects() + save/find](../img/models.png)](../img/models.png)

> **Ein Begriff hier ist neu für dich?** *model*, *primary key*, *foreign key*,
> *migration*, *nullable* — siehe das [Glossar](glossary.md).

> **Quelle:** `rustango::Model` (`#[derive(Model)]`), `rustango::core`
> (`Model`-Trait, `ModelSchema`, `FieldType`, `Auto`, `ForeignKey`) und die
> Dialekt-Typabbildungen in `rustango::sql::{dialect, mysql, sqlite}` — immer
> kompiliert (wähle ein Backend-Feature: `postgres` / `mysql` / `sqlite`).
>
> **Lauffähige Version:** die Feldtyp-Round-Trips, der benutzerdefinierte PK und die
> SCHEMA-Snippets sind aus
> [`models_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/models_doc.rs) kopiert
> (`cargo test -p rustango --features sqlite --test models_doc`).

## Inhaltsverzeichnis

- [Anatomie eines Modells](#anatomy-of-a-model)
- [Feldtypen](#field-types) · [Nur-PostgreSQL-Typen](#postgresql-only-types)
- [Primärschlüssel](#primary-keys) — [benutzerdefinierte PKs](#custom-primary-keys) · [zusammengesetzte](#composite-primary-keys)
- [Beziehungen](#relationships)
- [Übliche Feldattribute](#common-field-attributes)
- [Indizes & Constraints](#indexes-and-constraints)
- [Übliche Modellattribute](#common-model-attributes)
- [Die generierte API](#the-generated-api) — [save vs insert](#save-vs-insert)
- [Vollständige Attributreferenz](#full-attribute-reference)
- [Siehe auch](#see-also)

---

## Anatomie eines Modells

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(table = "posts", display = "title")]   // model-level attributes
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,                            // field-level attributes

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(fk = "authors", on = "id")]
    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}
```

Aus dieser einen Deklaration generiert das Derive:

- das **Schema** (`Post::SCHEMA` — Tabellenname, Spalten, Typen, den PK), das
  Migrationen und das Admin lesen;
- einen Abfrage-Einstiegspunkt, **`Post::objects()`**, der ein `QuerySet<Post>`
  zurückgibt;
- **typisierte Feldkonstanten** (`Post::title`, `Post::author_id`) für
  compilergeprüfte Filter — `Post::objects().where_(Post::author_id.eq(42))`;
- Zeilenmethoden — **`save`**, **`find`**, **`delete`** und mehr (siehe
  [die generierte API](#the-generated-api)).

Der Tabellenname ist standardmäßig der Modellname, wenn du `table` weglässt;
Spaltennamen sind standardmäßig der snake_case-Feldname, sofern du nicht `column`
setzt.

---

## Feldtypen

Der Rust-Typ des Feldes bestimmt seinen Datenbank-Spaltentyp. Rustango bildet jeden Typ
pro Dialekt ab, sodass dasselbe Modell auf PostgreSQL, MySQL und SQLite funktioniert:

| Rust-Typ | PostgreSQL | MySQL | SQLite |
|---|---|---|---|
| `i16` | `SMALLINT` | `SMALLINT` | `INTEGER` |
| `i32` | `INTEGER` | `INT` | `INTEGER` |
| `i64` | `BIGINT` | `BIGINT` | `INTEGER` |
| `f32` | `REAL` | `FLOAT` | `REAL` |
| `f64` | `DOUBLE PRECISION` | `DOUBLE` | `REAL` |
| `bool` | `BOOLEAN` | `TINYINT(1)` | `INTEGER` (0/1) |
| `String` | `TEXT` | `TEXT` | `TEXT` |
| `String` + `max_length = N` | `VARCHAR(N)` | `VARCHAR(N)` | `TEXT` |
| `chrono::DateTime<Utc>` | `TIMESTAMPTZ` | `DATETIME(6)` | `TEXT` (ISO-8601) |
| `chrono::NaiveDate` | `DATE` | `DATE` | `TEXT` |
| `chrono::NaiveTime` | `TIME` | `TIME(6)` | `TEXT` |
| `uuid::Uuid` | `UUID` | `CHAR(36)` | `TEXT` |
| `serde_json::Value` | `JSONB` | `JSON` | `TEXT` |
| `rust_decimal::Decimal` | `NUMERIC` | `DECIMAL(38,10)` | `NUMERIC` |
| `Vec<u8>` | `BYTEA` | `LONGBLOB` | `BLOB` |
| `Option<T>` | `T NULL` | `T NULL` | `T` (nullable) |

`Option<T>` ist die Art, eine Spalte **nullable** zu machen — ein Feld ohne `Option` ist
`NOT NULL`. All diese durchlaufen einen Round-Trip über `save` → `find`, durchgängig
verifiziert:

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "gadget")]
pub struct Gadget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
    pub qty: i64,
    pub active: bool,
    pub note: Option<String>,        // nullable
    pub made_at: DateTime<Utc>,
    pub meta: serde_json::Value,     // JSON
}
```

> **Dezimalpräzision.** PostgreSQL `NUMERIC` ist beliebig genau; MySQL verwendet
> `DECIMAL(38,10)` (38 Stellen, 10 Nachkommastellen — die breiteste portable Passung);
> SQLite verwendet `NUMERIC`-Affinität. Verwende `rust_decimal::Decimal` für Geld,
> niemals `f64`.

### Nur-PostgreSQL-Typen

Diese bilden auf native PostgreSQL-Spaltentypen ab und haben **kein MySQL/SQLite-
Äquivalent** — der Migrations-Writer gibt dort `TEXT` aus, um gültig zu bleiben, aber das
Lesen/Schreiben davon schlägt zur Laufzeit auf jenen Backends mit einem Fehler fehl.
Verwende sie nur in PostgreSQL-Deployments:

| Rust-Typ | PostgreSQL | Anmerkungen |
|---|---|---|
| `Array<T>` | `text[]` / `integer[]` / `bigint[]` | native Arrays |
| `Range<T>` | `int4range` / `int8range` / `numrange` / `daterange` / `tstzrange` | Range-Typen |
| `HStore` | `hstore` | flache String→String-Map (benötigt die Extension) |
| `Vector` + `#[rustango(vector(dims = N))]` | `vector(N)` | pgvector-Embeddings |
| `Point` + `#[rustango(geometry(srid = N))]` | `geometry(Point, N)` | PostGIS |

---

## Primärschlüssel

Jedes Modell braucht einen Primärschlüssel. Markiere ein Feld mit
`#[rustango(primary_key)]`; wenn du keines markierst, sucht das Schema nach einer Spalte
namens `id`.

Der **Standard und häufigste** PK ist eine auto-inkrementierende 64-Bit-Ganzzahl,
deklariert als `Auto<i64>`:

```rust
#[rustango(primary_key)]
pub id: Auto<i64>,
```

**`Auto<T>`-Semantik.** Ein `Auto<T>`-Feld ist entweder `Unset` (der Wert, den die DB
zuweisen wird) oder `Set(v)`. Beim Einfügen wird ein `Unset`-PK aus der Spaltenliste
ausgelassen, sodass die Datenbank ihn generiert, dann wird der Wert zurückgelesen
(`RETURNING` auf PostgreSQL/SQLite, `LAST_INSERT_ID()` auf MySQL) und auf deinem Struct
gespeichert:

```rust
let mut g = Gadget { id: Auto::default(), /* … */ };   // Unset
g.save_pool(&pool).await?;                              // DB assigns the id
let new_id = g.id.get().copied().unwrap();             // now populated
```

Unterstützte innere `Auto<T>`-Typen sind `i32`, `i64` und `Uuid`.

### Benutzerdefinierte Primärschlüssel

Der PK muss keine auto-inkrementierende Ganzzahl sein. Jeder Typ, der auf eine Spalte
abgebildet wird, kann der PK sein; du weist den Wert selbst zu:

| PK-Deklaration | Spaltentyp | Wer weist ihn zu |
|---|---|---|
| `Auto<i64>` *(Standard)* | `BIGSERIAL` / `BIGINT AUTO_INCREMENT` / `INTEGER … AUTOINCREMENT` | Datenbank |
| `Auto<i32>` | `SERIAL` / `INT AUTO_INCREMENT` / `INTEGER …` | Datenbank |
| `Auto<Uuid>` + `auto_uuid` | `UUID` | Rust-seitig (`uuid v4`) |
| `Auto<Uuid>` + `default_uuid_v7` | `UUID` | Rust-seitig (sortierbares `uuid v7`) |
| `String` + `primary_key` | `VARCHAR(N)` / `TEXT` | du (Anwendung) |
| `Uuid` + `primary_key` | `UUID` / `CHAR(36)` / `TEXT` | du (Anwendung) |
| `i64` / `i32` + `primary_key` | `BIGINT` / `INTEGER` | du (Anwendung) |

Ein **natürlicher String-Schlüssel** (z. B. ein Gutscheincode) — beachte, dass du den
Wert selbst bereitstellst und mit [`insert_pool`](#save-vs-insert) einfügst, da es kein
`Auto::Unset` gibt, das `save` erkennen könnte:

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "coupon")]
pub struct Coupon {
    #[rustango(primary_key, max_length = 32)]
    pub code: String,        // you assign this
    pub discount: i64,
}

let c = Coupon { code: "SAVE10".into(), discount: 10 };
c.insert_pool(&pool).await?;                       // explicit INSERT
let back = Coupon::find_or_fail("SAVE10".to_string(), &pool).await?;   // look up by the string PK
```

**Benenne die PK-Spalte um** mit `column` (das Rust-Feld bleibt `number`, die SQL-Spalte
ist `account_no`):

```rust
#[rustango(primary_key, column = "account_no")]
pub number: i64,
```

```rust
// Introspect it via the schema:
let pk = Account::SCHEMA.primary_key().unwrap();
assert_eq!(pk.name, "number");        // Rust field
assert_eq!(pk.column, "account_no");  // SQL column
```

**UUID-Primärschlüssel** generieren den Wert Rust-seitig: `auto_uuid` gibt ein zufälliges
v4, `default_uuid_v7` ein zeitlich sortierbares v7 (besser für Index-Lokalität):

```rust
#[rustango(primary_key, auto_uuid)]
pub id: Auto<uuid::Uuid>,
```

### Zusammengesetzte Primärschlüssel

Native mehrspaltige Primärschlüssel werden **nicht unterstützt** — genau ein Feld darf
`primary_key` sein. Das ausgelieferte Muster ist ein Surrogat-`Auto<i64>`-PK plus ein
deklarierter zusammengesetzter Unique-Constraint, was index-äquivalent ist:

```rust
#[derive(Model)]
#[rustango(table = "line_item", unique_together = "invoice_id, line_no")]
pub struct LineItem {
    #[rustango(primary_key)]
    pub id: Auto<i64>,        // surrogate PK
    pub invoice_id: i64,
    pub line_no: i32,         // (invoice_id, line_no) is unique together
}
```

Schlage Zeilen nach mit
`.where_(LineItem::invoice_id.eq(..)).where_(LineItem::line_no.eq(..))`.

---

## Beziehungen

Ein Fremdschlüssel ist eine `_id`-Spalte plus ein optionaler typisierter Accessor:

```rust
// Plain FK column — store the parent's id:
#[rustango(fk = "authors", on = "id")]
pub author_id: i64,

// Typed FK — lazy-loads the parent on demand:
pub author: ForeignKey<Author>,
```

`ForeignKey<T>` setzt seinen Schlüsseltyp standardmäßig auf `i64`; wenn der PK des
Elternobjekts einen anderen Typ hat, benenne ihn: `ForeignKey<User, String>`.
Eins-zu-eins verwendet `#[rustango(o2o)]`; viele-zu-viele ist eine separate Tabelle —
siehe [ORM-Kochbuch → Viele-zu-viele](orm.md#many-to-many). Lade verwandte Zeilen eager
mit `select_related` (ebenfalls im ORM-Leitfaden).

---

## Übliche Feldattribute

`#[rustango(...)]` auf einem Feld. Die, die du ständig verwenden wirst:

| Attribut | Beispiel | Wirkung |
|---|---|---|
| `primary_key` | `#[rustango(primary_key)]` | markiert den PK |
| `max_length = N` | `#[rustango(max_length = 200)]` | `VARCHAR(N)` + Längenprüfung beim Schreiben |
| `default = "…"` | `#[rustango(default = "'draft'")]` | DB-Spaltenstandard (SQL-Literal) |
| `unique` | `#[rustango(unique)]` | Unique-Constraint auf der Spalte |
| `choices = "…"` | `#[rustango(choices = "draft:Draft, published:Published")]` | aufgezählte Werte (`value:Label`); Admin-`<select>` + Validierung |
| `auto_now_add` | `#[rustango(auto_now_add)]` | beim **Einfügen** auf jetzt setzen (auf einem `Auto<DateTime<Utc>>`) |
| `auto_now` | `#[rustango(auto_now)]` | bei **jedem Speichern** auf jetzt setzen |
| `column = "…"` | `#[rustango(column = "account_no")]` | die SQL-Spalte umbenennen |
| `null` / `Option<T>` | `pub note: Option<String>` | nullable Spalte |
| `min` / `max` | `#[rustango(min = 0, max = 100)]` | Bereichsvalidierung beim Schreiben |
| `blank` / `editable` | `#[rustango(editable = false)]` | Formular-/Admin-Verhalten |
| `db_comment = "…"` | `#[rustango(db_comment = "cents")]` | Spalten-COMMENT |

`choices`, `default`, `auto_now_add` und Soft-Delete zusammen (alle verifiziert):

```rust
#[rustango(max_length = 20, default = "'draft'", choices = "draft:Draft, published:Published")]
pub status: String,
#[rustango(auto_now_add)]
pub created_at: Auto<DateTime<Utc>>,
#[rustango(soft_delete)]
pub deleted_at: Option<DateTime<Utc>>,
```

---

## Indizes und Constraints

Auf dem **Modell** deklariert:

```rust
#[rustango(
    table = "posts",
    index("status, published_at"),                 // composite btree index
    unique_together = "author_id, slug",           // multi-column unique
    check(name = "qty_nonneg", expr = "qty >= 0"), // CHECK constraint
)]
```

- **`index(...)`** — standardmäßig ein Btree-Index; wähle für PostgreSQL eine Methode
  mit `index(columns = "body", method = "gin")` (auch `gist`, `brin`, `hash`, `bloom`,
  `spgist`).
- **`unique_together` / `index_together`** — mehrspaltig unique / nicht-unique.
- **Partielle Indizes** — `unique_when(...)` / `index_when(...)` fügen eine `WHERE`-
  Bedingung hinzu.
- **`check(name, expr)`** — ein CHECK-Constraint; **`exclude(...)`** ist ein
  PostgreSQL-EXCLUDE-Constraint.

---

## Übliche Modellattribute

`#[rustango(...)]` auf dem Struct:

| Attribut | Beispiel | Wirkung |
|---|---|---|
| `table = "…"` | `table = "posts"` | Tabellenname (standardmäßig der Struct-Name) |
| `display = "…"` | `display = "title"` | das Feld, das angezeigt wird, wenn eine Zeile referenziert wird (FK-Labels, Admin) |
| `app = "…"` | `app = "blog"` | gruppiert das Modell unter einer App |
| `default_order = "…"` | `default_order = "-created_at"` | Standardsortierung für Abfragen |
| `default_permissions` | `default_permissions = "add, change"` | welche Auto-Berechtigungen erstellt werden |
| `soft_delete` *(Feld)* | `#[rustango(soft_delete)] deleted_at: Option<…>` | Soft-Delete aktivieren (markieren, nicht entfernen) |
| `audit(track = "…")` | `audit(track = "title, status")` | Änderungshistorie pro Zeile aufzeichnen |
| `scope = "…"` | `scope = "tenant"` | Multi-Tenancy-Scope (Registry vs. Mandant) |
| `admin(...)` | `admin(list_display = "…")` | Admin-UI-Konfiguration — siehe [das Admin](admin.md) |

---

## Die generierte API

`#[derive(Model)]` implementiert das `Model`-Trait (`Post::SCHEMA`) und generiert:

- **`Post::objects()`** (Alias `Post::query()`) → ein `QuerySet<Post>` zum Filtern,
  Ordnen und Abrufen (das [ORM-Kochbuch](orm.md) deckt die Abfrage-API ab).
- **Typisierte Feldkonstanten** — `Post::title`, `Post::author_id` — verwendet in
  `.where_(Post::author_id.eq(42))` für compilergeprüfte Filter.
- **Finder** — `find(pk, &pool)` → `Option<Self>`; `find_or_fail(pk, &pool)` →
  `Self` (Fehler, wenn nicht vorhanden); `find_many(pks, &pool)`; `find_or_insert(...)`.
- **Writer** — `save`/`save_pool`, `save_partial(&["title"], &pool)` (nur einige Spalten
  aktualisieren), `insert_pool` (explizites Einfügen), `delete`.
- **Soft-Delete** (wenn aktiviert) — `soft_delete`, `restore`, `force_delete`;
  `QuerySet::active()` / `with_trashed()` / `only_trashed()`.

### save vs insert

Das bringt Leute durcheinander, daher ist es die klare Aussage wert:

| Methode | Verhalten |
|---|---|
| `save_pool(&mut self, &pool)` | **INSERT**, wenn der `Auto`-PK `Unset` ist, andernfalls **UPDATE** |
| `insert_pool(&self, &pool)` | immer **INSERT** |

Für den Standard-`Auto<i64>`-PK macht `save_pool` automatisch das Richtige. Für einen
**anwendungsseitig zugewiesenen PK** (ein `String`/`Uuid`, den du selbst setzt) gibt es
keinen `Unset`-Zustand — daher würde `save_pool` eine (möglicherweise nicht existierende)
Zeile UPDATEn. Verwende **`insert_pool`**, um eine brandneue Zeile mit einem
benutzerdefinierten PK einzufügen (im begleitenden Test verifiziert).

---

## Vollständige Attributreferenz

Jeder `#[rustango(...)]`-Schlüssel, den das Derive akzeptiert. Die üblichen sind oben
behandelt; dies ist die vollständige Liste, einschließlich fortgeschrittener/PostgreSQL-
spezifischer.

### Auf Modellebene (auf dem Struct)

| Attribut | Wert | Wirkung |
|---|---|---|
| `table` | `"name"` | Tabellenname |
| `display` | `"field"` | menschenlesbares Label für eine Zeile |
| `app` | `"name"` | App-Gruppierung |
| `default_order` | `"-field"` | Standardsortierung |
| `default_permissions` | `"add, change, delete, view"` | zu erstellende Auto-Berechtigungen |
| `default_related_name` | `"posts"` | Name des Rückwärts-Accessors auf dem Elternobjekt |
| `base_manager_name` | `"all_objects"` | Name des Basis- (ungefilterten) Managers |
| `manager(ext = "Trait")` | Trait-Pfad | ein benutzerdefiniertes Manager-Erweiterungs-Trait generieren |
| `manager_fn` | `"published"` | einen Manager-Accessor über `objects()` hinaus hinzufügen |
| `get_latest_by` | `"created_at"` | Standardspalte für `latest()`/`earliest()` |
| `order_with_respect_to` | `"parent"` | Django elternrelative Ordnung |
| `index(...)` | `columns`, `method`, `name` | Sekundärindex (btree/gin/gist/brin/hash/bloom/spgist) |
| `unique_together` | `"a, b"` | zusammengesetzter Unique-Constraint |
| `index_together` | `"a, b"` | zusammengesetzter Nicht-Unique-Index |
| `unique_when(...)` / `index_when(...)` | Spalten + `condition` | partieller (bedingter) Index |
| `check(...)` | `name`, `expr` | CHECK-Constraint |
| `exclude(...)` | Operator-Spezifikation | PostgreSQL-EXCLUDE-Constraint |
| `audit(track = "…")` | Feldliste | Änderungshistorie pro Zeile |
| `scope` | `"tenant"` / `"registry"` | Multi-Tenancy-Scope |
| `proxy` | Flag | Proxy-Modell (teilt sich die Tabelle eines anderen) |
| `global_scope(name, apply = fn)` | Name + fn | Filter, der automatisch auf alle Abfragen angewendet wird |
| `through(...)` | Relationsspezifikation | benutzerdefinierter Through-Relation-Accessor |
| `reverse_has(...)` / `generic_has(...)` | Relationsspezifikation | Rückwärts-has-many / Rückwärts-generischer-FK-Accessor |
| `required_db_features` / `required_db_vendor` | Liste / Vendor | Deployment-Validierungs-Constraints |
| `db_table_comment` | `"…"` | Tabellen-COMMENT |
| `admin(...)` | Admin-Optionen | Admin-UI-Konfiguration (siehe [admin.md](admin.md)) |

### Auf Feldebene (auf einem Feld)

| Attribut | Wert | Wirkung |
|---|---|---|
| `primary_key` | Flag | markiert den PK |
| `column` | `"name"` | die SQL-Spalte umbenennen |
| `max_length` | `N` | `VARCHAR(N)` + Längenvalidierung |
| `default` | `"sql literal"` | Spalten-DEFAULT |
| `null` | Flag | nullable (oder verwende `Option<T>`) |
| `unique` | Flag | Unique-Constraint |
| `choices` | `"v:Label, …"` | aufgezählte Werte |
| `min` / `max` | Zahl | Bereichsvalidierung |
| `blank` | Flag | Leereingabe in Formularen/Admin erlauben |
| `editable` | `true`/`false` | Bearbeitbarkeit in Formular/Admin |
| `auto_now` | Flag | bei jedem Speichern auf jetzt setzen |
| `auto_now_add` | Flag | beim Einfügen auf jetzt setzen |
| `auto_uuid` | Flag | Rust-seitiges UUID v4 (auf `Auto<Uuid>`) |
| `default_uuid_v7` | Flag | Rust-seitiges sortierbares UUID v7 |
| `fk` + `on` | `"table"`, `"col"` | Fremdschlüsselspalte |
| `cascade` | Flag | `ON DELETE CASCADE` |
| `o2o` | Flag | Eins-zu-eins-Beziehung |
| `fk_composite(...)` / `generic_fk(...)` | Spezifikation | zusammengesetzter FK / generischer (Content-Type-) FK |
| `generated_as` | `"expr"` | DB-berechnete (generierte) Spalte |
| `citext` | Flag | case-insensitiver Text (PostgreSQL CITEXT) |
| `vector(dims = N)` | `N` | pgvector-Dimension |
| `geometry(srid = N)` | `N` | PostGIS-Raumbezugs-ID |
| `db_comment` | `"…"` | Spalten-COMMENT |

---

## Siehe auch

- [ORM-Kochbuch](orm.md) — Abfragen, Filter, Aggregationen, Joins, Transaktionen
  (was mit einem Modell zu tun ist, sobald es deklariert ist).
- [Serializer](serializers.md) — ein Modell für eine API in JSON formen.
- [Das Admin](admin.md) — der `admin(...)`-Block und die generierte UI.
- [Scaffolding](scaffolding.md) · [`manage`-CLI](manage.md) — ein Modell und seine
  Migration generieren.
