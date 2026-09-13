# Vistas HTML — páginas renderizadas en el servidor

Una vista HTML convierte un modelo en **páginas web renderizadas en el servidor** — una
página de listado, una página de detalle y formularios de creación/edición/borrado — a partir de
una sola declaración. Es la **hermana de los [ViewSets](viewsets.md)**: donde un ViewSet emite JSON
para clientes de API, una vista HTML emite una página renderizada para un navegador. Ambas se
construyen a partir del mismo `#[derive(Model)]`, y puedes servir un modelo de *ambas* formas a la
vez.

Son el equivalente en **Rustango** de las vistas genéricas basadas en clases de Django
(`ListView`, `DetailView`, `CreateView`, `UpdateView`, `DeleteView`) o de los controladores de
recursos de Laravel que devuelven vistas Blade. Renderizan mediante plantillas
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

- [Vistas de API vs vistas HTML — ¿cuál quieres?](#api-views-vs-html-views--which-do-you-want)
- [Las cinco vistas de modelo](#the-five-model-views)
- [ListView](#listview) · [DetailView](#detailview)
- [CreateView, UpdateView, DeleteView](#createview-updateview-deleteview)
- [El contexto de Tera](#the-tera-context)
- [TemplateView y RedirectView](#templateview-and-redirectview)
- [Mono-inquilino vs multi-inquilino](#single-tenant-vs-multi-tenant)
- [Servir un modelo de ambas formas](#serving-one-model-both-ways)
- [Véase también](#see-also)

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
| Análogo en Django | DRF `ModelViewSet` | vistas genéricas basadas en clases |

No tienes que elegir globalmente — elige por recurso, y puedes montar **ambas
sobre el mismo modelo** (ver [abajo](#serving-one-model-both-ways)). Reglas generales:

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

`?page=`, `?status=`, `?search=` y `?ordering=` funcionan igual que en un listado
ViewSet — la diferencia es puramente que el resultado es una página renderizada en lugar de un
sobre JSON. Usa `.context_object_name("posts")` si prefieres iterar sobre `posts` en vez de sobre
`object_list` en la plantilla.

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

let app = CreateView::for_model(Post::SCHEMA)
    .success_url("/posts")         // where to send the browser after a save
    .router("/posts", Arc::new(tera), pool);
```

La plantilla del formulario (`posts_form.html`) se comparte con UpdateView. `is_update`
distingue las dos, y `errors` transporta de vuelta cualquier mensaje de validación:

```html
<form method="post">
  <input name="title" value="{{ object.title | default(value='') }}">
  <textarea name="body">{{ object.body | default(value='') }}</textarea>
  {% for field, msgs in errors %}
    <p class="error">{{ field }}: {{ msgs | join(sep=', ') }}</p>
  {% endfor %}
  <button>{% if is_update %}Save{% else %}Create{% endif %}</button>
</form>
```

**Validación.** Las reglas del esquema (tipo, `max_length`, NOT NULL…) se aplican
automáticamente. Añade las tuyas con un validador de closure — ante un `Err`, el formulario
se vuelve a renderizar con los mensajes y un estado `422` en lugar de guardar:

```rust
use rustango::forms::FormErrors;

CreateView::for_model(Post::SCHEMA)
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

UpdateView::for_model(Post::SCHEMA)
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
    .merge(CreateView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(UpdateView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
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
| `CreateView` / `UpdateView` | `object` (vacío al crear, rellenado al actualizar), `is_update` (bool), `errors`, `values` |
| `DeleteView` | `object` (la fila a confirmar) |

Las filas se exponen como mapas planos indexados por nombre de columna (`{{ post.title }}`), con
el `NULL` de SQL renderizado como `null`. Usa `.context_object_name("posts" / "post")` para
añadir un alias más amigable junto a `object_list` / `object`.

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
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    // HTML pages for browsers:
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera, pool));
```

Ahora `GET /api/posts` devuelve el sobre JSON paginado y `GET /posts`
devuelve un listado HTML renderizado — las mismas filas, el mismo pool, dos formas. Esta
configuración exacta es lo que afirma el [test de respaldo](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs).

---

## Véase también

- [ViewSets — APIs REST CRUD](viewsets.md) — la contraparte JSON/API, en profundidad.
- [Admin](admin.md) — el admin autogenerado se construye sobre estas mismas vistas.
- [URLs y enrutamiento](urls.md) — cómo componer estos routers en tu app.
- [Serializadores](serializers.md) — da forma al JSON cuando tomas la ruta de la API.
