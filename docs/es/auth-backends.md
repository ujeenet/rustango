# Backends de autenticación

Un **backend de autenticación** responde a una única pregunta: *dada una petición
entrante, ¿quién es el usuario?* **Rustango** te permite apilar varios — HTTP
Basic, clave de API, JWT — en una cadena que el middleware de autenticación
prueba en orden, de modo que una misma aplicación puede aceptar humanos y
máquinas en las mismas rutas. Es la idea de `AUTHENTICATION_BACKENDS` de Django,
cableada a axum. Combínalo con `require_auth` / `require_perm` para restringir
rutas y el extractor `CurrentUser` para leer el resultado.

[![Backends de autenticación en Rustango: una petición fluye por una cadena de backends (ModelBackend, ApiKeyBackend, JwtBackend); el primero que reconoce la credencial gana e inyecta CurrentUser, luego require_perm comprueba un codename](img/auth-backends.png)](img/auth-backends.png)

> **¿Algún término aquí es nuevo para ti?** *Backend*, *middleware*, *extractor*,
> *codename de permiso* — consulta el [glosario](glossary.md).

> **Fuente:** `rustango::tenancy::auth_backends` (`AuthBackend`, `ModelBackend`,
> `ApiKeyBackend`, `JwtBackend`, `AuthUser`, `AuthError`) y
> `rustango::tenancy::{RouterAuthExt, CurrentUser}` — detrás de la característica
> `tenancy`. Un registro portable e independiente de la base de datos también
> vive en `rustango::auth_backends` (siempre compilado).
>
> **Versión ejecutable:** cada fragmento está copiado de
> [`auth_backends_doc.rs`](../crates/rustango/tests/auth_backends_doc.rs)
> (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Tabla de contenidos

- [La cadena](#the-chain) · [Los backends integrados](#the-built-in-backends)
- [Restringir rutas: require_auth](#gating-routes-require_auth)
- [Leer el usuario: CurrentUser](#reading-the-user-currentuser)
- [Permisos: require_perm](#permissions-require_perm)
- [El registro portable](#the-portable-registry)
- [Véase también](#see-also)

---

## La cadena

Pasas a `require_auth` un `Vec<Arc<dyn AuthBackend>>`. En cada petición, el
middleware los prueba **en orden**:

- el **primer** backend que reconoce la credencial gana (devuelve el usuario);
- un backend que no la reconoce devuelve «ninguno» y se prueba el siguiente;
- si un backend falla de forma dura (p. ej. una cuenta inactiva con un token
  *válido*), la cadena se detiene con ese error;
- si ninguno coincide, la petición recibe `401` (con `require_auth`) o continúa
  de forma anónima (con `optional_auth`).

```rust
use std::sync::Arc;
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),    // HTTP Basic  → humans
    Arc::new(ApiKeyBackend),   // Bearer key  → machines
];
```

---

## Los backends integrados

| Backend | Credencial que lee | Identifica a un usuario por |
|---|---|---|
| `ModelBackend` | `Authorization: Basic <base64(user:pass)>` | nombre de usuario + verificación de contraseña argon2id contra `rustango_users` |
| `ApiKeyBackend` | `Authorization: Bearer <prefix.secret>` | la tabla `rustango_api_keys` (véase [Claves de API](auth-api-keys.md)) |
| `JwtBackend` | `Authorization: Bearer <jwt>` | un token HS256 firmado (véase [JWT](auth-jwt.md)) |

`ApiKeyBackend` y `JwtBackend` leen ambos `Bearer` y desambiguan por la forma (el
primer segmento separado por punto de una clave de API tiene exactamente 8
caracteres). Construye `JwtBackend` con un secreto de **al menos 32 bytes**
(`JwtBackend::new(secret)` entra en pánico en caso contrario):

```rust
use rustango::tenancy::auth_backends::JwtBackend;

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),
    Arc::new(JwtBackend::new(jwt_secret_at_least_32_bytes.to_vec())),
];
```

Escribe un backend personalizado implementando el trait (un único método async
que inspecciona los `Parts` de la petición y devuelve `Option<AuthUser>`):

```rust
use async_trait::async_trait;   // add `async-trait` to your Cargo.toml
use axum::http::request::Parts;
use rustango::sql::Pool;
use rustango::tenancy::auth_backends::{AuthBackend, AuthError, AuthUser};

struct HeaderBackend;

#[async_trait]
impl AuthBackend for HeaderBackend {
    async fn authenticate(&self, parts: &Parts, _pool: &Pool)
        -> Result<Option<AuthUser>, AuthError>
    {
        // ...inspect parts.headers, return Some(AuthUser{..}) or Ok(None)
        Ok(None)
    }
}
```

---

## Restringir rutas: require_auth

`RouterAuthExt` añade el middleware. `require_auth` rechaza las peticiones
anónimas con `401`; `optional_auth` las deja pasar (de modo que un handler puede
ramificar según con sesión iniciada o no):

```rust
use rustango::tenancy::RouterAuthExt;

let app = Router::new()
    .route("/profile", get(profile))
    .require_auth(backends, pool);     // 401 if no backend matches
```

Comportamiento verificado:

```rust
// no credentials               → 401
// Basic alice:<correct>        → 200
// Basic alice:<wrong>          → 401   (no backend accepted; no enumeration)
// Bearer <valid api key>       → 200
```

---

## Leer el usuario: CurrentUser

Los handlers leen el usuario autenticado con el extractor `CurrentUser`. Es
infalible — `Some(user)` cuando un backend resolvió uno, `None` en caso
contrario:

```rust
use rustango::tenancy::CurrentUser;

async fn profile(CurrentUser(user): CurrentUser) -> Response {
    match user {
        Some(u) => format!("hello {}", u.username).into_response(),
        None    => StatusCode::UNAUTHORIZED.into_response(),
    }
}
```

> **Trampa:** como `CurrentUser` es infalible, olvidar `require_auth` no provoca
> un fallo de compilación — cada petición simplemente ve `None`. Detrás de
> `require_auth`, las peticiones anónimas ya reciben `401`, así que `user`
> siempre es `Some` ahí.

---

## Permisos: require_perm

`require_perm` restringe una ruta a un **codename** de permiso
(`{table}.{action}`, p. ej. `post.add`). Aplícalo al subrouter interno y
`require_auth` al externo, de modo que el usuario se resuelva *antes* de
comprobar el permiso:

```rust
let admin = Router::new()
    .route("/admin", get(admin_only))
    .require_perm("post.add", pool.clone());   // inner: needs the codename

let app = Router::new()
    .route("/profile", get(profile))
    .merge(admin)
    .require_auth(backends, pool);             // outer: resolves the user first
```

```rust
// alice (granted post.add)   → /admin 200
// bob   (authed, no grant)   → /admin 403
// anonymous                  → /admin 401   (auth runs first)
```

Resolución: un **superusuario** (activo) pasa todo; un usuario **desactivado** es
denegado incluso con concesiones; una anulación explícita por usuario prevalece
sobre las concesiones de rol; en caso contrario, cualquier rol que el usuario
posea que conceda el codename pasa. Concede con
`set_user_perm_pool` / los roles mediante `create_role_pool` + `assign_role_pool`
(las tablas de permisos las crea `ensure_tables_pool`).

---

## El registro portable

Por separado, `rustango::auth_backends` (nota: raíz del crate, **no** `tenancy`)
es un pequeño registro **independiente del framework** — una cadena
`Credentials` → `Principal` con su propio trait `AuthBackend`. No tiene ningún
pegamento HTTP/axum; úsalo cuando quieras una capacidad de conexión de backends
al estilo Django dentro de tu propio código de autenticación:

```rust
use rustango::auth_backends::{AuthBackendChain, Credentials, RemoteUserBackend};

let chain = AuthBackendChain::new().with(Arc::new(RemoteUserBackend::trust_username()));
let principal = chain.authenticate(&Credentials::remote("alice")).await?;
```

La misma semántica «el primer éxito gana / el primer error detiene» que la cadena
HTTP. Para restringir rutas reales, usa el middleware de `tenancy` de arriba.

---

## Véase también

- [Claves de API](auth-api-keys.md) y [JWT](auth-jwt.md) — las credenciales que
  `ApiKeyBackend` / `JwtBackend` consumen.
- [Contraseñas](auth-passwords.md) — el hashing contra el que `ModelBackend`
  verifica.
- [Decoradores de acceso](auth-decorators.md) — restricción por handler con
  `login_required` / `permission_required`, la alternativa de estilo decorador a
  `require_auth`/`require_perm`.
- [Sesiones](auth-sessions.md) — autenticación basada en cookies para
  navegadores.
