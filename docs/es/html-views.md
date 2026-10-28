# Vistas HTML — páginas renderizadas en el servidor

Una vista HTML convierte un modelo en **páginas web renderizadas en el servidor** — una
página de listado, una página de detalle y formularios de creación/edición/borrado — a partir de
una sola declaración. Es la **hermana de los [ViewSets](viewsets.md)**: donde un ViewSet emite JSON
para clientes de API, una vista HTML emite una página renderizada para un navegador. Ambas se
construyen a partir del mismo `#[derive(Model)]`, y puedes servir un modelo de *ambas* formas a la
vez.

Cada tipo de vista (`ListView`, `DetailView`, `CreateView`, `UpdateView`,
`DeleteView`) cubre un caso estándar y renderiza mediante plantillas
[Tera](https://keats.github.io/tera/).

[![Vistas HTML en Rustango: un modelo alimenta ListView, DetailView y CreateView/UpdateView/DeleteView, cada una renderizando una plantilla Tera en una página renderizada en el servidor](../img/html-views.png)](../img/html-views.png)

> **¿Nuevo con algún término aquí?** Si *modelo*, *plantilla*, *router* o *renderizado en el
> servidor* no te resultan familiares, el [glosario](glossary.md) explica cada uno en lenguaje
> sencillo.

> **Fuente:** `rustango::template_views` (`ListView`, `DetailView`, `CreateView`,
> `UpdateView`, `DeleteView`, `TemplateView`, `RedirectView`) — tras la
> característica `template_views` (activada por defecto).
>
> **Versión ejecutable:** el ejemplo API-vs-HTML de abajo está fijado por el
> test del framework
> [`html_and_api_contrast_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs)
> (`cargo test -p rustango --features sqlite --test html_and_api_contrast_sqlite_live`).
> Las vistas individuales están cubiertas por `template_view.rs` y
> `template_views_context_object_name_sqlite_live.rs`.

## Tabla de contenidos

- [Vistas de API vs vistas HTML — ¿cuál quieres?](#vistas-de-api-vs-vistas-html--cuál-quieres)
- [Las cinco vistas de modelo](#las-cinco-vistas-de-modelo)
- [ListView](#listview) · [DetailView](#detailview)
- [CreateView, UpdateView, DeleteView](#createview-updateview-deleteview)
- [El contexto de Tera](#el-contexto-de-tera)
- [TemplateView y RedirectView](#templateview-y-redirectview)
- [Mono-inquilino vs multi-inquilino](#mono-inquilino-vs-multi-inquilino)
- [Servir un modelo de ambas formas](#servir-un-modelo-de-ambas-formas)
- [Páginas de error](#páginas-de-error)
- [Véase también](#véase-también)

---

## Vistas de API vs vistas HTML — ¿cuál quieres?

Esta es la primera decisión. Ambas convierten un modelo en endpoints; difieren en
*qué sale* y *quién llama*.

| | **Vista de API** — [ViewSet](viewsets.md) | **Vista HTML** — esta guía |
|---|---|---|
| Módulo | `rustango::viewset` | `rustango::template_views` |
| Devuelve | **datos JSON** | una **página HTML renderizada en el servidor** |
| Pensada para | SPAs, apps móviles, otros servicios | navegadores, sitios renderizados en el servidor, CRUD tipo admin |
| Una «creación» | `POST` JSON → `201` + el nuevo objeto | `POST` de un formulario → redirección `303` a una página de éxito |
| Ante una entrada inválida | `400` + un mapa de errores JSON indexado por campo | vuelve a renderizar el formulario con los errores mostrados |
| Lee un listado como | un sobre JSON paginado | una `<table>`/bucle en tu plantilla |
| Habitualmente autenticada con | tokens / JWT / claves de API | cookies de sesión |

No tienes que elegir globalmente — elige por recurso, y puedes montar **ambas
sobre el mismo modelo** (ver [abajo](#servir-un-modelo-de-ambas-formas)). Reglas generales:

- Construyes un **backend JSON** para un framework de frontend o app móvil → ViewSet.
- Construyes un **sitio renderizado en el servidor** (el servidor devuelve páginas HTML) → vistas
  HTML.
- Necesitas ambas (una API pública *y* páginas CRUD internas) → monta ambas.

> ¿Buscas el lado JSON? Tiene su propia inmersión: [ViewSets — APIs REST
> CRUD](viewsets.md).

---

## Las cinco vistas de modelo

Cada vista es `for_model(SCHEMA)` más un `.router(prefix, tera, pool)`. Montarlas en el mismo
`prefix` (digamos `/posts`) da el conjunto clásico de URLs CRUD:

| Vista | Renderiza | Rutas montadas | Plantilla por defecto |
|---|---|---|---|
| [`ListView`](#listview) | un listado paginado | `GET <prefix>` | `<table>_list.html` |
| [`DetailView`](#detailview) | una fila | `GET <prefix>/{pk}` | `<table>_detail.html` |
| [`CreateView`](#createview-updateview-deleteview) | un formulario de nuevo registro | `GET`/`POST <prefix>/new` | `<table>_form.html` |
| [`UpdateView`](#createview-updateview-deleteview) | un formulario de edición rellenado | `GET`/`POST <prefix>/{pk}/edit` | `<table>_form.html` |
| [`DeleteView`](#createview-updateview-deleteview) | una página de confirmación | `GET`/`POST <prefix>/{pk}/delete` | `<table>_confirm_delete.html` |

`<table>` es el nombre de la tabla del modelo, así que un `Post` (tabla `posts`) busca
`posts_list.html`, `posts_detail.html`, y así sucesivamente. Reemplaza cualquiera de ellas con
`.template("my_name.html")`.

---

## ListView

Una página de listado paginado. Tú proporcionas una plantilla que itera sobre `object_list`;
la vista gestiona la paginación, el orden, el filtrado y la búsqueda a partir de los parámetros de
consulta.

```rust
use rustango::template_views::ListView;
use std::sync::Arc;
use tera::Tera;

let app = ListView::for_model(Post::SCHEMA)
    .page_size(20)                       // rows per page (?page=N to navigate)
    .order_by("published_at", true)      // default sort, true = DESC
    .filter_fields(&["status", "author_id"])  // ?status=published
    .search_fields(&["title", "body"])        // ?search=rust
    .router("/posts", Arc::new(tera), pool);
```

Un `posts_list.html` correspondiente — fíjate en `object_list` y en las variables de paginación
que la vista estampa por ti:

```html
<h1>Posts ({{ total }})</h1>
{% for post in object_list %}
  <article>
    <h2><a href="/posts/{{ post.id }}">{{ post.title }}</a></h2>
    <p>{{ post.body }}</p>
  </article>
{% endfor %}

{% if has_prev %}<a href="?page={{ page - 1 }}">← prev</a>{% endif %}
page {{ page }} / {{ total_pages }}
{% if has_next %}<a href="?page={{ page + 1 }}">next →</a>{% endif %}
```

`?page=`, `?status=` y `?search=` funcionan igual que en un listado
ViewSet — la diferencia es puramente que el resultado es una página renderizada en lugar de un
sobre JSON. Usa `.context_object_name("posts")` si prefieres iterar sobre `posts` en vez de sobre
`object_list` en la plantilla.

**`?ordering=` es la excepción.** La lista permitida de un `ListView` empieza *vacía*, así
que el parámetro se ignora hasta que nombras tú las columnas ordenables:

```rust
fn post_list() -> ListView {
    ListView::for_model(Post::SCHEMA).ordering_fields(&["title", "published_at"])
}
```

Sin esa llamada la lista vuelve al `.order_by(...)` del builder, o a PK-ASC para que la
paginación siga siendo determinista. Solo se acepta una columna, con un `-` inicial
para orden descendente.

---

## DetailView

Una fila, buscada a partir de la URL. Por defecto coincide con la clave primaria
(`/posts/42`); apúntala a otra columna con `.lookup_field("slug")` para URLs bonitas
(`/posts/my-first-post`). Una fila ausente es un `404`.

```rust
use rustango::template_views::DetailView;

let app = DetailView::for_model(Post::SCHEMA)
    .lookup_field("slug")          // GET /posts/{slug} instead of /posts/{id}
    .router("/posts", Arc::new(tera), pool);
```

La plantilla recibe la fila como `object`:

```html
<h1>{{ object.title }}</h1>
<p>{{ object.body }}</p>
<small>by author #{{ object.author_id }}</small>
```

---

## CreateView, UpdateView, DeleteView

El lado de escritura. Cada una gestiona un `GET` (renderizar un formulario / página de
confirmación) y un `POST` (hacer el trabajo, luego **redirigir**). La redirección-tras-POST es el
patrón estándar **Post/Redirect/Get** — evita que un refresco del navegador vuelva a enviar.

**CreateView** — `GET /posts/new` renderiza un formulario vacío; `POST /posts/new`
inserta la fila y hace un `303` a `success_url`:

```rust
use rustango::template_views::CreateView;

let app = CreateView::for_model(Post::SCHEMA, &["title", "body"])
    .success_url("/posts")         // where to send the browser after a save
    .router("/posts", Arc::new(tera), pool);
```

La lista de campos es obligatoria. Es la lista blanca de escritura: el formulario
muestra solo esos campos y cualquier otra clave enviada (por ejemplo `author_id=1`)
se ignora. En UpdateView, un campo listado que no llega conserva su valor; solo un
`bool` pasa a `false`, porque una casilla sin marcar no envía nada. En CreateView,
un campo listado con default de columna que no llega recibe ese default.

La plantilla del formulario (`posts_form.html`) se comparte con UpdateView. `is_update`
distingue las dos, y **`form`** transporta tanto los campos como cualquier mensaje de
validación:

```html
<form method="post">
  {{ csrf_input | safe }}
  {% for field in form.fields %}
    <label for="{{ field.name }}">{{ field.name }}</label>
    <input id="{{ field.name }}" name="{{ field.name }}" value="{{ field.value }}"
           {% if field.required %}required{% endif %}
           {% if field.max_length %}maxlength="{{ field.max_length }}"{% endif %}>
    {% if form.errors[field.name] %}
      <p class="error">{{ form.errors[field.name] }}</p>
    {% endif %}
  {% endfor %}
  <button>{% if is_update %}Save{% else %}Create{% endif %}</button>
</form>
```

Aquí hay dos cosas fáciles de equivocar. `form.errors` asocia un nombre de campo a **una
cadena**, no a una lista — aplicarle `join` es un error. Y no existe una variable `errors` de
nivel superior; iterar sobre una es un fallo de renderizado de Tera, así que la página
devuelve 500 en lugar de mostrar el mensaje.

`{{ csrf_input | safe }}` necesita el filtro: Tera autoescapa los `.html`, así que sin él el
token se renderiza como texto y todo POST es rechazado.

**Validación.** Las reglas del esquema (tipo, `max_length`, NOT NULL…) se aplican
automáticamente. Añade las tuyas con un validador de closure — ante un `Err`, el formulario
se vuelve a renderizar con los mensajes y un estado `422` en lugar de guardar:

```rust
use rustango::forms::FormErrors;

CreateView::for_model(Post::SCHEMA, &["title", "body"])
    .validator(|data| {
        let mut errs = FormErrors::default();
        if data.get("title").map_or(true, |t| t.len() < 5) {
            errs.add("title", "must be at least 5 characters");
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    })
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

También puedes reutilizar los validadores de una estructura `#[derive(Form)]` con `.form::<F>()`
(solo validación por ahora — ver la documentación de la API).

**UpdateView** — `GET /posts/{pk}/edit` renderiza el mismo formulario rellenado desde la
fila (`object` está poblado, `is_update` es `true`); `POST` actualiza y hace un `303`.

```rust
use rustango::template_views::UpdateView;

UpdateView::for_model(Post::SCHEMA, &["title", "body"])
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

**DeleteView** — `GET /posts/{pk}/delete` renderiza una página de confirmación
(`posts_confirm_delete.html`, con `object`); `POST` borra y hace un `303`.

```rust
use rustango::template_views::DeleteView;

DeleteView::for_model(Post::SCHEMA)
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

Monta las cinco en el mismo prefijo y tienes un CRUD HTML completo:

```rust
let app = axum::Router::new()
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(CreateView::for_model(Post::SCHEMA, &["title", "body"]).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(UpdateView::for_model(Post::SCHEMA, &["title", "body"]).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(DeleteView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera, pool));
```

---

## El contexto de Tera

Cada vista estampa un contexto consistente para que las plantillas se porten limpiamente entre
ellas:

| Vista | Variables disponibles en la plantilla |
|---|---|
| `ListView` | `object_list` (las filas de la página), `page`, `page_size`, `total`, `total_pages`, `has_next`, `has_prev` |
| `DetailView` | `object` (la fila) |
| `CreateView` | `form` (`.fields`, `.errors`), `is_create` (true), `is_update` (false) — **sin `object`** |
| `UpdateView` | `form` (`.fields`, `.errors`), `object` (la fila), `pk`, `is_create` (false), `is_update` (true) |
| `DeleteView` | `object` (la fila a confirmar) |

Las filas se exponen como mapas planos indexados por nombre de columna (`{{ post.title }}`), con
el `NULL` de SQL renderizado como `null`. Usa `.context_object_name("posts" / "post")` para
añadir un alias más amigable junto a `object_list` / `object`.

Cada entrada de `form.fields` lleva `name`, `column`, `ty`, `required`, `max_length` y
`value`. `form.errors` está indexado por nombre de campo y contiene un mensaje por campo, no
una lista. Cada vista estampa además `csrf_token` y `csrf_input`, y cada router de vista
rechaza un POST sin un token que coincida.

`CreateView` no estampa ningún `object`, así que `{{ object.title }}` en un formulario de
creación queda indefinido en lugar de vacío — usa `{{ field.value }}` de `form.fields`, que
está poblado en ambos casos.

---

## TemplateView y RedirectView

Dos ayudantes sin modelo para las páginas que todo sitio tiene:

**TemplateView** — renderiza una plantilla estática con un contexto fijo (una página «acerca de»,
una landing page). Sin modelo, sin base de datos:

```rust
use rustango::template_views::TemplateView;

let app = TemplateView::new("about.html")
    .context_value("title", "About us")
    .router("/about", Arc::new(tera));
```

**RedirectView** — una redirección permanente o temporal en una URL (para páginas movidas):

```rust
use rustango::template_views::RedirectView;

let app = RedirectView::to("/posts").router("/old-posts");
```

---

## Mono-inquilino vs multi-inquilino

Cada vista de modelo trae dos constructores de router — el mismo builder, elige el que
coincida con cómo tu app gestiona las conexiones a la base de datos:

- **`.router(prefix, tera, pool)`** — mono-inquilino; captura un pool en el momento del montaje.
  Esto es lo que usan los ejemplos de arriba.
- **`.tenant_router(prefix, tera)`** — multi-inquilino; resuelve una conexión por petición desde
  el extractor [`Tenant`](https://docs.rs). Disponible con las características
  `template_views` + `tenancy`. Las plantillas se portan sin cambios entre ambos.

Esto refleja la división de ViewSet (`router` / `router_pool` vs `tenant_router`).

---

## Servir un modelo de ambas formas

No estás limitado a una sola puerta de entrada. Monta una API JSON *y* páginas HTML sobre el
mismo modelo y pool — una API pública para clientes, páginas renderizadas en el servidor para
personas:

```rust
use rustango::viewset::ViewSet;
use rustango::template_views::{ListView, DetailView};

let app = axum::Router::new()
    // JSON for API clients:
    .merge(ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"]).router_pool("/api/posts", pool.clone()))
    // HTML pages for browsers:
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera, pool));
```

Ahora `GET /api/posts` devuelve el sobre JSON paginado y `GET /posts`
devuelve un listado HTML renderizado — las mismas filas, el mismo pool, dos formas. Esta
configuración exacta es lo que afirma el [test de respaldo](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs).

---

## Páginas de error

Una capa convierte cada 4xx/5xx de tu app en una página para personas y deja en
paz a los clientes de API. Está **desactivada por defecto**; nada cambia hasta
que la activas. Necesita la feature `template_views`.

### Activarla

En el servidor multi-inquilino, la capa lo envuelve todo: tu API, el login del
inquilino, el admin del inquilino, la consola de operador y las capas propias
del framework.

```rust
use std::sync::Arc;
use rustango::errors::ErrorPagesLayer;

let tera = Arc::new(rustango::template_extensions::html_tera_from_glob("templates/**/*.html")?);
let app = rustango::server::Builder::from_env().await?
    .api(my_app::urls::api())
    .error_pages(ErrorPagesLayer::new().templates(tera));
```

`AppBuilder::error_pages` hace lo mismo para una app de un solo pool. En un
router propio, móntala con `ErrorPagesRouterExt` y añade el fallback 404:

```rust
use rustango::errors::{ErrorPagesLayer, ErrorPagesRouterExt as _};

let app = Router::new()
    .route("/projects", get(projects))
    .fallback(rustango::errors::not_found)
    .error_pages(ErrorPagesLayer::new().templates(tera.clone()));
```

Con `Cli` y un archivo de configuración, usa en su lugar la sección `[errors]`:

```toml
[errors]
enabled = true            # default false
templates = "templates"   # template root holding errors/404.html etc.; default "templates"
htmx_target = "#flash"    # HX-Retarget for htmx errors; default "#flash", "" sends none
```

Un directorio `templates` que no existe o no se parsea queda en el log, y se
usan las páginas integradas.

### Qué recibe cada cliente

| Petición | Respuesta |
|---|---|
| Navegación del navegador (`Accept: text/html`, sin `HX-Request`) | Página HTML completa |
| htmx (`HX-Request`) | Fragmento HTML, más `HX-Retarget: <htmx_target>` y `HX-Reswap: innerHTML show:window:top` |
| Cualquier otro cliente (`fetch`, curl) | El cuerpo sin cambios; un cuerpo vacío recibe el mensaje como texto plano |
| JSON / problem+json | Sin cambios, salvo que lleve un `Message` y pregunte un navegador o htmx |
| `HEAD` | Sin cambios |
| Una respuesta marcada con `rustango::errors::KeepBody` | Sin cambios |

**También se reemplaza un cuerpo de error `text/html`**, salvo que la respuesta
lleve `KeepBody`: marca toda página de error HTML que construyas tú.

Solo se reemplazan cuerpos de texto (`text/plain`, `text/html`, ninguno) y JSON
con mensaje; imágenes y otros tipos pasan. Una respuesta vestida conserva su
estado y cabeceras (también `Retry-After`) y recibe `Vary: Accept, HX-Request`.

### Plantillas

Gana la primera plantilla que exista en tu Tera:

1. `errors/<code>.html`, p. ej. `errors/404.html`
2. `errors/4xx.html` o `errors/5xx.html`
3. `errors/default.html`
4. la página integrada

Los fragmentos htmx usan `errors/partial.html` y luego el fragmento integrado.
Una plantilla que falla al renderizar queda en el log y se usa la página
integrada; una página de error nunca vuelve vacía.

`templates/errors/404.html`:

```html
{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ code }} · {{ title }}</h1>
  <p>{{ message }}</p>
  <p>Nothing at <code>{{ path }}</code>.</p>
  {% if user %}<p>Signed in as {{ user.username }}.</p>{% endif %}
  <a href="/">Back to the start</a>
</section>
{% endblock content %}
```

`templates/errors/5xx.html`:

```html
{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ title }}</h1>
  <p>{{ message }}</p>
  <form method="post" action="/feedback">{{ csrf_input | safe }}
    <button>Tell us what you were doing</button>
  </form>
</section>
{% endblock content %}
```

Contexto:

| Clave | Valor |
|---|---|
| `code` | El estado, p. ej. `404` |
| `title` | Un título corto por estado («Not found») |
| `message` | Qué salió mal, para el usuario (ver abajo) |
| `path` | La ruta de la petición |
| `user` | `{ username, is_superuser }` del usuario del inquilino con sesión, o null |
| `username`, `is_superuser` | Lo mismo, plano |
| `csrf_token`, `csrf_input` | Un token CSRF y su input oculto, con el nombre de cookie de tu capa CSRF. La cookie solo se pone si la plantilla los usa |
| `csp_nonce` | El nonce CSP de esta petición, o `""` |
| `brand_name`, `brand_logo_url`, `brand_css`, `theme_mode` | De `[brand]` |

`user` necesita la feature `tenancy` y un host de inquilino; en otro caso es
null. Las plantillas son por app: todos los inquilinos reciben las mismas.

### La página integrada

HTML autónomo: sin CDN, sin script, CSS inline con el nonce CSP de la petición,
los tokens de tema del admin (claro y oscuro). Con `[errors]` en los settings
muestra `[brand] name` y `logo_url`; `primary_color` y `theme_mode` fijan los
colores (necesitan la feature `tenancy`, cuyo código de marca reutiliza).
`ErrorPagesLayer::brand(&settings.brand)` hace lo mismo en código.

### El mensaje

Un handler le da palabras a la página con `rustango::errors::Message`, una
extensión de respuesta, o con `PageError`, que lleva uno:

```rust
use rustango::errors::{Message, PageError};

async fn project(Path(slug): Path<String>) -> Result<Html<String>, PageError> {
    let project = find_project(&slug)
        .await
        .map_err(PageError::server)?            // logged; the page says something generic
        .ok_or_else(|| PageError::not_found("No project with that name."))?;
    Ok(Html(render_project(&project)))
}

async fn archived() -> impl IntoResponse {
    (StatusCode::GONE, Message::new("This project was archived."))
}
```

Sin `Message`, un cuerpo 4xx `text/plain` de hasta 2 KiB es el mensaje (los
rechazos de axum, la comprobación CSRF); si no, el texto por defecto del estado.
La primera letra pasa a mayúscula.

Un 5xx nunca muestra su causa: la página usa el texto genérico, y
`PageError::server(e)` registra `e` y envía el mismo cuerpo opaco que cualquier
otro 5xx. Solo cuando `RUSTANGO_DISCLOSE_ERRORS` está definida una página 5xx
muestra su mensaje o su cuerpo — la misma regla que para cualquier cuerpo 5xx
del framework.

### Qué cubre

Montada con `Builder::error_pages` (o `[errors]`), la capa queda dentro de las
cabeceras de seguridad y fuera de todo lo demás, así que viste:

- rutas y hosts desconocidos (404), y los 405;
- el 500 del panic guard;
- el 403 de la capa CSRF, con su motivo;
- las guardas `login_required_or_401` / `*_or_403` (401, 403);
- el límite de cuerpo (413), el timeout de petición (504), el rate limit y el throttle de login (429);
- los rechazos de axum (400, 415, 422);
- las denegaciones y 404 del admin del inquilino, y las rutas desconocidas de la consola de operador.

Las páginas con sus propios controles conservan su aspecto: toda página de
error HTML del admin, del admin de tenant, de la consola de operador y de las
vistas genéricas, como un formulario mostrado de nuevo con su error, la página
«admin access required» (tiene un formulario para cerrar sesión) y la página
«table not migrated». Marca tus propias respuestas con `KeepBody` para hacer lo
mismo.

### Migrar desde un middleware de errores escrito a mano

Una app que tenía su propio middleware `error_pages` puede borrar:

- el middleware (`from_fn(error_pages)`) y su `.fallback(not_found)`;
- su tabla de títulos/mensajes: `rustango::errors::error_words` es la misma tabla;
- su extensión de mensaje privada: devuelve `PageError` o `Message`;
- el clonado de cabeceras/extensiones que hacía para poner el CSRF y encontrar al usuario.

Conserva `templates/error.html` como `templates/errors/default.html` y la alerta
htmx como `templates/errors/partial.html`; las claves del contexto son las mismas.

---

## Véase también

- [ViewSets — APIs REST CRUD](viewsets.md) — la contraparte JSON/API, en profundidad.
- [Admin](admin.md) — el admin autogenerado se construye sobre estas mismas vistas.
- [URLs y enrutamiento](urls.md) — cómo componer estos routers en tu app.
- [Serializadores](serializers.md) — da forma al JSON cuando tomas la ruta de la API.
