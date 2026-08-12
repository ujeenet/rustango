# HTML-Views — serverseitig gerenderte Seiten

Ein HTML-View verwandelt ein Modell in **serverseitig gerenderte Webseiten** — eine
Listenseite, eine Detailseite sowie Erstellungs-/Bearbeitungs-/Löschformulare — aus einer
einzigen Deklaration. Es ist das **Gegenstück zu [ViewSets](viewsets.md)**: Während ein ViewSet
JSON für API-Clients ausgibt, gibt ein HTML-View eine gerenderte Seite für einen Browser aus. Beide
werden aus demselben `#[derive(Model)]` gebaut, und du kannst ein Modell *beides* zugleich
ausliefern.

Dies sind das Äquivalent von **Rustango** zu Djangos generischen klassenbasierten Views
(`ListView`, `DetailView`, `CreateView`, `UpdateView`, `DeleteView`) oder Laravels
Resource-Controllern, die Blade-Views zurückgeben. Sie rendern über [Tera](https://keats.github.io/tera/)-Templates.

[![HTML-Views in Rustango: Ein Modell speist ListView, DetailView und CreateView/UpdateView/DeleteView, die jeweils ein Tera-Template zu einer serverseitig gerenderten Seite rendern](img/html-views.png)](img/html-views.png)

> **Neu bei einem Begriff hier?** Falls *Modell*, *Template*, *Router* oder *serverseitig gerendert*
> unbekannt sind, erklärt das [Glossar](glossary.md) jeden in einfacher Sprache.

> **Quelle:** `rustango::template_views` (`ListView`, `DetailView`, `CreateView`,
> `UpdateView`, `DeleteView`, `TemplateView`, `RedirectView`) — hinter dem
> `template_views`-Feature (standardmäßig aktiviert).
>
> **Lauffähige Version:** Das API-vs-HTML-Beispiel unten ist durch den
> Framework-Test
> [`html_and_api_contrast_sqlite_live.rs`](../crates/rustango/tests/html_and_api_contrast_sqlite_live.rs)
> festgeschrieben (`cargo test -p rustango --features sqlite --test html_and_api_contrast_sqlite_live`).
> Die einzelnen Views werden von `template_view.rs` und
> `template_views_context_object_name_sqlite_live.rs` abgedeckt.

## Inhaltsverzeichnis

- [API-Views vs HTML-Views — welche willst du?](#api-views-vs-html-views--which-do-you-want)
- [Die fünf Modell-Views](#the-five-model-views)
- [ListView](#listview) · [DetailView](#detailview)
- [CreateView, UpdateView, DeleteView](#createview-updateview-deleteview)
- [Der Tera-Kontext](#the-tera-context)
- [TemplateView und RedirectView](#templateview-and-redirectview)
- [Single-Tenant vs Multi-Tenant](#single-tenant-vs-multi-tenant)
- [Ein Modell auf beide Arten ausliefern](#serving-one-model-both-ways)
- [Siehe auch](#see-also)

---

## API-Views vs HTML-Views — welche willst du?

Das ist die erste Entscheidung. Beide verwandeln ein Modell in Endpunkte; sie unterscheiden sich
darin, *was herauskommt* und *wer aufruft*.

| | **API-View** — [ViewSet](viewsets.md) | **HTML-View** — dieser Leitfaden |
|---|---|---|
| Modul | `rustango::viewset` | `rustango::template_views` |
| Gibt zurück | **JSON-Daten** | eine **serverseitig gerenderte HTML-Seite** |
| Gebaut für | SPAs, Mobile-Apps, andere Services | Browser, serverseitig gerenderte Sites, CRUD im Admin-Stil |
| Ein „Erstellen" | `POST` JSON → `201` + das neue Objekt | `POST` eines Formulars → `303`-Redirect zu einer Erfolgsseite |
| Bei fehlerhafter Eingabe | `400` + eine feldbasierte JSON-Fehlerkarte | rendert das Formular mit den angezeigten Fehlern neu |
| Liest eine Liste als | eine paginierte JSON-Hülle | eine `<table>`/Schleife in deinem Template |
| Üblicherweise authentifiziert per | Tokens / JWT / API-Keys | Session-Cookies |
| Django-Analogon | DRF `ModelViewSet` | generische klassenbasierte Views |

Du musst nicht global wählen — wähle pro Ressource, und du kannst **beide auf demselben Modell**
einhängen (siehe [unten](#serving-one-model-both-ways)). Faustregeln:

- Du baust ein **JSON-Backend** für ein Frontend-Framework oder eine Mobile-App → ViewSet.
- Du baust eine **serverseitig gerenderte Site** (der Server gibt HTML-Seiten zurück) → HTML-Views.
- Du brauchst beides (eine öffentliche API *und* interne CRUD-Seiten) → hänge beide ein.

> Suchst du die JSON-Seite? Sie hat ihre eigene Vertiefung: [ViewSets — CRUD-REST-APIs](viewsets.md).

---

## Die fünf Modell-Views

Jeder View ist `for_model(SCHEMA)` plus ein `.router(prefix, tera, pool)`. Sie am selben
`prefix` (sagen wir `/posts`) einzuhängen, ergibt den klassischen CRUD-URL-Satz:

| View | Rendert | Eingehängte Routen | Standard-Template |
|---|---|---|---|
| [`ListView`](#listview) | eine paginierte Liste | `GET <prefix>` | `<table>_list.html` |
| [`DetailView`](#detailview) | eine Zeile | `GET <prefix>/{pk}` | `<table>_detail.html` |
| [`CreateView`](#createview-updateview-deleteview) | ein Formular für einen neuen Datensatz | `GET`/`POST <prefix>/new` | `<table>_form.html` |
| [`UpdateView`](#createview-updateview-deleteview) | ein vorausgefülltes Bearbeitungsformular | `GET`/`POST <prefix>/{pk}/edit` | `<table>_form.html` |
| [`DeleteView`](#createview-updateview-deleteview) | eine Bestätigungsseite | `GET`/`POST <prefix>/{pk}/delete` | `<table>_confirm_delete.html` |

`<table>` ist der Tabellenname des Modells, ein `Post` (Tabelle `posts`) sucht also nach
`posts_list.html`, `posts_detail.html` und so weiter. Überschreibe jedes davon mit
`.template("my_name.html")`.

---

## ListView

Eine paginierte Listenseite. Du stellst ein Template bereit, das über `object_list` iteriert;
der View übernimmt Paginierung, Sortierung, Filterung und Suche aus den Query-Parametern.

```rust
use rustango::template_views::ListView;
use std::sync::Arc;
use tera::Tera;

let app = ListView::for_model(Post::SCHEMA)
    .page_size(20)                       // rows per page (?page=N to navigate)
    .order_by("published_at", true)      // default sort, true = DESC
    .filter_fields(&["status", "author_id"])  // ?status=published
    .search_fields(&["title", "body"])        // ?search=rust
    .router("/posts", Arc::new(tera), pool);
```

Ein passendes `posts_list.html` — beachte `object_list` und die Paginierungsvariablen,
die der View für dich einstempelt:

```html
<h1>Posts ({{ total }})</h1>
{% for post in object_list %}
  <article>
    <h2><a href="/posts/{{ post.id }}">{{ post.title }}</a></h2>
    <p>{{ post.body }}</p>
  </article>
{% endfor %}

{% if has_prev %}<a href="?page={{ page - 1 }}">← prev</a>{% endif %}
page {{ page }} / {{ total_pages }}
{% if has_next %}<a href="?page={{ page + 1 }}">next →</a>{% endif %}
```

`?page=`, `?status=`, `?search=` und `?ordering=` funktionieren genauso wie bei einer
ViewSet-Liste — der Unterschied liegt allein darin, dass das Ergebnis eine gerenderte Seite statt
einer JSON-Hülle ist. Verwende `.context_object_name("posts")`, falls du lieber über `posts` als
über `object_list` im Template iterierst.

---

## DetailView

Eine Zeile, anhand der URL nachgeschlagen. Standardmäßig passt sie zum Primärschlüssel
(`/posts/42`); richte sie mit `.lookup_field("slug")` auf eine andere Spalte aus, um schöne URLs zu
erhalten (`/posts/my-first-post`). Eine fehlende Zeile ist ein `404`.

```rust
use rustango::template_views::DetailView;

let app = DetailView::for_model(Post::SCHEMA)
    .lookup_field("slug")          // GET /posts/{slug} instead of /posts/{id}
    .router("/posts", Arc::new(tera), pool);
```

Das Template erhält die Zeile als `object`:

```html
<h1>{{ object.title }}</h1>
<p>{{ object.body }}</p>
<small>by author #{{ object.author_id }}</small>
```

---

## CreateView, UpdateView, DeleteView

Die Schreibseite. Jeder verarbeitet ein `GET` (ein Formular / eine Bestätigungsseite rendern) und
ein `POST` (die Arbeit erledigen, dann **weiterleiten**). Die Weiterleitung-nach-POST ist das
Standardmuster **Post/Redirect/Get** — es verhindert, dass ein Browser-Refresh erneut absendet.

**CreateView** — `GET /posts/new` rendert ein leeres Formular; `POST /posts/new`
fügt die Zeile ein und `303`t zu `success_url`:

```rust
use rustango::template_views::CreateView;

let app = CreateView::for_model(Post::SCHEMA)
    .success_url("/posts")         // where to send the browser after a save
    .router("/posts", Arc::new(tera), pool);
```

Das Formular-Template (`posts_form.html`) wird mit UpdateView geteilt. `is_update`
unterscheidet die beiden, und `errors` trägt etwaige Validierungsmeldungen zurück:

```html
<form method="post">
  <input name="title" value="{{ object.title | default(value='') }}">
  <textarea name="body">{{ object.body | default(value='') }}</textarea>
  {% for field, msgs in errors %}
    <p class="error">{{ field }}: {{ msgs | join(sep=', ') }}</p>
  {% endfor %}
  <button>{% if is_update %}Save{% else %}Create{% endif %}</button>
</form>
```

**Validierung.** Schema-Regeln (Typ, `max_length`, NOT NULL…) werden automatisch
erzwungen. Füge mit einem Closure-Validator eigene hinzu — bei `Err` wird das Formular
mit den Meldungen und einem `422`-Status neu gerendert statt gespeichert:

```rust
use rustango::forms::FormErrors;

CreateView::for_model(Post::SCHEMA)
    .validator(|data| {
        let mut errs = FormErrors::default();
        if data.get("title").map_or(true, |t| t.len() < 5) {
            errs.add("title", "must be at least 5 characters");
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    })
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

Du kannst auch die Validatoren einer `#[derive(Form)]`-Struktur mit `.form::<F>()` wiederverwenden
(vorerst nur Validierung — siehe die API-Dokumentation).

**UpdateView** — `GET /posts/{pk}/edit` rendert dasselbe Formular, vorausgefüllt aus der
Zeile (`object` ist befüllt, `is_update` ist `true`); `POST` aktualisiert und `303`t.

```rust
use rustango::template_views::UpdateView;

UpdateView::for_model(Post::SCHEMA)
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

**DeleteView** — `GET /posts/{pk}/delete` rendert eine Bestätigungsseite
(`posts_confirm_delete.html`, mit `object`); `POST` löscht und `303`t.

```rust
use rustango::template_views::DeleteView;

DeleteView::for_model(Post::SCHEMA)
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

Hänge alle fünf am selben Präfix ein und du hast vollständiges HTML-CRUD:

```rust
let app = axum::Router::new()
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(CreateView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(UpdateView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(DeleteView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera, pool));
```

---

## Der Tera-Kontext

Jeder View stempelt einen konsistenten Kontext ein, damit Templates sauber zwischen ihnen portieren:

| View | Im Template verfügbare Variablen |
|---|---|
| `ListView` | `object_list` (die Zeilen der Seite), `page`, `page_size`, `total`, `total_pages`, `has_next`, `has_prev` |
| `DetailView` | `object` (die Zeile) |
| `CreateView` / `UpdateView` | `object` (leer beim Erstellen, vorausgefüllt beim Aktualisieren), `is_update` (bool), `errors`, `values` |
| `DeleteView` | `object` (die zu bestätigende Zeile) |

Zeilen werden als schlichte, nach Spaltennamen indizierte Maps bereitgestellt (`{{ post.title }}`),
wobei SQL-`NULL` als `null` gerendert wird. Verwende `.context_object_name("posts" / "post")`, um
neben `object_list` / `object` einen freundlicheren Alias hinzuzufügen.

---

## TemplateView und RedirectView

Zwei modellfreie Helfer für die Seiten, die jede Site hat:

**TemplateView** — rendert ein statisches Template mit einem festen Kontext (eine „Über uns"-Seite,
eine Landingpage). Kein Modell, keine Datenbank:

```rust
use rustango::template_views::TemplateView;

let app = TemplateView::new("about.html")
    .context_value("title", "About us")
    .router("/about", Arc::new(tera));
```

**RedirectView** — eine permanente oder temporäre Weiterleitung an einer URL (für verschobene Seiten):

```rust
use rustango::template_views::RedirectView;

let app = RedirectView::to("/posts").router("/old-posts");
```

---

## Single-Tenant vs Multi-Tenant

Jeder Modell-View bringt zwei Router-Konstruktoren mit — derselbe Builder, wähle den, der dazu
passt, wie deine App Datenbankverbindungen verwaltet:

- **`.router(prefix, tera, pool)`** — Single-Tenant; erfasst zur Einhängezeit einen Pool.
  Das nutzen die Beispiele oben.
- **`.tenant_router(prefix, tera)`** — Multi-Tenant; löst eine Verbindung pro Request aus dem
  [`Tenant`](https://docs.rs)-Extractor auf. Verfügbar mit den Features
  `template_views` + `tenancy`. Templates portieren unverändert zwischen beiden.

Das spiegelt die ViewSet-Aufteilung (`router` / `router_pool` vs `tenant_router`).

---

## Ein Modell auf beide Arten ausliefern

Du bist nicht auf eine Eingangstür beschränkt. Hänge eine JSON-API *und* HTML-Seiten über dasselbe
Modell und denselben Pool ein — eine öffentliche API für Clients, serverseitig gerenderte Seiten
für Menschen:

```rust
use rustango::viewset::ViewSet;
use rustango::template_views::{ListView, DetailView};

let app = axum::Router::new()
    // JSON for API clients:
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    // HTML pages for browsers:
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera, pool));
```

Jetzt gibt `GET /api/posts` die paginierte JSON-Hülle zurück und `GET /posts`
gibt eine gerenderte HTML-Liste zurück — dieselben Zeilen, derselbe Pool, zwei Formen. Genau dieses
Setup ist es, was der [zugrunde liegende Test](../crates/rustango/tests/html_and_api_contrast_sqlite_live.rs)
behauptet.

---

## Siehe auch

- [ViewSets — CRUD-REST-APIs](viewsets.md) — das JSON/API-Gegenstück, ausführlich.
- [Admin](admin.md) — der automatisch generierte Admin baut auf denselben Views auf.
- [URLs & Routing](urls.md) — wie du diese Router zu deiner App zusammensetzt.
- [Serializer](serializers.md) — forme das JSON, wenn du den API-Weg gehst.
