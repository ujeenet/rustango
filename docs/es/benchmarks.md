# Benchmarks: Rustango vs Django vs Laravel vs Go

¿Qué tan rápido es **Rustango**, de verdad? Esta página reporta un benchmark
cara a cara contra los dos frameworks en los que se inspira **Rustango** —
Django y Laravel — y contra una línea base de **Go** (la `net/http` de la
biblioteca estándar) que ancla lo que consigue un segundo runtime compilado y
nativo sobre la misma carga de trabajo. Todos usan sitios de blog
*funcionalmente idénticos*: mismos datos, mismo esquema, mismos endpoints, mismo
presupuesto de hardware — la única variable es el runtime. Django y Laravel se
benchmarkean cada uno en **ambos** su despliegue de producción convencional *y*
un runtime más robusto: **Django** sobre **gunicorn** (WSGI) y sobre
**Hypercorn** (ASGI); **Laravel** sobre **php-fpm + nginx** y sobre **Octane**
(Swoole). Rustango y Go son cada uno un único binario residente, así que hay uno
de cada.

Cada número de abajo es **medido y reproducible**, de una ejecución consistente
de un arnés de un solo comando (ver [Reproducir](#reproduce)). Nada aquí es
palabrería.

> **TL;DR.** Sobre hardware idéntico sirviendo páginas HTML renderizadas
> idénticas, los dos runtimes **compilados y nativos** — **Rustango** y **Go** —
> dejan a los frameworks interpretados **5–30× atrás** y se intercambian el
> liderazgo entre ellos. En el índice sin caché **Go** lideró con **6.651 req/s**
> y **Rustango** le siguió con **4.781** — **5,6×** Django (gunicorn) y **11,7×**
> Laravel (php-fpm). La ventaja de Go es más amplia en la página de detalle sin
> caché (**13.921 vs 6.538**, 2,1×). **Rustango** recupera el liderazgo donde
> importa para el tráfico servido: las rutas **cacheadas en Redis** (**25.546**
> vs 20.929 de Go en el índice; 35.781 vs 29.470 en detalle) y el **cómputo puro**
> (14.341 vs 11.573). También mantuvo la **menor RAM bajo carga** (18,5 MiB, sin
> GC) y — a diferencia de la app de la stdlib de Go — incluye un framework
> completo con todo incluido. Incluso el resultado no compilado más rápido,
> **Laravel sobre Octane**, va 4–7× por detrás de ambos binarios.

[![Solicitudes/s en el índice del blog sin caché a través de los seis runtimes — Go 6.651, Rustango 4.781, Laravel+Octane 1.238, Django+Hypercorn 910, Django+gunicorn 850, Laravel+php-fpm 408](img/benchmarks.png)](img/benchmarks.png)

---

## La configuración

Cuatro apps de blog — **autores, posts, tags (muchos-a-muchos) y comentarios** —
renderizando páginas HTML:

| Ruta | Renderiza | ¿Cacheada? |
|---|---|---|
| `GET /` | los últimos 20 posts, cada uno con autor, tags, recuento de comentarios | no |
| `GET /cached` | igual que `/` | Redis, 60 s |
| `GET /post/{slug}` | cuerpo del post + autor + tags + todos los comentarios | no |
| `GET /post/{slug}/cached` | igual que detalle | Redis, 60 s |
| `GET /tag/{slug}` | posts que llevan un tag | no |
| `GET /compute` | suma de todos los primos por debajo de 20.000 (ligado a CPU; sin BD, sin caché) | no |

Los primeros cinco están ligados a E/S + render; `/compute` es una carga de
trabajo puramente de CPU — el idéntico algoritmo de división por tentativa en
cada lenguaje — para aislar la velocidad bruta del runtime. Cada app carga sus
relaciones de forma **eager** (sin N+1): **Rustango** agrupa las consultas
explícitamente, Django usa `select_related` / `prefetch_related` /
`annotate(Count)`, Laravel usa `with()` + `withCount()`, **Go** hace carga por
lotes con consultas `= ANY($1)`. Las plantillas son deliberadamente diminutas y
equivalentes (Tera, plantillas de Django, Blade, `html/template` de Go) para que
midamos el *framework*, no el esfuerzo de la plantilla.

### Qué lo convierte en una pelea justa

- **Datos idénticos.** Un esquema de PostgreSQL y una semilla determinista
  compartida por las cuatro apps — leen las *mismas tablas*: 10 autores, 30 tags,
  **1.000 posts**, 2.600 enlaces post-tag, **10.000 comentarios**. El índice
  muestra los mismos 20 posts en el mismo orden en cada framework, y el HTML
  renderizado es idéntico byte a byte (salvo el estilo de escape de entidades de
  cada motor).
- **Presupuesto de hardware idéntico.** Cada app corre en un contenedor limitado
  a **4 CPUs / 2 GB de RAM**. PostgreSQL y Redis son compartidos e idénticos.
- **Uno a la vez.** Las apps se prueban de carga secuencialmente para que nunca
  compitan por el host (una máquina de 12 núcleos / 18 GB). Generador de carga:
  [`oha`](https://github.com/hatoo/oha), 50 conexiones concurrentes, 10 s por
  endpoint, keep-alive HTTP activado. Cada ejecución reportó **100 % de éxito**.
- **Todo en modo producción.** Esto importa mucho (ver más abajo).

### Configuración de producción

| | Runtime | Endurecimiento de producción |
|---|---|---|
| **Rustango** | un binario `--release` (axum + Tokio, async, todos los núcleos) | `opt-level=3` + LTO; caché de página en Redis vía `CachePageLayer` |
| **Go** | un binario estático (stdlib `net/http`, goroutines, todos los núcleos) | pool `pgx` + caché de página `go-redis`; plantillas embebidas; se distribuye sobre `scratch` |
| **Django 5.2** · gunicorn | workers gthread + keep-alive (WSGI) | `DEBUG=False`; caché Redis integrada + `@cache_page`; conexiones de BD persistentes |
| **Django 5.2** · Hypercorn | servidor ASGI, 4 workers, keep-alive | la misma app, servida sobre ASGI; las vistas síncronas corren en un threadpool |
| **Laravel 13** · php-fpm | php-fpm + nginx, OPcache **activado**, 16 workers | `APP_ENV=production`; `composer install --no-dev --optimize-autoloader`; Blade cacheado; `Cache::remember` sobre Redis |
| **Laravel 13** · Octane | Octane + **Swoole**, workers persistentes | la misma app sobre un runtime residente en memoria — sin arranque del framework por solicitud |

> El modo producción no es una nota al pie. La primera ejecución (sin ajustar) de
> Laravel apenas logró ~53 req/s; activar OPcache, el autoloader optimizado y un
> pool de workers adecuado la llevó a ~408 req/s — un **cambio de 7,7×** solo por
> configuración. Los números de abajo son todos de la configuración de producción.

---

## Resultados

### Rendimiento (throughput) — solicitudes por segundo (más alto es mejor)

Los dos binarios compilados, luego cada framework interpretado en su runtime
convencional **y** su runtime robusto:

| Endpoint | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| índice, sin caché | 4 781 | **6 651** | 850 | 910 | 408 | 1 238 |
| índice, **cacheado** | **25 546** | 20 929 | 4 841 | 1 537 | 1 224 | 5 777 |
| detalle, sin caché | 6 538 | **13 921** | 1 983 | 916 | 464 | 1 790 |
| detalle, **cacheado** | **35 781** | 29 470 | 4 843 | 1 320 | 793 | 5 811 |
| tag, sin caché | 3 926 | **4 353** | 1 129 | 1 033 | 398 | 1 179 |
| **compute** (ligado a CPU) | **14 341** | 11 573 | 452 | 400 | 716 | 1 504 |

Saltan a la vista tres historias. Primero, **Go y Rustango se agrupan muy por
encima de todos los demás** — la diferencia entre los dos binarios nativos
(decenas de por ciento, con Go por delante en E/S sin caché y Rustango por
delante en aciertos de caché + cómputo) es pequeña al lado del abismo de 5–30×
hasta los runtimes interpretados. Segundo, **Laravel + Octane** (Swoole) es un
**salto de 3–7×** sobre php-fpm — un worker residente que se salta el arranque
del framework de Laravel por solicitud — y es el resultado no compilado más
rápido en cada página. Tercero, **Django + Hypercorn** (ASGI) está más o menos
**plano, y más lento en las rutas cacheadas**: las vistas del blog son
*síncronas*, así que ASGI solo añade un salto de threadpool sin nada del beneficio
de concurrencia que traerían las vistas *async*. Incluso lo mejor de ese campo
(Octane, 1.238 req/s en el índice sin caché; 5.811 en el detalle cacheado) va por
detrás de ambos binarios en 4–7×.

### Latencia — p50 en milisegundos (más bajo es mejor)

| Endpoint | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| índice, sin caché | 10.2 | **7.2** | 56.9 | 43.2 | 114.7 | 39.9 |
| índice, cacheado | **1.8** | 2.1 | 9.3 | 5.0 | 20.2 | 7.6 |
| detalle, cacheado | **1.3** | 1.5 | 5.8 | 32.1 | 81.8 | 7.6 |
| compute (ligado a CPU) | 3.5 | **2.5** | 70.8 | 130.0 | 87.4 | 32.9 |

(Se muestran las medianas; el p50 / p95 / p99 completo para cada endpoint y
runtime está en `bench/results/summary.tsv`.) Las medianas de **Rustango** y de
**Go** en el índice sin caché (10,2 / 7,2 ms) están por debajo de las de cada
competidor interpretado, y sus medianas cacheadas (1,3–2,1 ms) son un orden de
magnitud menores que incluso el runtime de framework más rápido. Una salvedad a
favor de Go *y* en su contra: su p50 de compute (2,5 ms) supera al de Rustango,
pero su p99 de compute se dispara a ~47 ms en una pausa de GC — una cola que el
binario de Rust sin GC no tiene.

### Huella — tamaño de imagen, memoria, CPU

| | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Imagen de contenedor (sin comprimir) | 164 MB | **18.5 MB** | 293 MB | 293 MB | 959 MB | 1.01 GB |
| RAM, en reposo | 12.1 MiB | **5.2 MiB** | 128 MiB | 173 MiB | 92 MiB | 248 MiB |
| RAM, bajo carga | **18.5 MiB** | 34.7 MiB | 218 MiB | 277 MiB | 133 MiB | 267 MiB |
| CPU bajo carga (del tope de 400 %) | 295 % | 366 % | 356 % | 408 % | 406 % | 335 % |

**Go** distribuye la imagen más pequeña — un binario estático sobre `scratch`,
**18,5 MB** — y en reposo consume apenas **5,2 MiB**. Pero bajo carga su heap de
GC crece hasta **34,7 MiB**, ~1,9× los **18,5 MiB** planos de **Rustango**: sin
recolector de basura y sin asignación por solicitud, el binario de Rust a plena
carga cabe en menos RAM que Go, y por debajo de lo que usa cualquier runtime
interpretado en *reposo*. Los runtimes robustos cuestan *más* memoria, no menos:
Octane mantiene un Laravel residente en cada worker; Hypercorn añade la pila ASGI
encima de Django.

### Eficiencia — trabajo hecho por recurso (la verdadera historia)

El throughput bruto es una cosa; el **throughput por unidad de recurso** es lo
que tu factura de la nube realmente rastrea (índice sin caché):

| Métrica | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Solicitudes/s **por MiB de RAM** | **258** | 192 | 3.9 | 3.3 | 3.1 | 4.6 |
| Solicitudes/s **por % de CPU** | 16.2 | **18.2** | 2.4 | 2.2 | 1.0 | 3.7 |

Por megabyte de memoria, **Rustango** hace el mayor trabajo de cualquier runtime
aquí — ~35× el mejor resultado interpretado y ~1,3× el de Go, porque su huella se
mantiene plana bajo carga. Por porcentaje de CPU, **Go** se adelanta (convierte
los núcleos extra que arranca en un poco más de throughput). En cualquier caso,
para igualar el throughput del índice de un binario compilado tendrías que
ejecutar ~4 Laravel con Octane o ~6 Django con gunicorn — cada uno cargando su
propia huella de varios cientos de MB.

---

## Qué cambia la caché

Cada runtime es más rápido cuando una caché de página en Redis le permite
saltarse por completo la base de datos y el render — req/s del índice sin caché →
**cacheado**:

- **Rustango**: 4.781 → **25.546** (5,3× por el caché) — recupera el primer puesto.
- **Go**: 6.651 → **20.929** (3,1×) — lidera sin caché, segundo cacheado.
- **Laravel · Octane**: 1.238 → **5.777** (4,7×) — lo mejor del campo interpretado.
- **Django · gunicorn**: 850 → **4.841** (5,7×).
- **Django · Hypercorn**: 910 → **1.537** (1,7×) — la sobrecarga del threadpool
  de ASGI limita la ganancia incluso en aciertos de caché.
- **Laravel · php-fpm**: 408 → **1.224** (3,0×).

El caché ayuda a todos, pero no borra la diferencia — y es donde los dos binarios
compilados se separan: sin código de aplicación ejecutándose, la ruta
HTTP-accept → lectura-de-caché → respuesta es todo lo que queda, y el
`CachePageLayer` sin asignaciones de **Rustango** (25.546 req/s) se adelanta a la
ruta `go-redis` de Go (20.929), con ambos ~4× el mejor runtime de framework.

---

## Cómputo puro: compilado vs interpretado (y compilado vs compilado)

Las cinco rutas de página están dominadas por la base de datos y el motor de
plantillas. La ruta `/compute` las quita de en medio — suma todos los primos por
debajo de 20.000 mediante división por tentativa, el algoritmo *idéntico* en
Rust, Go, Python y PHP. Los cuatro devuelven la misma respuesta (`21171191`);
solo difiere la velocidad:

| | **Rustango** | **Go** | Django | Laravel |
|---|--:|--:|--:|--:|
| Throughput | **14 341 req/s** | 11 573 | 452 | 716 |
| latencia p50 | 3.5 ms | **2.5 ms** | 70.8 ms | 87.4 ms |

Los dos binarios nativos corren el bucle **~26–32×** más rápido que Django y
**~16–20×** más rápido que Laravel — la diferencia entre código máquina compilado
y un intérprete de bytecode. Entre Rust y Go, el bucle `--release` con LTO de Rust
se lleva la corona del throughput mientras que la latencia mediana de Go es en
realidad más baja; el GC de Go entonces aparece como latencia de cola (p99 ~47 ms)
que el binario de Rust nunca paga. Curiosamente PHP 8.3 (con OPcache) supera en
cómputo a CPython en este bucle entero apretado, así que Laravel *supera en
cómputo* a Django aquí aunque pierda en cada página ligada a E/S. Esta es la carga
de trabajo donde domina el lenguaje, no el framework — y donde llevar la lógica
caliente a **Rustango** rinde más.

---

## Salvedades honestas

Un benchmark en el que no puedes encontrar fallos no vale la pena publicarlo. Así
que:

- Los números son de **un** host de 12 núcleos / 18 GB (macOS + Docker). Los
  valores absolutos cambian en otro hardware; lo que se traslada es el panorama
  **relativo**.
- Esta es una carga de trabajo **intensiva en lecturas y renderizada en el
  servidor** — la forma de blog más común. No mide escrituras, flujos de
  autenticación, websockets ni lógica de negocio pesada.
- **Go aquí es la biblioteca estándar, no un framework par.** Rustango, Django y
  Laravel son frameworks con todo incluido (ORM, admin, migraciones, enrutamiento,
  plantillas, multi-tenancy); la app de Go es `net/http` + SQL en crudo escrita a
  mano — la línea base más ligera y rápida que un servicio Go alcanza de forma
  realista, y la representación más justa del lenguaje. Que empate o supere a
  Rustango en throughput bruto sin caché es exactamente el punto: **Rustango
  entrega rendimiento de clase Go con una experiencia de desarrollador de clase
  Django/Laravel.** La ventaja de Go en los endpoints sin caché se paga
  escribiendo tú mismo el SQL, el mapeo y el cableado.
- Los runtimes son fundamentalmente distintos: Rustango y Go son cada uno un
  binario que usa todos los núcleos con tareas baratas; Django y Laravel usan
  pools fijos de procesos/hilos worker. Esa diferencia *es* parte del resultado, y
  los recuentos de workers se fijaron en valores sensatos por CPU, no ajustados
  para favorecer a nadie.
- Django y Laravel se muestran cada uno en **ambos** runtimes — convencional
  (gunicorn, php-fpm) y robusto (Hypercorn, Octane). **Laravel sobre Octane es
  3–7× más rápido** que php-fpm; **Django sobre Hypercorn** (vistas síncronas)
  está más o menos plano. Ambos estrechan la diferencia con los binarios
  compilados; ninguno la cierra.

El punto no es que Django o Laravel sean lentos — mueven una porción enorme de la
web. Es que **Rustango** te da esa misma experiencia de desarrollador con todo
incluido con el rendimiento y la huella de Rust compilado — igualando un servicio
Go ajustado a mano mientras te entrega el framework que Go te obliga a construir.

---

## Reproducir

El arnés completo — las cuatro apps, el esquema de PostgreSQL compartido + la
semilla determinista, la configuración de Docker Compose y el runner — es un
proyecto autocontenido (`rustango-bench`). Desde su directorio:

```sh
bench/vendor.sh                       # vendor the framework into the build
docker compose build                  # build all six images
DURATION=10s CONCURRENCY=50 bench/run.sh
```

Requisitos: Docker + Compose y Rust (para `cargo install oha`). Ajusta el tope de
hardware en `.env` (`CAP_CPUS`, `CAP_MEM`) y la carga con `DURATION` /
`CONCURRENCY`. La salida en crudo por ejecución aterriza en `bench/results/`.


---

## Véase también

- [Primeros pasos](getting-started.md)
- [Recetario del ORM](orm.md)
