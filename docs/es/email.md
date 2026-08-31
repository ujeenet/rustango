# Enviar correo electrónico

Mensajes de bienvenida, restablecimientos de contraseña, recibos, alertas — la mayoría de las
aplicaciones envían correo transaccional. **Rustango** te ofrece un trait `Mailer` con
backends intercambiables (consola para desarrollo, SMTP para producción, un grabador en memoria
para pruebas), un builder `Email` fluido con protección contra inyección de cabeceras, y el
renderizado de plantillas. Escribe `mailer.send(&email)` una vez; cambia de imprimir
en tu terminal a SMTP real con un cambio de una línea — como el framework de correo de Django.

[![Correo en Rustango: un builder Email (to/subject/body/html) se valida contra la inyección de cabeceras, luego se envía a través del trait Mailer — ConsoleMailer en dev, SmtpMailer en prod, InMemoryMailer en pruebas](../img/email.png)](../img/email.png)

> **¿Nuevo con algún término aquí?** *correo transaccional*, *SMTP*, *backend de correo* — ver
> el [glosario](glossary.md).

> **Fuente:** `rustango::email` (`Mailer`, `Email`, `ConsoleMailer`,
> `InMemoryMailer`, `NullMailer`, `SmtpMailer`, `BoxedMailer`, `send_mail`,
> `MailError`) — tras la característica `email` (activada por defecto). SMTP necesita la
> característica `email-smtp`.
>
> **Versión ejecutable:** cada snippet se copia de
> [`email_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/email_doc.rs)
> (`cargo test -p rustango --test email_doc`); los ayudantes de envío y los adjuntos
> se someten a prueba mediante `email_send_helpers.rs` y `email_attachments.rs`.

## Tabla de contenidos

- [Paso 1 — Construye un correo](#step-1--build-an-email)
- [Paso 2 — Elige un mailer](#step-2--pick-a-mailer)
- [Paso 3 — Envíalo](#step-3--send-it)
- [Validación y protección contra inyección de cabeceras](#validation-and-header-injection-safety)
- [Probar el correo](#testing-email)
- [Plantillas](#templates)
- [Envíalo fuera de la petición](#send-it-off-the-request)
- [Referencia](#reference)
- [Véase también](#see-also)

---

## Paso 1 — Construye un correo

`Email` es un builder fluido. Establece los destinatarios, el asunto, y un cuerpo de texto y/o
HTML:

```rust
use rustango::email::Email;

let email = Email::new()
    .to("ada@example.com")
    .from("noreply@example.com")
    .subject("Welcome")
    .body("Thanks for signing up.")              // plain-text part
    .html_body("<p>Thanks for signing up.</p>"); // optional HTML part
```

`.cc(...)`, `.reply_to(...)`, y los adjuntos también están disponibles.

---

## Paso 2 — Elige un mailer

Cada backend implementa `Mailer`, de modo que tu código nunca nombra el tipo concreto —
sostén un **`BoxedMailer`** (`Arc<dyn Mailer>`):

| Backend | Característica | Úsalo para |
|---|---|---|
| `ConsoleMailer` | `email` | dev — imprime el mensaje en stdout |
| `SmtpMailer` | `email-smtp` | producción — entrega real por SMTP |
| `InMemoryMailer` | `email` | pruebas — graba los mensajes, no envía nada |
| `FileMailer` | `email` | dev/CI — escribe cada mensaje en un archivo |
| `NullMailer` | `email` | deshabilitar el correo por completo |

Constrúyelo a partir de la configuración para que difiera por entorno (`ConsoleMailer`
en local, `SmtpMailer` en prod) mediante `email::from_settings(&settings.email)`.

---

## Paso 3 — Envíalo

`Email::send` toma cualquier `&dyn Mailer`:

```rust
email.send(&mailer).await?;
```

Para un envío rápido y puntual, `send_mail` se salta el builder:

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

`send_many` envía un lote en una sola llamada.

---

## Validación y protección contra inyección de cabeceras

`Email::validate()` se ejecuta antes de enviar (y puedes llamarlo tú mismo). Rechaza
mensajes incompletos **y** defiende contra la inyección de cabeceras — un salto de línea
introducido a escondidas en una cabecera es la forma en que los atacantes añaden un `Bcc` oculto:

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

Ambos se verifican en el test de respaldo.

---

## Probar el correo

Usa `InMemoryMailer` — graba cada mensaje en lugar de enviarlo, de modo que las pruebas
afirman sobre lo que *se habría* enviado, sin red:

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

## Plantillas

Para cualquier cosa más allá de una línea de texto, renderiza el cuerpo a partir de una plantilla
[Tera](html-views.md) en lugar de incrustar HTML. El `EmailRenderer` de la característica
`email_templates` sigue una convención `name.subject.txt` / `name.txt` / `name.html` — un solo
conjunto de plantillas produce el asunto, la parte de texto plano y la parte HTML juntos, de modo
que las tres nunca divergen. El trait `Mailable` empaqueta «algo que sabe convertirse
a sí mismo en un `Email`» para mensajes reutilizables.

---

## Envíalo fuera de la petición

Enviar el correo en línea hace que el usuario espere a tu servidor SMTP y acopla la
respuesta a su disponibilidad. Envíalo en su lugar desde un [trabajo en segundo plano](jobs.md) —
el handler retorna de inmediato y un worker lo entrega (con reintentos si el SMTP está
caído):

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

La característica `email_jobs` cablea esto por ti.

---

## Referencia

**Builder `Email`:** `to` · `cc` · `from` · `reply_to` · `subject` · `body` ·
`html_body` · adjuntos · `validate()` · `send(&mailer)`.

**Ayudantes:** `send_mail(mailer, subject, body, from, &recipients)` ·
`send_many(mailer, &emails)` · `from_settings(&EmailSettings)`.

**`MailError`:** `InvalidMessage` (incompleto) · `BadHeader` (inyección CRLF) ·
`Transport` (fallo de backend/entrega).

---

## Véase también

- [Trabajos en segundo plano](jobs.md) — entregar el correo fuera de la petición con reintentos.
- [Flujos de cuenta](auth-flows.md) — correos de restablecimiento de contraseña / verificación /
  enlace mágico construidos sobre esto.
- [Vistas HTML](html-views.md) — el motor Tera que también usan las plantillas de correo.
- [Caché](caching.md) — el mismo patrón de trait «cambia-el-backend».
