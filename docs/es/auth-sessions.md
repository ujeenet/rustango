# Sesiones

Una sesión mantiene a un usuario con la sesión iniciada a través de las
peticiones entregando al navegador un **ID opaco** en una cookie y guardando todo
lo demás en el servidor. El `SessionStore` de **Rustango** coloca ese estado en
una caché (Redis en producción, en memoria para las pruebas), de modo que la
cookie no lleva ningún secreto y una sesión puede **revocarse al instante** —
elimina la entrada y cada réplica ve el cierre de sesión en la siguiente
petición.

[![Sesiones en Rustango: la cookie solo contiene un id opaco, el SessionStore guarda los datos en Redis, y destroy() la revoca en todas partes](img/auth-sessions.png)](img/auth-sessions.png)

> **Fuente:** `rustango::sessions` (`Session`, `SessionStore`) +
> `rustango::cache` (`BoxedCache`, `InMemoryCache`) — detrás de la característica
> `sessions` (activada por defecto; arrastra `cache`). Para un almacén respaldado
> por Redis en producción, añade la característica `cache-redis` (desactivada por
> defecto) para obtener `RedisCache`.
>
> **Versión ejecutable:** los fragmentos a continuación están copiados del
> ejemplo probado
> [`auth_demo`](../crates/rustango/examples/auth_demo/tests/auth_sessions.rs) —
> `cargo test -p auth_demo --test auth_sessions`.

> **¿Algún término aquí es nuevo para ti?** *sesión*, *id opaco*, *cookie*,
> *caché* — consulta el [glosario](glossary.md).

> Compañero de inmersión profunda de la [guía de seguridad](security.md).
> Restringir rutas tras una sesión iniciada se trata en [Decoradores de
> autenticación](auth-decorators.md); para tokens de API sin estado en su lugar,
> consulta [JWT](auth-jwt.md).

---

## Tabla de contenidos
- [Inicio rápido](#quick-start) · [Sesiones vs. JWT](#sessions-vs-jwt)
- [La bolsa de sesión](#the-session-bag) · [La cookie](#the-cookie)
- [Elegir un backend](#picking-a-backend) · [Caducidad y renovación deslizante](#expiry-and-sliding-renewal)
- [Actualizar in situ](#updating-a-session-in-place) · [Notas y límites](#notes-and-limits)

---

## Inicio rápido

```rust
use rustango::sessions::{Session, SessionStore};
use rustango::cache::{BoxedCache, RedisCache};
use std::sync::Arc;

let store = SessionStore::new(Arc::new(RedisCache::new("redis://localhost/0")?) as BoxedCache);

// After the password check (see auth-passwords.md): stash who the user is,
// save → an opaque id, and set that id as the cookie.
let mut session = Session::new();
session.set("user_id", user.id);
let sid = store.save(&session).await?;
// Set-Cookie: rustango_session={sid}; HttpOnly; SameSite=Lax; Secure; Path=/

// On later requests: read the id from the cookie, load the session back.
let session = store.load(&sid).await?.unwrap_or_default();
let user_id: Option<i64> = session.get("user_id");

// Logout: drop the server-side entry — the cookie is now meaningless.
store.destroy(&sid).await?;
```

El id son 192 bits de aleatoriedad OS-CSPRNG, codificados en base64url a 32
caracteres — muy por encima del suelo de 128 bits para tokens de sesión, e
inadivinable.

---

## Sesiones vs. JWT

Ambos responden a «¿quién es esta petición?», con compensaciones opuestas:

| | Sesión | [JWT](auth-jwt.md) |
|---|---|---|
| Estado | del lado del servidor (búsqueda en caché por petición) | sin estado (token autocontenido) |
| Revocación | **instantánea** — `destroy()` la entrada | difícil — válido hasta la caducidad (necesita una lista de bloqueo) |
| Mejor para | aplicaciones de navegador, «cerrar la sesión de este usuario AHORA» | APIs, servicio a servicio, sin almacén compartido |

Recurre a las sesiones cuando necesites forzar el cierre de sesión de alguien
(cambio de contraseña, «cerrar sesión en todos los dispositivos», una cuenta
baneada). Recurre a JWT cuando quieras cero búsquedas por petición y no tengas
una caché compartida.

---

## La bolsa de sesión

`Session` es una bolsa tipada clave→valor con un bit de suciedad (*dirty bit*)
(para que el almacén pueda omitir una escritura cuando nada ha cambiado):

```rust
let mut s = Session::new();
s.set("user_id", 42_i64);            // serialize any Serialize value
s.set("role", "editor");
let uid: Option<i64> = s.get("user_id");   // None if absent or wrong type
s.remove("role");
s.clear();                            // wipe everything (e.g. on logout)
```

`get` es **tolerante a fallos**: una clave ausente *o* un valor que no se
deserializa al tipo solicitado devuelve `None` en lugar de entrar en pánico — de
modo que un cambio de esquema nunca provoca un 500 en una petición.

---

## La cookie

La cookie solo contiene `sid`. Configúrala con los atributos de seguridad que una
cookie de sesión necesita:

- **`HttpOnly`** — JavaScript no puede leerla (embota el robo de tokens por XSS).
- **`SameSite=Lax`** — no se envía en subpeticiones entre sitios (defensa CSRF;
  combínala con [tokens CSRF](security.md#protecting-against-csrf) para los envíos
  de formularios).
- **`Secure`** — solo HTTPS (omítelo únicamente para el desarrollo HTTP local).
- **`Path=/`** — visible para toda la aplicación.

Nada sensible hay en la cookie, así que una cookie filtrada es exactamente tan
poderosa como la sesión a la que apunta — y puedes revocar esa en el servidor en
cualquier momento.

---

## Elegir un backend

`SessionStore::new` acepta cualquier `BoxedCache`:

- **`RedisCache`** — producción. Compartida entre réplicas, de modo que un inicio
  de sesión en una instancia y un cierre de sesión en otra son ambos visibles en
  todas partes.
- **`InMemoryCache`** — proceso único / pruebas. Rápida, sin dependencias, pero
  las sesiones no sobreviven a un reinicio y no se comparten entre réplicas.

```rust
use rustango::cache::{BoxedCache, InMemoryCache};
use std::sync::Arc;

// Tests / single-process:
let store = SessionStore::new(Arc::new(InMemoryCache::new()) as BoxedCache);
```

---

## Caducidad y renovación deslizante

Las sesiones tienen por defecto un TTL de **2 semanas**. Sobrescríbelo por
almacén, y llama a `touch` en cada petición autenticada para una caducidad
deslizante (los usuarios activos siguen con la sesión iniciada, los inactivos
caducan):

```rust
use std::time::Duration;

let store = SessionStore::new(cache).ttl(Duration::from_secs(60 * 60)); // 1 hour

// On each request, after a successful load — extend without rewriting:
store.touch(&sid).await?;   // Ok(false) if the session is already gone
```

---

## Actualizar una sesión in situ

`save` siempre acuña un id nuevo (úsalo al iniciar sesión). Para modificar una
sesión existente durante una petición, carga → muta → `save_with_id` bajo el
mismo id:

```rust
let mut s = store.load(&sid).await?.unwrap_or_default();
s.set("last_seen", chrono::Utc::now().to_rfc3339());
store.save_with_id(&sid, &s).await?;
```

---

## Notas y límites

- **La revocación es la característica estrella** — `destroy()` (cierre de sesión)
  y la caducidad por TTL surten efecto ambas en la siguiente petición, en cada
  réplica que comparte la caché.
- **Los ids corruptos o desconocidos se cargan como `None`** (*fail-open*): un
  cambio de esquema de caché o una cookie manipulada produce una sesión vacía, no
  un error — la petición simplemente no está autenticada.
- **El almacén no establece la cookie por ti** — gestiona el estado del lado del
  servidor; tú adjuntas/lees la cookie `sid` en tu handler (o mediante una capa).
  Esto lo hace utilizable desde cualquier cableado de framework.
- **Acuña un id de sesión nuevo al cambiar de privilegio** (p. ej. justo después
  de iniciar sesión) para evitar la fijación de sesión — `save` ya lo hace,
  puesto que siempre genera un id nuevo.


---

## Véase también

- [Decoradores de autenticación](auth-decorators.md)
- [JWT](auth-jwt.md)
- [Backends de autenticación](auth-backends.md)
- [Guía de seguridad](security.md)
