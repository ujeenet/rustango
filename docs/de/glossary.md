# Glossar

Eine Referenz in einfacher Sprache für die in dieser Dokumentation verwendeten Wörter. Wenn dir ein
Begriff in einem Leitfaden unbekannt ist, schlage ihn zuerst hier nach. Die Definitionen sind
bewusst informell — die vertiefenden Leitfäden liefern die genauen Details.

Wenn du noch nie zuvor eine Web-API gebaut hast, lies [Grundlagen der Web-APIs](#grundlagen-der-web-apis)
von oben bis unten; es ist eine fünfminütige Einführung. Alles andere ist zum Nachschlagen
gedacht, während du vorankommst.

## Inhaltsverzeichnis

- [Grundlagen der Web-APIs](#grundlagen-der-web-apis) — was eine API ist, in alltäglichen Worten
- [Rustango-Bausteine](#rustango-bausteine) — die Teile, die du zusammensetzt
- [Daten und die Datenbank](#daten-und-die-datenbank)
- [Anmeldung und Sicherheit](#anmeldung-und-sicherheit) — Sessions, Tokens, Passwörter, Signaturen
- [Caching, Dateien, E-Mail und Jobs](#caching-dateien-e-mail-und-jobs)
- [Sprachen](#sprachen) — deine App übersetzen
- [Echtzeit und KI-Agenten](#echtzeit-und-ki-agenten) — Live-Updates, MCP
- [Mandantenfähigkeit (multi-tenancy)](#mandantenfähigkeit-multi-tenancy) — nur wenn du mehrere Kunden aus einem Deployment bedienst
- [Ein paar Rust-Wörter](#ein-paar-rust-wörter) — damit die Codeblöcke nicht angsteinflößend sind

---

## Grundlagen der Web-APIs

**API** — *Application Programming Interface* (Programmierschnittstelle). Ein Weg für ein Programm, mit
einem anderen zu sprechen. Eine **Web-API** tut das über das Internet: Deine App sendet eine Nachricht, der
Server sendet eine zurück. Stell sie dir wie einen Kellner vor — du bestellst von einer Speisekarte, die
Küche schickt das Essen zurück.

**REST-API** — der gängigste Stil von Web-API. „REST" ist einfach eine Reihe von
Konventionen: Du wirkst auf **Ressourcen** (wie „posts" oder „users") mit standardisierten
Web-Verben ein. Du musst die Theorie nicht kennen — in der Praxis bedeutet es *vorhersehbare
URLs und eine Handvoll Verben*, wie als Nächstes beschrieben.

**Endpoint** — eine bestimmte URL, auf die deine API antwortet, wie `/api/posts` (alle Posts)
oder `/api/posts/42` (der Post mit der id 42). Eine API ist eine Sammlung von Endpoints.

**HTTP-Verb (oder Methode)** — *was* du an einem Endpoint tun willst. Es gibt fünf,
die dir ständig begegnen:

| Verb | Bedeutet | Beispiel |
|---|---|---|
| `GET` | lesen / abrufen | „gib mir alle Posts" |
| `POST` | erstellen | „füge einen neuen Post hinzu" |
| `PUT` | ersetzen | „überschreibe Post 42 vollständig" |
| `PATCH` | teilweise aktualisieren | „ändere nur den Titel von Post 42" |
| `DELETE` | entfernen | „lösche Post 42" |

**Request / Response** — ein Request ist die Nachricht, die du sendest (ein Verb + ein Endpoint
+ optional ein Datenkörper). Die Response ist das, was zurückkommt (ein Statuscode +
üblicherweise ein Datenkörper).

**JSON** — das Textformat, das APIs zum Transport von Daten verwenden. Es sieht aus wie
`{"title": "Hello", "published": true}` — beschriftete Werte, menschenlesbar. Sowohl
Requests als auch Responses sind üblicherweise JSON.

**Statuscode** — eine dreistellige Zahl in jeder Response, die angibt, wie es gelaufen ist:

| Code | Bedeutung |
|---|---|
| `200` | OK — hier sind deine Daten |
| `201` | Created — dein neues Ding wurde gespeichert |
| `204` | Done — nichts zurückzusenden (z. B. nach einem Löschen) |
| `400` | Bad request — du hast etwas Ungültiges gesendet (der Körper sagt was) |
| `422` | Nicht verarbeitbar — die Daten haben die Validierung nicht bestanden (der Körper nennt das Feld) |
| `401` / `403` | Nicht eingeloggt / nicht erlaubt |
| `404` | Not found (nicht gefunden) |
| `429` | Too many requests — mach langsamer |
| `500` | Der Server ist auf einen Fehler gestoßen |

**401 / 403** — `401` heißt „wir wissen nicht, wer du bist": melde dich an oder
sende ein Token. `403` heißt „wir wissen, wer du bist, aber du darfst das nicht".

**Route** — ein URL-Muster plus das Verb, auf das es antwortet, wie
`GET /api/posts/{id}`, verbunden mit dem Code, der es bearbeitet. Siehe
[URLs & Routing](urls.md).

**Handler** — die Funktion, die für eine Route läuft. Sie bekommt den Request
und gibt die Response zurück.

**Cookie** — ein kleines Stück Text, das der Server im Browser speichern lässt.
Der Browser schickt es bei jedem späteren Request an dieselbe Seite mit. So
bleibt ein Browser angemeldet.

**Multipart (Formular-Upload)** — das Request-Format, mit dem ein Browser ein
Formular mit Dateien sendet. Jedes Feld und jede Datei reist als eigener Teil.
Siehe [Dateien](files.md#schritt-3--einen-upload-annehmen).

**Serverseitig gerenderte Seite** — eine HTML-Seite, die der Server fertig baut
und schickt, sodass der Browser sie nur anzeigt. Siehe [HTML-Views](html-views.md).

**CRUD** — *Create, Read, Update, Delete* (erstellen, lesen, aktualisieren, löschen). Die vier grundlegenden Dinge, die du mit Daten tust.
Eine „CRUD-API" bedeutet einfach eine API, die dich alle vier tun lässt. Siehe
[ViewSets](viewsets.md), die eine vollständige CRUD-API aus einer einzigen Deklaration bauen.

**Query-String / Query-Parameter** — der `?key=value`-Teil am Ende einer URL,
verwendet, um Ergebnisse zu filtern, zu durchsuchen, zu sortieren oder zu paginieren — z. B.
`/api/posts?status=published&page=2`. Jedes `key=value` ist ein Parameter.

**Pagination (Seitennummerierung)** — das Aufteilen einer langen Ergebnisliste in Seiten, damit eine Response nicht
riesig ist. Der **Envelope** ist die Hülle um die Seite, die dir auch die
Gesamtwerte nennt — z. B. `{"count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ]}`. Siehe
[Pagination](viewsets.md#paginierung).

**`curl`** — ein Kommandozeilen-Werkzeug zum manuellen Senden von API-Requests. Die
`curl ...`-Beispiele in dieser Dokumentation lassen dich einen Endpoint aus einem Terminal ausprobieren,
ohne Code zu schreiben.

---

## Rustango-Bausteine

Das sind die Teile, die du zusammensetzt, um eine App zu bauen. Jeder verlinkt auf seinen vollständigen Leitfaden.

**Model (Modell)** — eine Beschreibung einer Art von Ding, die deine App speichert, wie ein `Post` oder
ein `User`. Du schreibst es als Rust-`struct`; Rustango verwandelt es in eine
Datenbanktabelle. Siehe den [ORM-Leitfaden](orm.md).

**Migration** — eine aufgezeichnete Änderung der Form deiner Datenbank (Hinzufügen einer Tabelle,
einer Spalte…). Du generierst eine mit `makemigrations` und wendest sie mit `migrate` an,
sodass jede Umgebung mit derselben Datenbankstruktur endet.

**Serializer** — der Übersetzer zwischen deinen Datenbankzeilen und dem JSON, das deine API
sendet und empfängt. Er entscheidet, welche Felder sichtbar sind, benennt Felder um oder berechnet sie
für die Ausgabe und validiert eingehende Daten. Er *formt* Daten; er speichert sie
nicht (das tut das Modell). Siehe den [Serializer-Leitfaden](serializers.md).

**ViewSet** — nimmt ein Modell und einen Serializer und erzeugt automatisch eine vollständige CRUD-**JSON-API**
(alle fünf Verben oben), sodass du nicht jeden Endpoint von Hand schreibst.
Die *API-View*. Siehe den [ViewSets-Leitfaden](viewsets.md).

**HTML-View (Template-View, klassenbasierte View)** — das serverseitig gerenderte
Gegenstück zu einem ViewSet: verwandelt ein Modell in HTML-**Seiten** — eine Listenseite, eine
Detailseite und Erstellen-/Bearbeiten-/Löschen-Formulare — gerendert durch Tera-Templates,
statt JSON. Die *HTML-View*. Siehe [HTML-Views](html-views.md).

**Template** — eine Datei mit Platzhaltern (Rustango verwendet [Tera](https://keats.github.io/tera/)),
die der Server mit Daten füllt, um eine
HTML-Seite zu erzeugen. `{{ post.title }}` fügt einen Wert ein; `{% for … %}` schleift.

**Router / Mount (Einhängen)** — der Router bildet eingehende URLs auf den Code ab, der sie
verarbeitet. Ein ViewSet zu *mounten* bedeutet „seine Endpoints an einem gegebenen Pfad an deine
App anhängen", z. B. die Posts-API unter `/api/posts` mounten. Siehe [URLs & Routing](urls.md).

**Middleware (eine „Layer"/Schicht)** — Code, der *um* jeden Request herum läuft — vor deinem
Handler und danach — für querschnittliche Belange wie Logging, Rate-Limiting,
Security-Header oder CSRF. „Layer" ist Rustangos Wort für ein Stück
Middleware. Siehe den [Middleware-Leitfaden](middleware.md).

**Extraktor (extractor)** — ein Handler-Argument, das ein Ding für dich aus dem
Request holt, etwa den angemeldeten Benutzer, den JSON-Body oder den aktuellen
Mandanten. Fehlt es, lehnt der Extraktor den Request ab, bevor dein Code läuft.

**Pool** — das Datenbank-Handle, mit dem dein Code liest und schreibt. Rustango
bittet dich, es bei jedem Datenbankaufruf explizit zu übergeben (statt es in einem
Global zu verstecken), sodass immer klar ist, was die Datenbank berührt. Du wirst
`&pool` als letztes Argument von ORM-Aufrufen sehen. `rustango::sql::Pool` ist ein
Enum über die drei Backends, und die alltäglichen Methoden nehmen es.

**Executor** — *kein* Synonym für Pool. Ein Executor ist eine einzelne Verbindung
oder eine offene Transaktion: das, worauf ein Statement tatsächlich läuft. Methoden
mit der Endung `_on` nehmen einen, so bringst du mehrere Statements in eine
Transaktion. `fetch_on`, `first_on`, `count_on` und `exists_on` nehmen jeden
Executor auf allen drei Backends; der Rest der `_on`-Familie ist weiterhin nur für
Postgres. Die Unterscheidung entscheidet, welche Methoden es in deinem Build gibt,
deshalb erklärt [api-conventions](api-conventions.md#funktionen) die Regel vollständig.

**QuerySet** — eine Datenbankabfrage, die du Schritt für Schritt in Rust aufbaust
(`Post::objects().filter(...).order_by(...)`), bevor du sie ausführst. Sie ist lazy:
Nichts trifft die Datenbank, bis du sie `fetch`st.

**Feature-Flag** — ein An/Aus-Schalter, gesetzt in `Cargo.toml`, der ein Stück des Frameworks zur
Buildzeit einschließt oder ausschließt. Er lässt dich deine App klein halten,
indem nur kompiliert wird, was du verwendest. Die meisten Features sind standardmäßig an.

**Reverse (eine URL aus dem Namen bauen)** — eine URL aus ihrem Namen erzeugen,
statt den Pfad zu tippen. So bricht kein Link, wenn sich ein Pfad ändert. Siehe
[URL-Namen & Reverse](urls.md).

**Namespace / Namespacing** — ein Präfix im URL-Namen, wie `blog:post-detail`,
das Namen aus verschiedenen Teilen der App auseinanderhält. Siehe
[Namespacing](urls.md#namespacing).

**Fieldset (Feldgruppe)** — eine Gruppe von Feldern mit Titel auf einem
Admin-Bearbeitungsformular, etwa „Content" und „Publishing". Siehe
[Admin](admin.md#das-änderungsformular).

**Audit Trail (Prüfpfad)** — ein Protokoll, wer welchen Datensatz wann geändert
hat und was die alten und neuen Werte waren. Siehe [Admin](admin.md#prüfpfad).

**OpenAPI** — ein Standard-Dateiformat, das jeden Endpoint einer API beschreibt.
Werkzeuge lesen es, um interaktive API-Doku zu zeigen und Client-Code zu
erzeugen. Siehe [OpenAPI](openapi.md).

**Schema** — eine Beschreibung der Form von Daten. In OpenAPI beschreibt es
einen JSON-Body. In einer Datenbank meint es die Menge der Tabellen, und in
Postgres ist es auch eine benannte Gruppe von Tabellen in einer Datenbank.

**Fixture** — Testdaten, die du vor einem Test anlegst, damit jeder Test von
einem bekannten Stand startet. Siehe [Testen](testing.md).

**Scaffolding (Gerüstbau)** — Generator-Befehle (`startapp`, `make:serializer`,
`make:viewset`…), die Startdateien für dich schreiben, damit du nicht von einer
leeren Seite beginnst. Siehe [Scaffolding](scaffolding.md).

---

## Daten und die Datenbank

**Feld / Spalte (field / column)** — ein Stück Daten an einem Modell, wie der `title` oder
das `published_at` eines Posts. „Feld" ist die Rust-Seite; „Spalte" ist die Datenbankseite; sie
entsprechen einander eins zu eins.

**Primärschlüssel (primary key)** — die eindeutige id, die eine Zeile identifiziert, üblicherweise eine
automatisch hochzählende Zahl namens `id`.

**Fremdschlüssel (foreign key, FK)** — ein Feld an einem Modell, das auf die Zeile eines anderen Modells zeigt
und eine Beziehung modelliert — z. B. hat ein `Post` einen Fremdschlüssel `author_id`, der
auf einen `Author` zeigt. So referenzieren sich Zeilen gegenseitig.

**NULL / nullable** — `NULL` ist das Wort der Datenbank für „kein Wert / leer". Ein
**nullable** Feld darf leer sein; ein nicht-nullable Feld ist erforderlich.

**ORM** — *Object-Relational Mapper.* Der Teil des Frameworks, der deine
Rust-Structs in Datenbankzeilen verwandelt und zurück. So fragst du in Rust ab,
statt SQL von Hand zu schreiben. Siehe den [ORM-Leitfaden](orm.md).

**Transaktion** — eine Gruppe von Datenbankänderungen, die zusammen gelingen
oder scheitern. Entweder werden alle gespeichert oder keine.

**Rollback** — alle Änderungen in einer Transaktion rückgängig machen, als wäre
nichts passiert. Tests nutzen es, um die Datenbank sauber zu hinterlassen.
Siehe [Datenbanktests mit Rollback](testing.md#datenbanktests-mit-rollback).

**Tri-Dialekt (tri-dialect)** — „funktioniert gleich auf allen drei unterstützten Datenbanken" —
PostgreSQL, MySQL und SQLite. Wenn ein Feature tri-dialektfähig ist, kannst du die
Datenbank wechseln, ohne deinen Code zu ändern.

---

## Anmeldung und Sicherheit

**Session (Sitzung)** — das Gedächtnis des Servers für einen angemeldeten
Benutzer zwischen Requests. Der Browser hält nur eine ID in einem Cookie; die
Daten bleiben auf dem Server. Siehe [Sessions](auth-sessions.md).

**Opake ID** — eine ID, die für sich nichts bedeutet, nur eine lange
Zufallszeichenkette. Nur der Server kann nachsehen, worauf sie zeigt.

**Token** — eine Zeichenkette, mit der ein Client beweist, wer er ist, meist im
Header `Authorization`. Behandle es wie ein Passwort.

**Bearer-Token** — ein Token, gesendet als `Authorization: Bearer <token>`. Wer
es hat („trägt"), wird hereingelassen, also halte es geheim. Siehe
[API-Keys](auth-api-keys.md).

**JWT** — *JSON Web Token.* Ein Token mit signierten Daten (wer der Benutzer ist,
wann es abläuft). Der Server kann es ohne Datenbankabfrage prüfen. Siehe
[JWT](auth-jwt.md).

**Claims** — die Datenstücke in einem JWT, etwa die Benutzer-ID und die
Ablaufzeit.

**Zustandslos (stateless)** — der Server merkt sich nichts zwischen Requests.
Alles, was er braucht, kommt im Request selbst, wie bei einem JWT.

**Access-Token / Refresh-Token** — ein kurzlebiges Token für jeden Request, plus
ein langlebiges, das nur dazu dient, ein neues Access-Token zu holen, wenn das
alte abläuft. Siehe [JWT-API](auth-jwt-api.md).

**Token-Rotation** — jedes Mal, wenn ein Refresh-Token benutzt wird, wird es
durch ein neues ersetzt und das alte gilt nicht mehr. Ein gestohlenes
Refresh-Token funktioniert dann nur einmal.

**Widerruf (revocation)** — ein Token oder eine Session vor dem Ablauf ungültig
machen, zum Beispiel beim Abmelden. Siehe
[Widerruf](auth-jwt-api.md#widerruf-und-der-jti-speicher).

**Ablauf (expiry)** — der Moment, ab dem ein Token, ein Link oder eine Session
nicht mehr funktioniert.

**Secret / gemeinsames Secret** — ein privater Wert, der Daten signiert oder
prüft. Ein gemeinsames Secret kennen beide Seiten, etwa Client und Server.
Schreibe es nie in den Code oder in Logs.

**Hash** — ein Einweg-Fingerabdruck von Daten. Du kannst ein Passwort gegen
seinen Hash prüfen, aber das Passwort nicht daraus zurückgewinnen. Siehe
[Passwörter](auth-passwords.md).

**Salt** — Zufallsdaten, die vor dem Hashen in jedes Passwort gemischt werden,
damit zwei gleiche Passwörter verschiedene Hashes bekommen.

**Argon2id** — das Verfahren, mit dem Rustango Passwörter hasht. Es ist mit
Absicht langsam, was das Raten von Passwörtern teuer macht.

**PHC-Zeichenkette (PHC string)** — die Standard-Textform eines gespeicherten
Passwort-Hashes. Sie enthält Verfahren, Einstellungen, Salt und Hash in einer
Zeichenkette, die mit `$argon2id$` beginnt.

**HMAC** — eine Signatur aus Daten plus einem geheimen Schlüssel. Wer den
Schlüssel hat, kann prüfen, dass die Daten nicht verändert wurden. Siehe
[HMAC-Request-Signierung](auth-hmac.md) und [Konto-Flows](auth-flows.md).

**Vergleich in konstanter Zeit** — zwei Secrets so vergleichen, dass es immer
gleich lange dauert. So lernt ein Angreifer nichts aus der Dauer der Prüfung.

**Replay** — ein Angreifer sendet einen früher mitgeschnittenen Request noch
einmal. Ein Zeitlimit für signierte Requests blockiert das. Siehe
[Uhren-Skew und Replay](auth-hmac.md#uhren-skew-und-replay).

**Vorsignierte URL (presigned URL)** — ein Link zu einer Datei mit eingebauter
Signatur und Ablaufzeit. Wer den Link hat, kann diese eine Datei kurze Zeit
herunterladen (oder hochladen). Siehe [Dateien](files.md).

**CSRF** — *Cross-Site Request Forgery.* Eine andere Seite bringt einen
angemeldeten Browser dazu, einen Request an deine App zu senden. Ein CSRF-Token
in jedem Formular blockiert das. Siehe [CSRF-Schutz](middleware.md#csrf-schutz).

**Berechtigungs-Codename** — der Name einer Berechtigung, geschrieben als
`{table}.{action}`, zum Beispiel `post.add`. Routen prüfen ihn, um zu
entscheiden, wer was darf. Siehe
[Auth-Backends](auth-backends.md#berechtigungen-require_perm).

**OAuth** — ein Standard, mit dem eine App begrenzten Zugriff im Namen eines
Benutzers bekommt, ohne sein Passwort zu sehen. Darauf beruhen
„Anmelden mit …"-Buttons. Siehe [SSO](sso.md).

---

## Caching, Dateien, E-Mail und Jobs

**Backend (Storage-Backend, Mailer-Backend, Cache-Backend)** — der echte Ort
oder Dienst hinter einer gemeinsamen Schnittstelle: lokale Platte oder S3 für
Dateien, SMTP oder die Konsole für E-Mail, Speicher oder Redis für einen Cache.
Du kannst es tauschen, ohne den aufrufenden Code zu ändern. Auth-Backends sind
dieselbe Idee: jedes ist ein Weg, einen Benutzer zu erkennen. Siehe
[Auth-Backends](auth-backends.md).

**Cache** — ein schneller Speicher mit Kopien von Daten, die du oft liest, damit
du sie nicht jedes Mal neu berechnest oder holst. Siehe [Caching](caching.md).

**Cache-Key (Key)** — der Name, unter dem ein Wert im Cache liegt, wie
`post:42:summary`.

**TTL** — *time to live.* Wie lange ein Wert im Cache bleibt, bevor er abläuft.
Siehe [TTL und Ablauf](caching.md#ttl-und-ablauf).

**Objektspeicher (object storage)** — ein Dienst, der Dateien („Objekte") über
HTTP speichert, etwa Amazon S3 oder Azure Blob Storage. Siehe [Dateien](files.md).

**Mailer** — der Teil deiner App, der E-Mails sendet. In der Entwicklung gibt er
die E-Mail im Terminal aus, in Produktion sendet er sie wirklich. Siehe
[E-Mail](email.md).

**SMTP** — das Standardprotokoll, um E-Mails an einen Mailserver zu senden.

**Transaktionale E-Mail** — eine E-Mail, die deine App wegen einer Aktion eines
Benutzers sendet, etwa ein Passwort-Reset oder eine Quittung.

**Job-Queue (Warteschlange)** — eine Liste von Aufgaben, die im Hintergrund auf
ihre Ausführung warten, damit ein Request sofort antworten kann. Siehe
[Hintergrund-Jobs](jobs.md).

**Worker** — eine Hintergrund-Task, die Jobs aus der Queue nimmt und ausführt.

**Retry und Backoff** — einen fehlgeschlagenen Job erneut ausführen und nach
jedem Fehlschlag etwas länger warten. Siehe
[Retries und Backoff](jobs.md#retries-und-backoff).

**Dead-Letter** — wohin ein Job geht, wenn er endgültig gescheitert ist, damit
du ihn später ansehen kannst. Siehe
[Der Dead-Letter-Handler](jobs.md#der-dead-letter-handler).

---

## Sprachen

**Locale** — eine Sprache, manchmal mit Region, wie `en`, `de` oder `pt-BR`.
Siehe [i18n](i18n.md).

**Katalog (Message-Katalog)** — die Datei mit den übersetzten Texten für eine
Locale.

**Pluralisierung** — die richtige Form eines Wortes für eine Zahl wählen, wie
„1 Datei" und „2 Dateien". Manche Sprachen haben mehr als zwei Formen. Siehe
[Pluralisierung](i18n.md#pluralisierung).

**Accept-Language** — ein Header, den der Browser sendet und der die bevorzugten
Sprachen des Benutzers auflistet. Siehe
[Die Sprache aushandeln](i18n.md#die-sprache-aushandeln).

**RTL** — *right-to-left.* Sprachen wie Arabisch und Hebräisch, die von rechts
nach links geschrieben werden. Siehe
[Rechts-nach-links-Sprachen](i18n.md#rechts-nach-links-sprachen).

---

## Echtzeit und KI-Agenten

**WebSocket** — eine Verbindung, die offen bleibt, damit Browser und Server
jederzeit Nachrichten senden können. Siehe [WebSockets & SSE](websockets.md).

**SSE** — *Server-Sent Events.* Eine Verbindung, die offen bleibt, damit der
Server Updates an den Browser schicken kann. Nachrichten gehen nur in eine
Richtung, vom Server zum Browser.

**Broadcast** — eine Nachricht auf einmal an alle verbundenen Empfänger senden.

**Keep-alive / Ping** — eine kleine Nachricht, die ab und zu gesendet wird, damit
ein Proxy eine ruhige Verbindung nicht schließt und eine tote bemerkt wird.

**Back-Pressure** — was passiert, wenn ein Empfänger langsamer liest, als
Nachrichten ankommen. Der Sender muss warten, puffern oder Nachrichten
verwerfen.

**MCP** — *Model Context Protocol.* Ein Standard, mit dem ein KI-Agent die Tools
deiner App findet und aufruft. Siehe [MCP-Server](mcp.md).

**JSON-RPC** — ein einfaches Protokoll, um eine benannte Funktion auf einem
Server aufzurufen; Anfrage und Antwort sind JSON. MCP nutzt es.

**Agent** — ein KI-Programm, meist von einem großen Sprachmodell gesteuert, das
Tools aufruft, um Arbeit zu erledigen.

**Tool, Resource, Prompt (MCP)** — was ein MCP-Server einem Agenten anbietet: ein
Tool ist eine Aktion zum Aufrufen, eine Resource sind Daten zum Lesen, und ein
Prompt ist eine fertige Anweisung.

---

## Mandantenfähigkeit (multi-tenancy)

Hinter dem `tenancy`-Feature. Überspringe diesen Abschnitt, wenn du eine
gewöhnliche App für einen einzelnen Kunden baust — dann trifft nichts davon zu.

**Mandantenfähigkeit (multi-tenancy)** — ein Deployment betreiben, das mehrere
Kunden bedient, von denen jeder nur die eigenen Daten sieht. Rustango löst aus
dem Hostnamen auf, zu welchem Kunden eine Anfrage gehört, und leitet sie für den
Rest der Anfrage auf dessen Daten.

**Mandant / Tenant (oder Org)** — ein Kunde in einem solchen Deployment. Ein
Mandant hat einen Slug (`acme`), ein Host-Muster (`acme.example.com`) und eigene
Benutzer. `Org` ist die Registry-Zeile; „Mandant" ist das, was sie beschreibt.

**Registry** — die kleine Datenbank, die die Mandanten auflistet: wer sie sind,
wo ihre Daten liegen, ob sie aktiv sind. Getrennt von den Daten jedes Mandanten
und die eine Datenbank, die das Framework immer braucht.

**Storage-Modus** — wie die Daten eines Mandanten von denen der Nachbarn
getrennt werden: eine eigene Datenbank oder ein eigenes Schema in einer
gemeinsamen. Pro Mandant beim Provisionieren gewählt.

**Operator** — ein Administrator des *Deployments*, nicht eines Mandanten.
Operatoren legen Mandanten an und binden Hostnamen; sie sind keine Benutzer
eines Mandanten und leben in der Registry. Siehe
[Operator-Konsole](operator-console.md).

**Operator-Konsole** — die Weboberfläche, die Operatoren benutzen: Mandanten
provisionieren, Hostnamen binden, weitere Operatoren verwalten, den Audit-Trail
lesen. Fast jede Aktion darin ist auch ein `manage`-Verb, lässt sich also
skripten.

**Provisionieren (provisioning)** — einen Mandanten anlegen: seine Datenbank
oder sein Schema erzeugen, Migrationen hineinlaufen lassen und ihn in der
Registry eintragen.

---

## Ein paar Rust-Wörter

Du musst kein Rust können, um die meisten Beispiele zu *lesen*, aber diese vier Wörter tauchen
überall auf.

**`struct`** — ein benanntes Bündel von Feldern, wie ein Datensatz oder eine Klasse mit nur
Daten. Modelle und Serializer sind Structs.

**Derive-Makro (`#[derive(Model)]`, `#[derive(Serializer)]`…)** — eine einzeilige
Annotation über einem Struct, die dem Compiler sagt, einen Haufen
Code für dich automatisch zu generieren (das Datenbank-Mapping, die JSON-Konvertierung, …). Es ist die Magie, die
ein einfaches Struct in ein funktionierendes Modell oder Serializer verwandelt.

**`async` / `.await`** — Rusts Art, mit Arbeit umzugehen, die Warten beinhaltet (eine
Datenbankabfrage, ein Netzwerkaufruf). Eine mit `async` markierte Funktion ist „awaitable"; das
`.await` nach einem Aufruf bedeutet „warte hier auf das Ergebnis". Alles, was die
Datenbank berührt, ist `async`.

**`Result` / `Option`** — wie Rust Ergebnisse meldet, statt Ausnahmen zu
werfen. Ein `Result` ist „Erfolg *oder* ein Fehler"; ein `Option` ist „ein Wert *oder*
nichts". Das `?`, das du nach manchen Aufrufen siehst, bedeutet „falls dies fehlgeschlagen ist, halte an und
gib den Fehler zurück".
