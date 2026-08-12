# Konto-Abläufe (Zurücksetzen, Verifizieren, Magic Link)

Die Abläufe, die jede Anwendung am Rande der Anmeldung braucht: **Passwort-Zurücksetzung**,
**E-Mail-Verifizierung** und **Magic-Link-Anmeldung (passwortlos)**. Alle drei haben dieselbe
Form — dem Benutzer einen manipulationssicheren, zeitlich begrenzten Link per E-Mail schicken und
dann handeln, wenn er darauf klickt — und **Rustango** baut sie auf einem einzigen Fundament auf:
**signierten URLs**. Eine signierte URL ist eine normale URL mit angehängter HMAC-Signatur, sodass
der Server ihren Parametern vertrauen kann, ohne irgendetwas zu speichern.

[![Konto-Abläufe in Rustango: signed_url::sign hängt eine HMAC-Signatur + Ablauf an; die drei Abläufe (Passwort-Zurücksetzung, E-Mail-Verifizierung, Magic Link) stellen einen Link aus, versenden ihn per E-Mail und verifizieren ihn beim Klick](img/auth-flows.png)](img/auth-flows.png)

> **Ein Begriff hier neu für Sie?** *HMAC*, *Token*, *Ablauf* — siehe das [Glossar](glossary.md).

> **Quelle:** `rustango::signed_url` (`sign`, `verify`, `SignedUrlError`) und
> `rustango::auth_flows` (`PasswordReset`, `EmailVerification`, `MagicLink`,
> `confirm_password_reset_pool_into`) — hinter den Features `signed_url` / `auth_flows`
> (standardmäßig aktiv; die Reset-Bestätigung braucht zusätzlich `passwords` + ein DB-Backend).
>
> **Ausführbare Version:** jeder Ausschnitt ist aus
> [`auth_flows_doc.rs`](../crates/rustango/tests/auth_flows_doc.rs) kopiert
> (`cargo test -p rustango --features sqlite --test auth_flows_doc`).

## Inhaltsverzeichnis

- [Signierte URLs: das Fundament](#signed-urls-the-substrate)
- [Passwort-Zurücksetzung](#password-reset)
- [E-Mail-Verifizierung](#email-verification)
- [Magic-Link-Anmeldung](#magic-link-login)
- [Einmal-Tokens](#single-use-tokens)
- [Was Sie bereitstellen](#what-you-provide)
- [Siehe auch](#see-also)

---

## Signierte URLs: das Fundament

`sign` hängt eine HMAC-SHA256-Signatur (und einen optionalen Ablauf) über den Pfad + die Query der
URL an. `verify` berechnet sie neu: Manipulieren Sie irgendeinen Parameter, verwenden Sie das
falsche Secret oder lassen Sie sie ablaufen, und sie schlägt fehl.

```rust
use rustango::signed_url::{sign, verify, SignedUrlError};

let url = "https://app.example.com/files/42?user_id=7";
let signed = sign(url, secret, None);     // None = never expires
assert!(verify(&signed, secret).is_ok());

// Flip any signed byte → InvalidSignature.
let tampered = signed.replace("user_id=7", "user_id=8");
assert_eq!(verify(&tampered, secret), Err(SignedUrlError::InvalidSignature));
```

Fügen Sie eine TTL hinzu, und ein abgelaufener Link wird abgewiesen (`sign_at` / `verify_at`
nehmen explizite Unix-Sekunden für deterministische Tests):

```rust
use rustango::signed_url::{sign_at, verify_at, SignedUrlError};

let signed = sign_at(url, secret, Some(100));         // expires at t=100
assert!(verify_at(&signed, secret, 50).is_ok());      // before → ok
assert_eq!(verify_at(&signed, secret, 1000), Err(SignedUrlError::Expired));
```

Die Query wird vor dem Signieren sortiert, die Reihenfolge der Parameter spielt also keine Rolle.
Die Fehler sind `MissingSignature`, `MalformedSignature`, `InvalidSignature`, `Expired`.

---

## Passwort-Zurücksetzung

Die `auth_flows`-Helfer umhüllen signierte URLs mit einer **Zweck-Markierung** (damit ein
Reset-Token nicht als Magic Link wiederverwendet werden kann) und kodieren die Benutzer-ID.
`PasswordReset` liefert außerdem einen Bestätigungshelfer, der das Token verifiziert und den
**gespeicherten Hash rotiert** — in einem einzigen Aufruf.

```rust
use std::time::Duration;
use rustango::auth_flows::{PasswordReset, confirm_password_reset_pool_into};

// 1. User asks to reset → look them up → issue a link → email it.
let url = PasswordReset::issue(
    "https://app.example.com/auth/reset",   // your callback route
    user_id,                                // encoded in the token
    secret,
    Duration::from_secs(3600),              // 1-hour TTL
);
mailer.send(&Email::new().to(addr).subject("Reset your password").body(&url)).await?;

// 2. User clicks + submits a new password → verify + rotate the hash.
let user_id = confirm_password_reset_pool_into(
    &pool, &url, "a-brand-new-strong-password", secret,
    "rustango_users", "id", "password_hash",  // table, pk col, password col
).await?;
```

Der Bestätigungshelfer erzwingt eine Mindestlänge, hasht das neue Passwort mit argon2id und
schreibt es — wobei er schwache, abgelaufene, manipulierte oder mit falschem Secret versehene
Eingaben abweist, ohne die Zeile anzurühren:

```rust
// valid token + strong pw → hash rotated (starts "$argon2…")
// "short"                  → Err(WeakPassword), nothing written
// user_id tampered         → Err(InvalidSignature), nothing written
```

> `confirm_password_reset_pool` ist die bequeme Form, die die Standardwerte
> `rustango_users` / `id` / `password_hash` annimmt; verwenden Sie `_into`, um auf Ihre eigene
> Tabelle/Spalten zu verweisen.

---

## E-Mail-Verifizierung

`EmailVerification` kodiert sowohl die Benutzer-ID **als auch** die E-Mail, sodass Sie bei der
Verifizierung beide zurückerhalten und bestätigen können, dass die Adresse immer noch übereinstimmt
(um Links abzufangen, die vor einer E-Mail-Änderung versendet wurden). Hier gibt es keinen
integrierten DB-Schreibvorgang — Sie setzen Ihre eigene Spalte „verifiziert“:

```rust
use rustango::auth_flows::EmailVerification;

// On signup:
let url = EmailVerification::issue(callback, user_id, &email, secret, Duration::from_secs(86_400));
mailer.send(&Email::new().to(&email).subject("Confirm your email").body(&url)).await?;

// On click:
let (user_id, email) = EmailVerification::verify(&url, secret)?;
// → if email still matches the user's current address, mark them verified
```

---

## Magic-Link-Anmeldung

`MagicLink` kodiert nur die E-Mail — der Benutzer klickt, Sie schlagen das Konto nach und prägen
eine [Session](auth-sessions.md). Halten Sie die TTL kurz (10–30 Min) und machen Sie ihn
**einmalig nutzbar** (nächster Abschnitt), denn der Link *ist* die Zugangsberechtigung:

```rust
use rustango::auth_flows::MagicLink;

let url = MagicLink::issue(callback, &email, secret, Duration::from_secs(900));
mailer.send(&Email::new().to(&email).subject("Your sign-in link").body(&url)).await?;

// On click:
let email = MagicLink::verify_single_use(&url, secret, &cache).await?;
// → look up the user by email, create a session
```

---

## Einmal-Tokens

`verify` allein prüft nur Signatur + Ablauf, ein durchgesickerter Link ist also bis zum Ablauf
wiederholbar. Für Anmeldung und Zurücksetzung bevorzugen Sie `verify_single_use(url, secret,
&cache)` — es hält die Signatur des Tokens in einem `Cache` fest und verweigert eine zweite
Nutzung:

```rust
// first click  → Ok(email)
// same link reused → Err(AuthFlowError::AlreadyUsed)
```

Untermauern Sie es in der Produktion mit einem **gemeinsam genutzten** Cache (Redis), damit ein
Token nicht gegen ein anderes Replikat wiederholt werden kann. Die Prüfung schlägt geschlossen fehl
(ein Cache-Fehler verweigert, statt einen Wiederholungsangriff zu riskieren).

---

## Was Sie bereitstellen

Das Framework stellt Tokens aus/verifiziert sie und schreibt (beim Zurücksetzen) den Hash; Ihre
Anwendung liefert den Rest:

- Ein **Secret** (ein stabiler App-Schlüssel; per Konvention 32 Byte).
- Ein **Mailer** zum Versenden der Links — `rustango::email` liefert `ConsoleMailer`,
  `SmtpMailer` und `InMemoryMailer` (praktisch in Tests).
- Eine **Benutzertabelle** mit den Spalten, die jeder Ablauf braucht (E-Mail für die
  Verifizierungs-/Magic-Link-Suche; eine Passwort-Hash-Spalte für die Zurücksetzung; eine Spalte
  „verifiziert“, die Ihnen gehört).
- Die **Callback-Routen**, die den Klick empfangen, und die Session-Prägung für die
  Magic-Link-Anmeldung.

---

## Siehe auch

- [Passwörter](auth-passwords.md) — das Hashing, das die Zurücksetzung rotiert.
- [Sessions](auth-sessions.md) — was die Magic-Link-Anmeldung bei Erfolg erzeugt.
- [HMAC-Request-Signierung](auth-hmac.md) — dieselbe HMAC-Primitive, angewandt auf API-Requests
  statt auf URLs.
- [Sicherheitsleitfaden](security.md) — die umfassendere Härtungs-Checkliste.
