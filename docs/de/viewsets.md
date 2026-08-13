# ViewSets — CRUD-REST-APIs

Ein ViewSet verwandelt ein Model in eine vollständige REST-Ressource — Endpunkte zum **Auflisten,
Erstellen, Lesen, Aktualisieren und Löschen** von Datensätzen — aus einer einzigen Deklaration. (Es ist
das **Rustango**-Äquivalent zu einem `ModelViewSet` des Django REST Framework oder einem Laravel-
API-Resource-Controller, falls du diese schon einmal verwendet hast.)

> **Neu bei REST-APIs?** Diese Anleitung setzt voraus, dass du weißt, was ein *Endpunkt*, ein *HTTP-
> Verb* (GET / POST / …) und eine *JSON-Anfrage und -Antwort* sind. Falls dir davon etwas
> unklar ist, ist das [Glossar](glossary.md#web-api-basics) eine Fünf-Minuten-Einführung —
> lies es zuerst und komm dann hierher zurück.

Kombiniere ein ViewSet mit einem [Serializer](serializers.md) — dem Baustein, der dein
JSON formt — und es schützt **beide Richtungen** auf einmal: Der Serializer formatiert jede
**Antwort** (Felder umbenennen, verbergen, berechnen oder verschachteln) *und* regelt jede
**Anfrage** (er validiert eingehende Daten und ignoriert stillschweigend Felder, die ein Client
nicht setzen dürfen sollte). Abgelehnte Eingaben kommen in der vertrauten DRF-
Form zurück — ein JSON-Objekt mit dem Feldnamen als Schlüssel. Das funktioniert überall gleich auf PostgreSQL,
MySQL und SQLite.

Diese Anleitung ist tutorial-orientiert: Wir **bauen eine vollständige REST-Blog-API** von Anfang bis Ende —
Gerüst, Models, ein Serializer, das ViewSet, alle sechs CRUD-Endpunkte, Eingabe-
validierung, Filterung/Suche/Paginierung und Tests — und der Rest der Seite
ist eine Referenz für jede Stellschraube.

[![Ein Rustango-ViewSet, verdrahtet mit einem Serializer: Ein einziger #[viewset(serializer = …)]-Block liefert typisierte JSON-Ausgabe und validierte Eingabe über die sechs CRUD-Routen hinweg](img/viewsets.png)](img/viewsets.png)

> **Quelle:** `rustango::viewset` (`ViewSet`, `#[derive(ViewSet)]`, die
> `#[viewset(...)]`-Optionen + der `for_model`-Builder) — immer kompiliert.
>
> **Lauffähige Version:** Der hier gebaute Blog spiegelt das getestete, kompilierbare
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog)-
> Beispiel (dessen `Post` / `PostSerializer` / `PostViewSet`), und jedes Verhalten ist
> durch die eigenen Live-Tests des Frameworks abgesichert — `crates/rustango/tests/viewset_*.rs`
> (insbesondere `viewset_serializer_render_sqlite_live` und
> `viewset_serializer_input_sqlite_live`).

---

## Inhaltsverzeichnis
- [API-Views vs. HTML-Views](#api-views-vs-html-views) — JSON für Clients oder HTML-Seiten?
- [Eine REST-Blog-API bauen](#build-a-rest-blog-api) — die vollständige Anleitung
- [Die Serializer-Ehe: Eingabe + Ausgabe](#the-serializer-marriage-input--output)
- [Die zwei Wege, ein ViewSet zu definieren](#the-two-ways-to-define-a-viewset)
- [Die CRUD-Endpunkte](#the-crud-endpoints) · [Auswahl, welche exponiert werden](#choosing-which-operations-to-expose)
- [`#[viewset(...)]`-Referenz](#viewset-attribute-reference) · [Builder-Referenz](#builder-reference)
- [Filterung, Suche & Sortierung](#filtering-search-and-ordering) · [Paginierung](#pagination)
- [Validierung](#validation) · [Berechtigungen & Drosselung](#permissions-and-throttling) · [Eigene Aktionen](#custom-actions-beyond-crud)
- [Einbinden](#mounting) · [Backends](#backend-support)

---

## API-Views vs. HTML-Views

Vor dem Tutorial noch eine Weggabelung. **Rustango** hat zwei Wege, ein
Model in Endpunkte zu verwandeln, und ein ViewSet ist einer davon:

- Ein **ViewSet** (diese Anleitung) ist ein **API-View** — es spricht **JSON**, für
  Frontend-Frameworks, mobile Apps und andere Dienste.
- Ein **Template-View** ([HTML-Views](html-views.md)) ist ein **HTML-View** — es
  rendert **serverseitige Seiten** über Tera, für Browser und servergerenderte
  Websites.

Darunter dasselbe Model; was sich unterscheidet, ist, was herauskommt und wer aufruft.

| | **API-View** — ViewSet (hier) | **HTML-View** — [Template-Views](html-views.md) |
|---|---|---|
| Modul | `rustango::viewset` | `rustango::template_views` |
| Sendet zurück | **JSON-Daten** | eine **servergerenderte HTML-Seite** |
| Gebaut für | SPAs, Mobile, andere Dienste | Browser, servergerenderte Websites, admin-artiges CRUD |
| Ein „Erstellen" | `POST` JSON → `201` + das Objekt | `POST` eines Formulars → `303`-Weiterleitung (Post/Redirect/Get) |
| Bei ungültiger Eingabe | `400` + eine feldbasierte JSON-Fehlerabbildung | das Formular mit angezeigten Fehlern neu rendern |
| Eine „Liste" ist | ein paginierter JSON-Umschlag | eine Schleife über Zeilen in deinem Template |
| Üblicherweise authentifiziert per | Tokens / JWT / API-Keys | Session-Cookies |
| Django-Entsprechung | DRF `ModelViewSet` | generische klassenbasierte Views |

Wähle pro Ressource — und du kannst **beide auf demselben Model** einbinden (eine öffentliche JSON-
API *und* interne CRUD-Seiten). Der Rest dieser Anleitung ist die JSON-/API-Seite; für
die HTML-Seite siehe [HTML-Views — servergerenderte Seiten](html-views.md).

---

## Eine REST-Blog-API bauen

Wir bauen einen Blog mit zwei Models — `Author` und `Post` — und exponieren `Post` als
REST-Ressource unter `/api/posts`, deren JSON-Form und Validierung von einem
Serializer gesteuert werden. Am Ende kannst du jedes CRUD-Verb per `curl` aufrufen und beobachten, wie der Serializer
die Ausgabe formt und ungültige Eingaben ablehnt.

Diese Anleitung setzt ein mit `cargo rustango new myblog` erstelltes Projekt voraus
(siehe [Erste Schritte](getting-started.md) für Projekteinrichtung und Datenbank).
Jeder Schritt ist ein echter Befehl oder eine echte Datei.

### Schritt 1 — Die Blog-App erstellen

Apps sind eigenständige Feature-Module (Djangos `startapp`):

```bash
cargo run -- startapp blog
```

Das schreibt `src/blog/{mod,models,views,urls,tests}.rs` und bindet das Modul
in `main.rs` + den `urls::api()`-Aggregator ein.

### Schritt 2 — Die Models definieren

`src/blog/models.rs` — ein `Author` und ein `Post` (ein Fremdschlüssel verknüpft sie):

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(table = "authors", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub email: String,
}

#[derive(Model, Clone, Debug)]
#[rustango(table = "posts", display = "title", index("status, published_at"))]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,                       // draft | published | archived

    #[rustango(fk = "authors", on = "id")]
    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,
}
```

### Schritt 3 — Migrieren

Migration generieren und anwenden (wie `makemigrations` + `migrate`):

```bash
cargo run -- makemigrations
cargo run -- migrate
```

### Schritt 4 — Den Serializer gerüsten

Der Serializer ist das, was daraus eine *DRF*-API macht — er definiert den Anfrage-/Antwort-
Kontrakt. Generiere das Grundgerüst:

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Dann fülle es aus. Dieses hier nutzt die gesamte Eingabe-+-Ausgabe-Oberfläche — eine Umbenennung,
ein berechnetes schreibgeschütztes Feld, ein schreibgeschütztes Server-Feld und einen Feld-Validator:

```rust
// src/blog/post_serializer.rs
use rustango::{Auto, Serializer};
use chrono::{DateTime, Utc};
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,

    #[serializer(validate = "title_min_3")]   // input: reject titles < 3 chars
    pub title: String,

    #[serializer(source = "body")]            // JSON key `content`, column `body`
    pub content: String,

    pub status: String,
    pub author_id: i64,

    #[serializer(method = "summary")]         // output: computed, never written
    pub summary: String,

    #[serializer(read_only)]                  // output: shown, ignored on write
    pub published_at: Auto<DateTime<Utc>>,
}

impl PostSerializer {
    fn title_min_3(t: &String) -> Result<(), String> {
        if t.chars().count() < 3 {
            Err("title must be at least 3 characters".into())
        } else {
            Ok(())
        }
    }
    fn summary(p: &Post) -> String {
        p.body.chars().take(80).collect::<String>()
    }
}
```

Registriere das Modul — füge `pub mod post_serializer;` zu `src/blog/mod.rs` hinzu.

Beachte, dass wir nur einen Validator geschrieben haben (`title_min_3`); die Felder **erben** zudem
automatisch **die Constraints des Models** — `title` wird gegen die Längenvorgabe des Models
`max_length = 200` geprüft, und eine `choices`/`min`/`max`-Spalte würde ebenfalls geprüft,
wobei alle beim Schreiben freundliche `400`er zurückgeben. Füge `max_length` / `min_length` /
`min` / `max` als Serializer-Attribute hinzu, um die Grenze eines Feldes zu überschreiben. (Siehe die
[Serializer-Anleitung](serializers.md#validation) für die vollständige Validierungsgeschichte.)

### Schritt 5 — Das ViewSet gerüsten und den Serializer verdrahten

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Bearbeite es, um die Ressource zu deklarieren und **den Serializer mit dem `serializer`-
Attribut zu verdrahten** — diese eine Zeile schaltet serializer-gesteuerte Ausgabe *und* Eingabe ein:

```rust
// src/blog/post_view_set.rs
use rustango::ViewSet;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    serializer    = crate::blog::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
```

Füge `pub mod post_view_set;` zu `src/blog/mod.rs` hinzu.

> Mit einem verdrahteten Serializer brauchst du kein `fields = "..."` — der Serializer ist
> die Projektion. Verwende `fields` nur, wenn du stattdessen die standardmäßige (serializer-freie)
> Feldprojektion möchtest.

### Schritt 6 — Die Routen einbinden

In einem Single-Tenant-Projekt verschachtelst du den Router des ViewSets unter einem Pfad und übergibst
den Pool:

```rust
// src/blog/urls.rs (or your urls::api aggregator)
use axum::Router;
use rustango::sql::sqlx::PgPool;
use crate::blog::post_view_set::PostViewSet;

pub fn api(pool: PgPool) -> Router {
    Router::new()
        .merge(PostViewSet::router("/api/posts", pool))
}
```

`make:api_routes blog` gerüstet genau diesen Aggregator, falls du ihn lieber
generieren möchtest. Binde `blog::urls::api(pool)` in deine oberste `urls.rs` ein.

### Schritt 7 — Ausführen und jeden Endpunkt durchprobieren

```bash
cargo run            # listening on http://0.0.0.0:8080
```

**Erstellen** (`POST`). Der Serializer validiert zuerst und schreibt dann nur die
Felder, die er akzeptiert:

```bash
# happy path — note `content` (the renamed `body`) on the way in
curl -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"Hello Rustango","content":"First post body.","status":"published","author_id":1}'
```
```json
{
  "id": 1,
  "title": "Hello Rustango",
  "content": "First post body.",
  "status": "published",
  "author_id": 1,
  "summary": "First post body.",
  "published_at": "2026-01-02T12:00:00Z"
}
```
Die Antwort hat die Form des **Serializers**: `body` kam als `content` zurück, das
berechnete `summary` erschien, und `published_at` (schreibgeschützt, servergesetzt) ist
vorhanden.

**Die Validierung lehnt ungültige Eingaben** mit einem `400` in DRF-Form ab — feldbasierte Arrays von
Meldungen:

```bash
curl -i -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"hi","content":"x","author_id":1}'
# HTTP/1.1 400 Bad Request
# {"title":["title must be at least 3 characters"]}
```

**Schreibgeschützte / berechnete Felder, die ein Client postet, werden ignoriert** — sie können weder
`published_at` noch `summary` unterschieben:

```bash
curl -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"Sneaky","content":"x","author_id":1,"published_at":"1999-01-01T00:00:00Z","summary":"hax"}'
# → published_at is the server value, not 1999; summary is recomputed from body.
```

**Auflisten** (`GET`) — paginiert, jede Zeile in der Form des Serializers:

```bash
curl localhost:8080/api/posts
```
```json
{ "count": 1, "page": 1, "page_size": 20, "last_page": 1, "results": [ { "id": 1, "title": "Hello Rustango", … } ] }
```

**Abrufen / Aktualisieren / Teilaktualisieren / Löschen:**

```bash
curl localhost:8080/api/posts/1                       # retrieve  → 200
curl -X PUT   localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Edited","content":"new body","status":"published","author_id":1}'   # full update → 200
curl -X PATCH localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Just the title"}'                   # partial update → 200 (other fields untouched)
curl -X DELETE localhost:8080/api/posts/1              # destroy → 204
```

Die `PATCH`-Validierung läuft über das, was du sendest; schreibgeschützte Felder behalten ihren Server-
Wert, selbst wenn sie gepostet werden.

### Schritt 8 — Filtern, suchen, sortieren, paginieren

Alles am Listen-Endpunkt, ohne zusätzlichen Code (du hast die Felder in Schritt 5 deklariert):

```bash
curl 'localhost:8080/api/posts?status=published&author_id=1'      # filter
curl 'localhost:8080/api/posts?status__in=published,archived'     # lookup
curl 'localhost:8080/api/posts?search=rustango'                   # search title+body
curl 'localhost:8080/api/posts?ordering=title'                    # sort (asc)
curl 'localhost:8080/api/posts?page=2&page_size=10'               # paginate
```

### Schritt 9 — Testen

Das Framework liefert einen In-Process-Testclient — prüfe echte HTTP-Antworten
ab, ohne einen Server hochzufahren:

```rust
// tests/post_api.rs
use rustango::test_client::TestClient;
use myblog::blog::post_view_set::PostViewSet;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn rejects_short_title() {
    let client = TestClient::new(app().await);
    let res = client.post("/api/posts")
        .json(&json!({"title":"hi","content":"x","author_id":1}))
        .send().await;
    assert_eq!(res.status, 400);
    assert!(res.json_value()["title"].is_array());   // DRF field-error shape
}

#[tokio::test]
async fn create_then_list() {
    let client = TestClient::new(app().await);
    let created = client.post("/api/posts")
        .json(&json!({"title":"Hello","content":"b","status":"published","author_id":1}))
        .send().await;
    assert_eq!(created.status, 201);
    let list = client.get("/api/posts").send().await;
    assert!(list.json_value()["results"].is_array());
}
```

```bash
cargo test --test post_api
```

Das ist eine vollständige, validierte REST-Ressource. Der Rest dieser Seite ist die
Referenz hinter jedem Schritt.

---

## Die Serializer-Ehe: Eingabe + Ausgabe

Das Verdrahten eines Serializers (über `serializer = …` an der Derive oder `.serializer::<S>()`
am Builder) verändert **beide** Richtungen. Es funktioniert auf PostgreSQL, MySQL und
SQLite gleichermaßen.

### Ausgabe — Antworten werden durch den Serializer gerendert

`list`-, `retrieve`-, `create`- und `update`-Antworten werden durch
`S::from_model(&row)` erzeugt, sodass die Overrides des Serializers das JSON formen:

| Serializer-Feld | Wirkung auf die Antwort |
|---|---|
| `#[serializer(source = "body")]` | Spalte `body` wird unter dem Namen des Feldes ausgegeben (z. B. `content`) |
| `#[serializer(method = "fn")]` | ein berechnetes Feld erscheint (aus `Self::fn(&model)`) |
| `#[serializer(read_only)]` | in der Ausgabe enthalten |
| `#[serializer(write_only)]` | **weggelassen** aus der Ausgabe |

> **`nested` / `many`-Vorbehalt.** Verschachtelte und Sammlungs-Serializer-Felder werden
> nur gerendert, wenn die verwandten Zeilen geladen wurden (über `select_related` / ein Eager-
> Fetch); andernfalls fallen sie auf ihren Standardwert zurück. Die automatische ViewSet-Listen-
> Abfrage lädt die Basiszeile — verdrahte Beziehungen explizit, wenn ein verschachteltes Feld befüllt
> werden muss.

### Eingabe — Anfragen werden validiert und gefiltert

Bei `create` und `update`, wenn ein Serializer registriert ist:

1. **Die Validierung läuft.** Das `validate()` des Serializers — jedes einzelne
   `#[serializer(validate = "fn")]` pro Feld plus das feldübergreifende `validate` auf Container-
   Ebene — läuft gegen den JSON-Body. Bei Fehlschlag wird die Anfrage abgelehnt
   mit `400 Bad Request` in der DRF-Fehlerform: ein JSON-Objekt mit dem Feldnamen als Schlüssel
   und Arrays von Meldungen, z. B. `{"title":["title must be at least 3 characters"]}`.
2. **Filterung schreibbarer Felder.** Nur die schreibbaren Felder des Serializers werden
   gespeichert; `read_only`- und `method`-/berechnete Felder, die ein Client postet, werden
   **ignoriert** (nicht geschrieben), und `source`-Umbenennungen werden auf die Model-
   Spalte aufgelöst. So kann ein Client kein servergesteuertes Feld setzen, indem er es in den
   Body einschließt.

> **Form-urlencoded-Bodies** (vs. JSON) überspringen `validate()` — es gibt keinen typisierten
> Wert zu validieren — bekommen aber trotzdem die Filterung schreibbarer Felder.

Unter der Haube sind dies die Methoden `validate()`, `writable_source_fields()`
und `from_writable_json()` des `ModelSerializer`-Traits, alle generiert von
`#[derive(Serializer)]`. Siehe die [Serializer-Anleitung](serializers.md) dazu, wie man
die Validatoren schreibt.

---

## Die zwei Wege, ein ViewSet zu definieren

Beide erzeugen einen `axum::Router` mit denselben CRUD-Routen.

**1. Das Derive-Makro** — deklarativ, Single-Tenant; verdrahte einen Serializer mit
`serializer = …`:

```rust
#[derive(ViewSet)]
#[viewset(
    model         = Post,
    serializer    = crate::blog::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;

let router = PostViewSet::router("/api/posts", pool);
```

**2. Der Builder** — `ViewSet::for_model(...)`, programmatisch, tri-dialektfähig
(PostgreSQL / SQLite / MySQL) und mandantenfähig; verdrahte einen Serializer mit
`.serializer::<S>()`:

```rust
use rustango::viewset::ViewSet;
use rustango::core::Model as _;

let router = ViewSet::for_model(Post::SCHEMA)
    .serializer::<PostSerializer>()
    .filter_fields(&["author_id", "status"])
    .search_fields(&["title", "body"])
    .ordering(&[("published_at", true)])    // true = DESC
    .page_size(20)
    .router_pool("/api/posts", pool);       // tri-dialect Pool
```

Greife zum Builder, wenn du SQLite/MySQL, Mandantenfähigkeit, eine zur Laufzeit erstellte
Konfiguration oder die Extras (Drosselung, eigene Filter-Backends, Cursor-Paginierung) brauchst.

---

## Die CRUD-Endpunkte

Das Einbinden unter `/api/posts` verdrahtet alle sechs REST-Operationen:

| Verb | Pfad | Aktion | Erfolg | Body |
|---|---|---|---|---|
| `GET` | `/api/posts` | **list** | 200 | paginierter Umschlag (siehe [Paginierung](#pagination)) |
| `POST` | `/api/posts` | **create** | 201 | das erstellte Objekt — *oder ein Array bei Bulk-Create* |
| `GET` | `/api/posts/{pk}` | **retrieve** | 200 | das Objekt |
| `PUT` | `/api/posts/{pk}` | **update** (vollständig) | 200 | das aktualisierte Objekt |
| `PATCH` | `/api/posts/{pk}` | **partial update** | 200 | das aktualisierte Objekt (nur gelieferte Felder ändern sich) |
| `DELETE` | `/api/posts/{pk}` | **destroy** | 204 | leer |

Ein abschließender Schrägstrich am Mount-Präfix ist optional. Nur diese sechs Verben werden
verdrahtet — kein automatisches `HEAD`/`OPTIONS`. **Bulk-Create** gibt es gratis: `POST` ein JSON-
*Array*, und jedes Element wird der Reihe nach eingefügt, atomar validiert (ein fehlerhaftes
Element lehnt die ganze Charge ab).

---

## Auswahl, welche Operationen exponiert werden

Für eine **schreibgeschützte** Ressource (nur list + retrieve) füge `read_only` hinzu:

```rust
#[viewset(model = Post, read_only)]            // macro
ViewSet::for_model(Post::SCHEMA).read_only()   // builder
```

Es gibt keinen Umschalter pro Verb außer read_only. Für „alles außer Löschen"
binde das ViewSet ein und überschreibe die eine Route mit deinem eigenen Handler (siehe
[Eigene Aktionen](#custom-actions-beyond-crud)).

---

## `#[viewset(...)]`-Attributreferenz

| Schlüssel | Beispiel | Standard | Was er tut |
|---|---|---|---|
| `model` | `model = Post` | **erforderlich** | Das Model, auf dem die Ressource aufbaut. |
| `serializer` | `serializer = path::To::S` | keiner | Einen Serializer für typisierte **Ausgabe + Eingabe** verdrahten (siehe [oben](#the-serializer-marriage-input--output)). |
| `fields` | `"id, title, body"` | alle Skalarfelder | Whitelist für die standardmäßige (serializer-freie) Projektion + schreibbare Felder. |
| `filter_fields` | `"author_id, status"` | keiner | Über `?field=value` filterbare Felder (+ Lookups). |
| `search_fields` | `"title, body"` | keiner | Felder, die die `?search=`-Box durchsucht (Groß-/Kleinschreibung-unabhängiges ODER). |
| `ordering` | `"-published_at, id"` | keiner | Standardsortierung (`-` = DESC). |
| `page_size` | `20` | 20 | Zeilen pro Seite (Client-`?page_size=` gedeckelt bei 1000). |
| `read_only` | *(Flag)* | aus | Nur GET (list + retrieve) exponieren. |
| `permissions(...)` | `permissions(create = "post.add")` | keiner | Berechtigungs-Codenamen pro Aktion. |

---

## Builder-Referenz

Jede Methode auf `ViewSet::for_model(SCHEMA)` (jede gibt `Self` zurück):

| Methode | Zweck |
|---|---|
| `serializer::<S>()` | Einen Serializer für typisierte Ausgabe + Eingabe verdrahten (tri-dialektfähig). |
| `fields(&["…"])` | Standardprojektion + Whitelist schreibbarer Felder (wenn kein Serializer). |
| `filter_fields(&["…"])` | `?field=value`-Filterung aktivieren. |
| `search_fields(&["…"])` | `?search=` aktivieren. |
| `ordering(&[("field", desc)])` | Standardsortierreihenfolge. |
| `ordering_fields(&["…"])` | Festlegen, welche Felder `?ordering=` verwenden darf. |
| `page_size(n)` | Standard-Seitengröße (≤ 1000). |
| `read_only()` | Nur GET. |
| `permissions(ViewSetPerms{…})` / `permissions_for_model::<T>()` | Codename-Gates pro Aktion (letzteres bei Mandantenfähigkeit). |
| `cursor_pagination("id")` / `cursor_pagination_desc("id")` | Keyset-Paginierung (überspringt `COUNT(*)`). |
| `limit_offset_pagination()` | `?limit=&offset=`-Fensterung. |
| `pagination(PaginationStyle::…)` | Den Stil explizit setzen. |
| `filter_backend(closure)` | Eigene `WHERE`-Prädikate über `filter_fields` hinaus hinzufügen. |
| `throttle(…)` / `throttle_all(max, secs)` | Rate-Limits pro Aktion mit festem Zeitfenster. |
| `router(prefix, pgpool)` | Einbinden (Postgres, statischer Pool). |
| `router_pool(prefix, pool)` | Tri-dialektfähig einbinden (PG / SQLite / MySQL). |
| `tenant_router(prefix)` | *(Mandantenfähigkeit)* mit Mandantenauflösung pro Anfrage einbinden. |

---

## Filterung, Suche und Sortierung

Alles gesteuert über Query-Parameter am **Listen**-Endpunkt.

**Filterung** — jeder `filter_fields`-Eintrag akzeptiert `?field=value` (exakt) plus
Django-artige Lookups über ein `__suffix`:

```
?status=published
?author_id__in=1,2,3
?published_at__gte=2026-01-01
?title__icontains=rust
?body__isnull=false
```

Unterstützte Lookups: `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `not_in`, `contains`,
`icontains`, `startswith`, `istartswith`, `endswith`, `iendswith`, `isnull`
(kein Suffix = exakt). Felder, die nicht in `filter_fields` stehen, werden ignoriert.

**Suche** — `?search=term` durchsucht `search_fields` mit einem Groß-/Kleinschreibung-unabhängigen ODER.

**Sortierung** — `?ordering=field,-other` (`-` = DESC). Jedes Feld ist sortierbar,
sofern du es nicht mit `.ordering_fields([...])` einschränkst. Ohne Parameter gilt der
`ordering`-Standard. Alle lassen sich kombinieren.

---

## Paginierung

> **Fallstrick — paginiere über eine deterministische Reihenfolge.** Seitenzahl- und
> Limit/Offset-Paginierung setzen eine stabile Sortierung voraus; das Sortieren über eine nicht-eindeutige Spalte
> (oder gar keine) lässt Zeilen zwischen den Seiten verrutschen — dupliziert oder übersprungen. Füge immer
> einen eindeutigen Tiebreaker hinzu, z. B. `ordering = "-published_at, id"`. (Beide führen zudem
> `COUNT(*)` pro Aufruf aus; Cursor-Paginierung überspringt das bei großen Tabellen.)

Drei Stile; Seitenzahl ist der Standard. Der Listen-Umschlag unterscheidet sich je Stil:

**Seitenzahl** (Standard) — `?page=2&page_size=20`:

```json
{ "count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ] }
```

**Cursor** — `.cursor_pagination("id")` (oder `_desc`); überspringt `COUNT(*)`, ideal
für sehr große Tabellen. `?cursor=<token>&page_size=20`:

```json
{ "page_size": 20, "next": "<opaque-cursor-or-null>", "results": [ … ] }
```

**Limit/Offset** — `.limit_offset_pagination()`. `?limit=20&offset=40`:

```json
{ "count": 137, "limit": 20, "offset": 40, "results": [ … ] }
```

`page_size` / `limit` werden auf 1000 begrenzt.

---

## Validierung

Mit einem **verdrahteten Serializer** führt der Create-/Update-Pfad die Validatoren des Serializers
aus und gibt `400`er in DRF-Form zurück — der empfohlene Weg zu validieren (siehe
[die Ehe](#the-serializer-marriage-input--output) und die
[Serializer-Anleitung](serializers.md#validation)). Drei Schichten laufen:

- **Deklarative Constraints** — `max_length` / `min_length` / `min` / `max`, und
  standardmäßig **erbt** das Feld das `max_length` / `min` / `max` /
  `choices` **des Models**. So wird eine `#[rustango(max_length = 200)]`-Spalte an der
  API längengeprüft, ohne zusätzliche Konfiguration (Verhalten des DRF-`ModelSerializer`), wodurch
  potenzielle `500`er aus DB-Constraints in freundliche `400`er verwandelt werden wie
  `{"title":["Ensure this value has at most 200 characters."]}`.
- **Pro-Feld-** `validate = "fn"` und ein **feldübergreifender** `validate`-Hook — deine
  eigenen Regeln (Formate, feldübergreifend, Geschäftslogik).

Unabhängig von einem Serializer erzwingt der Schreibpfad immer das **Schema**:

- **Typen werden konvertiert und geprüft** — ein ungültiger `i64`- / `DateTime`- / `Uuid`- / `bool`-
  Wert ergibt einen `400`, der das Feld nennt.
- **Erforderlich / NOT NULL** — ein fehlendes nicht-nullbares Feld (oder ein leerer String für einen
  nicht-nullbaren `String`) ergibt einen `400`; nullbare Felder akzeptieren leer → `NULL`.
- **Datenbank-Constraints** — Unique, Fremdschlüssel und Check-Constraints tauchen
  als `400` beim INSERT/UPDATE auf.

Also bekommst du selbst ohne Serializer Typ- + Erforderlich- + DB-Constraint-Validierung;
verdrahte einen Serializer, um deklarative Längen-/Bereichs-/Auswahl-Prüfungen (automatisch geerbt)
plus deine eigenen Pro-Feld- und feldübergreifenden Regeln zu erhalten.

---

## Berechtigungen und Drosselung

> **Ein ViewSet ist standardmäßig öffentlich.** Das Einbinden eines solchen exponiert alle sechs CRUD-Verben
> für jedermann — es gibt keine eingebaute Authentifizierung. Sichere es mit `permissions(...)`
> (unten), stelle es hinter die [Auth-Middleware](auth-backends.md) (`require_auth`)
> oder beides, bevor du Schreibzugriffe exponierst.

**Berechtigungen** sichern jede Aktion über Codenamen (ODER innerhalb einer Aktion):

```rust
use rustango::viewset::{ViewSet, ViewSetPerms};

ViewSet::for_model(Post::SCHEMA)
    .permissions(ViewSetPerms {
        list:     vec!["post.view".into()],
        retrieve: vec!["post.view".into()],
        create:   vec!["post.add".into()],
        update:   vec!["post.change".into()],
        destroy:  vec!["post.delete".into()],
    })
    .router_pool("/api/posts", pool);
```

Eine leere Aktionsliste = keine Prüfung. Die Durchsetzung liest einen authentifizierten Benutzer aus
der Anfrage (die `tenancy`-Auth-Integration); Superuser umgehen sie, ein fehlender Benutzer
wird abgelehnt. `.permissions_for_model::<Post>()` füllt automatisch die Standard-
Codenamen `post.view`/`add`/`change`/`delete`.

**Drosselung** wendet Limits pro Client mit festem Zeitfenster an, pro Aktion:

```rust
ViewSet::for_model(Post::SCHEMA)
    .throttle_all(60, 60)              // 60 requests / 60s per client, every action
    .router_pool("/api/posts", pool);
```

Über dem Limit → `429 Too Many Requests` + `Retry-After`. Die Zähler sind pro Prozess;
der Client-Schlüssel ist die Verbindungs-IP (oder `X-Forwarded-For` / `X-Real-IP`).

---

## Eigene Aktionen jenseits von CRUD

Es gibt keinen DRF-`@action`-Decorator — das ViewSet ist strikt auf die sechs CRUD-
Routen beschränkt. Für zusätzliche Endpunkte binde deine eigenen Handler neben dem ViewSet ein:

```rust
use axum::{Router, routing::{get, post}};

let api = Router::new()
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    .route("/api/posts/stats", get(post_stats))
    .route("/api/posts/bulk_archive", post(bulk_archive));
```

Für zusätzliche `WHERE`-Logik trägt `.filter_backend(…)` Prädikate ohne eine
separate Route bei.

### Zeilen auf den authentifizierten Principal beschränken

Ein Backend läuft bei **jeder** Aktion — `list`, `retrieve`, `update`, `destroy` —
verhält sich also wie DRFs `get_queryset()`. Eine vom Backend ausgeschlossene Zeile ergibt einen
**404** auf den Item-Routen, keinen 403: Ein 403 würde bestätigen, dass die ID existiert.

Die Identität muss aus der Credential kommen, niemals aus dem Query-String. Ein
`?owner_id=`-Filter ist kein Scope — er ist ein Parameter, den der Aufrufer wählt.

#### `OwnedBy` — das mitgelieferte Backend

Die meisten besitzgebundenen Ressourcen brauchen genau eine Regel: *Zeilen, deren Besitzspalte der
Aufrufer ist*. Benenne die Spalte und binde es ein.

```rust
use rustango::viewset::{OwnedBy, ViewSet};

ViewSet::for_model(Note::SCHEMA)
    .filter_backend(OwnedBy::column("member_id"))
    .tenant_router("/api/notes")
    .layer(axum::middleware::from_fn(
        rustango::tenancy::auth_routes::require_bearer,
    ))
```

Jede Spalte funktioniert — `owner_id`, `member_id`, `author_id` — weil das Backend
den Namen entgegennimmt, statt eine Konvention anzunehmen. Es scheitert geschlossen bei den beiden
Arten, wie es falsch sein kann: Eine unauthentifizierte Anfrage und eine Spalte, die das Model nicht
hat, treffen beide auf **nichts**, sodass ein Tippfehler beim Einbinden nicht zu „keine
Prädikate, gib die Tabelle zurück" werden kann.

Superuser sind standardmäßig nicht besonders; `.superuser_sees_all()` schaltet das frei, denn
„Admins sehen alles" ist eine Produktentscheidung, keine des Frameworks.

#### Woher die Identität kommt

[`Principal`] ist der eine Identitätstyp, aufgelöst aus dem, was die Anfrage
verifiziert hat — ein expliziter `Principal`, ein von einer Session- oder Bearer-Middleware hinterlassener
`AuthenticatedUser` oder ein MCP-Agent-Token (der als der Benutzer agiert, der ihn ausgestellt hat).
Er authentifiziert selbst nichts; er liest nur, was eine verifizierende Middleware
bereits nachgewiesen hat, sodass niemand einen einfügen darf, ohne zuerst eine Credential zu prüfen.

`require_bearer` ist diese Middleware für eine JSON-API: Sie verifiziert das Access-Token
gegen den aufgelösten Mandanten, liest die Benutzerzeile erneut (ein deaktiviertes Konto hört
bei der nächsten Anfrage auf zu funktionieren, nicht erst wenn das Token abläuft), und fügt sowohl
`AuthenticatedUser` als auch `Principal` ein. Verwende sie überall als Extraktor:

```rust
use rustango::tenancy::{OptionalPrincipal, Principal};

async fn mine(principal: Principal) -> String {          // 401 when absent
    format!("user {}", principal.user_id)
}

async fn home(OptionalPrincipal(who): OptionalPrincipal) -> String {
    who.map_or("anonymous".into(), |p| format!("user {}", p.user_id))
}
```

#### Ein eigenes Backend schreiben

Wenn Besitz keine einzelne Spalte ist — ein geteiltes Team, eine soft-gelöschte Zeile, ein
Zeitfenster von Daten — implementiere das Trait und überschreibe `filter_with`, das
die Anfrage-`Parts` erhält:

```rust
use axum::http::request::Parts;
use rustango::tenancy::Principal;
use rustango::viewset::ViewSetFilter;

struct OwnerFilter;

impl ViewSetFilter for OwnerFilter {
    // No principal in hand — fail closed. Returning no predicates here would
    // widen the query to every row in the table.
    fn filter(&self, _p: &HashMap<String, String>, schema: &'static ModelSchema) -> Vec<WhereExpr> {
        deny_all(schema)
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _p: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        let Some(principal) = Principal::from_parts(parts) else {
            return deny_all(schema);
        };
        vec![WhereExpr::Predicate(Filter {
            column: schema.field("owner_id").expect("owner_id").column,
            op: Op::Eq,
            value: SqlValue::from(principal.user_id),
        })]
    }
}

ViewSet::for_model(Note::SCHEMA)
    .filter_backend(OwnerFilter)
    .tenant_router("/api/notes")
```

`filter_with` fällt standardmäßig auf `filter` zurück, sodass ein Backend, das die Anfrage nicht braucht
— einschließlich der schlichten Closure-Form — nur `filter` wie zuvor implementiert.

---

## Einbinden

Setze den Router des ViewSets in deine App ein. Single-Tenant, statischer Pool:

```rust
let api = urls::api()
    .merge(PostViewSet::router("/api/posts", pool.clone()))                          // macro
    .merge(ViewSet::for_model(Author::SCHEMA).router_pool("/api/authors", pool.clone())); // builder
```

Multi-Tenant (kein Pool erfasst — jede Anfrage löst ihre Mandantenverbindung auf):

```rust
let api = urls::api()
    .merge(ViewSet::for_model(Post::SCHEMA).tenant_router("/api/posts"));
```

`make:api_routes <app>` generiert eine `api()` pro App, die diese
`.merge(...)`-Zeilen sammelt; binde sie in deine oberste `urls.rs` ein.

---

## Backend-Unterstützung

- **Builder + `router_pool` / `tenant_router`** ist **tri-dialektfähig** — PostgreSQL,
  SQLite und MySQL — und ist der empfohlene Weg.
- **Das `router(prefix, PgPool)` des Derive-Makros** erfasst einen `PgPool` (PostgreSQL).
- **Serializer-Eingabe + -Ausgabe** funktioniert jetzt auf **allen drei Backends** (das
  Rendern pro Zeile ist tri-dialektfähig; das alte PG-only-Gate ist weg).
- Filterung, Suche, Sortierung, die drei Paginierungsmodi, Berechtigungen,
  Drosselung und Bulk-Create funktionieren alle über die unterstützten Backends hinweg auf dem
  Builder-Pfad.

---

## Probier es aus

Der obige End-to-End-Ablauf spiegelt das kompilierbare `getting_started_blog`-Beispiel
(Schritte 12–13 der [Erste-Schritte-Anleitung](getting-started.md)). Die
eigenen Live-Tests des Frameworks unter `crates/rustango/tests/viewset_*.rs` sind die
vollständigste lauffähige Referenz — einschließlich der Serializer-Eingabe-/Ausgabe-Tests.
Sie laufen auf In-Memory-SQLite, brauchen aber die passenden Feature-Flags, z. B.:

```bash
cd crates/rustango
cargo test --features sqlite,tenancy --test viewset_serializer_render_sqlite_live
cargo test --features sqlite,tenancy --test viewset_serializer_input_sqlite_live
cargo test --features sqlite,tenancy --test viewset_sqlite_live
```

---

## Siehe auch

- [Serializer](serializers.md) — forme das JSON, das ein ViewSet sendet und validiert.
- [HTML-Views](html-views.md) — das servergerenderte Gegenstück zu dieser JSON-API.
- [OpenAPI](openapi.md) — generiere eine Spezifikation + Swagger-UI aus deinen ViewSets.
- [URLs & Routing](urls.md) — setze ViewSet-Router in deine App ein.
