# Recetario del ORM

Patrones para el ORM de **Rustango** más allá de lo básico. Si vienes del ORM de Django, de Eloquent de Laravel o de ActiveRecord de Rails, las formas que verás aquí te resultarán familiares. La mayoría de los ejemplos asumen que ya tienes un modelo `Post` de `Getting Started`.

[![Consultas del ORM verificadas por tipos: filtros encadenados, ordenamiento, límites y agregación — todo sin SQL crudo](img/orm.png)](img/orm.png)

> **Fuente:** `rustango::sql` (`QuerySet`, la macro `Q!` / el builder `Qb`) y la
> API de consulta de `#[derive(Model)]` — siempre compilada; elige una característica de backend
> (`postgres` / `mysql` / `sqlite`).
>
> **Versión ejecutable:** los patrones aquí se ejecutan en el ejemplo probado
> [`orm_cookbook`](../crates/rustango/examples/orm_cookbook).
>
> **¿Nuevo con algún término de aquí?** El [glosario](glossary.md) define *model*, *queryset*,
> *pool* y *migración* en lenguaje sencillo.

Algunos términos de Rust se repiten a lo largo del documento. `&pool` es una referencia compartida al pool de conexiones de la base de datos; se la pasas a los métodos que realmente ejecutan SQL. `.await` ejecuta una llamada asíncrona y espera el resultado. `Option<T>` es un valor que puede estar presente (`Some`) o ausente (`None`) — el null de Rust. `Result` es éxito-o-error; el `?` al final de una llamada retorna anticipadamente ante un error. `Auto<i64>` es una primary key autoincremental que está o bien `Set` (cargada desde la BD) o bien `Unset` (aún no insertada).

## Novedades (v0.41 / v0.42)

Las versiones recientes añadieron un lote de características con paridad con Django que aún no están integradas en cada una de las secciones de abajo. Referencias rápidas:

- **Macro `Q!` + builder en tiempo de ejecución `Qb`** (#269, #263) — filtros con forma de Django, seguros en tiempo de compilación. `User::objects().where_(Q!(User.email__icontains = "alice"))` no compila si el nombre del campo tiene una errata. Variante componible en tiempo de ejecución para chips de filtro del admin: `let q = Qb::eq("active", true) & Qb::gt("age", 18i64);`.
- **`.distinct_on(&["author_id"])`** (#264) — nativo en PG; fallback portable con funciones de ventana en MySQL / SQLite. Patrones de "el más reciente por grupo".
- **`bulk_upsert_pool(rows, unique_fields, update_fields, &pool)`** (#267) — el `bulk_create(update_conflicts=True)` de Django. `ON CONFLICT` / `ON DUPLICATE KEY UPDATE` tri-dialecto.
- **`explain_pool()`** (#272) — `EXPLAIN` tri-dialecto. PG `EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS)` / MySQL `EXPLAIN ANALYZE` / SQLite `EXPLAIN QUERY PLAN`.
- **Biblioteca de funciones de BD** (#266) — `Cast`, `LPad`, `RPad`, `MD5`, `SHA1`, `SHA256`, `Position`, `Repeat`, `Reverse`, `Sign`, `Mod`, `Power`, `Sqrt`. Emisión por dialecto con errores claros donde SQLite carece de la función.
- **Tipos de campo** — `rust_decimal::Decimal` (nativo en PG/MySQL, en SQLite vía shim de Decode), `chrono::NaiveTime`, `Vec<u8>` (`FieldType::Binary`) ahora aceptados por `#[derive(Model)]` (#524, v0.42).
- **`ModelForm::prepare_save()` / `PreparedSave`** (#375, v0.42) — el `save(commit=False)` de Django. Valida ahora, muta el conjunto de escritura preparado, confirma cuando estés listo.
- **`#[rustango(unique_when(columns = "...", condition = "..."))]`** (#265) — restricciones únicas parciales. "Email único por fila no eliminada" / "Slug único por tenant".
- **`#[rustango(manager(ext = "FooManagerExt"))]`** (#271) — trait de extensión de manager personalizado con forma de Django, emitido junto al modelo. (También es la forma en Rust de los proxy models de Django — misma tabla física, múltiples "personalidades" vía métodos por trait. Ver `inheritance.rs:98-127`.)
- **`manage makemigrations --merge`** (#346, v0.42) — nodo de fusión con forma de Django para cadenas de ramas divergentes. Ver [`docs/manage.md`](manage.md#makemigrations---merge).

El CHANGELOG contiene el índice completo de tickets de cada versión.

## Tabla de contenidos

- [Consultas](#querying)
- [Valores calculados y funciones de base de datos](#computed-values--database-functions)
- [Agregaciones](#aggregations)
- [Joins y precarga de filas relacionadas](#joins--preloading-related-rows)
- [Operaciones en lote](#bulk-operations)
- [Insertar o actualizar (upsert)](#insert-or-update-upsert)
- [Transacciones](#transactions)
- [Muchos-a-muchos](#many-to-many)
- [JSON / JSONB](#json--jsonb)
- [Borrado lógico (soft delete)](#soft-delete)
- [Rastro de auditoría](#audit-trail)
- [Válvula de escape a SQL crudo](#raw-sql-escape-hatch)
- [Carga perezosa de FK](#lazy-fk-loading)
- [Cuatro maneras de filtrar](#four-ways-to-filter)
- [Consultas acotadas por tenant](#tenant-scoped-queries)
- [Señales](#signals)
- [Consejos de rendimiento](#performance-tips)

---

## Consultas

Lee filas de la base de datos. `Post::objects()` inicia una consulta (como `Post.objects` de Django); encadenas filtros y ordenamiento, luego llamas a `.fetch(&pool).await?` para ejecutarla y recuperar un `Vec<Post>`. `.where_(...)` añade una condición unida con AND.

```rust
use rustango::core::Column as _;
use rustango::core::{Op, SqlValue, WhereExpr};   // for filter_op / where_raw below
use rustango::sql::FetcherPool as _;

// Simplest — fetch all
let posts = Post::objects().fetch(&pool).await?;

// Single equality filter
let drafts = Post::objects()
    .where_(Post::status.eq("draft"))
    .fetch(&pool).await?;

// Chained filters (AND)
let recent_drafts = Post::objects()
    .where_(Post::status.eq("draft"))
    .where_(Post::author_id.eq(42))
    .where_(Post::deleted_at.is_null())
    .order_by(&[("created_at", true)])        // true = DESC
    .limit(20)
    .fetch(&pool).await?;

// String-keyed filter (validated at compile of the queryset)
let by_id = Post::objects()
    .filter_op("id", Op::Eq, SqlValue::I64(42))
    .fetch(&pool).await?;

// OR / nested
let qs = Post::objects().where_raw(WhereExpr::Or(vec![
    Post::status.eq("draft").into(),
    Post::status.eq("review").into(),
]));

// XOR — Django 4.1+ `Q(a) ^ Q(b)`. Matches rows where an odd number
// of operands evaluate to true (binary case = "exactly one is true").
// Issue #27.
let either_but_not_both = Post::objects()
    .where_(Post::status.eq("draft").xor(Post::author_id.eq(42)))
    .fetch(&pool).await?;
// Tri-dialect emission: native logical XOR exists on MySQL but not PG
// or SQLite, so the writer emits a portable rewrite uniformly —
// `(a AND NOT b) OR (NOT a AND b)` for the binary form, or a
// CASE-WHEN-1/0 tally `% 2 = 1` for N-ary chains.
```

### Filtros de comparación

Los métodos de filtro cotidianos, uno por cada operador SQL. Estos son los field lookups de Django (`__gt`, `__in`, `__icontains`, etc.) en forma tipada.

```rust
Post::objects().where_(Post::view_count.gt(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.gte(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.lt(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.lte(100)).fetch(&pool).await?;
Post::objects().where_(Post::status.ne("archived")).fetch(&pool).await?;
Post::objects().where_(Post::id.is_in([1, 2, 3])).fetch(&pool).await?;
Post::objects().where_(Post::status.not_in(["draft", "deleted"])).fetch(&pool).await?;
Post::objects().where_(Post::title.like("%draft%")).fetch(&pool).await?;          // case-sensitive contains
Post::objects().where_(Post::title.ilike("%draft%")).fetch(&pool).await?;         // case-insensitive contains
Post::objects().where_(Post::title.ilike("Hello%")).fetch(&pool).await?;          // case-insensitive starts-with
Post::objects().where_(Post::deleted_at.is_null()).fetch(&pool).await?;
Post::objects().where_(Post::published_at.between(start, end)).fetch(&pool).await?;
```

### Ordenar resultados

Ordena filas por una o más columnas, por una expresión, o con control explícito sobre dónde caen los NULL. Más allá del `.order_by(&[("col", desc)])` básico, obtienes tres dimensiones extra:

```rust
use rustango::core::funcs::lower;
use rustango::core::{F, NullsOrder};

// 1. Plain field + ASC/DESC (back-compat — implicit NULLS handling
//    differs between dialects; see the dialect note below).
Post::objects()
    .order_by(&[("published_at", true), ("id", false)])
    .fetch(&pool).await?;

// 2. Explicit NULLS FIRST/LAST control — portable across PG, MySQL,
//    and SQLite. MySQL has no native `NULLS …` keyword; the writer
//    emulates with an `<col> IS NULL` pre-sort term so the on-wire
//    ordering matches PG/SQLite.
Post::objects()
    .order_by_with_nulls(&[("score", true, NullsOrder::Last)])
    .fetch(&pool).await?;

// 3. Arbitrary Expr in the ORDER BY position — case-insensitive
//    title sort via `LOWER(title)`, computed sort keys via
//    `case() / when() / value()`, arithmetic via `F("a") + F("b")`.
Post::objects()
    .order_by_expr(lower(F("title")), false)
    .order_by_expr_with_nulls(F("score") + 1_i64, true, NullsOrder::Last)
    .fetch(&pool).await?;
```

**Manejo de NULL por dialecto (sin `NullsOrder` explícito establecido):**

| Dialecto | Predeterminado ASC | Predeterminado DESC |
|---|---|---|
| PostgreSQL | NULLS LAST | NULLS FIRST |
| SQLite | NULLS LAST | NULLS FIRST |
| MySQL | NULLs primero (semántica de valor-más-pequeño) | NULLs al final |

Usa `.order_by_with_nulls(...)` / `.order_by_expr_with_nulls(...)` para fijar la ubicación; de lo contrario se aplica el valor predeterminado nativo de la base de datos. En MySQL el writer emite `<col> IS NULL <asc|desc>` por delante del ordenamiento real para emular; el SQL emitido tiene dos términos ORDER BY por columna fijada, pero la semántica coincide con PG/SQLite.

**Composición de la cadena.** `.order_by(...)`, `.order_by_with_nulls(...)` y `.order_by_expr(...)` se acumulan en una lista unificada en **orden de registro**. `.replace_order_by(&[...])` limpia toda llamada de order-by previa. `.flip_order_by()` invierte cada dirección Y también intercambia `NullsOrder::First` ↔ `NullsOrder::Last` para que la semántica de "NULLs en el mismo extremo" sobreviva a una inversión (para `First` / `Last` explícitos; el comportamiento predeterminado del dialecto bajo `Default` sigue rastreando la dirección).

### Ordenamiento aleatorio

Devuelve las filas en orden aleatorio — el `.order_by('?')` de Django. Usa `.order_random()`. Emite `ORDER BY RANDOM()` en PG y SQLite, `ORDER BY RAND()` en MySQL. Práctico para rotación de banners, muestreo o asignación de buckets de tests A/B sin traer las filas a la aplicación para barajarlas.

```rust
// Three random posts.
Post::objects()
    .order_random()
    .limit(3)
    .fetch(&pool).await?;

// Random tie-breaker after a primary sort: posts ordered by score
// descending, with ties shuffled.
Post::objects()
    .order_by(&[("score", true)])
    .order_random()
    .fetch(&pool).await?;
```

La variante de IR no lleva dirección ni cláusula NULLS: el ordenamiento aleatorio es no-ordenado por definición, y la clave aleatoria se calcula por fila (no-NULL).

**Advertencia de rendimiento.** `ORDER BY RANDOM()` fuerza un **escaneo completo de tabla + ordenamiento en memoria por una clave aleatoria por fila**. El planificador de consultas no puede usar un índice. Para tablas mucho más grandes que la memoria, prefiere el patrón amigable con los índices:

```rust
// Coin-flip offset; range-scans the PK index.
let max_id: i64 = Post::objects().max::<i64>("id", &pool).await?.unwrap_or(0);
let offset = rand::random::<u32>() as i64 % max_id.max(1);
Post::objects()
    .where_(Post::id.gte(offset))
    .order_by(&[("id", false)])
    .limit(1)
    .fetch(&pool).await?;
```

La contrapartida: la adyacencia en las filas del resultado refleja la adyacencia de la PK, así que no es "uniformemente aleatorio" en sentido estricto — pero está libre del costo del escaneo completo de tabla.

### Paginación

Trae una página de resultados a la vez. `.limit(size).offset(...)` es la forma simple por número de página; la forma con cursor ("todo lo que sigue al último id que vi") escala mejor en tablas grandes.

```rust
// Page-number — page 2 of 50-row pages = LIMIT 50 OFFSET 50.
let page = Post::objects().limit(50).offset(50).fetch(&pool).await?;

// Cursor (manual — no auto-next-token from QuerySet)
let next = Post::objects()
    .where_(Post::id.gt(last_id))
    .order_by(&[("id", false)])
    .limit(50)
    .fetch(&pool).await?;
```

Para paginación con cursor del lado HTTP, usa `ViewSet::cursor_pagination("id")` en su lugar.

### Traer filas a un mapa

Busca muchas filas por una lista de valores y recupéralas como un `HashMap` indexado por esa columna. Este es el `in_bulk(ids, field_name=)` de Django. Usa `.in_bulk(...)` para "trae estas N filas en un solo viaje de ida y vuelta, indexadas por id". Un `HashMap<K, V>` es el diccionario / tabla hash de Rust.

```rust
use std::collections::HashMap;
use rustango::sql::Auto;

// Default Django shape: keyed by the Auto<i64> PK.
let books: HashMap<i64, Book> = Book::objects()
    .in_bulk(Book::id, [1_i64, 2, 3], |b| match b.id {
        Auto::Set(v) => v,
        Auto::Unset  => unreachable!("fetched row has Auto::Set PK"),
    }, &pool)
    .await?;
assert_eq!(books[&1].title, "The Rust Programming Language");

// `field_name=` equivalent — key by any unique column.
let by_isbn: HashMap<String, Book> = Book::objects()
    .in_bulk(Book::isbn, ["isbn-1".to_string()], |b| b.isbn.clone(), &pool)
    .await?;
```

Se compone con filtros `.where_()` previos — la lista `IN` se une con AND al WHERE existente. Un `ids` vacío hace un corto-circuito con un mapa vacío (no se emite SQL). El closure maneja el desempaquetado de `Auto<T>` / `ForeignKey<T, K>` explícitamente, dando a quien llama control sobre cómo se materializa la clave.

Hermano acotado por tenant: `in_bulk_on(column, ids, extract, &executor)` acepta cualquier executor de sqlx — combínalo con `tenant.conn()` para tenants en modo schema.

### Bloquear filas para actualización

Bloquea las filas que seleccionas para que ninguna otra transacción pueda cambiarlas hasta que confirmes — la forma estándar de reclamar trabajo o prevenir actualizaciones perdidas. Este es el `select_for_update(skip_locked=, nowait=, of=, no_key=)` de Django. Llama a `.select_for_update()`; añade `SELECT … FOR UPDATE` (o una variante) y el bloqueo dura mientras dure la transacción circundante.

```rust
// Canonical "claim next available row" pattern. Worker A grabs the
// lowest-priority pending job; concurrent worker B with SKIP LOCKED
// skips A's row and grabs the next instead — no blocking.
let mut tx = pool.begin().await?;
let claim: Vec<Job> = Job::objects()
    .where_(Job::status.eq("pending"))
    .order_by(&[("priority", false)])
    .limit(1)
    .select_for_update()
    .skip_locked()
    .fetch_on(&mut *tx).await?;
// ... mark claim[0] as in-progress, do work ...
tx.commit().await?;
```

**Métodos del builder** — encadena para optar por ellos:

- `.select_for_update()` — `FOR UPDATE` simple.
- `.skip_locked()` — añade `SKIP LOCKED`; las filas retenidas por otra tx se filtran silenciosamente en lugar de bloquear.
- `.nowait()` — añade `NOWAIT`; expone un error del driver de inmediato si cualquier fila coincidente está bloqueada. Mutuamente excluyente con `skip_locked` (el writer elige el más permisivo `SKIP LOCKED` si ambos están establecidos).
- `.no_key()` — emite `FOR NO KEY UPDATE` en su lugar (PG 9.3+). Bloqueo más débil que no bloquea a escritores que tocan solo columnas que no son clave.
- `.of(&["table_or_alias", …])` — restringe el bloqueo a tablas específicas cuando la consulta hace JOIN.

Llamar a `.skip_locked()` / `.nowait()` / `.no_key()` / `.of(…)` sin un `.select_for_update()` previo habilita el bloqueo implícitamente, coincidiendo con la ergonomía de Django.

**Comportamiento tri-dialecto:**

| Dialecto | Comportamiento |
|---|---|
| PostgreSQL | Soporte completo — cada flag emite su sintaxis nativa. |
| MySQL 8.0.1+ | Soporta todo excepto `NO KEY` — ese flag recae en `FOR UPDATE` simple (el bloqueo más estricto). |
| SQLite | Sin sintaxis de bloqueo a nivel de fila. El writer no emite ninguna cláusula; las transacciones mantienen un bloqueo de escritura implícito sobre toda la base de datos. Usa una estrategia distinta para SQLite (típicamente un bucle busy-wait sobre la transacción misma). |

**Debe ejecutarse dentro de una transacción.** `FOR UPDATE` fuera de una tx es un no-op en PostgreSQL (la tx implícita de una sola sentencia libera el bloqueo de inmediato) y un error en MySQL. Combínalo con `pool.begin()` (o `rustango::sql::atomic`).

### Combinar consultas (unión, intersección, diferencia)

Fusiona dos o más consultas sobre el mismo modelo con operadores de conjunto de SQL. Estos son `.union()`, `.intersection()` y `.difference()` de Django.

```rust
// Posts that are EITHER drafts OR currently in review.
let inbox: Vec<Post> = Post::objects()
    .where_(Post::status.eq("draft"))
    .union(Post::objects().where_(Post::status.eq("review")))
    .order_by(&[("created_at", true)])
    .limit(50)
    .fetch(&pool).await?;
```

**Métodos del builder**:

| Método | SQL | Semántica |
|---|---|---|
| `.union(other)` | `UNION` | Combinar + deduplicar |
| `.union_all(other)` | `UNION ALL` | Combinar, conservar duplicados (más barato, sin pasada DISTINCT) |
| `.intersection(other)` | `INTERSECT` | Filas en AMBOS querysets |
| `.difference(other)` | `EXCEPT` | Filas en el primer queryset pero NO en los otros |

Cada método toma un `QuerySet<T>` — ambas ramas deben apuntar al mismo modelo `T`, así que la forma de columnas coincide por construcción (verificado en tiempo de compilación por los genéricos de Rust). Las llamadas se acumulan; mezclar operadores en una sola cadena está permitido (`a.union(b).intersection(c)` se evalúa de izquierda a derecha según el estándar SQL).

**Los modificadores externos se aplican al resultado fusionado**:

```rust
// Outer .order_by() / .limit() / .offset() / .select_for_update()
// set AFTER the union apply to the combined resultset, NOT per-branch.
let page: Vec<Post> = qs_a
    .union(qs_b)
    .union(qs_c)
    .order_by(&[("id", false)])    // sorts the merged rows
    .limit(20)                     // caps the merged count
    .offset(40)                    // skips into the merged result
    .fetch(&pool).await?;

// Per-branch ORDER BY / LIMIT stay INSIDE the branch's parens:
let mixed = qs_a
    .union(qs_b.order_by(&[("id", true)]).limit(5))   // branch picks its top 5
    .fetch(&pool).await?;
```

**Tri-dialecto**: PostgreSQL + SQLite soportan los cuatro operadores en todas las versiones que **Rustango** soporta. MySQL 8.0+ soporta `UNION`/`UNION ALL`; `INTERSECT`/`EXCEPT` llegaron en MySQL 8.0.31. Las versiones más antiguas de MySQL exponen el error de sintaxis del driver en el momento del fetch — no hay un gate del lado del cliente.

**Camino de error en el builder tipado**: `.union(other_qs)` (y `.intersection()` / `.difference()`) compila la rama de forma anticipada y hace panic si la rama no compila (columna con errata, etc.). Para composición falible donde quien llama quiere un `Result`, compila la rama primero y pásala vía `.with_compound(SetOp::Union, branch)` — un único punto de entrada genérico cubre todos los operadores. La forma del panic coincide con la de Django: una rama incorrecta es un error de programador, no una condición de datos en tiempo de ejecución.

### Streaming de conjuntos de resultados grandes

Procesa una tabla enorme sin cargarla toda en memoria. Este es el `.iterator(chunk_size=2000)` de Django. Llama a `.iterator(chunk_size)`; trae `chunk_size` filas a la vez (vía `LIMIT N OFFSET M`) y nunca almacena en búfer el conjunto de resultados completo. Recurre a ella en exportaciones de millones de filas, pipelines de ETL y trabajos por lotes.

```rust
// 1. Whole-chunk loop — process N rows at a time.
let mut iter = Post::objects()
    .where_(Post::published.eq(true))
    .order_by(&[("id", false)])
    .iterator(2_000)?;
while let Some(chunk) = iter.next_chunk(&pool).await? {
    for post in chunk { /* … */ }
}

// 2. Row-by-row loop — buffer one chunk internally, yield one row.
let mut iter = Post::objects().order_by(&[("id", false)]).iterator(2_000)?;
while let Some(post) = iter.next_row(&pool).await? {
    /* … */
}
```

**Establece un `order_by`.** `OFFSET` contra una consulta sin un ordenamiento estable devuelve filas impredecibles entre chunks — típicamente `.order_by(&[("pk", false)])` para que cada chunk se recoja limpiamente. El método no impone el ordenamiento (algunas consultas legítimamente no quieren ordenamiento, p. ej. un vaciado de un solo tiro), pero iterar sin ordenar es un peligro latente.

**Contrapartida frente a los cursores del lado del servidor.** Esto es un simple fragmentador con LIMIT/OFFSET. Sobre una columna de ordenamiento indexada con btree, PostgreSQL escanea las primeras N filas antes de devolver la (N+1)-ésima — así que la paginación profunda es un trabajo total de `O(n²)`. Para un vaciado de 10M filas esto importa; para 100k filas normalmente no. El fragmentador gana en portabilidad (funciona en todos los backends sin sobrecarga de transacciones) y simplicidad (sin gestión del ciclo de vida del cursor). Para lecturas verdaderamente en streaming en PG, baja a `pool.begin()` + la API Stream cruda `sqlx::query(...).fetch(&mut *tx)` directamente — el protocolo extendido hace streaming desde el servidor sin re-búsqueda por offset.

**Mezclar `next_chunk` y `next_row` en el mismo iterador es seguro.** El búfer interno `VecDeque` se drena en orden de fila antes de cualquier nuevo fetch de BD, así que `next_chunk` tras un vaciado parcial de `next_row` produce primero las filas restantes en búfer, y luego continúa con chunks nuevos.

Tanto `.rows_seen()` (conteo acumulado) como `.is_exhausted()` (bandera post-vaciado) están disponibles para reportar progreso y para comprobaciones de terminación.

**Peligro de escritura concurrente.** Cada chunk es una consulta separada, así que las filas insertadas/eliminadas entre chunks pueden omitirse o duplicarse (el clásico problema de "ventanas" de la paginación con OFFSET). Para tablas de solo-lectura / solo-append — el caso de uso típico de exportación — esto no es una preocupación. Para tablas que se escriben de forma concurrente necesitas una transacción con aislamiento de snapshot para que cada chunk vea la misma vista. **`ChunkedIter` toma `&Pool`, no un `&mut Transaction`, así que la API del fragmentador no se puede usar dentro de la tx directamente** — en su lugar, haz a mano el SELECT fragmentado contra la tx:

```rust
let mut tx = pool.begin().await?;
sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
    .execute(&mut *tx).await?;

// Hand-loop LIMIT/OFFSET chunks against the tx with `.fetch_on(&mut *tx)`,
// so every chunk reads from the same snapshot.
let chunk_size = 2_000_i64;
let mut offset = 0_i64;
loop {
    let rows: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .limit(chunk_size)
        .offset(offset)
        .fetch_on(&mut *tx)
        .await?;
    if rows.is_empty() { break; }
    for post in &rows { /* … */ }
    if (rows.len() as i64) < chunk_size { break; }
    offset += rows.len() as i64;
}
tx.commit().await?;
```

**`select_for_update()` no se propaga entre chunks.** Los bloqueos de fila retenidos por `.select_for_update()` se liberan al final de la transacción implícita de cada chunk. No hay un arreglo con forma de fragmentador: el builder `.iterator()` toma `&Pool`, las variantes de bloqueo necesitan un `&mut Transaction`, y los dos no se componen. Para un vaciado con bloqueo tienes dos caminos, cada uno con una contrapartida:

- **`.fetch_on(&mut *tx)` de todo el resultado** — un solo viaje de ida y vuelta, el `Vec<T>` completo en memoria. Está bien cuando el resultado cabe.
- **LIMIT/OFFSET a mano dentro de la tx** — la misma forma que el fragmento de aislamiento de snapshot de arriba; los chunks se mantienen en streaming pero estás fuera de la API `ChunkedIter`.

Un futuro compañero `iterator_on(&mut *tx, chunk_size)` (seguimiento en un issue) cerraría esta brecha. Fuera de alcance para el issue #23.

**`chunk_size` debe ser > 0.** Los valores cero o negativos hacen panic. Elige un valor que quepa en tu presupuesto de tamaño de fila (el predeterminado de Django es `2000`; razonable para filas estrechas, más bajo para columnas anchas TEXT/JSONB).

### Seleccionar columnas específicas

Trae solo unas pocas columnas en lugar de structs `Post` completos — los `.values('col')` y `.values_list('col', flat=True)` de Django. Úsalos cuando solo necesitas un par de columnas de una tabla ancha, o cuando el resultado alimenta código dinámico (plantillas, exportación a CSV, JSON). Recuperas mapas, tuplas o una lista tipada plana en lugar de instancias del modelo.

```rust
use rustango::core::SqlValue;
use std::collections::HashMap;

// 1. Column-keyed map per row — Django's `.values('id', 'title')`.
let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
    .where_(Post::published.eq(true))
    .order_by(&[("id", false)])
    .values_dict(&["id", "title"])
    .fetch(&pool).await?;

// 2. Ordered tuple per row — Django's `.values_list('id', 'title')`.
//    Cell ordering matches the column-list argument.
let rows: Vec<Vec<SqlValue>> = Post::objects()
    .values_list(&["title", "id"])  // title first, id second
    .fetch(&pool).await?;

// 3. Single-column typed scalar — Django's `.values_list('id', flat=True)`.
//    Returns Vec<U> directly via sqlx's typed scalar path.
let ids: Vec<i64> = Post::objects()
    .where_(Post::published.eq(true))
    .values_list_flat("id")
    .fetch::<i64>(&pool).await?;
```

**Tres builders, un IR.** Los tres establecen `SelectQuery::projection` a la lista de columnas validada — el SQL es idéntico entre las tres formas terminales; solo difiere la decodificación de fila:

| Builder | Forma SQL | Devuelve |
|---|---|---|
| `.values_dict(&[cols])` | `SELECT col1, col2 FROM …` | `Vec<HashMap<String, SqlValue>>` |
| `.values_list(&[cols])` | `SELECT col1, col2 FROM …` | `Vec<Vec<SqlValue>>` (ordenado por `cols`) |
| `.values_list_flat(col)` | `SELECT col FROM …` | `Vec<U>` (tipado, vía `fetch::<U>(...)`) |

**Funciona con el resto de la cadena de consulta.** `.where_()`, `.filter()`, `.order_by()`, `.limit()`, `.offset()` y los operadores de conjunto (`.union()` / `.intersection()` / `.difference()`) — cada método llamado ANTES de `.values_*` se propaga. Los builders de values son terminales (nada se encadena después de ellos), así que establece la forma de la consulta primero y luego trae.

**Validación en el momento de `.compile()` / `.fetch()`:**
- Lista de columnas vacía (`.values_dict(&[])`) → [`QueryError::EmptyValuesProjection`].
- Nombre de columna con errata (`.values_dict(&["nope"])`) → [`QueryError::UnknownField`].

**Tri-dialecto: emisión de proyección idéntica entre PG / MySQL / SQLite** (solo difiere el entrecomillado de identificadores). Para `.values_list_flat::<U>(...)`, `U` debe implementar `Decode + Type` de sqlx en cada backend que el binario tenga como objetivo — las opciones comunes (`i64`, `i32`, `String`, `bool`, `f64`) funcionan universalmente.

**¿Por qué no cambiar el `.values()` existente para hacer proyección pura?** `QuerySet::values(cols)` ya asciende a [`AggregateBuilder`] para el camino de auto-inferencia de GROUP BY (issue #75). Renombrarlo rompería ~20 sitios de llamada existentes. Los nuevos métodos de cadena `.values_dict()` / `.values_list()` / `.values_list_flat()` conviven junto a él, dejando el camino de agregación intacto. El error preexistente `QueryError::ValuesRequiresAggregate` sigue disparándose para `.values(cols).compile()` sin un `.annotate(...)` posterior — su mensaje ahora dirige a quien llama hacia los nuevos métodos de proyección pura.

### Incluir o excluir columnas

Misma idea que la sección anterior, pero en la forma include/exclude de Django: `.only('id', 'name')` conserva solo las columnas nombradas, `.defer('big_field')` conserva todo excepto esas. Úsalos en tablas anchas donde columnas grandes TEXT / BLOB / JSONB hacen que las vistas de lista sean caras de leer:

```rust
// .only(...) — fetch only the named columns.
let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
    .where_(Post::published.eq(true))
    .only(&["id", "title"])
    .fetch(&pool).await?;

// .defer(...) — fetch everything except the named columns.
// Useful for "list view: skip body / metadata / large JSON".
let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
    .defer(&["body", "raw_html"])
    .fetch(&pool).await?;
```

**Semántica**: `.only(&[cols])` es un sinónimo de `.values_dict(cols)` — mismo IR, misma forma de retorno, punto de entrada separado para legibilidad con forma de Django. `.defer(&[cols])` calcula el complemento contra el schema del modelo (cada columna escalar del modelo EXCEPTO las listadas) y encamina al mismo camino.

**Salvedad — el tipo de retorno difiere del de Django.** Los `.only()` / `.defer()` de Django devuelven instancias `Model` parcialmente hidratadas donde los campos diferidos se cargan de forma perezosa al acceder al atributo. **Rustango** no tiene equivalente de la magia de descriptores de Python; la forma de retorno es `Vec<HashMap<String, SqlValue>>` (o `Vec<Vec<SqlValue>>` si intercambias por `.values_list(...)` en su lugar). La decodificación tipada de fila parcial está en cola para una futura entrega.

**Seguridad ante erratas**: `.defer(&["nope_col"])` expone `QueryError::UnknownField` en el momento de `.compile()` — la errata no se convierte silenciosamente en "proyecta todas las columnas". `.only(&[])` expone `QueryError::EmptyValuesProjection`; `.defer(&[])` es un no-op semántico (proyecta todas las columnas).

### Coincidencia con expresiones regulares

Compara una columna contra un patrón regex — los `__regex` / `__iregex` de Django. `.regex()` distingue mayúsculas/minúsculas, `.iregex()` no las distingue, y `.not_regex()` / `.not_iregex()` son las formas negadas.

```rust
use rustango::core::Column as _;

// Names starting with "al" (case-sensitive).
User::objects()
    .where_(User::name.regex("^al.*"))
    .fetch(&pool).await?;

// Names starting with "al" — case-insensitive.
User::objects()
    .where_(User::name.iregex("^al.*"))
    .fetch(&pool).await?;

// Negated: exclude names starting with "admin" (case-sensitive).
User::objects()
    .where_(User::name.not_regex("^admin"))
    .fetch(&pool).await?;

// Django-shape lookup-suffix form.
User::objects()
    .filter("name__iregex", "^bob")
    .fetch(&pool).await?;
```

**Emisión tri-dialecto**:

| Dialecto | Sensible a mayúsculas | Insensible a mayúsculas | Notas |
|---|---|---|---|
| PostgreSQL | `<col> ~ ?` / `<col> !~ ?` | `<col> ~* ?` / `<col> !~* ?` | Operadores POSIX nativos |
| MySQL | `` `col` REGEXP ? `` / `` `col` NOT REGEXP ? `` | `LOWER(`col`) REGEXP LOWER(?)` (negado envuelve en `NOT`) | Fallback con LOWER() para `i*` |
| SQLite | `"col" REGEXP ?` / `"col" NOT REGEXP ?` | `LOWER("col") REGEXP LOWER(?)` (negado envuelve en `NOT`) | Necesita la función de usuario `regexp` cargada en la conexión |

**SQLite requiere una función de usuario `regexp` registrada** — no está incorporada. sqlx-sqlite 0.8 **no** registra una por defecto. Dos caminos para habilitarla:

1. **Fácil** — habilita la característica cargo `regexp` de sqlx-sqlite, luego opta por ella en la conexión:
   ```rust
   use sqlx::sqlite::SqliteConnectOptions;
   let opts = SqliteConnectOptions::new()
       .filename("app.db")
       .with_regexp();  // gated on sqlx-sqlite/regexp
   ```
2. **Manual** — registra un closure de Rust vía `SqliteConnection::lock_handle()` + FFI cruda (`sqlite3_create_function_v2`).

Sin una, la consulta emite SQL `REGEXP` válido que SQLite rechaza en ejecución con `no such function: regexp` (limpio para el parser — `tests/regex_sqlite_live.rs` fija esto).

**El dialecto de patrón difiere entre backends.** PostgreSQL usa regex extendida POSIX; MySQL usa regex basada en ICU con su propio sabor; SQLite delega a lo que sea que implemente la función de usuario (típicamente el crate `regex` de Rust). Los patrones que se apoyan en sintaxis específica del dialecto (p. ej. los límites de palabra `\m` / `\M` de PG) no son intercambiables — mantente en el subconjunto portable (`^`, `$`, `.`, `*`, `+`, `?`, `[...]`, `()`, `|`) si el mismo modelo se consulta desde múltiples backends.

**Los valores no-string se rechazan en `.compile()`** — pasar `SqlValue::I64(42)` a `__regex` expone `QueryError::InvalidLookupValue { suffix: "regex", expected: "SqlValue::String(<regex pattern>)", … }` en lugar de convertir silenciosamente.

---

## Valores calculados y funciones de base de datos

Deja que la base de datos calcule cosas en lugar de traer filas a la aplicación, mutarlas y escribirlas de vuelta. `F("col")` se refiere a una columna por nombre (el objeto `F()` de Django), y los builders `funcs::*` envuelven funciones SQL escalares como `LOWER` o `COALESCE`. Juntos desbloquean tres patrones que el `.set()` / `.where_()` basado en valores simples no puede expresar:

### Incrementos atómicos (sin carrera de leer-modificar-escribir)

El clásico bug del contador — traer una fila, incrementar un campo, guardar — pierde actualizaciones cuando dos peticiones se ejecutan a la vez. `F("col") + 1` colapsa el viaje de ida y vuelta en un solo `UPDATE`, así que la base de datos mantiene el bloqueo de fila por ti:

```rust
use rustango::core::F;

Post::objects()
    .eq("id", post_id)
    .update()
    .set_expr("view_count", F("view_count") + 1_i64)
    .execute(&pool).await?;
```

Tri-dialecto: emite `views = ("views" + $1)` en PG, ``views = (`views` + ?)`` en MySQL, idéntico en SQLite. La aritmética se pone entre paréntesis para que las operaciones anidadas se mantengan sin ambigüedad: `F("a") + F("b") * 2`.

Operadores soportados: `+ - * / %` más `& | ^ << >>` (a nivel de bits; XOR en SQLite emite un claro `OpNotSupportedInDialect` ya que SQLite no tiene símbolo XOR).

### Comparar dos columnas en un filtro

Filtra una columna contra otra, no contra un literal — p. ej. `Reservation start_date < end_date` para validar la coherencia de una fila, o `Inventory available > reserved` para encontrar filas con capacidad:

```rust
use rustango::core::Column as _;

// `start_date < end_date` for every selected row.
let valid = Reservation::objects()
    .where_(Reservation::start_date.lt_expr(F("end_date")))
    .fetch(&pool).await?;

// Combine with literal predicates.
let oversold = Inventory::objects()
    .where_(Inventory::available.lt_expr(F("reserved")))
    .where_(Inventory::active.eq(true))
    .fetch(&pool).await?;
```

La familia `*_expr` — `eq_expr`, `ne_expr`, `lt_expr`, `lte_expr`, `gt_expr`, `gte_expr` — refleja los métodos literales `eq`, `ne`, … pero toma cualquier `impl Into<Expr>` en el lado derecho: referencias de columna simples (`F("col")`), aritmética (`F("price") * 2`), o resultados de función (siguiente sección).

### Funciones escalares — texto, matemáticas, manejo de NULL

`rustango::core::funcs` incluye builders para las funciones SQL más usadas. Las 17 disponibles hasta ahora:

| Grupo | Builders |
|---|---|
| **Texto** | `lower`, `upper`, `length`, `trim`, `ltrim`, `rtrim`, `concat`, `substr`, `replace` |
| **Matemáticas** | `abs`, `ceil`, `floor`, `round` (1 arg) / `round_to` (precisión de 2 args) |
| **NULL** | `coalesce`, `greatest`, `least`, `nullif` |

```rust
use rustango::core::funcs::{lower, upper, concat, coalesce, trim, abs, round};
use rustango::core::F;

// Normalize on write.
User::objects()
    .eq("id", id)
    .update()
    .set_expr("email", lower(trim(F("email"))))
    .execute(&pool).await?;

// Build a derived column from two FKs + a literal.
User::objects()
    .update()
    .set_expr(
        "display_name",
        concat([F("first").into(), " ".into(), F("last").into()]),
    )
    .execute(&pool).await?;

// First non-NULL fallback.
User::objects()
    .update()
    .set_expr(
        "label",
        coalesce([F("nickname").into(), F("username").into(), "anonymous".into()]),
    )
    .execute(&pool).await?;

// Function on the WHERE rhs.
User::objects()
    .where_(User::email_norm.eq_expr(lower(F("email_norm"))))
    .fetch(&pool).await?;

// Functions compose freely — `abs(round(F("score") * 100))` is one Expr.
Player::objects()
    .update()
    .set_expr("score_int", abs(round(F("score") * 100_f64)))
    .execute(&pool).await?;
```

### Comportamiento tri-dialecto

La mayoría de las funciones emiten SQL idéntico entre PG / MySQL / SQLite. Las formas divergentes se manejan por dialecto de forma transparente:

| Builder | PG | MySQL | SQLite |
|---|---|---|---|
| `concat([a, b])` | `CONCAT(a, b)` | `CONCAT(a, b)` | `(a \|\| b)` |
| `substr(s, 1, 3)` | `SUBSTRING(s FROM 1 FOR 3)` | `SUBSTRING(s, 1, 3)` | `SUBSTR(s, 1, 3)` |
| `greatest([a, b])` | `GREATEST(a, b)` | `GREATEST(a, b)` | `MAX(a, b)` escalar |
| `least([a, b])` | `LEAST(a, b)` | `LEAST(a, b)` | `MIN(a, b)` escalar |

### Pasar argumentos mixtos a una función

Las funciones que toman una lista de argumentos (como `concat`) aceptan cualquier iterable de `Expr`. Los arrays de Rust deben contener un solo tipo, así que una mezcla de `F` (columna) y `&str` (literal) no verificará por sí sola — llama a `.into()` una vez por elemento para elevar cada uno a un `Expr`:

```rust
concat([F("first").into(), " ".into(), F("last").into()])
//          ^^^^^^ each element lifted to Expr
```

O construye un `Vec<Expr>` y pásalo directamente — misma forma, mismo resultado.

### Salvedades

- **`length` byte-vs-carácter**: PG devuelve caracteres en `TEXT`/`VARCHAR`, MySQL devuelve **bytes** (usa el futuro builder `CharLength` del framework o envuelve en `CHAR_LENGTH` manualmente si necesitas conteos de caracteres entre dialectos).
- **`round(x, n)` en PG**: la forma de 2 args de PG requiere `numeric`, no `double`. O pasa una columna entera o castea el float primero; MySQL y SQLite aceptan cualquiera de los tipos.
- **`greatest([single_arg])` / `least([single_arg])` en SQLite**: no soportado — el `MAX(x)` de SQLite con un argumento es la forma *agregada*, no la escalar. El writer devuelve `OpNotSupportedInDialect`. PG y MySQL aceptan la forma de un solo argumento como un no-op que devuelve `x`. Envuelve con al menos un literal para mantenerte portable.
- **`substr` con inicio negativo**: PG trata un negativo como "empieza desde la posición de carácter N" (efectivamente lo limita a 0); MySQL y SQLite tratan el negativo como "cuenta desde el final". Evita inicios negativos en código portable.

### Funciones de fecha y hora

Los builders `now()`, `extract_*` y `trunc_*` trabajan sobre fechas y timestamps. Úsalos para consultas de cohortes, agregados por bucket de tiempo y estampar la hora actual en la escritura — todo en la base de datos, sin hacer viajes de ida y vuelta de filas a través de la aplicación.

```rust
use rustango::core::funcs::{
    now, trunc_date, trunc_month,
    extract_year, extract_month, extract_weekday,
};
use rustango::core::F;

// 1. Stamp server-side current time on write.
Post::objects()
    .eq("id", id)
    .update()
    .set_expr("published_at", now())
    .execute(&pool).await?;

// 2. Extract year / month / weekday into denormalized indexable
// columns so cohort + day-of-week queries are cheap.
Signup::objects()
    .update()
    .set_expr("bucket_year", extract_year(F("created_at")))
    .set_expr("bucket_month", extract_month(F("created_at")))
    .set_expr("weekday", extract_weekday(F("created_at")))
    .execute(&pool).await?;

// 3. Filter on the stored bucket — typed integer comparison, uses
// the index, portable across all three dialects.
let friday_signups = Signup::objects()
    .where_(Signup::weekday.eq(5_i64))            // 5 = Friday (0=Sun)
    .fetch(&pool).await?;

// 4. For range filters where you'd be tempted to write
// `created_at >= trunc_year(now())` directly: don't. The function
// builders for `Trunc*` return text on MySQL/SQLite (see caveats
// below), so a column-vs-trunc comparison in WHERE only behaves
// well on PG. Compute the boundary in Rust instead and pass it as a
// typed literal — works the same on every backend and uses the
// index on `created_at`:
use chrono::{Datelike, TimeZone};
let this_year = chrono::Utc::now().year();
let year_start = chrono::Utc.with_ymd_and_hms(this_year, 1, 1, 0, 0, 0).unwrap();

let recent = Order::objects()
    .where_(Order::created_at.gte(year_start))
    .fetch(&pool).await?;

// 5. `Trunc*` shines on the *write* side. `trunc_date` is the
// one trunc-family builder with identical SQL on every dialect
// (`DATE(x)`) — handy for grouping by day without the type-divergence
// caveat the year/month variants carry.
Order::objects()
    .update()
    .set_expr("day_bucket", trunc_date(F("created_at")))     // DATE column on every backend
    .set_expr("month_bucket", trunc_month(F("created_at")))  // see caveat
    .execute(&pool).await?;
// `month_bucket` should be `TIMESTAMPTZ` on PG and `VARCHAR(10)` /
// `TEXT` on MySQL/SQLite — parse client-side when reading if you
// need a typed `chrono::NaiveDate`.
```

**Emisión por dialecto:**

| Builder | PG | MySQL | SQLite |
|---|---|---|---|
| `now()` | `NOW()` | `NOW()` | `CURRENT_TIMESTAMP` |
| `extract_year(x)` | `CAST(EXTRACT(YEAR FROM x) AS INTEGER)` | `YEAR(x)` | `CAST(strftime('%Y', x) AS INTEGER)` |
| `extract_week(x)` ⚠ | `EXTRACT(WEEK FROM x)` — ISO 8601, rango 1–53 | `WEEK(x)` — inicio en domingo, rango **0**–53 | `strftime('%W', x)` — inicio en lunes, rango 00–53 |
| `extract_weekday(x)` | `CAST(EXTRACT(DOW FROM x) AS INTEGER)` | `(DAYOFWEEK(x) - 1)` | `CAST(strftime('%w', x) AS INTEGER)` |
| `extract_quarter(x)` | `EXTRACT(QUARTER FROM x)` | `QUARTER(x)` | **no soportado** — error |
| `trunc_date(x)` | `DATE(x)` | `DATE(x)` | `DATE(x)` |
| `trunc_year(x)` | `DATE_TRUNC('year', x)` → timestamp | `DATE_FORMAT(x, '%Y-01-01')` → **string** | `strftime('%Y-01-01', x)` → **string** |
| `trunc_month(x)` | `DATE_TRUNC('month', x)` → timestamp | `DATE_FORMAT(x, '%Y-%m-01')` → **string** | `strftime('%Y-%m-01', x)` → **string** |
| `trunc_day(x)` | `DATE_TRUNC('day', x)` → timestamp | `DATE(x)` → date | `date(x)` → text |

**Salvedades específicas de fecha/hora:**

- **El tipo de retorno de `trunc_year/month` diverge**: timestamp en PG, texto en MySQL/SQLite. Castea del lado de la aplicación al leer si necesitas un `chrono::NaiveDate` tipado — o guarda el bucket como un entero simple (`extract_year` + `extract_month`) y reconstrúyelo en código.
- **`extract_weekday` está normalizado a 0 = domingo** en los tres dialectos. El `DAYOFWEEK()` nativo de MySQL devuelve 1=domingo, así que el writer resta 1.
- **⚠ `extract_week` NO es portable.** PG devuelve números de semana ISO 8601 (inicio en lunes, rango 1–53); el `WEEK(x)` predeterminado de MySQL es inicio en domingo con rango **0**–53; el `strftime('%W')` de SQLite es inicio en lunes con rango 00–53. Para 2024-01-01 (un lunes), los tres backends devuelven `1`, `0` y `01` respectivamente. El código de un solo backend puede usarlo libremente; el código entre dialectos debería calcular el límite de la semana como un `chrono::DateTime` tipado en Rust y filtrar sobre la columna de timestamp en su lugar.
- **`extract_quarter` en SQLite da error** con `OpNotSupportedInDialect` — SQLite no tiene un token nativo de trimestre. O bien pon la característica detrás de un `cfg(not(sqlite))` o calcula vía `((extract_month - 1) / 3) + 1` en código de aplicación.
- **Manejo de zona horaria**: el `EXTRACT` de PG opera en la zona horaria de la columna; el `YEAR()` de MySQL opera en la zona horaria de la sesión (`SET time_zone = ...`); SQLite no tiene soporte real de TZ — trata todo como UTC. Usa `TIMESTAMPTZ` en PG, `DATETIME` en MySQL con la TZ de sesión establecida, strings ISO-8601 en SQLite.

### Expresiones CASE WHEN

Construye un `CASE WHEN … THEN … ELSE … END` de SQL con los builders `case()` / `.when()` / `value()` — los `Case`/`When` de Django. Úsalo para ordenamientos personalizados, columnas derivadas en `annotate`, valores predeterminados calculados en `update` y (combinado con `Sum`) agregados condicionales.

```rust
use rustango::core::case::{case, value};
use rustango::core::{Column as _, F};
use rustango::core::funcs::lower;

// Custom ordering — published posts first, drafts last.
Post::objects()
    .update()
    .set_expr(
        "priority",
        case()
            .when(Post::status.eq("published"), 0_i64)
            .when(Post::status.eq("review"), 1_i64)
            .when(Post::status.eq("draft"), 2_i64)
            .default(99_i64),
    )
    .execute(&pool).await?;

let ordered = Post::objects()
    .order_by(&[("priority", false), ("id", false)])
    .fetch(&pool).await?;

// Computed default on update — drafts get a lowercased title for
// the label, everything else uses the title verbatim.
Post::objects()
    .update()
    .set_expr(
        "label",
        case()
            .when(Post::status.eq("draft"), lower(F("title")))
            .default(F("title")),
    )
    .execute(&pool).await?;

// AND / OR composition in the WHEN predicate.
let viral = Post::status.eq("published").and(Post::views.gt(1_000_i64));
Post::objects()
    .update()
    .set_expr(
        "label",
        case()
            .when(viral, value("viral"))
            .when(Post::status.eq("published"), value("live"))
            .default(value("pending")),
    )
    .execute(&pool).await?;
```

**Forma del builder:**

- `case()` — inicia un builder.
- `.when(condition, then)` — añade una rama. `condition` es cualquier cosa `Into<WhereExpr>` (típicamente `Column::eq()`, `.and()`, `.or()`); `then` es cualquier cosa `Into<Expr>` (literal, `F()`, llamada de función, `case()` anidado).
- `.default(expr)` — establece la rama opcional `ELSE`. Omitirla produce un `CASE` que devuelve `NULL` para las filas sin coincidencia (estándar SQL).
- `.build()` o `.into()` — finaliza en un `Expr` para `set_expr` / `eq_expr` / `annotate`.
- `value(literal)` — azúcar al estilo Django para `Expr::Literal(...)`. Opcional — los literales simples se convierten vía `Into<Expr>`, pero `value("…")` se lee explícitamente como "esto es un literal de string, no una referencia de columna".

**Emisión tri-dialecto:**

`CASE WHEN … THEN … [ELSE …] END` es estándar SQL-92 — emitido de forma idéntica entre PG, MySQL y SQLite. Sin despacho por dialecto en el writer.

**Salvedades:**

- **Ramas vacías**: `case().build()` sin llamadas a `.when(...)` se rechaza en el momento de emisión con `SqlError::EmptyCaseBranches`. SQL requiere al menos una cláusula `WHEN`. Una condición `WHEN` vacía (p. ej. `WhereExpr::And(vec![])`) se rechaza con `SqlError::EmptyCaseWhenCondition` por la misma razón.
- **Unificación de tipos entre ramas**: cada dialecto elige un tipo común de los valores `THEN` y `ELSE`. Mezclar tipos (`THEN 1_i64` + `ELSE "string"`) puede lanzar un error de cast en tiempo de ejecución o convertir de forma sorprendente. Mantente en un solo tipo por `CASE`.
- **Rendimiento**: cada fila evalúa los predicados `WHEN` en orden hasta que uno coincide (gana la primera coincidencia, por fila). El costo crece con el número de ramas y el costo de los predicados. Para muchos mapeos de string fijos, un join contra una pequeña tabla de búsqueda puede ser más barato y legible.

### Subconsultas (EXISTS, IN, escalar)

Incrusta una consulta dentro de otra — los `Exists`, `Subquery` y `OuterRef` de Django. Estos builders cubren la mayoría de los patrones de "¿existe una fila relacionada?" y "¿está este valor en ese conjunto?":

| Builder | Forma | Úsalo para |
|---|---|---|
| `exists(qs)` | `EXISTS (SELECT … FROM …)` | "Autores que tienen al menos un libro" |
| `not_exists(qs)` | `NOT EXISTS (SELECT …)` | "Autores sin libros" (anti-join) |
| `in_subquery(col, qs)` | `<col> IN (SELECT …)` | "Posts en cualquier categoría pública" |
| `not_in_subquery(col, qs)` | `<col> NOT IN (SELECT …)` | Inverso del anterior |
| `subquery(qs)` | `(SELECT …)` como escalar | Valor predeterminado calculado en `set_expr` |
| `outer_ref(col)` | `"<outer_table>"."<col>"` | Referenciar la fila externa desde dentro de cualquiera de los anteriores |

```rust
use rustango::core::subquery::{exists, not_exists, in_subquery, outer_ref};
use rustango::core::{Column as _, WhereExpr};

// "Authors with no books" — the canonical anti-join. Build the inner
// queryset first so its compile() catches typos; embed via not_exists.
let no_books = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .compile()?;
let orphans = Author::objects()
    .where_raw(not_exists(no_books))
    .fetch(&pool).await?;

// "Authors who have a published book of more than 100 pages" — the
// inner predicate combines a correlation (outer_ref) with literal
// filters in the same WHERE.
let inner = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .where_(Book::status.eq("published"))
    .where_(Book::pages.gt(100_i64))
    .compile()?;
let long_writers = Author::objects()
    .where_raw(exists(inner))
    .fetch(&pool).await?;

// Compose EXISTS with an OR.
let inner = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .compile()?;
let featured = Author::objects()
    .where_raw(WhereExpr::Or(vec![
        Author::name.eq("Carol").into(),
        exists(inner),
    ]))
    .fetch(&pool).await?;
```

**La correlación anidada funciona.** OuterRef dentro de una subconsulta doblemente anidada se resuelve al ámbito envolvente *inmediato* — el writer mantiene una pila de ámbitos a medida que desciende, así que `EXISTS (Book WHERE id = outer.id AND EXISTS (Comment WHERE book_id = outer.id))` resuelve el `outer.id` interno a `Book.id`, no al `Author.id` más externo. Usa `outer_ref(...)` dos veces si realmente necesitas alcanzar dos ámbitos hacia arriba.

**Errores:**

- **`OuterRefOutsideSubquery`** — emitir `outer_ref("col")` en el nivel superior (no dentro de ningún envoltorio de subconsulta) es un error de programación. El writer lo señala ruidosamente con el nombre de la columna para que el sitio de llamada sea fácil de encontrar.

**Salvedades:**

- **Estrechamiento de proyección de `IN (SELECT …)`**: PG requiere estrictamente que el SELECT interno proyecte exactamente una columna para la forma `<col> IN (…)`. **Rustango** aún no incluye estrechamiento de proyección al estilo `.values("col")` (issue #62), así que el queryset interno siempre proyecta todas las columnas del modelo — lo que hace que `in_subquery` solo funcione hoy contra tablas cuyo modelo tiene una sola columna. Para el caso multi-columna, recurre a `exists(inner.where_(<outer col>.eq_expr(outer_ref(...))))` — tiene la misma semántica y no depende de la forma de la proyección.
- **El `subquery(...)` escalar requiere un interno de una columna y una fila**: el SQL emitido es `SET col = (SELECT …)` — si el interno produce más de una fila, la base de datos da error en tiempo de ejecución. Restríngelo vía `.limit(1)` y o bien estrecha la proyección (una vez que llegue) o diseña el interno en torno a una invariante de unicidad.
- **La validación en tiempo de compilación de la subconsulta vive en el queryset interno**: las erratas de columna se exponen en la llamada interna `queryset.compile()?`, no en el `compile()` de la consulta externa. Construye el interno primero y propaga `?`.

### Cuándo bajar a SQL crudo en su lugar

Los builders de arriba cubren los casos comunes. Para cosas que aún no expresan — `Cast`, búsqueda de texto completo, operadores de path de JSON, funciones de hash, trigonometría, funciones de ventana — ver la sección [Válvula de escape a SQL crudo](#raw-sql-escape-hatch) más abajo, o espera a los issues de seguimiento que extienden el mismo árbol de expresiones.

---

## Agregaciones

Cuenta, suma, promedia y agrupa filas. `.count()`, `.sum()`, `.avg()`, `.min()` y `.max()` devuelven un solo número; `.annotate(...)` más `.values(...)` construye consultas GROUP BY (los `aggregate` / `annotate` de Django). Los resultados de agregación vuelven como `Vec<HashMap<String, SqlValue>>` en lugar de structs tipados, ya que la forma es dinámica.

```rust
use rustango::sql::CounterPool as _;

// COUNT
let n = Post::objects()
    .where_(Post::status.eq("published"))
    .count(&pool).await?;

// SUM / AVG / MIN / MAX — string column name; each returns Option<U>
// (None when the filtered result set is empty).
let total_views = Post::objects().sum::<i64>("view_count", &pool).await?;
let avg_views = Post::objects().avg::<f64>("view_count", &pool).await?;
let max_views = Post::objects().max::<i64>("view_count", &pool).await?;

// Annotate + GROUP BY (issue #75 — Django-shape auto-inference)
use rustango::core::aggregates::{count_all, sum};

// "Posts per author" — `.values()` lists the GROUP BY columns.
let by_author = Sale::objects()
    .values(&["author_id"])
    .annotate("n", count_all().into())
    .compile()?;
let rows = rustango::sql::fetch_aggregate_dict(&pool, &by_author).await?;
// rows: Vec<HashMap<String, SqlValue>> — { author_id: 1, n: 3 }, …
```

### Cómo se infiere el GROUP BY

Rara vez escribes `GROUP BY` tú mismo — **Rustango** lo infiere de la forma de la consulta, igual que Django. Solo llamas a `.group_by(...)` para sobrescribir esa inferencia. La tabla muestra qué produce cada forma:

| Forma | Builder | `GROUP BY` resultante |
|---|---|---|
| **2 — values + agregado** | `.values(&["author_id"]).annotate("n", count_all().into())` | `GROUP BY "author_id"` |
| **3 — agregado suelto** | `.annotate("n", count_all().into())` | `GROUP BY` cada columna escalar no-agregada del modelo |
| **Solo ventana** | `.aggregate().annotate("rn", row_number()…)` | (sin `GROUP BY` — las funciones de ventana son por fila) |
| **Sobrescritura explícita** | `.aggregate().group_by("month").annotate(...)` | `GROUP BY "month"` — lo explícito gana |

El clasificador `AggregateExpr::is_aggregating()` distingue las variantes que colapsan filas (`Count` / `Sum` / `Avg` / `Max` / `Min` / `CountDistinct` / `StdDev*` / `Variance*` — más los envoltorios recursivos `Filtered` / `Coalesced`) de `Window`, que es por fila. Solo las variantes que agregan disparan la inferencia de Forma 3.

```rust
use rustango::core::aggregates::{count_all, sum};

// Shape 2 — "monthly revenue per author".
Sale::objects()
    .where_(Sale::status.eq("paid"))
    .values(&["author_id", "month"])
    .annotate("total", sum("amount").into())
    .compile()?;
// → SELECT "author_id", "month", SUM("amount")::bigint AS "total"
//   FROM "sale" WHERE "status" = $1
//   GROUP BY "author_id", "month"

// Shape 3 — a bare .annotate() with no .values(): rustango adds every
// non-aggregate scalar column of the model to the GROUP BY.
Post::objects()
    .annotate("n", count_all().into())
    .compile()?;
// → SELECT <every Post column>, COUNT(*) AS "n"
//   FROM "post" GROUP BY <every Post column>
```

**Salvedad de proyección pura.** `.values(cols)` *por sí solo* (sin anotación de agregado) **no** está soportado en v0.40 — `compile()` devuelve `QueryError::ValuesRequiresAggregate`. La proyección pura como dicts necesita un camino de writer separado (es un SELECT sin GROUP BY, decodificado en `Vec<HashMap>`) y está en cola para un seguimiento. Por ahora, usa el `QuerySet::fetch(...)` tipado para leer filas completas.

### Agregados condicionales y estadísticos

Cuenta o suma solo las filas que coinciden con una condición, provee un fallback para resultados vacíos, y calcula desviación estándar / varianza. Estos reflejan los `Count('id', filter=...)`, `Sum('price', default=0)` y `StdDev` de Django. Encadena `.filter(...)` y `.default(...)` a cualquier builder de agregado.

```rust
use rustango::core::aggregates::{avg, count, count_all, stddev, sum};
use rustango::core::Column as _;

let rows = Post::objects()
    .aggregate()
    // COUNT(*) FILTER (WHERE is_active AND status = 'published')
    .annotate(
        "active_published",
        count_all()
            .filter(Post::is_active.eq(true).and(Post::status.eq("published")))
            .into(),
    )
    // COALESCE(SUM(price) FILTER (WHERE status = 'published'), 0)
    //   — returns 0 instead of NULL when the queryset is empty.
    .annotate(
        "revenue_or_zero",
        sum("price")
            .filter(Post::status.eq("published"))
            .default(0_i64)
            .into(),
    )
    .annotate("avg_pages", avg("pages").into())
    .annotate("page_stddev", stddev("pages").into())
    .compile()?;
let result = rustango::sql::fetch_aggregate_dict(&pool, &rows).await?;
```

**Builders** en `rustango::core::aggregates`:

| Builder | SQL |
|---|---|
| `count(col)` | `COUNT(col)` |
| `count_all()` | `COUNT(*)` |
| `count_distinct(col)` | `COUNT(DISTINCT col)` |
| `sum(col)` / `avg(col)` / `max(col)` / `min(col)` | los de siempre |
| `stddev(col)` / `stddev_pop(col)` | `STDDEV_SAMP` / `STDDEV_POP` |
| `variance(col)` / `variance_pop(col)` | `VAR_SAMP` / `VAR_POP` |

Cada uno devuelve un `AggregateBuilder` con dos modificadores encadenables:

- `.filter(predicate)` — envuelve en `FILTER (WHERE predicate)`. El predicado es cualquier `WhereExpr` (`.eq()` / `.and()` tipado / `WhereExpr::Or(...)` crudo), así que se compone de la misma manera que un WHERE normal.
- `.default(value)` — envuelve en `COALESCE(..., value)` para que un queryset vacío devuelva el valor predeterminado en lugar de `NULL`.

Llamar a ambas cadenas como `Coalesced` fuera de `Filtered`: `COALESCE(SUM(col) FILTER (WHERE p), 0)`. El orden de la cadena no importa — `.filter(p).default(0)` y `.default(0).filter(p)` producen el mismo IR.

**Emisión tri-dialecto:**

| Característica | PG | MySQL | SQLite |
|---|---|---|---|
| `Count` / `Sum` / `Avg` / `Max` / `Min` / `CountDistinct` | ✓ | ✓ | ✓ |
| `StdDev` / `StdDevPop` / `Variance` / `VariancePop` | ✓ | ✓ (8.0+) | ✗ `SqlError::AggregateNotSupported` |
| `.filter(...)` — `FILTER (WHERE …)` nativo | ✓ | ✗ reescrito | ✓ (3.30+) |
| `.filter(...)` — fallback con `CASE WHEN` | — | ✓ `<agg>(CASE WHEN … THEN <arg> END)` | — |
| `.default(...)` — `COALESCE` | ✓ | ✓ | ✓ |

El writer aplica el cast int/float del dialecto (`::bigint`, `CAST(... AS SIGNED)`, etc.) alrededor de toda la expresión `FILTER` — `SUM(col)::bigint FILTER (...)` es un error de parseo de PG, así que la forma emitida es `(SUM(col) FILTER (...))::bigint`. La misma forma para `STDDEV_SAMP` / `VAR_SAMP` (devuelven NUMERIC en PG para entrada bigint).

**SQLite + StdDev/Variance:** SQLite no tiene agregados estadísticos incorporados, así que el writer rechaza con `SqlError::AggregateNotSupported { aggregate, dialect: "sqlite" }`. Calcula la fórmula de varianza en código de aplicación si se necesitan estadísticas portables (la misma postura que toma Django).

### Funciones de ventana

Calcula totales acumulados, rankings y deltas fila-sobre-fila sin colapsar filas — el `Window(expression, partition_by=, order_by=, frame=)` de Django. Ocho funciones (`row_number`, `rank`, `dense_rank`, `lag`, `lead`, `first_value`, `last_value`, `ntile`) más frames ROWS/RANGE. Cada backend que **Rustango** soporta (PG ≥ 9.0, MySQL ≥ 8.0, SQLite ≥ 3.25) incluye sintaxis nativa `OVER (…)`, así que la emisión es uniforme.

```rust
use rustango::core::aggregates::max;
use rustango::core::window::{lag, rank, row_number};

// "Rank users by score within each tenant" — the canonical
// integration target.
let q = User::objects()
    .aggregate()
    .group_by("id")
    .group_by("tenant_id")
    .group_by("name")
    .group_by("score")
    .annotate("_a", max("id").into())  // satisfies GROUP BY on the projection
    .annotate(
        "tenant_rank",
        rank().partition_by("tenant_id").order_by(&[("score", true)]).into(),
    )
    .order_by(&[("tenant_id", false), ("score", true)])
    .compile()?;
let rows = rustango::sql::fetch_aggregate_dict(&pool, &q).await?;

// Day-over-day delta via LAG with a default for the first row.
let q = Event::objects()
    .aggregate()
    .group_by("id")
    .group_by("day")
    .group_by("count")
    .annotate("_a", max("id").into())
    .annotate(
        "prev_count",
        lag("count", 1, Some(SqlValue::I64(0)))
            .partition_by("user_id")
            .order_by(&[("day", false)])
            .into(),
    )
    .compile()?;

// Stable row index per group for "show me row N" pagination.
let q = Post::objects()
    .aggregate()
    .group_by("id")
    .group_by("status")
    .group_by("created_at")
    .annotate("_a", max("id").into())
    .annotate(
        "rn",
        row_number()
            .partition_by("status")
            .order_by(&[("created_at", true)])
            .into(),
    )
    .compile()?;
```

**Builders** en `rustango::core::window`:

| Builder | SQL | Args |
|---|---|---|
| `row_number()` | `ROW_NUMBER()` | — |
| `rank()` | `RANK()` | — |
| `dense_rank()` | `DENSE_RANK()` | — |
| `ntile(buckets)` | `NTILE(buckets)` | conteo de buckets |
| `lag(col, offset, default)` | `LAG(col, offset, default?)` | columna + offset + default opcional |
| `lead(col, offset, default)` | `LEAD(col, offset, default?)` | columna + offset + default opcional |
| `first_value(col)` | `FIRST_VALUE(col)` | columna |
| `last_value(col)` | `LAST_VALUE(col)` | columna |

Cada uno devuelve un `WindowBuilder` con tres modificadores encadenables:

- `.partition_by("col")` — añade una columna `PARTITION BY`. Llama varias veces para particionamiento multi-columna.
- `.order_by(&[("col", desc)])` — añade columnas `ORDER BY` (`desc = true` → DESC).
- `.frame(WindowFrame { kind, start, end })` — establece la cláusula de frame `ROWS`/`RANGE` opcional. `FrameBoundary::UnboundedPreceding` / `Preceding(n)` / `CurrentRow` / `Following(n)` / `UnboundedFollowing`.

El builder desciende vía `Into<AggregateExpr>` para que las funciones de ventana se compongan con `annotate()`. `Into<Expr>` también está implementado (el slot a nivel de IR para expresiones de ventana), pero **cada backend que **Rustango** soporta restringe las funciones de ventana a la lista `SELECT` y a la cláusula `ORDER BY` de una consulta** — no pueden aparecer en `WHERE` / `HAVING` / `GROUP BY` / `UPDATE SET` / `JOIN ON` / `RETURNING`. El writer no bloquea la emisión por esto, así que `set_expr("col", row_number())` compila a un SQL que la base de datos rechaza en ejecución. Construye las expresiones de ventana a través de `annotate()`; recurre a una subconsulta si necesitas alimentar un resultado de ventana a un filtro WHERE o a un UPDATE.

**Trampa del frame predeterminado de `LAST_VALUE`:**

Un `last_value(col).order_by(&[("x", false)])` simple emite `LAST_VALUE("col") OVER (ORDER BY "x")` y parece que debería devolver el último `col` de la partición. No lo hace — el frame de ventana *predeterminado* de SQL es `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`, así que `LAST_VALUE` devuelve el valor de la **fila actual**, no la última fila de la partición. Para obtener el comportamiento intuitivo de "última fila de la partición", pasa un frame no acotado explícito:

```rust
use rustango::core::{FrameBoundary, FrameKind, WindowFrame};

last_value("score")
    .partition_by("tenant_id")
    .order_by(&[("created_at", true)])
    .frame(WindowFrame {
        kind: FrameKind::Rows,
        start: FrameBoundary::UnboundedPreceding,
        end: Some(FrameBoundary::UnboundedFollowing),
    })
```

`first_value` no tiene esta trampa — el inicio del frame predeterminado coincide con el inicio de la partición, así que la respuesta intuitiva sale sola.

**Salvedad de annotate (hasta que llegue el issue #75):**

`annotate()` vive en el aggregate-builder que requiere `GROUP BY` para proyectar columnas escalares por fila junto a los agregados. Para proyectar resultados de función de ventana junto a columnas de fila hoy, lista cada columna de fila que quieras devolver en llamadas `.group_by(...)` y `annotate("_a", max("id").into())` como un placeholder no-op para mantener estable la identidad de la fila. El issue #75 (auto-inferencia de GROUP BY) trae una forma más limpia.

**Cláusulas de frame:**

```rust
use rustango::core::{FrameBoundary, FrameKind, WindowFrame};

// Running total over the last 7 rows:
let frame = WindowFrame {
    kind: FrameKind::Rows,
    start: FrameBoundary::Preceding(6),
    end: Some(FrameBoundary::CurrentRow),
};

// Centered 11-row window:
let frame = WindowFrame {
    kind: FrameKind::Rows,
    start: FrameBoundary::Preceding(5),
    end: Some(FrameBoundary::Following(5)),
};
```

**Emisión tri-dialecto:**

`<fn>(args) OVER (PARTITION BY … ORDER BY … [frame])` es estándar SQL — idéntico entre PG, MySQL 8+ y SQLite 3.25+. La única peculiaridad: `LAG` / `LEAD` / `NTILE` requieren offsets/buckets enteros en PG (vincularlos como un parámetro bigint `$N` causa `function lag(bigint, bigint, bigint) does not exist`). El writer incrusta los literales enteros directamente en el SQL para esos slots; los argumentos de valor predeterminado se vinculan normalmente.

**Salvedades:**

- **`FILTER` + `Window` aún no soportado**: combinar `.filter(...)` con una función de ventana levanta `SqlError::NestedAggregateWrapper { wrapper: "Filtered(Window)" }` — la sintaxis subyacente varía según el tipo de función (PG permite `agg_fn() FILTER (WHERE …) OVER (…)` para funciones agregadas-de-ventana pero no para las de ranking), y al writer no se le ha enseñado el despacho. Registrado para un seguimiento si surge demanda.
- **`PercentRank` / `CumeDist` / `NthValue`** no están en v1 — el conjunto completo de Django es más grande. v1 incluye las 8 variantes más usadas; las tres faltantes pueden añadirse incrementalmente con la misma forma de builder.

### Filtrar sobre agregados (HAVING)

Una llamada `.filter(...)` después de `.annotate(...)` cae en `WHERE` o en `HAVING`, dependiendo de si el nombre coincide con un alias de agregado — exactamente el comportamiento de Django. Así que filtrar sobre una columna real añade un `WHERE`, mientras que filtrar sobre una anotación como `post_count` añade un `HAVING`:

```rust
use rustango::core::aggregates::count_all;
use rustango::core::Op;

// "Authors with > 10 published posts" — the canonical pattern.
// status='published' is on the model       → routes to WHERE.
// post_count > 10 references the annotation → routes to HAVING.
let q = Post::objects()
    .aggregate()
    .group_by("author_id")
    .annotate("post_count", count_all().into())
    .filter("status",     Op::Eq, "published")
    .filter("post_count", Op::Gt, 10_i64)
    .compile()?;
let rows = rustango::sql::fetch_aggregate_dict(&pool, &q).await?;
```

Emite, en PG:

```sql
SELECT "author_id", COUNT(*) AS "post_count"
FROM "post"
WHERE "status" = $1
GROUP BY "author_id"
HAVING COUNT(*) > $2
```

**La expresión de agregado se eleva a HAVING, no al alias del SELECT.** PG prohíbe estrictamente los alias en HAVING (solo se resuelve la expresión); MySQL + SQLite son más permisivos. El writer emite la forma elevada de manera uniforme en los tres para que la misma consulta funcione en todas partes.

**El orden de la cadena importa en v1.** Llama a `.annotate(alias, ...)` ANTES del correspondiente `.filter(alias, ...)`. Si el orden se invierte, `filter()` busca en un registro de anotaciones vacío y encamina a `WHERE` — y el validador `resolve_pending` expone `UnknownField` en `compile()` porque el alias no es una columna real del modelo. Django difiere esta resolución al momento de construcción de la consulta; un seguimiento en v0.50 podría igualar esa postura.

**Brecha del validador (coincide con la postura existente de agregados)**: los predicados HAVING encaminados por alias omiten el recorrido de columnas del schema del modelo. Los alias con errata se exponen en la base de datos, no en `compile()`. La misma brecha que `Sum("typo_col")` — preexistente y ortogonal.

**Ops soportadas en `.filter()` encaminado por alias** (issue #87): el conjunto de comparación binaria (`Op::Eq` / `Ne` / `Lt` / `Lte` / `Gt` / `Gte`) **más** los predicados estándar SQL-92 que se componen contra un LHS de agregado de manera uniforme en todos los backends — `Op::In` / `NotIn`, `Between`, `IsNull`, `Like` / `NotLike`, `ILike` / `NotILike`. Cada uno emite la forma predecible:

```rust
use rustango::core::{Op, SqlValue};

// HAVING COUNT(*) IN ($1, $2, $3)
Post::objects()
    .aggregate()
    .group_by("author_id")
    .annotate("post_count", count_all().into())
    .filter("post_count", Op::In, SqlValue::List(vec![5_i64.into(), 10_i64.into(), 20_i64.into()]))
    .compile()?;

// HAVING COUNT(*) BETWEEN $1 AND $2
.filter("post_count", Op::Between, SqlValue::List(vec![5_i64.into(), 10_i64.into()]))

// HAVING COUNT(*) IS NULL  /  IS NOT NULL  (bool: true = IS NULL)
.filter("post_count", Op::IsNull, SqlValue::Bool(false))

// HAVING MAX("name") LIKE $1  /  ILIKE $1 (PG) / LOWER(MAX("name")) LIKE LOWER(?) (MySQL/SQLite)
.filter("max_name", Op::ILike, "SMITH%")
```

Las ops restantes — la familia de ops JSON (`JsonContains` / `JsonContainedBy` / `JsonHasKey` / `JsonHasAnyKey` / `JsonHasAllKeys`) y la igualdad null-safe (`IsDistinctFrom` / `IsNotDistinctFrom`) — aún necesitan writers específicos de dialecto que tomen un `&str` para el LHS, así que rechazan en `.compile()` con `QueryError::HavingOpNotSupported { alias, op }`. Para esas, baja a la forma tipada `.having(<TypedExpr>)` con un predicado pre-construido.

**Inflado del vector de parámetros con agregados no triviales**: cuando el alias apunta a una anotación `Filtered { Count, filter: pred }` o `Coalesced { Sum, default: 0 }`, el writer eleva la **expresión de agregado completa** a HAVING — incluyendo sus predicados internos y valores predeterminados. Sus literales vinculados obtienen slots de parámetro nuevos en HAVING, separados de la emisión en la lista SELECT. Concretamente:

```text
SELECT … COUNT(*) FILTER (WHERE "status" = $1) AS "published_count" …
HAVING COUNT(*) FILTER (WHERE "status" = $2) > $3
              -- "published" bound twice (once at $1, once at $2)
```

La semántica SQL no cambia (vuelven los mismos conteos de filas), pero `stmt.params.len()` crece por cada llamada `.filter()` que apunte a un alias no trivial. Para alias `COUNT(*)` (sin literales internos) el inflado es cero. Documéntalo si tu suite de tests fija conteos de parámetros.

---

## Joins y precarga de filas relacionadas

Trae el objetivo de una foreign key junto con la fila principal en una sola consulta, para no disparar una consulta extra por fila (el problema N+1). `.select_related("author")` es el `select_related` de Django / la carga anticipada de Eloquent. Un campo `ForeignKey<T>` entonces llega ya poblado en lugar de necesitar una búsqueda separada.

```rust
let posts = Post::objects()
    .select_related("author")              // JOIN posts.author -> authors.id
    .fetch(&pool).await?;

for post in &posts {
    let author = post.author.value().unwrap();   // already loaded, no DB round-trip
    println!("{} by {}", post.title, author.name);
}
```

`select_related` resuelve los campos FK en el momento de compilación del queryset. El campo `ForeignKey<T>` del padre pasa de `Unloaded(pk)` a `Loaded { pk, value }`.

Para FKs inversas (parent.children), usa el método `_set` generado por la macro:

```rust
let author_posts = author.post_set(&pool).await?;
```

### Joins personalizados

Cuando el join no está impulsado por una foreign key — un predicado personalizado, un join no-equi, INNER en lugar de LEFT, un self-join, o unir por una columna que no es PK — usa `.join(Join { … })`. Su campo `on` toma cualquier `WhereExpr`, así que `and()` / `or()` / `Not` / llamadas de función / columna-vs-columna / filtros literales se componen todos libremente.

```rust
use rustango::core::joins::aliased;
use rustango::core::{Join, JoinKind, Op, WhereExpr};

// "Posts that have at least one APPROVED comment" — INNER JOIN with
// an extra predicate inside the ON. Posts with no approved comment
// drop out; LEFT JOIN would keep them.
Post::objects()
    .join(Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::And(vec![
            // Column-on-column condition — both sides aliased.
            WhereExpr::ExprCompare {
                lhs: aliased("c", "post_id"),
                op: Op::Eq,
                rhs: aliased("post", "id"),
            },
            // Bare Filter — unqualified columns inside `on` resolve
            // to the joined alias ("c"), so this becomes
            // `"c"."is_approved" = $N`.
            Comment::is_approved.eq(true).into(),
        ]),
        project: vec![],
    })
    .fetch(&pool).await?;
```

**Reglas de calificación de columna dentro de `on`:**

- **Columnas `Filter` / `ColumnFilter` simples + referencias de columna `F()`** se resuelven al alias del join (`<alias>` que pasaste). Esa es la lectura natural porque la mayor parte de un predicado ON trata sobre la tabla unida.
- **`aliased(alias, col)`** emite `"<alias>"."<col>"` explícitamente — usa esto para referencias cruzadas de vuelta a la tabla externa (`aliased("<outer_table>", "<col>")`) o a un alias previamente unido.
- **`WhereExpr::ExprCompare { lhs, op, rhs }`** es la forma correcta para comparaciones columna-vs-columna entre tablas, ya que ambos lados toman cualquier `Expr`.

> ⚠️ **PATRÓN PELIGROSO — filtros tipados del modelo EXTERNO dentro de `on`.**
> `Post::status.eq("draft").into()` produce un `WhereExpr::Predicate(Filter { column: "status", ... })` y **descarta la etiqueta del modelo `Post`** en el límite `Into<WhereExpr>`. La regla de auto-calificación de arriba entonces encamina mal ese filtro al **alias del join**, no a `Post`. Obtienes `"<joined_alias>"."status" = $N` — tabla equivocada — y el compilador no puede detectarlo. **Usa [`joins::col_filter`] para predicados contra cualquier columna cuya tabla no sea el alias predeterminado del join:**
>
> ```rust
> use rustango::core::joins::{aliased, col_filter};
> use rustango::core::Op;
>
> // SAFE: explicit alias on the LHS.
> col_filter("post", "status", Op::Eq, "draft")
> ```
>
> Reserva los filtros tipados simples (`Comment::is_approved.eq(true).into()`) solo para columnas del modelo UNIDO — nunca para columnas del modelo externo.

**Soporte tri-dialecto de JoinKind:**

| Tipo | PG | MySQL | SQLite |
|---|---|---|---|
| `Inner` | ✓ | ✓ | ✓ |
| `Left` (predeterminado) | ✓ | ✓ | ✓ |
| `Right` | ✓ | ✓ | ✗ `SqlError::JoinKindNotSupported` |
| `Full` | ✓ | ✗ | ✗ |

`Right` es fácil de sortear — intercambia los operandos y usa `Left`. `Full` en MySQL se emula normalmente con `(LEFT JOIN) UNION (RIGHT JOIN)` si realmente lo necesitas.

**Otros errores en tiempo de emisión:**

- **Predicado `on` vacío** (`WhereExpr::And(vec![])` o sin `ExprCompare`s) se rechaza con `SqlError::EmptyJoinOnCondition`. SQL requiere al menos un predicado booleano dentro de `ON`; la abreviatura auto-`true` del WHERE de nivel superior no aplica aquí.

**`project` es actualmente dato muerto en joins ad-hoc.**

El campo `Join.project` le dice al writer que emita columnas `<alias>"."<col>" AS "<alias>__<col>"` en la lista SELECT. Hoy solo `select_related` realmente las decodifica (vía el decoder de fila completa del objetivo de la FK); los joins ad-hoc emiten las columnas pero el decoder `Vec<MainModel>` las ignora, así que poblar `project` en un join ad-hoc solo añade bytes al cable. Déjalo como `vec![]` hasta que lleguen el estrechamiento de proyección + la decodificación de tuplas.

**Cuándo recurrir a joins ad-hoc:**

| Necesidad | Herramienta |
|---|---|
| Traer filas relacionadas junto con la fila principal | `select_related` (forma de Django) |
| Filtrar filas principales por un predicado de tabla relacionada | `exists(...)` / `not_exists(...)` |
| Filtrar vía INNER en lugar de LEFT, o con predicados ON extra | `.join(...)` |
| Self-join (p. ej. `employee.manager_id = manager.id`) | `.join(...)` |
| Anti-join (filas en A SIN coincidencia en B) | `not_exists(...)` |

`select_related` sigue siendo la herramienta correcta cuando el join es "sigue esta FK y proyecta todas sus columnas". Los joins ad-hoc son la válvula de escape cuando necesitas: clave de join que no es FK, INNER en lugar de LEFT, un predicado extra dentro del ON, o un self-join.

[`joins::col_filter`]: https://docs.rs/rustango/latest/rustango/core/joins/fn.col_filter.html

[`WhereExpr`]: https://docs.rs/rustango/latest/rustango/core/enum.WhereExpr.html

---

## Guardar solo algunos campos

Escribe solo los campos que cambiaste en lugar de cada columna — el `save(update_fields=[...])` de Django. Un save normal reescribe cada columna que no es PK; `save_partial(&[...], &pool)` reescribe solo las que nombras.

```rust
let mut post = Post::objects().fetch(&pool).await?.pop().unwrap();
post.title = "new title".into();
post.save_partial(&["title"], &pool).await?;  // SET "title" = $1
                                                  // — leaves body, status, views untouched
```

Dos motivaciones:

* **Rendimiento.** Las filas anchas con columnas `TEXT` / `JSON` / `bytea` pagan por revincular y reescribir cada campo en cada `save()` incluso cuando solo uno mutó. `save_partial` mantiene la cláusula `SET` en exactamente lo que cambió.
* **Seguridad ante concurrencia.** Cuando dos escritores divergen tras una lectura compartida, el perdedor sobrescribe silenciosamente las ediciones del ganador en los campos que no tocó. Nombrar solo el campo que realmente cambiaste preserva el trabajo del otro escritor en todo lo demás.

```rust
// Writer A — flips title.
a.title = "from-A".into();
a.save_partial(&["title"], &pool).await?;

// Writer B — started from the same read, flips status.
// B's local `title` is stale, but it's not in the list, so A's
// write survives.
b.status = "from-B".into();
b.save_partial(&["status"], &pool).await?;
```

**Los nombres de campo son campos del struct del lado de Rust**, no columnas SQL — `["author_id"]` (no `["author"]` para un campo tipo FK). Los nombres de campo desconocidos devuelven `ExecError::Query(QueryError::UnknownField)`. Una lista vacía es un no-op (devuelve `Ok(())` y registra un `tracing::warn!`), coincidiendo con la semántica de "nada que hacer" de Django. Los modelos auditados (`#[rustango(audit(...))]`) estrechan la instantánea del log de auditoría al mismo conjunto de columnas — el log refleja exactamente lo que se escribió.

**Nota sobre auto-PK.** `save_partial` es solo-UPDATE; llamarlo sobre una PK `Auto::Unset` es un error de usuario (usa `insert_pool` / `save_pool` para ese caso). A diferencia de `save_pool` que auto-despacha `Unset → insert_pool`, este método asume que ya has insertado.

### Lista de campos verificada en tiempo de compilación

La forma con claves de string de arriba se adapta a listas de campos dinámicas (formularios de admin, payloads de API). Cuando la lista es fija en tu código, `save_partial_typed((Post::title, ...), &pool)` detecta campos mal escritos o renombrados en **tiempo de compilación** en lugar de en tiempo de ejecución:

```rust
post.save_partial_typed((Post::title, Post::slug), &pool).await?;
//                       ──────────  ──────────
//                       title_col   slug_col   ← distinct ZSTs
```

Cada `Post::<field>` es su propio tipo de tamaño cero — un slice homogéneo (`&[Post::title, Post::slug]`) no verifica en Rust, así que la API toma una **tupla** en su lugar. Las llamadas de un solo campo usan el idioma de la coma final: `(Post::title,)`. Las tuplas se soportan desde aridad 1 hasta 12 — más allá de eso, baja a `save_partial(&[&str], _)`.

Las tuplas entre modelos son un **error de compilación** — `(Post::title, Author::name)` falla la cota de trait `TypedFieldList<Post>` porque el `Column::Model` de `Author::name` es `Author`. Este es el valor principal sobre la forma con claves de string: los refactors de renombrado sobre un nombre de columna se exponen en el sitio de llamada tipado, no en tiempo de ejecución.

Internamente desciende a `save_partial` — mismo estrechamiento de auditoría, misma restricción `Auto::Unset`, misma semántica de no-op para lista vacía.

---

## Operaciones en lote

> **Trampa — las ops en lote omiten los hooks por fila.** `bulk_insert`, el
> `.update().execute()` de queryset, y `.delete()` se ejecutan como SQL basado en conjuntos: **no**
> disparan señales, no escriben el rastro de auditoría, no encaminan a través del borrado lógico, ni ejecutan
> validación por fila. Úsalas por velocidad; baja a `save()` / `delete()` por fila
> cuando necesites esos efectos secundarios.

Inserta, actualiza o elimina muchas filas en una sentencia en lugar de una por fila — los `bulk_create`, `QuerySet.update()` y `QuerySet.delete()` de Django. El import `as _` trae los métodos de un trait al ámbito sin nombrar el trait directamente.

```rust
// Bulk INSERT — rows FIRST (a `&mut [Self]`), executor/pool second.
let mut rows = [p1, p2, p3];
Post::bulk_insert_on(&mut rows, &pool).await?;

// Bulk UPDATE — applies the same set to every matched row. `.set`
// takes a string column name.
Post::objects()
    .where_(Post::status.eq("draft"))
    .where_(Post::created_at.lt(thirty_days_ago))
    .update()
    .set("status", "archived")
    .execute_on(&pool).await?;

// Bulk DELETE
Post::objects()
    .where_(Post::deleted_at.is_not_null())
    .delete_on(&pool).await?;
```

---

## Insertar o actualizar (upsert)

Inserta una fila, o actualízala si ya existe una fila con la misma clave — el `update_or_create` de Django / el `upsert` de Rails. Emite el `ON CONFLICT … DO UPDATE` nativo de la base de datos.

El `.upsert_on(executor)` de instancia única entra en conflicto sobre la **primary key**: con una PK `Auto::Unset` el servidor asigna una clave nueva (equivalente a `insert`); con una PK `Auto::Set` la fila se inserta si está ausente o se sobrescriben todas las columnas que no son PK si está presente.

```rust
// Upsert on the PK — INSERT, or UPDATE every non-PK column if the
// PK already exists.
post.upsert_on(&pool).await?;
```

Para hacer upsert sobre una clave única arbitraria (el `bulk_create(update_conflicts=True, unique_fields=…, update_fields=…)` de Django), usa el helper en lote — toma las filas, las columnas objetivo del conflicto, las columnas a actualizar en conflicto, y el pool AL FINAL:

```rust
// ON CONFLICT (external_id) DO UPDATE SET title = EXCLUDED.title
Post::bulk_upsert_pool(
    &[post],
    &["external_id"],          // conflict target (unique key)
    &["title"],                // columns to overwrite on conflict
    &pool,
).await?;
```

---

## Transacciones

> **Trampa — no mezcles llamadas `&pool` dentro de una transacción.** Cada llamada
> entre `pool.begin()` y `commit` debe apuntar al handle de la transacción
> (`&mut *tx`). Un `&pool` / `fetch()` / `save_on(&pool)` perdido saca una
> *segunda* conexión y puede provocar un deadlock del pool bajo carga. Enhebra la `tx`
> por todo, o usa `rustango::sql::atomic`.

Ejecuta varias escrituras como una unidad que o bien todas tienen éxito o todas hacen rollback — el `transaction.atomic()` de Django. Abre una con `pool.begin()` y ejecuta cada sentencia contra la conexión de la transacción vía los métodos `_on` (`fetch_on`, `save_on`), para que el trabajo caiga en la transacción en curso en lugar de una nueva conexión del pool.

```rust
let mut tx = pool.begin().await?;

let mut a = Account::objects()
    .where_(Account::id.eq(1))
    .fetch_on(&mut *tx).await?
    .pop().unwrap();
let mut b = Account::objects()
    .where_(Account::id.eq(2))
    .fetch_on(&mut *tx).await?
    .pop().unwrap();

a.balance -= 100;
b.balance += 100;
a.save_on(&mut *tx).await?;
b.save_on(&mut *tx).await?;

tx.commit().await?;
```

Descarta la `tx` sin llamar a `commit()` (p. ej. en un retorno anticipado con `?`) y la transacción hace rollback. Para un hook post-commit (el `transaction.on_commit` de Django) recurre al helper de estilo closure `rustango::sql::atomic(&pool, |tx| Box::pin(async move { … }))`, que auto-confirma en `Ok` y auto-hace-rollback en `Err`.

---

## Muchos-a-muchos

Relaciona muchas filas con muchas otras a través de una tabla de unión — el `ManyToManyField` de Django. Declara la relación en el modelo, luego usa el accessor generado para añadir, quitar, establecer o listar los ids enlazados.

```rust
#[rustango(
    table = "posts",
    m2m(name = "tags", to = "tags", through = "post_tags",
        src = "post_id", dst = "tag_id"),
)]
pub struct Post { ... }
```

Usa el accessor auto-generado:

```rust
let tag_ids: Vec<i64> = post.tags_m2m().all(&pool).await?;
post.tags_m2m().add(42, &pool).await?;
post.tags_m2m().remove(42, &pool).await?;
post.tags_m2m().set(&[1, 2, 3], &pool).await?;        // replace all
post.tags_m2m().clear(&pool).await?;
let has = post.tags_m2m().contains(42, &pool).await?;
```

La tabla de unión (`post_tags`) es auto-creada por `make_migrations` con una PK compuesta + dos FKs `ON DELETE CASCADE`. Actualmente la tabla de unión tiene solo las dos columnas FK — para columnas extra (added_by, order, created_at) definirás un Model separado y lo recorrerás manualmente hasta que llegue el "custom through model".

---

## JSON / JSONB

Almacena y consulta un documento JSON en una columna — el `JSONField` de Django. Declara el campo como `serde_json::Value` (el tipo JSON genérico), luego consulta dentro de él con `json_contains` o un filtro de path.

```rust
#[derive(Model)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(default = r#"'{}'::jsonb"#)]
    pub data: serde_json::Value,
}
```

Consulta el contenido JSON:

```rust
use rustango::core::{Expr, Op, SqlValue, WhereExpr};
use rustango::core::funcs::json_path;
use rustango::core::F;

let with_email = Event::objects()
    .where_(Event::data.json_contains(serde_json::json!({"email_set": true})))
    .fetch(&pool).await?;

// Path extract — `json_path(F("data"), &["type"], true)` builds the
// `data ->> 'type'` text-extract LHS; compare it via `where_raw`.
let typed = Event::objects()
    .where_raw(WhereExpr::ExprCompare {
        lhs: json_path(F("data"), &["type"], true),
        op: Op::Eq,
        rhs: Expr::Literal(SqlValue::String("user.created".into())),
    })
    .fetch(&pool).await?;
```

Lee/escribe tipos de Rust vía `serde_json::from_value` / `to_value`.

---

## Borrado lógico (soft delete)

Marca una fila como eliminada estableciendo un timestamp en lugar de removerla — como el `django-safedelete` de Django o los `SoftDeletes` de Laravel. Marca la columna de timestamp con el atributo `#[rustango(soft_delete)]` (una anotación de derive que le dice a la macro cómo tratar el campo):

```rust
#[derive(Model)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}
```

Uso:

```rust
post.soft_delete_on(&pool).await?;     // sets deleted_at = NOW()
post.restore_on(&pool).await?;          // sets deleted_at = NULL

// Default queries DO include soft-deleted rows. Filter explicitly:
let live = Post::objects().where_(Post::deleted_at.is_null()).fetch(&pool).await?;
```

El botón "Delete" del admin encamina automáticamente a `soft_delete_on` para cualquier modelo que tenga la columna. El auto-filtro (exclusión predeterminada) está en la hoja de ruta de la v0.21.

---

## Rastro de auditoría

Registra quién cambió qué campos y cuándo, automáticamente en cada save y delete — como el `django-simple-history` de Django o los paquetes de auditing de Laravel. Anota el modelo con los campos a rastrear:

```rust
#[derive(Model)]
#[rustango(audit(track = "title, body, status"))]
pub struct Post { ... }
```

Cada save/delete escribe una fila en `rustango_audit_log` con un diff JSONB `before / after` para los campos listados. Establece la fuente por petición:

```rust
use rustango::audit::{with_source, AuditSource};

with_source(
    AuditSource::User { id: user_id.to_string() },
    async {
        post.save_on(&pool).await
    },
).await?;
```

El panel de historial por fila del admin lee de esta tabla; el feed entre modelos está en `/__audit`.

Limpieza:

```rust
rustango::audit::cleanup_older_than(&pool, 90).await?;       // delete > 90 days
rustango::audit::cleanup_keep_last_n(&pool, 50).await?;      // keep most recent 50/row

// CLI
manage audit-cleanup --days 90
manage audit-cleanup --keep-last 50 --tenant acme
```

---

## Válvula de escape a SQL crudo

Baja a SQL escrito a mano cuando el query builder no puede expresar lo que necesitas — el `Model.objects.raw()` / `connection.cursor()` de Django. Las macros de `sqlx` ejecutan una consulta y decodifican el resultado en una tupla, un `Model` tipado, o nada:

```rust
use rustango::sql::sqlx;

// Raw query → typed rows
let rows = sqlx::query_as::<_, (i64, String)>("SELECT id, title FROM posts WHERE views > $1 ORDER BY views DESC")
    .bind(1000)
    .fetch_all(&pool)
    .await?;

// Raw with model decoding
let posts: Vec<Post> = sqlx::query_as::<_, Post>("SELECT * FROM posts WHERE complicated_condition")
    .fetch_all(&pool)
    .await?;

// Raw without rows (DDL / DML)
sqlx::query("REINDEX TABLE posts").execute(&pool).await?;
```

Para SQL crudo programático dentro de la capa de consulta de **Rustango** (tri-dialecto; toma el SQL, un `Vec<SqlValue>` de binds, y luego el pool AL FINAL, y devuelve `Vec<T>`):

```rust
use rustango::sql::raw_query_pool;

let rows = raw_query_pool::<(i64,)>(
    "SELECT COUNT(*) FROM posts WHERE complicated",
    vec![],
    &pool,
).await?;
let count = rows.first().map(|r| r.0).unwrap_or(0);
```

---

## Carga perezosa de FK

Una foreign key empieza conteniendo solo el id relacionado (`Unloaded`), y traes la fila relacionada completa solo cuando la pides — el acceso perezoso a objetos relacionados de Django. Haz `match` sobre el `ForeignKey` para manejar ambos estados, o llama a `.get(&pool)` para cargarlo bajo demanda. Para un lote entero, usa `select_related` (arriba) para precargarlos en una consulta y saltarte el fetch por fila.

```rust
let mut post = Post::objects().find_or_fail(1, &pool).await?;

// FK starts Unloaded — just the PK. `Loaded` is a struct variant
// `{ pk, value }`; `value` is a `Box<Author>`.
match &post.author {
    ForeignKey::Unloaded(pk) => println!("author id = {pk}"),
    ForeignKey::Loaded { pk, value } => println!("author = {}", value.name),
}

// Force-load
let author = post.author.get(&pool).await?;          // fetches if Unloaded
```

Usa `select_related("author")` sobre el queryset para precargar un lote.

---

## Cuatro maneras de filtrar

Hay cuatro maneras de expresar un filtro; elige según el contexto. Las columnas tipadas se verifican en tiempo de compilación y son las mejores para el código de aplicación; la forma de string `field__lookup` es la sintaxis familiar de Django para el admin y CRUD genérico; `filter_op` es para cuando ya tienes un `Op` en la mano; la query string de HTTP impulsa la API pública.

```rust
// 1. HTTP query string (set via ViewSet filter_fields)
//    GET /api/posts?author_id=42&status__ne=archived

// 2. Django-shape string lookup (the same `field__lookup` grammar your
//    URL parser uses, but inside Rust). Suffix decides the operator
//    and value-shape; bare key is exact-eq. Field name is validated
//    at `.compile()`.
Post::objects()
    .filter("status", "published")                 // exact-eq
    .filter("title__icontains", "rust")            // ILIKE %rust%
    .filter("views__gt", 100_i64);

// 3. Explicit operator (legacy 3-arg shape — when you want to pass
//    an Op directly without parsing a suffix)
Post::objects().filter_op("author_id", Op::Eq, SqlValue::I64(42));

// 4. Typed columns (compile-time field check; preferred in app code)
Post::objects().where_(Post::author_id.eq(42));
```

**Convención:** tipado en código de aplicación, forma de Django en código de admin / CRUD genérico, `filter_op` solo cuando ya has calculado un `Op` (p. ej. de un parser de peticiones), query HTTP para la superficie de API pública.

### Sufijos de lookup soportados

| Sufijo | Operador SQL | Forma del valor | Notas |
|---|---|---|---|
| *(ninguno)* / `__exact` | `=` | escalar | la clave simple es exact-eq |
| `__ne` | `<>` | escalar | |
| `__gt` / `__gte` / `__lt` / `__lte` | `>` `>=` `<` `<=` | escalar | |
| `__contains` | `LIKE` | string | envuelve el valor como `%v%` |
| `__icontains` | `ILIKE` | string | envuelve el valor como `%v%`; emulado en MySQL vía `LOWER()` |
| `__startswith` | `LIKE` | string | envuelve como `v%` |
| `__istartswith` | `ILIKE` | string | envuelve como `v%` |
| `__endswith` | `LIKE` | string | envuelve como `%v` |
| `__iendswith` | `ILIKE` | string | envuelve como `%v` |
| `__iexact` | `ILIKE` | string | sin envoltura de comodines — coincidencia exacta insensible a mayúsculas |
| `__in` | `IN (…)` | `SqlValue::List` | rechaza valores que no son listas |
| `__isnull` | `IS NULL` / `IS NOT NULL` | `bool` | `true` → IS NULL, `false` → IS NOT NULL |
| `__between` / `__range` | `BETWEEN … AND …` | `SqlValue::List` de 2 elementos | inclusivo en ambos extremos |
| `__regex` / `__iregex` | PG `~` / `~*`, MySQL/SQLite `REGEXP` | string | insensible a mayúsculas emulado en MySQL/SQLite vía envoltura `LOWER()`; SQLite necesita una función de usuario `regexp` |

**Los errores se exponen en `.compile()`, no en el momento de la llamada a `.filter()`** — los desajustes de forma de valor (p. ej. `__in` con un escalar, `__isnull` con un no-bool, `__between` con la aridad equivocada) y los sufijos desconocidos (`status__nope`) devuelven `QueryError::UnknownLookup` / `QueryError::InvalidLookupValue` desde `.compile()` para que la cadena fluida se mantenga limpia de tipos. Los recorridos encadenados (`author__name__icontains`) **no** están soportados en v0.39 — el splitter toma el sufijo después del primer `__`, así que toda la cola `name__icontains` se trata como un sufijo desconocido.

Cada llamada de filtro se une con AND a cualquier anterior; mezcla la forma de Django, `filter_op` y `where_` libremente en el mismo queryset.

---

## Consultas acotadas por tenant

En una app multi-tenant, ejecuta cada consulta contra la conexión del tenant actual en lugar del pool compartido. Toma una conexión por petición y pásala a `fetch_on` (que acepta cualquier executor de base de datos) en lugar de `fetch` (que siempre usa `&pool`).

```rust
use rustango::extractors::Tenant;

async fn handler(mut t: Tenant) -> Result<...> {
    let conn = t.conn();        // &mut PgConnection for this tenant
    let posts = Post::objects().fetch_on(&mut *conn).await?;
    Ok(...)
}
```

`fetch_on` funciona con cualquier `sqlx::Executor`; `fetch` es azúcar para `fetch_on(&pool)`.

---

## Señales

Ejecuta un callback cuando algo ocurre — las señales de Django. Hay dos registros independientes: uno para escrituras de modelo, otro para peticiones HTTP.

### Ciclo de vida del modelo

Dispara un hook antes o después de que un modelo se guarde o elimine: `pre_save`, `post_save`, `pre_delete`, `post_delete`. Registra uno con `connect_post_save::<Post, _, _>(...)`.

```rust
use rustango::signals::{connect_post_save, PostSaveContext};

connect_post_save::<Post, _, _>(|post, ctx| async move {
    if ctx.created {
        tracing::info!("new post #{}", post.id.get().copied().unwrap_or(0));
    }
});
```

Se requiere `T: Clone + 'static` (el dispatcher entrega a cada receptor un clon `Arc<T>`). Los receptores se ejecutan secuencialmente en orden de registro. Desconecta vía el `ReceiverId` devuelto por `connect_*`. Los cuatro tipos de señal + sus formas de contexto están documentados inline en `rustango::signals`.

### Ciclo de vida de la petición

Dispara un hook alrededor de cada petición HTTP: `request_started`, `request_finished`, `got_request_exception`. Añade el middleware `RequestSignalsLayer` a tu router, luego conecta callbacks. Útil para tracing, auditoría, métricas en tiempo de petición y reporte de errores al estilo de Django.

```rust
use axum::Router;
use rustango::signals::request::{
    connect_request_started, connect_request_finished, RequestSignalsLayer,
};

connect_request_started(|ctx| Box::pin(async move {
    tracing::info!(method = %ctx.method, path = %ctx.path, "started");
}));
connect_request_finished(|ctx| Box::pin(async move {
    metrics::histogram!("http_request_ms").record(ctx.elapsed_ms);
}));

let app: Router = Router::new()
    .route("/", get(home))
    .layer(RequestSignalsLayer::new());  // outermost — sees request first / response last
```

| Señal | Campos de contexto |
|---|---|
| `request_started` | `method`, `path`, `query` |
| `request_finished` | `method`, `path`, `status`, `elapsed_ms` |
| `got_request_exception` | `method`, `path`, `error` |

Los receptores se ejecutan secuencialmente en orden de registro; envuelve un cuerpo en `tokio::spawn` para fanout paralelo o aislamiento de panics. Los registros de petición y de modelo son independientes — conectar / desconectar / limpiar uno no toca el otro.

---

## Consejos de rendimiento

Una lista de verificación rápida para mantener las consultas veloces a medida que los datos crecen:

- **Usa siempre índices para las columnas de `WHERE` y `ORDER BY`.** Decláralos vía `#[rustango(index)]` para que estén en las migraciones.
- **`select_related` para mostrar FK en listas** — elimina el N+1 en las vistas de admin/lista.
- **`page` en lugar de `fetch().drain()`** — nunca cargues tablas enteras.
- **Paginación con cursor para tablas enormes** — se salta el `COUNT(*)` por página.
- **`bulk_insert_on` para lotes** — un solo viaje de ida y vuelta en lugar de N.
- **`upsert_on` para importaciones idempotentes** — `ON CONFLICT` es más rápido que SELECT-luego-INSERT.
- **`transaction` para escrituras relacionadas** — reduce la sobrecarga de commit y mantiene la consistencia.
- **Cachea lecturas calientes** con `cache::get_or_set` — invalida en el handler de la señal `connect_post_save<T>(...)`.

---

## Ver también

- [Models](models.md) — declarar un modelo: tipos de campo, primary keys, cada atributo (el complemento de esta guía de consultas).
- [Serializers](serializers.md) — dar forma a las filas del modelo como JSON.
- [ViewSets](viewsets.md) — convertir un modelo en una API CRUD JSON.
- [The admin](admin.md) — una UI auto-generada sobre los mismos modelos.
- [`manage` CLI](manage.md) — `makemigrations` / `migrate` para cambios de schema.
