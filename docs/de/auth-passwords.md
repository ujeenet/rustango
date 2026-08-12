# Passwörter

Ein Passwort zu speichern bedeutet, etwas zu speichern, das ein Angreifer selbst
mit Ihrer gesamten Datenbank in der Hand nicht umkehren kann. **Rustango** liefert
Ihnen das in zwei Aufrufen — `hash` beim Eingang, `verify` beim Ausgang —
gestützt auf **argon2id**, den *memory-hard* Gewinner der Password Hashing
Competition und die aktuelle Erstwahl der OWASP. Sie speichern, protokollieren
oder vergleichen den Klartext niemals.

[![Passwörter in Rustango: hash() erzeugt eine gesalzene argon2id-PHC-Zeichenkette, verify() prüft einen Versuch dagegen, und verify_dummy() gleicht die Anmelde-Timing an](img/auth-passwords.png)](img/auth-passwords.png)

> **Quelle:** `rustango::passwords` (`hash`, `verify`, `verify_dummy`,
> `strength_score`, `StrengthIssue`) — hinter dem Feature `passwords`
> (standardmäßig aktiviert). Für die in die Mandantenfähigkeit integrierten
> Passwort-Hilfsfunktionen siehe `rustango::tenancy::password`.
>
> **Ausführbare Version:** jeder Ausschnitt unten ist aus dem getesteten Beispiel
> [`auth_demo`](../crates/rustango/examples/auth_demo/tests/auth_passwords.rs)
> kopiert — `cargo test -p auth_demo --test auth_passwords`.

> **Ein Begriff hier neu?** *hash*, *salt*, *argon2id*, *PHC-Zeichenkette* — siehe
> das [Glossar](glossary.md).

> Dies ist die Vertiefung zum Abschnitt „Passwörter hashen und prüfen" des
> [Sicherheitsleitfadens](security.md).

---

## Inhaltsverzeichnis
- [Schnellstart](#quick-start) · [Warum argon2id](#why-argon2id)
- [Hashen bei der Registrierung](#hashing-on-signup) · [Prüfen bei der Anmeldung](#verifying-on-login)
- [Timing-sichere Anmeldungen](#timing-safe-logins-account-enumeration) · [Stärkeprüfungen](#strength-checks)
- [Wo der Hash lebt](#where-the-hash-lives) · [Hinweise und Grenzen](#notes-and-limits)

---

## Schnellstart

```rust
use rustango::passwords::{hash, verify};

// Signup — store the returned PHC string, never the plaintext.
let stored: String = hash("CorrectHorseBatteryStaple!42")?;

// Login — check an attempt against the stored hash.
if verify("CorrectHorseBatteryStaple!42", &stored)? {
    // credentials good
}
```

`hash` gibt eine [PHC-Zeichenkette](https://github.com/P-H-C/phc-string-format)
zurück — eine selbstbeschreibende Zeile, die den Algorithmus, seine
Kostenparameter, das zufällige Salt und den Digest trägt:

```text
$argon2id$v=19$m=19456,t=2,p=1$<base64 salt>$<base64 hash>
```

Da Salt und Parameter *innerhalb* der Zeichenkette mitreisen, benötigt `verify`
nur den gespeicherten Wert und den Versuch — es gibt keine separate Salt-Spalte
zu verwalten.

---

## Warum argon2id

`hash` verwendet **argon2id** mit den von der OWASP empfohlenen Standardwerten
(m=19 MiB, t=2, p=1). argon2id ist *memory-hard*: jeder Rateversuch kostet echten
RAM, und genau das stumpft die GPU/ASIC-Farmen ab, die schnelle Hashes (MD5,
SHA-256, sogar bcrypt bei niedrigen Kosten) per Brute-Force angreifbar machen.
Zwei Eigenschaften sind für die Korrektheit wichtig:

- **Das Salting ist automatisch und pro Hash.** Dasselbe Passwort zweimal zu
  hashen ergibt zwei verschiedene PHC-Zeichenketten, sodass identische Passwörter
  in Ihrer Tabelle nicht kollidieren und Angriffe mit vorberechneten
  Rainbow-Tables nicht greifen.

  ```rust
  let a = hash("same-password-12345")?;
  let b = hash("same-password-12345")?;
  assert_ne!(a, b);                 // different random salt each time
  assert!(verify("same-password-12345", &a)?);
  assert!(verify("same-password-12345", &b)?);
  ```

- **Die Prüfung erfolgt in konstanter Zeit** beim Digest-Vergleich (argon2s
  eigener `PasswordVerifier`), sodass ein Byte-für-Byte-Timing-Leak nicht
  verraten kann, wie viel eines Versuchs richtig war.

---

## Hashen bei der Registrierung

```rust
use rustango::passwords::{hash, strength_score};

fn create_user(username: &str, plaintext: &str) -> Result<String, String> {
    // Optional: nudge users away from weak choices (see below).
    let issues = strength_score(plaintext);
    if !issues.is_empty() {
        return Err(format!("password too weak: {issues:?}"));
    }
    // Store the PHC string on the user row (e.g. auth_users.password_hash).
    hash(plaintext).map_err(|e| e.to_string())
}
```

---

## Prüfen bei der Anmeldung

```rust
use rustango::passwords::verify;

// `stored` is the PHC string you saved at signup.
let ok = verify(attempt, &stored)?;
```

`verify` gibt zurück:
- `Ok(true)` — der Versuch stimmt überein.
- `Ok(false)` — er stimmt nicht.
- `Err(PasswordError::Verify)` — `stored` war keine gültige PHC-Zeichenkette
  (eine beschädigte oder abgeschnittene Spalte), behandeln Sie es also als
  fehlgeschlagene Anmeldung, nicht als 500.

---

## Timing-sichere Anmeldungen (Kontoaufzählung)

Wenn Ihre Anmeldung das teure `verify` **nur** ausführt, wenn der Benutzername
existiert, kehrt ein unbekannter Benutzername merklich schneller zurück als ein
echter — und diese Zeitlücke lässt einen Angreifer gültige Konten aufzählen.
`verify_dummy` schließt sie: rufen Sie es im Zweig Benutzer-nicht-gefunden (und
Konto-inaktiv) auf, damit jede Anmeldung unabhängig davon die Arbeit eines
argon2-verify aufwendet.

```rust
use rustango::passwords::{verify, verify_dummy};

let row = users::find_by_username(username).await?;
let authenticated = match row {
    Some(u) if u.is_active => verify(attempt, &u.password_hash)?,
    _ => {
        verify_dummy(attempt); // burn the same work, then fail
        false
    }
};
```

---

## Stärkeprüfungen

`strength_score` gibt einen `Vec<StrengthIssue>` zurück — leer bedeutet „gut
genug". Es ist eine bewusst leichtgewichtige Heuristik, um Benutzer zu
*ermutigen*, kein hartes Richtlinien-Gatter; kombinieren Sie sie für ernsthafte
Deployments mit einer Prüfung gegen eine Leak-Liste (HIBP / pwned-passwords).

```rust
use rustango::passwords::{strength_score, StrengthIssue};

assert!(strength_score("Tr0ub4dor&3-CorrectBattery").is_empty());
assert!(strength_score("password123").contains(&StrengthIssue::KnownWeak));
assert!(strength_score("short").contains(&StrengthIssue::TooShort));
```

| `StrengthIssue` | Ausgelöst, wenn |
|---|---|
| `TooShort` | weniger als 12 Zeichen |
| `NoDigitsOrSymbols` | nur Buchstaben — keine Ziffer oder kein Symbol |
| `NoVariety` | nur Kleinbuchstaben |
| `KnownWeak` | stimmt mit der kleinen eingebauten Liste schwacher Passwörter überein (ohne Berücksichtigung der Groß-/Kleinschreibung) |

---

## Wo der Hash lebt

Die PHC-Zeichenkette ist lediglich eine `String`-Spalte auf dem beliebigen
Kontomodell, das Ihnen gehört. Im Beispiel
[`auth_demo`](../crates/rustango/examples/auth_demo/src/models.rs):

```rust
#[derive(Model, Clone, Debug)]
#[rustango(table = "auth_users", display = "username")]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 150, unique)]
    pub username: String,
    #[rustango(max_length = 254)]
    pub email: String,
    #[rustango(max_length = 255)]      // PHC strings are ~95 chars at these params
    pub password_hash: String,
    pub is_active: bool,
    pub is_superuser: bool,
}
```

Sobald der Benutzer authentifiziert ist, übergeben Sie an eine
[Session](auth-sessions.md) (für Browser-Anwendungen) oder stellen Sie ein
[JWT](auth-jwt.md) aus (für APIs).

---

## Hinweise und Grenzen

- **Niemals** den Klartext speichern, protokollieren oder mit `==` vergleichen.
  `hash` → speichern; `verify` → prüfen. Das ist der ganze Vertrag.
- **Die Kostenparameter sind die OWASP-Standardwerte**, fest eingebaut. Sie sind
  eine vernünftige Untergrenze; sie später anzuheben ist sicher — alte Hashes
  verifizieren weiterhin (ihre Parameter leben in der PHC-Zeichenkette), und Sie
  können bei der nächsten erfolgreichen Anmeldung neu hashen, um sie zu
  aktualisieren.
- `strength_score` ist eine Heuristik, keine Richtlinien-Engine — es wird
  `Summer2024!` nicht erkennen. Legen Sie für echte Stärkedurchsetzung eine
  Leak-Listen-Abfrage darüber.
- Für mandantenfähige Anwendungen mit dem Benutzerspeicher des Frameworks
  bevorzugen Sie `rustango::tenancy::password` (dasselbe argon2id, integriert in
  das Benutzermodell des Mandanten). Dieses Modul ist die eigenständige Version
  für Anwendungen, die ihre eigene User-Tabelle besitzen.


---

## Siehe auch

- [Sessions](auth-sessions.md)
- [Konto-Abläufe](auth-flows.md)
- [Auth-Backends](auth-backends.md)
- [Sicherheitsleitfaden](security.md)
