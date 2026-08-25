# Caché

La caché almacena el resultado de un trabajo costoso — una consulta pesada, un
fragmento renderizado, una llamada a una API de terceros — para que la siguiente
petición lo obtenga al instante en lugar de recalcularlo. **Rustango** te da un
único trait `Cache` con backends intercambiables (in-memory, Redis, base de
datos), un helper de cálculo-al-fallar (`get_or_set`) y helpers JSON tipados.
Cambia el backend sin tocar un solo sitio de llamada — como el framework de caché
de Django o la fachada `Cache` de Laravel.

[![Caching in Rustango: get_or_set checks the cache, runs the factory only on a miss, stores the result with a TTL, and serves hits instantly; the same Cache trait backs InMemory, Redis, and DB](img/caching.png)](img/caching.png)

> **¿Un término nuevo aquí?** *caché*, *TTL*, *clave*, *backend* — ver el
> [glosario](glossary.md).

> **Fuente:** `rustango::cache` (`Cache`, `InMemoryCache`, `NullCache`,
> `get_or_set`, `get_json`, `set_json`, `BoxedCache`, `from_settings`) — tras la
> feature `cache` (activa por defecto). `RedisCache` requiere la feature
> `cache-redis` (desactivada por defecto).
>
> **Versión ejecutable:** cada fragmento está copiado de
> [`cache_doc.rs`](../crates/rustango/tests/cache_doc.rs)
> (`cargo test -p rustango --test cache_doc`); el backend de base de datos se
> prueba con dogfooding sobre SQLite mediante
> [`cache_db_backend_sqlite_live.rs`](../crates/rustango/tests/cache_db_backend_sqlite_live.rs).

## Tabla de contenidos

- [Paso 1 — Elegir un backend](#step-1--pick-a-backend)
- [Paso 2 — get / set / delete](#step-2--get--set--delete)
- [Paso 3 — get_or_set (cache-aside)](#step-3--get_or_set-cache-aside)
- [Valores JSON tipados](#typed-json-values)
- [TTL y expiración](#ttl-and-expiry)
- [Cambiar de backend](#swapping-backends)
- [Caché con multi-tenancy](#caching-under-multi-tenancy)
- [Referencia](#reference)
- [Véase también](#see-also)

---

## Paso 1 — Elegir un backend

Cada backend implementa el mismo trait `Cache`, así que tu código es idéntico sea
cual sea el que elijas. El código de la aplicación mantiene un **`BoxedCache`**
(`Arc<dyn Cache>`) y nunca nombra el tipo concreto:

```rust
use rustango::cache::{BoxedCache, InMemoryCache};
use std::sync::Arc;

let cache: BoxedCache = Arc::new(InMemoryCache::new());
```

| Backend | Feature | Usar para |
|---|---|---|
| `InMemoryCache` | `cache` | dev, tests, un solo proceso (HashMap por proceso + TTL) |
| `RedisCache` | `cache-redis` | producción; compartido entre réplicas |
| `DbCache` | `cache` | producción sin Redis; una tabla `rustango_cache` |
| `NullCache` | `cache` | deshabilitar la caché (cada lectura falla) — práctico en tests |

---

## Paso 2 — get / set / delete

El núcleo del trait son cuatro métodos async. `set` toma un TTL opcional
(`None` = sin expiración); `get` retorna `Option<String>` (`None` en un fallo):

```rust
use rustango::cache::{Cache, InMemoryCache};

let cache = InMemoryCache::new();

assert_eq!(cache.get("greeting").await?, None);     // miss
cache.set("greeting", "hello", None).await?;        // store, no expiry
assert_eq!(cache.get("greeting").await?.as_deref(), Some("hello"));
assert!(cache.exists("greeting").await?);

cache.delete("greeting").await?;                    // gone
```

También hay variantes por lotes — `get_many` / `set_many` / `delete_many`.

---

## Paso 3 — get_or_set (cache-aside)

Esta es la que usarás más. `get_or_set` retorna el valor cacheado, o — en un
fallo — ejecuta tu factory, almacena el resultado con un TTL y lo retorna. La
factory **solo se ejecuta en un fallo**:

```rust
use rustango::cache::get_or_set;
use std::time::Duration;

let stats: HomeStats = get_or_set(
    &*cache,                       // &dyn Cache
    "home:stats",
    || async { compute_home_stats(&pool).await },   // runs only on a miss
    Some(Duration::from_secs(60)),                   // cache for 60s
).await?;
```

El test que lo respalda llama a `get_or_set` dos veces para la misma clave y
afirma que la factory se ejecutó **exactamente una vez** — la segunda llamada se
sirve desde la caché.

> **Invalida al escribir.** Cache-aside significa datos obsoletos hasta que
> expira el TTL. Para datos que cambian, haz también `delete(key)` cuando los
> escribes — p. ej. desde una [señal `post_save`](orm.md) — para que la siguiente
> lectura recalcule.

---

## Valores JSON tipados

`get_json` / `set_json` serializan cualquier tipo `Serialize`/`Deserialize` a
JSON, de modo que cacheas structs y listas, no solo strings:

```rust
use rustango::cache::{get_json, set_json};

#[derive(serde::Serialize, serde::Deserialize)]
struct Profile { id: i64, name: String }

set_json(&*cache, "profile:7", &profile, None).await?;
let back: Option<Profile> = get_json(&*cache, "profile:7").await?;   // None on a miss
```

(`get_or_set` los usa por debajo, por lo que su tipo de valor debe ser
`Serialize + Deserialize`.)

---

## TTL y expiración

Pasa una `Duration` a `set` (o `get_or_set`) y la entrada desaparece después de
ella. Verificado: una entrada de 50 ms es legible de inmediato y desaparece tras
80 ms.

```rust
cache.set("flash", "x", Some(Duration::from_millis(50))).await?;
// ...50ms later...
assert_eq!(cache.get("flash").await?, None);   // expired
```

`InMemoryCache::with_default_ttl(d)` fija un TTL por defecto que se aplica cuando
pasas `None`.

---

## Cambiar de backend

Como todo es el trait `Cache`, cambiar de in-memory a Redis es un cambio de una
línea en el arranque — normalmente impulsado por la configuración para que
difiera según el entorno:

```rust
// Build the cache from `[cache]` settings (backend = "memory" | "redis" | "db" | "null").
let cache: BoxedCache = rustango::cache::from_settings(&settings.cache);
```

En producción, apúntalo a Redis (compartido entre todas tus réplicas):

```rust
use rustango::cache::RedisCache;   // needs the `cache-redis` feature
let cache: BoxedCache = std::sync::Arc::new(RedisCache::new("redis://localhost").await?);
```

Las mismas llamadas `get` / `set` / `get_or_set` — solo cambió el constructor.

---

## Caché con multi-tenancy

`Cache` es un almacén plano indexado por `&str`, y con multi-tenancy eso
convierte la clave natural en la clave que se filtra: un handler — o peor, una
tarea de fondo, que no tiene ningún tenant ambiental del que tirar — escribe
`"stats:monthly"` para un tenant y todos los demás lo leen.

Envuelve la caché compartida en un **`ScopedCache`** para que el namespace se
aplique por ti y el sitio de llamada no pueda olvidarlo:

```rust
use rustango::cache::ScopedCache;

// From the Org the resolver already produced:
let cache = ScopedCache::for_tenant(shared.clone(), &t.org.slug);

cache.set("stats:monthly", &json, ttl).await?;   // stored as tenant:acme:stats:monthly
cache.get("stats:monthly").await?;               // reads only acme's entry
cache.clear().await?;                            // drops ONLY acme's entries
```

`ScopedCache` es en sí mismo un `Cache`, así que encaja en cualquier cosa que
tome un `BoxedCache` — `cache_page`, `cache_fragment`, los rate limiters,
`DistributedLock`. Reenvía al backend interno con las claves mapeadas en lugar
de reimplementar nada, de modo que las primitivas nativas (Redis `INCRBY`,
`SET NX`, `MGET`) conservan su atomicidad y su batching.

Dos cosas que conviene saber:

| | |
|---|---|
| **Es un namespace, no una frontera** | Todo sigue viviendo en un solo backend, y el código que tenga la caché *sin acotar* puede leer cualquier clave. La idea es que el camino ergonómico sea el correcto. |
| **`clear()` necesita enumerar claves** | Va por `Cache::delete_prefix`. `InMemoryCache` filtra su mapa y `DatabaseCache` lanza un `DELETE … LIKE 'prefix%'`. Un backend que *no* puede enumerar — `FileCache` convierte las claves en rutas con hash — recurre a borrar todo y registra una advertencia. Es deliberado: borrar de menos dejaría que otro namespace leyera una entrada obsoleta, lo cual es un fallo de corrección; borrar de más solo cuesta un fallo de caché. |

El `Cache::clear()` sin acotar sigue siendo global al proceso, así que usa la
vista acotada siempre que el cambio de un solo tenant sea lo que disparó la
invalidación.

---

## Referencia

**Trait `Cache`:** `get` · `set(key, value, ttl)` · `delete` · `exists` ·
`get_many` / `set_many` / `delete_many` · `get_or(key, default)`.

**Helpers libres:** `get_or_set(cache, key, factory, ttl)` ·
`get_json` / `set_json` · `from_settings(&CacheSettings)`.

**Qué está construido sobre la caché:** las [sesiones](auth-sessions.md) del lado
del servidor, la [limitación de tasa](middleware.md) distribuida
(`CacheRateLimitLayer`), las claves de idempotencia y los feature flags toman
todos un `BoxedCache` — de modo que una sola instancia de Redis los respalda a
todos.

---

## Véase también

- [Trabajos en segundo plano](jobs.md) — la otra mitad de mantener rápidas las
  peticiones (diferir el trabajo en lugar de cachear su resultado).
- [Sesiones](auth-sessions.md) — un almacén del lado del servidor construido
  sobre `Cache`.
- [Middleware](middleware.md) — `CacheRateLimitLayer` comparte un contador entre
  réplicas vía la caché.
- [Cookbook del ORM](orm.md) — invalidar lecturas cacheadas desde una señal
  `post_save`.
