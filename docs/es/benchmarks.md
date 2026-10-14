# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

¿Qué tan rápido es **Rustango**? Esta página compara el mismo blog en diez
runtimes: **Go** (`net/http` de la biblioteca estándar + `pgx`), **Rustango**
sobre axum y sobre actix-web, **axum** y **actix-web** puros con `sqlx` en crudo,
un framework de **Python** sobre gunicorn y sobre Hypercorn, **Laravel** sobre
php-fpm y sobre Octane, y **WordPress**. Cada app renderiza el mismo HTML a partir
de los mismos datos.

Todos los números se midieron el **13 de octubre de 2026** con rustango 0.60.6,
en un solo arnés, en una sola máquina, con las mismas reglas.

> **TL;DR.** Con exactamente el SQL de Go, Rust puro (axum + sqlx) supera a Go en
> un **34 %** en el índice sin caché, empata en la página de tag y queda un
> **18 %** por detrás en la página de detalle sin caché. En páginas cacheadas en
> Redis, cada app de Rust sirve **1,5–2,2×** las solicitudes de Go con un p99
> cercano a **0,45 ms** (Go: 2,0–2,4 ms). En 0.60.6 **Rustango** costaba un
> **5–32 %** frente a sqlx en crudo en el mismo servidor y con las mismas
> consultas; **0.60.7 cierra esa brecha**: Rustango ahora sirve el **93–104 %** de
> axum + sqlx en crudo en el índice sin caché y el 94–100 % en las demás páginas
> (ver [0.60.7](#0607-cerrar-la-brecha-del-orm)). Los runtimes interpretados quedan **5–40×** por detrás en las páginas sin caché; el
> más rápido de ellos es la app de Python en el detalle (2.865 req/s) y Laravel
> sobre Octane en el índice (1.098). Todas las apps de Rust corren con **7–8 MiB**
> bajo carga; Go con 17–21 MiB; los runtimes interpretados con 89–296 MiB.

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

### Las apps

| App | Servidor | Acceso a datos | Caché de página |
|---|---|---|---|
| **Go** | stdlib `net/http` | pool `pgx`, SQL en crudo | `go-redis` GET/SET |
| **rustango · axum** | axum 0.8 | ORM de rustango | `CachePageLayer` |
| **rustango · actix** | actix-web 4 | ORM de rustango | GET/SET de Redis escrito a mano |
| **axum + sqlx** | axum 0.8 | `sqlx` en crudo, texto SQL de Go | GET/SET de Redis escrito a mano |
| **actix + sqlx** | actix-web 4 | `sqlx` en crudo, texto SQL de Go | GET/SET de Redis escrito a mano |
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI) | el ORM del framework, pool de psycopg integrado | caché Redis del framework |
| **Python · Hypercorn** | Hypercorn, 4 workers (ASGI, vistas síncronas) | igual | igual |
| **Laravel · php-fpm** | nginx + php-fpm, 16 procesos hijos estáticos | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 workers | igual | igual |
| **WordPress** | nginx + php-fpm, 16 procesos hijos estáticos | `WP_Query`, `get_the_tags`, `get_comments` sobre **MySQL 8.4** | drop-in `advanced-cache.php`, phpredis |

Plantillas: Tera en las apps de Rust, `html/template` en Go, el motor propio del
framework de Python, Blade en Laravel y un tema pequeño en WordPress.

### Qué mantiene la comparación justa

- **Mismo presupuesto de CPU.** Cada app recibe 4 CPUs (núcleos 0–3, `cpus: 4`,
  2 GB de RAM). Go corre con `GOMAXPROCS=4`, Tokio y actix-web con 4 workers.
- **Mismo pool.** Cada app compilada tiene 16 conexiones de PostgreSQL, mínimo 4.
  La app de Python usa el pool integrado de su framework, 4 por proceso worker
  (16 por app).
- **Mismas consultas.** Todas las apps compiladas siguen el plan de Go: 3
  consultas para el índice, 3 para el detalle, 4 para la página de tag. Las apps
  de Rustango las construyen con el ORM; las apps de Rust puro usan el texto SQL
  de Go tal cual. Las apps interpretadas usan la carga eager habitual de su ORM.
  WordPress ejecuta 11–13 consultas por página, 3 de ellas sus propias consultas
  de arranque.
- **Misma salida.** Antes de cualquier carga se obtiene cada endpoint de cada app
  y se compara con el HTML de Go; debe coincidir tras normalizar espacios y estilo
  de entidades, o la ejecución se detiene. Las respuestas van sin comprimir en
  todas partes (`Accept-Encoding: identity`, gzip desactivado en nginx), así que
  cada app envía la misma página de índice de ~6,4 KB.
- **Modo producción.** Debug desactivado en todas partes. PHP: OPcache activado
  con la comprobación de marcas de tiempo desactivada, `php artisan optimize`
  (config, rutas, eventos y vistas cacheados). Python: conexiones persistentes del
  pool.
- **Núcleos separados para todo lo demás.** PostgreSQL 16, MySQL 8.4 y Redis 7
  corren en los núcleos 4–7, el generador de carga en los núcleos 8–11.
- **Generador de carga dentro de Docker.** [`oha`](https://github.com/hatoo/oha)
  1.16.0 (fijado por digest) corre en la red de compose, no a través de la
  redirección de puertos de Docker Desktop. 50 conexiones, keep-alive activado.
- **Ejecuciones repetidas.** Por endpoint: 5 s de calentamiento y luego 3 × 20 s
  de ejecución; se reporta la mediana. El orden de las apps rota en cada ronda.
  Solo las respuestas HTTP 200 cuentan para req/s.
- **Builds fijados.** Rust: rustc 1.99.0, `opt-level=3`, fat LTO,
  `codegen-units=1`, `panic=abort`. Go 1.25.14. PHP 8.3.35, Laravel 13.36.0,
  Swoole 6.2.3, WordPress 6.9.10. Imágenes base por digest, archivos de bloqueo
  versionados en el arnés.

Host: Apple M3 Pro, VM de Docker con 12 CPUs y 16 GB. Go corrió en cada lote como
referencia y se mantuvo a menos del 6 % de su primera ejecución.

---

## Runtimes compilados

### Throughput: solicitudes por segundo (más alto es mejor)

| Endpoint | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| índice, sin caché | 9.360 | 8.485 | 6.341 | **12.505** | 9.963 |
| índice, cacheado | 96.214 | 164.942 | 200.167 | **214.097** | 197.252 |
| detalle, sin caché | **22.675** | 17.634 | 13.883 | 18.578 | 14.697 |
| detalle, cacheado | 143.795 | 194.939 | 214.662 | **233.669** | 211.293 |
| tag, sin caché | **6.457** | 5.577 | 5.146 | 6.299 | 5.903 |
| compute | **16.588** | 14.733 | 15.137 | 15.085 | 14.741 |

### Latencia: p50 / p99 en ms (más bajo es mejor)

| Endpoint | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| índice, sin caché | 5,11 / 9,31 | 5,81 / 8,00 | 7,78 / 10,20 | **3,93 / 5,80** | 4,95 / 6,99 |
| índice, cacheado | 0,36 / 2,40 | 0,30 / 0,53 | 0,24 / 0,47 | **0,23 / 0,42** | 0,24 / 0,46 |
| detalle, sin caché | **1,91** / 4,97 | 2,78 / 4,32 | 3,54 / 5,19 | 2,64 / **4,12** | 3,34 / 4,88 |
| detalle, cacheado | 0,24 / 2,01 | 0,25 / 0,47 | 0,22 / 0,46 | **0,21 / 0,40** | 0,22 / 0,47 |
| tag, sin caché | **7,53** / 11,73 | 8,83 / 11,99 | 9,59 / 12,65 | 7,82 / **10,42** | 8,36 / 11,28 |
| compute | **2,36** / 10,84 | 3,40 / 6,07 | 3,32 / **3,58** | 3,33 / 5,97 | 3,39 / 3,65 |

### Huella

| | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| Imagen (sin comprimir) | **18,5 MB** | 147 MB | 146 MB | 143 MB | 143 MB |
| RAM, en reposo | 5,1 MiB | 1,4 MiB | 1,7 MiB | **1,3 MiB** | 1,6 MiB |
| RAM, índice bajo carga | 21,2 MiB | 7,5 MiB | 7,3 MiB | 7,3 MiB | **6,9 MiB** |
| CPU, índice bajo carga (de 400 %) | 296 % | 393 % | 296 % | 328 % | 272 % |

Go se distribuye sobre `scratch`; las imágenes de Rust son `debian:bookworm-slim`
más el binario, así que la mayor parte de su tamaño es la imagen base.

### Cómo leer los números

- **Rust vs Go, mismo SQL.** axum + sqlx lidera en el índice sin caché (+34 %), Go
  lidera en el detalle sin caché (+22 %) y la página de tag es un empate. Go
  también sirve un 10 % más de solicitudes de compute, pero su p99 de compute
  (10,8 ms) es 2–3× el de las apps de Rust.
- **Páginas cacheadas.** Sin trabajo de base de datos pendiente, cada app de Rust
  sirve 1,5–2,2× las solicitudes de Go, y el p99 se mantiene por debajo de 0,55 ms.
- **La página de tag mide PostgreSQL.** La base de datos usó 3,65 de sus 4 núcleos
  en esta ruta, así que las cinco apps chocan con el mismo muro.
- **Rustango vs sqlx en crudo** (mismo servidor axum, mismas consultas): −32 % en
  el índice, −5 % en el detalle, −12 % en el tag. Rustango es la única app que usa
  todo su presupuesto de CPU (393 %). La mayor parte de la diferencia es una sola
  función; ver [Costes conocidos](#costes-conocidos-en-rustango). 0.60.7 la
  cierra; ver abajo.
- **actix-web vs axum.** Con un pool de sqlx compartido, las apps de actix se
  quedan en 272–296 % de CPU y van detrás de axum en las páginas sin caché. La
  causa aún no se ha identificado
  ([#2660](https://github.com/ujeenet/rustango/issues/2660)).

### 0.60.7: cerrar la brecha del ORM

La tabla de arriba es rustango 0.60.6. 0.60.7 decodifica cada columna una sola
vez según su tipo en lugar de probar diez tipos por celda
([#2655](https://github.com/ujeenet/rustango/issues/2655)), lee los nombres de
columna una vez por conjunto de resultados
([#2661](https://github.com/ujeenet/rustango/issues/2661)) y entrecomilla los
identificadores con una sola asignación de memoria
([#2662](https://github.com/ujeenet/rustango/issues/2662)). Medido en una sola
ejecución, mismo método, el Rustango antiguo y el nuevo lado a lado (mediana de
req/s, sin otros builds en marcha):

| Endpoint | axum + sqlx | rustango 0.60.6 | rustango 0.60.7 | 0.60.7 vs axum |
|---|--:|--:|--:|--:|
| índice, sin caché | 10.750 | 8.596 | **11.143** | 104 % |
| detalle, sin caché | 17.907 | 17.024 | 16.875 | 94 % |
| tag, sin caché | 6.242 | 5.596 | 6.023 | 96 % |
| compute | 15.091 | 14.736 | 15.084 | 100 % |

La decodificación bajó del 46 % de la CPU de la app al 4,9 %, lo mismo que las
lecturas tipadas de sqlx en crudo (5,0 %). El índice sin caché varió entre
ejecuciones; comparando la mejor ejecución de cada app, Rustango llega al 93 % de
axum + sqlx. Lo que queda es común a ambas apps: el renderizado de Tera (cerca del
26 %) y la comprobación de conexión del pool al adquirirla (cerca del 18 %).

---

## Runtimes interpretados

### Throughput: solicitudes por segundo (más alto es mejor)

| Endpoint | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| índice, sin caché | 683 | 601 | 518 | **1.098** | 599 |
| índice, cacheado | 11.052 | 2.744 | 3.326 | 5.884 | **21.285** |
| detalle, sin caché | **2.865** | 1.314 | 572 | 1.712 | 687 |
| detalle, cacheado | 11.473 | 2.644 | 3.013 | 5.836 | **21.591** |
| tag, sin caché | **1.337** | 786 | 490 | 1.102 | 647 |
| compute | 410 | 353 | **1.611** | 1.521 | 732 |

### Latencia: p50 / p99 en ms (más bajo es mejor)

| Endpoint | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| índice, sin caché | 74,5 / 137,7 | 96,4 / 213,0 | 95,5 / 122,6 | **44,1** / 110,1 | 78,1 / **103,9** |
| índice, cacheado | 4,4 / 12,5 | 18,7 / 40,4 | 14,6 / 22,4 | 7,4 / 57,7 | **2,3 / 3,5** |
| detalle, sin caché | **14,2 / 43,0** | 35,3 / 76,2 | 86,5 / 113,4 | 27,9 / 96,2 | 70,5 / 99,0 |
| detalle, cacheado | 4,1 / 11,4 | 19,0 / 48,2 | 16,4 / 21,6 | 7,4 / 58,2 | **2,3 / 3,6** |
| tag, sin caché | **35,6 / 77,0** | 66,0 / 131,1 | 100,8 / 130,1 | 44,0 / 110,9 | 75,9 / 97,0 |
| compute | 117,9 / 222,5 | 129,6 / 245,3 | **31,3 / 38,7** | 32,7 / 78,5 | 67,0 / 89,0 |

### Huella

| | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| Imagen (sin comprimir) | **294 MB** | **294 MB** | 844 MB | 895 MB | 1,01 GB |
| RAM, en reposo | 128 MiB | 172 MiB | **54 MiB** | 219 MiB | 61 MiB |
| RAM, índice bajo carga | 224 MiB | 296 MiB | **89 MiB** | 241 MiB | 142 MiB |
| CPU, índice bajo carga (de 400 %) | 260 % | 323 % | 380 % | 337 % | 405 % |

### Cómo leer los números

- **Páginas sin caché.** El mejor resultado interpretado en cada página queda
  6–11× por detrás de Rust puro y Go en las páginas de índice y detalle, y unas 5×
  en la página de tag.
- **Páginas cacheadas.** El `advanced-cache.php` de WordPress sirve un acierto
  antes de que WordPress cargue, así que supera a todos los demás runtimes
  interpretados (21k req/s). Le sigue la caché de la app de Python sobre gunicorn
  (11k); Laravel ejecuta su pila de middlewares antes de `Cache::remember` (3–6k).
- **gunicorn vs Hypercorn.** Las vistas son síncronas, así que ASGI solo añade un
  salto de threadpool: Hypercorn es 2× más lento en el detalle y 4× más lento en
  las páginas cacheadas que gunicorn con la misma app.
- **Octane vs php-fpm.** Un worker residente de Swoole rinde 2–3× php-fpm en las
  páginas sin caché y unas 1,8× en las páginas cacheadas, e igual en compute.
- **Compute.** PHP 8.3 con OPcache ejecuta el bucle de enteros unas 4× más rápido
  que CPython.
- **WordPress** corre sobre MySQL, no sobre PostgreSQL, así que sus números sin
  caché miden en parte otra base de datos. Una de sus tres ejecuciones del índice
  sin caché falló con 14.253 errores de "conexión a la base de datos" en 7 s
  (WordPress abre una conexión MySQL por solicitud); la mediana, 599, viene de una
  ejecución limpia.

---

## Qué cambió desde la versión anterior de esta página

La versión de junio de 2026 reportaba Go a 6.651 req/s y Rustango a 4.781 en el
índice, y Laravel, la app de Python y Hypercorn con "100 % de éxito". Varios de
esos números eran incorrectos.

**Runtimes compilados.** Dos diferencias, cada una medida por separado con el
método anterior:

| Endpoint | Go, 12 hilos | Go, 4 hilos | rustango, plan antiguo | rustango, plan de Go |
|---|--:|--:|--:|--:|
| índice, sin caché | 6.814 | 9.322 (+37 %) | 7.385 | 8.462 (+15 %) |
| detalle, sin caché | 19.184 | 22.125 (+15 %) | 12.546 | 17.594 (+40 %) |
| tag, sin caché | 5.450 | 6.250 (+15 %) | 5.965 | 5.515 (−8 %) |
| compute | 9.263 | 16.599 (+79 %) | 14.714 | 14.710 (0 %) |

- **El número de hilos de Go.** El módulo de Go declaraba `go 1.23`, así que
  Go 1.25 no aplicó su valor por defecto de `GOMAXPROCS` consciente de
  contenedores y corrió con 12 hilos en una cuota de 4 CPUs. La limitación de CPU
  que eso causó también explica el antiguo p99 de compute de 47 ms, que la página
  anterior atribuía a la recolección de basura.
- **Las consultas de Rustango.** La app antigua ejecutaba 5 consultas en el índice
  y el detalle y 7 en el tag, cargando todos los autores, todos los tags y el
  cuerpo completo de cada post. Ahora sigue el plan de Go.
- **También corregido, sin medir por separado:** tamaño del pool (10 por defecto
  en sqlx frente a 16 en Go), rustango 0.43 → 0.60.6, el generador de carga pasó
  del host macOS a la red de Docker, fijación de núcleos, y una sola ejecución de
  10 s se sustituyó por la mediana de tres ejecuciones de 20 s.

**Runtimes interpretados.**

- **"100 % de éxito" era éxito de conexión, no HTTP 200.** La ejecución de
  Hypercorn en el detalle en junio tuvo 5.457 HTTP 500 frente a 3.655 respuestas
  buenas. Ahora solo cuenta HTTP 200.
- **Hypercorn perdía conexiones a la base de datos.** Las conexiones persistentes
  bajo ASGI abrían una nueva conexión de PostgreSQL para la mayoría de las
  solicitudes y alcanzaban el límite de 100 conexiones en segundos. También abría
  una nueva conexión de Redis por solicitud. Ahora usa el pool de PostgreSQL del
  framework y un pool de conexiones de Redis compartido por worker.
- **Laravel se quedaba sin puertos.** predis abría una nueva conexión de Redis por
  solicitud; bajo 20 s de carga el contenedor se quedaba sin puertos locales y
  devolvía 500. Ahora usa phpredis con conexiones persistentes.
- **Laravel enviaba páginas comprimidas.** nginx y Octane comprimían la respuesta
  con gzip, así que Laravel enviaba ~700 bytes donde las demás apps enviaban
  6,4 KB. La compresión ahora está desactivada en todas partes.
- **Faltaban las cachés de producción de Laravel.** Solo las vistas Blade estaban
  cacheadas; `php artisan optimize` ahora cachea también config, rutas y eventos.

---

## Costes conocidos en Rustango

Aparecieron al igualar las consultas de Go. Son trabajo para el framework, no
problemas del arnés. Las cuotas de CPU vienen de perfiles `perf` de la app de
rustango bajo la misma carga (0.60.6). Cada uno tiene un ticket:

| Coste | Ticket | Estado |
|---|---|---|
| Las filas de `values_list` en PostgreSQL prueban unos diez tipos por celda y construyen un error por cada fallo. **46 % de la CPU de la app** en el índice sin caché. | [#2655](https://github.com/ujeenet/rustango/issues/2655) | **Corregido en 0.60.7** (ahora 4,9 %) |
| `CachePageLayer` guarda el cuerpo como base64 dentro de JSON, así que cada acierto parsea JSON y decodifica base64: **cerca del 21 % de la CPU por acierto**, 165k aciertos/s frente a 214k de un `GET` simple de Redis. | [#2656](https://github.com/ujeenet/rustango/issues/2656) | Abierto |
| `values()` y `annotate()` crean para cada fila un `HashMap` con claves `String` propias: cerca del 6–7 % de la CPU en el índice sin caché. | [#2661](https://github.com/ujeenet/rustango/issues/2661) | **Corregido en 0.60.7** (los nombres se leen una vez; la clave propia se queda, es el tipo público) |
| Cada petición reconstruye y recompila los mismos querysets: cerca del 2 % de la CPU. | [#2662](https://github.com/ujeenet/rustango/issues/2662) | Corregido en parte en 0.60.7 (entrecomillado); caché de SQL compilado abierta |
| `.only()` y `values_list` no pueden seleccionar una columna relacionada como `author__name`; igualar las columnas de Go requiere un `Join` manual con `project`. | [#2657](https://github.com/ujeenet/rustango/issues/2657) | Abierto |
| No hay bind de array `= ANY($1)`; una lista `IN` se convierte en `$1..$n`, así que cada longitud de lista es su propia sentencia preparada. Sin coste de CPU en este benchmark (menos del 0,5 %). | [#2658](https://github.com/ujeenet/rustango/issues/2658) | Abierto |
| No hay prefetch muchos-a-muchos por lotes para una lista de filas; los tags llegan mediante un join sobre la tabla intermedia. | [#2659](https://github.com/ujeenet/rustango/issues/2659) | Abierto |
| Con un pool de sqlx compartido, las apps de actix-web usan menos del 300 % de sus 4 CPU. También se ve sin rustango. | [#2660](https://github.com/ujeenet/rustango/issues/2660) | Abierto |

---

## Límites de este benchmark

- Es una carga de trabajo **intensiva en lecturas y renderizada en el servidor**.
  No mide escrituras, flujos de autenticación, websockets ni lógica de negocio
  pesada.
- Un solo host (un portátil M3 Pro con 6 núcleos de rendimiento y 6 de
  eficiencia) que también ejecutaba otras tareas durante las ejecuciones (carga
  media de 3–4). El panorama relativo se mantiene; los números absolutos serán
  distintos en otro hardware.
- Go y las apps de Rust puro son servicios escritos a mano. Rustango, la app de
  Python, Laravel y WordPress son frameworks; las filas de Rustango vs sqlx en
  crudo muestran lo que cuesta esa capa hoy.
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

- [Primeros pasos](getting-started.md)
- [Recetario del ORM](orm.md)
