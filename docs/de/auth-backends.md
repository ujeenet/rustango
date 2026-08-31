# Auth-Backends

Ein **Auth-Backend** beantwortet eine einzige Frage: *wer ist der Benutzer bei
einer eingehenden Anfrage?* **Rustango** lässt Sie mehrere davon stapeln — HTTP
Basic, API-Schlüssel, JWT — zu einer Kette, die die Auth-Middleware der Reihe
nach durchprobiert, sodass eine Anwendung Menschen und Maschinen auf denselben
Routen akzeptieren kann. Das ist Djangos `AUTHENTICATION_BACKENDS`-Idee, an axum
verdrahtet. Kombinieren Sie es mit `require_auth` / `require_perm`, um Routen
abzusichern, und dem `CurrentUser`-Extraktor, um das Ergebnis zu lesen.

[![Auth-Backends in Rustango: eine Anfrage durchläuft eine Kette von Backends (ModelBackend, ApiKeyBackend, JwtBackend); das erste, das den Anmeldenachweis erkennt, gewinnt und injiziert CurrentUser, dann prüft require_perm einen Codename](../img/auth-backends.png)](../img/auth-backends.png)

> **Ein Begriff hier neu?** *Backend*, *Middleware*, *Extraktor*,
> *Berechtigungs-Codename* — siehe das [Glossar](glossary.md).

> **Quelle:** `rustango::tenancy::auth_backends` (`AuthBackend`, `ModelBackend`,
> `ApiKeyBackend`, `JwtBackend`, `AuthUser`, `AuthError`) und
> `rustango::tenancy::{RouterAuthExt, CurrentUser}` — hinter dem Feature
> `tenancy`. Eine portable, datenbankunabhängige Registry lebt auch unter
> `rustango::auth_backends` (immer kompiliert).
>
> **Ausführbare Version:** jeder Ausschnitt ist aus
> [`auth_backends_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_backends_doc.rs)
> kopiert (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Inhaltsverzeichnis

- [Die Kette](#the-chain) · [Die eingebauten Backends](#the-built-in-backends)
- [Routen absichern: require_auth](#gating-routes-require_auth)
- [Den Benutzer lesen: CurrentUser](#reading-the-user-currentuser)
- [Berechtigungen: require_perm](#permissions-require_perm)
- [Die portable Registry](#the-portable-registry)
- [Siehe auch](#see-also)

---

## Die Kette

Sie übergeben `require_auth` einen `Vec<Arc<dyn AuthBackend>>`. Bei jeder Anfrage
probiert die Middleware sie **der Reihe nach** durch:

- das **erste** Backend, das den Anmeldenachweis erkennt, gewinnt (gibt den
  Benutzer zurück);
- ein Backend, das ihn nicht erkennt, gibt „keiner" zurück, und das nächste wird
  probiert;
- wenn ein Backend hart fehlschlägt (z. B. ein inaktives Konto bei einem
  *gültigen* Token), stoppt die Kette mit diesem Fehler;
- wenn keines passt, erhält die Anfrage `401` (mit `require_auth`) oder fährt
  anonym fort (mit `optional_auth`).

```rust
use std::sync::Arc;
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),    // HTTP Basic  → humans
    Arc::new(ApiKeyBackend),   // Bearer key  → machines
];
```

---

## Die eingebauten Backends

| Backend | Anmeldenachweis, den es liest | Identifiziert einen Benutzer über |
|---|---|---|
| `ModelBackend` | `Authorization: Basic <base64(user:pass)>` | Benutzername + argon2id-Passwortprüfung gegen `rustango_users` |
| `ApiKeyBackend` | `Authorization: Bearer <prefix.secret>` | die Tabelle `rustango_api_keys` (siehe [API-Schlüssel](auth-api-keys.md)) |
| `JwtBackend` | `Authorization: Bearer <jwt>` | ein signiertes HS256-Token (siehe [JWT](auth-jwt.md)) |

`ApiKeyBackend` und `JwtBackend` lesen beide `Bearer` und unterscheiden anhand der
Form (das erste Punkt-Segment eines API-Schlüssels ist genau 8 Zeichen lang).
Konstruieren Sie `JwtBackend` mit einem Geheimnis von **mindestens 32 Byte**
(`JwtBackend::new(secret)` panikt andernfalls):

```rust
use rustango::tenancy::auth_backends::JwtBackend;

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),
    Arc::new(JwtBackend::new(jwt_secret_at_least_32_bytes.to_vec())),
];
```

Schreiben Sie ein eigenes Backend, indem Sie den Trait implementieren (eine
einzige async-Methode, die die `Parts` der Anfrage inspiziert und
`Option<AuthUser>` zurückgibt):

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

## Routen absichern: require_auth

`RouterAuthExt` fügt die Middleware hinzu. `require_auth` weist anonyme Anfragen
mit `401` ab; `optional_auth` lässt sie durch (sodass ein Handler auf angemeldet
vs. nicht verzweigen kann):

```rust
use rustango::tenancy::RouterAuthExt;

let app = Router::new()
    .route("/profile", get(profile))
    .require_auth(backends, pool);     // 401 if no backend matches
```

Verifiziertes Verhalten:

```rust
// no credentials               → 401
// Basic alice:<correct>        → 200
// Basic alice:<wrong>          → 401   (no backend accepted; no enumeration)
// Bearer <valid api key>       → 200
```

---

## Den Benutzer lesen: CurrentUser

Handler lesen den authentifizierten Benutzer mit dem `CurrentUser`-Extraktor. Er
ist unfehlbar — `Some(user)`, wenn ein Backend einen aufgelöst hat, sonst `None`:

```rust
use rustango::tenancy::CurrentUser;

async fn profile(CurrentUser(user): CurrentUser) -> Response {
    match user {
        Some(u) => format!("hello {}", u.username).into_response(),
        None    => StatusCode::UNAUTHORIZED.into_response(),
    }
}
```

> **Fallstrick:** weil `CurrentUser` unfehlbar ist, führt das Vergessen von
> `require_auth` nicht zu einem Kompilierfehler — jede Anfrage sieht einfach
> `None`. Hinter `require_auth` erhalten anonyme Anfragen bereits `401`, sodass
> `user` dort stets `Some` ist.

---

## Berechtigungen: require_perm

`require_perm` sichert eine Route über einen Berechtigungs-**Codename** ab
(`{table}.{action}`, z. B. `post.add`). Wenden Sie es auf den inneren Sub-Router
an und `require_auth` auf den äußeren, sodass der Benutzer aufgelöst wird,
*bevor* die Berechtigung geprüft wird:

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

Auflösung: ein (aktiver) **Superuser** passiert alles; ein **deaktivierter**
Benutzer wird selbst mit Erteilungen abgelehnt; eine explizite Überschreibung pro
Benutzer gewinnt über Rollen-Erteilungen; andernfalls passiert jede Rolle, die
der Benutzer hält und die den Codename erteilt. Erteilen Sie mit
`set_user_perm_pool` / Rollen über `create_role_pool` + `assign_role_pool` (die
Berechtigungstabellen werden von `ensure_tables_pool` erstellt).

---

## Die portable Registry

Separat davon ist `rustango::auth_backends` (Achtung: Crate-Wurzel, **nicht**
`tenancy`) eine kleine **framework-unabhängige** Registry — eine
`Credentials` → `Principal`-Kette mit ihrem eigenen `AuthBackend`-Trait. Sie hat
keinerlei HTTP/axum-Verklebung; verwenden Sie sie, wenn Sie Django-artige
Backend-Steckbarkeit innerhalb Ihres eigenen Auth-Codes wünschen:

```rust
use rustango::auth_backends::{AuthBackendChain, Credentials, RemoteUserBackend};

let chain = AuthBackendChain::new().with(Arc::new(RemoteUserBackend::trust_username()));
let principal = chain.authenticate(&Credentials::remote("alice")).await?;
```

Dieselbe Semantik „erster Erfolg gewinnt / erster Fehler stoppt" wie die
HTTP-Kette. Zum Absichern echter Routen verwenden Sie die `tenancy`-Middleware
oben.

---

## Siehe auch

- [API-Schlüssel](auth-api-keys.md) und [JWT](auth-jwt.md) — die
  Anmeldenachweise, die `ApiKeyBackend` / `JwtBackend` konsumieren.
- [Passwörter](auth-passwords.md) — das Hashing, gegen das `ModelBackend`
  verifiziert.
- [Zugriffs-Dekoratoren](auth-decorators.md) — Absicherung pro Handler mit
  `login_required` / `permission_required`, die dekorator-artige Alternative zu
  `require_auth`/`require_perm`.
- [Sessions](auth-sessions.md) — cookie-basierte Authentifizierung für Browser.
