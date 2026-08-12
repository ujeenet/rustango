# Nombres de URL y reverse

Codificar URLs a mano (`/posts/42`) por todos los handlers y templates es frágil —
cambia una ruta y cada literal se rompe en silencio. **Rustango** te da la
respuesta de Django: **nombra un patrón de URL una vez, luego construye la URL por
su nombre en todas partes** — en Rust con `reverse(...)`, en templates con
`{{ url(...) }}`, y en redirecciones con `redirect_to_view(...)`. La superficie de
la API refleja los `reverse()` / `{% url %}` / `resolve_url()` / `redirect()` de
Django.

[![URLs reverse al estilo Django: register_url! nombra un patrón, reverse() construye la URL en Rust, y {{ url(...) }} construye la URL en un template](img/urls.png)](img/urls.png)

> **Fuente:** `rustango::urls` (`register_url!`, `reverse`, `reverse_owned`,
> `all_routes`, `duplicates`, `register_url_tag`) y `rustango::shortcuts`
> (`resolve_url`, `redirect_to_view`).

> **¿Nuevo con algún término aquí?** *ruta*, *reverse*, *namespacing* — consulta
> el [glosario](glossary.md).

---

## Tabla de contenidos
- [Registrar una URL con nombre](#register-a-named-url)
- [Reverse en Rust](#reverse-in-rust) · [Reverse en templates](#reverse-in-templates)
- [Redirigir por nombre](#redirect-by-name) · [Namespacing](#namespacing)
- [Inspeccionar el mapa de URLs](#inspect-the-url-map) · [Errores](#errors)
- [Patrones regex y de ruta tipados](#regex--typed-path-patterns) · [Notas y límites](#notes-and-limits)

---

## Registrar una URL con nombre

`register_url!("name", "/pattern")` registra un mapeo nombre → patrón. Se ejecuta
al cargar el módulo (vía `inventory`), así que la ruta aterriza en un registro
global en el momento en que su módulo se enlaza — sin `urls.py` central que editar,
y sin `include()` que cablear.

```rust
use rustango::register_url;

register_url!("post-detail", "/posts/{id}");
register_url!("user-posts",  "/users/{user_id}/posts/{post_id}");
register_url!("home",        "/");
```

Los placeholders usan la sintaxis de ruta `{name}` de axum. El patrón es la misma
cadena en la que montas el handler — mantenlos sincronizados (registra el nombre
junto a donde construyes la ruta).

---

## Reverse en Rust

`reverse(name, &params)` sustituye los `{placeholders}` del patrón por los valores
dados (percent-encodando cada uno) y devuelve la URL:

```rust
use std::collections::HashMap;
use rustango::urls::reverse;

let mut params = HashMap::new();
params.insert("id", "42".to_string());

let url = reverse("post-detail", &params)?;   // → "/posts/42"
```

Para claves dinámicas (p. ej. valores ensamblados a partir de una petición),
`reverse_owned` toma `HashMap<String, String>` en lugar de
`HashMap<&str, String>`:

```rust
use rustango::urls::reverse_owned;
let url = reverse_owned("post-detail", &owned_params)?;
```

`reverse` es **estricto**: un placeholder faltante, o una clave `params` extra que
el patrón no tiene, es un error (no un desajuste silencioso) — consulta
[Errores](#errors).

---

## Reverse en templates

Los templates obtienen el `{% url %}` de Django como función Tera. Regístrala una
vez en tu instancia `Tera` durante el setup (está detrás del feature
`template_views`):

```rust
rustango::urls::register_url_tag(&mut tera);
```

Luego llama a `url(name=..., <param>=...)` en cualquier template — `name` es
obligatorio, y cada otro argumento con nombre es un parámetro de ruta (se aceptan
cadenas, números y booleanos):

```jinja
<a href="{{ url(name='post-detail', id=42) }}">View post</a>
<a href="{{ url(name='user-posts', user_id=7, post_id=42) }}">…</a>
```

Ese es el equivalente del `{% url 'post-detail' id=42 %}` de Django. Para el patrón
de captura `{% url 'x' as var %}`, usa el `{% set %}` de Tera:

```jinja
{% set post_url = url(name='post-detail', id=post.id) %}
<a href="{{ post_url }}">{{ post.title }}</a>
```

Un argumento `null` (normalmente una variable de template no definida) falla
ruidosamente en lugar de producir en silencio una URL rota.

---

## Redirigir por nombre

`rustango::shortcuts` refleja los helpers de redirección por nombre de vista de
Django, de modo que los handlers nunca codifican un `Location` a mano:

```rust
use std::collections::HashMap;
use rustango::shortcuts::{redirect_to_view, resolve_url};

// redirect('post-detail', id=42) → 302 Location: /posts/42
let mut params = HashMap::new();
params.insert("id", "42".to_string());
let response = redirect_to_view("post-detail", &params)?;
```

`resolve_url(spec, &params)` es el `resolve_url` de Django: si `spec` ya parece una
URL (`/…`, `http://`, `https://`, `./`, `../`) se devuelve sin cambios; en caso
contrario se trata como un nombre de ruta y se resuelve por reverse. Útil para un
parámetro `?next=` o un ajuste que pueda contener *o bien* una ruta *o bien* un
nombre:

```rust
let url = resolve_url("post-detail", &params)?;  // name  → "/posts/42"
let url = resolve_url("/dashboard", &params)?;   // path  → "/dashboard" (as-is)
```

(Para redirecciones crudas a una URL conocida, `rustango::shortcuts::redirect(url)`
devuelve un simple `302`.)

---

## Namespacing

No hay `include()` ni un namespace de app auto-aplicado — cada `register_url!`
aterriza en un único registro global. El namespacing es una **convención en el
nombre mismo**: prefija con `app:`, exactamente como llamarías al
`reverse("app:detail")` de Django.

```rust
register_url!("blog:post-detail", "/blog/posts/{id}");
register_url!("shop:product",     "/shop/products/{slug}");
```

```rust
reverse("blog:post-detail", &params)?;   // "/blog/posts/42"
```

Los dos puntos son simplemente parte de la cadena registrada — elige un prefijo
consistente por app para evitar colisiones.

---

## Inspeccionar el mapa de URLs

Lista cada ruta registrada desde la CLI — útil para una auditoría rápida o para
scriptear:

```bash
cargo run -- showurls                  # plain table of name → pattern
cargo run -- showurls --format json    # machine-readable
```

En código, `all_routes()` devuelve todo el registro, y `duplicates()` devuelve
cualquier nombre registrado más de una vez (gana el primero en caso contrario —
conviene aseverarlo al arrancar):

```rust
use rustango::urls::{all_routes, duplicates};

for route in all_routes() {
    println!("{} → {}", route.name, route.pattern);
}

let dups = duplicates();
assert!(dups.is_empty(), "duplicate URL names: {dups:?}");
```

---

## Errores

`reverse` / `reverse_owned` / `resolve_url` / `redirect_to_view` devuelven
`Result<_, rustango::urls::ReverseError>`:

| Variante | Cuándo |
|---|---|
| `UnknownName(name)` | Ningún `register_url!` se ejecutó para ese nombre (errata, o su módulo no se enlazó). |
| `MissingParam { name, param }` | El patrón tiene `{param}` pero `params` no lo proporcionó. |
| `UnexpectedParam { name, param }` | `params` llevaba una clave que el patrón no tiene (atrapa erratas). |
| `MalformedPattern { name, detail }` | El patrón registrado está malformado (p. ej. una `{` sin cerrar). |

En los templates estos afloran como errores de render de Tera (un 500 vía
`shortcuts::render` / `template_views`), así que un `{{ url(...) }}` erróneo falla
de forma visible en lugar de renderizar un enlace roto.

---

## Patrones regex y de ruta tipados

**Rustango no tiene `re_path`, y nunca se aplica ningún convertidor de ruta.** Un
segmento de patrón es o bien un literal (`/posts/new`) o un placeholder `{name}`
que captura exactamente un segmento; `{*name}` captura el resto de la ruta. Ese es
todo el vocabulario — no hay `r'(?P<year>[0-9]{4})'`, y `{int:id}` **no** restringe
`id` a un entero.

### Por qué — el matcher no es un motor de regex

El enrutamiento *es* [axum](https://docs.rs/axum) 0.8, y axum empareja rutas con
[`matchit`](https://docs.rs/matchit), un router de **radix-trie (árbol radix)**.
Recorre la URL un segmento a la vez por un árbol de prefijos, de modo que un match
cuesta O(longitud de la ruta) y es independiente de cuántas rutas hayas registrado.
Un router de regex hace lo contrario: Django evalúa `urlpatterns` de arriba abajo,
ejecutando la regex de cada entrada contra la ruta hasta que una coincide. El trie
compra emparejamiento en tiempo constante y una precedencia inequívoca de «gana el
literal más específico» — a costa de no expresar restricciones de clase de
caracteres *en la ruta misma*.

Rustango hereda ese matcher por completo. **No hay un segundo resolvedor basado en
regex** superpuesto, y `register_url!` registra deliberadamente las *mismas*
cadenas `{name}` que el router ya entiende — nunca compila una regex. Así que las
rutas regex no están «apagadas»; la capa de enrutamiento simplemente nunca fue un
motor de regex de entrada.

La forma `{int:id}` se acepta solo como **facilidad de portado** para `reverse()`:
el constructor divide el placeholder por `:` y conserva solo el nombre, descartando
el prefijo de tipo ([`urls.rs`](../crates/rustango/src/urls.rs)). Eso permite que
`reverse()` funcione sobre un patrón copiado literalmente de un
`path("<int:id>/", …)` de Django — pero nada valida que el valor suministrado sea
realmente un entero.

### Cómo expresar una ruta restringida

Empareja el segmento con un simple `{placeholder}`, luego impón su forma donde se
usa el valor. El `re_path(r'^articles/(?P<year>[0-9]{4})/$', …)` de Django se
convierte en:

```rust
register_url!("article-by-year", "/articles/{year}");
// router:
.route("/articles/{year}", get(article_by_year))

async fn article_by_year(Path(year): Path<String>) -> impl IntoResponse {
    // the router accepted any single segment; enforce [0-9]{4} here
    match year.parse::<u16>() {
        Ok(y) if (1000..=9999).contains(&y) => render_year(y).await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
```

Para rechazar *antes* de que el handler se ejecute (más cerca de la semántica de
convertidores de Django), pon la comprobación en un extractor axum personalizado
(`FromRequestParts`) y toma ese tipo como argumento del handler en lugar de
`Path<String>` — el framework no incluye ninguno, pero el trait de extractor de
axum es la costura prevista. El crate `regex` ya es una dependencia (el ORM lo usa
para los lookups `__regex`), así que un extractor validador puede compilar una
`Regex` una vez y reutilizarla entre peticiones.

---

## Notas y límites

- **El registro es en tiempo de enlace.** Un `register_url!` solo surte efecto si su
  módulo se compila en el binario. Un error `UnknownName` normalmente significa que
  el nombre es una errata *o* que su módulo no se referencia en ningún sitio (así
  que el linker lo descartó).
- **Los patrones no se validan contra tus rutas reales.** `register_url!` registra
  un mapeo nombre → cadena; no comprueba que haya realmente un handler montado en
  ese patrón. Registra el nombre junto a donde montas la ruta para que se mantengan
  sincronizados.
- **Los valores se percent-encodan** con `reverse`, así que son seguros para
  colocar en un header `Location` o un `href`.
- **Sin convertidores regex/tipados** en los patrones (el `<int:pk>` de Django);
  los placeholders son simples `{name}` y los valores se sustituyen tal cual
  (después de codificar). Consulta [Patrones regex y de ruta tipados](#regex--typed-path-patterns)
  para el porqué, y cómo restringir una ruta en su lugar.


---

## Véase también

- [Vistas HTML](html-views.md)
- [ViewSets](viewsets.md)
- [Middleware](middleware.md)
