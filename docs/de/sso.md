# SSO (OpenID Connect / Social Login)

Melden Sie sich mit einem externen Identitätsanbieter an — Google, Microsoft / Azure
AD, GitHub, GitLab, Discord oder **jedem beliebigen OpenID-Connect-Anbieter** (Okta,
Auth0, Keycloak, …) — statt mit einem lokalen Passwort.

Sie können **mehrere Anbieter** konfigurieren, jeder als Zeile über die Admin-UI
verwaltet (keine Konfigurationsdatei, kein Neubau). Die Endpunkte eines Anbieters
werden bei der Anmeldung automatisch aus seiner OIDC-Issuer-URL ermittelt;
Social-Anbieter verwenden eingebaute Presets.

SSO ist für den Admin **Verknüpfung mit einem bestehenden Konto**: die verifizierte
E-Mail, die der IdP zurückgibt, muss mit einem bestehenden Admin-Benutzer
übereinstimmen. Es authentifiziert die Person; es erstellt niemals Konten und
gewährt niemals von sich aus Zugriff. Eine unbekannte oder unverifizierte E-Mail
wird abgewiesen. (Der Member-Ablauf, weiter unten, kann sich für
Auto-Provisionierung entscheiden.)

> **Quelle:** der admin-unabhängige Kern `rustango::sso` (`SsoProvider`,
> `build_provider`, `verified_email`, `ResolvedSso`, `SsoError`), die
> Bare-Admin-Verdrahtung `rustango::admin::sso`, das SSO pro Tenant/Konsole
> `rustango::tenancy::sso` (`SharedSsoProvider`) und das Member-SSO
> `rustango::tenancy::member_auth`.

## Features & wer sie nutzt

Seit **0.49** ist der SSO-Kern ein eigenes Feature, unabhängig vom
Auto-Admin, sodass eine Endbenutzer-Anmeldung (Member) gebaut werden kann, ohne
`crate::admin` hereinzuziehen:

| Feature | Zieht herein | Gibt Ihnen |
|---|---|---|
| `sso` | `oauth2`, `casts` | Den admin-unabhängigen Kern: `rustango::sso` — den OIDC- / Social-OAuth-Handshake, das DB-gestützte `SsoProvider`-Modell (Secret im Ruhezustand über `casts` verschlüsselt) und den Member-Ablauf (`tenancy::member_auth`, mit `tenancy`). |
| `admin-sso` | `admin`, `sso` | Das Obige **plus** die Bare-Admin-Login-Verdrahtung (`rustango::admin::sso`) — SSO-Buttons auf der Admin-Anmeldeseite, die die Admin-Session prägen. |

```toml
[dependencies]
# Admin login with SSO:
rustango = { version = "0.52", features = ["admin-sso"] }
# Member (end-user) SSO without the auto-admin:
rustango = { version = "0.52", features = ["tenancy", "sso"] }
```

`admin::sso_provider` und die historischen `admin::sso::*`-Kernpfade sind
nun **Re-Export-Shims** über `sso::provider` / `sso::*`, sodass bestehende
Imports von `crate::admin::sso::{build_provider, ResolvedSso, …}` und
`crate::admin::sso_provider::SsoProvider` unverändert weiter aufgelöst
werden (der Tabellenname `rustango_sso_providers` und jedes Feld bleiben
unangetastet — Migrationen sind nicht betroffen).

Die E-Mail, über die ein Benutzer verknüpft wird, ist die `email`-Spalte. Beim
Tenant-`User`-Modell hängt sie am Feature **`sso`** (in 0.49 von `admin-sso`
weggezogen, sodass reine Member-SSO-Builds die Spalte trotzdem erhalten); das
nackte `AdminUser.email` bleibt hinter `admin-sso`. Das Aktivieren oder
Deaktivieren des Features gibt eine `AddColumn` / `DropColumn`-Migration für
diese Spalte aus.

## Wie es funktioniert

1. Die Anmeldeseite zeigt einen **„Mit &lt;Anbieter&gt; anmelden“**-Button pro
   aktiviertem Anbieter.
2. Ein Klick darauf (`GET <login>/sso/<slug>`) leitet zum IdP weiter, mit einem
   signierten, kurzlebigen Flow-Cookie (PKCE + CSRF-`state`).
3. Der IdP schickt den Benutzer zurück an `<login>/sso/<slug>/callback`.
4. rustango verifiziert den Flow, tauscht den Code ein, liest `/userinfo`
   und verlangt **`email_verified`**.
5. Es sucht einen Admin-Benutzer über diese E-Mail. Existiert einer und ist aktiv,
   prägt es **dieselbe signierte Cookie-Session**, die eine Passwort-Anmeldung
   erzeugt, gebunden an diesen Benutzer — sodass jedes bestehende Gate (Superuser /
   Berechtigungen, Live-Passwortänderungs-Invalidierung) weiterhin gilt.
6. Keine Übereinstimmung → der Benutzer wird mit einem generischen Fehler zur
   Anmeldeseite zurückgeschickt (Details gehen ins Server-Log, nie in den Browser).

Das Client-**Secret ist im Ruhezustand verschlüsselt** — die `client_secret`-Spalte
ist ein [`EncryptedString`](#secret-storage)-Cast, erst zur Anmeldezeit im Speicher
entschlüsselt.

## Anbieter sind Zeilen, verwaltet im Admin

Jeder Anbieter ist eine `SsoProvider`-Zeile. Sie erscheint als gewöhnliches
Admin-Modell — hinzufügen/bearbeiten/aktivieren über die Admin-UI, kein Redeploy.
Felder:

| Feld | Bedeutung |
|---|---|
| `slug` | Stabiler Routen-Schlüssel + Button-ID (`<login>/sso/<slug>`). Eindeutig. |
| `label` | Button-Text, z. B. „Mit Google anmelden“. |
| `kind` | Ein Preset — `google` / `microsoft` / `github` / `gitlab` / `discord` — oder `oidc` für einen generischen OpenID-Connect-Anbieter. |
| `issuer_url` | OIDC-Discovery-Basis-URL (für `kind = "oidc"`); rustango holt `{issuer}/.well-known/openid-configuration`. Bei Presets ungenutzt. |
| `client_id` | Die OAuth-Client-ID vom IdP. |
| `client_secret` | Das OAuth-Client-Secret, **im Ruhezustand verschlüsselt** (nie im Klartext in der DB). |
| `enabled` | Ob der Button auf der Anmeldeseite erscheint. |
| `sort_order` | Button-Reihenfolge (aufsteigend). |
| `scopes` | Optionale, durch Leerzeichen getrennte Scope-Überschreibung (Standard `openid email profile`). |

Um einen Anbieter hinzuzufügen: geben Sie `client_id` + `client_secret` ein, wählen
Sie ein `kind` (oder `oidc` + eine `issuer_url`) und speichern Sie. Die Endpunkte
werden bei der Anmeldung ermittelt — keine Endpunkt-Verdrahtung pro Anbieter.

## Wo jede Oberfläche Anbieter verwaltet

- **Single-Tenant / Standalone-Admin** (`crate::admin`): `SsoProvider`-Zeilen
  sind eine schlichte globale Tabelle, verwaltet vom nackten Admin. Erfordert
  `Builder::with_session_auth` (SSO prägt dieselbe Session).
- **Tenant-Admin** (Multi-Tenancy): jeder Tenant verwaltet seine **eigenen**
  `SsoProvider`-Zeilen aus seinem Admin — granular, self-service, pro Tenant
  isoliert.
- **Operator-Konsole** (Multi-Tenancy): ein Operator definiert einmal einen
  **`SharedSsoProvider`**, und er wird **jedem** Tenant angeboten
  (etwa ein unternehmensweites Google). Verwaltet über das *Shared SSO*-Panel
  der Konsole.

Auf der Anmeldeseite eines Tenants verschmelzen die beiden Mengen, und bei einer
Slug-Kollision **gewinnt der eigene Anbieter des Tenants** gegenüber dem geteilten
— ein Tenant kann also einen geteilten Anbieter für sich selbst überschreiben.

Die Callback-URL wird pro Anfrage aus Host + Slug abgeleitet
(`https://<host><login>/sso/<slug>/callback`), registrieren Sie diese also beim
IdP. Verknüpfen Sie einen Benutzer, indem Sie die `email`-Spalte in seiner
`rustango_users`- (Tenant) / `rustango_admin_users`- (bare) Zeile auf die vom IdP
zurückgegebene Adresse setzen.

## Member (Endbenutzer) SSO

Die obigen Oberflächen melden Personen bei einem **Admin** an.
`tenancy::member_auth` ist das member-seitige Gegenstück: es meldet einen
Endbenutzer in den eigenen Benutzerpool eines Tenants an (`rustango_users`) und
prägt eine **Member-Session**, sodass ein Fitnessstudio-Mitglied / SaaS-Kunde
„Mit Google anmelden“ kann, ohne den Admin zu berühren. Es verwendet exakt
denselben `rustango::sso`-Kern und die eigenen `SsoProvider`-Zeilen des Tenants —
nur die Session, die es prägt, unterscheidet sich, weshalb es hinter dem
`sso`-Feature (nicht `admin-sso`) lebt und keinen Auto-Admin braucht.

Hängen Sie `member_sso_router` in einen `tenancy::server::Builder`-Stack ein (es
liest den aufgelösten `Arc<TenantContext>`, den der Builder injiziert):

```rust
use rustango::tenancy::member_auth::{member_sso_router, MemberAuthConfig};

let members = member_sso_router(MemberAuthConfig {
    login_base:     "/auth".into(),   // buttons link to /auth/sso/<slug>
    landing_url:    "/".into(),       // post-login destination (honors a same-origin ?next)
    auto_provision: true,             // create a user from a verified email on first sign-in
    session_ttl:    7 * 24 * 60 * 60, // 7 days
    ..Default::default()
});
```

Es hängt zwei Routen pro Slug an `login_base` an:

- `GET {login_base}/sso/{slug}` — den Handshake beginnen, zum IdP weiterleiten.
- `GET {login_base}/sso/{slug}/callback` — ihn abschließen, den Member
  finden-oder-provisionieren, das Session-Cookie prägen.

Unterschiede zum Admin-Ablauf:

- **Auto-Provisionierung.** Mit `auto_provision = true` (dem Standard) **erstellt**
  eine verifizierte IdP-E-Mail ohne passende `rustango_users`-Zeile eine solche
  — Benutzername aus dem lokalen Teil der E-Mail (bei Kollision entdupliziert),
  ein echter, aber unbrauchbarer zufälliger Passwort-Hash (SSO-Benutzer können
  sich nicht per Passwort anmelden). Setzen Sie es auf `false` für die
  admin-artige Verknüpfung-mit-Bestehendem (unbekannte E-Mail abgewiesen).
- **Ein eigenes Session-Cookie.** Das Member-Cookie
  (`rustango_member_session`) ist von den Tenant- / Admin-Session-Cookies
  **domänengetrennt**: die signierte Nachricht trägt einen Tag pro Domäne und
  einen Audience-Claim, sodass ein Member-Cookie niemals als Tenant-/Admin-Cookie
  validieren kann (oder umgekehrt), obwohl beide mit
  `RUSTANGO_SESSION_SECRET` signiert sind. Es ist slug-gebunden (ein für `acme`
  geprägtes Cookie authentifiziert niemals auf `globex`) und wird durch eine
  Passwort-Rotation invalidiert (analog zur Admin-Session).

Lesen Sie den aktuellen Member in einem Handler mit dem **`CurrentMember`**-
Extraktor — dem Member-Gegenstück zu `SessionUser`. Er ist unfehlbar
(`None` für anonyme / abgelaufene / rotierte / tenant-fremde Sessions),
sodass er sich mit öffentlichen Routen komponieren lässt:

```rust
use rustango::tenancy::member_auth::CurrentMember;

async fn dashboard(CurrentMember(member): CurrentMember) -> impl axum::response::IntoResponse {
    match member {
        Some(user) => format!("Hi, {}", user.username),
        None => "Please sign in".to_owned(),
    }
}
```

> **v1-Umfang.** Member-SSO löst Anbieter nur aus den eigenen
> `SsoProvider`-Zeilen des Tenants auf — der registry-weite
> `SharedSsoProvider`-Merge und ein eigener `provision`-Hook sind Folgeaufgaben.

## Secret-Speicherung

`client_secret` wird **im Ruhezustand verschlüsselt** mit XChaCha20-Poly1305
(AEAD) gespeichert, der Schlüssel abgeleitet aus der Umgebungsvariable
**`RUSTANGO_SECRET_KEY`**. Es wird nur zur Anmeldezeit im Speicher entschlüsselt,
um sich am Token-Endpunkt des IdP zu authentifizieren. So legt ein durchgesickerter
DB-Dump das Secret nie offen, und jeder Tenant behält sein eigenes Secret ohne
Umgebungsvariable pro Anbieter.

> Setzen Sie `RUSTANGO_SECRET_KEY` im Deployment (beliebige Länge; es wird per
> SHA-256 zu einem 32-Byte-Schlüssel gehasht). Ohne sie schlägt das Speichern oder
> Verwenden eines Anbieters schnell fehl — dieselbe Haltung wie bei einer
> fehlenden Datenbank-URL.

## Anbieter (Presets)

Eingebaute Presets: `google`, `microsoft` (Azure AD), `github`, `gitlab`,
`discord`. Für alles andere verwenden Sie `kind = "oidc"` mit einer `issuer_url` —
rustango führt OpenID-Connect-Discovery aus, um die Endpunkte zu finden. (Sign in
with Apple ist kein Preset; es benötigt id_token/JWKS-Verifizierung.)

## Sicherheitshinweise

- **Nur verifizierte E-Mail** — unverifizierte IdP-E-Mails werden abgewiesen.
- **Keine Auto-Provisionierung** — eine unbekannte E-Mail kommt nicht hinein;
  erstellen Sie den Admin-Benutzer (und setzen Sie seine `email`) zuerst.
- **Secrets im Ruhezustand verschlüsselt** (`RUSTANGO_SECRET_KEY`), nur zur
  Anmeldezeit im Speicher entschlüsselt; Bearbeitungsformulare maskieren das
  gespeicherte Secret.
- Das Flow-Cookie ist kurzlebig (10 Min), `HttpOnly`, `SameSite=Lax`
  und `Secure` über HTTPS; der Handshake trägt PKCE + ein signiertes `state`.
- SSO-Sessions sind die gewöhnliche Admin-Session — das Rotieren oder
  Deaktivieren des verknüpften Benutzers invalidiert sie über das bestehende
  Live-Gate.
- Das Vertrauensmodell ist `/userinfo` über TLS (das id_token wird nicht
  unabhängig verifiziert); stellen Sie dem Admin HTTPS voran.

## Siehe auch

- [Sicherheitsleitfaden](security.md) · [Authentifizierung](auth-flows.md)
