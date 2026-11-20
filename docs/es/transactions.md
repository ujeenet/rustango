# Transacciones

Un bloque `atomic` ejecuta un grupo de sentencias como una sola unidad: o se
confirman todas, o ninguna. Funciona en PostgreSQL, MySQL (y MariaDB) y SQLite.
Donde los backends difieren, esta página lo dice.

Para un handle de transacción simple (`transaction_pool`) y bloqueos de fila,
consulta la [guía del ORM](orm.md).

> **¿Algún término aquí es nuevo para ti?** *Transacción*, *rollback* —
> consulta el [glosario](glossary.md).

## Tabla de contenidos

- [Un primer bloque](#un-primer-bloque)
- [Qué se ejecuta en el bloque](#qué-se-ejecuta-en-el-bloque)
- [Bloques anidados](#bloques-anidados)
- [Después del commit](#después-del-commit)
- [Cuando una sentencia falla](#cuando-una-sentencia-falla)
- [SQLite toma el bloqueo de escritura en el BEGIN](#sqlite-toma-el-bloqueo-de-escritura-en-el-begin)
- [Niveles de aislamiento y ajustes](#niveles-de-aislamiento-y-ajustes)
- [¿Este código está en un bloque?](#este-código-está-en-un-bloque)
- [Get or create en una transacción](#get-or-create-en-una-transacción)
- [Las formas `_on`](#las-formas-_on)
- [DDL en un bloque](#ddl-en-un-bloque)
- [Iterar en un bloque](#iterar-en-un-bloque)
- [Cancelación](#cancelación)
- [Una transacción por petición](#una-transacción-por-petición)
- [Errores](#errores)

## Un primer bloque

Los ejemplos usan este modelo y un `pool: &Pool`:

```rust
use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "accounts")]
pub struct Account {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(unique, max_length = 64)]
    pub email: String,
    pub balance: i64,
}
```

`rustango::sql::atomic(&pool, closure)` abre una transacción, ejecuta la
closure y confirma cuando devuelve `Ok`. Un `Err` lo revierte todo.

```rust
use rustango::sql::{atomic, ExecError, Pool};

async fn transfer(pool: &Pool, from: i64, to: i64, amount: i64) -> Result<(), ExecError> {
    let p = pool.clone(); // the closure must own what it uses
    atomic(pool, move |_tx| {
        Box::pin(async move {
            let mut a = Account::find_or_fail(from, &p).await?;
            let mut b = Account::find_or_fail(to, &p).await?;
            a.balance -= amount;
            b.balance += amount;
            a.save(&p).await?;
            b.save(&p).await?;
            Ok(()) // Ok commits; an Err (or a failed `?`) rolls both saves back
        })
    })
    .await
}
```

La closure recibe su propio clon del pool, no un préstamo: el future del bloque
no debe tomar prestado al llamador. Clonar un `Pool` es barato. La macro
`rustango::atomic!(&pool, |tx| { … })` escribe el `Box::pin` por ti.

## Qué se ejecuta en el bloque

Dentro de la closure, cada llamada del ORM que recibe el mismo pool (o un clon)
se ejecuta en la transacción del bloque:

- lecturas: `fetch`, `find`, `count`, `get_or_create`, …
- escrituras: `insert`, `save`, `delete`, escrituras masivas, actualizaciones de queryset
- SQL crudo: `raw_query_pool`, `raw_execute_pool`
- DDL (ver [DDL en un bloque](#ddl-en-un-bloque))
- escrituras del framework: filas de auditoría, fixtures, escrituras de ViewSet
  y del admin, media, registro e invitaciones SSO

Ven las filas sin confirmar del bloque, y su rollback las deshace. En SQLite, y
en MySQL con su nivel por defecto (repeatable read), todas las lecturas del
bloque ven una sola instantánea.

Estas se ejecutan **fuera** del bloque, en su propia conexión:
`pool.acquire()`, la caché, la toma de jobs, los cambios del
registro de tenancy, una tarea lanzada con `tokio::spawn` y un pool conectado
aparte (aunque sea a la misma base de datos). No ven las escrituras del bloque
y pueden esperar bloqueos que el bloque retiene.

El argumento de la closure es un `AtomicTx`. `tx.lock().await?` da un
`TxGuard` que se desreferencia a `PoolTx`, para los helpers `_tx` y las formas
`_on`: `acc.insert_tx(&mut *guard)`. Ejecuta una sentencia cada vez: mientras
se retiene un guard, o cuando dos sentencias corren a la vez (`join!`), las
llamadas sobre el pool devuelven `ExecError::NestedAtomic`. Suelta el guard
antes de la siguiente llamada con `&pool`.

**Coste.** Una lectura o una sentencia cruda cuesta un viaje de ida y vuelta.
Una escritura del ORM cuesta tres, porque corre en su propio savepoint (así una
escritura fallida deja el bloque utilizable). Para muchas filas, escribe a
través del guard (`insert_tx`) o con una escritura masiva.

## Bloques anidados

`atomic` sobre el mismo pool dentro de un bloque abre un savepoint. Su `Err`
revierte solo sus propias escrituras. Un `Err` del bloque externo lo revierte
todo, incluido el trabajo anidado.

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        account("a@example.com", 0).insert(&p).await?;
        let q = p.clone();
        let inner = atomic(&p, move |_sp| {
            Box::pin(async move {
                account("b@example.com", 0).insert(&q).await?;
                // Stands in for any failure inside the nested block.
                Err::<(), _>(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))
            })
        })
        .await;
        assert!(inner.is_err()); // b@ is rolled back, a@ is kept
        Ok(())
    })
})
.await?;
```

(`account(email, balance)` construye un `Account` con `id: Auto::Unset`.)

El anidamiento va por objeto pool: pasa el pool de la petición en vez de
conectar de nuevo. Un bloque anidado descartado o que hace panic se revierte.
Si una sentencia de savepoint falla por sí misma, todo el bloque se revierte
con `ExecError::AtomicAborted`. PostgreSQL se ralentiza a partir de 64
savepoints en una transacción, y cada escritura del ORM es uno: mantén cortos
los bloques con muchas escrituras.

## Después del commit

`rustango::sql::on_commit(callback)` encola un callback que corre después de
que el bloque más externo confirma. Nunca corre en un rollback, y un bloque
anidado revertido descarta sus callbacks. Los callbacks corren en el orden en
que se encolaron.

```rust
use rustango::sql::on_commit;

let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let mut acc = account("new@example.com", 0);
        acc.insert(&p).await?;
        on_commit(move || {
            tokio::spawn(async move { /* send the welcome email */ });
        });
        Ok(())
    })
})
.await?;
```

El callback es síncrono: lanza el trabajo asíncrono desde él. Un callback que
hace panic se registra en el log y los demás corren igualmente. `on_commit`
fuera de un bloque hace panic.

## Cuando una sentencia falla

| Backend | Una lectura o sentencia cruda que falla | Una escritura del ORM que falla |
|---|---|---|
| PostgreSQL | Aborta el bloque: las sentencias siguientes y `atomic` devuelven `ExecError::AtomicAborted` | Se deshace sola (su savepoint); el bloque sigue |
| MySQL, MariaDB | Se deshace sola; el bloque sigue | Se deshace sola; el bloque sigue |
| SQLite | Se deshace sola; el bloque sigue | Se deshace sola; el bloque sigue |

Una closure que ignora un error y devuelve `Ok` confirma el resto del bloque
(en PostgreSQL, tras una lectura abortada, `atomic` devuelve en cambio
`AtomicAborted`).

Algunos errores terminan toda la transacción en el servidor: un deadlock de
MySQL (1213), un timeout de espera de bloqueo de MySQL (1205) cuando el
servidor revierte por ello, un rollback automático de SQLite, una conexión
perdida. Cada sentencia del ORM posterior en el bloque, y el propio `atomic`,
devuelven entonces `ExecError::AtomicAborted`. MariaDB informa de un `nowait()`
fallido como 1205; con el valor por defecto `innodb_rollback_on_timeout = OFF`
deshace solo la sentencia.

**Seguir tras una lectura fallida.** Para intentar un paso que puede fallar (un
bloqueo `nowait()`, una prueba sobre una tabla que quizá no existe), ponlo en un
bloque anidado. Su fallo deshace entonces solo ese bloque, en todos los
backends:

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let q = p.clone();
        // A failed read in a nested block undoes only that block, even on PG.
        let locked = atomic(&p, move |_sp| {
            Box::pin(async move {
                Account::objects()
                    .filter("email", "a@example.com")
                    .nowait()
                    .fetch(&q)
                    .await
            })
        })
        .await;
        match locked {
            Ok(_rows) => { /* got the lock */ }
            Err(_) => { /* row busy: the outer block goes on */ }
        }
        Ok(())
    })
})
.await?;
```

El SQL crudo que termina la transacción (`COMMIT`, `ROLLBACK` o una sentencia
con la que MySQL confirma) hace que el bloque devuelva
`ExecError::AtomicEndedEarly`. Las sentencias posteriores corrieron sin
transacción, así que no reintentes a ciegas.

## SQLite toma el bloqueo de escritura en el BEGIN

En SQLite el bloque más externo empieza con `BEGIN IMMEDIATE`. Un bloque que
lee y luego escribe no falla con `SQLITE_BUSY` en su primera escritura; en su
lugar, un segundo escritor espera en su propio `BEGIN`. Otras conexiones que
escriben esperan `busy_timeout` (5 segundos por defecto) y luego fallan con
`SQLITE_BUSY`; en una base de datos en memoria esperan sin límite de tiempo.
Mantén cortos los bloques de SQLite.

`AtomicOptions::deferred()` empieza con un `BEGIN` simple: un bloque que solo
lee no toma el bloqueo de escritura, y su primera escritura tras una lectura
falla enseguida con `SQLITE_BUSY` si otra conexión escribió entretanto. No
afecta a PostgreSQL ni a MySQL, cuyo `BEGIN` nunca bloquea.

## Niveles de aislamiento y ajustes

`atomic_with(&pool, options, closure)` abre el bloque más externo con un nivel
de aislamiento, ajustes de PostgreSQL, o ambos:

```rust
use rustango::sql::{atomic_with, raw_query_pool, AtomicOptions, Isolation, TxSetting};

let opts = AtomicOptions::new()
    .isolation(Isolation::RepeatableRead)
    .setting(TxSetting::new("app.account_id", "42")?); // PostgreSQL only
let p = pool.clone();
let seen: Vec<(Option<String>,)> = atomic_with(pool, opts, move |_tx| {
    Box::pin(async move {
        raw_query_pool("SELECT current_setting('app.account_id', true)", vec![], &p).await
    })
})
.await?;
```

- **Niveles:** `Isolation::ReadCommitted`, `RepeatableRead`, `Serializable`.
  PostgreSQL empieza con `BEGIN ISOLATION LEVEL …`. MySQL fija el nivel solo
  para esa transacción; el siguiente bloque en la conexión vuelve al valor por
  defecto del servidor. SQLite acepta cualquier nivel: siempre es
  serializable. Sin nivel, el bloque usa el valor por defecto del servidor
  (PostgreSQL: read committed; MySQL: repeatable read).
- **Los ajustes** (`TxSetting`) son solo de PostgreSQL, por ejemplo
  `app.account_id` para políticas de seguridad de fila. Cada uno se fija con
  `set_config(name, value, true)` y desaparece al terminar el bloque. El valor
  se enlaza. El nombre debe llevar punto (`prefix.name`) y cada parte ser un
  identificador simple, así que `search_path` o `role` devuelven
  `ExecError::InvalidTxSetting`. En MySQL y SQLite los ajustes devuelven
  `ExecError::Sql(SqlError::OperatorNotSupportedInDialect)` antes de que empiece
  el bloque.
- **Los bloques anidados** no pueden cambiarlos: `atomic_with` con opciones
  dentro de un bloque sobre el mismo pool devuelve `ExecError::InAtomic`.

## ¿Este código está en un bloque?

`rustango::sql::in_atomic(&pool)` es `true` mientras la tarea en curso está
dentro de un bloque sobre `pool` (o un clon), de modo que las llamadas del ORM
que reciben `pool` corren en su transacción. Es `false` fuera y para cualquier
otro pool. Sigue siendo `true` en un bloque que falló o terminó antes de tiempo.

## Get or create en una transacción

`get_or_create_tx` y `update_or_create_tx` toman un `PoolTx` abierto. En un
bloque, bloquea el guard y pásalo:

```rust
use rustango::sql::get_or_create_tx;

let email = "a@example.com".to_owned();
let (acc, created) = atomic(pool, move |tx| {
    Box::pin(async move {
        let mut g = tx.lock().await?;
        let filter = Account::objects().filter("email", email.clone());
        get_or_create_tx(
            filter,
            move |t| {
                Box::pin(async move {
                    let mut acc = Account { id: Auto::Unset, email, balance: 0 };
                    acc.insert_tx(t).await?;
                    Ok(acc)
                })
            },
            &mut *g,
        )
        .await
    })
})
.await?;
```

```rust
use rustango::sql::update_or_create_tx;

let (acc, created) = atomic(pool, move |tx| {
    Box::pin(async move {
        let mut g = tx.lock().await?;
        update_or_create_tx(
            Account::objects().filter("email", "b@example.com"),
            |t, mut acc: Account| {
                Box::pin(async move {
                    acc.balance += 10;
                    acc.save_tx(t).await?;
                    Ok(acc)
                })
            },
            |t| {
                Box::pin(async move {
                    let mut acc = Account {
                        id: Auto::Unset,
                        email: "b@example.com".into(),
                        balance: 10,
                    };
                    acc.insert_tx(t).await?;
                    Ok(acc)
                })
            },
            &mut *g,
        )
        .await
    })
})
.await?;
```

**Carreras.** La closure de creación corre en un savepoint. Cuando dos
transacciones crean la misma fila a la vez, el insert del perdedor choca con la
restricción de unicidad: su savepoint se revierte y lee la fila del ganador
(`created == false`; `update_or_create_tx` la actualiza entonces). La
transacción sigue utilizable. Hace falta una restricción `UNIQUE` sobre las
columnas filtradas. Un `get_or_create_tx` dentro de la closure de creación de
otro (una fila padre y luego la hija) funciona igual.

| Backend y nivel | El perdedor recibe |
|---|---|
| PostgreSQL read committed (por defecto) | La fila del ganador |
| PostgreSQL repeatable read | El error de violación de unicidad |
| PostgreSQL serializable | Un fallo de serialización (40001) |
| MySQL, MariaDB | La fila del ganador (releída con una lectura con bloqueo) |
| SQLite | Sin carrera: los bloques corren de uno en uno |

Con repeatable read o serializable en PostgreSQL, reintenta todo el bloque.

Las formas de pool `get_or_create` y `update_or_create` se comportan igual
dentro de un bloque: sus lecturas se unen a él y la closure de creación corre en
un savepoint. No retengas el guard mientras las llamas.

## Las formas `_on`

Los métodos `_on` toman cualquier ejecutor: un `&Pool`, una conexión retenida
(`&mut PoolConn` de `pool.acquire()`) o una transacción abierta (`&mut PoolTx`
de `transaction_pool`, o `&mut *guard` en un bloque). La sentencia corre sobre
lo que pasas.

- `fetch_on`, `insert_on`, `save_on`, `delete_on`, `upsert_on`
- `Model::bulk_insert_on`, `Model::bulk_upsert_on`, `Model::bulk_insert_or_ignore_on`
- `rustango::sql::bulk_update_on`
- `rustango::sql::fetch_dates_on`, `rustango::sql::fetch_datetimes_on`
- `rustango::sql::fetch_with_prefetch_on`
- `rustango::sql::model_shortcuts::increment_one_on`
- actualizaciones y borrados de queryset: `.update().execute_on(..)`, `.delete_on(..)`

```rust
use rustango::core::{BulkUpdateQuery, Model as _, SqlValue}; // `Model` brings `Account::SCHEMA` into scope
use rustango::sql::model_shortcuts::increment_one_on;
use rustango::sql::{bulk_update_on, transaction_pool};

let mut tx = transaction_pool(pool).await?;
let mut rows = vec![
    Account { id: Auto::Unset, email: "a@example.com".into(), balance: 0 },
    Account { id: Auto::Unset, email: "b@example.com".into(), balance: 0 },
];
Account::bulk_insert_on(&mut rows, &mut tx).await?;
// MySQL leaves `Auto` ids unset after a bulk insert: read them back.
let ids: Vec<i64> = Account::objects()
    .order_by(&[("id", false)])
    .fetch_on(&mut tx)
    .await?
    .iter()
    .filter_map(|a| a.id.get().copied())
    .collect();
let set_100 = BulkUpdateQuery::new(
    Account::SCHEMA,
    vec!["balance"],
    ids.iter()
        .map(|id| vec![SqlValue::from(*id), SqlValue::from(100_i64)])
        .collect(),
);
bulk_update_on(&mut tx, &set_100).await?;
increment_one_on::<Account, _>("id", SqlValue::from(ids[0]), "balance", 5, &mut tx).await?;
tx.commit().await?; // or drop `tx` to roll it all back
```

En un modelo auditado, las escrituras `_on` derivadas e `increment_one_on`
devuelven `ExecError::AuditUnsupported`: no pueden escribir las filas de
auditoría. Usa ahí las formas de pool.

## DDL en un bloque

- **PostgreSQL y SQLite:** el DDL corre en la transacción del bloque y se
  revierte con ella.
- **MySQL:** el DDL confirma la transacción. `run_ddl_idempotent` (y los
  helpers `ensure_table` construidos sobre él) se niega dentro de un bloque con
  un `sqlx::Error::Protocol` cuyo mensaje empieza por `DDL on MySQL is refused
  inside an atomic block on its pool`. El DDL crudo vía `raw_execute_pool` corre,
  confirma el trabajo hecho hasta entonces, y el bloque devuelve
  `ExecError::AtomicEndedEarly`.
- **Cualquier backend:** una migración con `"atomic": false` devuelve
  `MigrateError::Exec(ExecError::InAtomic { .. })`.

Ejecuta el DDL y las migraciones antes o después del bloque.

## Iterar en un bloque

Dentro de un bloque sobre el mismo pool, `.iterator(n)` ejecuta cada chunk en la
transacción del bloque. Ve las escrituras del bloque, conserva los bloqueos de
fila de `select_for_update()` y pagina por clave primaria (`WHERE pk > last`)
en lugar de `OFFSET`. Las filas que el bucle saca del filtro no se saltan:

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let mut it = Account::objects()
            .filter("balance", 0_i64)
            .order_by(&[("id", false)]) // primary key only, or no order
            .iterator(500)?;
        while let Some(chunk) = it.next_chunk(&p).await? {
            for mut acc in chunk {
                acc.balance = 1; // leaves the filter; paging by key still reaches every row
                acc.save(&p).await?;
            }
        }
        Ok(())
    })
})
.await?;
```

El queryset debe estar ordenado por la clave primaria (en cualquier sentido) o
no estar ordenado, seleccionar la clave primaria y no ser un `union` /
`intersection` / `difference`. Si no, `next_chunk` devuelve
`ExecError::InAtomic`.

## Cancelación

Descartar el future de un bloque (un `tokio::time::timeout`, un cliente que se
va) revierte el bloque, y sus callbacks `on_commit` nunca corren:

```rust
use std::time::Duration;

let p = pool.clone();
let slow = atomic(pool, move |_tx| {
    Box::pin(async move {
        Account { id: Auto::Unset, email: "late@example.com".into(), balance: 0 }
            .insert(&p)
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await; // stands in for slow work
        Ok(())
    })
});
// Dropping the block (here on timeout) rolls it back.
assert!(tokio::time::timeout(Duration::from_millis(100), slow).await.is_err());
```

Si una sentencia estaba corriendo cuando se descartó el bloque, el servidor no
la detiene. La conexión sigue ocupada hasta que el servidor termina esa
sentencia; luego corre el rollback y la conexión vuelve al pool. Mientras tanto,
a un pool pequeño le pueden faltar conexiones. Un bloque anidado descartado se
revierte en el siguiente uso del bloque.

## Una transacción por petición

`atomic_requests` ejecuta cada petición de escritura a las rutas de tu
aplicación en un bloque `atomic` sobre el pool de la petición. Una respuesta
que no es 2xx ni 3xx, o un panic, deshace todas las escrituras que el handler
hizo allí.

```rust
use rustango::atomic_request::AtomicRequests;
use rustango::sql::{AtomicOptions, Isolation};

// Tenancy: each block runs on the request's tenant pool.
let app = rustango::server::Builder::from_env().await?
    .api(routes)
    .atomic_requests(AtomicRequests::new())
    .into_router()
    .await?;

// One database: each block runs on the builder's pool.
rustango::server::AppBuilder::from_env().await?
    .api(other_routes)
    .atomic_requests(AtomicRequests::new().options(
        AtomicOptions::new().isolation(Isolation::ReadCommitted),
    ))
    .serve("0.0.0.0:8080")
    .await?;
```

- Solo POST, PUT, PATCH, DELETE y los demás métodos no seguros. GET, HEAD,
  OPTIONS, TRACE y QUERY se ejecutan sin transacción: una ruta GET que escribe
  (un callback de SSO) sigue en autocommit.
- Solo las rutas propias de tu aplicación: el admin del tenant, el login, el
  SSO y la consola del operador no se envuelven.
- Cada extractor de tenant de la petición (`TenantScope`, `Tenant`) sirve el
  pool del bloque, aunque la caché de pools lo desaloje entretanto. Una
  petición sin tenant pasa tal cual; un tenant cuyo pool no se encuentra
  recibe 500, nunca el pool del registro.
- Cada petición de escritura retiene una conexión hasta que termina. El
  arranque rechaza `max_cached_scoped_pools = 0` o
  `max_cached_database_pools = 0`, y avisa cuando
  `scoped_pool_max_connections` es menor que 8.

**Orden.** El builder pone el bloque dentro de sus propias capas (guarda de
panic, log de accesos, tracing, reparto de tenants) y alrededor de tu router.
Las capas de tu router (sesiones, CSRF, auth, idempotencia) se ejecutan dentro
del bloque: lo que escriben en el pool se confirma o se deshace con la
petición. Un `IdempotencyLayer` dentro del bloque guarda una respuesta solo
después del `COMMIT`. En un router hecho a mano, añade
`AtomicRequestLayer::new(pool)` o `AtomicRequestLayer::tenant()` antes de las
capas que deben ver la respuesta final.

**Un commit que falla.** La respuesta del handler se descarta, con sus cookies
y su redirección, y se envía una nueva. No se reintenta nada: los handlers
tienen efectos secundarios.

| Causa | Respuesta |
|---|---|
| Fallo de serialización (40001), interbloqueo, espera de bloqueo de MySQL (1205), `SQLITE_BUSY`, ninguna conexión libre | 503 con `Retry-After: 1` |
| `AtomicAborted`, `AtomicEndedEarly`, cualquier otro error | 500, registrado con el id de la petición |

**Lo que escapa al bloque.**

- Una tarea lanzada con `tokio::spawn` confirma por su cuenta; lánzala desde
  `on_commit(|| ..)`.
- Un cuerpo en streaming se ejecuta después del `COMMIT`.
- Dos sentencias a la vez en un handler (`join!`) devuelven `NestedAtomic`.
- `outside_atomic(fut)` ejecuta `fut` fuera del bloque, para una escritura que
  debe sobrevivir a un rollback. El framework escribe así las filas de
  auditoría de seguridad y las marcas de cierre de sesión. En el pool de la
  petición necesita una segunda conexión: en SQLite espera el bloqueo de
  escritura del bloque, en PostgreSQL puede esperar una fila que el bloque
  bloqueó.

**`Tenant::conn`.** Dentro de un bloque sobre el pool del tenant, `t.conn()`
presta la conexión del bloque, así que sus sentencias se confirman o se
deshacen con la petición. Las llamadas ORM sobre el pool devuelven
`NestedAtomic` mientras está prestada; `t.pool()` la devuelve.
`t.pool_conn()` y `t.into_conn()` devuelven ahí `InAtomic`.

**Según el backend.** PostgreSQL: un error de sentencia que el handler ignora
aborta el bloque, y la petición responde 500. MySQL: `REPEATABLE READ`
mantiene una sola instantánea para toda la petición, así que
`Isolation::ReadCommitted` suele encajar mejor; un DDL confirma el bloque
antes de tiempo (500). SQLite: los bloques empiezan diferidos, así que un POST
que solo lee no toma el bloqueo de escritura; una escritura tras una lectura
falla enseguida con `SQLITE_BUSY` (503) si otra conexión escribió entretanto.

**Coste.** `BEGIN` y `COMMIT` por petición de escritura, una comprobación
antes del commit y un savepoint alrededor de cada escritura ORM. En un
PostgreSQL local, un POST con una escritura añade alrededor de 1 ms al p99 en
solitario, más bajo carga.

## Errores

| Error | Significado |
|---|---|
| `ExecError::NestedAtomic` | Una sentencia corrió mientras se retenía un `TxGuard`, o dos corrieron a la vez (`join!`) |
| `ExecError::AtomicAborted` | La transacción se revirtió tras una sentencia o un savepoint fallido |
| `ExecError::AtomicEndedEarly` | El SQL crudo confirmó o terminó la transacción; algunas escrituras pueden estar confirmadas |
| `ExecError::InAtomic` | La operación se rechaza dentro de un bloque sobre su pool |
| `ExecError::InvalidTxSetting` | Un nombre de `TxSetting` no es un `prefix.name` con punto |
