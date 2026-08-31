# API de autenticación JWT

El módulo [JWT independiente](auth-jwt.md) firma y verifica un solo token. Una
API real necesita todo el **ciclo de vida**: un token de *acceso* de corta
duración, un token de *refresco* de larga duración, rotación en el refresco, y
**revocación** para el cierre de sesión. **Rustango** lo entrega como
`JwtLifecycle` — y un router listo para usar que monta por ti
`POST /api/auth/login`, `/refresh`, `/logout`, y `GET /me`.

[![API de autenticación JWT: login emite un par access+refresh, refresh rota y pone en lista negra el token antiguo, logout revoca a través de un almacén de JTI](../img/auth-jwt-api.png)](../img/auth-jwt-api.png)

> **Fuente:** `rustango::tenancy::jwt_lifecycle` (`JwtLifecycle`, `JwtTokenPair`,
> `JwtClaims`) y `rustango::tenancy::auth_routes` (`jwt_router`, `Config`) +
> `rustango::jti_store` (`JtiStore`, `InMemoryJtiStore`) — tras `jwt` +
> `tenancy`.
>
> **Versión ejecutable:** el motor de tokens está cubierto por el test
> [`auth_demo`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/auth_demo/tests/auth_jwt_api.rs) —
> `cargo test -p auth_demo --test auth_jwt_api`. Los endpoints HTTP están
> delimitados por tenant y se ejercitan de extremo a extremo por el propio
> `crates/rustango/tests/tenant_auth_live.rs` del framework.

> **¿Algún término te resulta nuevo?** *token de acceso/de refresco*,
> *rotación*, *revocación* — consulta el [glosario](glossary.md).

> Complemento en profundidad de la sección «Emitir y renovar JWT» de la
> [Guía de seguridad](security.md). Para un solo token gestionado manualmente,
> consulta en su lugar [JWT (independiente)](auth-jwt.md).

---

## Tabla de contenidos
- [El router integrado](#the-built-in-router) · [El cableado](#wiring-it-up)
- [El motor de tokens](#the-token-engine-jwtlifecycle) · [Refresco y rotación](#refresh-and-rotation)
- [Revocación y el almacén de JTI](#revocation-and-the-jti-store) · [Claims personalizados](#custom-claims)
- [Notas y límites](#notes-and-limits)

---

## El router integrado

`jwt_router` monta los cuatro endpoints estándar contra la tabla
`rustango_users` propia de cada tenant — las ~50 líneas de boilerplate de login
que todo proyecto reescribe de otro modo:

| Método | Ruta | Cuerpo / Auth | Devuelve |
|---|---|---|---|
| POST | `/api/auth/login` | `{username, password}` | `{access, refresh, user}` |
| POST | `/api/auth/refresh` | `{refresh}` | `{access, refresh}` |
| POST | `/api/auth/logout` | `Authorization: Bearer <access>` | `204` (revoca el JTI) |
| GET | `/api/auth/me` | `Authorization: Bearer <access>` | `{user_id, username, is_superuser}` |

Login verifica la contraseña con [argon2id](auth-passwords.md), luego emite un
par. Las rutas, los TTL y la clave de firma son configurables mediante `Config`.

## El cableado

```rust
use rustango::tenancy::auth_routes::{jwt_router, Config};

rustango::manage::Cli::new()
    .tenancy()
    .api(my_app::urls::api()
        .merge(jwt_router(Config::default())))   // monta /api/auth/*
    .run()
    .await
```

`Config::default()` firma con `RUSTANGO_SESSION_SECRET` (la misma clave que la
cookie de sesión del admin) y usa TTL de 15 min de acceso / 7 días de refresco.
Sobrescribe `prefix`, `access_ttl_secs`, `refresh_ttl_secs`, o `session_secret`
según necesites. Los endpoints se ejecutan bajo el contexto del tenant, así que
móntalos en una aplicación tenancy.

```sh
# Login → access + refresh
curl -sX POST localhost:8080/api/auth/login \
  -H 'content-type: application/json' \
  -d '{"username":"alice","password":"hunter2hunter"}'

# Llamar a un endpoint protegido
curl localhost:8080/api/auth/me -H "Authorization: Bearer $ACCESS"
```

---

## El motor de tokens (`JwtLifecycle`)

Bajo el router se encuentra `JwtLifecycle` — utilizable directamente si quieres
el ciclo de vida sin la forma HTTP integrada:

```rust
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

let jwt = JwtLifecycle::new(secret_32_bytes);

// Login: emitir el par.
let pair = jwt.issue_pair(user_id);
// → pair.access  (TTL corto, enviar en la cabecera Authorization)
// → pair.refresh (TTL largo, almacenar en una cookie HttpOnly / almacenamiento seguro)

// Petición autenticada: verificar el token de acceso.
match jwt.verify_access(&access).await {
    Some(claims) => { /* claims.sub es el id de usuario */ }
    None => { /* 401: inválido, caducado, revocado, o tipo incorrecto */ }
}
```

Los tokens de acceso y de refresco **no son intercambiables** — `verify_access`
rechaza un token de refresco y viceversa, de modo que un token de acceso de corta
duración robado no puede usarse para acuñar nuevos:

```rust
let pair = jwt.issue_pair(42);
assert!(jwt.verify_refresh(&pair.access).await.is_none());
assert!(jwt.verify_access(&pair.refresh).await.is_none());
```

---

## Refresco y rotación

`refresh` intercambia un token de refresco válido por un **nuevo par** y pone en
lista negra el JTI del token de refresco antiguo — expiración deslizante con
tokens de refresco de un solo uso (la reproducción del antiguo se rechaza):

```rust
let pair = jwt.issue_pair(7);
let rotated = jwt.refresh(&pair.refresh).await.expect("refresh ok");
assert_ne!(pair.access, rotated.access);
assert!(jwt.refresh(&pair.refresh).await.is_none());   // el refresh antiguo ya está muerto
```

Por defecto, `refresh` **preserva** los claims personalizados del token. Si los
permisos pueden haber cambiado (rol revocado, alcance degradado), usa
`refresh_with(token, new_claims)` para sustituir un payload nuevo mientras se
sigue poniendo en lista negra el JTI de refresco antiguo.

---

## Revocación y el almacén de JTI

Cada token lleva un `jti` único. `revoke` lo añade a una lista negra para que las
llamadas `verify_*` posteriores fallen hasta que el token hubiera expirado de
todos modos — esto es lo que llama `POST /api/auth/logout`:

```rust
let pair = jwt.issue_pair(1);
assert!(jwt.revoke(&pair.access).await);
assert!(jwt.verify_access(&pair.access).await.is_none());
```

La lista negra reside en un `JtiStore` intercambiable. El `InMemoryJtiStore` por
defecto es **de un solo proceso y pierde las revocaciones al reiniciar** — bien
para una sola instancia. Cualquier despliegue con múltiples réplicas DEBE
instalar un almacén compartido y duradero (Redis / BD) para que un cierre de
sesión en una réplica sea respetado por todas:

```rust
use rustango::jti_store::{InMemoryJtiStore, JtiStore};
use std::sync::Arc;

let shared: Arc<dyn JtiStore> = Arc::new(InMemoryJtiStore::new()); // sustituir por Redis en prod
let a = JwtLifecycle::new(secret.clone()).with_jti_store(Arc::clone(&shared));
let b = JwtLifecycle::new(secret).with_jti_store(Arc::clone(&shared));

let pair = a.issue_pair(5);
a.revoke(&pair.access).await;
assert!(b.verify_access(&pair.access).await.is_none());   // B ve la revocación de A
```

> Sin un almacén compartido, `/logout` es, en el mejor de los casos, «best-effort»:
> un token revocado puede seguir siendo aceptado en otra réplica hasta su
> expiración natural. Este es el ajuste de producción más importante para la
> autenticación JWT.

### Escribir un almacén duradero

`JtiStore` es asíncrono (desde v0.52, #1191), así que una implementación duradera
es una sola consulta — sin volcado en segundo plano ni ventana de convergencia
durante la cual un `jti` revocado siga aceptándose en otra réplica. Ambos métodos
devuelven `JtiFuture<'_, T>` (un future en caja — el trait se usa como
`Arc<dyn JtiStore>`, y un `async fn` nativo en un trait no es compatible con
objetos dyn):

```rust
use rustango::jti_store::{JtiFuture, JtiStore};

impl JtiStore for PgJtiStore {
    fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool> {
        Box::pin(async move { self.lookup(jti).await })
    }

    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
        Box::pin(async move { self.insert_if_absent(jti, exp_unix).await })
    }
}
```

`mark_used` DEBE ser atómico: entre llamadas concurrentes para el mismo `jti`,
exactamente una debe obtener `true` — una única escritura condicional
(`INSERT … ON CONFLICT DO NOTHING`, Redis `SET NX`), nunca una lectura seguida de
una escritura. De lo contrario se pierde la garantía de un solo uso de los tokens
de refresco.

Como la verificación consulta el almacén, `verify_access`, `verify_refresh`,
`refresh`, `revoke` y los helpers de token de MCP / tenant
(`mcp::verify_agent_token`, `tenancy::auth_routes::verify_for_tenant`) son todos
`async`. La **expiración se comprueba antes** de consultar el almacén, así que un
token caducado no cuesta ningún viaje de ida y vuelta.

---

## Claims personalizados

Incrusta `roles` / `tenant` / `scope` directamente en el token para que la
verificación no necesite ninguna consulta a la BD. Los nombres reservados
(`sub`, `exp`, `jti`, `typ`) se rechazan:

```rust
let custom = serde_json::json!({ "roles": ["admin"], "tenant": "acme" })
    .as_object().unwrap().clone();
let pair = jwt.issue_pair_with(99, custom)?;

let claims = jwt.verify_access(&pair.access).await.unwrap();
let roles: Vec<String> = claims.get_custom("roles").unwrap();   // ["admin"]
```

Los claims personalizados sobreviven a `refresh` (se trasladan al nuevo par) a
menos que uses `refresh_with`.

---

## Notas y límites

- **Sesiones vs JWT vs esto:** un [JWT](auth-jwt.md) simple no se puede revocar;
  una [Sesión](auth-sessions.md) es revocable pero necesita una consulta al
  almacén por petición; `JwtLifecycle` es el camino intermedio — verificación
  sin estado, más una lista de bloqueo JTI para las revocaciones que realmente
  necesitas (logout, rotación).
- **Los endpoints HTTP están delimitados por tenant.** `jwt_router` resuelve los
  usuarios mediante el contexto del tenant + `rustango_users`; móntalo en una
  aplicación `.tenancy()`. El motor de tokens (`JwtLifecycle`) en sí no tiene tal
  requisito.
- **Combina esto** con el `JwtBackend` de la [cadena de backends de
  autenticación](auth-backends.md) para autenticar rutas arbitrarias a partir de
  la cabecera `Authorization: Bearer`.
- **Firma HS256**, suelo de clave de 32 bytes — mismo algoritmo y mismas
  restricciones que el [JWT independiente](auth-jwt.md#security-model).


---

## Véase también

- [JWT (independiente)](auth-jwt.md)
- [Backends de autenticación](auth-backends.md)
- [Sesiones](auth-sessions.md)
- [Guía de seguridad](security.md)
