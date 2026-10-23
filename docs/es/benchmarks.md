# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

El mismo blog servido por diez runtimes: **Go** (`net/http` de la biblioteca
estándar + `pgx`), **Rustango** sobre axum y sobre actix-web, **axum** y
**actix-web** puros con `sqlx` en crudo, un framework de **Python** sobre
gunicorn y sobre Hypercorn, **Laravel** sobre php-fpm y sobre Octane, y
**WordPress**. Cada app renderiza el mismo HTML a partir de los mismos datos.

Medido el **13 de octubre de 2026**: rustango release 0.60.6 en las tablas principales,
rustango 0.60.12 en [su propia tabla](#rustango-0607). Un solo arnés, una sola
máquina, las mismas reglas para cada app.

## Resumen

- **Páginas sin caché, apps compiladas.** Rust puro (axum + sqlx) es un 34 % más
  rápido que Go en el índice, Go es un 22 % más rápido en el detalle, la página
  de tag es un empate.
- **Rustango 0.60.7** sirve el 93–104 % de axum + sqlx puro en el índice sin
  caché y el 94–100 % en detalle, tag y compute.
- **Páginas cacheadas.** Cada app de Rust sirve 1,5–2,2× las solicitudes de Go,
  con un p99 cercano a 0,45 ms (Go: 2,0–2,4 ms).
- **Los runtimes interpretados** quedan 5–40× por detrás de las apps compiladas
  en las páginas sin caché. En las páginas cacheadas, WordPress es el más rápido
  de ellos (21k req/s).
- **Memoria bajo carga:** apps de Rust 7–8 MiB, Go 17–21 MiB, runtimes
  interpretados 89–296 MiB.

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

### Reglas de la prueba

- **CPU y memoria.** Cada app recibe 4 CPUs (núcleos 0–3) y 2 GB de RAM. Go corre
  con `GOMAXPROCS=4`, Tokio y actix-web con 4 workers.
- **Pool de base de datos.** 16 conexiones de PostgreSQL por app (la app de
  Python: 4 por proceso worker, 16 en total).
- **Consultas.** Las apps compiladas ejecutan el mismo plan: 3 consultas para el
  índice, 3 para el detalle, 4 para la página de tag. Rustango las construye con
  el ORM; las apps de Rust puro usan el texto SQL de Go. Las apps interpretadas
  usan la carga eager de su ORM. WordPress ejecuta 11–13 consultas por página.
- **Salida.** Antes de cualquier carga, cada página de cada app se compara con el
  HTML de Go; una diferencia detiene la ejecución. Sin compresión, así que cada
  app envía la misma página de índice de ~6,4 KB.
- **Ajustes de producción.** Debug desactivado. PHP: OPcache activado,
  comprobación de marcas de tiempo desactivada, `php artisan optimize`. Python:
  conexiones del pool.
- **Otros servicios.** PostgreSQL 16, MySQL 8.4 y Redis 7 en los núcleos 4–7; el
  generador de carga [`oha`](https://github.com/hatoo/oha) 1.16.0 en los núcleos
  8–11, dentro de la red de Docker. 50 conexiones, keep-alive activado.
- **Ejecuciones.** Por endpoint: 5 s de calentamiento y luego 3 × 20 s; cuenta la
  mediana. Solo cuentan las respuestas HTTP 200.
- **Builds.** rustc 1.99.0 (`opt-level=3`, fat LTO, `codegen-units=1`,
  `panic=abort`), Go 1.25.14, PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3,
  WordPress 6.9.10.

Host: Apple M3 Pro, VM de Docker con 12 CPUs y 16 GB.

---

## Runtimes compilados

### Throughput: solicitudes por segundo (más alto es mejor)

Columnas de Rustango: release 0.60.6.

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
más el binario.

### Rustango 0.60.7

Medido en una ejecución aparte, con axum + sqlx y rustango release 0.60.6 medidos de
nuevo al lado (mediana de req/s):

| Endpoint | axum + sqlx | rustango release 0.60.6 | rustango 0.60.7 | 0.60.7 vs axum |
|---|--:|--:|--:|--:|
| índice, sin caché | 10.750 | 8.596 | **11.143** | 104 % |
| detalle, sin caché | 17.907 | 17.024 | 16.875 | 94 % |
| tag, sin caché | 6.242 | 5.596 | 6.023 | 96 % |
| compute | 15.091 | 14.736 | 15.084 | 100 % |

El índice sin caché varió entre ejecuciones; comparando la mejor ejecución de
cada app, Rustango 0.60.7 llega al 93 % de axum + sqlx.

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

Una de las tres ejecuciones de WordPress en el índice sin caché devolvió 14.253
errores de "conexión a la base de datos" en 7 s; la mediana viene de una
ejecución limpia.

---

## Por qué: qué es más rápido, y dónde

Solo hechos medidos; donde una causa no se perfiló, se indica.

- **Página de tag: decide la base de datos.** PostgreSQL usó 3,65 de sus 4
  núcleos en esta ruta, así que todas las apps compiladas quedan a menos del
  20 % entre sí.
- **Índice sin caché: Rust puro supera a Go en un 34 %.** Las mismas tres
  consultas, las mismas filas. No perfilado en el lado de Go.
- **Detalle sin caché: Go supera a Rust puro en un 22 %.** La página renderiza un
  post con todos sus comentarios. No perfilado.
- **Páginas cacheadas: Rust 1,5–2,2× Go, p99 ~0,45 ms frente a 2,0–2,4 ms.** Un
  acierto es una lectura de Redis y una respuesta; sin base de datos, sin
  plantilla. La causa de la cola más ancha de Go no se perfiló.
- **Compute: Go sirve un 10 % más de solicitudes, Rust tiene una cola más corta**
  (p99 3,6–6,1 ms frente a 10,8 ms) con el mismo bucle de división por tanteo.
- **Memoria: Rust 7–8 MiB, Go 17–21 MiB bajo carga.** El runtime de Go mantiene
  un heap con recolección de basura; los binarios de Rust liberan la memoria
  sobre la marcha.
- **Rustango vs axum + sqlx puro.** En 0.60.6, el 46 % de la CPU de Rustango se
  iba en decodificar los resultados de las consultas probando unos diez tipos por
  valor. 0.60.7 decodifica cada columna una vez según su tipo (4,9 % de la CPU,
  sqlx en crudo: 5,0 %), lo que lo lleva a la velocidad de sqlx puro. El resto de
  la CPU es igual en ambas apps: el renderizado de Tera (~26 %) y la comprobación
  de conexión del pool (~18 %).
- **actix-web vs axum.** Con un pool de sqlx compartido, las apps de actix usan
  solo el 272–296 % de 4 CPUs y van detrás de axum en las páginas sin caché.
  Causa no encontrada ([#2660](https://github.com/ujeenet/rustango/issues/2660)).
- **Páginas cacheadas, interpretadas.** WordPress responde desde Redis en
  `advanced-cache.php`, antes de que WordPress mismo cargue (21k req/s). La app
  de Python y Laravel ejecutan primero su framework y sus middlewares, y luego
  leen la caché (11k y 3–6k).
- **Laravel: Octane 2–3× php-fpm en las páginas sin caché.** php-fpm arranca el
  framework en cada solicitud; Octane lo mantiene en memoria.
- **Python: gunicorn 2–4× Hypercorn.** Las vistas son síncronas, así que bajo
  ASGI cada solicitud pasa por un threadpool.
- **Compute, interpretado.** PHP 8.3 con OPcache ejecuta el bucle de enteros unas
  4× más rápido que CPython.
- **WordPress sin caché** ejecuta 11–13 consultas por página sobre MySQL, frente
  a 3–4 sobre PostgreSQL en las demás apps.

---

## Perfil de CPU de Rustango

Perfil `perf` de rustango release 0.60.6 bajo carga en el índice sin caché, con el
estado en 0.60.7:

| Coste | Cuota de CPU | Ticket | Estado |
|---|---|---|---|
| Decodificar cada valor probando unos diez tipos | 46 % | [#2655](https://github.com/ujeenet/rustango/issues/2655) | **Corregido en 0.60.7** (ahora 4,9 %) |
| `CachePageLayer` guarda las páginas como JSON + base64 | ~21 % de un acierto en caché | [#2656](https://github.com/ujeenet/rustango/issues/2656) | Abierto |
| Un `HashMap` con claves propias para cada fila de `values()` | 6–7 % | [#2661](https://github.com/ujeenet/rustango/issues/2661) | **Corregido en 0.60.7** (los nombres se leen una vez) |
| Querysets reconstruidos y compilados en cada solicitud | ~2 % | [#2662](https://github.com/ujeenet/rustango/issues/2662) | Corregido en parte en 0.60.7 |
| `.only()` / `values_list` no pueden seleccionar una columna relacionada (`author__name`) | Falta en la API | [#2657](https://github.com/ujeenet/rustango/issues/2657) | Abierto |
| Sin bind de array `= ANY($1)` para listas `IN` | menos del 0,5 % | [#2658](https://github.com/ujeenet/rustango/issues/2658) | Abierto |
| Sin prefetch muchos-a-muchos por lotes | Falta en la API | [#2659](https://github.com/ujeenet/rustango/issues/2659) | Abierto |

---

## Límites

- Una carga de trabajo **intensiva en lecturas y renderizada en el servidor**.
  Sin escrituras, flujos de autenticación, websockets ni lógica de negocio
  pesada.
- Un solo host (portátil M3 Pro, 6 núcleos de rendimiento + 6 de eficiencia).
  Los números absolutos serán distintos en otro hardware.
- Go y las apps de Rust puro son servicios escritos a mano; Rustango, la app de
  Python, Laravel y WordPress son frameworks.
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
