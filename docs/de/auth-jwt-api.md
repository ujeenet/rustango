# JWT-Auth-API

Das Modul [eigenständiges JWT](auth-jwt.md) signiert und verifiziert einen
einzelnen Token. Eine echte API braucht den ganzen **Lebenszyklus**: einen
kurzlebigen *Access*-Token, einen langlebigen *Refresh*-Token, Rotation beim
Refresh und **Widerruf** für das Abmelden. **Rustango** liefert das als
`JwtLifecycle` — und einen schlüsselfertigen Router, der `POST /api/auth/login`,
`/refresh`, `/logout` und `GET /me` für Sie einhängt.

[![JWT-Auth-API: login stellt ein Access+Refresh-Paar aus, refresh rotiert und setzt den alten Token auf die Sperrliste, logout widerruft über einen JTI-Speicher](img/auth-jwt-api.png)](img/auth-jwt-api.png)

> **Quelle:** `rustango::tenancy::jwt_lifecycle` (`JwtLifecycle`, `JwtTokenPair`,
> `JwtClaims`) und `rustango::tenancy::auth_routes` (`jwt_router`, `Config`) +
> `rustango::jti_store` (`JtiStore`, `InMemoryJtiStore`) — hinter `jwt` +
> `tenancy`.
>
> **Lauffähige Version:** Die Token-Engine wird durch den getesteten
> [`auth_demo`](../crates/rustango/examples/auth_demo/tests/auth_jwt_api.rs)
> abgedeckt — `cargo test -p auth_demo --test auth_jwt_api`. Die HTTP-Endpunkte
> sind tenant-bezogen und werden durchgängig durch das eigene
> `crates/rustango/tests/tenant_auth_live.rs` des Frameworks geprüft.

> **Ein Begriff hier neu für Sie?** *Access-/Refresh-Token*, *Rotation*,
> *Widerruf* — siehe das [Glossar](glossary.md).

> Vertiefende Ergänzung zum Abschnitt „JWTs ausstellen und erneuern“ des
> [Sicherheitsleitfadens](security.md). Für einen einzelnen, manuell verwalteten
> Token siehe stattdessen [JWT (eigenständig)](auth-jwt.md).

---

## Inhaltsverzeichnis
- [Der eingebaute Router](#the-built-in-router) · [Die Verdrahtung](#wiring-it-up)
- [Die Token-Engine](#the-token-engine-jwtlifecycle) · [Refresh & Rotation](#refresh-and-rotation)
- [Widerruf & der JTI-Speicher](#revocation-and-the-jti-store) · [Benutzerdefinierte Claims](#custom-claims)
- [Hinweise und Grenzen](#notes-and-limits)

---

## Der eingebaute Router

`jwt_router` hängt die vier Standard-Endpunkte gegen die tenant-spezifische
Tabelle `rustango_users` ein — die ~50 Zeilen Login-Boilerplate, die jedes
Projekt sonst neu schreibt:

| Methode | Pfad | Body / Auth | Liefert |
|---|---|---|---|
| POST | `/api/auth/login` | `{username, password}` | `{access, refresh, user}` |
| POST | `/api/auth/refresh` | `{refresh}` | `{access, refresh}` |
| POST | `/api/auth/logout` | `Authorization: Bearer <access>` | `204` (widerruft die JTI) |
| GET | `/api/auth/me` | `Authorization: Bearer <access>` | `{user_id, username, is_superuser}` |

Login verifiziert das Passwort mit [argon2id](auth-passwords.md) und stellt dann
ein Paar aus. Pfade, TTLs und der Signierschlüssel sind über `Config`
konfigurierbar.

## Die Verdrahtung

```rust
use rustango::tenancy::auth_routes::{jwt_router, Config};

rustango::manage::Cli::new()
    .tenancy()
    .api(my_app::urls::api()
        .merge(jwt_router(Config::default())))   // hängt /api/auth/* ein
    .run()
    .await
```

`Config::default()` signiert mit `RUSTANGO_SESSION_SECRET` (demselben Schlüssel
wie das Admin-Session-Cookie) und verwendet TTLs von 15 Min. Access / 7 Tage
Refresh. Überschreiben Sie `prefix`, `access_ttl_secs`, `refresh_ttl_secs` oder
`session_secret` nach Bedarf. Die Endpunkte laufen im Tenant-Kontext, hängen Sie
sie also in einer Tenancy-App ein.

```sh
# Login → access + refresh
curl -sX POST localhost:8080/api/auth/login \
  -H 'content-type: application/json' \
  -d '{"username":"alice","password":"hunter2hunter"}'

# Einen geschützten Endpunkt aufrufen
curl localhost:8080/api/auth/me -H "Authorization: Bearer $ACCESS"
```

---

## Die Token-Engine (`JwtLifecycle`)

Unter dem Router sitzt `JwtLifecycle` — direkt nutzbar, wenn Sie den
Lebenszyklus ohne die eingebaute HTTP-Form wollen:

```rust
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

let jwt = JwtLifecycle::new(secret_32_bytes);

// Login: das Paar ausstellen.
let pair = jwt.issue_pair(user_id);
// → pair.access  (kurze TTL, im Authorization-Header senden)
// → pair.refresh (lange TTL, in einem HttpOnly-Cookie / sicheren Speicher ablegen)

// Authentifizierte Anfrage: den Access-Token verifizieren.
match jwt.verify_access(&access) {
    Some(claims) => { /* claims.sub ist die Benutzer-ID */ }
    None => { /* 401: ungültig, abgelaufen, widerrufen oder falscher Typ */ }
}
```

Access- und Refresh-Tokens sind **nicht austauschbar** — `verify_access` lehnt
einen Refresh-Token ab und umgekehrt, sodass ein gestohlener kurzlebiger
Access-Token nicht zum Erzeugen neuer Tokens verwendet werden kann:

```rust
let pair = jwt.issue_pair(42);
assert!(jwt.verify_refresh(&pair.access).is_none());
assert!(jwt.verify_access(&pair.refresh).is_none());
```

---

## Refresh und Rotation

`refresh` tauscht einen gültigen Refresh-Token gegen ein **neues Paar** und setzt
die JTI des alten Refresh-Tokens auf die Sperrliste — gleitender Ablauf mit
Refresh-Tokens für den einmaligen Gebrauch (das Wiedereinspielen des alten wird
abgelehnt):

```rust
let pair = jwt.issue_pair(7);
let rotated = jwt.refresh(&pair.refresh).expect("refresh ok");
assert_ne!(pair.access, rotated.access);
assert!(jwt.refresh(&pair.refresh).is_none());   // der alte Refresh ist jetzt tot
```

Standardmäßig **bewahrt** `refresh` die benutzerdefinierten Claims des Tokens.
Wenn sich Berechtigungen geändert haben könnten (Rolle widerrufen, Scope
herabgestuft), verwenden Sie `refresh_with(token, new_claims)`, um ein frisches
Payload einzusetzen, während die alte Refresh-JTI dennoch auf die Sperrliste
gesetzt wird.

---

## Widerruf und der JTI-Speicher

Jeder Token trägt eine eindeutige `jti`. `revoke` fügt sie einer Sperrliste
hinzu, sodass nachfolgende `verify_*`-Aufrufe fehlschlagen, bis der Token ohnehin
abgelaufen wäre — genau das ruft `POST /api/auth/logout` auf:

```rust
let pair = jwt.issue_pair(1);
assert!(jwt.revoke(&pair.access));
assert!(jwt.verify_access(&pair.access).is_none());
```

Die Sperrliste liegt in einem austauschbaren `JtiStore`. Der Standard
`InMemoryJtiStore` ist **einprozessig und verliert Widerrufe beim Neustart** —
in Ordnung für eine einzelne Instanz. Jede Multi-Replica-Bereitstellung MUSS
einen gemeinsamen, dauerhaften Speicher (Redis / DB) installieren, damit ein
Logout auf einer Replica von allen respektiert wird:

```rust
use rustango::jti_store::{InMemoryJtiStore, JtiStore};
use std::sync::Arc;

let shared: Arc<dyn JtiStore> = Arc::new(InMemoryJtiStore::new()); // in Prod durch Redis ersetzen
let a = JwtLifecycle::new(secret.clone()).with_jti_store(Arc::clone(&shared));
let b = JwtLifecycle::new(secret).with_jti_store(Arc::clone(&shared));

let pair = a.issue_pair(5);
a.revoke(&pair.access);
assert!(b.verify_access(&pair.access).is_none());   // B sieht As Widerruf
```

> Ohne gemeinsamen Speicher ist `/logout` bestenfalls „best-effort“: Ein
> widerrufener Token kann auf einer anderen Replica bis zu seinem natürlichen
> Ablauf noch akzeptiert werden. Dies ist die einzelne wichtigste
> Produktionseinstellung für JWT-Auth.

---

## Benutzerdefinierte Claims

Betten Sie `roles` / `tenant` / `scope` direkt in den Token ein, sodass die
Verifizierung keine DB-Abfrage benötigt. Reservierte Namen (`sub`, `exp`, `jti`,
`typ`) werden abgelehnt:

```rust
let custom = serde_json::json!({ "roles": ["admin"], "tenant": "acme" })
    .as_object().unwrap().clone();
let pair = jwt.issue_pair_with(99, custom)?;

let claims = jwt.verify_access(&pair.access).unwrap();
let roles: Vec<String> = claims.get_custom("roles").unwrap();   // ["admin"]
```

Benutzerdefinierte Claims überleben `refresh` (werden auf das neue Paar
übertragen), es sei denn, Sie verwenden `refresh_with`.

---

## Hinweise und Grenzen

- **Sessions vs. JWT vs. dies:** Ein einfaches [JWT](auth-jwt.md) kann nicht
  widerrufen werden; eine [Session](auth-sessions.md) ist widerrufbar, braucht
  aber eine Speicherabfrage pro Anfrage; `JwtLifecycle` ist der Mittelweg —
  zustandslose Verifizierung, plus eine JTI-Sperrliste für die Widerrufe, die Sie
  tatsächlich brauchen (Logout, Rotation).
- **HTTP-Endpunkte sind tenant-bezogen.** `jwt_router` löst Benutzer über den
  Tenant-Kontext + `rustango_users` auf; hängen Sie ihn in einer
  `.tenancy()`-App ein. Die Token-Engine (`JwtLifecycle`) selbst hat diese
  Anforderung nicht.
- **Kombinieren Sie dies** mit dem `JwtBackend` der
  [Auth-Backend-Kette](auth-backends.md), um beliebige Routen aus dem
  `Authorization: Bearer`-Header zu authentifizieren.
- **HS256-Signierung**, 32-Byte-Schlüssel-Untergrenze — derselbe Algorithmus und
  dieselben Einschränkungen wie beim [eigenständigen JWT](auth-jwt.md#security-model).


---

## Siehe auch

- [JWT (eigenständig)](auth-jwt.md)
- [Auth-Backends](auth-backends.md)
- [Sessions](auth-sessions.md)
- [Sicherheitsleitfaden](security.md)
