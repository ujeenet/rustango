# JWT (eigenständig)

Ein JSON Web Token ist ein **zustandsloses** Credential: eine signierte,
in sich geschlossene Zeichenkette, die der Client bei jeder Anfrage sendet und
die Ihr Server mit einem Secret verifiziert — ohne Datenbank- oder
Cache-Abfrage pro Anfrage. Das Modul `rustango::jwt` von **Rustango** ist der
minimale Baustein: `encode` zum Signieren von Claims, `decode` zum Verifizieren
und Zurücklesen, HS256 unter der Haube.

[![Eigenständiges JWT in Rustango: Claims tragen sub-/exp-/benutzerdefinierte Felder, encode() signiert mit einem gemeinsamen Secret, decode() verifiziert Signatur + Ablauf](../img/auth-jwt.png)](../img/auth-jwt.png)

> **Quelle:** `rustango::jwt` (`Claims`, `encode`, `decode`, `decode_at`,
> `decode_unverified`, `JwtError`) — hinter dem Feature `jwt` (standardmäßig
> aktiviert). Für eine schlüsselfertige Access+Refresh-**API** mit Widerruf siehe
> [JWT-Auth-API](auth-jwt-api.md).
>
> **Lauffähige Version:** Die Snippets sind aus dem getesteten
> [`auth_demo`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/auth_demo/tests/auth_jwt.rs) kopiert —
> `cargo test -p auth_demo --test auth_jwt`.

> **Ein Begriff hier neu für Sie?** *JWT*, *Claims*, *zustandslos*, *Secret* —
> siehe das [Glossar](glossary.md).

> Vertiefende Ergänzung zum Abschnitt „JWTs ausstellen und erneuern“ des
> [Sicherheitsleitfadens](security.md).

---

## Inhaltsverzeichnis
- [Schnellstart](#quick-start) · [Wann einsetzen](#when-to-use-standalone-jwt)
- [Claims erstellen](#building-claims) · [Verifizieren](#verifying-a-token)
- [Sicherheitsmodell](#security-model) — unbedingt lesen · [Inspizieren ohne Vertrauen](#inspecting-without-verifying)
- [Hinweise und Grenzen](#notes-and-limits)

---

## Schnellstart

```rust
use rustango::jwt::{Claims, encode, decode};
use std::time::Duration;

// HS256 ist symmetrisch — dasselbe Secret signiert und verifiziert. Muss >= 32 Bytes sein.
let secret = b"a-shared-signing-secret-at-least-32-bytes!!";

let mut claims = Claims::new("user-42").ttl(Duration::from_secs(900));
claims.set("roles", vec!["editor", "author"]);

let token = encode(&claims, secret)?;        // header.payload.signature

let verified = decode(&token, secret)?;       // prüft Signatur + exp/nbf
assert_eq!(verified.subject(), Some("user-42"));
let roles: Vec<String> = verified.get("roles").unwrap();
```

---

## Wann ein eigenständiges JWT einsetzen

Greifen Sie zu `rustango::jwt`, wenn Sie einen schlichten signierten Token wollen
und den Lebenszyklus selbst verwalten:

- **Magic-Link- / Einmal-Tokens** — wenige Claims (Benutzer-ID, Zweck, kurzes
  `exp`).
  Siehe [Magic Links & Auth-Abläufe](auth-flows.md).
- **Service-zu-Service**-Bearer-Tokens (das JWT-Gegenstück zur [HMAC-
  Anfragesignierung](auth-hmac.md) — HMAC für kanonische Anfragen im AWS-Stil,
  JWT für einen zustandslosen Bearer).
- **SSO-Tokens**, die Sie an einen Dritten übergeben.

Wenn Sie eine schlüsselfertige **login → access + refresh → refresh →
logout**-API mit Token-Widerruf wollen, bauen Sie sie nicht darauf auf —
verwenden Sie die [JWT-Auth-API](auth-jwt-api.md), die dieses Modul mit Rotation +
einem Widerrufsspeicher umhüllt. Und wenn Sie einen Benutzer *jetzt* zwangsweise
abmelden müssen, bevorzugen Sie eine widerrufbare [Session](auth-sessions.md):
Ein einfaches JWT ist gültig, bis es abläuft.

---

## Claims erstellen

`Claims` umhüllt ein JSON-Objekt, sodass Standard-Claims und Ihre eigenen
Erweiterungsfelder koexistieren:

```rust
let mut claims = Claims::new("user-42")     // setzt `sub` + `iat=now`
    .ttl(Duration::from_secs(3600))         // setzt `iat`=now und `exp`=now+ttl
    .issuer("api.example.com")              // `iss`
    .audience("web-client")                 // `aud`
    .jti("unique-token-id");                // `jti` (für Ihre eigene Sperrliste)
claims.set("role", "admin");                // beliebiger Serialize-Wert
claims.set("org_id", 7_i64);
```

| Builder / Setter | Claim |
|---|---|
| `Claims::new(sub)` | `sub` + `iat` |
| `Claims::empty()` | keine (volle Kontrolle) |
| `.ttl(Duration)` | `iat` (now) + `exp` (now+ttl) |
| `.expires_at(secs)` / `.not_before(secs)` | absolutes `exp` / `nbf` |
| `.issuer(s)` / `.audience(s)` / `.jti(s)` | `iss` / `aud` / `jti` |
| `.set(name, value)` | beliebiger benutzerdefinierter Claim |

Lesen Sie sie mit `.subject()` und `.get::<T>(name)` zurück (liefert `None` für
einen fehlenden oder falsch typisierten Claim).

---

## Einen Token verifizieren

```rust
use rustango::jwt::{decode, JwtError};

match decode(&token, secret) {
    Ok(claims) => { /* claims.subject() usw. vertrauen */ }
    Err(JwtError::Expired(_))      => { /* 401 — Token abgelaufen */ }
    Err(JwtError::BadSignature)    => { /* 401 — gefälscht oder falscher Schlüssel */ }
    Err(JwtError::NotYetValid(_))  => { /* nbf in der Zukunft */ }
    Err(_)                         => { /* fehlerhaft / nicht unterstützter alg */ }
}
```

`decode` verifiziert die **Signatur**, dann `exp` und `nbf`. Um das Verhalten des
Zeitfensters zu testen (oder eine Skew-Toleranz hinzuzufügen), lässt
`decode_at(token, secret, now)` Sie die „aktuelle“ Sekunde festlegen:

```rust
let token = encode(&Claims::new("x").expires_at(1000), secret)?;
assert!(decode_at(&token, secret, 500).is_ok());                     // vor exp
assert!(matches!(decode_at(&token, secret, 2000), Err(JwtError::Expired(_)))); // danach
```

---

## Sicherheitsmodell

Dies ist Code an der Auth-Grenze — drei Dinge, die Sie wissen müssen:

1. **`decode` validiert `iss` / `aud` NICHT.** Eine gültige Signatur beweist,
   dass der Token mit Ihrem Secret erzeugt wurde, nicht dass er *für Ihren
   Dienst* erzeugt wurde. Wenn Sie `iss`/`aud` bei der Ausstellung setzen,
   **prüfen Sie sie selbst** an den decodierten Claims:

   ```rust
   let c = decode(&token, secret)?;
   if c.get::<String>("aud").as_deref() != Some("web-client") {
       return Err("wrong audience");
   }
   ```

2. **Das Secret muss ≥ 32 Bytes sein** — `encode` weigert sich, mit einem
   kürzeren Schlüssel zu signieren (ein kurzer Schlüssel ist erratbar, und ein
   erratbarer HMAC-Schlüssel bedeutet fälschbare Tokens). HS256 ist symmetrisch:
   Wer das Verifizierungs-Secret besitzt, kann auch Tokens *erzeugen*, es bleibt
   also innerhalb Ihrer Vertrauensgrenze (einzelner Dienst / gemeinsames
   Backend). Die organisationsübergreifende Token-Ausstellung verlangt
   asymmetrisches RS256/ES256, das dieses Modul bewusst nicht mitbringt.

3. **`alg=none` und Manipulation werden abgelehnt.** `decode` fixiert HS256 (die
   klassische „alg: none“-Fälschung wird verweigert), und jede Änderung an
   Header oder Payload bricht die Signatur — verifiziert durch einen Vergleich
   in konstanter Zeit.

Es gibt **keinen Spielraum für Uhren-Skew**: `exp`/`nbf` vergleichen mit der
exakten aktuellen Sekunde. Wenn die Uhren von Aussteller und Verifizierer
auseinanderdriften, ziehen Sie ein paar Sekunden über `decode_at` ab.

---

## Inspizieren ohne Verifizieren

`decode_unverified` liest das Payload, **ohne** Signatur oder Ablauf zu prüfen —
nützlich nur, um einen Blick auf einen Claim zu werfen (z. B. eine Schlüssel-ID),
damit Sie das richtige Secret wählen können, und rufen Sie dann `decode` echt auf.

```rust
let peek = rustango::jwt::decode_unverified(&token)?;   // NICHT vertrauenswürdig
let kid = peek.get::<String>("kid");
// ... das Secret für `kid` nachschlagen, dann korrekt verifizieren:
let claims = decode(&token, &resolved_secret)?;
```

**Autorisieren Sie niemals auf Basis der Ausgabe von `decode_unverified`** — sie
trägt keine Integritätsgarantie.

---

## Hinweise und Grenzen

- **Nur HS256** — symmetrisch, ein einziges gemeinsames Secret. Kein RS256/ES256
  (hält den stets aktiven Abhängigkeitsbaum klein; die meisten
  Einzeldienst-Apps verwenden ohnehin HS256).
- **Zustandslos = nicht widerrufbar.** Ein einfaches JWT ist gültig bis `exp`.
  Wenn Sie „jetzt abmelden“ / Widerruf pro Token benötigen, verwenden Sie die
  [JWT-Auth-API](auth-jwt-api.md) (JTI-Sperrliste) oder eine
  [Session](auth-sessions.md) (löschen Sie den Servereintrag).
- **Halten Sie `exp` kurz** für Access-Tokens (Minuten). Langlebige einfache
  JWTs sind gerade deshalb ein Risiko, weil sie nicht widerrufen werden können.
- Verbinden Sie die Ausstellung mit [Passwörtern](auth-passwords.md)
  (verifizieren, dann ausstellen) und schützen Sie API-Routen über das
  `JwtBackend` der [Auth-Backend-Kette](auth-backends.md).


---

## Siehe auch

- [JWT-Auth-API](auth-jwt-api.md)
- [Auth-Backends](auth-backends.md)
- [API-Schlüssel](auth-api-keys.md)
- [Sessions](auth-sessions.md)
