# E-Mails versenden

Willkommensnachrichten, Passwort-Resets, Belege, Alerts — die meisten Apps versenden
transaktionale E-Mails. **Rustango** gibt dir ein `Mailer`-Trait mit austauschbaren
Backends (Konsole für die Entwicklung, SMTP für die Produktion, ein In-Memory-Recorder für Tests),
einen flüssigen `Email`-Builder mit Schutz vor Header-Injection und
Template-Rendering. Schreibe `mailer.send(&email)` einmal; wechsle vom Drucken
in dein Terminal zu echtem SMTP mit einer einzeiligen Änderung — wie Djangos E-Mail-Framework.

[![E-Mail in Rustango: Ein Email-Builder (to/subject/body/html) wird gegen Header-Injection validiert und dann durch das Mailer-Trait versendet — ConsoleMailer in dev, SmtpMailer in prod, InMemoryMailer in Tests](img/email.png)](img/email.png)

> **Neu bei einem Begriff hier?** *transaktionale E-Mail*, *SMTP*, *Mailer-Backend* — siehe
> das [Glossar](glossary.md).

> **Quelle:** `rustango::email` (`Mailer`, `Email`, `ConsoleMailer`,
> `InMemoryMailer`, `NullMailer`, `SmtpMailer`, `BoxedMailer`, `send_mail`,
> `MailError`) — hinter dem `email`-Feature (standardmäßig aktiviert). SMTP benötigt das
> `email-smtp`-Feature.
>
> **Lauffähige Version:** Jedes Snippet ist kopiert aus
> [`email_doc.rs`](../crates/rustango/tests/email_doc.rs)
> (`cargo test -p rustango --test email_doc`); die Send-Helfer und Anhänge
> werden von `email_send_helpers.rs` und `email_attachments.rs` selbst erprobt.

## Inhaltsverzeichnis

- [Schritt 1 — Eine E-Mail bauen](#step-1--build-an-email)
- [Schritt 2 — Einen Mailer wählen](#step-2--pick-a-mailer)
- [Schritt 3 — Sie versenden](#step-3--send-it)
- [Validierung und Schutz vor Header-Injection](#validation-and-header-injection-safety)
- [E-Mails testen](#testing-email)
- [Templates](#templates)
- [Sie außerhalb des Requests versenden](#send-it-off-the-request)
- [Referenz](#reference)
- [Siehe auch](#see-also)

---

## Schritt 1 — Eine E-Mail bauen

`Email` ist ein flüssiger Builder. Setze Empfänger, Betreff und einen Text- und/oder HTML-Body:

```rust
use rustango::email::Email;

let email = Email::new()
    .to("ada@example.com")
    .from("noreply@example.com")
    .subject("Welcome")
    .body("Thanks for signing up.")              // plain-text part
    .html_body("<p>Thanks for signing up.</p>"); // optional HTML part
```

`.cc(...)`, `.reply_to(...)` und Anhänge sind ebenfalls verfügbar.

---

## Schritt 2 — Einen Mailer wählen

Jedes Backend implementiert `Mailer`, sodass dein Code den konkreten Typ nie benennt —
halte ein **`BoxedMailer`** (`Arc<dyn Mailer>`):

| Backend | Feature | Verwenden für |
|---|---|---|
| `ConsoleMailer` | `email` | dev — druckt die Nachricht auf stdout |
| `SmtpMailer` | `email-smtp` | Produktion — echte Zustellung über SMTP |
| `InMemoryMailer` | `email` | Tests — zeichnet Nachrichten auf, sendet nichts |
| `FileMailer` | `email` | dev/CI — schreibt jede Nachricht in eine Datei |
| `NullMailer` | `email` | E-Mail vollständig deaktivieren |

Baue ihn aus der Konfiguration, damit er sich je Umgebung unterscheidet (`ConsoleMailer`
lokal, `SmtpMailer` in prod) über `email::from_settings(&settings.email)`.

---

## Schritt 3 — Sie versenden

`Email::send` nimmt jedes `&dyn Mailer`:

```rust
email.send(&mailer).await?;
```

Für einen schnellen Einzelfall überspringt `send_mail` den Builder:

```rust
use rustango::email::send_mail;

send_mail(
    &mailer,
    "Your report is ready",                  // subject
    "Download it from your dashboard.",      // body
    Some("noreply@example.com"),             // from (or None for the default)
    &["ops@example.com", "qa@example.com"],  // recipients
).await?;
```

`send_many` versendet einen Batch in einem Aufruf.

---

## Validierung und Schutz vor Header-Injection

`Email::validate()` läuft vor dem Versand (und du kannst es selbst aufrufen). Es
lehnt unvollständige Nachrichten ab **und** verteidigt gegen Header-Injection — ein in einen
Header eingeschmuggelter Zeilenumbruch ist die Art, wie Angreifer ein verstecktes `Bcc` hinzufügen:

```rust
// Missing recipients or an empty subject → MailError::InvalidMessage
Email::new().subject("hi").validate()?;          // Err: no recipients

// A CRLF in any header field → MailError::BadHeader (Django's BadHeaderError)
Email::new()
    .to("a@example.com")
    .subject("Hello\r\nBcc: victim@example.com")  // injection attempt
    .body("x")
    .validate()?;                                  // Err: BadHeader
```

Beide werden im zugrunde liegenden Test verifiziert.

---

## E-Mails testen

Verwende `InMemoryMailer` — es zeichnet jede Nachricht auf, statt zu senden, sodass Tests
darauf prüfen, was *hinausgegangen wäre*, ohne Netzwerk:

```rust
use rustango::email::InMemoryMailer;

let mailer = InMemoryMailer::new();
welcome_flow(&mailer).await?;          // your code under test

let sent = mailer.sent();              // Vec<Email>
assert_eq!(sent.len(), 1);
assert_eq!(sent[0].to, vec!["ada@example.com".to_string()]);
assert_eq!(sent[0].subject, "Welcome");
```

---

## Templates

Für alles jenseits einer Textzeile rendere den Body aus einem [Tera](html-views.md)-Template,
statt HTML inline zu setzen. Der `EmailRenderer` des `email_templates`-Features
folgt einer `name.subject.txt` / `name.txt` / `name.html`-Konvention — ein Template-Satz
produziert den Betreff, den Klartextteil und den HTML-Teil zusammen, sodass die
drei nie auseinanderdriften. Das `Mailable`-Trait verpackt „ein Ding, das weiß, wie es sich
selbst in eine `Email` verwandelt" für wiederverwendbare Nachrichten.

---

## Sie außerhalb des Requests versenden

E-Mail inline zu versenden lässt den Nutzer auf deinen SMTP-Server warten und koppelt die
Antwort an dessen Verfügbarkeit. Versende sie stattdessen aus einem [Hintergrundjob](jobs.md) —
der Handler kehrt sofort zurück und ein Worker stellt sie zu (mit Retries, falls SMTP
ausgefallen ist):

```rust
// in the handler: enqueue, don't send inline
queue.dispatch(&SendWelcomeEmail { user_id }).await?;

// the job (see the Background jobs guide):
async fn run(&self) -> Result<(), JobError> {
    let email = Email::new().to(/* ... */).subject("Welcome").body("...");
    email.send(&*mailer).await.map_err(|e| JobError::Retryable(e.to_string()))?;
    Ok(())
}
```

Das `email_jobs`-Feature verdrahtet dies für dich.

---

## Referenz

**`Email`-Builder:** `to` · `cc` · `from` · `reply_to` · `subject` · `body` ·
`html_body` · Anhänge · `validate()` · `send(&mailer)`.

**Helfer:** `send_mail(mailer, subject, body, from, &recipients)` ·
`send_many(mailer, &emails)` · `from_settings(&EmailSettings)`.

**`MailError`:** `InvalidMessage` (unvollständig) · `BadHeader` (CRLF-Injection) ·
`Transport` (Backend-/Zustellungsfehler).

---

## Siehe auch

- [Hintergrundjobs](jobs.md) — E-Mail außerhalb des Requests mit Retries zustellen.
- [Konto-Abläufe](auth-flows.md) — Passwort-Reset- / Verifizierungs- / Magic-Link-E-Mails,
  die darauf aufbauen.
- [HTML-Views](html-views.md) — die Tera-Engine, die auch E-Mail-Templates verwenden.
- [Caching](caching.md) — dasselbe Muster „Backend-austauschen" per Trait.
