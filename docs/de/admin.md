# Die Administrationsoberfläche

**Rustango** erzeugt eine vollständige Admin-Oberfläche aus deinen Modellen — ein
fertiges Backoffice zum Durchsuchen und Bearbeiten deiner Daten, aber mit
**null Boilerplate pro Modell**. Füge `#[derive(Model)]` hinzu, binde das Admin einmal
ein, und jedes Modell erhält eine Listenansicht mit Suche, Filtern, Sortierung,
Paginierung und Massenaktionen; ein nach Feldgruppen gegliedertes Erstellen-/Bearbeiten-
Formular; inline-Bearbeitung von Kindobjekten; einen Prüfpfad pro Zeile; und eine
Live-Modellreferenz. Alles Folgende wird deklarativ in einem `admin(...)`-Block am
Derive konfiguriert, oder mit einer Handvoll `Builder`-Methoden und Registrierungsmakros
auf Modulebene.

[![Das automatisch generierte Admin: eine Beitragsliste mit Filter-Facetten, Suche, Massenaktionen und Paginierung — alles aus einem einzigen `admin(...)`-Block](../img/admin.png)](../img/admin.png)

> **Quelle:** `rustango::admin` (die `admin(...)`-Derive-Optionen, die `Builder`-
> API und die Registrierungsmakros) — hinter dem `admin`-Feature (standardmäßig
> aktiviert).
>
> **Lauffähige Version:** jedes Feature auf dieser Seite wird in einem getesteten,
> kompilierbaren Beispiel unter
> [`crates/rustango/examples/admin_demo`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/admin_demo)
> ausgeführt. Die Screenshots auf dieser Seite stammen aus diesem Beispiel. Wenn ein
> Snippet seltsam aussieht, vergleiche es damit.

> **Ein Begriff hier ist neu für dich?** *model*, *fieldset*, *audit trail* — siehe das
> [Glossar](glossary.md).

---

## Inhaltsverzeichnis
- [Einbinden](#einbinden) · [Die Startseite](#die-startseite)
- [Ein Modell konfigurieren: der `admin(...)`-Block](#ein-modell-konfigurieren-der-admin-block)
- [Die Listenansicht](#die-listenansicht) — Spalten, Suche, Filter, Datumshierarchie, Sortierung, Paginierung
- [Das Änderungsformular](#das-änderungsformular) — Feldgruppen, Widgets, FK-Bearbeitung, vorbefüllte & schreibgeschützte Felder
- [Inlines](#inlines) · [Massenaktionen](#massenaktionen) · [Prüfpfad](#prüfpfad)
- [Berechnete Spalten & benutzerdefinierte Filter](#berechnete-spalten-und-benutzerdefinierte-filter)
- [Benutzerdefinierte Views, Querysets & Berechtigungen](#benutzerdefinierte-views-querysets-und-berechtigungen)
- [Authentifizierung](#authentifizierung) · [Theming & Branding](#theming-und-branding)
- [`Builder`-Referenz](#builder-referenz) · [Routen-Referenz](#routen-referenz)
- [Die Modellreferenz (`__docs`)](#die-modellreferenz) · [Das Beispiel ausprobieren](#das-beispiel-ausprobieren)

---

## Einbinden

> **Das Admin ist standardmäßig offen.** Es erkennt und liefert *jedes* Modell
> automatisch — auflisten, erstellen, bearbeiten, löschen — ohne Authentifizierung,
> bis du sie hinzufügst. Mache es nicht öffentlich zugänglich, bevor du das Login
> verdrahtet hast: siehe [Authentifizierung](#authentifizierung) weiter unten.

Das Admin ist ein `axum::Router`, den du aus einem Datenbank-Pool baust und unter
einen Pfad einhängst:

```rust
use rustango::admin;

let admin_router = admin::Builder::new(pool.clone())
    .title("Admin Demo")
    .subtitle("rustango auto-admin showcase")
    .admin_prefix("/admin")          // MUST match the nest path below
    .build();

let api = axum::Router::new().nest("/admin", admin_router);
```

Das Auto-Admin erkennt jedes `#[derive(Model)]` in deiner Binärdatei **automatisch**
über die `inventory`-Registry — du registrierst Modelle nicht einzeln. Öffne
`http://localhost:8080/admin`, und sie erscheinen gruppiert in der Seitenleiste.

> **`admin_prefix` muss dem Einhängepfad entsprechen.** Das Admin baut seine Links und
> Formularaktionen aus `admin_prefix` (Standard `/__admin`). Wenn du unter `/admin`
> einhängst, aber den Standard-Präfix belässt, liefert jeder Link einen 404. Setze sie
> gleich.

> **Die Registry einbinden.** Die Inventory-Registrierung wird nur dann in die finale
> Binärdatei eingebunden, wenn die Modelltypen irgendwo referenziert werden. Ein
> Bibliotheks-Crate, dessen Modelle nicht anderweitig verwendet werden, benötigt
> eventuell einen Anstoß per `let _ = std::any::type_name::<Post>();` in `main` (das
> Beispiel macht das) — andernfalls verwirft der Linker sie und sie erscheinen nie.

### Die Startseite

Die Admin-Wurzel (`GET /<prefix>`) listet jedes registrierte Modell auf, gruppiert nach
App, mit Tabellennamen und Feldanzahl jedes Modells — plus einem **Recent actions**-
Feed der neuesten geprüften Änderungen.

[![Die Admin-Startseite: jedes registrierte Modell nach App gruppiert mit Tabellen- und Feldanzahl sowie ein Aktivitäts-Feed der letzten Aktionen](../img/admin-home.png)](../img/admin-home.png)

---

## Ein Modell konfigurieren: der `admin(...)`-Block

Alles darüber, wie ein Modell erscheint, wird in einem `admin(...)`-Block am Derive
festgelegt. Hier der Vorzeige-`Post` aus dem Beispiel, der fast jeden Stellhebel
ausübt:

```rust
#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display       = "id, title, author_id, status, view_count, published_at",
        list_display_links = "id, title",
        list_filter        = "status, author_id",
        search_fields      = "title, body",
        search_help_text   = "Search posts by title or body",
        ordering           = "-published_at",
        list_per_page      = 10,
        date_hierarchy     = "published_at",
        fieldsets          = "Content: title, body, status | Publishing: author_id, published_at, view_count",
        actions            = "publish, archive",
    ),
    audit(track = "title, body, status"),
)]
pub struct Post { /* … */ }
```

`display = "title"` (am Modell, außerhalb von `admin(...)`) legt das menschenlesbare
Label fest, das überall verwendet wird, wo eine Zeile referenziert wird — FK-Spalten in
den Listen anderer Modelle, der Breadcrumb, der Titel der Detailseite.

### Jede `admin(...)`-Option

| Schlüssel | Beispiel | Was es bewirkt |
|---|---|---|
| `list_display` | `"id, title, status"` | In der Liste angezeigte Spalten, in Reihenfolge. FK-Spalten rendern den `display`-Wert des Ziels. Berechnete Spalten (siehe unten) können hier benannt werden. Leer = jedes skalare Feld. |
| `list_display_links` | `"id, title"` | Welche `list_display`-Zellen zur Detailseite verlinken. Muss eine Teilmenge von `list_display` sein. |
| `list_filter` | `"status, author_id"` | Facetten-Karten in der rechten Leiste — eindeutige Werte + Anzahlen, zum Filtern anklicken. Funktioniert bei skalaren und FK-Spalten. |
| `search_fields` | `"title, body"` | Felder, die das `?q=`-Suchfeld abgleicht (Groß-/Kleinschreibung-unabhängig `ILIKE`/`LIKE`). |
| `search_help_text` | `"Search by title"` | Beschriftung, die neben dem Suchfeld gerendert wird. |
| `ordering` | `"-published_at, id"` | Standardsortierung. `-`-Präfix = DESC; ohne = ASC. Mehrere Schlüssel durch Komma getrennt. |
| `list_per_page` | `10` | Seitengröße (Standard 50). |
| `date_hierarchy` | `"published_at"` | Aufschlüsselungsleiste Jahr → Monat → Tag über der Liste, auf einer Date-/DateTime-Spalte. |
| `fieldsets` | `"Content: title, body \| Meta: status"` | Gliedert das Änderungsformular in benannte Abschnitte. Pipe `\|` trennt Abschnitte, Komma trennt Felder; die `Title:`-Legende ist optional. |
| `actions` | `"publish, archive"` | Massenaktionen, die im Aktionsauswähler der Liste angeboten werden (jede benötigt einen registrierten Handler — siehe [Massenaktionen](#massenaktionen)). |
| `readonly_fields` | `"created_at"` | Felder, die im Änderungsformular als Text (ohne Eingabe) gerendert werden. |
| `raw_id_fields` | `"author_id"` | FK-Felder, die über eine reine ID-Eingabe + Nachschlage-Link bearbeitet werden (gut für große Zieltabellen). |
| `autocomplete_fields` | `"author_id"` | FK-Felder, die über eine Ajax-Vervollständigung bearbeitet werden, gestützt auf den `__autocomplete`-Endpunkt des Ziels. |
| `prepopulated_fields` | `"slug:title"` | Ein Feld automatisch befüllen, indem ein anderes beim Tippen sluggifiziert wird (`target:source`; kombiniere Quellen mit `+`). |
| `list_select_related` | `"all"` / `"none"` / `"author_id"` | Steuert das automatische JOIN von FK-Spalten in der Listenabfrage. `"all"` (Standard) joint jeden FK; `"none"` deaktiviert; ein CSV beschränkt auf benannte FKs. |
| `formfield_overrides` | `"status:textarea"` | Überschreibt das Formular-Widget eines Feldes (`field:widget`) — siehe die [Widget-Tabelle](#formular-widgets). |
| `actions_on_top` | `true` | Rendert die Massenaktionsleiste über der Liste (Standard `true`). |
| `actions_on_bottom` | `false` | Rendert eine zweite Aktionsleiste unter der Liste (Standard `false`). |

---

## Die Listenansicht

`GET /<prefix>/<table>` rendert die Liste. Aus dem einzelnen `admin(...)`-Block oben
erhältst du sortierbare Spalten, ein Suchfeld mit Hilfetext, die Status-/Autor-Facetten-
Karten mit Live-Anzahlen, die Datumsaufschlüsselung, Paginierung mit 10/Seite und den
publish/archive-Aktionsauswähler.

**Filtern.** Klicke auf einen beliebigen Wert in einer `list_filter`-Facetten-Karte, um
die Liste einzuschränken; der aktive Filter erscheint als Chip mit einem **clear**-Link,
und Zeilenanzahl sowie Facetten-Anzahlen aktualisieren sich. Filter, Suche, Sortierung
und die Datumshierarchie fügen sich alle in der Query-Zeichenkette zusammen und lassen
sich kombinieren.

Eine Facetten-Karte zeigt bis zu 15 Werte; der Rest klappt in einen Link "+N more"
(`?facet_show_all=<field>`) ein, der höchstens 200 auflistet.

[![Die nach status=published gefilterte Beitragsliste: ein aktiver Filter-Chip, die passende hervorgehobene Facette, das Suchfeld und der Massenaktionsauswähler](../img/admin-list-filtered.png)](../img/admin-list-filtered.png)

**Sortieren.** Klicke auf einen Spaltenkopf zum Sortieren; erneut klicken, um die
Richtung umzukehren (`?sort=col&order=asc|desc`). Der Standard kommt aus `ordering`.

**Paginierung.** `list_per_page` legt die Seitengröße fest; navigiere mit `?page=N`.
Registriere sehr große Tabellen mit `Builder::skip_count_for([...])`, um das
`SELECT COUNT(*)` zu überspringen (der Pager zeigt dann "Page N" ohne Gesamtsumme); ein
`?count=skip` pro Anfrage bewirkt dasselbe ad hoc.

**Suche.** Wenn `search_fields` gesetzt ist, erscheint ein Suchfeld und gleicht diese
Felder mit `ILIKE` (PostgreSQL) / `LIKE` (MySQL, SQLite) ab. `search_help_text` wird als
dessen Beschriftung gerendert.

**Datumshierarchie.** Mit gesetztem `date_hierarchy` sitzt ein Breadcrumb Jahr → Monat →
Tag über der Tabelle; das Hineinnavigieren fügt halboffene Bereichsfilter auf dieser
Spalte hinzu, unter Verwendung der tri-dialektalen Datumsextraktion (PostgreSQL
`EXTRACT`, MySQL, SQLite `strftime`).

---

## Das Änderungsformular

`GET /<prefix>/<table>/new` (erstellen) und `GET /<prefix>/<table>/<pk>/edit`
(bearbeiten) rendern das Formular. `fieldsets` gruppiert die Eingaben in benannte
Abschnitte; ohne es erscheinen alle bearbeitbaren Felder in einem Block.

[![Das Post-Änderungsformular gegliedert in die Feldgruppen Content und Publishing, jedes Feld mit dem passenden Eingabe-Widget für seinen Typ](../img/admin-fieldsets.png)](../img/admin-fieldsets.png)

Das Absenden eines Formulars validiert die Eingabe, schreibt die Zeile, erfasst einen
Prüfpfad-Eintrag und leitet zur schreibgeschützten **Detail**-Ansicht
(`GET /<prefix>/<table>/<pk>`) weiter, die jedes Feld plus Inlines und die Prüfpfad-Karte
(unten) zeigt. Die **Edit**- und **Delete**-Schaltflächen der Detailseite führen zum
Formular und zur Löschbestätigung.

### Formular-Widgets

Jedes Feld rendert standardmäßig eine seinem Typ entsprechende Eingabe —
`<input type="number">` für Ganzzahlen, `type="date"`/`datetime-local` für Datumsangaben,
`type="checkbox"` für Booleans, ein `<textarea>` für lange Zeichenketten, ein `<select>`
für FK-Spalten und so weiter. Überschreibe pro Feld mit
`formfield_overrides = "field:widget"`:

| Widget | Gilt für | Rendert |
|---|---|---|
| `textarea` | String | mehrzeiliges `<textarea>` |
| `password` | String | `<input type="password">` |
| `email` | String | `<input type="email">` |
| `url` | String | `<input type="url">` |
| `color` | String | `<input type="color">` |
| `slug` | String | Texteingabe mit Slug-Muster |
| `ipaddress` | String | Texteingabe mit IP-Muster |
| `json` | Json | monospaced `<textarea>` |
| `hidden` | beliebig | `<input type="hidden">` |

### Fremdschlüssel bearbeiten

FK-Spalten haben drei Bearbeitungsmodi:

- **Standard** — ein `<select>`, befüllt aus der Zieltabelle, das den `display`-Wert
  jeder Zeile anzeigt.
- **`raw_id_fields`** — eine reine ID-Eingabe plus ein Nachschlage-Link; am besten, wenn
  die Zieltabelle zu groß ist, um sie in einem Dropdown aufzuzählen.
- **`autocomplete_fields`** — eine Ajax-Vervollständigung, die die `search_fields` des
  Zielmodells über `GET /<prefix>/<target>/__autocomplete?q=…` abfragt.

### Vorbefüllte & schreibgeschützte Felder

`prepopulated_fields = "slug:title"` gibt clientseitiges JS aus, das das Quellfeld beim
Tippen in das Zielfeld sluggifiziert (kombiniere mehrere Quellen mit `+`, z. B.
`"slug:section+title"`). `readonly_fields` rendert die benannten Felder als maskierten
Text im Formular statt als Eingaben.

---

## Inlines

Inlines zeigen die Zeilen eines Kindmodells direkt auf der Seite des Elternobjekts,
sodass beides in einem Formular bearbeitet wird. Registriere eines auf Modulebene:

```rust
rustango::register_admin_inline!(
    parent = "posts",
    child  = "comments",
    fk     = "post_id",                                     // child column → parent PK
    kind   = rustango::admin::inlines::InlineKind::Tabular, // or Stacked
    label  = "Comments",
    fields = &["author_name", "body", "created_at"],
);
```

Auf der **Detail**-Seite des Elternobjekts rendern die Kinder als schreibgeschützte
Tabelle; auf der **Edit**-Seite werden sie zu einem bearbeitbaren FormSet (Zeilen
hinzufügen / ändern / löschen an Ort und Stelle). Optionen: `kind` (`Tabular` — eine
Tabellenzeile pro Kind, oder `Stacked` — eine Feldgruppe pro Kind), `label`, `fields`
(Standard: jedes Skalar außer dem FK), `extra` (leere Zeilen zum Hinzufügen angeboten),
`max_num` und `readonly_fields`.

[![Die Detailseite eines Beitrags: schreibgeschützte Felder, die Comments-Inline-Tabelle und die Prüfpfad-Karte, die den Erstellungs-Eintrag als JSON-Diff zeigt](../img/admin-detail.png)](../img/admin-detail.png)

Für Kindzeilen, die über einen generischen Fremdschlüssel (Paar aus Content-Type +
Objekt-PK) statt über eine einzelne FK-Spalte angehängt sind, verwende
`register_admin_inline_generic!(parent, child, ct = "content_type_id", pk = "object_pk",
…)` — ansonsten dieselben Optionen.

---

## Massenaktionen

Benenne die Aktionen in `admin(actions = "...")` und registriere dann einen Handler pro
Aktion am `Builder`. Der Handler erhält den Pool und die Primärschlüssel der
ausgewählten Zeilen:

```rust
use rustango::core::SqlValue;

let admin_router = admin::Builder::new(pool)
    .register_action("posts", "publish", |pool, pks| {
        Box::pin(async move {
            let ids: Vec<String> = pks.iter().filter_map(|v| match v {
                SqlValue::I64(n) => Some(n.to_string()),
                SqlValue::I32(n) => Some(n.to_string()),
                _ => None,
            }).collect();
            if !ids.is_empty() {
                let sql = format!("UPDATE posts SET status='published' WHERE id IN ({})", ids.join(","));
                rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
            }
            Ok(())
        })
    })
    .register_action("posts", "archive", /* … */)
    .build();
```

Wähle Zeilen mit den Kontrollkästchen aus, wähle die Aktion im Auswähler und sende ab
(`POST /<prefix>/<table>/__action`). `delete_selected` ist eingebaut — du registrierst es
nicht. Ein in `admin(actions = ...)` gelisteter Aktionsname ohne registrierten Handler
erscheint schlicht nicht.

Eine eigene Aktion wird wie eine Bearbeitung geprüft: `{table}.change` plus ein
Objekt-Hook, der nach der Aktion benannt ist. Eine Aktion, die löscht, sollte das mit
`register_action_with_perm(table, name, ActionPerm::Delete, handler)` sagen, dann
braucht sie `{table}.delete` und führt stattdessen den `delete`-Hook aus. Sie braucht
weiterhin `{table}.change` und wird als Bearbeitung auditiert und signalisiert.

Ein `register_action`-Handler schreibt über seine eigene Verbindung, daher werden
die Prüfpfad-Zeilen eines geprüften Modells danach geschrieben, nach bestem Bemühen.
`register_tx_action(table, name, handler)` (und `register_tx_action_with_perm`)
übergibt dem Handler stattdessen `&mut PoolTx`, sodass seine Schreibvorgänge und
ihre Prüfpfad-Zeilen gemeinsam committen oder zurückrollen. Schreibe nur über
diese Transaktion: Eine zweite Verbindung kann einen SQLite-Pool mit nur einer
Verbindung blockieren.

---

## Prüfpfad

Füge `audit(track = "field1, field2")` zu einem Modell hinzu, und jedes Erstellen,
Aktualisieren und Löschen wird in der Tabelle `rustango_audit_log` erfasst (für dich
erstellt, wenn du `migrate` ausführst). `track` wählt aus, welche Felder im Diff
erfasst werden (lasse es weg, um alle Skalare zu verfolgen). Admin-Schreibvorgänge,
Inline-Zeilen eingeschlossen, werden für jedes Modell protokolliert: bei einem
`audit(...)`-Modell in der Transaktion des Schreibvorgangs, sodass ein fehlgeschlagener
Eintrag ihn rückgängig macht; bei jedem anderen Modell nach bestem Bemühen nach dem Commit.

```rust
#[rustango(table = "posts", audit(track = "title, body, status"))]
```

Jeder Eintrag speichert die Tabelle, den Primärschlüssel, die Operation, die Quelle, ein
Diff pro Feld (`{before, after}`) als JSON, den Akteur, einen Zeitstempel und einen
manipulationssicheren Fingerabdruck. Zwei Stellen zeigen es an:

- Die **Detailseite** des Modells erhält eine **Audit trail**-Karte, die aktuelle
  Änderungen auflistet (wer, wann und das Diff), mit einem **View full history**-Link
  (im [Detail-Screenshot](#inlines) oben gezeigt).
- Die Einträge einer Zeile liegen unter dem kanonischen Text ihres Schlüssels, also
  teilen `/posts/007` und `/posts/7` eine Historie.
- Die **Activity**-Ansicht der Seitenleiste (`GET /<prefix>/__audit`) ist ein
  zeilenübergreifender Feed, neueste zuerst, mit Facetten-Karten für Entität / Operation
  / Quelle und einem Aufräumformular, um Einträge älter als N Tage zu bereinigen (was
  selbst als Prüfpfad-Eintrag erfasst wird).
  Jede Facetten-Karte zeigt bis zu 15 Werte plus den aktiven; "+N more"
  (`?facet_show_all=<col>`) listet höchstens 200, also filtere den Feed, um den
  Rest zu erreichen.

[![Der Activity-Feed: jede geprüfte Änderung über Modelle hinweg mit JSON-Diffs, Facetten-Karten nach Tabelle/Operation/Quelle und einem Aufräumformular](../img/admin-audit.png)](../img/admin-audit.png)

---

## Berechnete Spalten und benutzerdefinierte Filter

Wenn der deklarative Block nicht ausreicht, erweitern zwei Makros auf Modulebene die
Listenansicht:

**Berechnete Spalten** — eine abgeleitete Spalte, nicht aus der Datenbank:

```rust
rustango::register_admin_computed!(
    "posts", "word_count", "Words",
    |row| row.get("body").and_then(|v| v.as_str())
             .unwrap_or_default().split_whitespace().count().to_string(),
);
// then add `word_count` to admin(list_display = "...").
```

Der Closure erhält die Zeile als `serde_json::Value` und gibt vorab maskiertes HTML
zurück. Eine Form mit 5 Argumenten fügt `link = |row| Option<String>` hinzu, um die
Zelle in ein `<a>` einzuschließen.

**Benutzerdefinierte Listenfilter** — Filterlogik, die die Auto-Facetten nicht ausdrücken
können:

```rust
fn by_status(value: &str) -> Vec<rustango::core::Filter> { /* map value → predicates */ }

rustango::register_admin_list_filter!(
    "posts", "status", "Status",
    &[("draft", "Drafts"), ("published", "Published")],   // (value, label) choices
    by_status,                                            // fn(&str) -> Vec<Filter>
);
```

---

## Benutzerdefinierte Views, Querysets und Berechtigungen

Drei weitere Registrierungsmakros hängen sich in das Verhalten eines registrierten Modells ein:

- **Benutzerdefinierte Admin-Seiten** —
  `register_admin_view!("posts", "duplicate", Method::POST, "Duplicate", handler)`
  bindet eine zusätzliche Seite/Aktion unter `/<prefix>/posts/duplicate` ein. Der
  Handler ist ein asynchrones `fn(Pool, Request) -> Response`. (Reservierte Suffixe wie
  `new`, `__action`, `__autocomplete`, `{pk}`, `{pk}/edit`, `{pk}/delete` werden mit
  einer Warnung übersprungen.) Unter `with_user_perms` braucht ein GET `{table}.view`
  und ein POST/PUT/PATCH/DELETE `{table}.change`; ergänze `perm = "publish"`, um
  `{table}.publish` zu verlangen.
- **Queryset-Einschränkung** —
  `register_queryset_scope!("posts", hook)`, wobei `hook: fn(&Parts) -> Vec<Filter>`
  einschränkt, was eine Anfrage sehen kann (z. B. nur die Zeilen des aktuellen
  Benutzers). Mehrere Hooks auf einer Tabelle fügen sich zusammen. Ein Anlegen,
  Bearbeiten oder eine Inline-Zeile, deren FK auf eine Zielzeile zeigt, die deren Hooks,
  Soft Delete oder `view`-Hook verbergen, wird mit "no such object" abgelehnt.
- **Berechtigungen auf Zeilenebene** —
  `register_object_permission!("posts", "change", check)`, wobei
  `check: fn(&Parts, Option<&Value>) -> bool` pro Zeile erlaubt oder verweigert.
  Eingebaute Handler konsultieren die Aktionen `add`, `change`, `delete` und `view`;
  mehrere Hooks werden mit UND verknüpft. Ein `view`-Hook verbirgt Zeilen auch in der
  Liste, im Autocomplete sowie in FK-Facettennamen und FK-Zellnamen. Die Liste führt
  ihn über jede Zeile aus, die ihre Filter und ihre Suche treffen, sodass Summen,
  Facetten, Datums-Buckets und Suche verborgene Zeilen auslassen; ab 10 000 Zeilen
  (`Builder::view_hook_scan_cap`) hält sie sie stattdessen zurück. Ein Queryset-Hook
  erledigt das in SQL und skaliert.
- **Geheime Felder** — ein Feld mit dem Widget `password`
  (`formfield_overrides = "x: password"`) zeigt seinen Wert nie und wird nie
  durchsucht, gefiltert oder facettiert. Nichts anderes markiert ein Geheimnis: Gib
  Feldern wie `token` / `api_key` ebenfalls dieses Widget.

Für gröbere, codename-basierte Zugriffskontrolle sperrt `Builder::with_user_perms([...])`
jede Tabelle auf `{table}.view` / `.add` / `.change` / `.delete`: fehlendes `view`
verbirgt das Modell und liefert bei direkten Treffern 404, fehlendes `change` rendert es
schreibgeschützt, und fehlendes `add` / `delete` entfernt diese Schaltflächen.

---

## Authentifizierung

Standardmäßig ist das Admin **offen** — jeder, der es erreichen kann, kann es benutzen.
Sperre es auf eine von zwei Arten:

- **Session-Auth (eingebaut).** `Builder::with_session_auth(secret)` bindet
  `/login` + `/logout` (und eine optionale `/account/password`-Änderungsseite) ein und
  umgibt jede andere Route mit Middleware, die anonyme Anfragen auf das Login-Formular
  umleitet. Anmeldedaten liegen in der Tabelle `rustango_admin_users`
  (`username`, argon2 `password_hash`, `is_superuser`, `active`, `created_at`); das
  Ändern eines Passworts widerruft die anderen Sitzungen dieses Benutzers. Optionale
  TOTP-Zweifaktor-Authentifizierung ist hinter dem `totp`-Feature verfügbar, mit
  Registrierung unter `/account/totp`.

  ```rust
  let admin = admin::Builder::new(pool)
      .with_session_auth(session_secret)
      .secure_cookies(true)              // HTTPS-only cookie in production
      .build();
  ```

- **Setze deine eigene Auth davor.** Lasse das Admin offen und setze HTTP-Basic-Auth,
  OAuth2 oder unternehmensweites SSO mit deiner eigenen Middleware vor den Einhängepfad.
  Der Admin hängt CSRF nur mit Session-Auth ein, und Basic-Auth schickt der Browser
  von sich aus mit, also sind die Mutationen eines Basic-Auth-Admins cross-site
  fälschbar; siehe [Sicherheit → CSRF](security.md#schutz-vor-csrf).

Wenn die Session-Auth aktiv ist, zeigt die Fußzeile der Seitenleiste eine Zeile
**"Signed in as _username_"** und eine **Logout**-Schaltfläche (ein `POST`-Formular).
Eigenständige Admins posten standardmäßig an `{admin_prefix}/logout`; ein Mandanten-Admin
sitzt hinter der eigenen Logout-Route der Mandanten-Schicht, also verweise die
Schaltfläche dorthin mit `Builder::logout_url`:

```rust
let admin = admin::Builder::new(pool)
    .with_session_auth(session_secret)
    .logout_url("/staff-logout")       // POST target for the sidebar Logout button
    .build();
```

Der Mandanten-Admin-Builder verdrahtet dies automatisch mit seiner
`RouteConfig::logout_url`, sodass die Schaltfläche immer eine Route trifft, die
existiert.

Der Admin lehnt eine Bearbeitung ab, die **active** in der eigenen Zeile des
angemeldeten Benutzers abwählt, wie die Operator-Konsole: Bitte einen anderen
Admin darum.

Das Abwählen von **active** im Admin setzt auch `sessions_revoked_at`. Ein direkter
ORM-Schreibzugriff auf `active` tut das nicht; nutze in deinem eigenen Code
`rustango::session::deactivate::<AdminUser>(&pool, id)` (oder `Operator`, `User`),
damit eine spätere Reaktivierung kein altes Cookie wiederbelebt.

---

## Theming und Branding

| Methode | Wirkung |
|---|---|
| `.theme_mode("light" \| "dark" \| "auto")` | Standard-Farbthema (setzt `data-theme` auf `<html>`). |
| `.title(s)` / `.subtitle(s)` | Kopftext der Seitenleiste. |
| `.brand_logo_url(url)` | Über dem Titel gerendertes Logo. |
| `.brand_name(s)` / `.brand_tagline(s)` | Mandantenspezifische Überschreibungen von Titel/Untertitel. |
| `.tenant_brand_css(css)` | Ein vorgefertigter `:root{…}`-CSS-Variablen-Block, inline eingebettet für mandantenspezifische Paletten. |
| `.from_settings(pool, &settings)` | Baut Branding + Sichtbarkeit aus den Abschnitten `[admin]` / `[brand]` deiner Konfigurationsdatei; siehe unten. |

### Konfiguration: `[admin]` und `[brand]`

`Builder::from_settings(pool, &settings)` liest `[admin]` aus
`config/default.toml` oder `config/<env>_settings.toml` und greift für die
Oberfläche auf `[brand]` zurück. Die `urls.rs` eines generierten Projekts ruft es
bereits auf. `Builder`-Aufrufe danach gewinnen weiterhin.

```toml
[admin]
title            = "Acme Admin"
subtitle         = "Back office"
logo_url         = "/static/acme-logo.png"
theme_mode       = "auto"           # auto | light | dark
url_prefix       = "/admin"
allowed_tables   = []               # empty = every registered model
read_only_tables = ["orders"]
```

| Schlüssel | Standard | Rückfall auf | Was er tut |
|---|---|---|---|
| `title` | `Rustango Admin` | `[brand] name` | Überschrift der Seitenleiste und der `<title>` der Seite |
| `subtitle` | keiner | `[brand] tagline` | Zeile unter der Überschrift |
| `logo_url` | keiner: kein Logo | `[brand] logo_url` | Bild neben der Überschrift |
| `theme_mode` | `auto` | `[brand] theme_mode` | Start-Theme, das `data-theme` auf `<html>` |
| `url_prefix` | `/__admin` | `[routes] admin_url` | Präfix jedes Admin-Links. Hänge den Router unter demselben Pfad ein: `Builder::settings_prefix(&settings)` liefert ihn, und das Scaffold fällt auf `/admin` zurück |
| `allowed_tables` | `[]`, jedes Modell | | Wie `show_only` |
| `read_only_tables` | `[]` | | Wie `read_only` |
| `primary_color` | | | Noch ohne Wirkung; nur `manage check --deploy` liest es |
| `csrf_cookie_secure` | | | Noch ohne Wirkung; das CSRF-Cookie folgt `[security] secure_cookies` |
| `session_timeout_minutes` | | | Noch ohne Wirkung; der Admin hat kein Idle-Timeout |

Auch `[brand] primary_color` färbt diesen Admin nicht; es ist für die
[Operator-Konsole](operator-console.md#branding). Das Session-Cookie ist
`Secure`, außer bei `[security] secure_cookies = false`.

Die `config/dev_settings.toml` des Scaffolds setzt `[brand] tagline = "dev"`,
was hier als Untertitel erscheint; siehe
[die Taglines der Stufen](operator-console.md#die-taglines-dev-und-staging).

**Eigenes Logo.** `logo_url` geht unverändert in `<img src>`. Liefere die Datei
aus deiner App aus: Lege sie unter `assets/acme-logo.png` ab, ergänze
`.with_static("/static", "./assets")` in deiner `Cli::new()`-Kette in `main.rs`
und setze `logo_url = "/static/acme-logo.png"`. Admin und Static-Mount teilen
sich einen Host, ein Pfad funktioniert also. Eine vollständige URL zu einem CDN
funktioniert auch. `[brand] logo_path` gilt nur für die Operator-Konsole; dieser
Admin ignoriert es.

### Der Tenant-Admin

In einem Tenancy-Projekt läuft der Admin jedes Tenants auf dessen Host unter
`/admin` (`RouteConfig::admin_url`; der `Builder`-Standard `/__admin` gilt hier
nicht). Alte `/__admin`-Links erreichen ihn weiterhin, außer
`RouteConfig::legacy()` macht `/__admin` zum Hauptpfad.

Der Tenant-Admin liest weder `[admin]` noch `[brand]`. Er nimmt seine Marke aus
der `Org`-Zeile des Tenants: Der Name ist `brand_name`, sonst der Anzeigename
des Tenants; das Logo ist das auf der Bearbeitungsseite der Konsole hochgeladene,
sonst das Framework-Logo (`rustango.png` unter der Static-URL); das Theme ist
`theme_mode` und die Akzentfarbe `primary_color`. Siehe
[Branding pro Tenant](operator-console.md#branding-pro-tenant).

---

## `Builder`-Referenz

Jede Methode auf `admin::Builder` (jede gibt `Self` zur Verkettung zurück, sofern nicht
anders angegeben):

| Methode | Zweck |
|---|---|
| `new(pool)` | Aus einem beliebigen Pool konstruieren (PostgreSQL / MySQL / SQLite). Standardwerte: Präfix `/__admin`, Dev-Cookies. |
| `from_settings(pool, &settings)` | Aus geparster Konfiguration konstruieren (`config`-Feature). |
| `settings_prefix(&settings)` | Assoziierte Funktion, kein Builder-Schritt: das Mount-Präfix, das die Einstellungen verlangen (`[admin] url_prefix`, sonst `[routes] admin_url`), oder `None`. |
| `title(s)` / `subtitle(s)` | Kopf- / Unterüberschrift der Seitenleiste. |
| `admin_prefix(p)` | URL-Präfix — **muss dem Einhängepfad entsprechen**. Standard `/__admin`. |
| `audit_url(u)` | Pfad der Aktivitäts-/Prüfpfad-Ansicht. Standard `/__audit`. |
| `static_url(u)` | Präfix für eingebettete Assets (Favicon, Logo). Standard `/__static__`. Der einfache Admin liefert ein Pfad-Präfix unter seinem Mount aus. |
| `change_password_url(u)` | Pfad der Selbstbedienungs-Passwortänderungsseite (fügt den Seitenleisten-Link hinzu). |
| `show_only([tables])` | Whitelist, welche Tabellen erscheinen; andere liefern 404 und sind verborgen. |
| `read_only([tables])` | Diese Tabellen rendern, aber Erstellen/Bearbeiten/Löschen verbieten. |
| `read_only_all()` | Markiert **jede** Tabelle als schreibgeschützt. |
| `skip_count_for([tables])` | Überspringt `COUNT(*)` auf riesigen Tabellen (Pager zeigt "Page N"). |
| `view_hook_scan_cap(rows)` | Wie viele Zeilen die Liste durch `view`-Hooks prüft, bevor sie den Rest zurückhält. Standard 10 000. |
| `with_user_perms([codenames])` | Sperrt Tabellen auf `{table}.view/add/change/delete`. |
| `register_action(table, name, handler)` | Registriert einen Massenaktions-Handler. |
| `register_action_with_perm(table, name, perm, handler)` | Ebenso, aber gegen `ActionPerm::Delete` statt `change` geprüft. |
| `register_tx_action(table, name, handler)` / `register_tx_action_with_perm` | Dasselbe, aber der Handler erhält die Schreibtransaktion, sodass Prüfpfad-Zeilen mit ihr committen. |
| `with_session_auth(secret)` | Erfordert Cookie-Login (`/login` + `/logout`). |
| `logout_url(u)` | POST-Ziel für die Logout-Schaltfläche der Seitenleiste. Standard `{admin_prefix}/logout`; Mandanten-Admins setzen es auf ihre Mandanten-Logout-Route. |
| `secure_cookies(bool)` | Setzt das `Secure`-Flag (HTTPS-only) auf dem Session-Cookie. |
| `theme_mode(m)` | `"light"` / `"dark"` / `"auto"`. |
| `brand_logo_url(url)` | Logo über dem Titel. |
| `brand_favicon_url(url)` | Favicon-URL; ohne Wert bleibt das mitgelieferte Icon. |
| `brand_name(s)` / `brand_tagline(s)` | Mandantenspezifische Marken-Überschreibungen. |
| `tenant_brand_css(css)` | Mandantenspezifischer CSS-Variablen-Block. |
| `impersonated_by(operator_id)` | Rendert ein Impersonations-Banner (Operator-Konsole). |
| `tenant_mode()` | Verbirgt registry-scoped Modelle (automatisch für Mandanten-Admins gesetzt). |
| `registry_mode()` | Bedient eine Tenancy-Registry: listet `Org`, `Operator` und die anderen Registry-Tabellen, die ein einfacher Admin verbirgt. |
| `build()` | Finalisiert und gibt den `axum::Router` zurück. |

---

## Routen-Referenz

Alle Pfade sind relativ zu `admin_prefix`:

| Pfad | Methode | Was |
|---|---|---|
| `/` | GET | Startseite — Modellindex + aktuelle Aktionen. |
| `/<table>` | GET | Listenansicht (Suche, Filter, Sortierung, Paginierung). |
| `/<table>` | POST | Erstellen-Absenden. |
| `/<table>/new` | GET | Erstellungsformular. |
| `/<table>/<pk>` | GET | Detailansicht (schreibgeschützt), mit Inlines + Prüfpfad-Karte. |
| `/<table>/<pk>` | POST | Aktualisieren-Absenden. |
| `/<table>/<pk>/edit` | GET | Bearbeitungsformular. |
| `/<table>/<pk>/delete` | POST | Löschen (nach Bestätigung). Eine Zeile, auf die andere noch zeigen, antwortet mit 409: eine Seite für den Browser, sonst JSON. |
| `/<table>/__action` | POST | Führt eine Massenaktion auf ausgewählten PKs aus. |
| `/<table>/__autocomplete` | GET | FK-Vervollständigungs-JSON (`?q=`). |
| `/__docs` | GET | Modellreferenz. |
| `/__audit` (oder `audit_url`) | GET | Aktivitäts-Feed + Aufräumen. |
| `/login`, `/logout` | GET/POST | Session-Auth (wenn aktiviert). |
| `/account/password`, `/account/totp` | GET/POST | Selbstbedienungs-Passwortänderung / TOTP-Registrierung. |
| `/account/totp/disable` | POST | TOTP mit einem aktuellen Code abschalten. |

Mit `register_admin_view!` registrierte benutzerdefinierte Routen binden unter
`/<table>/<suffix>` ein.

---

## Die Modellreferenz

Jedes Admin liefert eine Live-Modellreferenz unter
`<prefix>/__docs` — ein schreibgeschützter Katalog jedes registrierten Modells mit seinen
Feldern, Spalten, Typen, Flags (PK, unique, …) und Beziehungen. Nichts zu konfigurieren;
es wird aus deinen Modellen generiert, also weicht es nie vom Schema ab.

[![Die Modellreferenz: die Felder jedes Modells mit Spaltenname, Rust-Typ, Flags und Beziehungen — aus den Modellen generiert](../img/admin-model-reference.png)](../img/admin-model-reference.png)

---

## Das Beispiel ausprobieren

```bash
cd crates/rustango/examples/admin_demo
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/admin_demo
cargo run -- migrate     # tables + the audit-log table
cargo run                # seeds demo data, serves the admin at /admin
```

Öffne dann <http://localhost:8080/admin> und klicke in **Posts** hinein, um die Filter,
Suche, Datumshierarchie, Aktionen, Feldgruppen, Inline-Kommentare, den Prüfpfad und die
Modellreferenz zu sehen — jeden Screenshot auf dieser Seite — an einem Ort.


---

## Siehe auch

- [Das ORM-Kochbuch](orm.md) — die Modelle, aus denen das Admin generiert wird (inkl. des geteilten Prüfpfads).
- [HTML-Views](html-views.md) — die generischen klassenbasierten Views, auf denen das Admin aufbaut.
- [Auth-Backends](auth-backends.md) · [Sessions](auth-sessions.md) — das Admin hinter einem Login absichern.
- [Sicherheitsleitfaden](security.md) — Härtung, bevor du es öffentlich zugänglich machst.
