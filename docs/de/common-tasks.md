# Häufige Aufgaben

Kurze Antworten auf „Wie mache ich …?". Jede Antwort ist eine Zeile und verlinkt
die Seite mit dem Code. Die Seiten gehen von der App aus
[Erste Schritte](getting-started.md) aus.

Für manche Aufgaben gibt es noch kein vollständiges Beispiel zum Kopieren und
Ausführen. Diese Zeilen verlinken die nächstbeste Seite und sagen
„(vollständiges Beispiel folgt)".

## Inhaltsverzeichnis

- [Aufgaben](#aufgaben)
- [Mehr Rezepte: das Kochbuch](#mehr-rezepte-das-kochbuch)

## Aufgaben

| Aufgabe | Antwort | Lesen |
|---|---|---|
| Einen Benutzer registrieren | Prüfe das Passwort mit `passwords::strength_score`, hashe es mit `passwords::hash` und speichere den Hash in der Benutzerzeile. | [Erste Schritte, Schritt 14](getting-started.md#schritt-14-registrierung-login-und-jwt-authentifizierung) · [Hashen bei der Registrierung](auth-passwords.md#hashen-bei-der-registrierung) |
| Anmelden | Prüfe das Passwort mit `passwords::verify` und starte dann eine Session oder gib ein JWT aus. | [Erste Schritte, Schritt 14](getting-started.md#schritt-14-registrierung-login-und-jwt-authentifizierung) · [Prüfen bei der Anmeldung](auth-passwords.md#prüfen-bei-der-anmeldung) · [Sessions](auth-sessions.md) |
| Ein Passwort zurücksetzen | `PasswordReset::issue` erzeugt einen signierten Link für die E-Mail; `confirm_password_reset_pool` prüft ihn und speichert den neuen Hash. | [Passwort-Zurücksetzung](auth-flows.md#passwort-zurücksetzung) (vollständiges Beispiel folgt) |
| Eine Datei hochladen | `save_uploads` prüft die Datei und schreibt sie in ein `Storage`; du speicherst den zurückgegebenen Key in einem Model. | [Ein Avatar-Upload von Anfang bis Ende](files.md#ein-avatar-upload-von-anfang-bis-ende) |
| Eine E-Mail senden | Baue eine `Email`, wähle einen Mailer und rufe `email.send(&mailer)` auf. | [E-Mails senden](email.md#schritt-1--eine-e-mail-bauen) |
| Einen Hintergrundjob ausführen | Implementiere `Job`, registriere ihn an einer Queue, starte die Queue und rufe `dispatch` aus einem Handler auf. | [Hintergrundjobs](jobs.md#schritt-1--einen-job-definieren) |
| Eine View testen | Pack deinen Router in einen `TestClient` und sende Requests im Prozess, ohne Server. | [Testen](testing.md#schritt-1--steuere-deine-app-mit-testclient-an) |
| Deployen | Setze `RUSTANGO_ENV=prod`, führe `migrate` und `check --deploy` aus und baue dann mit `cargo build --release`. | [Erste Schritte, Schritt 18](getting-started.md#schritt-18-in-die-produktion-deployen) · [Vor dem Deploy auditieren](security.md#vor-dem-deploy-auditieren) |

## Mehr Rezepte: das Kochbuch

Das [Kochbuch](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/cookbook_blog/COOKBOOK.md)
(auf Englisch) ist eine lauffähige Blog-App mit Rezepten für Models, das ORM,
Migrationen, den Admin, REST-APIs, Multi-Tenancy und mehr. Die Rezepte sind durch
Tests abgesichert. Für Multi-Tenancy selbst beginne mit
[Multi-Tenancy](tenancy.md).
