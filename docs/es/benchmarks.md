# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

El mismo blog servido por cada app, con los mismos datos y el mismo HTML. Dos
rondas hasta ahora:

- **Ronda 2, 14 de octubre de 2026: apps compiladas, perfiladas.** **Go**
  (stdlib `net/http` + `pgx`) y siete apps de Rust: **axum** y **actix-web**
  puros con `sqlx` en crudo, **Rocket**, **Loco**, **Cot**, y **Rustango**
  sobre axum y sobre actix-web. Solo corre la app bajo prueba. Perfiles de
  CPU, de syscalls y del protocolo de Postgres muestran dónde gasta su tiempo
  cada app.
- **Ronda 1, 13 de octubre de 2026: todos los runtimes.** Go, Rustango y Rust
  puro, un framework de **Python** sobre gunicorn, Hypercorn, Uvicorn y
  Granian (servidores de Python medidos el 14 de octubre), **Laravel** sobre
  php-fpm y sobre Octane, y **WordPress**. Sus resultados compilados los
  reemplaza la ronda 2; sus resultados interpretados están más abajo.

Artículos con gráficos (en inglés):
[Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled)
y [ronda 1](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress).

## Resumen

- **Páginas sin caché.** Rustango sobre actix-web y sobre axum son las dos
  apps más rápidas de las ocho compiladas en el índice, el detalle y la página
  de tag (índice: 17.401 y 14.686 req/s; axum + sqlx 14.140, Go 8.741).
- **Páginas cacheadas.** Cada app de Rust sirve más que Go: 138.224–231.975
  req/s en el índice cacheado frente a 91.839 de Go. La causa principal es el
  recolector de basura de Go.
- **Compute.** Go es un 8–17 % más rápido que las apps de Rust. Rust compilado
  con el orden de instrucciones de Go corre en el tiempo de Go.
- **Memoria bajo carga:** 6,3–12,0 MiB en seis de las apps de Rust, Cot
  22,6 MiB, Go 22,8 MiB.
- **Los runtimes interpretados** (ronda 1) quedan 4–40× por detrás de las apps
  compiladas en las páginas sin caché. En las páginas cacheadas, WordPress es
  el más rápido de ellos (21k req/s). En Python, gunicorn (síncrono) supera a
  los tres servidores ASGI; Granian es el servidor ASGI más rápido. Memoria
  bajo carga: 89–293 MiB.
- **Correcciones de Rustango.** Los perfiles mostraron dónde perdía tiempo
  Rustango; 0.60.7 y 0.60.8 corrigen los costes grandes y 0.61.2 añade dos
  funciones de consulta que faltaban (ver
  [la tabla](#rustango-hallazgos-del-perfil-y-estado)).

---

## La configuración

Un blog con **autores, posts, tags (muchos-a-muchos) y comentarios**:

| Ruta | Renderiza | ¿Cacheada? |
|---|---|---|
| `GET /` | los últimos 20 posts, cada uno con autor, tags, recuento de comentarios | no |
| `GET /cached` | igual que `/` | Redis, 60 s |
| `GET /post/{slug}` | cuerpo del post + autor + tags + todos los comentarios | no |
| `GET /post/{slug}/cached` | igual que detalle | Redis, 60 s |
| `GET /tag/{slug}` | los últimos 20 posts que llevan un tag | no |
| `GET /compute` | suma de todos los primos por debajo de 20.000 (solo CPU, sin BD) | no |

La semilla es determinista: 10 autores, 30 tags, **1.000 posts**, 2.600 enlaces
post-tag y **10.000 comentarios**. Las relaciones se cargan de forma eager
(sin N+1). `/compute` ejecuta el mismo bucle de división por tanteo en cada
lenguaje.

### Reglas de la prueba

- **CPU y memoria.** Cada app recibe 4 CPUs (núcleos 0–3) y 2 GB de RAM. Go corre
  con `GOMAXPROCS=4`, Tokio y actix-web con 4 workers.
- **Pool de base de datos.** 16 conexiones de PostgreSQL por app (ronda 2: 16
  como máximo, 4 como mínimo; la app de Python: 4 por proceso worker, 16 en
  total).
- **Consultas.** Las apps compiladas ejecutan el mismo plan: 3 consultas para el
  índice, 3 para el detalle, 4 para la página de tag (Cot: 5, 5 y 7). Rustango
  y Loco las construyen con su ORM; las apps de Rust puro usan el texto SQL de
  Go. Las apps interpretadas usan la carga eager de su ORM. WordPress ejecuta
  11–13 consultas por página.
- **Salida.** Antes de cualquier carga, cada página de cada app se compara con el
  HTML de Go; una diferencia detiene la ejecución. Sin compresión, así que cada
  app envía la misma página de índice de ~6,4 KB.
- **Ajustes de producción.** Debug desactivado. PHP: OPcache activado,
  comprobación de marcas de tiempo desactivada, `php artisan optimize`. Python:
  conexiones del pool, logs de acceso desactivados.
- **Otros servicios.** PostgreSQL 16, MySQL 8.4 (solo WordPress) y Redis 7 en
  los núcleos 4–7; el generador de carga [`oha`](https://github.com/hatoo/oha)
  1.16.0 en los núcleos 8–11, dentro de la red de Docker. 50 conexiones,
  keep-alive activado.
- **Aislamiento (ronda 2).** Solo corre la app bajo prueba; las demás están
  detenidas. Entre apps, el número de conexiones de Postgres vuelve a su valor
  base.
- **Ejecuciones.** Por endpoint: 5 s de calentamiento y luego 3 × 20 s; cuenta la
  mediana. Solo cuentan las respuestas HTTP 200. La ronda 2 tuvo 0 errores o
  respuestas distintas de 200 en 144 ejecuciones.

Host: Apple M3 Pro, VM de Docker con 12 CPUs y 16 GB.

---

## Ronda 2: runtimes compilados

Medido el **14 de octubre de 2026**.

### Las apps

| App | Servidor | Acceso a datos | Plantillas | Caché de página |
|---|---|---|---|---|
| **Go** | stdlib `net/http` | pool `pgx` v5, SQL en crudo | `html/template` | `go-redis` |
| **axum + sqlx** | axum | `sqlx` en crudo, texto SQL de Go | Tera | Redis escrito a mano |
| **actix + sqlx** | actix-web | `sqlx` en crudo, texto SQL de Go | Tera | Redis escrito a mano |
| **Rocket** | Rocket | `rocket_db_pools` (sqlx) | `rocket_dyn_templates` (Tera) | Redis |
| **Rustango · axum** | axum | ORM de rustango | Tera | `CachePageLayer` |
| **Rustango · actix** | actix-web | ORM de rustango | Tera | caché de página de rustango |
| **Loco** | Loco | SeaORM | Tera | Redis |
| **Cot** | Cot | ORM de Cot | askama | Redis |

### Versiones

- Go 1.25.14, pgx v5.7.2, go-redis v9.7.0
- rustc 1.99.0 (`opt-level=3`, fat LTO, `codegen-units=1`, `panic=abort`);
  axum 0.8.9, actix-web 4.16.0, tokio 1.53, sqlx 0.8.6, Tera 1.20.1
- Rocket 0.5.1 (sqlx 0.7.4, hyper 0.14)
- Loco 1.2.0 (SeaORM 2.0.4, sqlx 0.9.0)
- Cot 0.7.0 (sqlx 0.9.0, askama 0.16.1)
- Rustango: un build de desarrollo sin ping al adquirir, con una caché de
  página que guarda bytes en crudo y una conexión del pool por solicitud.
  Estos tres cambios salieron en 0.60.8.

### Ajustes que no se pudieron igualar

| Ajuste | Apps | Por qué |
|---|---|---|
| El pool hace ping a Postgres en cada checkout (sqlx `test_before_acquire`) | Rocket, Loco, Cot | No hay ajuste para desactivarlo. Está desactivado en axum + sqlx, actix + sqlx y Rustango. |
| 10 conexiones de pool en lugar de 16 | Cot | Cot no tiene ajustes de pool. |
| Una porción del pool por worker (4 de las 16 conexiones) y una conexión de Redis por worker | actix + sqlx, Rustango · actix | Un pool compartido es más lento en actix-web ([hallazgo](#actix-web-un-pool-compartido-es-lento)). |
| sqlx 0.7.4 y hyper 0.14 | Rocket | Fijados por Rocket 0.5.1. |
| 5, 5 y 7 consultas por página de índice, detalle y tag en lugar de 3, 3 y 4 | Cot | Su API de consultas no puede expresar las consultas de Go. |
| askama en lugar de Tera | Cot | El motor de plantillas de Cot. |

### Throughput: solicitudes por segundo (más alto es mejor)

| App | Índice | Detalle | Tag | Índice, cacheado | Detalle, cacheado | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 8.741 | 22.607 | 6.386 | 91.839 | 141.574 | **16.580** |
| axum + sqlx | 14.140 | 22.842 | 6.467 | 198.669 | 214.871 | 14.963 |
| actix + sqlx | 14.408 | 21.630 | 6.628 | 226.887 | 240.161 | 14.578 |
| Rocket | 13.316 | 24.561 | 6.836 | 138.224 | 155.691 | 14.696 |
| Rustango · axum | 14.686 | 25.788 | **7.090** | 174.501 | 192.897 | 14.799 |
| Rustango · actix | **17.401** | **27.292** | 7.056 | **231.975** | **242.609** | 14.710 |
| Loco | 10.290 | 15.635 | 5.604 | 153.243 | 170.158 | 14.184 |
| Cot | 2.018 | 9.282 | 3.065 | 190.245 | 203.119 | 15.330 |

Índice, detalle y tag van sin caché.

### Latencia: p50 / p99 en ms (más bajo es mejor)

| App | Índice | Detalle | Tag | Índice, cacheado | Detalle, cacheado | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 5,50 / 9,96 | 1,91 / 5,02 | 7,61 / 11,72 | 0,35 / 2,73 | 0,24 / 2,05 | **2,37** / 10,75 |
| axum + sqlx | 3,49 / 4,71 | 2,14 / 3,60 | 7,42 / 13,89 | 0,24 / 0,53 | 0,22 / 0,51 | 3,35 / 6,07 |
| actix + sqlx | 3,32 / 6,38 | 2,21 / 4,30 | 7,23 / 13,23 | **0,20** / 0,50 | **0,19 / 0,45** | 3,41 / 4,97 |
| Rocket | 3,66 / 5,83 | 1,97 / 3,58 | 7,14 / 11,55 | 0,34 / 0,70 | 0,30 / 0,67 | 3,40 / 6,14 |
| Rustango · axum | 3,28 / 5,65 | 1,83 / 3,55 | **6,90 / 11,18** | 0,27 / 0,59 | 0,25 / 0,52 | 3,38 / 6,09 |
| Rustango · actix | **2,80 / 4,51** | **1,77 / 3,37** | 6,91 / 11,53 | **0,20 / 0,48** | **0,19 / 0,45** | 3,41 / **3,96** |
| Loco | 4,62 / 10,84 | 2,99 / 6,34 | 8,67 / 14,52 | 0,30 / 0,79 | 0,28 / 0,62 | 3,48 / 6,90 |
| Cot | 23,35 / 50,36 | 5,28 / 8,02 | 15,57 / 27,06 | 0,25 / 0,50 | 0,23 / 0,52 | 3,27 / 5,85 |

### Memoria bajo carga

Memoria residente del contenedor de la app en el índice sin caché
(`docker stats`, la muestra con más CPU de seis tomadas durante la primera
ejecución del índice):

| | Go | axum + sqlx | actix + sqlx | Rocket | Rustango · axum | Rustango · actix | Loco | Cot |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| RAM, MiB | 22,8 | 7,6 | 7,2 | 8,7 | 6,9 | **6,3** | 12,0 | 22,6 |

Tamaños de imagen de la ronda 1 (sin comprimir): Go 18,5 MB sobre `scratch`,
las apps de Rust 143–147 MB sobre `debian:bookworm-slim`.

### Rustango de la ronda 1 a la ronda 2

Rustango · actix, índice sin caché, req/s:

| Ejecución | req/s |
|---|--:|
| Ronda 1 (0.60.6) | 6.341 |
| Correcciones, pool compartido (ejecución de perfilado) | 12.052 |
| Correcciones, una porción del pool por worker (ronda 2) | **17.401** |

Rustango · axum antes → después de las correcciones, ejecuciones de perfilado:

| | Antes | Después |
|---|--:|--:|
| Índice, req/s | 11.215 | **15.849** |
| Detalle, req/s | 17.267 | **28.097** |
| Índice, CPU de la app por solicitud | 286,6 µs | **220,8 µs** |
| Detalle, CPU de la app por solicitud | 164,7 µs | **110,4 µs** |

---

## Por qué: qué es más rápido, y dónde

Solo hechos medidos; donde una causa no está probada, se indica. Los perfiles
son ejecuciones únicas de 25 s, una app cada vez, así que sus req/s difieren
un poco de las tablas de arriba.

### Compute: Go es un 8–17 % más rápido

Los dos bucles compilados tienen 8 instrucciones, un `sdiv` de 64 bits y una
comprobación de división por cero; LLVM elige otro orden de instrucciones.
Solo en un núcleo, fuera de HTTP, una llamada tarda 194 µs en Go, 224 µs en
Rust y 194 µs en Rust con exactamente el orden de instrucciones de Go. Qué
detalle de la CPU hace más lento ese orden no está probado: la VM de Docker no
tiene contadores de hardware. El resto de cada solicitud es más barato en Rust
(8 µs frente a 14 µs de Go).

Go tiene el p50 más bajo pero el p99 más alto: sirve las solicitudes fuera de
orden (p10 0,56 ms, p99 10,75 ms), las apps de Rust casi en orden de llegada
(axum + sqlx: p10 1,85 ms, p99 6,07 ms). Por qué el p99 de Go queda cerca de
10 ms no está probado.

### Páginas cacheadas: el recolector de basura de Go

Go hace 441 recolecciones por segundo en el índice cacheado; las pausas
stop-the-world suman cerca del 10 % del tiempo. Subir `GOGC` cierra la mayor
parte de la brecha (ejecuciones de perfilado, `GOGC=100` por defecto:
97.289 req/s):

| Go, índice cacheado | req/s | p99 |
|---|--:|--:|
| `GOGC=100` (por defecto) | 97.289 | 2,48 ms |
| `GOGC=400` | 144.083 | |
| GC casi desactivado | 159.734 | 0,90 ms |

El resto es CPU por solicitud:

| Índice cacheado, por solicitud | Go | axum + sqlx |
|---|--:|--:|
| CPU de la app | 27,2 µs | **14,6 µs** |
| Ruta de escritura del kernel | 10,6 µs | **6,9 µs** |
| Syscalls | 10,7 (6,6 read, 2,9 write) | **4,8** (2,0 recvfrom, 1,4 writev) |
| CPU de Redis | 6,8 µs | **2,5 µs** |
| Tráfico de Redis | go-redis: un GET por solicitud | una conexión, los GET de muchas solicitudes agrupados en un pipeline |

### Página de detalle: viajes de ida y vuelta a Postgres

Todas las apps envían las mismas tres consultas, pero el número de viajes de
ida y vuelta difiere. pgx envía uno por consulta. sqlx añade uno cuando se toma
una conexión del pool (el ping al adquirir) y otro cuando vuelve. axum + sqlx
toma una conexión por consulta, así que paga por consulta; Rocket y Rustango
toman una conexión por página, así que pagan una vez. Captura del protocolo
bajo carga, contando mensajes ReadyForQuery por página de detalle (el índice
muestra las mismas cifras):

| App | Viajes de ida y vuelta |
|---|--:|
| Go · pgx | **3** |
| Rustango | 4 |
| Rocket | 5 |
| axum + sqlx, ping desactivado | 6 |
| axum + sqlx, ping activado | 9 |

Ping al adquirir desactivado en axum + sqlx (ejecuciones de perfilado):
detalle 18.222 → 23.472 req/s (CPU de Postgres 166,6 → 134,3 µs por
solicitud), índice 12.140 → 14.419 req/s (237,9 → 208,5 µs). Tres viajes
menos ahorran 29–32 µs de CPU de Postgres por página: unos 10 µs cada uno. Go
tiene menos viajes, pero su recolector de basura lo detiene cerca del 9 % del
tiempo en esta ruta.

### Página de índice: GC y renderizado de plantillas

Otra vez el recolector de basura de Go (pausas cerca del 8 % del tiempo;
`GOGC=400` lleva el índice de 9.359 a 12.848 req/s, GC casi desactivado a
13.967) y el renderizado de plantillas: `html/template` de Go usa reflexión y
cuesta 167 µs por solicitud, serde + Tera 72 µs. En axum + sqlx, sqlx cuesta
unos 112 µs por solicitud (de ellos unos 25 µs son el ping cuando una conexión
vuelve al pool), malloc de glibc unos 56 µs y `sendto` del kernel 38 µs; las
partes se solapan.

### actix-web: un pool compartido es lento

En la ronda 1 las apps de actix-web iban detrás de axum en las páginas sin
caché y usaban menos del 300 % de su 400 % de CPU. axum ejecuta un runtime de
Tokio compartido y multihilo; actix-web, un runtime de un solo hilo por
worker. Con un pool compartido por todos los workers, cada consulta despierta
un hilo de otro runtime. Una porción del pool (4 de las 16 conexiones) y una
conexión de Redis por worker lo resuelven (ejecuciones de perfilado, req/s):

| | Pool compartido | Pool por worker |
|---|--:|--:|
| actix + sqlx, índice | 11.094 | **15.108** |
| actix + sqlx, detalle | 18.080 | **23.330** |
| actix + sqlx, índice cacheado | 192.604 | **238.884** |
| Rustango · actix, índice | 12.052 | **17.454** |
| Rustango · actix, detalle | 23.032 | **28.693** |
| Rustango · actix, índice cacheado | 190.889 | **237.962** |

Los cambios de contexto por solicitud bajan unas 2,5× (actix + sqlx índice
4,04 → 1,45, detalle 3,57 → 1,64; Rustango · actix índice 2,70 → 0,81,
detalle 2,34 → 1,03). Más workers no ayudan: 8 workers con un pool compartido
llegan a 11.904 req/s en el índice de actix + sqlx. El efecto está medido; el
mecanismo (un socket lo atiende el runtime que lo abrió) es la mejor
explicación, pero no está probado directamente. **En actix-web, crea el pool
de base de datos dentro de cada worker, no una vez para todos.**

### Rocket, Loco y Cot

- **Rocket** es fuerte en las páginas con base de datos (una conexión por
  página, 5 viajes) y la app de Rust más lenta en las páginas cacheadas:
  24,1 µs de CPU por solicitud frente a 14,6 µs de axum + sqlx, 8,1 syscalls
  frente a 4,8, un `writev` de más (2,24 por solicitud frente a 1,36) y
  contención en malloc (futex 2,88 por solicitud frente a 0,91).
- **Loco**: SeaORM envía tantas consultas como Go. En las páginas cacheadas,
  Loco gasta 21,0 µs por solicitud: 2,4 µs más en malloc y 1,2 µs más en
  middleware que axum + sqlx. En las páginas con base de datos el ping al
  adquirir no se puede desactivar; esas páginas no se perfilaron, así que no
  está probado cuánto de la brecha explica.
- **Cot**: la API de consultas solo tiene filter, limit y offset (sin ORDER BY,
  JOIN, IN ni COUNT), así que el índice carga los 1.000 posts con su cuerpo en
  cada solicitud: 5, 5 y 7 consultas en lugar de 3, 3 y 4, y 22,6 MiB bajo
  carga. Las páginas cacheadas no pasan por el ORM y son rápidas (16,4 µs de
  CPU por solicitud).

### Costes compartidos

Dos costes aparecen en cada app de Rust: el renderizado de Tera (cerca del
18–20 % de la CPU en las páginas sin caché) y malloc de glibc (cerca del 12 %).
Un asignador más rápido como mimalloc o jemalloc podría reducir el segundo;
no está probado.

### Runtimes interpretados (ronda 1)

- **Páginas cacheadas.** WordPress responde desde Redis en
  `advanced-cache.php`, antes de que WordPress mismo cargue (21k req/s). Las
  apps de Python y Laravel ejecutan primero su framework y sus middlewares, y
  luego leen la caché (Python 5–12k, Laravel 3–6k).
- **Laravel: Octane 2–3× php-fpm en las páginas sin caché.** php-fpm arranca el
  framework en cada solicitud; Octane lo mantiene en memoria.
- **Python: las vistas asíncronas no superan a gunicorn.** gunicorn sirve
  1,4–2,3× lo de los servidores ASGI en las páginas cacheadas y 1,4–1,8× en el
  detalle y el tag sin caché; en el índice sin caché los cuatro empatan
  (663–697). Según el código del framework: el ORM asíncrono ejecuta el ORM
  síncrono en un hilo (`sync_to_async`), no hay pool de base de datos
  asíncrono, y el handler ASGI crea un ejecutor de hilos por solicitud más unos
  3 saltos de hilo (señales de solicitud, cierre de la respuesta). La ruta
  asíncrona de la caché hace menos trabajo que la de gunicorn (un GET de Redis
  de HTML en crudo frente a dos GET y un unpickle) y aun así es más lenta. La
  parte de cada coste no se perfiló.
- **Servidores ASGI: Granian > Uvicorn > Hypercorn.** Índice cacheado 7.706 /
  6.233 / 5.151 req/s, detalle sin caché 1.854 / 1.654 / 1.550. No perfilado.
- **Compute.** PHP 8.3 con OPcache ejecuta el bucle de enteros unas 4× más
  rápido que CPython.
- **WordPress sin caché** ejecuta 11–13 consultas por página sobre MySQL, frente
  a 3–4 sobre PostgreSQL en las demás apps.

---

## Rustango: hallazgos del perfil y estado

De perfiles `perf` de Rustango bajo carga: ronda 1 (0.60.6) y las ejecuciones
de perfilado de la ronda 2.

| Coste | Cuota de CPU | Estado |
|---|---|---|
| Decodificar cada valor probando unos diez tipos | 46 % del índice | **Corregido en 0.60.7**: cada columna se decodifica una vez según su tipo (ahora 4,9 %) |
| Un checkout del pool por cada consulta | 19–35 % | **Corregido en 0.60.8**: `pool.acquire()` ejecuta las consultas de una solicitud en una sola conexión (detalle +26 %) |
| Ping a Postgres en cada checkout | 8–16 % | **Corregido en 0.60.8**: desactivado por defecto; `pool_test_before_acquire` lo activa |
| `CachePageLayer` guarda las páginas como JSON + base64 | ~12–20 % por acierto en caché | **Corregido en 0.60.8**: páginas guardadas como bytes en crudo |
| Un `HashMap` con claves propias para cada fila de `values()` | 6–7 % | Reducido en 0.60.7: los nombres se leen una vez por conjunto de resultados; la clave propia se queda (tipo público) |
| Querysets reconstruidos y compilados en cada solicitud | ~2 % | Abierto; 0.60.7 entrecomilla identificadores con una sola asignación |
| `.only()` / `values_list` no pueden seleccionar una columna relacionada (`author__name`) | Falta en la API | **Añadido en 0.61.2** |
| Sin bind de array `= ANY($1)` para listas `IN` | menos del 0,5 % | **Añadido en 0.61.2** (PostgreSQL, listas de un solo tipo) |
| Sin prefetch muchos-a-muchos por lotes | Falta en la API | Abierto |
| Índice cacheado: Rustango · axum 174.501 req/s frente a 198.669 de axum + sqlx | sin perfilar | Abierto |

---

## Ronda 1: runtimes interpretados

Los cuatro servidores de Python vienen de una sola ejecución (14 de octubre de
2026); Laravel y WordPress, del 13 de octubre de 2026. Las mismas reglas que
arriba, con todas las apps en marcha.

### Las apps

| App | Servidor | Acceso a datos | Caché de página |
|---|---|---|---|
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI, vistas síncronas) | el ORM del framework, pool de psycopg integrado | caché Redis del framework |
| **Python · Hypercorn** | Hypercorn, 4 workers, uvloop (ASGI, vistas asíncronas) | la API asíncrona del ORM del framework, el mismo pool | `redis.asyncio`, un GET por acierto |
| **Python · Uvicorn** | Uvicorn, 4 workers, uvloop + httptools (ASGI, vistas asíncronas) | igual | igual |
| **Python · Granian** | Granian, 4 workers, uvloop (ASGI, vistas asíncronas) | igual | igual |
| **Laravel · php-fpm** | nginx + php-fpm, 16 procesos hijos estáticos | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 workers | igual | igual |
| **WordPress** | nginx + php-fpm, 16 procesos hijos estáticos | `WP_Query`, `get_the_tags`, `get_comments` sobre **MySQL 8.4** | drop-in `advanced-cache.php`, phpredis |

Plantillas: el motor propio del framework de Python, Blade en Laravel y un tema
pequeño en WordPress. Las apps asíncronas de Python usan vistas `async def`
sobre la API asíncrona del ORM del framework, un wrapper de middleware
asíncrono y un pool `redis.asyncio` compartido de 16 por proceso; un acierto en
caché es un GET de Redis de HTML en crudo.

Versiones: PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3, WordPress 6.9.10,
Uvicorn 0.54.0, Granian 2.8.3, uvloop 0.22.1, httptools 0.8.0.

### Servidores de Python

#### Throughput: solicitudes por segundo (más alto es mejor)

| Endpoint | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| índice, sin caché | **697** | 663 | 675 | 692 |
| índice, cacheado | **11.012** | 5.151 | 6.233 | 7.706 |
| detalle, sin caché | **2.778** | 1.550 | 1.654 | 1.854 |
| detalle, cacheado | **11.848** | 5.085 | 6.215 | 7.810 |
| tag, sin caché | **1.411** | 906 | 948 | 1.020 |
| compute | **422** | 388 | 385 | 404 |

#### Latencia: p50 / p99 en ms (más bajo es mejor)

| Endpoint | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| índice, sin caché | **66,9** / 155,8 | 71,2 / 128,1 | 72,8 / **113,6** | 69,9 / 119,9 |
| índice, cacheado | **4,9 / 10,1** | 9,3 / 23,5 | 7,1 / 19,6 | 6,4 / 15,4 |
| detalle, sin caché | **18,9 / 34,9** | 30,5 / 57,0 | 29,2 / 52,5 | 26,0 / 47,6 |
| detalle, cacheado | **3,8 / 8,5** | 9,7 / 24,5 | 7,7 / 21,1 | 6,2 / 15,1 |
| tag, sin caché | **27,9** / 89,0 | 52,3 / 95,0 | 52,4 / 84,4 | 49,0 / **80,3** |
| compute | **113,0** / 215,8 | 128,6 / 160,6 | 131,7 / **154,5** | 117,9 / 187,1 |

#### Huella

| | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| Imagen (sin comprimir, compartida) | 345 MB | 345 MB | 345 MB | 345 MB |
| RAM, índice bajo carga | **224 MiB** | 255 MiB | 249 MiB | 293 MiB |
| CPU, índice bajo carga (de 400 %) | 244 % | 323 % | 314 % | 303 % |

### PHP

#### Throughput: solicitudes por segundo (más alto es mejor)

| Endpoint | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| índice, sin caché | 518 | **1.098** | 599 |
| índice, cacheado | 3.326 | 5.884 | **21.285** |
| detalle, sin caché | 572 | **1.712** | 687 |
| detalle, cacheado | 3.013 | 5.836 | **21.591** |
| tag, sin caché | 490 | **1.102** | 647 |
| compute | **1.611** | 1.521 | 732 |

#### Latencia: p50 / p99 en ms (más bajo es mejor)

| Endpoint | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| índice, sin caché | 95,5 / 122,6 | **44,1** / 110,1 | 78,1 / **103,9** |
| índice, cacheado | 14,6 / 22,4 | 7,4 / 57,7 | **2,3 / 3,5** |
| detalle, sin caché | 86,5 / 113,4 | **27,9** / 96,2 | 70,5 / **99,0** |
| detalle, cacheado | 16,4 / 21,6 | 7,4 / 58,2 | **2,3 / 3,6** |
| tag, sin caché | 100,8 / 130,1 | **44,0** / 110,9 | 75,9 / **97,0** |
| compute | **31,3 / 38,7** | 32,7 / 78,5 | 67,0 / 89,0 |

#### Huella

| | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| Imagen (sin comprimir) | **844 MB** | 895 MB | 1,01 GB |
| RAM, en reposo | **54 MiB** | 219 MiB | 61 MiB |
| RAM, índice bajo carga | **89 MiB** | 241 MiB | 142 MiB |
| CPU, índice bajo carga (de 400 %) | 380 % | 337 % | 405 % |

Una de las tres ejecuciones de WordPress en el índice sin caché devolvió 14.253
errores de "conexión a la base de datos" en 7 s; la mediana viene de una
ejecución limpia.

---

## Límites

- Una carga de trabajo **intensiva en lecturas y renderizada en el servidor**.
  Sin escrituras, transacciones, flujos de autenticación, websockets ni lógica
  de negocio pesada.
- Un solo host: un portátil Apple M3 Pro (6 núcleos de rendimiento + 6 de
  eficiencia). Otras CPUs pueden ordenar los resultados de otra forma, sobre
  todo en compute.
- Las apps corren en la VM de Docker sobre macOS, no en Linux sin VM. La VM no
  tiene contadores de rendimiento de hardware.
- Go y las apps de Rust puro son servicios escritos a mano; Rustango, Rocket,
  Loco, Cot, la app de Python, Laravel y WordPress son frameworks.
- Las rondas 1 y 2 son ejecuciones separadas; compara cifras dentro de una
  misma ronda.
- WordPress corre sobre MySQL porque no soporta PostgreSQL.

---

## Reproducir

El arnés es un proyecto autocontenido (`rustango-bench`). Desde su directorio:

```sh
bench/vendor.sh                 # vendor rustango at origin/develop into the build
docker compose --profile app build
bench/run.sh                    # 5 compiled apps; APPS="..." to pick others
```

Ajusta la ejecución con `REPS`, `DURATION`, `WARMUP` y `CONCURRENCY`, y el reparto
de CPU con `CAP_CPUS` y los valores `CAP_CPUSET_*` en `.env`. El JSON en crudo de
`oha`, los archivos de paridad HTML y un `meta.txt` con cada versión aterrizan en
`bench/results/`.

---

## Véase también

- [Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled) (en inglés)
- [Ronda 1: el benchmark justo](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress) (en inglés)
- [Primeros pasos](getting-started.md)
- [Recetario del ORM](orm.md)
