# Modelos

Un modelo es una struct de Rust que se asigna a una tabla de base de datos. Añade
`#[derive(Model)]`, anota los campos, y **Rustango** genera el esquema, un punto de
entrada de consultas con tipos seguros y los métodos `save`/`find`/`delete` — los modelos
de Django o el Eloquent de Laravel, con el compilador verificando tus columnas. Esta es
la referencia de **declaración**: cada tipo de campo, cada opción de clave primaria y
cada atributo `#[rustango(...)]`. Para *consultar* los modelos una vez declarados,
consulta el [recetario del ORM](orm.md).

[![Modelos en Rustango: una struct #[derive(Model)] asigna tipos de campo Rust a columnas por dialecto, la clave primaria puede ser un Auto<i64> autoincremental o una clave personalizada asignada por la aplicación, y el derive genera SCHEMA + objects() + save/find](img/models.png)](img/models.png)

> **¿Un término aquí es nuevo para ti?** *model*, *primary key*, *foreign key*,
> *migration*, *nullable* — consulta el [glosario](glossary.md).

> **Fuente:** `rustango::Model` (`#[derive(Model)]`), `rustango::core`
> (el trait `Model`, `ModelSchema`, `FieldType`, `Auto`, `ForeignKey`), y las
> asignaciones de tipos por dialecto en `rustango::sql::{dialect, mysql, sqlite}` —
> siempre compilados (elige una característica de backend: `postgres` / `mysql` /
> `sqlite`).
>
> **Versión ejecutable:** los round-trips de tipos de campo, la PK personalizada y los
> fragmentos de SCHEMA están copiados de
> [`models_doc.rs`](../crates/rustango/tests/models_doc.rs)
> (`cargo test -p rustango --features sqlite --test models_doc`).

## Tabla de contenidos

- [Anatomía de un modelo](#anatomy-of-a-model)
- [Tipos de campo](#field-types) · [Tipos exclusivos de PostgreSQL](#postgresql-only-types)
- [Claves primarias](#primary-keys) — [PKs personalizadas](#custom-primary-keys) · [compuestas](#composite-primary-keys)
- [Relaciones](#relationships)
- [Atributos de campo comunes](#common-field-attributes)
- [Índices y restricciones](#indexes-and-constraints)
- [Atributos de modelo comunes](#common-model-attributes)
- [La API generada](#the-generated-api) — [save vs insert](#save-vs-insert)
- [Referencia completa de atributos](#full-attribute-reference)
- [Véase también](#see-also)

---

## Anatomía de un modelo

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

A partir de esa única declaración el derive genera:

- el **esquema** (`Post::SCHEMA` — nombre de tabla, columnas, tipos, la PK) que leen las
  migraciones y el admin;
- un punto de entrada de consultas, **`Post::objects()`**, que devuelve un
  `QuerySet<Post>`;
- **constantes de campo tipadas** (`Post::title`, `Post::author_id`) para filtros
  verificados en compilación — `Post::objects().where_(Post::author_id.eq(42))`;
- métodos de fila — **`save`**, **`find`**, **`delete`** y más (consulta
  [la API generada](#the-generated-api)).

El nombre de la tabla toma por defecto el nombre del modelo si omites `table`; los
nombres de columna toman por defecto el nombre del campo en snake_case a menos que
establezcas `column`.

---

## Tipos de campo

El tipo Rust del campo determina su tipo de columna en la base de datos. Rustango asigna
cada tipo por dialecto, de modo que el mismo modelo funciona en PostgreSQL, MySQL y
SQLite:

| Tipo Rust | PostgreSQL | MySQL | SQLite |
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

`Option<T>` es la forma de hacer que una columna sea **nullable** — un campo que no es
`Option` es `NOT NULL`. Todos estos hacen un round-trip a través de `save` → `find`,
verificado de extremo a extremo:

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

> **Precisión decimal.** El `NUMERIC` de PostgreSQL es de precisión arbitraria; MySQL usa
> `DECIMAL(38,10)` (38 dígitos, 10 fraccionarios — el ajuste portable más amplio); SQLite
> usa afinidad `NUMERIC`. Usa `rust_decimal::Decimal` para dinero, nunca `f64`.

### Tipos exclusivos de PostgreSQL

Estos se asignan a tipos de columna nativos de PostgreSQL y **no tienen equivalente en
MySQL/SQLite** — el escritor de migraciones emite `TEXT` allí para mantener la validez,
pero leerlos/escribirlos produce un error en tiempo de ejecución en esos backends. Úsalos
solo en despliegues de PostgreSQL:

| Tipo Rust | PostgreSQL | Notas |
|---|---|---|
| `Array<T>` | `text[]` / `integer[]` / `bigint[]` | arrays nativos |
| `Range<T>` | `int4range` / `int8range` / `numrange` / `daterange` / `tstzrange` | tipos de rango |
| `HStore` | `hstore` | mapa plano string→string (necesita la extensión) |
| `Vector` + `#[rustango(vector(dims = N))]` | `vector(N)` | embeddings de pgvector |
| `Point` + `#[rustango(geometry(srid = N))]` | `geometry(Point, N)` | PostGIS |

---

## Claves primarias

Cada modelo necesita una clave primaria. Marca un campo con
`#[rustango(primary_key)]`; si no marcas ninguno, el esquema busca una columna llamada
`id`.

La PK **por defecto y más común** es un entero de 64 bits autoincremental, declarado como
`Auto<i64>`:

```rust
#[rustango(primary_key)]
pub id: Auto<i64>,
```

**Semántica de `Auto<T>`.** Un campo `Auto<T>` es o bien `Unset` (el valor que asignará
la BD) o `Set(v)`. Al insertar, una PK `Unset` se omite de la lista de columnas para que
la base de datos la genere, luego el valor se vuelve a leer (`RETURNING` en
PostgreSQL/SQLite, `LAST_INSERT_ID()` en MySQL) y se almacena en tu struct:

```rust
let mut g = Gadget { id: Auto::default(), /* … */ };   // Unset
g.save_pool(&pool).await?;                              // DB assigns the id
let new_id = g.id.get().copied().unwrap();             // now populated
```

Los tipos internos de `Auto<T>` admitidos son `i32`, `i64` y `Uuid`.

### Claves primarias personalizadas

La PK no tiene por qué ser un entero autoincremental. Cualquier tipo que se asigne a una
columna puede ser la PK; tú mismo asignas el valor:

| Declaración de PK | Tipo de columna | Quién la asigna |
|---|---|---|
| `Auto<i64>` *(por defecto)* | `BIGSERIAL` / `BIGINT AUTO_INCREMENT` / `INTEGER … AUTOINCREMENT` | base de datos |
| `Auto<i32>` | `SERIAL` / `INT AUTO_INCREMENT` / `INTEGER …` | base de datos |
| `Auto<Uuid>` + `auto_uuid` | `UUID` | lado Rust (`uuid v4`) |
| `Auto<Uuid>` + `default_uuid_v7` | `UUID` | lado Rust (`uuid v7` ordenable) |
| `String` + `primary_key` | `VARCHAR(N)` / `TEXT` | tú (aplicación) |
| `Uuid` + `primary_key` | `UUID` / `CHAR(36)` / `TEXT` | tú (aplicación) |
| `i64` / `i32` + `primary_key` | `BIGINT` / `INTEGER` | tú (aplicación) |

Una **clave natural de tipo string** (por ejemplo, un código de cupón) — ten en cuenta
que proporcionas el valor e insertas con [`insert_pool`](#save-vs-insert), ya que no hay
un `Auto::Unset` que `save` pueda detectar:

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

**Renombra la columna de la PK** con `column` (el campo Rust sigue siendo `number`, la
columna SQL es `account_no`):

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

**Las claves primarias UUID** generan el valor en el lado Rust: `auto_uuid` da un v4
aleatorio, `default_uuid_v7` un v7 ordenable por tiempo (mejor para la localidad de
índice):

```rust
#[rustango(primary_key, auto_uuid)]
pub id: Auto<uuid::Uuid>,
```

### Claves primarias compuestas

Las claves primarias nativas de varias columnas **no están soportadas** — exactamente un
campo puede ser `primary_key`. El patrón provisto es una PK subrogada `Auto<i64>` más una
restricción de unicidad compuesta declarada, que es equivalente en términos de índice:

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

Busca las filas con
`.where_(LineItem::invoice_id.eq(..)).where_(LineItem::line_no.eq(..))`.

---

## Relaciones

Una clave foránea es una columna `_id` más un accesor tipado opcional:

```rust
// Plain FK column — store the parent's id:
#[rustango(fk = "authors", on = "id")]
pub author_id: i64,

// Typed FK — lazy-loads the parent on demand:
pub author: ForeignKey<Author>,
```

`ForeignKey<T>` toma por defecto `i64` como tipo de clave; si la PK del padre es de un
tipo distinto, nómbralo: `ForeignKey<User, String>`. Uno-a-uno usa `#[rustango(o2o)]`;
muchos-a-muchos es una tabla separada — consulta
[Recetario del ORM → Muchos-a-muchos](orm.md#many-to-many). Carga de forma anticipada las
filas relacionadas con `select_related` (también en la guía del ORM).

---

## Atributos de campo comunes

`#[rustango(...)]` sobre un campo. Los que usarás constantemente:

| Atributo | Ejemplo | Efecto |
|---|---|---|
| `primary_key` | `#[rustango(primary_key)]` | marca la PK |
| `max_length = N` | `#[rustango(max_length = 200)]` | `VARCHAR(N)` + comprobación de longitud en escritura |
| `default = "…"` | `#[rustango(default = "'draft'")]` | valor por defecto de columna en la BD (literal SQL) |
| `unique` | `#[rustango(unique)]` | restricción de unicidad sobre la columna |
| `choices = "…"` | `#[rustango(choices = "draft:Draft, published:Published")]` | valores enumerados (`value:Label`); `<select>` del admin + validación |
| `auto_now_add` | `#[rustango(auto_now_add)]` | establecer a ahora en la **inserción** (en un `Auto<DateTime<Utc>>`) |
| `auto_now` | `#[rustango(auto_now)]` | establecer a ahora en **cada guardado** |
| `column = "…"` | `#[rustango(column = "account_no")]` | renombrar la columna SQL |
| `null` / `Option<T>` | `pub note: Option<String>` | columna nullable |
| `min` / `max` | `#[rustango(min = 0, max = 100)]` | validación de rango en escritura |
| `blank` / `editable` | `#[rustango(editable = false)]` | comportamiento en formulario/admin |
| `db_comment = "…"` | `#[rustango(db_comment = "cents")]` | COMMENT de columna |

`choices`, `default`, `auto_now_add` y borrado lógico juntos (todos verificados):

```rust
#[rustango(max_length = 20, default = "'draft'", choices = "draft:Draft, published:Published")]
pub status: String,
#[rustango(auto_now_add)]
pub created_at: Auto<DateTime<Utc>>,
#[rustango(soft_delete)]
pub deleted_at: Option<DateTime<Utc>>,
```

---

## Índices y restricciones

Declarados sobre el **modelo**:

```rust
#[rustango(
    table = "posts",
    index("status, published_at"),                 // composite btree index
    unique_together = "author_id, slug",           // multi-column unique
    check(name = "qty_nonneg", expr = "qty >= 0"), // CHECK constraint
)]
```

- **`index(...)`** — un índice btree por defecto; elige un método para PostgreSQL con
  `index(columns = "body", method = "gin")` (también `gist`, `brin`, `hash`, `bloom`,
  `spgist`).
- **`unique_together` / `index_together`** — de varias columnas único / no único.
- **Índices parciales** — `unique_when(...)` / `index_when(...)` añaden una condición
  `WHERE`.
- **`check(name, expr)`** — una restricción CHECK; **`exclude(...)`** es una restricción
  EXCLUDE de PostgreSQL.

---

## Atributos de modelo comunes

`#[rustango(...)]` sobre la struct:

| Atributo | Ejemplo | Efecto |
|---|---|---|
| `table = "…"` | `table = "posts"` | nombre de tabla (por defecto el nombre de la struct) |
| `display = "…"` | `display = "title"` | el campo mostrado cuando se referencia una fila (etiquetas FK, admin) |
| `app = "…"` | `app = "blog"` | agrupa el modelo bajo una app |
| `default_order = "…"` | `default_order = "-created_at"` | orden por defecto para las consultas |
| `default_permissions` | `default_permissions = "add, change"` | qué autopermisos crear |
| `soft_delete` *(campo)* | `#[rustango(soft_delete)] deleted_at: Option<…>` | habilitar borrado lógico (marcar, no eliminar) |
| `audit(track = "…")` | `audit(track = "title, status")` | registrar el historial de cambios por fila |
| `scope = "…"` | `scope = "tenant"` | ámbito de multi-tenancy (registro vs inquilino) |
| `admin(...)` | `admin(list_display = "…")` | configuración de la UI del admin — consulta [el admin](admin.md) |

---

## La API generada

`#[derive(Model)]` implementa el trait `Model` (`Post::SCHEMA`) y genera:

- **`Post::objects()`** (alias `Post::query()`) → un `QuerySet<Post>` para filtrar,
  ordenar y obtener (el [recetario del ORM](orm.md) cubre la API de consultas).
- **Constantes de campo tipadas** — `Post::title`, `Post::author_id` — usadas en
  `.where_(Post::author_id.eq(42))` para filtros verificados en compilación.
- **Buscadores** — `find(pk, &pool)` → `Option<Self>`; `find_or_fail(pk, &pool)` →
  `Self` (error si no existe); `find_many(pks, &pool)`; `find_or_insert(...)`.
- **Escritores** — `save`/`save_pool`, `save_partial(&["title"], &pool)` (actualiza solo
  algunas columnas), `insert_pool` (inserción explícita), `delete`.
- **Borrado lógico** (cuando está habilitado) — `soft_delete`, `restore`,
  `force_delete`; `QuerySet::active()` / `with_trashed()` / `only_trashed()`.

### save vs insert

Esto confunde a la gente, así que vale la pena decirlo con claridad:

| Método | Comportamiento |
|---|---|
| `save_pool(&mut self, &pool)` | **INSERT** si la PK `Auto` está `Unset`, de lo contrario **UPDATE** |
| `insert_pool(&self, &pool)` | siempre **INSERT** |

Para la PK `Auto<i64>` por defecto, `save_pool` hace lo correcto automáticamente. Para una
**PK asignada por la aplicación** (un `String`/`Uuid` que estableces tú mismo), no hay un
estado `Unset` — así que `save_pool` haría un UPDATE de una fila (posiblemente
inexistente). Usa **`insert_pool`** para insertar una fila totalmente nueva con una PK
personalizada (verificado en el test de respaldo).

---

## Referencia completa de atributos

Cada clave `#[rustango(...)]` que acepta el derive. Las comunes se cubren arriba; esta es
la lista completa, incluyendo las avanzadas/específicas de PostgreSQL.

### A nivel de modelo (sobre la struct)

| Atributo | Valor | Efecto |
|---|---|---|
| `table` | `"name"` | nombre de tabla |
| `display` | `"field"` | etiqueta legible para una fila |
| `app` | `"name"` | agrupación por app |
| `default_order` | `"-field"` | orden por defecto |
| `default_permissions` | `"add, change, delete, view"` | autopermisos a crear |
| `default_related_name` | `"posts"` | nombre del accesor inverso en el padre |
| `base_manager_name` | `"all_objects"` | nombre del manager base (sin filtrar) |
| `manager(ext = "Trait")` | ruta del trait | generar un trait de extensión de manager personalizado |
| `manager_fn` | `"published"` | añadir un accesor de manager más allá de `objects()` |
| `get_latest_by` | `"created_at"` | columna por defecto para `latest()`/`earliest()` |
| `order_with_respect_to` | `"parent"` | ordenación relativa al padre de Django |
| `index(...)` | `columns`, `method`, `name` | índice secundario (btree/gin/gist/brin/hash/bloom/spgist) |
| `unique_together` | `"a, b"` | restricción de unicidad compuesta |
| `index_together` | `"a, b"` | índice no único compuesto |
| `unique_when(...)` / `index_when(...)` | columnas + `condition` | índice parcial (condicional) |
| `check(...)` | `name`, `expr` | restricción CHECK |
| `exclude(...)` | especificación de operador | restricción EXCLUDE de PostgreSQL |
| `audit(track = "…")` | lista de campos | historial de cambios por fila |
| `scope` | `"tenant"` / `"registry"` | ámbito de multi-tenancy |
| `proxy` | flag | modelo proxy (comparte la tabla de otro) |
| `global_scope(name, apply = fn)` | nombre + fn | filtro aplicado automáticamente a todas las consultas |
| `through(...)` | especificación de relación | accesor de relación through personalizado |
| `reverse_has(...)` / `generic_has(...)` | especificación de relación | accesor inverso has-many / inverso de FK genérica |
| `required_db_features` / `required_db_vendor` | lista / proveedor | restricciones de validación de despliegue |
| `db_table_comment` | `"…"` | COMMENT de tabla |
| `admin(...)` | opciones de admin | configuración de la UI del admin (consulta [admin.md](admin.md)) |

### A nivel de campo (sobre un campo)

| Atributo | Valor | Efecto |
|---|---|---|
| `primary_key` | flag | marca la PK |
| `column` | `"name"` | renombrar la columna SQL |
| `max_length` | `N` | `VARCHAR(N)` + validación de longitud |
| `default` | `"sql literal"` | DEFAULT de columna |
| `null` | flag | nullable (o usa `Option<T>`) |
| `unique` | flag | restricción de unicidad |
| `choices` | `"v:Label, …"` | valores enumerados |
| `min` / `max` | número | validación de rango |
| `blank` | flag | permitir vacío en formularios/admin |
| `editable` | `true`/`false` | editabilidad en formulario/admin |
| `auto_now` | flag | establecer a ahora en cada guardado |
| `auto_now_add` | flag | establecer a ahora en la inserción |
| `auto_uuid` | flag | UUID v4 del lado Rust (en `Auto<Uuid>`) |
| `default_uuid_v7` | flag | UUID v7 ordenable del lado Rust |
| `fk` + `on` | `"table"`, `"col"` | columna de clave foránea |
| `cascade` | flag | `ON DELETE CASCADE` |
| `o2o` | flag | relación uno-a-uno |
| `fk_composite(...)` / `generic_fk(...)` | especificación | FK compuesta / FK genérica (content-type) |
| `generated_as` | `"expr"` | columna computada (generada) por la BD |
| `citext` | flag | texto insensible a mayúsculas (CITEXT de PostgreSQL) |
| `vector(dims = N)` | `N` | dimensión de pgvector |
| `geometry(srid = N)` | `N` | id de referencia espacial de PostGIS |
| `db_comment` | `"…"` | COMMENT de columna |

---

## Véase también

- [Recetario del ORM](orm.md) — consultas, filtros, agregaciones, joins, transacciones
  (qué hacer con un modelo una vez declarado).
- [Serializadores](serializers.md) — dar forma a un modelo en JSON para una API.
- [El admin](admin.md) — el bloque `admin(...)` y la UI generada.
- [Scaffolding](scaffolding.md) · [CLI `manage`](manage.md) — generar un modelo y su
  migración.
