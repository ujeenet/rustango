# SSO (OpenID Connect / inicio de sesión social)

Inicia sesión con un proveedor de identidad externo — Google, Microsoft / Azure
AD, GitHub, GitLab, Discord o **cualquier proveedor OpenID Connect** (Okta,
Auth0, Keycloak, …) — en lugar de con una contraseña local.

Puedes configurar **múltiples proveedores**, cada uno gestionado desde la interfaz
de administración como una fila (sin archivo de configuración, sin recompilar). Los
endpoints de un proveedor se autodescubren a partir de su URL de emisor OIDC al
iniciar sesión; los proveedores sociales usan presets integrados.

El SSO es **vinculación con existente** para el administrador: el correo verificado
que devuelve el IdP debe coincidir con un usuario administrador existente. Autentica
a la persona; nunca crea cuentas ni concede acceso por sí solo. Un correo
desconocido o no verificado se rechaza. (El flujo de miembro, más abajo, puede optar
por el aprovisionamiento automático.)

> **Fuente:** el núcleo independiente del admin `rustango::sso` (`SsoProvider`,
> `build_provider`, `verified_email`, `ResolvedSso`, `SsoError`), el cableado del
> bare-admin `rustango::admin::sso`, el SSO por tenant/consola
> `rustango::tenancy::sso` (`SharedSsoProvider`) y el SSO de miembro
> `rustango::tenancy::member_auth`.

## Funcionalidades y quién las usa

Desde **0.49** el núcleo de SSO es su propia funcionalidad, independiente del
auto-admin, de modo que un inicio de sesión de usuario final (miembro) puede
compilarse sin arrastrar `crate::admin`:

| Funcionalidad | Arrastra | Te da |
|---|---|---|
| `sso` | `oauth2`, `casts` | El núcleo independiente del admin: `rustango::sso` — el handshake OIDC / OAuth social, el modelo `SsoProvider` respaldado por BD (secreto cifrado en reposo vía `casts`) y el flujo de miembro (`tenancy::member_auth`, con `tenancy`). |
| `admin-sso` | `admin`, `sso` | Lo anterior **más** el cableado de inicio de sesión del bare-admin (`rustango::admin::sso`) — botones de SSO en la página de inicio de sesión del admin que acuñan la sesión de administrador. |

```toml
[dependencies]
# Admin login with SSO:
rustango = { version = "0.51", features = ["admin-sso"] }
# Member (end-user) SSO without the auto-admin:
rustango = { version = "0.51", features = ["tenancy", "sso"] }
```

`admin::sso_provider` y las rutas históricas del núcleo `admin::sso::*` son
ahora **shims de reexportación** sobre `sso::provider` / `sso::*`, de modo que
los imports existentes de `crate::admin::sso::{build_provider, ResolvedSso, …}` y
`crate::admin::sso_provider::SsoProvider` siguen resolviéndose sin cambios
(el nombre de tabla `rustango_sso_providers` y cada campo quedan intactos — las
migraciones no se ven afectadas).

El correo por el que se vincula a un usuario es la columna `email`. En el modelo
`User` del tenant está condicionado por la funcionalidad **`sso`** (movida fuera de
`admin-sso` en 0.49, de modo que las compilaciones solo con SSO de miembro siguen
obteniendo la columna); el `AdminUser.email` escueto permanece tras `admin-sso`.
Activar o desactivar la funcionalidad emite una migración `AddColumn` / `DropColumn`
para esa columna.

## Cómo funciona

1. La página de inicio de sesión muestra un botón **«Iniciar sesión con
   &lt;proveedor&gt;»** por cada proveedor habilitado.
2. Al hacer clic en uno (`GET <login>/sso/<slug>`) se redirige al IdP con una
   cookie de flujo firmada y de vida corta (PKCE + `state` CSRF).
3. El IdP devuelve al usuario a `<login>/sso/<slug>/callback`.
4. rustango verifica el flujo, intercambia el código, lee `/userinfo`
   y exige **`email_verified`**.
5. Busca un usuario administrador por ese correo. Si existe uno y está activo,
   acuña la **misma sesión de cookie firmada** que produce un inicio de sesión con
   contraseña, vinculada a ese usuario — de modo que cada control existente
   (superusuario / permisos, invalidación en vivo por cambio de contraseña) sigue
   aplicándose.
6. Sin coincidencia → el usuario es devuelto a la página de inicio de sesión con un
   error genérico (los detalles van al log del servidor, nunca al navegador).

El **secreto** del cliente está **cifrado en reposo** — la columna `client_secret`
es un cast [`EncryptedString`](#secret-storage), descifrado en memoria solo en el
momento del inicio de sesión.

## Los proveedores son filas, gestionadas en el admin

Cada proveedor es una fila `SsoProvider`. Aparece como un modelo de administración
corriente — añadir/editar/habilitar desde la interfaz de administración, sin
redespliegue. Campos:

| Campo | Significado |
|---|---|
| `slug` | Clave de ruta estable + id del botón (`<login>/sso/<slug>`). Única. |
| `label` | Texto del botón, p. ej. «Iniciar sesión con Google». |
| `kind` | Un preset — `google` / `microsoft` / `github` / `gitlab` / `discord` — o `oidc` para un proveedor OpenID Connect genérico. |
| `issuer_url` | URL base de descubrimiento OIDC (para `kind = "oidc"`); rustango obtiene `{issuer}/.well-known/openid-configuration`. No se usa en los presets. |
| `client_id` | El id de cliente OAuth del IdP. |
| `client_secret` | El secreto de cliente OAuth, **cifrado en reposo** (nunca en texto plano en la BD). |
| `enabled` | Si el botón aparece en la página de inicio de sesión. |
| `sort_order` | Orden de los botones (ascendente). |
| `scopes` | Anulación opcional de scopes separados por espacios (por defecto `openid email profile`). |

Para añadir un proveedor: introduce el `client_id` + `client_secret`, elige un
`kind` (o `oidc` + un `issuer_url`) y guarda. Los endpoints se descubren al iniciar
sesión — sin cableado de endpoints por proveedor.

## Dónde gestiona los proveedores cada superficie

- **Admin de un solo tenant / independiente** (`crate::admin`): las filas
  `SsoProvider` son una tabla global sencilla, gestionada desde el bare-admin.
  Requiere `Builder::with_session_auth` (el SSO acuña la misma sesión).
- **Admin de tenant** (multi-tenancy): cada tenant gestiona sus **propias**
  filas `SsoProvider` desde su admin — granular, autoservicio, aislado por
  tenant.
- **Consola de operador** (multi-tenancy): un operador define un
  **`SharedSsoProvider`** una vez y se ofrece a **todos** los tenants
  (un Google a nivel de empresa, por ejemplo). Gestionado desde el panel
  *Shared SSO* de la consola.

En la página de inicio de sesión de un tenant los dos conjuntos se fusionan, y ante
un choque de slug **gana el propio proveedor del tenant** sobre el compartido — así
un tenant puede anular un proveedor compartido para sí mismo.

La URL de callback se deriva por petición a partir del host + slug
(`https://<host><login>/sso/<slug>/callback`), así que regístrala con el
IdP. Vincula a un usuario estableciendo la columna `email` en su fila de
`rustango_users` (tenant) / `rustango_admin_users` (bare) con la dirección que
devuelve el IdP.

## SSO de miembro (usuario final)

Las superficies anteriores inician sesión a las personas en un **admin**.
`tenancy::member_auth` es el análogo orientado a miembros: inicia la sesión de un
usuario final en el propio pool de usuarios de un tenant (`rustango_users`) y
acuña una **sesión de miembro**, de modo que un socio de gimnasio / cliente SaaS
puede «Iniciar sesión con Google» sin tocar el admin. Reutiliza exactamente el mismo
núcleo `rustango::sso` y las propias filas `SsoProvider` del tenant — solo difiere
la sesión que acuña, razón por la cual vive tras la funcionalidad `sso` (no
`admin-sso`) y no necesita auto-admin.

Monta `member_sso_router` en una pila `tenancy::server::Builder` (lee el
`Arc<TenantContext>` resuelto que el builder inyecta):

```rust
use rustango::tenancy::member_auth::{member_sso_router, MemberAuthConfig};

let members = member_sso_router(MemberAuthConfig {
    login_base:     "/auth".into(),   // buttons link to /auth/sso/<slug>
    landing_url:    "/".into(),       // post-login destination (honors a same-origin ?next)
    auto_provision: true,             // create a user from a verified email on first sign-in
    session_ttl:    7 * 24 * 60 * 60, // 7 days
    ..Default::default()
});
```

Monta dos rutas por slug a partir de `login_base`:

- `GET {login_base}/sso/{slug}` — inicia el handshake, redirige al IdP.
- `GET {login_base}/sso/{slug}/callback` — lo completa, encuentra-o-aprovisiona
  al miembro, acuña la cookie de sesión.

Diferencias respecto al flujo del admin:

- **Aprovisionamiento automático.** Con `auto_provision = true` (el valor por
  defecto), un correo de IdP verificado sin una fila `rustango_users` coincidente
  **crea** una — nombre de usuario a partir de la parte local del correo (deduplicado
  ante un choque), un hash de contraseña aleatorio real pero inutilizable (los
  usuarios de SSO no pueden iniciar sesión con contraseña).
  Ponlo a `false` para la vinculación-con-existente al estilo admin (correo
  desconocido rechazado).
- **Su propia cookie de sesión.** La cookie de miembro
  (`rustango_member_session`) está **separada por dominio** de las cookies de
  sesión de tenant / admin: el mensaje firmado lleva una etiqueta por dominio y
  un claim de audiencia, de modo que una cookie de miembro nunca puede validarse
  como una cookie de tenant/admin (o viceversa) aunque ambas estén firmadas con
  `RUSTANGO_SESSION_SECRET`. Está ligada al slug (una cookie acuñada para `acme`
  nunca autentica en `globex`) y se invalida con una rotación de contraseña
  (a la par que la sesión del admin).

Lee el miembro actual en un handler con el extractor **`CurrentMember`** — el
análogo de miembro de `SessionUser`. Es infalible
(`None` para sesiones anónimas / caducadas / rotadas / entre tenants),
así que se compone con rutas públicas:

```rust
use rustango::tenancy::member_auth::CurrentMember;

async fn dashboard(CurrentMember(member): CurrentMember) -> impl axum::response::IntoResponse {
    match member {
        Some(user) => format!("Hi, {}", user.username),
        None => "Please sign in".to_owned(),
    }
}
```

> **Alcance v1.** El SSO de miembro resuelve proveedores únicamente a partir de las
> propias filas `SsoProvider` del tenant — la fusión de `SharedSsoProvider` a nivel
> de registro y un hook `provision` personalizado son seguimientos futuros.

## Almacenamiento de secretos

`client_secret` se almacena **cifrado en reposo** con XChaCha20-Poly1305
(AEAD), con la clave derivada de la variable de entorno
**`RUSTANGO_SECRET_KEY`**. Se descifra en memoria solo al iniciar sesión, para
autenticarse ante el endpoint de token del IdP. Así, un volcado de BD filtrado nunca
expone el secreto, y cada tenant conserva su propio secreto sin una variable de
entorno por proveedor.

> Establece `RUSTANGO_SECRET_KEY` en el despliegue (cualquier longitud; se le aplica
> SHA-256 para obtener una clave de 32 bytes). Sin ella, guardar o usar un proveedor
> falla de inmediato — la misma postura que ante una URL de base de datos ausente.

## Proveedores (presets)

Presets integrados: `google`, `microsoft` (Azure AD), `github`, `gitlab`,
`discord`. Para cualquier otra cosa, usa `kind = "oidc"` con un `issuer_url` —
rustango ejecuta el descubrimiento de OpenID Connect para encontrar los endpoints.
(Sign in with Apple no es un preset; necesita verificación de id_token/JWKS.)

## Notas de seguridad

- **Solo correo verificado** — los correos de IdP no verificados se rechazan.
- **Sin aprovisionamiento automático** — un correo desconocido no puede entrar;
  crea el usuario administrador (y establece su `email`) primero.
- **Secretos cifrados en reposo** (`RUSTANGO_SECRET_KEY`), descifrados solo en
  memoria al iniciar sesión; los formularios de edición enmascaran el secreto
  almacenado.
- La cookie de flujo es de vida corta (10 min), `HttpOnly`, `SameSite=Lax`
  y `Secure` sobre HTTPS; el handshake lleva PKCE + un `state` firmado.
- Las sesiones de SSO son la sesión de administrador corriente — rotar o
  desactivar al usuario vinculado las invalida a través del control en vivo
  existente.
- El modelo de confianza es `/userinfo` sobre TLS (el id_token no se verifica de
  forma independiente); antepón HTTPS al admin.

## Véase también

- [Guía de seguridad](security.md) · [Autenticación](auth-flows.md)
