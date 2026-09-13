# API-Schlüssel

Ein API-Schlüssel ist ein **langlebiges Credential für Maschinen** — CI-Jobs,
Skripte, Server-zu-Server-Aufrufe — die kein Anmeldeformular vorlegen oder kein
Session-Cookie tragen können. Der Client sendet den Schlüssel bei jeder Anfrage;
der Server schlägt ihn nach und identifiziert den Aufrufer. **Rustango** gibt
Ihnen zwei Ebenen: einen eigenständigen Erzeugungs-/Verifizierungshelfer, den
Sie an Ihre eigene Tabelle anbinden können, und ein schlüsselfertiges Backend,
das Schlüssel speichert und `Authorization: Bearer`-Anfragen authentifiziert.

[![API-Schlüssel in Rustango: generate_key liefert einen Einmal-Token prefix.secret, Sie speichern das 8-stellige Präfix plus einen argon2id-Hash, und verify_key prüft ein eingehendes Secret](../img/auth-api-keys.png)](../img/auth-api-keys.png)

> **Ein Begriff hier neu für Sie?** *Token*, *Hash*, *Bearer*, *argon2id* — das
> [Glossar](glossary.md) definiert die Bausteine.

> **Quelle:** `rustango::api_keys` (`generate_key`, `hash_secret`, `verify_key`,
> `split_token`, `ApiKeyError`) — der eigenständige Helfer, hinter dem Feature
> `api_keys` (standardmäßig aktiviert). Das speichernde Backend ist
> `rustango::tenancy::auth_backends` (`create_api_key`, `ApiKeyBackend`,
> `ensure_api_keys_table_pool`) — hinter dem Feature `tenancy`.
>
> **Lauffähige Version:** Die Helfer-Snippets sind aus
> [`auth_api_keys_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_api_keys_doc.rs) kopiert
> (`cargo test -p rustango --test auth_api_keys_doc`); der Middleware-Ablauf des
> `ApiKeyBackend` aus
> [`auth_backends_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_backends_doc.rs)
> (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Inhaltsverzeichnis

- [Wie ein API-Schlüssel funktioniert](#how-an-api-key-works)
- [Der eigenständige Helfer](#the-standalone-helper)
- [Das speichernde Backend](#the-stored-backend)
- [Einen Schlüssel ausstellen (CLI + Code)](#issuing-a-key)
- [Anfragen authentifizieren](#authenticating-requests)
- [Sicherheitshinweise](#security-notes)
- [Siehe auch](#see-also)

---

## Wie ein API-Schlüssel funktioniert

Ein Schlüssel hat zwei durch einen Punkt verbundene Teile:
**`{prefix}.{secret}`**.

- Das **Präfix** hat 8 Zeichen — im Klartext gespeichert und als schneller,
  eindeutiger Nachschlage-Index verwendet („welcher Schlüssel ist das?“).
- Das **Secret** ist das eigentliche Credential. Sie speichern nur einen
  **argon2id-Hash** davon, niemals das Secret selbst.

Der vollständige Token wird dem Benutzer **genau einmal** angezeigt, bei der
Erstellung. Verlieren Sie ihn, stellen Sie ihn neu aus — es gibt keine
Möglichkeit, ihn wiederherzustellen, da nur der Hash aufbewahrt wird. Das ist
dieselbe „hashen, nicht speichern“-Disziplin wie bei
[Passwörtern](auth-passwords.md), angewandt auf Maschinen-Credentials.

---

## Der eigenständige Helfer

`rustango::api_keys` ist ein abhängigkeitsfreies Toolkit (keine Datenbank, keine
Tabellen) — verwenden Sie es, wenn Sie Schlüssel in Ihrem eigenen Schema
speichern wollen.

```rust
use rustango::api_keys::{generate_key, split_token, verify_key};

// Bei der Erstellung: liefert (full_token, prefix, hash).
let (token, prefix, hash) = generate_key()?;
// → token  = "a1b2c3d4.<secret>"   dem Benutzer EINMAL zeigen
// → prefix = "a1b2c3d4"            als Nachschlage-Schlüssel speichern
// → hash   = "$argon2id$v=19$..."  statt des Secrets speichern

// Bei einer eingehenden Anfrage: Token abrufen, Zeile per Präfix finden, verifizieren.
let (prefix, secret) = split_token(&token).expect("well-formed token");
let stored_hash = lookup_hash_by_prefix(prefix);     // Ihre Abfrage
if verify_key(secret, &stored_hash)? {
    // authentifiziert
}
```

`split_token` ist strikt — es liefert `None`, es sei denn, das Präfix hat genau
8 Zeichen und das Secret ist nicht leer, sodass fehlerhafte Eingaben abgelehnt
werden, bevor Sie die Datenbank berühren:

```rust
assert!(split_token("no-dot-here").is_none());
assert!(split_token("short.secret").is_none()); // Präfix muss 8 Zeichen haben
assert!(split_token("a1b2c3d4.").is_none());     // leeres Secret
```

`hash_secret` und `verify_key` verwenden argon2id mit einem zufälligen Salt pro
Hash, sodass das zweimalige Hashen desselben Secrets unterschiedliche Zeichenketten
ergibt — und beide verifizieren sich. `verify_key` liefert `Ok(false)` bei einer
Nichtübereinstimmung und `Err(ApiKeyError)` nur dann, wenn die gespeicherte
Zeichenkette kein gültiger Hash ist.

---

## Das speichernde Backend

Wenn Sie bereits auf der `tenancy`-Ebene sind, brauchen Sie keine eigene Tabelle.
`rustango::tenancy::auth_backends` liefert ein `ApiKey`-Modell (Tabelle
`rustango_api_keys`), einen Ersteller und ein Auth-Backend, das sich in die
[Backend-Kette](auth-backends.md) einklinkt.

Initialisieren Sie die Tabelle einmalig (tri-dialektfähig, idempotent):

```rust
use rustango::tenancy::auth_backends::ensure_api_keys_table_pool;

ensure_api_keys_table_pool(&pool).await?;   // CREATE TABLE IF NOT EXISTS
```

Die `ApiKey`-Zeile speichert `user_id` (FK auf `rustango_users`), das
8-stellige `key_prefix` (eindeutig), den argon2id-`key_hash`, ein `label` und
ein optionales `expires_at`.

---

## Einen Schlüssel ausstellen

`create_api_key` erzeugt den Token, hasht das Secret, fügt die Zeile ein und
liefert den **Klartext-Token einmalig**:

```rust
use rustango::tenancy::auth_backends::create_api_key;

// Einen nicht ablaufenden Schlüssel für Benutzer 42 ausstellen, mit "ci-key" beschriftet.
let token = create_api_key(42, "ci-key", None, &pool).await?;
println!("Store this — it won't be shown again: {token}");

// Oder mit einem Ablauf:
use chrono::{Duration, Utc};
let token = create_api_key(42, "tmp", Some(Utc::now() + Duration::days(30)), &pool).await?;
```

Von der Kommandozeile aus umhüllt die `manage`-CLI denselben Aufruf:

```bash
cargo run -- create-api-key <tenant> <username> --label "ci-key" --expires-days 30
```

---

## Anfragen authentifizieren

Registrieren Sie `ApiKeyBackend` in Ihrer
[Auth-Backend-Kette](auth-backends.md), und die Middleware authentifiziert jede
`Authorization: Bearer {prefix}.{secret}`-Anfrage:

```rust
use std::sync::Arc;
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};
use rustango::tenancy::RouterAuthExt;

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),   // HTTP Basic (Menschen)
    Arc::new(ApiKeyBackend),  // Bearer-Schlüssel  (Maschinen)
];

let app = Router::new()
    .route("/api/data", get(handler))
    .require_auth(backends, pool);
```

Ein Client ruft dann auf:

```bash
curl https://api.example.com/api/data \
  -H "Authorization: Bearer a1b2c3d4.the-secret-half"
```

Das Backend findet den `ApiKey` anhand seines 8-stelligen Präfixes, prüft
`expires_at`, verifiziert das Secret gegen den gespeicherten Hash, lädt den
besitzenden Benutzer und injiziert ihn, damit Ihre Handler ihn über
[`CurrentUser`](auth-backends.md) lesen können. Ein falsches Secret oder ein
unbekanntes Präfix ist ein `401`; ein abgelaufener Schlüssel wird abgelehnt; ein
deaktivierter Besitzer ist ein `403`.

---

## Sicherheitshinweise

- **Das Secret wird einmal angezeigt.** Nur das Präfix + der argon2id-Hash werden
  persistiert — es gibt keine Wiederherstellung, nur eine Neuausstellung.
- **Das Präfix wird absichtlich im Klartext gespeichert** — es ist der
  O(1)-Nachschlage-Index. Ein Datenbankleck verrät, welche Präfixe existieren,
  niemals die Secrets.
- **Das Timing ist ausgeglichen.** Ein unbekanntes Präfix führt trotzdem eine
  Dummy-Verifizierung durch, sodass ein fehlender Schlüssel etwa gleich lang
  braucht wie ein echter — keine Aufzählung über das Antwort-Timing.
- **Beschränken Sie Schlüssel auf einen Benutzer, setzen Sie einen Ablauf und
  rotieren Sie.** Stellen Sie pro Integration einen aus, damit Sie einen
  widerrufen können, ohne die anderen zu stören; bevorzugen Sie kurze
  `expires_at`-Fenster für temporären Zugriff.
- **Abgrenzung von JWTs:** Das Backend behandelt einen Bearer-Wert nur dann als
  API-Schlüssel, wenn sein erstes Punkt-Segment genau 8 Zeichen hat — so können
  sich API-Schlüssel und [JWTs](auth-jwt.md) den
  `Authorization: Bearer`-Header teilen.

---

## Siehe auch

- [Auth-Backends](auth-backends.md) — die Kette, in die sich `ApiKeyBackend`
  einklinkt, sowie der `CurrentUser`-Extraktor + die
  `require_auth`/`require_perm`-Middleware.
- [HMAC-Anfragesignierung](auth-hmac.md) — für Maschinen-Aufrufer, die
  Integrität pro Anfrage benötigen, nicht nur ein Bearer-Credential.
- [Passwörter](auth-passwords.md) — dieselbe „hashen, nicht speichern“-Disziplin
  für menschliche Anmeldungen.
- [JWT](auth-jwt.md) — kurzlebige zustandslose Tokens, die andere
  Maschinen-Option.
