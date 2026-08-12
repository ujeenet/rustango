# Envoyer des e-mails

Messages de bienvenue, réinitialisations de mot de passe, reçus, alertes — la plupart des
applications envoient des e-mails transactionnels. **Rustango** vous offre un trait `Mailer` avec
des backends interchangeables (console pour le développement, SMTP pour la production, un
enregistreur en mémoire pour les tests), un builder `Email` fluide avec protection contre
l'injection d'en-têtes, et le rendu de templates. Écrivez `mailer.send(&email)` une seule fois ;
passez de l'impression dans votre terminal à un vrai SMTP avec un changement d'une ligne — comme le
framework d'e-mail de Django.

[![L'e-mail dans Rustango : un builder Email (to/subject/body/html) est validé contre l'injection d'en-têtes, puis envoyé à travers le trait Mailer — ConsoleMailer en dev, SmtpMailer en prod, InMemoryMailer en test](img/email.png)](img/email.png)

> **Un terme vous est inconnu ?** *e-mail transactionnel*, *SMTP*, *backend de messagerie* — voir
> le [glossaire](glossary.md).

> **Source :** `rustango::email` (`Mailer`, `Email`, `ConsoleMailer`,
> `InMemoryMailer`, `NullMailer`, `SmtpMailer`, `BoxedMailer`, `send_mail`,
> `MailError`) — derrière la fonctionnalité `email` (activée par défaut). SMTP nécessite la
> fonctionnalité `email-smtp`.
>
> **Version exécutable :** chaque snippet est copié depuis
> [`email_doc.rs`](../crates/rustango/tests/email_doc.rs)
> (`cargo test -p rustango --test email_doc`) ; les helpers d'envoi et les pièces jointes
> sont mis à l'épreuve par `email_send_helpers.rs` et `email_attachments.rs`.

## Table des matières

- [Étape 1 — Construire un e-mail](#step-1--build-an-email)
- [Étape 2 — Choisir un mailer](#step-2--pick-a-mailer)
- [Étape 3 — L'envoyer](#step-3--send-it)
- [Validation et protection contre l'injection d'en-têtes](#validation-and-header-injection-safety)
- [Tester les e-mails](#testing-email)
- [Templates](#templates)
- [L'envoyer en dehors de la requête](#send-it-off-the-request)
- [Référence](#reference)
- [Voir aussi](#see-also)

---

## Étape 1 — Construire un e-mail

`Email` est un builder fluide. Définissez les destinataires, l'objet, et un corps texte et/ou
HTML :

```rust
use rustango::email::Email;

let email = Email::new()
    .to("ada@example.com")
    .from("noreply@example.com")
    .subject("Welcome")
    .body("Thanks for signing up.")              // plain-text part
    .html_body("<p>Thanks for signing up.</p>"); // optional HTML part
```

`.cc(...)`, `.reply_to(...)`, et les pièces jointes sont également disponibles.

---

## Étape 2 — Choisir un mailer

Chaque backend implémente `Mailer`, si bien que votre code ne nomme jamais le type concret —
tenez un **`BoxedMailer`** (`Arc<dyn Mailer>`) :

| Backend | Fonctionnalité | À utiliser pour |
|---|---|---|
| `ConsoleMailer` | `email` | dev — imprime le message sur stdout |
| `SmtpMailer` | `email-smtp` | production — livraison réelle par SMTP |
| `InMemoryMailer` | `email` | tests — enregistre les messages, n'envoie rien |
| `FileMailer` | `email` | dev/CI — écrit chaque message dans un fichier |
| `NullMailer` | `email` | désactive complètement l'e-mail |

Construisez-le à partir de la configuration afin qu'il diffère selon l'environnement (`ConsoleMailer`
en local, `SmtpMailer` en prod) via `email::from_settings(&settings.email)`.

---

## Étape 3 — L'envoyer

`Email::send` prend n'importe quel `&dyn Mailer` :

```rust
email.send(&mailer).await?;
```

Pour un envoi ponctuel rapide, `send_mail` évite le builder :

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

`send_many` envoie un lot en un seul appel.

---

## Validation et protection contre l'injection d'en-têtes

`Email::validate()` s'exécute avant l'envoi (et vous pouvez l'appeler vous-même). Elle
rejette les messages incomplets **et** défend contre l'injection d'en-têtes — un saut de ligne
introduit subrepticement dans un en-tête est la façon dont les attaquants ajoutent un `Bcc` caché :

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

Les deux sont vérifiés dans le test de support.

---

## Tester les e-mails

Utilisez `InMemoryMailer` — il enregistre chaque message au lieu d'envoyer, si bien que les tests
affirment sur ce qui *serait* parti, sans réseau :

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

Pour tout ce qui va au-delà d'une ligne de texte, rendez le corps à partir d'un template
[Tera](html-views.md) plutôt que d'inliner du HTML. L'`EmailRenderer` de la fonctionnalité
`email_templates` suit une convention `name.subject.txt` / `name.txt` / `name.html` — un seul
ensemble de templates produit l'objet, la partie texte brut et la partie HTML ensemble, si bien que
les trois ne divergent jamais. Le trait `Mailable` empaquette « une chose qui sait se transformer
elle-même en `Email` » pour des messages réutilisables.

---

## L'envoyer en dehors de la requête

Envoyer l'e-mail en ligne fait attendre l'utilisateur après votre serveur SMTP et couple la
réponse à sa disponibilité. Envoyez-le plutôt depuis une [tâche d'arrière-plan](jobs.md) —
le handler renvoie immédiatement et un worker le livre (avec des relances si le SMTP est
indisponible) :

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

La fonctionnalité `email_jobs` câble cela pour vous.

---

## Référence

**Builder `Email` :** `to` · `cc` · `from` · `reply_to` · `subject` · `body` ·
`html_body` · pièces jointes · `validate()` · `send(&mailer)`.

**Helpers :** `send_mail(mailer, subject, body, from, &recipients)` ·
`send_many(mailer, &emails)` · `from_settings(&EmailSettings)`.

**`MailError` :** `InvalidMessage` (incomplet) · `BadHeader` (injection CRLF) ·
`Transport` (échec de backend/livraison).

---

## Voir aussi

- [Tâches d'arrière-plan](jobs.md) — livrer l'e-mail en dehors de la requête avec des relances.
- [Flux de compte](auth-flows.md) — e-mails de réinitialisation de mot de passe / vérification /
  lien magique construits là-dessus.
- [Vues HTML](html-views.md) — le moteur Tera qu'utilisent aussi les templates d'e-mail.
- [Mise en cache](caching.md) — le même motif de trait « change-le-backend ».
