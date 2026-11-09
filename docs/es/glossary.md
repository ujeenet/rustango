# Glosario

Una referencia en lenguaje sencillo de las palabras usadas en esta documentación. Si un
término de una guía te resulta desconocido, búscalo aquí primero. Las definiciones son
deliberadamente informales — las guías en profundidad tienen los detalles precisos.

Si nunca has construido una API web antes, lee [Fundamentos de las API web](#fundamentos-de-las-api-web)
de principio a fin; es una introducción de cinco minutos. Todo lo demás está pensado para
consultarse sobre la marcha.

## Tabla de contenidos

- [Fundamentos de las API web](#fundamentos-de-las-api-web) — qué es una API, en términos cotidianos
- [Bloques de construcción de Rustango](#bloques-de-construcción-de-rustango) — las piezas que ensamblas
- [Los datos y la base de datos](#los-datos-y-la-base-de-datos)
- [Inicio de sesión y seguridad](#inicio-de-sesión-y-seguridad) — sesiones, tokens, contraseñas, firmas
- [Caché, archivos, correo y jobs](#caché-archivos-correo-y-jobs)
- [Idiomas](#idiomas) — traducir tu app
- [Tiempo real y agentes de IA](#tiempo-real-y-agentes-de-ia) — actualizaciones en vivo, MCP
- [Multi-tenancy](#multi-tenancy) — solo si sirves a varios clientes desde un único despliegue
- [Unas pocas palabras de Rust](#unas-pocas-palabras-de-rust) — para que los bloques de código no den miedo

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
| `422` | No procesable — los datos no pasaron la validación (el cuerpo nombra el campo) |
| `401` / `403` | No autenticado / no permitido |
| `404` | Not found (no encontrado) |
| `429` | Too many requests — ve más despacio |
| `500` | El servidor topó con un error |

**401 / 403** — `401` significa "no sabemos quién eres": inicia sesión o envía un
token. `403` significa "sabemos quién eres, pero no puedes hacer esto".

**Ruta (route)** — un patrón de URL más el verbo al que responde, como
`GET /api/posts/{id}`, unido al código que lo atiende. Consulta
[URLs y enrutamiento](urls.md).

**Handler** — la función que se ejecuta para una ruta. Recibe la petición y
devuelve la respuesta.

**Cookie** — un pequeño texto que el servidor pide al navegador que guarde. El
navegador lo envía en cada petición posterior al mismo sitio. Así un navegador
sigue con la sesión iniciada.

**Multipart (subida de formulario)** — el formato de petición que usa un
navegador para enviar un formulario con archivos. Cada campo y cada archivo
viaja como una parte propia. Consulta [Archivos](files.md#paso-3--acepta-una-subida).

**Renderizado en el servidor (server-rendered)** — una página HTML que el
servidor construye y envía completa, así que el navegador solo la muestra.
Consulta [Vistas HTML](html-views.md).

**CRUD** — *Create, Read, Update, Delete* (crear, leer, actualizar, eliminar). Las cuatro cosas básicas que haces con los datos.
Una "API CRUD" simplemente significa una API que te permite hacer las cuatro. Consulta
[ViewSets](viewsets.md), que construyen una API CRUD completa a partir de una sola declaración.

**Cadena de consulta / parámetro de consulta (query string / query parameter)** — la parte `?key=value` al final de una URL,
usada para filtrar, buscar, ordenar o paginar los resultados — p. ej.
`/api/posts?status=published&page=2`. Cada `key=value` es un parámetro.

**Paginación** — dividir una lista larga de resultados en páginas para que una respuesta no sea
enorme. El **envelope (envoltorio)** es lo que rodea a la página y también te indica los
totales — p. ej. `{"count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ]}`. Consulta
[Paginación](viewsets.md#paginación).

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

**Plantilla (Template)** — un archivo con marcadores de posición (Rustango usa [Tera](https://keats.github.io/tera/))
que el servidor rellena con datos para producir
una página HTML. `{{ post.title }}` inserta un valor; `{% for … %}` itera.

**Router / montaje (mount)** — el router mapea las URL entrantes al código que las
maneja. *Montar* un ViewSet significa "adjuntar sus endpoints a tu app en una ruta
dada", p. ej. montar la API de posts en `/api/posts`. Consulta [URLs y enrutamiento](urls.md).

**Middleware (una "capa" / layer)** — código que se ejecuta *alrededor* de cada petición — antes de tu
handler y después de él — para preocupaciones transversales como el registro, el límite de tasa, las
cabeceras de seguridad o el CSRF. "Layer" es la palabra de Rustango para una pieza de
middleware. Consulta la [guía de Middleware](middleware.md).

**Extractor** — un argumento del handler que saca una cosa de la petición por ti,
como el usuario con sesión iniciada, el cuerpo JSON o el tenant actual. Si falta,
el extractor rechaza la petición antes de que se ejecute tu código.

**Pool** — el handle de base de datos que tu código usa para leer y escribir.
Rustango te pide pasarlo explícitamente a cada llamada a la base de datos (en vez
de ocultarlo en un global), para que siempre quede claro qué toca la base de
datos. Verás `&pool` como último argumento de las llamadas del ORM.
`rustango::sql::Pool` es un enum sobre los tres backends, y es lo que reciben los
métodos de uso diario.

**Executor** — *no* es un sinónimo de pool. Un executor es una sola conexión o una
transacción abierta: aquello sobre lo que realmente se ejecuta una sentencia. Los
métodos que terminan en `_on` reciben uno, y así es como metes varias sentencias
en una misma transacción. `fetch_on`, `first_on`, `count_on` y `exists_on` aceptan
cualquier executor en los tres backends; el resto de la familia `_on` sigue siendo
solo para Postgres. La distinción decide qué métodos existen en tu build, así que
[api-conventions](api-conventions.md#funciones) explica la regla completa.

**QuerySet** — una consulta a la base de datos que construyes paso a paso en Rust
(`Post::objects().filter(...).order_by(...)`) antes de ejecutarla. Es perezosa (lazy):
nada llega a la base de datos hasta que la `fetch`eas.

**Feature flag (bandera de funcionalidad)** — un interruptor de encendido/apagado, definido en `Cargo.toml`, que incluye o
excluye una parte del framework en tiempo de compilación. Te permite mantener tu app pequeña
compilando solo lo que usas. La mayoría de las features están activadas por defecto.

**Reverse (construir una URL por su nombre)** — generar una URL a partir de su
nombre en lugar de escribir la ruta, para que cambiar una ruta no rompa enlaces.
Consulta [Nombres de URL y reverse](urls.md).

**Namespace / namespacing** — un prefijo en el nombre de una URL, como
`blog:post-detail`, que mantiene separados los nombres de distintas partes de la
app. Consulta [Namespacing](urls.md#namespacing).

**Fieldset (grupo de campos)** — un grupo de campos con título en un formulario
de edición del admin, como "Content" y "Publishing". Consulta
[Admin](admin.md#el-formulario-de-cambio).

**Audit trail (registro de auditoría)** — un registro de quién cambió qué fila,
cuándo, y cuáles eran los valores antiguos y nuevos. Consulta
[Admin](admin.md#registro-de-auditoría).

**OpenAPI** — un formato de archivo estándar que describe cada endpoint de una
API. Las herramientas lo leen para mostrar documentación interactiva y generar
código cliente. Consulta [OpenAPI](openapi.md).

**Schema (esquema)** — una descripción de la forma de los datos. En OpenAPI
describe un cuerpo JSON. En una base de datos es el conjunto de tablas, y en
Postgres también es un grupo con nombre de tablas dentro de una base de datos.

**Fixture** — datos de prueba que preparas antes de un test, para que cada test
empiece desde un estado conocido. Consulta [Pruebas](testing.md).

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

**ORM** — *Object-Relational Mapper.* La parte del framework que convierte tus
structs de Rust en filas de la base de datos y al revés, para que consultes en
Rust en lugar de escribir SQL a mano. Consulta la [guía del ORM](orm.md).

**Transacción** — un grupo de cambios en la base de datos que tienen éxito o
fallan juntos. O se guardan todos, o ninguno.

**Rollback (reversión)** — deshacer todos los cambios hechos dentro de una
transacción, como si nunca hubieran ocurrido. Los tests lo usan para dejar la
base de datos limpia. Consulta
[Pruebas de base de datos con reversión](testing.md#pruebas-de-base-de-datos-con-reversión).

**Tri-dialecto (tri-dialect)** — "funciona igual en las tres bases de datos soportadas" —
PostgreSQL, MySQL y SQLite. Cuando una feature es tri-dialecto, puedes cambiar de
base de datos sin cambiar tu código.

---

## Inicio de sesión y seguridad

**Sesión (session)** — la memoria que guarda el servidor de un usuario con sesión
iniciada entre peticiones. El navegador solo guarda un id en una cookie; los
datos se quedan en el servidor. Consulta [Sesiones](auth-sessions.md).

**Id opaco** — un id que no significa nada por sí solo, solo una cadena aleatoria
larga. Solo el servidor puede ver a qué apunta.

**Token** — una cadena que envía un cliente para demostrar quién es, normalmente
en la cabecera `Authorization`. Trátalo como una contraseña.

**Bearer token** — un token enviado como `Authorization: Bearer <token>`. Quien
lo tiene ("lo porta") entra, así que mantenlo en secreto. Consulta
[Claves de API](auth-api-keys.md).

**JWT** — *JSON Web Token.* Un token que lleva datos firmados (quién es el
usuario, cuándo caduca). El servidor puede comprobarlo sin consultar la base de
datos. Consulta [JWT](auth-jwt.md).

**Claims** — los datos dentro de un JWT, como el id del usuario y la hora de
caducidad.

**Sin estado (stateless)** — el servidor no guarda nada entre peticiones. Todo lo
que necesita llega en la propia petición, como con un JWT.

**Token de acceso / token de refresco** — un token de vida corta que se usa en
cada petición, más uno de vida larga que solo sirve para obtener un nuevo token
de acceso cuando el anterior caduca. Consulta [API JWT](auth-jwt-api.md).

**Rotación de tokens** — cada vez que se usa un token de refresco, se sustituye
por uno nuevo y el anterior deja de funcionar. Un token de refresco robado solo
sirve una vez.

**Revocación** — anular un token o una sesión antes de que caduque, por ejemplo
al cerrar sesión. Consulta
[Revocación](auth-jwt-api.md#revocación-y-el-almacén-de-jti).

**Caducidad (expiry)** — el momento en que un token, un enlace o una sesión deja
de funcionar.

**Secreto / secreto compartido** — un valor privado que firma o comprueba datos.
Un secreto compartido lo conocen ambos lados, como el cliente y el servidor.
Nunca lo pongas en el código ni en los logs.

**Hash** — una huella de un solo sentido de unos datos. Puedes comprobar una
contraseña contra su hash, pero no puedes recuperar la contraseña a partir de
él. Consulta [Contraseñas](auth-passwords.md).

**Sal (salt)** — datos aleatorios que se mezclan con cada contraseña antes de
calcular el hash, para que dos contraseñas iguales tengan hashes distintos.

**Argon2id** — el método de hash de contraseñas que usa Rustango. Es lento a
propósito, lo que hace caro adivinar contraseñas.

**Cadena PHC** — la forma de texto estándar de un hash de contraseña guardado.
Contiene el método, sus ajustes, la sal y el hash en una sola cadena que empieza
por `$argon2id$`.

**HMAC** — una firma hecha con unos datos más una clave secreta. Quien tenga la
clave puede comprobar que los datos no se cambiaron. Consulta
[Firma de peticiones HMAC](auth-hmac.md) y [Flujos de cuenta](auth-flows.md).

**Comparación en tiempo constante** — comparar dos secretos de forma que siempre
tarde lo mismo, para que un atacante no aprenda nada de cuánto tardó la
comprobación.

**Replay (repetición)** — un atacante vuelve a enviar una petición que capturó
antes. Un límite de tiempo en las peticiones firmadas lo bloquea. Consulta
[Desfase de reloj y replay](auth-hmac.md#desfase-de-reloj-y-replay).

**URL prefirmada (presigned URL)** — un enlace a un archivo con una firma y una
caducidad incluidas, así que cualquiera con el enlace puede descargar (o subir)
ese archivo durante poco tiempo. Consulta [Archivos](files.md).

**CSRF** — *Cross-Site Request Forgery.* Otro sitio engaña a un navegador con
sesión iniciada para que envíe una petición a tu app. Un token CSRF en cada
formulario lo bloquea. Consulta [Protección CSRF](middleware.md#protección-csrf).

**Codename de permiso** — el nombre de un permiso, escrito como
`{table}.{action}`, por ejemplo `post.add`. Las rutas lo comprueban para decidir
quién puede hacer qué. Consulta
[Backends de autenticación](auth-backends.md#permisos-require_perm).

**OAuth** — una forma estándar de que una app obtenga acceso limitado en nombre
de un usuario sin ver su contraseña. Es la base de los botones "Iniciar sesión
con …". Consulta [SSO](sso.md).

---

## Caché, archivos, correo y jobs

**Backend (backend de almacenamiento, backend de correo, backend de caché)** — el
lugar o servicio real detrás de una interfaz común: disco local o S3 para
archivos, SMTP o la consola para el correo, memoria o Redis para una caché.
Puedes cambiarlo sin tocar el código que lo llama. Los backends de autenticación
son la misma idea: cada uno es una forma de reconocer a un usuario. Consulta
[Backends de autenticación](auth-backends.md).

**Caché** — un almacén rápido que guarda copias de datos que lees a menudo, para
no calcularlos ni buscarlos cada vez. Consulta [Caché](caching.md).

**Clave de caché** — el nombre con el que se guarda un valor en la caché, como
`post:42:summary`.

**TTL** — *time to live.* Cuánto tiempo se guarda un valor en la caché antes de
caducar. Consulta [TTL y expiración](caching.md#ttl-y-expiración).

**Almacenamiento de objetos (object storage)** — un servicio que guarda archivos
("objetos") por HTTP, como Amazon S3 o Azure Blob Storage. Consulta
[Archivos](files.md).

**Mailer** — la parte de tu app que envía correo. En desarrollo imprime el correo
en la terminal; en producción lo envía de verdad. Consulta [Correo](email.md).

**SMTP** — el protocolo estándar para enviar correo a un servidor de correo.

**Correo transaccional** — un correo que tu app envía por algo que hizo un
usuario, como un restablecimiento de contraseña o un recibo.

**Cola de jobs** — una lista de tareas que esperan para ejecutarse en segundo
plano, para que una petición pueda responder enseguida. Consulta
[Jobs en segundo plano](jobs.md).

**Worker** — una tarea en segundo plano que toma jobs de la cola y los ejecuta.

**Reintento y backoff** — volver a ejecutar un job fallido, esperando un poco más
después de cada fallo. Consulta [Reintentos y backoff](jobs.md#reintentos-y-backoff).

**Dead-letter** — adonde va un job cuando ha fallado del todo, para que puedas
revisarlo más tarde. Consulta [El handler dead-letter](jobs.md#el-handler-dead-letter).

---

## Idiomas

**Locale** — un idioma, a veces con región, como `en`, `es` o `pt-BR`. Consulta
[i18n](i18n.md).

**Catálogo de mensajes** — el archivo con los textos traducidos de un locale.

**Pluralización** — elegir la forma correcta de una palabra para un número, como
"1 archivo" y "2 archivos". Algunos idiomas tienen más de dos formas. Consulta
[Pluralización](i18n.md#pluralización).

**Accept-Language** — una cabecera que envía el navegador con los idiomas que
prefiere el usuario. Consulta [Negociar el idioma](i18n.md#negociar-el-idioma).

**RTL** — *right-to-left.* Idiomas como el árabe y el hebreo, que se escriben de
derecha a izquierda. Consulta
[Idiomas de derecha a izquierda](i18n.md#idiomas-de-derecha-a-izquierda).

---

## Tiempo real y agentes de IA

**WebSocket** — una conexión que se mantiene abierta para que el navegador y el
servidor puedan enviar mensajes en cualquier momento. Consulta
[WebSockets y SSE](websockets.md).

**SSE** — *Server-Sent Events.* Una conexión que se mantiene abierta para que el
servidor envíe actualizaciones al navegador. Los mensajes van en un solo
sentido, del servidor al navegador.

**Broadcast (difusión)** — enviar un mensaje a todos los oyentes conectados a la
vez.

**Keep-alive / ping** — un pequeño mensaje que se envía de vez en cuando para que
un proxy no cierre una conexión inactiva y para notar una conexión muerta.

**Back-pressure (contrapresión)** — lo que pasa cuando un oyente lee más despacio
de lo que llegan los mensajes. El emisor tiene que esperar, guardar en búfer o
descartar mensajes.

**MCP** — *Model Context Protocol.* Una forma estándar de que un agente de IA
descubra y llame a las herramientas que ofrece tu app. Consulta
[Servidor MCP](mcp.md).

**JSON-RPC** — un protocolo sencillo para llamar a una función con nombre en un
servidor, con la petición y la respuesta en JSON. MCP lo usa.

**Agente (agent)** — un programa de IA, normalmente guiado por un modelo de
lenguaje grande, que llama a herramientas para hacer un trabajo.

**Tool, resource, prompt (MCP)** — lo que un servidor MCP ofrece a un agente: un
tool es una acción que puede llamar, un resource son datos que puede leer y un
prompt es una instrucción ya preparada.

---

## Multi-tenancy

Detrás de la feature `tenancy`. Sáltate esta sección si construyes una app
corriente de un solo cliente — nada de esto aplica.

**Multi-tenancy** — ejecutar un único despliegue que sirve a varios clientes,
cada uno viendo solo sus propios datos. Rustango resuelve a qué cliente
pertenece una petición a partir de su nombre de host y luego la enruta a los
datos de ese cliente durante el resto de la petición.

**Tenant (u org)** — un cliente en un despliegue así. Un tenant tiene un slug
(`acme`), un patrón de host (`acme.example.com`) y sus propios usuarios. `Org`
es la fila del registro; «tenant» es lo que esa fila describe.

**Registro (registry)** — la pequeña base de datos que lista los tenants: quiénes
son, dónde viven sus datos, si están activos. Distinta de los datos de cualquier
tenant, y la única base de datos que el framework siempre necesita.

**Modo de almacenamiento (storage mode)** — cómo se mantienen separados los datos
de un tenant de los de sus vecinos: una base de datos aparte, o un esquema aparte
dentro de una compartida. Se elige por tenant al aprovisionar.

**Operador (operator)** — un administrador del *despliegue*, no de un tenant. Los
operadores crean tenants y enlazan nombres de host; no son usuarios de ningún
tenant y viven en el registro. Consulta
[la consola de operador](operator-console.md).

**Consola de operador (operator console)** — la interfaz web que usan los
operadores: aprovisionar tenants, enlazar nombres de host, gestionar otros
operadores, leer el rastro de auditoría. Casi cada acción en ella es también un
verbo `manage`, así que puede guionizarse.

**Aprovisionamiento (provisioning)** — crear un tenant: fabricar su base de datos
o esquema, aplicarle las migraciones y registrarlo en el registro.

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
