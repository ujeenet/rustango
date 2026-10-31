# SSO (OpenID Connect / Social Login)

Melden Sie sich mit einem externen Identitätsanbieter an — Google, Microsoft / Azure
AD, GitHub, GitLab, Discord oder **jedem beliebigen OpenID-Connect-Anbieter** (Okta,
Auth0, Keycloak, …) — statt mit einem lokalen Passwort.

Sie können **mehrere Anbieter** konfigurieren, jeder als Zeile über die Admin-UI
verwaltet (keine Konfigurationsdatei, kein Neubau). Die Endpunkte eines Anbieters
werden bei der Anmeldung automatisch aus seiner OIDC-Issuer-URL ermittelt;
Social-Anbieter verwenden eingebaute Presets.

SSO meldet den Benutzer an, der mit der IdP-Identität **verknüpft** ist: eine
`rustango_sso_links`-Zeile pro `(provider, sub)`. Die E-Mail, die der IdP sendet,
reicht allein nie aus. Ein Erstbenutzer wird nur dann über seine verifizierte
E-Mail verknüpft, wenn beim Anbieter **`allow_email_link`** eingeschaltet ist
(Standard: aus), und nie, wenn das Konto ein Superuser oder Staff ist (Tenant:
besitzt irgendeine Berechtigung; Bare-Admin: jedes Konto). Solche Konten
verknüpft ein Admin, der die `SsoLink`-Zeile im Admin anlegt; die Log-Zeile der
Abweisung nennt das Subject. (Der Member-Ablauf, weiter unten, kann für eine
unbekannte E-Mail auch ein Konto anlegen; das ist standardmäßig aus.)

> **Quelle:** der admin-unabhängige Kern `rustango::sso` (`SsoProvider`,
> `build_provider`, `verified_email`, `ResolvedSso`, `SsoError`), die
> Link-Tabelle `rustango::sso::link` (`SsoLink`, `ProviderKey`, `sign_in`), die
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
rustango = { version = "0.61", features = ["admin-sso"] }
# Member (end-user) SSO without the auto-admin:
rustango = { version = "0.61", features = ["tenancy", "sso"] }
```

`admin::sso_provider` und die historischen `admin::sso::*`-Kernpfade sind
nun **Re-Export-Shims** über `sso::provider` / `sso::*`, sodass bestehende
Imports von `crate::admin::sso::{build_provider, ResolvedSso, …}` und
`crate::admin::sso_provider::SsoProvider` unverändert weiter aufgelöst
werden. Seit 0.58 hat `rustango_sso_providers` eine Spalte `allow_email_link`,
und es gibt eine Tabelle `rustango_sso_links`; `makemigrations` erzeugt beides.

Die E-Mail, die das optionale Verknüpfen per E-Mail abgleicht, ist die `email`-Spalte. Beim
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
4. rustango verifiziert den Flow, tauscht den Code ein und liest `/userinfo`.
5. Es sucht die Verknüpfung für diesen Anbieter und das IdP-`sub`. Gibt es keine
   und ist `allow_email_link` eingeschaltet, legt eine **verifizierte** E-Mail,
   die zu einem nicht privilegierten Benutzer passt, die Verknüpfung an.
6. Ist der verknüpfte Benutzer aktiv, prägt es **dieselbe signierte
   Cookie-Session**, die eine Passwort-Anmeldung erzeugt — sodass jedes
   bestehende Gate (Superuser / Berechtigungen, Live-Passwortänderungs-
   Invalidierung) weiterhin gilt.
7. Andernfalls wird der Benutzer mit einem generischen Fehler zur Anmeldeseite
   zurückgeschickt (Details gehen ins Server-Log, nie in den Browser).

Die Link-Tabelle ist ein normales migriertes Modell: im Speicher des Tenants für
Tenant-Anmeldungen und in der Admin-Datenbank für den Bare-Admin. Eine
Verknüpfung wird exakt abgeglichen (Issuer und Subject über einen SHA-256
verschlüsselt; bei der E-Mail wird nur die ASCII-Groß-/Kleinschreibung
ignoriert), unabhängig von der Kollation der Datenbank.

Das Client-**Secret ist im Ruhezustand verschlüsselt** — die `client_secret`-Spalte
ist ein [`EncryptedString`](#secret-speicherung)-Cast, erst zur Anmeldezeit im Speicher
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
| `allow_email_link` | Einen Erstbenutzer über seine verifizierte E-Mail verknüpfen (Standard: aus). Verknüpft nie ein Superuser- oder Staff-Konto; vom Bare-Admin ignoriert. |

Nur ein Superuser kann `SsoProvider`- und `SsoLink`-Zeilen im Admin anlegen,
ändern oder löschen; anderes Staff kann sie nur auflisten.

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
  der Konsole (`/sso-shared`, Feature `admin-sso`; jeder Operator kann die
  Liste sehen, Änderungen brauchen eine editierbare Konsole), wo *Allow email linking* `allow_email_link` an Ort und Stelle
  umschaltet (die ID und ihre Verknüpfungen bleiben). Das Flag gilt für
  **jeden** Tenant. *Edit* (`/sso-shared/{id}/edit`) ändert einen Provider an
  Ort und Stelle (gleiche ID und Slug; ein leeres Secret behält das
  gespeicherte), und jede Zeile zeigt die Callback-URL, die beim IdP zu
  registrieren ist. Das Feld für die Issuer-URL erscheint nur bei `oidc`. Ein
  abgelehntes Hinzufügen oder Bearbeiten zeigt das Formular erneut (Status 400)
  mit den eingegebenen Werten und dem Fehler, nie mit dem Secret.

Auf der Anmeldeseite eines Tenants verschmelzen die beiden Mengen, und bei einer
Slug-Kollision **gewinnt der eigene Anbieter des Tenants** gegenüber dem geteilten
— ein Tenant kann also einen geteilten Anbieter für sich selbst überschreiben.

Die Callback-URL wird pro Anfrage aus Host + Slug abgeleitet
(`https://<host><login>/sso/<slug>/callback`), registrieren Sie diese also beim
IdP. Das Schema ist `https`, außer ein vertrauenswürdiger Proxy sendet
`X-Forwarded-Proto`. Ein Benutzer wird verknüpft durch das optionale Verknüpfen per E-Mail
(nicht privilegierte Tenant-Benutzer) oder dadurch, dass ein Superuser eine
`SsoLink`-Zeile anlegt: `provider_source` (`tenant`, `shared`, `admin` oder `app` für eine native Anmeldung),
`provider_id` (die ID der Anbieter-Zeile), `issuer` (`kind` oder
`kind|issuer_url` ohne abschließenden Schrägstrich), `subject` und `user_id`.
Der Admin berechnet `key_sha256`. Die Log-Zeile der
Abweisung (`sso refused`) enthält `provider_id`, `issuer` und `subject`. Eine
Zeile anzulegen erfordert die Session-Authentifizierung des Admins
(`Builder::with_session_auth` oder `with_session` des Tenant-Admins); ohne sie
kann niemand Verknüpfungen anlegen.

Eine Verknüpfung, die mehr als 5 Minuten vor dem Konto angelegt wurde, auf das
sie zeigt (etwa eine nach einem Restore wiederverwendete ID), loggt eine Warnung
und meldet trotzdem an. Eine Verknüpfung, deren Benutzer nicht mehr existiert,
wird bei der nächsten Anmeldung gelöscht.
`rustango::sso::link::set_stale_links(StaleLinks::Drop)` löscht eine solche
Verknüpfung stattdessen. Nutze das nur, wenn `created_at`-Werte von Konten nie
importiert oder nachträglich befüllt werden.

Mit dem Feature `totp` muss ein Bare-Admin-Benutzer mit bestätigtem
TOTP-Gerät nach SSO noch einen Code eingeben. Der Admin zeigt das Code-Formular
und hält die offene Anmeldung 5 Minuten in einem signierten Cookie; das
Formular sendet an `<login>/sso-totp`. Der Tenant-Admin hat keinen TOTP-Schritt.

`manage check --deploy` warnt vor einem aktivierten Anbieter, der jeden
bestehenden Benutzer abweisen würde: keine `SsoLink`-Zeilen, und E-Mail-Verknüpfung
aus oder nur privilegierte Benutzer. Im Bare-Admin ist das jeder Anbieter ohne
Verknüpfungen.

## Member (Endbenutzer) SSO

Die obigen Oberflächen melden Personen bei einem **Admin** an.
`tenancy::member_auth` ist das member-seitige Gegenstück: es meldet einen
Endbenutzer in den eigenen Benutzerpool eines Tenants an (`rustango_users`) und
prägt eine **Member-Session**, sodass ein Fitnessstudio-Mitglied / SaaS-Kunde
„Mit Google anmelden“ kann, ohne den Admin zu berühren. Es verwendet exakt
denselben `rustango::sso`-Kern und die eigenen `SsoProvider`-Zeilen des Tenants —
nur die Session, die es prägt, unterscheidet sich, weshalb es hinter dem
`sso`-Feature (nicht `admin-sso`) lebt und keinen Auto-Admin braucht.

Hängen Sie `member_sso_router` in einen `rustango::server::Builder`-Stack ein (es
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

Mit mehreren einkompilierten Backends ist `member_sso_router` für den
Standard-Tenant-Typ (Postgres); nutzen Sie `member_sso_router_for::<sqlx::Sqlite>`
(oder `MySql`).

Es hängt zwei Routen pro Slug an `login_base` an:

- `GET {login_base}/sso/{slug}` — den Handshake beginnen, zum IdP weiterleiten.
- `GET {login_base}/sso/{slug}/callback` — ihn abschließen, den Member
  finden-oder-provisionieren, das Session-Cookie prägen.

Ein Tenant mit Pfad-Präfix bekommt dieselben zwei Routen unter seinem Präfix.

Unterschiede zum Admin-Ablauf:

- **Callback-Schema.** Die Member-`redirect_uri` nimmt ihr Schema aus dem
  `X-Forwarded-Proto` eines vertrauenswürdigen Proxys (und ihren Host aus
  `X-Forwarded-Host`), sonst `http` auf einem
  Loopback-Host (`localhost`, `*.localhost`, eine Loopback-IP) und überall sonst
  `https`, sodass `localhost.example.com` `https` bekommt.
- **Auto-Provisionierung.** Mit `auto_provision = true` (optional; Standard ist `false`) **erstellt**
  eine verifizierte IdP-E-Mail ohne passende `rustango_users`-Zeile eine solche
  — Benutzername aus dem lokalen Teil der E-Mail (bei Kollision entdupliziert),
  ein echter, aber unbrauchbarer zufälliger Passwort-Hash (SSO-Benutzer können
  sich nicht per Passwort anmelden) — und verknüpft sie. Eine E-Mail, die zu
  einem bestehenden Konto passt, folgt der obigen `allow_email_link`-Regel.
  Setzen Sie es auf `false`, um unbekannte E-Mails abzuweisen. Eine native
  Anmeldung (eine App, die ihr ID-Token selbst geprüft hat) ruft
  `find_or_provision_member` direkt auf, mit `ProviderKey::app(issuer)` als
  Schlüssel. Ihr Ergebnis `MemberSignIn` ist `Member(id)`, `NotLinked` (ein
  Konto hat die E-Mail, darf aber nicht darüber verknüpft werden), `NoAccount`,
  `Inactive`, `Unverified` oder `Refused`.
- **Ein eigenes Session-Cookie.** Das Member-Cookie
  (`rustango_member_session`) ist von den Tenant- / Admin-Session-Cookies
  **domänengetrennt**: die signierte Nachricht trägt einen Tag pro Domäne und
  einen Audience-Claim, sodass ein Member-Cookie niemals als Tenant-/Admin-Cookie
  validieren kann (oder umgekehrt), obwohl beide mit
  `RUSTANGO_SESSION_SECRET` signiert sind. Es ist slug-gebunden (ein für `acme`
  geprägtes Cookie authentifiziert niemals auf `globex`) und wird durch eine
  Passwort-Rotation invalidiert (analog zur Admin-Session).
- **Logout.** `member_auth::logout(&pool, &user)` beendet die Sessions des
  Members auf allen Geräten und gibt den `Set-Cookie`-Wert zurück, der das
  Cookie löscht. Bei einem Tenant mit Pfad-Präfix nutzen Sie `logout_at`, das
  das Cookie auf dem Präfix-Pfad löscht.

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

## Wer sich anmelden darf

Jeder Anbieter (und jeder geteilte Anbieter in der Operator-Konsole) hat
Anmelderegeln. Leere Regeln lassen jeden zu, den der IdP anmeldet.

| Feld | Regel |
|---|---|
| `allowed_email_domains` | Nur diese Domains: `client.org` passt nur auf sich selbst, `*.client.org` nur auf Subdomains. Durch Komma oder Zeilenumbruch getrennt. |
| `blocked_email_domains` | Nie diese Domains, auch wenn oben erlaubt. Gilt für jede E-Mail, verifiziert oder nicht. |
| `allowed_idp_tenants` | Microsoft-Verzeichnis-IDs (`tid`). Nur aus einem verifizierten id_token gelesen. |
| `allowed_hosted_domains` | Google-Workspace-Domains (Claim `hd`). |
| `required_claims` | JSON `{"claim": ["wert", ...]}`: der Claim (String oder Liste) muss einen der Werte enthalten. `{"iss": [...]}` legt den Issuer fest. Ein fehlender Claim lehnt ab. |
| `require_verified_email` | Standardmäßig an: die Allowlist akzeptiert nur eine vom IdP verifizierte E-Mail. |

Admin und Operator-Konsole speichern Listen in einer festen Form und
lehnen Werte ab, die sich nicht parsen lassen. Ein trotzdem ungültiger Wert
lehnt jede Anmeldung über den Anbieter ab.

Eine abgelehnte Anmeldung landet im Server-Log und im Audit-Log
(`source = sso:<surface>`, `changes.rule`, z. B. `domain_not_allowed`).
Der Browser sieht eine allgemeine Meldung; die Tenant-Login-Seite zeigt
zusätzlich die `access_denied_message` des Tenants (aus `SsoTenantPolicy`), escaped.

Die Member-Route (`member_sso_router`) löst nur die eigenen Anbieter des
Tenants auf; geteilte Anbieter und ihre Verschärfung durch den Tenant
gelten dort noch nicht.

### id_token-Vertrauen

Profildaten kommen von `/userinfo` über TLS. Claims, die nur ein id_token
trägt (`tid`, `iss`, `xms_edov`), werden erst genutzt, wenn das id_token
besteht: Signatur gegen das JWKS des Issuers (RS256 oder ES256; gecacht
gemäß `max-age`, bei unbekanntem Schlüssel höchstens einmal pro Minute neu
geladen), `aud` enthält die Client-ID, `exp`/`nbf` mit einer Minute
Toleranz, der Issuer (Microsoft: `https://login.microsoftonline.com/{tid}/v2.0`)
und dasselbe `sub` wie userinfo. Sonst fehlen diese Claims, und eine Regel,
die sie braucht, lehnt ab. Eine Microsoft-E-Mail gilt als verifiziert, wenn
`xms_edov` wahr ist oder der Anbieter `allowed_idp_tenants` festlegt und die
`tid` des Tokens darin steht.

## Wer sich registrieren darf

Ein Anbieter kann das Konto auch bei der ersten Anmeldung anlegen. Das
passiert nur, wenn weder ein Link noch ein Konto zur verifizierten E-Mail
passt, und erst nachdem die Anmelderegeln oben bestanden sind.

| Feld | Bedeutung |
|---|---|
| `signup_mode` | `closed` (nur Links), `invite_only` (eine `SsoInvite` für diese E-Mail), `allowlist` (eine Einladung für die E-Mail oder ihre Domain) oder `open` (jeder, den die Anmelderegeln zulassen). Leer: geschlossen auf dem Tenant-Login; der Member-Flow folgt `MemberAuthConfig::auto_provision`. Der Bare-Admin legt nie Konten an. |
| `default_roles` | Tenant-Rollen für neue Benutzer, z. B. `Viewer`. Eine Einladung mit Rollen ersetzt sie. |
| `role_map` | JSON `{"<Gruppe oder App-Rolle>": "Editor"}` (oder eine Liste). Gelesen aus den Claims in `role_claims` (leer: `groups,roles`). |
| `sync_roles_on_login` | `role_map` bei jeder Anmeldung neu anwenden. Nur Rollen aus `role_map` werden vergeben oder entzogen; von Hand vergebene bleiben. |
| `username_from` | `email_local` (Standard), `email` oder `preferred_username` (sonst der lokale Teil der E-Mail). |
| `username_clash` | `suffix` (Standard: `ann-x7k2q9`) oder `refuse`. |
| `sync_profile` | Anzeigename und E-Mail bei jeder Anmeldung vom IdP übernehmen (nie für Superuser; eine E-Mail eines anderen Benutzers wird nicht übernommen). |
| `allow_email_link_staff` | Zusammen mit `allow_email_link` auch Benutzer mit Rollen verknüpfen. Ein Superuser wird nie per E-Mail verknüpft. |

Ein neuer Benutzer ist nie Superuser. Rollen laufen über die
Berechtigungs-Engine (`UserRole`); ein Rollenname ohne `Role`-Zeile wird
übersprungen und im Audit-Log vermerkt (`sso_role_unknown`). Benutzer,
Link (`provisioned = true`), Rollen und die Nutzung der Einladung werden in
einer Transaktion geschrieben.

**Einladungen** (`SsoInvite`, im Tenant-Admin nur für Superuser) gelten für
eine E-Mail (`ann@client.org`) oder eine Domain (`@client.org`, oder
`*.client.org` für Subdomains). Sie können einen Anbieter nennen, Rollen
tragen, ablaufen (`expires_at`) und Nutzungen begrenzen (`max_uses`). Eine
Nutzung wird atomar gezählt: Zwei gleichzeitige Registrierungen um die
letzte Nutzung ergeben ein Konto. `.../rustango_sso_invites/bulk` legt viele
auf einmal an, aus einer eingefügten Liste oder einer CSV-Spalte.

**Trennen und neu verknüpfen.** Auf `SsoLink`-Zeilen gibt es die
Admin-Aktionen *Unlink* und *Re-link on next sign-in* (nur Superuser).
Re-link löscht den Link und hinterlässt eine einmalige Einladung, 7 Tage
gültig, die denselben Benutzer bei der nächsten Anmeldung mit seiner
verifizierten E-Mail wieder verknüpft, auch mit neuem Subject. Ein
Superuser wird nie neu verknüpft.

`check --deploy` meldet einen Anbieter mit offener Registrierung nicht als
blockierend und warnt bei `signup_mode = open` ohne
`allowed_email_domains`, `allowed_hosted_domains` oder `allowed_idp_tenants`.

## Secret-Speicherung

`client_secret` wird **im Ruhezustand verschlüsselt** mit XChaCha20-Poly1305
(AEAD) gespeichert, der Schlüssel abgeleitet aus der Umgebungsvariable
**`RUSTANGO_SECRET_KEY`**. Es wird nur zur Anmeldezeit im Speicher entschlüsselt,
um sich am Token-Endpunkt des IdP zu authentifizieren. So legt ein durchgesickerter
DB-Dump das Secret nie offen, und jeder Tenant behält sein eigenes Secret ohne
Umgebungsvariable pro Anbieter.

> Setzen Sie `RUSTANGO_SECRET_KEY` im Deployment. Es wird per SHA-256 zu einem
> 32-Byte-Schlüssel gehasht; nutzen Sie mindestens 32 zufällige Zeichen (mit eingehängter
> Tenant-Provisionierung warnt `check --deploy` bei einem kürzeren Schlüssel). Ohne
> sie schlägt das Speichern oder Verwenden eines Anbieters schnell fehl —
> dieselbe Haltung wie bei einer fehlenden Datenbank-URL.

Zum Rotieren setzen Sie den neuen Wert in `RUSTANGO_SECRET_KEY` und den alten in
`RUSTANGO_SECRET_KEY_PREVIOUS`. Lesen fällt auf den alten Schlüssel zurück. In
einem Multi-Tenant-Deployment versiegelt `manage reseal-tenant-secrets --apply`
danach die geteilten und die Tenant-SSO-Secrets mit dem neuen Schlüssel neu.

## Anbieter (Presets)

Eingebaute Presets: `google`, `microsoft` (Azure AD), `github`, `gitlab`,
`discord`. Für alles andere verwenden Sie `kind = "oidc"` mit einer `issuer_url` —
rustango führt OpenID-Connect-Discovery aus, um die Endpunkte zu finden (einmal pro Issuer und Stunde). (Sign in
with Apple ist kein Preset; es benötigt id_token/JWKS-Verifizierung.)

## Sicherheitshinweise

- **Zuerst die Verknüpfung** — die `(provider, sub)`-Verknüpfung entscheidet;
  die E-Mail wird nur vom optionalen Verknüpfen per E-Mail genutzt, und nur
  wenn sie verifiziert ist.
- **Keine privilegierten E-Mail-Verknüpfungen** — Superuser und Staff
  verknüpft nur ein Superuser. Eine per E-Mail angelegte Verknüpfung
  funktioniert weiter, nachdem der Benutzer befördert wurde; löschen Sie sie,
  wenn das nicht gewollt ist.
- **Keine Auto-Provisionierung** für die Admins — eine unbekannte E-Mail
  kommt nicht hinein.
- **Secrets im Ruhezustand verschlüsselt** (`RUSTANGO_SECRET_KEY`), nur zur
  Anmeldezeit im Speicher entschlüsselt; Bearbeitungsformulare maskieren das
  gespeicherte Secret.
- Das Flow-Cookie ist kurzlebig (10 Min), `HttpOnly`, `SameSite=Lax`
  und `Secure` über HTTPS; der Handshake trägt PKCE + ein signiertes `state`.
- SSO-Sessions sind die gewöhnliche Admin-Session — das Rotieren oder
  Deaktivieren des verknüpften Benutzers invalidiert sie über das bestehende
  Live-Gate.
- Das Vertrauensmodell ist `/userinfo` über TLS; id_token-Claims zählen erst
  nach der JWKS-Prüfung (siehe [id_token-Vertrauen](#id_token-vertrauen)).
  Stellen Sie dem Admin HTTPS voran.

## Siehe auch

- [Sicherheitsleitfaden](security.md) · [Authentifizierung](auth-flows.md)
