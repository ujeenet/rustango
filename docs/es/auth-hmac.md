# Firma de solicitudes con HMAC

La firma HMAC demuestra **tanto quién envió una solicitud como que no fue
alterada en tránsito**. El cliente firma cada solicitud con un secreto
compartido; el servidor recalcula la firma y las compara. A diferencia de una
[clave de API](auth-api-keys.md) de tipo bearer — que es reproducible si se
captura — una firma HMAC cubre el método, la ruta, la query, la marca temporal
y el cuerpo, de modo que una solicitud manipulada o caducada se rechaza. Es el
esquema que usan AWS SigV4 y las firmas de webhooks, y **Rustango** lo incluye
como una única capa tower.

[![Firma HMAC en Rustango: el cliente firma method+path+query+date+body-hash con un secreto compartido; HmacAuthLayer recalcula y compara en tiempo constante, rechazando solicitudes manipuladas o caducadas](../img/auth-hmac.png)](../img/auth-hmac.png)

> **¿Nuevo en algún término de aquí?** *HMAC*, *secreto compartido*, *replay*,
> *comparación en tiempo constante* — consulta el [glosario](glossary.md).

> **Fuente:** `rustango::hmac_auth` (`HmacAuthLayer`, `KeyResolver`, `sign_now`,
> `sign_request`) — detrás de la feature `hmac-auth` (activa por defecto; la
> protección contra replay además necesita `cache`).
>
> **Versión ejecutable:** cada fragmento está copiado de
> [`auth_hmac_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_hmac_doc.rs)
> (`cargo test -p rustango --test auth_hmac_doc`).

## Tabla de contenidos

- [Cuándo usarlo](#when-to-use-it)
- [Qué se firma](#what-gets-signed)
- [Servidor: verificar con la capa](#server-verify-with-the-layer)
- [Cliente: firmar una solicitud](#client-sign-a-request)
- [Desfase de reloj y replay](#clock-skew-and-replay)
- [Límites](#limits)
- [Véase también](#see-also)

---

## Cuándo usarlo

| Usa… | Cuándo |
|---|---|
| [Clave de API](auth-api-keys.md) (Bearer) | Autenticación de máquina sencilla; el riesgo de captura es aceptable (TLS, rotación corta). |
| **Firma HMAC** | Necesitas **integridad por solicitud + resistencia a replay** — webhooks, APIs de socios, cualquier cosa donde una solicitud capturada no deba ser reutilizable ni modificable. |
| [JWT](auth-jwt.md) | Tokens de usuario sin estado y autodescriptivos con claims. |

HMAC requiere que ambos lados tengan el mismo secreto fuera de banda (tú lo
aprovisionas), y relojes razonablemente sincronizados.

---

## Qué se firma

El cliente construye una cadena canónica y le aplica HMAC-SHA256 con el secreto
compartido:

```text
<UPPERCASE-METHOD>\n
<PATH>\n
<SORTED-QUERY>\n
<X-DATE>\n
<HEX-SHA256(BODY)>
```

Dos cabeceras de la solicitud llevan el resultado:

- `X-Date` — una marca temporal RFC 3339 (también parte de la cadena firmada).
- `Authorization: HMAC-SHA256 keyId=<id>,signature=<base64>`

Como la query se **ordena** en ambos extremos, `?b=2&a=1` y `?a=1&b=2` producen
la misma firma. Como el cuerpo se hashea dentro de la cadena, cambiar un solo
byte la invalida.

---

## Servidor: verificar con la capa

`HmacAuthLayer::new` recibe un **`KeyResolver`** — un closure que mapea un
`keyId` a su secreto (`None` ⇒ clave desconocida ⇒ 401). Adjúntalo como una
capa tower normal delante de las rutas que quieras proteger:

```rust
use std::sync::Arc;
use rustango::hmac_auth::{HmacAuthLayer, KeyResolver};
use tower::Layer;

// Resolve key ids to secrets — back this with your DB / secret store.
let resolver: KeyResolver = Arc::new(|key_id: &str| {
    (key_id == "k_demo").then(|| b"shared-secret-at-least-32-bytes-long!!".to_vec())
});

let layer = HmacAuthLayer::new(resolver)
    .tolerance_secs(300);                 // ±5 min clock-skew window (default)

let app = protected_router.layer(layer);
```

Una solicitud firmada correctamente pasa; manipula el cuerpo, quita `X-Date`, o
firma con una clave desconocida y será un `401`:

```rust
// correctly signed            → 200
// body changed after signing  → 401  (signature mismatch)
// missing X-Date header       → 401
// keyId the resolver rejects  → 401
```

> **Sin extractor de identidad.** La capa verifica la firma pero **no** inyecta
> qué `keyId` firmó en la solicitud — no hay un extractor `HmacUser`. Si un
> handler necesita la identidad del llamante, envuelve la capa o transpórtala tú
> mismo. Los rechazos son respuestas `401`/`413` planas, no un error tipado
> sobre el que hagas match.

---

## Cliente: firmar una solicitud

`sign_now` firma con la hora actual y devuelve los dos valores de cabecera para
adjuntar (`sign_request` es la variante que toma una fecha RFC 3339 explícita):

```rust
use rustango::hmac_auth::sign_now;

let body = br#"{"amount": 100}"#;
let (x_date, authorization) =
    sign_now("k_demo", b"shared-secret-at-least-32-bytes-long!!",
             "POST", "/api/charge", "", body);

// Attach both headers and send the EXACT body you signed:
let req = http::Request::post("/api/charge")
    .header("x-date", x_date)
    .header("authorization", authorization)
    .body(body.to_vec())?;
```

La firma es base64; el hash del cuerpo dentro de la cadena canónica es hex. Envía
el cuerpo byte por byte tal como lo firmaste — cualquier proxy que lo reescriba
(recompresión, reserialización de JSON) rompe la verificación.

---

## Desfase de reloj y replay

La marca temporal `X-Date` acota el replay: una solicitud cuya fecha esté fuera
de `tolerance_secs` (por defecto ±300 s) se rechaza, de modo que una solicitud
capturada solo es reutilizable dentro de esa ventana corta. Para cerrarla por
completo, adjunta un **almacén de nonce** (cualquier `cache::Cache`) y cada firma
podrá gastarse una sola vez dentro de la ventana:

```rust
use rustango::cache::InMemoryCache;

let layer = HmacAuthLayer::new(resolver)
    .tolerance_secs(120)
    .nonce_store(Arc::new(InMemoryCache::new()));  // reject replays
```

En producción usa un almacén **compartido** (Redis) para que la protección se
mantenga a través de las réplicas — una caché en proceso solo protege una
instancia. La comprobación de replay falla en modo abierto (fail-open) ante un
error de caché (disponibilidad por encima del estrecho riesgo dentro de la
ventana).

---

## Límites

- **±desfase simétrico, fechas RFC 3339.** Ambos relojes deben estar más o menos
  sincronizados; el cliente debe enviar la misma marca temporal que firmó
  (`sign_now` te la devuelve).
- **Almacenamiento completo del cuerpo en búfer.** El cuerpo se lee en memoria
  para hashearlo (límite por defecto 10 MiB → `413`; súbelo con `.body_limit(n)`
  pero cuida la memoria). Los cuerpos en streaming no están soportados.
- **La firma va en base64 en el cable, el hash del cuerpo va en hex** — fácil de
  confundir al escribir un cliente en otro lenguaje.
- **Mantén la capa lo más externa posible** respecto a cualquier cosa que mute
  el cuerpo.

---

## Véase también

- [Claves de API](auth-api-keys.md) — credencial bearer más sencilla cuando la
  integridad/replay no son una preocupación.
- [Backends de autenticación](auth-backends.md) — para identificar a un *usuario*
  por solicitud (HMAC demuestra la integridad del mensaje, no una identidad de
  sesión).
- [Webhooks](security.md) — la contraparte entrante: verificar firmas en los
  eventos que recibes.
- [Middleware](middleware.md) — cómo se adjuntan y ordenan las capas tower.
