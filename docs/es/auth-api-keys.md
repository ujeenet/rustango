# Claves de API

Una clave de API es una **credencial de larga duración para máquinas** — trabajos
de CI, scripts, llamadas de servidor a servidor — que no pueden presentar un
formulario de inicio de sesión ni llevar una cookie de sesión. El cliente envía
la clave en cada petición; el servidor la busca e identifica al llamante.
**Rustango** te ofrece dos capas: un ayudante autónomo de generación/verificación
que puedes conectar a tu propia tabla, y un backend listo para usar que almacena
las claves y autentica las peticiones `Authorization: Bearer`.

[![Claves de API en Rustango: generate_key devuelve un token de un solo uso prefix.secret, almacenas el prefijo de 8 caracteres más un hash argon2id, y verify_key comprueba un secreto entrante](../img/auth-api-keys.png)](../img/auth-api-keys.png)

> **¿Algún término te resulta nuevo?** *Token*, *hash*, *Bearer*, *argon2id* — el
> [glosario](glossary.md) define los bloques de construcción.

> **Fuente:** `rustango::api_keys` (`generate_key`, `hash_secret`, `verify_key`,
> `split_token`, `ApiKeyError`) — el ayudante autónomo, tras la característica
> `api_keys` (activada por defecto). El backend con almacenamiento es
> `rustango::tenancy::auth_backends` (`create_api_key`, `ApiKeyBackend`,
> `ensure_api_keys_table_pool`) — tras la característica `tenancy`.
>
> **Versión ejecutable:** los fragmentos del ayudante están copiados de
> [`auth_api_keys_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_api_keys_doc.rs)
> (`cargo test -p rustango --test auth_api_keys_doc`); el flujo del middleware
> `ApiKeyBackend` de
> [`auth_backends_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_backends_doc.rs)
> (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Tabla de contenidos

- [Cómo funciona una clave de API](#how-an-api-key-works)
- [El ayudante autónomo](#the-standalone-helper)
- [El backend con almacenamiento](#the-stored-backend)
- [Emitir una clave (CLI + código)](#issuing-a-key)
- [Autenticar peticiones](#authenticating-requests)
- [Notas de seguridad](#security-notes)
- [Véase también](#see-also)

---

## Cómo funciona una clave de API

Una clave tiene dos partes unidas por un punto: **`{prefix}.{secret}`**.

- El **prefijo** tiene 8 caracteres — se almacena en texto claro y se usa como
  índice de búsqueda rápido y único («¿qué clave es esta?»).
- El **secreto** es la credencial real. Solo almacenas un **hash argon2id** del
  mismo, nunca el secreto en sí.

El token completo se muestra al usuario **exactamente una vez**, en la creación.
Piérdelo y lo reemites — no hay forma de recuperarlo, porque solo se conserva el
hash. Es la misma disciplina de «hashea, no almacenes» que en las
[contraseñas](auth-passwords.md), aplicada a las credenciales de máquina.

---

## El ayudante autónomo

`rustango::api_keys` es un conjunto de herramientas sin dependencias (sin base de
datos, sin tablas) — úsalo cuando quieras almacenar las claves en tu propio
esquema.

```rust
use rustango::api_keys::{generate_key, split_token, verify_key};

// En la creación: devuelve (full_token, prefix, hash).
let (token, prefix, hash) = generate_key()?;
// → token  = "a1b2c3d4.<secret>"   mostrar al usuario UNA VEZ
// → prefix = "a1b2c3d4"            almacenar como clave de búsqueda
// → hash   = "$argon2id$v=19$..."  almacenar en lugar del secreto

// En una petición entrante: extraer el token, encontrar la fila por prefijo, verificar.
let (prefix, secret) = split_token(&token).expect("well-formed token");
let stored_hash = lookup_hash_by_prefix(prefix);     // tu consulta
if verify_key(secret, &stored_hash)? {
    // autenticado
}
```

`split_token` es estricto — devuelve `None` a menos que el prefijo tenga
exactamente 8 caracteres y el secreto no esté vacío, de modo que una entrada
malformada se rechaza antes de que toques la base de datos:

```rust
assert!(split_token("no-dot-here").is_none());
assert!(split_token("short.secret").is_none()); // el prefijo debe tener 8 caracteres
assert!(split_token("a1b2c3d4.").is_none());     // secreto vacío
```

`hash_secret` y `verify_key` usan argon2id con una sal aleatoria por hash, de
modo que hashear el mismo secreto dos veces produce cadenas distintas — y ambas
se verifican. `verify_key` devuelve `Ok(false)` en caso de discrepancia y
`Err(ApiKeyError)` solo cuando la cadena almacenada no es un hash válido.

---

## El backend con almacenamiento

Si ya estás en la capa `tenancy`, no necesitas tu propia tabla.
`rustango::tenancy::auth_backends` incluye un modelo `ApiKey` (tabla
`rustango_api_keys`), un creador, y un backend de autenticación que se conecta a
la [cadena de backends](auth-backends.md).

Inicializa la tabla una vez (tri-dialecto, idempotente):

```rust
use rustango::tenancy::auth_backends::ensure_api_keys_table_pool;

ensure_api_keys_table_pool(&pool).await?;   // CREATE TABLE IF NOT EXISTS
```

La fila `ApiKey` almacena `user_id` (FK a `rustango_users`), el `key_prefix` de
8 caracteres (único), el `key_hash` argon2id, una `label`, y un `expires_at`
opcional.

---

## Emitir una clave

`create_api_key` genera el token, hashea el secreto, inserta la fila, y devuelve
el **token en texto claro una sola vez**:

```rust
use rustango::tenancy::auth_backends::create_api_key;

// Emitir una clave sin expiración para el usuario 42, etiquetada "ci-key".
let token = create_api_key(42, "ci-key", None, &pool).await?;
println!("Store this — it won't be shown again: {token}");

// O con una expiración:
use chrono::{Duration, Utc};
let token = create_api_key(42, "tmp", Some(Utc::now() + Duration::days(30)), &pool).await?;
```

Desde la línea de comandos, la CLI `manage` envuelve la misma llamada:

```bash
cargo run -- create-api-key <tenant> <username> --label "ci-key" --expires-days 30
```

---

## Autenticar peticiones

Registra `ApiKeyBackend` en tu [cadena de backends de
autenticación](auth-backends.md) y el middleware autentica cualquier petición
`Authorization: Bearer {prefix}.{secret}`:

```rust
use std::sync::Arc;
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};
use rustango::tenancy::RouterAuthExt;

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),   // HTTP Basic (humanos)
    Arc::new(ApiKeyBackend),  // clave Bearer  (máquinas)
];

let app = Router::new()
    .route("/api/data", get(handler))
    .require_auth(backends, pool);
```

Un cliente entonces llama:

```bash
curl https://api.example.com/api/data \
  -H "Authorization: Bearer a1b2c3d4.the-secret-half"
```

El backend encuentra la `ApiKey` por su prefijo de 8 caracteres, comprueba
`expires_at`, verifica el secreto contra el hash almacenado, carga al usuario
propietario, y lo inyecta para que tus handlers lo lean mediante
[`CurrentUser`](auth-backends.md). Un secreto incorrecto o un prefijo
desconocido es un `401`; una clave caducada se rechaza; un propietario
deshabilitado es un `403`.

---

## Notas de seguridad

- **El secreto se muestra una vez.** Solo se persisten el prefijo + el hash
  argon2id — no hay recuperación, solo reemisión.
- **El prefijo se almacena en texto claro a propósito** — es el índice de
  búsqueda O(1). Una fuga de base de datos revela qué prefijos existen, nunca los
  secretos.
- **El tiempo está igualado.** Un prefijo desconocido igualmente ejecuta una
  verificación ficticia, de modo que una clave ausente tarda más o menos lo mismo
  que una real — sin enumeración a través del tiempo de respuesta.
- **Limita las claves a un usuario, establece una expiración, y rótalas.** Emite
  una por integración para poder revocar una sin perturbar las demás; prefiere
  ventanas `expires_at` cortas para acceso temporal.
- **Desambiguación de los JWT:** el backend trata un valor Bearer como una clave
  de API solo cuando su primer segmento separado por puntos tiene exactamente 8
  caracteres — así las claves de API y los [JWT](auth-jwt.md) pueden compartir la
  cabecera `Authorization: Bearer`.

---

## Véase también

- [Backends de autenticación](auth-backends.md) — la cadena a la que se conecta
  `ApiKeyBackend`, y el extractor `CurrentUser` + el middleware
  `require_auth`/`require_perm`.
- [Firma de peticiones HMAC](auth-hmac.md) — para llamantes de máquina que
  necesitan integridad por petición, no solo una credencial bearer.
- [Contraseñas](auth-passwords.md) — la misma disciplina de «hashea, no
  almacenes» para los inicios de sesión humanos.
- [JWT](auth-jwt.md) — tokens sin estado de corta duración, la otra opción de
  máquina.
