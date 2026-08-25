# Trabajos en segundo plano

Cierto trabajo no debería suceder durante una petición — enviar un correo de
bienvenida, redimensionar una subida, sincronizar una API de terceros. Hacerlo en
línea hace esperar al usuario y acopla la respuesta a una llamada externa poco
fiable. Un **trabajo en segundo plano** mueve ese trabajo a una cola: el handler
retorna de inmediato, y un pool de workers ejecuta el trabajo momentos después,
con **reintentos automáticos** y una ruta **dead-letter** para los fallos. Esto
es Django-Q / Celery / las colas de Laravel, en Rust.

[![Background jobs in Rustango: a handler dispatches a Job onto a queue, worker tasks run it, retryable failures back off and retry, fatal ones go to a dead-letter handler](img/jobs.png)](img/jobs.png)

> **¿Un término nuevo aquí?** *cola*, *worker*, *reintento/backoff*,
> *dead-letter* — ver el [glosario](glossary.md).

> **Fuente:** `rustango::jobs` (`Job`, `JobQueue`, `InMemoryJobQueue`,
> `JobError`, `JobDeadLetter`) y `rustango::jobs::DatabaseJobQueue` (la cola
> respaldada por tabla, alias de `pg::PgJobQueue`) — tras la feature `jobs`
> (activa por defecto).
>
> **Versión ejecutable:** los fragmentos in-memory, de reintento y de
> dead-letter están copiados de
> [`jobs_doc.rs`](../crates/rustango/tests/jobs_doc.rs)
> (`cargo test -p rustango --test jobs_doc`); la cola persistente se prueba con
> dogfooding sobre SQLite mediante
> [`jobs_sqlite_live.rs`](../crates/rustango/tests/jobs_sqlite_live.rs)
> (`cargo test -p rustango --features sqlite,jobs-postgres --test jobs_sqlite_live`).

## Tabla de contenidos

- [Paso 1 — Definir un trabajo](#step-1--define-a-job)
- [Paso 2 — Iniciar una cola](#step-2--start-a-queue)
- [Paso 3 — Despachar desde un handler](#step-3--dispatch-from-a-handler)
- [Paso 4 — Cablearlo en tu aplicación](#step-4--wire-it-into-your-app) — el ejemplo completo
- [Ejecutar los workers (CLI + producción)](#running-the-workers-cli--production)
- [Reintentos y backoff](#retries-and-backoff)
- [El handler dead-letter](#the-dead-letter-handler)
- [La cola persistente (producción)](#the-persistent-queue-production)
- [Trabajos con multi-tenancy](#jobs-under-multi-tenancy)
- [Referencia](#reference)
- [Véase también](#see-also)

---

## Paso 1 — Definir un trabajo

Un trabajo es una struct serializable que implementa `Job`. El payload es lo que
se encola (serializado a JSON); `run()` es el trabajo. `NAME` enruta un payload
encolado de vuelta a su handler, así que debe ser único.

Genera un esqueleto con la CLI — `cargo run -- make:job WelcomeEmail` — o
escríbelo a mano:

```rust
use rustango::jobs::{Job, JobError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct WelcomeEmail {
    user_id: i64,
}

#[async_trait::async_trait]   // add `async-trait` to your Cargo.toml
impl Job for WelcomeEmail {
    const NAME: &'static str = "welcome_email";

    async fn run(&self) -> Result<(), JobError> {
        // ... look up the user and send the email
        Ok(())
    }
}
```

`run()` retorna:

- `Ok(())` — hecho.
- `Err(JobError::Retryable(msg))` — transitorio; el worker reintenta con backoff.
- `Err(JobError::Fatal(msg))` — permanente; omite reintentos, dead-letter
  inmediato.

Sobrescribe `const MAX_ATTEMPTS: u32 = 3;` en la impl para cambiar el tope de
reintentos (5 por defecto).

---

## Paso 2 — Iniciar una cola

Para un único proceso (y para dev y tests), `InMemoryJobQueue` ejecuta los
trabajos en tareas worker de tokio — sin base de datos. **Registra cada tipo de
trabajo, luego `start()` los workers** (los trabajos no se recogen hasta que lo
hagas):

```rust
use rustango::jobs::{InMemoryJobQueue, JobQueue};
use std::sync::Arc;

let queue = Arc::new(InMemoryJobQueue::with_workers(4));   // 4 worker tasks
queue.register::<WelcomeEmail>().await;                    // before start/dispatch
queue.start().await;

// ... app runs ...

queue.shutdown().await;   // on shutdown: drain in-flight jobs, then stop
```

Mantén el `Arc<InMemoryJobQueue>` en el estado de tu aplicación para que los
handlers puedan alcanzarlo.

> **In-memory significa in-memory.** Los trabajos encolados o en curso se
> **pierden al reiniciar**. Para todo lo que no puedas permitirte perder, usa la
> [cola persistente](#the-persistent-queue-production).

---

## Paso 3 — Despachar desde un handler

Una vez la cola está en marcha, despachar es una sola llamada — encola y retorna
de inmediato, así que la petición no espera al trabajo:

```rust
queue.dispatch(&WelcomeEmail { user_id: 42 }).await?;
```

Un worker lo recoge y ejecuta `run()` momentos después. Verificado de extremo a
extremo: los tres trabajos despachados se ejecutan todos en los workers.

```rust
for user_id in 1..=3 {
    queue.dispatch(&WelcomeEmail { user_id }).await.unwrap();
}
// → all three WelcomeEmail::run() calls execute on the worker pool
```

---

## Paso 4 — Cablearlo en tu aplicación

Juntando los pasos 1–3: construye la cola y registra cada trabajo **una sola vez
en el arranque**, inicia los workers, entrega la cola a tus rutas para que los
handlers puedan alcanzarla, y luego drena al apagar. La cola vive en tu
`main.rs`:

```rust
// src/main.rs
use std::sync::Arc;
use rustango::jobs::{InMemoryJobQueue, JobQueue};
use rustango::manage::Cli;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Build the queue + register every job type BEFORE starting workers.
    let queue = Arc::new(InMemoryJobQueue::with_workers(4));
    queue.register::<WelcomeEmail>().await;
    queue
        .on_dead_letter(|dl| async move {
            tracing::error!(job = dl.name, attempts = dl.attempts, error = %dl.error,
                            "job dead-lettered");
        })
        .await;

    // 2. Start the workers — tokio tasks that run inside THIS process.
    queue.start().await;

    // 3. Share the queue with handlers (axum extension/state).
    let app = urls::api().layer(axum::Extension(queue.clone()));

    // 4. Boot the server. Cli::run() blocks until Ctrl-C / SIGTERM.
    Cli::new().api(app).with_health().run().await?;

    // 5. On shutdown: drain in-flight jobs, then stop.
    queue.shutdown().await;
    Ok(())
}
```

Un handler despacha releyendo la cola desde la petición:

```rust
use axum::Extension;
use std::sync::Arc;
use rustango::jobs::{InMemoryJobQueue, JobQueue};

async fn signup(Extension(queue): Extension<Arc<InMemoryJobQueue>>) -> StatusCode {
    // ... create the user ...
    queue.dispatch(&WelcomeEmail { user_id: 42 }).await.ok();  // returns instantly
    StatusCode::CREATED
}
```

> **Almacena el tipo de cola concreto.** El trait `JobQueue` **no es
> object-safe** (sus `register`/`dispatch` son genéricos), así que no puedes
> mantener `Arc<dyn JobQueue>` en el estado — conserva el `Arc<InMemoryJobQueue>`
> concreto (o `Arc<DatabaseJobQueue>`).

---

## Ejecutar los workers (CLI + producción)

`start()` lanza los workers como **tareas de tokio dentro del proceso actual**.
Así que cuando arrancas tu aplicación con la CLI — `cargo run` (que ejecuta el
servidor) — los workers corren justo a su lado. Para la mayoría de las
aplicaciones eso es todo lo que necesitas: **un proceso sirve peticiones *y*
drena la cola**; no hay un comando worker aparte.

```bash
cargo run                 # starts the server AND the in-process workers
cargo run -- make:job WelcomeEmail   # scaffold a new job type
```

### Un proceso worker dedicado (producción)

A escala, a menudo quieres los workers **separados** del nivel web — para que un
pico de tráfico no pueda dejar sin recursos a los trabajos, y para escalar cada
uno de forma independiente. Con la [cola
persistente](#the-persistent-queue-production) cada proceso extrae de la misma
tabla `rustango_jobs`, así que simplemente ejecuta un segundo binario, sin
servidor, que construye la cola, la inicia y bloquea hasta una señal:

```rust
// src/bin/worker.rs — run with `cargo run --bin worker`
use std::sync::Arc;
use rustango::jobs::{DatabaseJobQueue, JobQueue};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = /* connect your tri-dialect Pool */;
    DatabaseJobQueue::ensure_table_pool(&pool).await?;

    let queue = Arc::new(DatabaseJobQueue::with_workers_pool(pool, 8));
    queue.register::<WelcomeEmail>().await;   // register the SAME job types
    queue.start().await;

    tokio::signal::ctrl_c().await?;            // block until Ctrl-C / SIGTERM
    queue.shutdown().await;                     // drain in-flight, then exit
    Ok(())
}
```

Despliégalo como su propio contenedor/servicio y escálalo a **N réplicas** —
todas extraen de la tabla compartida de forma segura. El proceso web entonces
solo necesita `dispatch` (no tiene que `start()` los workers). Empareja el worker
con un barrido periódico
[`reclaim_stuck_jobs_pool`](#the-persistent-queue-production) para recuperar
trabajos de un worker que se ha caído.

---

## Reintentos y backoff

Un trabajo que retorna `Retryable` se reencola con **backoff exponencial** (1s,
2s, 4s, 8s, …) hasta `MAX_ATTEMPTS`. Úsalo para fallos transitorios — un timeout,
una API con límite de tasa, un deadlock:

```rust
#[async_trait::async_trait]
impl Job for FlakyImport {
    const NAME: &'static str = "flaky_import";
    async fn run(&self) -> Result<(), JobError> {
        match call_external_api().await {
            Ok(_)  => Ok(()),
            Err(e) => Err(JobError::Retryable(e.to_string())),   // try again later
        }
    }
}
```

El test que lo respalda despacha un trabajo que falla una vez y luego tiene
éxito, y afirma que **se ejecutó más de una vez y finalmente tuvo éxito** — el
reintento realmente ocurre.

---

## El handler dead-letter

Cuando un trabajo agota sus reintentos — o retorna `Fatal` de inmediato — se
entrega al callback **dead-letter** en lugar de desvanecerse. Registra uno (antes
de `start`) para registrar, alertar o persistir el fallo:

```rust
queue
    .on_dead_letter(|dl| async move {
        // dl: JobDeadLetter { name, payload, attempts, error }
        tracing::error!(job = dl.name, attempts = dl.attempts, error = %dl.error,
                        "job dead-lettered");
    })
    .await;
```

Un error `Fatal` omite los reintentos y aterriza aquí en el primer intento:

```rust
async fn run(&self) -> Result<(), JobError> {
    Err(JobError::Fatal("unprocessable payload".into()))   // → dead-letter now
}
```

El test confirma que el callback se dispara exactamente una vez para un trabajo
`Fatal`, con el `name` y el `error` del trabajo intactos.

---

## La cola persistente (producción)

`InMemoryJobQueue` es de un solo proceso y olvida al reiniciar. Para despliegues
reales — múltiples workers, múltiples réplicas, trabajos que deben sobrevivir a
una caída — usa **`DatabaseJobQueue`** (el mismo tipo que `PgJobQueue`). Los
trabajos viven en una tabla `rustango_jobs`; los workers los recogen con un
`UPDATE … RETURNING` acotado por transacción, así que es seguro entre procesos.
Es **tri-dialecto** (PostgreSQL, MySQL, SQLite). Las definiciones `Job` del paso
1 no cambian.

```rust
use rustango::jobs::DatabaseJobQueue;   // = rustango::jobs::pg::PgJobQueue
use rustango::jobs::JobQueue;
use std::sync::Arc;
use std::time::Duration;

// 1. Create the jobs table once at boot (idempotent, tri-dialect).
DatabaseJobQueue::ensure_table_pool(&pool).await?;

// 2. Build the queue over your pool + N workers, and start it.
let queue = Arc::new(
    DatabaseJobQueue::with_workers_pool(pool.clone(), 4)
        .poll_interval(Duration::from_millis(50)),
);
queue.register::<WelcomeEmail>().await;
queue.start().await;

// 3. Dispatch exactly as before — now it's durable.
queue.dispatch(&WelcomeEmail { user_id: 42 }).await?;
```

**Recuperar workers caídos.** Si un worker muere a mitad de un trabajo, la fila
queda bloqueada. Ejecuta un barrido periódico para liberar los bloqueos más
antiguos que un umbral, de modo que otro worker retome el trabajo:

```rust
// e.g. from a scheduled task every few minutes:
let reclaimed = DatabaseJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(300)).await?;
```

Tanto el flujo dispatch-and-run como `reclaim_stuck_jobs_pool` se prueban con
dogfooding contra SQLite en `jobs_sqlite_live.rs`.

---

## Trabajos con multi-tenancy

Un worker no es un request. Nada resolvió un tenant para él — ningún host,
ninguna cabecera, ningún middleware se ejecutó — así que **un trabajo no lleva
contexto de tenant**: `run(&self)` solo recibe el payload deserializado. El
aislamiento viene **del pool sobre el que se construyó la cola**, y de ahí sale
la regla:

> Una cola por pool de tenant. Un id de tenant en el payload es enrutado, no
> aislamiento.

Esa única decisión es la que hace seguro cada modo:

| Modo | Dónde vive `rustango_jobs` | Por qué un worker permanece acotado |
|---|---|---|
| `database` | dentro de la propia base de datos / archivo SQLite del tenant | las filas de otro tenant no están en la tabla que consulta el worker — las lecturas entre tenants no son expresables |
| `schema` (PG) | en el esquema del tenant | `scoped_pool_dyn` devuelve un pool con `search_path` incorporado en sus **connect options**, no un `SET` por request — así cada checkout queda acotado durante toda la vida del pool, incluido un worker que viva días |

Construye las colas en el arranque, una por tenant activo:

```rust
use rustango::core::Column as _;
use rustango::jobs::{DatabaseJobQueue, JobQueue};
use rustango::sql::FetcherPool as _;
use rustango::tenancy::{Org, TenantPools};
use std::sync::Arc;
use std::time::Duration;

let pools = Arc::new(TenantPools::new(registry));
let registry_pool = pools.registry_pool();

// The registry holds the tenant list; each `Org` names its own
// database (or PG schema).
let orgs: Vec<Org> = Org::objects()
    .where_(Org::active.eq(true))
    .fetch(&registry_pool)
    .await?;

let mut queues = Vec::new();
for org in orgs {
    let pool = pools.scoped_pool_dyn(&org).await?;   // scoped for its lifetime
    DatabaseJobQueue::ensure_table_pool(&pool).await?;

    let queue = Arc::new(
        DatabaseJobQueue::with_workers_pool(pool, 2)
            .poll_interval(Duration::from_secs(1)),
    );
    queue.register::<WelcomeEmail>().await;
    queue.start().await;
    queues.push((org.slug, queue));
}
```

El despacho va entonces a la cola *de ese tenant* — en un handler de request, a
la que está indexada por el `Org` que el resolver ya produjo.

**Qué no hacer.** Una sola cola sobre el pool del registry con un campo
`org_id` en el payload pone los trabajos de todos los tenants en una tabla
compartida, y deja a cada `run()` la tarea de acordarse de reacotar. Un solo
reacotado olvidado es una escritura entre tenants. Si necesitas una cola
compartida por razones operativas, resuelve el pool del tenant como lo
*primero* que hace `run()` y no vuelvas a tocar el pool del registry después.

**Límites conocidos.** El framework no hace el fan-out por ti: no hay
supervisor de workers por tenant, ni `Job::run(&ctx)` con el tenant adjunto, ni
un barrido `reclaim_stuck_jobs_pool` consciente del tenant — iteras sobre los
tenants tú mismo, como arriba. Los tenants provisionados después del arranque
no obtienen workers hasta que el proceso se reinicia. Seguimiento en
[#1223](https://github.com/ujeenet/rustango/issues/1223).

---

## Referencia

**Backends**

| Backend | Usar para | ¿Sobrevive al reinicio? |
|---|---|---|
| `InMemoryJobQueue` | un solo proceso, dev, tests | no |
| `DatabaseJobQueue` (`PgJobQueue`) | producción; multi-worker / multi-réplica | sí (tabla `rustango_jobs`) |

**`JobError`**

| Variante | Efecto |
|---|---|
| `Retryable(String)` | reencolar con backoff exponencial, hasta `MAX_ATTEMPTS` |
| `Fatal(String)` | omitir reintentos → dead-letter de inmediato |
| `Queue(String)` | error interno de la cola (serialización/registro) |

**Métodos de `JobQueue`:** `register::<T>()` · `dispatch(&payload)` · `start()` ·
`shutdown()` · `pending_count()`. `DatabaseJobQueue` añade
`ensure_table_pool`, `with_workers_pool`, `poll_interval` y
`reclaim_stuck_jobs_pool`.

---

## Véase también

- [Planificador](manage.md) — para trabajo recurrente *basado en el tiempo*
  (estilo cron), en contraste con los trabajos bajo demanda.
- [Correo](email.md) — la carga de trabajo canónica del «hazlo en un trabajo».
- [Caché](caching.md) — la otra manera de mantener rápidos los handlers de
  peticiones.
- [Signals](orm.md) — hooks fire-and-forget que a menudo *despachan* un trabajo.
- [Comandos de tenancy](manage.md#tenancy-commands) — el provisionamiento de los
  tenants sobre los que una cola por tenant hace fan-out.
