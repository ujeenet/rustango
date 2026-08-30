# Sessions

Eine Session hält einen Benutzer über Anfragen hinweg angemeldet, indem sie dem
Browser eine **opake ID** in einem Cookie übergibt und alles andere serverseitig
behält. Der `SessionStore` von **Rustango** legt diesen Zustand in einen Cache
(Redis in Produktion, im Speicher für Tests), sodass das Cookie keine Geheimnisse
trägt und eine Session **sofort widerrufen** werden kann — löschen Sie den
Eintrag, und jede Replik sieht das Abmelden bei der nächsten Anfrage.

[![Sessions in Rustango: das Cookie enthält nur eine opake ID, der SessionStore hält die Daten in Redis, und destroy() widerruft sie überall](../img/auth-sessions.png)](../img/auth-sessions.png)

> **Quelle:** `rustango::sessions` (`Session`, `SessionStore`) +
> `rustango::cache` (`BoxedCache`, `InMemoryCache`) — hinter dem Feature
> `sessions` (standardmäßig aktiviert; zieht `cache` nach). Für einen
> Redis-gestützten Speicher in Produktion fügen Sie das Feature `cache-redis`
> hinzu (standardmäßig deaktiviert), um `RedisCache` zu erhalten.
>
> **Ausführbare Version:** die Ausschnitte unten sind aus dem getesteten Beispiel
> [`auth_demo`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/auth_demo/tests/auth_sessions.rs)
> kopiert — `cargo test -p auth_demo --test auth_sessions`.

> **Ein Begriff hier neu?** *Session*, *opake ID*, *Cookie*, *Cache* — siehe das
> [Glossar](glossary.md).

> Vertiefungsbegleiter zum [Sicherheitsleitfaden](security.md). Das Absichern von
> Routen hinter einer angemeldeten Session wird in
> [Auth-Dekoratoren](auth-decorators.md) behandelt; für zustandslose API-Tokens
> stattdessen siehe [JWT](auth-jwt.md).

---

## Inhaltsverzeichnis
- [Schnellstart](#quick-start) · [Sessions vs. JWT](#sessions-vs-jwt)
- [Die Session-Tasche](#the-session-bag) · [Das Cookie](#the-cookie)
- [Ein Backend wählen](#picking-a-backend) · [Ablauf und gleitende Erneuerung](#expiry-and-sliding-renewal)
- [An Ort und Stelle aktualisieren](#updating-a-session-in-place) · [Hinweise und Grenzen](#notes-and-limits)

---

## Schnellstart

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

Die ID besteht aus 192 Bit OS-CSPRNG-Zufälligkeit, base64url-kodiert auf 32
Zeichen — weit über der 128-Bit-Untergrenze für Session-Tokens und nicht
erratbar.

---

## Sessions vs. JWT

Beide beantworten „wer ist diese Anfrage?", mit gegensätzlichen Kompromissen:

| | Session | [JWT](auth-jwt.md) |
|---|---|---|
| Zustand | serverseitig (Cache-Abfrage pro Anfrage) | zustandslos (selbstenthaltenes Token) |
| Widerruf | **sofort** — den Eintrag `destroy()` | schwierig — gültig bis zum Ablauf (benötigt eine Sperrliste) |
| Am besten für | Browser-Anwendungen, „diesen Benutzer JETZT abmelden" | APIs, Service-zu-Service, kein gemeinsamer Speicher |

Greifen Sie zu Sessions, wenn Sie jemanden zwangsweise abmelden müssen
(Passwortänderung, „von allen Geräten abmelden", ein gesperrtes Konto). Greifen
Sie zu JWT, wenn Sie null Abfragen pro Anfrage wünschen und keinen gemeinsamen
Cache haben.

---

## Die Session-Tasche

`Session` ist eine typisierte Schlüssel→Wert-Tasche mit einem Dirty-Bit (sodass
der Speicher einen Schreibvorgang überspringen kann, wenn sich nichts geändert
hat):

```rust
let mut s = Session::new();
s.set("user_id", 42_i64);            // serialize any Serialize value
s.set("role", "editor");
let uid: Option<i64> = s.get("user_id");   // None if absent or wrong type
s.remove("role");
s.clear();                            // wipe everything (e.g. on logout)
```

`get` ist **fehlertolerant**: ein fehlender Schlüssel *oder* ein Wert, der sich
nicht in den angeforderten Typ deserialisieren lässt, gibt `None` zurück, statt
zu paniken — sodass eine Schemaänderung nie eine Anfrage mit 500 quittiert.

---

## Das Cookie

Das Cookie enthält nur `sid`. Setzen Sie es mit den Sicherheitsattributen, die
ein Session-Cookie benötigt:

- **`HttpOnly`** — JavaScript kann es nicht lesen (stumpft Token-Diebstahl per
  XSS ab).
- **`SameSite=Lax`** — wird bei seitenübergreifenden Unteranfragen nicht gesendet
  (CSRF-Abwehr; kombinieren Sie es mit [CSRF-Tokens](security.md#protecting-against-csrf)
  für Formular-Posts).
- **`Secure`** — nur HTTPS (nur für lokale HTTP-Entwicklung weglassen).
- **`Path=/`** — für die gesamte Anwendung sichtbar.

Nichts Sensibles steckt im Cookie, sodass ein durchgesickertes Cookie genau so
mächtig ist wie die Session, auf die es verweist — und Sie können diese jederzeit
serverseitig widerrufen.

---

## Ein Backend wählen

`SessionStore::new` nimmt jeden `BoxedCache`:

- **`RedisCache`** — Produktion. Über Repliken hinweg geteilt, sodass eine
  Anmeldung auf einer Instanz und eine Abmeldung auf einer anderen beide überall
  sichtbar sind.
- **`InMemoryCache`** — Einzelprozess / Tests. Schnell, keine Abhängigkeiten,
  aber Sessions überleben keinen Neustart und werden nicht zwischen Repliken
  geteilt.

```rust
use rustango::cache::{BoxedCache, InMemoryCache};
use std::sync::Arc;

// Tests / single-process:
let store = SessionStore::new(Arc::new(InMemoryCache::new()) as BoxedCache);
```

---

## Ablauf und gleitende Erneuerung

Sessions haben standardmäßig eine TTL von **2 Wochen**. Überschreiben Sie sie pro
Speicher und rufen Sie bei jeder authentifizierten Anfrage `touch` auf, um eine
gleitende Ablaufzeit zu erhalten (aktive Benutzer bleiben angemeldet, untätige
laufen aus):

```rust
use std::time::Duration;

let store = SessionStore::new(cache).ttl(Duration::from_secs(60 * 60)); // 1 hour

// On each request, after a successful load — extend without rewriting:
store.touch(&sid).await?;   // Ok(false) if the session is already gone
```

---

## Eine Session an Ort und Stelle aktualisieren

`save` prägt stets eine frische ID (verwenden Sie es bei der Anmeldung). Um eine
bestehende Session während einer Anfrage zu ändern, laden → mutieren →
`save_with_id` unter derselben ID:

```rust
let mut s = store.load(&sid).await?.unwrap_or_default();
s.set("last_seen", chrono::Utc::now().to_rfc3339());
store.save_with_id(&sid, &s).await?;
```

---

## Hinweise und Grenzen

- **Der Widerruf ist das Hauptmerkmal** — `destroy()` (Abmeldung) und der
  TTL-Ablauf treten beide bei der nächsten Anfrage in Kraft, auf jeder Replik,
  die sich den Cache teilt.
- **Beschädigte oder unbekannte IDs laden als `None`** (*fail-open*): eine
  Cache-Schemaänderung oder ein manipuliertes Cookie ergibt eine leere Session,
  keinen Fehler — die Anfrage ist einfach nicht authentifiziert.
- **Der Speicher setzt das Cookie nicht für Sie** — er verwaltet den
  serverseitigen Zustand; Sie hängen das `sid`-Cookie in Ihrem Handler an bzw.
  lesen es (oder über einen Layer). Das macht ihn aus jeder Framework-Verdrahtung
  heraus nutzbar.
- **Prägen Sie bei einer Rechteänderung eine frische Session-ID** (z. B. direkt
  nach der Anmeldung), um Session-Fixierung zu vermeiden — `save` tut dies
  bereits, da es stets eine neue ID erzeugt.


---

## Siehe auch

- [Auth-Dekoratoren](auth-decorators.md)
- [JWT](auth-jwt.md)
- [Auth-Backends](auth-backends.md)
- [Sicherheitsleitfaden](security.md)
