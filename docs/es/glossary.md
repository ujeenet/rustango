# Glosario

Una referencia en lenguaje sencillo de las palabras usadas en esta documentación. Si un
término de una guía te resulta desconocido, búscalo aquí primero. Las definiciones son
deliberadamente informales — las guías en profundidad tienen los detalles precisos.

Si nunca has construido una API web antes, lee [Fundamentos de las API web](#web-api-basics)
de principio a fin; es una introducción de cinco minutos. Todo lo demás está pensado para
consultarse sobre la marcha.

## Tabla de contenidos

- [Fundamentos de las API web](#web-api-basics) — qué es una API, en términos cotidianos
- [Bloques de construcción de Rustango](#rustango-building-blocks) — las piezas que ensamblas
- [Los datos y la base de datos](#data-and-the-database)
- [Unas pocas palabras de Rust](#a-few-rust-words) — para que los bloques de código no den miedo
- [Frameworks con los que comparamos](#frameworks-we-compare-to)

---

## Fundamentos de las API web

**API** — *Application Programming Interface* (interfaz de programación de aplicaciones). Una forma de que un programa hable con
otro. Una **API web** lo hace por internet: tu app envía un mensaje, el
servidor envía uno de vuelta. Piénsalo como un camarero — pides de un menú, la
cocina te devuelve comida.

**API REST** — el estilo más común de API web. "REST" es solo un conjunto de
convenciones: actúas sobre **recursos** (como "posts" o "users") usando verbos
web estándar. No necesitas conocer la teoría — en la práctica significa *URL
predecibles y un puñado de verbos*, descritos a continuación.

**Endpoint** — una URL específica a la que responde tu API, como `/api/posts` (todos los posts)
o `/api/posts/42` (el post con id 42). Una API es una colección de endpoints.

**Verbo HTTP (o método)** — *qué* quieres hacer en un endpoint. Hay cinco
que verás constantemente:

| Verbo | Significa | Ejemplo |
|---|---|---|
| `GET` | leer / obtener | "dame todos los posts" |
| `POST` | crear | "añade un post nuevo" |
| `PUT` | reemplazar | "sobrescribe el post 42 por completo" |
| `PATCH` | actualizar parcialmente | "cambia solo el título del post 42" |
| `DELETE` | eliminar | "elimina el post 42" |

**Petición / Respuesta (request / response)** — una petición es el mensaje que envías (un verbo + un endpoint
+ opcionalmente un cuerpo de datos). La respuesta es lo que vuelve (un código de estado +
normalmente un cuerpo de datos).

**JSON** — el formato de texto que las API usan para transportar datos. Se ve como
`{"title": "Hello", "published": true}` — valores etiquetados, legibles por humanos. Tanto
las peticiones como las respuestas suelen ser JSON.

**Código de estado (status code)** — un número de tres dígitos en cada respuesta que indica cómo fue:

| Código | Significado |
|---|---|
| `200` | OK — aquí están tus datos |
| `201` | Created — tu cosa nueva se guardó |
| `204` | Done — nada que devolver (p. ej. tras un borrado) |
| `400` | Bad request — enviaste algo inválido (el cuerpo dice qué) |
| `401` / `403` | No autenticado / no permitido |
| `404` | Not found (no encontrado) |
| `429` | Too many requests — ve más despacio |
| `500` | El servidor topó con un error |

**CRUD** — *Create, Read, Update, Delete* (crear, leer, actualizar, eliminar). Las cuatro cosas básicas que haces con los datos.
Una "API CRUD" simplemente significa una API que te permite hacer las cuatro. Consulta
[ViewSets](viewsets.md), que construyen una API CRUD completa a partir de una sola declaración.

**Cadena de consulta / parámetro de consulta (query string / query parameter)** — la parte `?key=value` al final de una URL,
usada para filtrar, buscar, ordenar o paginar los resultados — p. ej.
`/api/posts?status=published&page=2`. Cada `key=value` es un parámetro.

**Paginación** — dividir una lista larga de resultados en páginas para que una respuesta no sea
enorme. El **envelope (envoltorio)** es lo que rodea a la página y también te indica los
totales — p. ej. `{"count": 137, "page": 2, "results": [ … ]}`. Consulta
[Paginación](viewsets.md#pagination).

**`curl`** — una herramienta de línea de comandos para enviar peticiones a la API a mano. Los
ejemplos `curl ...` de esta documentación te permiten probar un endpoint desde una terminal
sin escribir nada de código.

---

## Bloques de construcción de Rustango

Estas son las piezas que ensamblas para construir una app. Cada una enlaza a su guía completa.

**Modelo (Model)** — una descripción de un tipo de cosa que tu app almacena, como un `Post` o
un `User`. Lo escribes como un `struct` de Rust; Rustango lo convierte en una tabla de base de
datos. Consulta la [guía del ORM](orm.md).

**Migración (Migration)** — un cambio registrado en la forma de tu base de datos (añadir una tabla,
una columna…). Generas una con `makemigrations` y la aplicas con `migrate`,
para que cada entorno acabe con la misma estructura de base de datos.

**Serializer** — el traductor entre las filas de tu base de datos y el JSON que tu API
envía y recibe. Decide qué campos son visibles, renombra o calcula
campos para la salida, y valida los datos entrantes. *Da forma* a los datos; no los
guarda (eso lo hace el modelo). Consulta la [guía de Serializers](serializers.md).

**ViewSet** — toma un modelo y un serializer y produce una **API JSON** CRUD
completa (los cinco verbos de arriba) automáticamente, para que no escribas cada
endpoint a mano. La *vista de API*. Consulta la [guía de ViewSets](viewsets.md).

**Vista HTML (template view, class-based view)** — la contraparte renderizada en el servidor
de un ViewSet: convierte un modelo en **páginas** HTML — una página de lista, una
página de detalle, y formularios de crear/editar/eliminar — renderizadas mediante plantillas Tera,
en lugar de JSON. La *vista HTML*. Consulta [Vistas HTML](html-views.md).

**Plantilla (Template)** — un archivo con marcadores de posición (Rustango usa [Tera](https://keats.github.io/tera/),
muy parecido a las plantillas de Django o a Jinja) que el servidor rellena con datos para producir
una página HTML. `{{ post.title }}` inserta un valor; `{% for … %}` itera.

**Router / montaje (mount)** — el router mapea las URL entrantes al código que las
maneja. *Montar* un ViewSet significa "adjuntar sus endpoints a tu app en una ruta
dada", p. ej. montar la API de posts en `/api/posts`. Consulta [URLs y enrutamiento](urls.md).

**Middleware (una "capa" / layer)** — código que se ejecuta *alrededor* de cada petición — antes de tu
handler y después de él — para preocupaciones transversales como el registro, el límite de tasa, las
cabeceras de seguridad o el CSRF. "Layer" es la palabra de Rustango para una pieza de
middleware. Consulta la [guía de Middleware](middleware.md).

**Pool (o executor)** — la conexión a la base de datos que tu código usa para leer y
escribir. Rustango te pide pasar el pool explícitamente a cada llamada a la base de datos
(en vez de ocultarlo en un global), para que siempre quede claro qué toca la
base de datos. Verás `&pool` como último argumento de las llamadas del ORM.

**QuerySet** — una consulta a la base de datos que construyes paso a paso en Rust
(`Post::objects().filter(...).order_by(...)`) antes de ejecutarla. Es perezosa (lazy):
nada llega a la base de datos hasta que la `fetch`eas.

**Feature flag (bandera de funcionalidad)** — un interruptor de encendido/apagado, definido en `Cargo.toml`, que incluye o
excluye una parte del framework en tiempo de compilación. Te permite mantener tu app pequeña
compilando solo lo que usas. La mayoría de las features están activadas por defecto.

**Andamiaje (Scaffolding)** — comandos generadores (`startapp`, `make:serializer`,
`make:viewset`…) que escriben archivos iniciales por ti para que no empieces desde una
página en blanco. Consulta [Andamiaje](scaffolding.md).

---

## Los datos y la base de datos

**Campo / columna (field / column)** — un dato de un modelo, como el `title` o el
`published_at` de un post. "Campo" es el lado Rust; "columna" es el lado de la base de datos; se
corresponden uno a uno.

**Clave primaria (primary key)** — el id único que identifica una fila, normalmente un
número autoincremental llamado `id`.

**Clave foránea (foreign key, FK)** — un campo de un modelo que apunta a la fila de otro modelo,
modelando una relación — p. ej. un `Post` tiene una clave foránea `author_id` que apunta
a un `Author`. Es como las filas se referencian entre sí.

**NULL / nullable** — `NULL` es la palabra de la base de datos para "sin valor / vacío". Un
campo **nullable** puede estar vacío; uno no-nullable es obligatorio.

**Tri-dialecto (tri-dialect)** — "funciona igual en las tres bases de datos soportadas" —
PostgreSQL, MySQL y SQLite. Cuando una feature es tri-dialecto, puedes cambiar de
base de datos sin cambiar tu código.

---

## Unas pocas palabras de Rust

No necesitas saber Rust para *leer* la mayoría de los ejemplos, pero estas cuatro palabras aparecen
por todas partes.

**`struct`** — un paquete con nombre de campos, como un registro o una clase que solo tiene
datos. Los modelos y los serializers son structs.

**Macro derive (`#[derive(Model)]`, `#[derive(Serializer)]`…)** — una anotación de una
línea encima de un struct que le dice al compilador que auto-genere un montón de
código por ti (el mapeo de la base de datos, la conversión a JSON, …). Es la magia que
convierte un simple struct en un modelo o serializer funcional.

**`async` / `.await`** — la forma en que Rust maneja el trabajo que implica esperar (una
consulta a la base de datos, una llamada de red). Una función marcada como `async` es "awaitable"; el
`.await` tras una llamada significa "espera aquí el resultado". Todo lo que toca la
base de datos es `async`.

**`Result` / `Option`** — cómo Rust reporta resultados en lugar de lanzar
excepciones. Un `Result` es "éxito *o* un error"; una `Option` es "un valor *o*
nada". El `?` que ves tras algunas llamadas significa "si esto falló, detente y
devuelve el error".

---

## Frameworks con los que comparamos

Esta documentación dice de vez en cuando "como X" para ayudar a los lectores que vienen de otros
ecosistemas. Las comparaciones son un extra — nunca las necesitas para seguir una guía.

**Django** — un framework web de Python popular. Rustango toma prestada buena parte de su forma
(modelos, migraciones, una interfaz de administración, los comandos `manage`).

**DRF (Django REST Framework)** — el complemento de Django para construir API REST.
Los serializers y ViewSets de Rustango están inspirados en él, así que "forma DRF" significa
"dispuesto tal como lo hace DRF" — p. ej. errores de validación devueltos como un objeto JSON
indexado por nombre de campo.

**Laravel / Rails** — frameworks web populares de PHP y Ruby, mencionados por la misma
razón de "si has usado esto, esto te resultará familiar".
