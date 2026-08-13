# Glossar

Eine Referenz in einfacher Sprache für die in dieser Dokumentation verwendeten Wörter. Wenn dir ein
Begriff in einem Leitfaden unbekannt ist, schlage ihn zuerst hier nach. Die Definitionen sind
bewusst informell — die vertiefenden Leitfäden liefern die genauen Details.

Wenn du noch nie zuvor eine Web-API gebaut hast, lies [Grundlagen der Web-APIs](#web-api-basics)
von oben bis unten; es ist eine fünfminütige Einführung. Alles andere ist zum Nachschlagen
gedacht, während du vorankommst.

## Inhaltsverzeichnis

- [Grundlagen der Web-APIs](#web-api-basics) — was eine API ist, in alltäglichen Worten
- [Rustango-Bausteine](#rustango-building-blocks) — die Teile, die du zusammensetzt
- [Daten und die Datenbank](#data-and-the-database)
- [Ein paar Rust-Wörter](#a-few-rust-words) — damit die Codeblöcke nicht angsteinflößend sind
- [Frameworks, mit denen wir vergleichen](#frameworks-we-compare-to)

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
| `401` / `403` | Nicht eingeloggt / nicht erlaubt |
| `404` | Not found (nicht gefunden) |
| `429` | Too many requests — mach langsamer |
| `500` | Der Server ist auf einen Fehler gestoßen |

**CRUD** — *Create, Read, Update, Delete* (erstellen, lesen, aktualisieren, löschen). Die vier grundlegenden Dinge, die du mit Daten tust.
Eine „CRUD-API" bedeutet einfach eine API, die dich alle vier tun lässt. Siehe
[ViewSets](viewsets.md), die eine vollständige CRUD-API aus einer einzigen Deklaration bauen.

**Query-String / Query-Parameter** — der `?key=value`-Teil am Ende einer URL,
verwendet, um Ergebnisse zu filtern, zu durchsuchen, zu sortieren oder zu paginieren — z. B.
`/api/posts?status=published&page=2`. Jedes `key=value` ist ein Parameter.

**Pagination (Seitennummerierung)** — das Aufteilen einer langen Ergebnisliste in Seiten, damit eine Response nicht
riesig ist. Der **Envelope** ist die Hülle um die Seite, die dir auch die
Gesamtwerte nennt — z. B. `{"count": 137, "page": 2, "results": [ … ]}`. Siehe
[Pagination](viewsets.md#pagination).

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

**Template** — eine Datei mit Platzhaltern (Rustango verwendet [Tera](https://keats.github.io/tera/),
sehr ähnlich zu Django-Templates oder Jinja), die der Server mit Daten füllt, um eine
HTML-Seite zu erzeugen. `{{ post.title }}` fügt einen Wert ein; `{% for … %}` schleift.

**Router / Mount (Einhängen)** — der Router bildet eingehende URLs auf den Code ab, der sie
verarbeitet. Ein ViewSet zu *mounten* bedeutet „seine Endpoints an einem gegebenen Pfad an deine
App anhängen", z. B. die Posts-API unter `/api/posts` mounten. Siehe [URLs & Routing](urls.md).

**Middleware (eine „Layer"/Schicht)** — Code, der *um* jeden Request herum läuft — vor deinem
Handler und danach — für querschnittliche Belange wie Logging, Rate-Limiting,
Security-Header oder CSRF. „Layer" ist Rustangos Wort für ein Stück
Middleware. Siehe den [Middleware-Leitfaden](middleware.md).

**Pool (oder Executor)** — die Datenbankverbindung, die dein Code zum Lesen und
Schreiben verwendet. Rustango bittet dich, den Pool bei jedem Datenbankaufruf explizit zu übergeben
(statt ihn in einem Global zu verstecken), sodass immer klar ist, was die
Datenbank berührt. Du wirst `&pool` als letztes Argument von ORM-Aufrufen sehen.

**QuerySet** — eine Datenbankabfrage, die du Schritt für Schritt in Rust aufbaust
(`Post::objects().filter(...).order_by(...)`), bevor du sie ausführst. Sie ist lazy:
Nichts trifft die Datenbank, bis du sie `fetch`st.

**Feature-Flag** — ein An/Aus-Schalter, gesetzt in `Cargo.toml`, der ein Stück des Frameworks zur
Buildzeit einschließt oder ausschließt. Er lässt dich deine App klein halten,
indem nur kompiliert wird, was du verwendest. Die meisten Features sind standardmäßig an.

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

**Tri-Dialekt (tri-dialect)** — „funktioniert gleich auf allen drei unterstützten Datenbanken" —
PostgreSQL, MySQL und SQLite. Wenn ein Feature tri-dialektfähig ist, kannst du die
Datenbank wechseln, ohne deinen Code zu ändern.

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

---

## Frameworks, mit denen wir vergleichen

Diese Dokumentation sagt gelegentlich „wie X", um Leser zu unterstützen, die aus anderen
Ökosystemen kommen. Die Vergleiche sind ein Bonus — du brauchst sie nie, um einem Leitfaden zu folgen.

**Django** — ein populäres Python-Webframework. Rustango übernimmt viel von seiner Form
(Modelle, Migrationen, eine Admin-Oberfläche, die `manage`-Befehle).

**DRF (Django REST Framework)** — Djangos Erweiterung zum Bauen von REST-APIs.
Rustangos Serializer und ViewSets sind daran modelliert, also bedeutet „DRF-Form"
„so angeordnet, wie DRF es tut" — z. B. Validierungsfehler, die als JSON-Objekt
mit Feldnamen als Schlüsseln zurückgegeben werden.

**Laravel / Rails** — populäre PHP- und Ruby-Webframeworks, aus demselben
Grund „wenn du dies verwendet hast, wird sich das vertraut anfühlen" erwähnt.
