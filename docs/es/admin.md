# El panel de administración

**Rustango** genera una interfaz de administración completa a partir de tus modelos —
la misma idea que el admin de Django o un panel Nova/Filament de Laravel, pero con
**cero código repetitivo por modelo**. Añade `#[derive(Model)]`, monta el admin una
vez, y cada modelo obtiene una vista de lista con búsqueda, filtros, ordenación,
paginación y acciones masivas; un formulario de creación/edición agrupado en conjuntos
de campos; edición inline de hijos; un registro de auditoría por fila; y una referencia
de modelos en vivo. Todo lo que sigue se configura de forma declarativa en un bloque
`admin(...)` sobre el derive, o con un puñado de métodos del `Builder` y macros de
registro a nivel de módulo.

[![El admin autogenerado: una lista de entradas con facetas de filtro, búsqueda, acciones masivas y paginación — todo desde un solo bloque `admin(...)`](../img/admin.png)](../img/admin.png)

> **Fuente:** `rustango::admin` (las opciones del derive `admin(...)`, la API del
> `Builder` y las macros de registro) — tras la característica `admin` (activada por
> defecto).
>
> **Versión ejecutable:** cada característica de esta página se ejercita en un ejemplo
> probado y compilable en
> [`crates/rustango/examples/admin_demo`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/admin_demo).
> Las capturas de esta página provienen de ese ejemplo. Si un fragmento parece extraño,
> compáralo con él.

> **¿Un término aquí es nuevo para ti?** *model*, *fieldset*, *audit trail* — consulta el
> [glosario](glossary.md).

---

## Tabla de contenidos
- [Montarlo](#mount-it) · [La página de inicio](#the-home-page)
- [Configurar un modelo: el bloque `admin(...)`](#configure-a-model-the-admin-block)
- [La vista de lista](#the-list-view) — columnas, búsqueda, filtros, jerarquía de fechas, ordenación, paginación
- [El formulario de cambio](#the-change-form) — conjuntos de campos, widgets, edición de FK, campos precompletados y de solo lectura
- [Inlines](#inlines) · [Acciones masivas](#bulk-actions) · [Registro de auditoría](#audit-trail)
- [Columnas calculadas y filtros personalizados](#computed-columns-and-custom-filters)
- [Vistas, querysets y permisos personalizados](#custom-views-querysets-and-permissions)
- [Autenticación](#authentication) · [Temas y marca](#theming-and-branding)
- [Referencia del `Builder`](#builder-reference) · [Referencia de rutas](#routes-reference)
- [La referencia de modelos (`__docs`)](#the-model-reference) · [Prueba el ejemplo](#try-the-example)

---

## Montarlo

> **El admin está abierto por defecto.** Descubre y sirve *todos* los modelos
> automáticamente — listar, crear, editar, eliminar — sin autenticación hasta que la
> añadas. No lo expongas públicamente antes de conectar el inicio de sesión: consulta
> [Autenticación](#authentication) más abajo.

El admin es un `axum::Router` que construyes a partir de un pool de base de datos y
anidas bajo una ruta:

```rust
use rustango::admin;

let admin_router = admin::Builder::new(pool.clone())
    .title("Admin Demo")
    .subtitle("rustango auto-admin showcase")
    .admin_prefix("/admin")          // MUST match the nest path below
    .build();

let api = axum::Router::new().nest("/admin", admin_router);
```

El auto-admin descubre cada `#[derive(Model)]` de tu binario **automáticamente** a
través del registro `inventory` — no registras los modelos uno por uno. Abre
`http://localhost:8080/admin` y aparecen agrupados en la barra lateral.

> **`admin_prefix` debe coincidir con la ruta de anidamiento.** El admin construye sus
> enlaces y acciones de formulario a partir de `admin_prefix` (por defecto `/__admin`).
> Si anidas en `/admin` pero dejas el prefijo por defecto, cada enlace devuelve 404.
> Ajústalos al mismo valor.

> **Vincular el registro.** El registro de inventory solo se vincula al binario final si
> los tipos de modelo se referencian en algún lugar. Un crate de biblioteca cuyos
> modelos no se usan de otro modo puede necesitar un empujón con
> `let _ = std::any::type_name::<Post>();` en `main` (el ejemplo lo hace) — de lo
> contrario el enlazador los descarta y nunca aparecen.

### La página de inicio

La raíz del admin (`GET /<prefix>`) lista cada modelo registrado, agrupado por app, con
el nombre de tabla y el recuento de campos de cada modelo — más un feed de **Recent
actions** con los cambios auditados más recientes.

[![La página de inicio del admin: cada modelo registrado agrupado por app con recuentos de tabla y campos, y un feed de actividad de acciones recientes](../img/admin-home.png)](../img/admin-home.png)

---

## Configurar un modelo: el bloque `admin(...)`

Todo lo relativo a cómo aparece un modelo se establece en un bloque `admin(...)` sobre
el derive. Aquí tienes el `Post` de exhibición del ejemplo, que ejercita casi todos los
controles:

```rust
#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display       = "id, title, author_id, status, view_count, published_at",
        list_display_links = "id, title",
        list_filter        = "status, author_id",
        search_fields      = "title, body",
        search_help_text   = "Search posts by title or body",
        ordering           = "-published_at",
        list_per_page      = 10,
        date_hierarchy     = "published_at",
        fieldsets          = "Content: title, body, status | Publishing: author_id, published_at, view_count",
        actions            = "publish, archive",
    ),
    audit(track = "title, body, status"),
)]
pub struct Post { /* … */ }
```

`display = "title"` (en el modelo, fuera de `admin(...)`) establece la etiqueta legible
que se usa allí donde se referencia una fila — columnas FK en las listas de otros
modelos, la miga de pan, el título de la página de detalle.

### Cada opción de `admin(...)`

| Clave | Ejemplo | Qué hace |
|---|---|---|
| `list_display` | `"id, title, status"` | Columnas mostradas en la lista, en orden. Las columnas FK renderizan el valor `display` del objetivo. Las columnas calculadas (ver abajo) pueden nombrarse aquí. Vacío = cada campo escalar. |
| `list_display_links` | `"id, title"` | Qué celdas de `list_display` enlazan a la página de detalle. Debe ser un subconjunto de `list_display`. |
| `list_filter` | `"status, author_id"` | Tarjetas de faceta en el panel derecho — valores distintos + recuentos, clic para filtrar. Funciona en columnas escalares y FK. |
| `search_fields` | `"title, body"` | Campos que coincide el cuadro de búsqueda `?q=` (`ILIKE`/`LIKE` insensible a mayúsculas). |
| `search_help_text` | `"Search by title"` | Leyenda renderizada junto al cuadro de búsqueda. |
| `ordering` | `"-published_at, id"` | Orden por defecto. Prefijo `-` = DESC; sin él = ASC. Varias claves separadas por comas. |
| `list_per_page` | `10` | Tamaño de página (por defecto 50). |
| `date_hierarchy` | `"published_at"` | Franja de desglose año → mes → día sobre la lista, en una columna Date/DateTime. |
| `fieldsets` | `"Content: title, body \| Meta: status"` | Agrupa el formulario de cambio en secciones con título. La barra vertical `\|` separa secciones, la coma separa campos; la leyenda `Title:` es opcional. |
| `actions` | `"publish, archive"` | Acciones masivas ofrecidas en el selector de acciones de la lista (cada una necesita un manejador registrado — ver [Acciones masivas](#bulk-actions)). |
| `readonly_fields` | `"created_at"` | Campos renderizados como texto (sin entrada) en el formulario de cambio. |
| `raw_id_fields` | `"author_id"` | Campos FK editados mediante una entrada de id sin procesar + un enlace de búsqueda (bueno para tablas objetivo grandes). |
| `autocomplete_fields` | `"author_id"` | Campos FK editados mediante un autocompletado Ajax respaldado por el endpoint `__autocomplete` del objetivo. |
| `prepopulated_fields` | `"slug:title"` | Autocompleta un campo sluggificando otro mientras escribes (`target:source`; combina fuentes con `+`). |
| `list_select_related` | `"all"` / `"none"` / `"author_id"` | Controla el JOIN automático de columnas FK en la consulta de lista. `"all"` (por defecto) une cada FK; `"none"` lo desactiva; un CSV lo restringe a los FK nombrados. |
| `formfield_overrides` | `"status:textarea"` | Sobrescribe el widget de formulario de un campo (`field:widget`) — ver la [tabla de widgets](#form-widgets). |
| `actions_on_top` | `true` | Renderiza la barra de acciones masivas sobre la lista (por defecto `true`). |
| `actions_on_bottom` | `false` | Renderiza una segunda barra de acciones bajo la lista (por defecto `false`). |

---

## La vista de lista

`GET /<prefix>/<table>` renderiza la lista. A partir del único bloque `admin(...)` de
arriba obtienes columnas ordenables, un cuadro de búsqueda con texto de ayuda, las
tarjetas de faceta de estado/autor con recuentos en vivo, el desglose de fechas,
paginación a 10/página y el selector de acciones publish/archive.

**Filtrado.** Haz clic en cualquier valor de una tarjeta de faceta `list_filter` para
acotar la lista; el filtro activo se muestra como un chip con un enlace **clear**, y el
recuento de filas y los recuentos de facetas se actualizan. Los filtros, la búsqueda, la
ordenación y la jerarquía de fechas se componen todos en la cadena de consulta y pueden
combinarse.

[![La lista de entradas filtrada por status=published: un chip de filtro activo, la faceta coincidente resaltada, el cuadro de búsqueda y el selector de acciones masivas](../img/admin-list-filtered.png)](../img/admin-list-filtered.png)

**Ordenación.** Haz clic en un encabezado de columna para ordenar; clic de nuevo para
invertir la dirección (`?sort=col&order=asc|desc`). El valor por defecto proviene de
`ordering`.

**Paginación.** `list_per_page` establece el tamaño de página; navega con `?page=N`.
Para tablas muy grandes, regístralas con `Builder::skip_count_for([...])` para omitir el
`SELECT COUNT(*)` (el paginador muestra entonces "Page N" sin un total general); un
`?count=skip` por petición hace lo mismo de forma ad hoc.

**Búsqueda.** Cuando `search_fields` está configurado, aparece un cuadro de búsqueda y
coincide esos campos con `ILIKE` (PostgreSQL) / `LIKE` (MySQL, SQLite).
`search_help_text` se renderiza como su leyenda.

**Jerarquía de fechas.** Con `date_hierarchy` configurado, una miga de pan año → mes →
día se sitúa sobre la tabla; profundizar añade filtros de rango semiabierto en esa
columna usando extracción de fechas tri-dialecto (PostgreSQL `EXTRACT`, MySQL, SQLite
`strftime`).

---

## El formulario de cambio

`GET /<prefix>/<table>/new` (crear) y `GET /<prefix>/<table>/<pk>/edit` (editar)
renderizan el formulario. `fieldsets` agrupa las entradas en secciones con título; sin
él, todos los campos editables aparecen en un solo bloque.

[![El formulario de cambio de Post agrupado en los conjuntos de campos Content y Publishing, cada campo con el widget de entrada adecuado para su tipo](../img/admin-fieldsets.png)](../img/admin-fieldsets.png)

Enviar un formulario valida la entrada, escribe la fila, registra una entrada de
auditoría y redirige a la vista de **detalle** de solo lectura
(`GET /<prefix>/<table>/<pk>`), que muestra cada campo más los inlines y la tarjeta de
auditoría (abajo). Los botones **Edit** y **Delete** de la página de detalle llevan al
formulario y a la confirmación de eliminación.

### Widgets de formulario

Cada campo renderiza por defecto una entrada que coincide con su tipo —
`<input type="number">` para enteros, `type="date"`/`datetime-local` para fechas,
`type="checkbox"` para booleanos, un `<textarea>` para cadenas largas, un `<select>`
para columnas FK, y así sucesivamente. Sobrescribe por campo con
`formfield_overrides = "field:widget"`:

| Widget | Se aplica a | Renderiza |
|---|---|---|
| `textarea` | String | `<textarea>` multilínea |
| `password` | String | `<input type="password">` |
| `email` | String | `<input type="email">` |
| `url` | String | `<input type="url">` |
| `color` | String | `<input type="color">` |
| `slug` | String | entrada de texto con patrón de slug |
| `ipaddress` | String | entrada de texto con patrón de IP |
| `json` | Json | `<textarea>` monoespaciado |
| `hidden` | cualquiera | `<input type="hidden">` |

### Editar claves foráneas

Las columnas FK tienen tres modos de edición:

- **Por defecto** — un `<select>` poblado desde la tabla objetivo, que muestra el valor
  `display` de cada fila.
- **`raw_id_fields`** — una entrada de id simple más un enlace de búsqueda; ideal cuando
  la tabla objetivo es demasiado grande para enumerarla en un desplegable.
- **`autocomplete_fields`** — un autocompletado Ajax que consulta los `search_fields` del
  modelo objetivo mediante `GET /<prefix>/<target>/__autocomplete?q=…`.

### Campos precompletados y de solo lectura

`prepopulated_fields = "slug:title"` emite JS del lado del cliente que sluggifica el
campo fuente en el destino mientras escribes (combina varias fuentes con `+`, por
ejemplo `"slug:section+title"`). `readonly_fields` renderiza los campos nombrados como
texto escapado en el formulario en lugar de como entradas.

---

## Inlines

Los inlines muestran las filas de un modelo hijo en la página del padre (inlines de
Django). Registra uno a nivel de módulo:

```rust
rustango::register_admin_inline!(
    parent = "posts",
    child  = "comments",
    fk     = "post_id",                                     // child column → parent PK
    kind   = rustango::admin::inlines::InlineKind::Tabular, // or Stacked
    label  = "Comments",
    fields = &["author_name", "body", "created_at"],
);
```

En la página de **detalle** del padre los hijos se renderizan como una tabla de solo
lectura; en la página de **edición** se convierten en un FormSet editable (añadir /
cambiar / eliminar filas en el sitio). Opciones: `kind` (`Tabular` — una fila de tabla
por hijo, o `Stacked` — un conjunto de campos por hijo), `label`, `fields` (por defecto:
cada escalar excepto el FK), `extra` (filas en blanco ofrecidas para añadir), `max_num`
y `readonly_fields`.

[![La página de detalle de una entrada: campos de solo lectura, la tabla inline de Comments y la tarjeta de registro de auditoría que muestra la entrada de creación como un diff JSON](../img/admin-detail.png)](../img/admin-detail.png)

Para filas hijas adjuntadas mediante una clave foránea genérica (par content-type +
object-pk) en lugar de una única columna FK, usa
`register_admin_inline_generic!(parent, child, ct = "content_type_id", pk = "object_pk",
…)` — con las mismas opciones por lo demás.

---

## Acciones masivas

Nombra las acciones en `admin(actions = "...")`, luego registra un manejador por acción
en el `Builder`. El manejador recibe el pool y las claves primarias de las filas
seleccionadas:

```rust
use rustango::core::SqlValue;

let admin_router = admin::Builder::new(pool)
    .register_action("posts", "publish", |pool, pks| {
        Box::pin(async move {
            let ids: Vec<String> = pks.iter().filter_map(|v| match v {
                SqlValue::I64(n) => Some(n.to_string()),
                SqlValue::I32(n) => Some(n.to_string()),
                _ => None,
            }).collect();
            if !ids.is_empty() {
                let sql = format!("UPDATE posts SET status='published' WHERE id IN ({})", ids.join(","));
                rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
            }
            Ok(())
        })
    })
    .register_action("posts", "archive", /* … */)
    .build();
```

Selecciona filas con las casillas, elige la acción en el selector y envía
(`POST /<prefix>/<table>/__action`). `delete_selected` está incorporada — no la
registras. Un nombre de acción listado en `admin(actions = ...)` sin un manejador
registrado simplemente no aparecerá.

---

## Registro de auditoría

Añade `audit(track = "field1, field2")` a un modelo y cada creación, actualización y
eliminación se registra en la tabla `rustango_audit_log` (creada por ti cuando ejecutas
`migrate`). Solo se registran los modelos con un atributo `audit(...)`; `track`
selecciona qué campos se capturan en el diff (omítelo para rastrear todos los escalares).

```rust
#[rustango(table = "posts", audit(track = "title, body, status"))]
```

Cada entrada almacena la tabla, la clave primaria, la operación, la fuente, un diff por
campo (`{before, after}`) como JSON, el actor, una marca de tiempo y una huella
resistente a manipulaciones. Dos lugares lo exponen:

- La **página de detalle** del modelo gana una tarjeta **Audit trail** que lista los
  cambios recientes (quién, cuándo y el diff), con un enlace **View full history**
  (mostrado en la [captura de detalle](#inlines) de arriba).
- La vista **Activity** de la barra lateral (`GET /<prefix>/__audit`) es un feed que
  cruza filas, del más reciente al más antiguo, con tarjetas de faceta para entidad /
  operación / fuente y un formulario de limpieza para purgar entradas de más de N días
  (lo cual se registra a su vez como una entrada de auditoría).

[![El feed de Activity: cada cambio auditado a través de los modelos con diffs JSON, tarjetas de faceta por tabla/operación/fuente y un formulario de limpieza](../img/admin-audit.png)](../img/admin-audit.png)

---

## Columnas calculadas y filtros personalizados

Cuando el bloque declarativo no es suficiente, dos macros a nivel de módulo extienden la
vista de lista:

**Columnas calculadas** — una columna derivada, no de base de datos:

```rust
rustango::register_admin_computed!(
    "posts", "word_count", "Words",
    |row| row.get("body").and_then(|v| v.as_str())
             .unwrap_or_default().split_whitespace().count().to_string(),
);
// then add `word_count` to admin(list_display = "...").
```

El closure recibe la fila como `serde_json::Value` y devuelve HTML preescapado. Una forma
de 5 argumentos añade `link = |row| Option<String>` para envolver la celda en un `<a>`.

**Filtros de lista personalizados** — lógica de filtrado que las autofacetas no pueden
expresar:

```rust
fn by_status(value: &str) -> Vec<rustango::core::Filter> { /* map value → predicates */ }

rustango::register_admin_list_filter!(
    "posts", "status", "Status",
    &[("draft", "Drafts"), ("published", "Published")],   // (value, label) choices
    by_status,                                            // fn(&str) -> Vec<Filter>
);
```

---

## Vistas, querysets y permisos personalizados

Tres macros de registro más reflejan los hooks de `ModelAdmin` de Django:

- **Páginas de admin personalizadas** —
  `register_admin_view!("posts", "duplicate", Method::POST, "Duplicate", handler)`
  monta una página/acción adicional en `/<prefix>/posts/duplicate`. El manejador es un
  `fn(Pool, Request) -> Response` asíncrono. (Los sufijos reservados como `new`,
  `__action`, `__autocomplete`, `{pk}`, `{pk}/edit`, `{pk}/delete` se omiten con una
  advertencia.)
- **Acotación de querysets** —
  `register_admin_queryset!("posts", hook)` donde `hook: fn(&Parts) -> Vec<Filter>`
  restringe lo que una petición puede ver (por ejemplo, solo las filas del usuario
  actual). Varios hooks sobre una tabla se componen.
- **Permisos a nivel de fila** —
  `register_admin_object_permission!("posts", "change", check)` donde
  `check: fn(&Parts, Option<&Value>) -> bool` permite o deniega por fila. Los manejadores
  incorporados consultan las acciones `add`, `change`, `delete` y `view`; varios hooks se
  combinan con Y (AND).

Para un control de acceso más grueso basado en codename, `Builder::with_user_perms([...])`
condiciona cada tabla a `{table}.view` / `.add` / `.change` / `.delete`: la ausencia de
`view` oculta el modelo y devuelve 404 en accesos directos, la ausencia de `change` lo
renderiza de solo lectura, y la ausencia de `add` / `delete` elimina esos botones.

---

## Autenticación

Por defecto el admin está **abierto** — cualquiera que pueda alcanzarlo puede usarlo.
Restríngelo de una de dos maneras:

- **Auth de sesión (incorporada).** `Builder::with_session_auth(secret)` monta
  `/login` + `/logout` (y una página opcional de cambio en `/account/password`) y
  envuelve cada otra ruta en middleware que redirige las peticiones anónimas al
  formulario de inicio de sesión. Las credenciales viven en la tabla
  `rustango_admin_users` (`username`, `password_hash` argon2, `is_superuser`, `active`,
  `created_at`); cambiar una contraseña revoca las demás sesiones de ese usuario. La
  autenticación de dos factores TOTP opcional está disponible tras la característica
  `totp`, con inscripción en `/account/totp`.

  ```rust
  let admin = admin::Builder::new(pool)
      .with_session_auth(session_secret)
      .secure_cookies(true)              // HTTPS-only cookie in production
      .build();
  ```

- **Ponle tu propia auth por delante.** Deja el admin abierto y coloca HTTP Basic auth,
  OAuth2 o SSO corporativo delante de la ruta de anidamiento con tu propio middleware.

Cuando la auth de sesión está activa, el pie de la barra lateral muestra una línea
**"Signed in as _username_"** y un botón **Logout** (un formulario `POST`). Los admins
independientes hacen POST a `{admin_prefix}/logout` por defecto; un admin de inquilino se
sitúa tras la propia ruta de logout de la capa de tenancy, así que apunta el botón allí
con `Builder::logout_url`:

```rust
let admin = admin::Builder::new(pool)
    .with_session_auth(session_secret)
    .logout_url("/staff-logout")       // POST target for the sidebar Logout button
    .build();
```

El builder del admin de inquilino conecta esto a su `RouteConfig::logout_url`
automáticamente, de modo que el botón siempre alcanza una ruta que existe.

---

## Temas y marca

| Método | Efecto |
|---|---|
| `.theme_mode("light" \| "dark" \| "auto")` | Tema de color por defecto (establece `data-theme` en `<html>`). |
| `.title(s)` / `.subtitle(s)` | Texto de cabecera de la barra lateral. |
| `.brand_logo_url(url)` | Logo renderizado sobre el título. |
| `.brand_name(s)` / `.brand_tagline(s)` | Sobrescrituras por inquilino del título/subtítulo. |
| `.tenant_brand_css(css)` | Un bloque de variables CSS `:root{…}` prefabricado, incrustado inline para paletas por inquilino. |
| `.from_settings(pool, &settings)` | Construye la marca + visibilidad a partir de las secciones `[admin]` / `[brand]` de tu archivo de configuración. |

`from_settings` lee `admin.title`, `admin.subtitle`, `admin.logo_url`,
`admin.theme_mode`, `admin.url_prefix`, `admin.allowed_tables`,
`admin.read_only_tables`, recurriendo a la sección `[brand]`, y establece
`secure_cookies` por defecto en `true`. Las llamadas imperativas al `Builder` posteriores
siguen prevaleciendo.

---

## Referencia del `Builder`

Cada método de `admin::Builder` (cada uno devuelve `Self` para encadenar salvo que se
indique lo contrario):

| Método | Propósito |
|---|---|
| `new(pool)` | Construir a partir de cualquier pool (PostgreSQL / MySQL / SQLite). Valores por defecto: prefijo `/__admin`, cookies de desarrollo. |
| `from_settings(pool, &settings)` | Construir a partir de configuración parseada (característica `config`). |
| `title(s)` / `subtitle(s)` | Cabecera / subcabecera de la barra lateral. |
| `admin_prefix(p)` | Prefijo de URL — **debe coincidir con la ruta de anidamiento**. Por defecto `/__admin`. |
| `audit_url(u)` | Ruta de la vista de actividad/auditoría. Por defecto `/__audit`. |
| `static_url(u)` | Prefijo para los assets incrustados (favicon, logo). Por defecto `/__static__`. |
| `change_password_url(u)` | Ruta de la página de autoservicio de cambio de contraseña (añade el enlace en la barra lateral). |
| `show_only([tables])` | Lista blanca de qué tablas aparecen; las demás devuelven 404 y quedan ocultas. |
| `read_only([tables])` | Renderiza esas tablas pero prohíbe crear/editar/eliminar. |
| `read_only_all()` | Marca **cada** tabla como de solo lectura. |
| `skip_count_for([tables])` | Omite `COUNT(*)` en tablas enormes (el paginador muestra "Page N"). |
| `with_user_perms([codenames])` | Condiciona tablas a `{table}.view/add/change/delete`. |
| `register_action(table, name, handler)` | Registra un manejador de acción masiva. |
| `with_session_auth(secret)` | Requiere inicio de sesión por cookie (`/login` + `/logout`). |
| `logout_url(u)` | Destino POST del botón Logout de la barra lateral. Por defecto `{admin_prefix}/logout`; los admins de inquilino lo ajustan a su ruta de logout de tenancy. |
| `secure_cookies(bool)` | Establece el flag `Secure` (solo HTTPS) en la cookie de sesión. |
| `theme_mode(m)` | `"light"` / `"dark"` / `"auto"`. |
| `brand_logo_url(url)` | Logo sobre el título. |
| `brand_name(s)` / `brand_tagline(s)` | Sobrescrituras de marca por inquilino. |
| `tenant_brand_css(css)` | Bloque de variables CSS por inquilino. |
| `impersonated_by(operator_id)` | Renderiza un banner de suplantación (consola de operador). |
| `tenant_mode()` | Oculta los modelos con ámbito de registro (se establece automáticamente para los admins de inquilino). |
| `build()` | Finaliza y devuelve el `axum::Router`. |

---

## Referencia de rutas

Todas las rutas son relativas a `admin_prefix`:

| Ruta | Método | Qué |
|---|---|---|
| `/` | GET | Inicio — índice de modelos + acciones recientes. |
| `/<table>` | GET | Vista de lista (búsqueda, filtros, ordenación, paginación). |
| `/<table>` | POST | Envío de creación. |
| `/<table>/new` | GET | Formulario de creación. |
| `/<table>/<pk>` | GET | Vista de detalle (solo lectura), con inlines + tarjeta de auditoría. |
| `/<table>/<pk>` | POST | Envío de actualización. |
| `/<table>/<pk>/edit` | GET | Formulario de edición. |
| `/<table>/<pk>/delete` | POST | Eliminación (tras confirmación). |
| `/<table>/__action` | POST | Ejecuta una acción masiva sobre los PK seleccionados. |
| `/<table>/__autocomplete` | GET | JSON de autocompletado de FK (`?q=`). |
| `/__docs` | GET | Referencia de modelos. |
| `/__audit` (o `audit_url`) | GET | Feed de actividad + limpieza. |
| `/login`, `/logout` | GET/POST | Auth de sesión (cuando está activada). |
| `/account/password`, `/account/totp` | GET/POST | Autoservicio de cambio de contraseña / inscripción TOTP. |

Las rutas personalizadas registradas con `register_admin_view!` se montan en
`/<table>/<suffix>`.

---

## La referencia de modelos

Cada admin incluye una referencia de modelos en vivo (los admindocs de Django) en
`<prefix>/__docs` — un catálogo de solo lectura de cada modelo registrado con sus campos,
columnas, tipos, flags (PK, unique, …) y relaciones. Nada que configurar; se genera a
partir de tus modelos, así que nunca se desvía del esquema.

[![La referencia de modelos: los campos de cada modelo con nombre de columna, tipo Rust, flags y relaciones — generados a partir de los modelos](../img/admin-model-reference.png)](../img/admin-model-reference.png)

---

## Prueba el ejemplo

```bash
cd crates/rustango/examples/admin_demo
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/admin_demo
cargo run -- migrate     # tables + the audit-log table
cargo run                # seeds demo data, serves the admin at /admin
```

Luego abre <http://localhost:8080/admin> y entra en **Posts** para ver los filtros, la
búsqueda, la jerarquía de fechas, las acciones, los conjuntos de campos, los comentarios
inline, el registro de auditoría y la referencia de modelos — cada captura de esta
página — en un solo lugar.


---

## Véase también

- [El recetario del ORM](orm.md) — los modelos a partir de los cuales se genera el admin (incl. el registro de auditoría compartido).
- [Vistas HTML](html-views.md) — las vistas genéricas basadas en clases sobre las que se construye el admin.
- [Backends de autenticación](auth-backends.md) · [Sesiones](auth-sessions.md) — asegurar el admin tras un inicio de sesión.
- [Guía de seguridad](security.md) — endurecimiento antes de exponerlo.
