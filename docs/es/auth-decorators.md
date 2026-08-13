# Decoradores de acceso

Una vez que un usuario está autenticado, restringes rutas. **Rustango** entrega
la familia `@login_required` de Django como **capas** de axum componibles: adjunta
una a un router y las peticiones anónimas son rechazadas — redirigidas con 302 a
tu página de inicio de sesión (flujo de navegador) o respondidas con 401/403
(flujo de API) — antes de que lleguen siquiera al handler.

[![Decoradores de acceso: login_required redirige con 302 a los navegadores anónimos a /login?next=, la familia _or_403 devuelve 401/403 para APIs, superuser_required restringe por rol](img/auth-decorators.png)](img/auth-decorators.png)

> **Fuente:** `rustango::auth_decorators` (`login_required`, `login_required_or_401`,
> `user_passes_test`, `superuser_required`, `active_required`,
> `permission_required` + las variantes `_or_403`; `safe_next`, `extract_next`) —
> detrás de la característica `tenancy` (las barreras leen el extractor
> `SessionUser`).
>
> **Versión ejecutable:** el comportamiento de restricción está cubierto por el
> [`auth_demo`](../crates/rustango/examples/auth_demo/tests/auth_decorators.rs)
> probado — `cargo test -p auth_demo --test auth_decorators`.

> **¿Algún término aquí es nuevo para ti?** *middleware/capa*, *extractor*,
> *401/403* — consulta el [glosario](glossary.md).

> Compañero de inmersión profunda de la [guía de seguridad](security.md). Las
> barreras leen la sesión establecida al iniciar sesión — consulta
> [Sesiones](auth-sessions.md).

---

## Tabla de contenidos
- [Inicio rápido](#quick-start) · [Barreras de navegador vs. API](#browser-vs-api-gates)
- [La familia de barreras](#the-gate-family) · [Barreras por predicado y por rol](#predicate-and-role-gates)
- [Barreras de permiso](#permission-gates) · [El viaje de ida y vuelta de `?next=`](#the-next-round-trip)
- [Notas y límites](#notes-and-limits)

---

## Inicio rápido

```rust
use rustango::auth_decorators::login_required;

// Scope the gate to a sub-router (the idiomatic shape):
let private = Router::new()
    .route("/profile", get(profile))
    .route("/settings", get(settings))
    .layer(login_required("/login"));      // anonymous → 302 /login?next=...

let app = Router::new()
    .route("/", get(home))                 // public
    .merge(private);
```

Las peticiones anónimas a `/profile` se redirigen a `/login?next=%2Fprofile`; una
petición autenticada pasa hasta el handler.

---

## Barreras de navegador vs. API

La misma barrera viene en dos formas de respuesta. Elige según lo que el llamante
puede hacer con la respuesta:

- **Navegador / HTML** → las barreras base **redirigen con 302** a tu página de
  inicio de sesión (un humano puede seguirla e iniciar sesión).
- **API JSON** → la familia `_or_403` devuelve **códigos de estado**:
  `401 Unauthorized` para anónimo, `403 Forbidden` para autenticado-pero-no-permitido
  (un cliente no puede renderizar una página de inicio de sesión HTML, y la
  división 401/403 le permite distinguir «inicia sesión» de «no puedes hacer
  eso»).

```rust
// Browser: redirect to /login
let app = Router::new().route("/dashboard", get(dash)).layer(login_required("/login"));

// API: 401 for anonymous, never a redirect
let api = Router::new().route("/api/me", get(me)).layer(login_required_or_401());
```

---

## La familia de barreras

| Barrera (navegador, 302) | Variante de API (401/403) | Deja pasar |
|---|---|---|
| `login_required(url)` | `login_required_or_401()` | cualquier usuario con sesión iniciada |
| `active_required(url)` | `active_required_or_403()` | con sesión iniciada **y** `active` |
| `superuser_required(url)` | `superuser_required_or_403()` | `is_superuser && active` |
| `user_passes_test(url, pred)` | `user_passes_test_or_403(pred)` | predicado sobre la fila `User` |
| `permission_required(url, codename)` | `permission_required_or_403(codename)` | posee el codename de permiso |

Todas son capas tower — `.layer(...)`-las sobre un router o subrouter.

---

## Barreras por predicado y por rol

`user_passes_test` ejecuta tu closure contra la fila `User` resuelta, de modo que
puedes restringir por cualquier campo:

```rust
use rustango::auth_decorators::{user_passes_test, superuser_required_or_403};

// Staff-only sub-router (browser):
let staff = Router::new()
    .route("/admin/dashboard", get(dashboard))
    .layer(user_passes_test("/login", |u| u.is_superuser));

// Superuser-only JSON API → 401 anonymous / 403 non-superuser:
let api = Router::new()
    .route("/api/admin/stats", get(stats))
    .layer(superuser_required_or_403());
```

`superuser_required` / `active_required` son atajos fijados para los predicados
comunes `is_superuser && active` / `active`, de modo que los puntos de llamada no
divergen silenciosamente en si las cuentas desactivadas siguen contando.

---

## Barreras de permiso

`permission_required` comprueba un codename de permiso contra el motor de
permisos del inquilino (los superusuarios lo eluden automáticamente). Además
resuelve el extractor `Tenant`, así que las rutas que lo usan deben montarse bajo
el contexto del inquilino:

```rust
use rustango::auth_decorators::permission_required;
use rustango::tenancy::permissions::ACCESS_ADMIN_CODENAME;

let admin = Router::new()
    .route("/admin", get(dashboard))
    .layer(permission_required("/login", ACCESS_ADMIN_CODENAME));
```

---

## El viaje de ida y vuelta de `?next=`

`login_required` conserva la URL solicitada originalmente en `?next=` para que tu
handler de inicio de sesión pueda devolver al usuario tras autenticarse. **Debes
sanear ese valor** — reflejarlo en una redirección sin comprobarlo es un agujero
de redirección abierta (phishing) de manual. `safe_next` es la salvaguarda:

```rust
use rustango::auth_decorators::{extract_next, safe_next};

async fn login_post(Query(q): Query<HashMap<String, String>>, /* … */) -> Response {
    // … verify credentials, set the session …
    let dest = extract_next(&q)
        .and_then(|n| safe_next(&n))          // rejects open redirects
        .unwrap_or_else(|| "/".to_owned());
    Redirect::to(&dest).into_response()
}
```

`safe_next` solo acepta rutas del mismo origen, relativas a la raíz — rechaza las
URL absolutas, las `//host` relativas al esquema, las variantes con barra
invertida y sus formas codificadas en porcentaje:

```rust
assert_eq!(safe_next("/dashboard"),            Some("/dashboard".to_owned()));
assert_eq!(safe_next("https://evil.example/x"), None);
assert_eq!(safe_next("//evil.example/x"),       None);   // scheme-relative
assert_eq!(safe_next("%2F%2Fevil.example/x"),   None);   // decodes to //evil
```

---

## Notas y límites

- **Estas barreras leen la sesión.** «Con sesión iniciada» significa que el
  extractor [`SessionUser`](auth-sessions.md) resolvió un usuario a partir de la
  cookie de sesión — así que son para autenticación por sesión/cookie. La
  autenticación por token de API ([JWT](auth-jwt-api.md), [claves de
  API](auth-api-keys.md)) se restringe en su lugar en la capa de la [cadena de
  backends](auth-backends.md), leyendo la cabecera `Authorization`.
- **El orden de las capas importa.** `.layer(gate)` protege cada ruta añadida al
  router *antes* de ella; las rutas añadidas después son públicas. Acotar la
  barrera a un subrouter dedicado (la forma del inicio rápido) evita esa trampa.
- **`permission_required` necesita contexto de inquilino** (consulta el motor de
  permisos del inquilino) — móntalo bajo el inquilino; una ruta sin inquilino
  provoca un 500.
- El `?next=` de la redirección siempre está codificado en porcentaje, de modo
  que CRLF / división de respuestas no puede filtrarse en la cabecera
  `Location`.


---

## Véase también

- [Backends de autenticación](auth-backends.md)
- [Sesiones](auth-sessions.md)
- [Guía de seguridad](security.md)
