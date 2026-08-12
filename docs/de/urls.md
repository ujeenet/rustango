# URL-Namen & Reverse

URLs (`/posts/42`) überall in Handlern und Templates fest zu verdrahten ist fragil
— ändern Sie eine Route und jedes Literal bricht stillschweigend. **Rustango**
gibt Ihnen Djangos Antwort: **benennen Sie ein URL-Muster einmal, dann bauen Sie
die URL überall über den Namen** — in Rust mit `reverse(...)`, in Templates mit
`{{ url(...) }}` und in Redirects mit `redirect_to_view(...)`. Die API-Oberfläche
spiegelt Djangos `reverse()` / `{% url %}` / `resolve_url()` / `redirect()`.

[![Reverse-URLs im Django-Stil: register_url! benennt ein Muster, reverse() baut die URL in Rust, und {{ url(...) }} baut die URL in einem Template](img/urls.png)](img/urls.png)

> **Quelle:** `rustango::urls` (`register_url!`, `reverse`, `reverse_owned`,
> `all_routes`, `duplicates`, `register_url_tag`) und `rustango::shortcuts`
> (`resolve_url`, `redirect_to_view`).

> **Ein Begriff hier neu?** *Route*, *Reverse*, *Namespacing* — siehe das
> [Glossar](glossary.md).

---

## Inhaltsverzeichnis
- [Eine benannte URL registrieren](#register-a-named-url)
- [Reverse in Rust](#reverse-in-rust) · [Reverse in Templates](#reverse-in-templates)
- [Redirect per Name](#redirect-by-name) · [Namespacing](#namespacing)
- [Die URL-Map inspizieren](#inspect-the-url-map) · [Fehler](#errors)
- [Regex & typisierte Pfadmuster](#regex--typed-path-patterns) · [Hinweise & Grenzen](#notes-and-limits)

---

## Eine benannte URL registrieren

`register_url!("name", "/pattern")` registriert ein Mapping Name → Muster. Es läuft
beim Laden des Moduls (via `inventory`), sodass die Route in einer globalen
Registry landet, sobald ihr Modul gelinkt ist — keine zentrale `urls.py` zu
bearbeiten und kein `include()` zu verdrahten.

```rust
use rustango::register_url;

register_url!("post-detail", "/posts/{id}");
register_url!("user-posts",  "/users/{user_id}/posts/{post_id}");
register_url!("home",        "/");
```

Platzhalter nutzen axums `{name}`-Pfadsyntax. Das Muster ist derselbe String, an
dem Sie den Handler mounten — halten Sie sie synchron (registrieren Sie den Namen
dort, wo Sie die Route bauen).

---

## Reverse in Rust

`reverse(name, &params)` ersetzt die `{placeholders}` des Musters durch die
gegebenen Werte (jeden prozentkodiert) und gibt die URL zurück:

```rust
use std::collections::HashMap;
use rustango::urls::reverse;

let mut params = HashMap::new();
params.insert("id", "42".to_string());

let url = reverse("post-detail", &params)?;   // → "/posts/42"
```

Für dynamische Schlüssel (z. B. aus einer Anfrage zusammengesetzte Werte) nimmt
`reverse_owned` `HashMap<String, String>` statt `HashMap<&str, String>`:

```rust
use rustango::urls::reverse_owned;
let url = reverse_owned("post-detail", &owned_params)?;
```

`reverse` ist **strikt**: ein fehlender Platzhalter oder ein zusätzlicher
`params`-Schlüssel, den das Muster nicht hat, ist ein Fehler (kein stiller
Fehlabgleich) — siehe [Fehler](#errors).

---

## Reverse in Templates

Templates erhalten Djangos `{% url %}` als Tera-Funktion. Registrieren Sie sie
einmalig auf Ihrer `Tera`-Instanz beim Setup (sie liegt hinter dem
`template_views`-Feature):

```rust
rustango::urls::register_url_tag(&mut tera);
```

Rufen Sie dann `url(name=..., <param>=...)` in einem beliebigen Template auf —
`name` ist erforderlich, und jedes weitere Schlüsselwort-Argument ist ein
Pfadparameter (Strings, Zahlen und Booleans werden akzeptiert):

```jinja
<a href="{{ url(name='post-detail', id=42) }}">View post</a>
<a href="{{ url(name='user-posts', user_id=7, post_id=42) }}">…</a>
```

Das entspricht Djangos `{% url 'post-detail' id=42 %}`. Für das Capture-Muster
`{% url 'x' as var %}` verwenden Sie Teras `{% set %}`:

```jinja
{% set post_url = url(name='post-detail', id=post.id) %}
<a href="{{ post_url }}">{{ post.title }}</a>
```

Ein `null`-Argument (meist eine undefinierte Template-Variable) schlägt lautstark
fehl, statt stillschweigend eine kaputte URL zu erzeugen.

---

## Redirect per Name

`rustango::shortcuts` spiegelt Djangos View-Namen-Redirect-Helfer, sodass Handler
niemals ein `Location` fest verdrahten:

```rust
use std::collections::HashMap;
use rustango::shortcuts::{redirect_to_view, resolve_url};

// redirect('post-detail', id=42) → 302 Location: /posts/42
let mut params = HashMap::new();
params.insert("id", "42".to_string());
let response = redirect_to_view("post-detail", &params)?;
```

`resolve_url(spec, &params)` ist Djangos `resolve_url`: Wenn `spec` bereits wie
eine URL aussieht (`/…`, `http://`, `https://`, `./`, `../`), wird es unverändert
zurückgegeben; sonst wird es als Routenname behandelt und per Reverse aufgelöst.
Praktisch für einen `?next=`-Parameter oder eine Einstellung, die *entweder* einen
Pfad *oder* einen Namen halten kann:

```rust
let url = resolve_url("post-detail", &params)?;  // name  → "/posts/42"
let url = resolve_url("/dashboard", &params)?;   // path  → "/dashboard" (as-is)
```

(Für rohe Redirects zu einer bekannten URL gibt `rustango::shortcuts::redirect(url)`
ein schlichtes `302` zurück.)

---

## Namespacing

Es gibt kein `include()` und keinen automatisch angewandten App-Namespace — jedes
`register_url!` landet in einer globalen Registry. Namespacing ist eine
**Konvention im Namen selbst**: mit `app:` präfixieren, genau wie Sie Djangos
`reverse("app:detail")` aufrufen würden.

```rust
register_url!("blog:post-detail", "/blog/posts/{id}");
register_url!("shop:product",     "/shop/products/{slug}");
```

```rust
reverse("blog:post-detail", &params)?;   // "/blog/posts/42"
```

Der Doppelpunkt ist einfach Teil des registrierten Strings — wählen Sie ein
konsistentes Präfix pro App, um Kollisionen zu vermeiden.

---

## Die URL-Map inspizieren

Listen Sie jede registrierte Route über die CLI auf — nützlich für ein schnelles
Audit oder zum Skripten:

```bash
cargo run -- showurls                  # plain table of name → pattern
cargo run -- showurls --format json    # machine-readable
```

Im Code gibt `all_routes()` die gesamte Registry zurück, und `duplicates()` gibt
jeden Namen zurück, der mehr als einmal registriert wurde (ansonsten gilt
„erster gewinnt“ — beim Booten wert zu asserten):

```rust
use rustango::urls::{all_routes, duplicates};

for route in all_routes() {
    println!("{} → {}", route.name, route.pattern);
}

let dups = duplicates();
assert!(dups.is_empty(), "duplicate URL names: {dups:?}");
```

---

## Fehler

`reverse` / `reverse_owned` / `resolve_url` / `redirect_to_view` geben
`Result<_, rustango::urls::ReverseError>` zurück:

| Variante | Wann |
|---|---|
| `UnknownName(name)` | Kein `register_url!` lief für diesen Namen (Tippfehler, oder sein Modul wurde nicht gelinkt). |
| `MissingParam { name, param }` | Das Muster hat `{param}`, aber `params` lieferte es nicht. |
| `UnexpectedParam { name, param }` | `params` trug einen Schlüssel, den das Muster nicht hat (fängt Tippfehler). |
| `MalformedPattern { name, detail }` | Das registrierte Muster ist fehlerhaft (z. B. eine nicht geschlossene `{`). |

In Templates tauchen sie als Tera-Render-Fehler auf (ein 500 via
`shortcuts::render` / `template_views`), sodass ein fehlerhaftes `{{ url(...) }}`
sichtbar fehlschlägt, statt einen kaputten Link zu rendern.

---

## Regex & typisierte Pfadmuster

**Rustango hat kein `re_path` und erzwingt niemals einen Pfad-Konverter.** Ein
Mustersegment ist entweder ein Literal (`/posts/new`) oder ein
`{name}`-Platzhalter, der genau ein Segment erfasst; `{*name}` erfasst den Rest des
Pfades. Das ist das gesamte Vokabular — es gibt kein `r'(?P<year>[0-9]{4})'`, und
`{int:id}` schränkt `id` **nicht** auf eine Ganzzahl ein.

### Warum — der Matcher ist keine Regex-Engine

Das Routing *ist* [axum](https://docs.rs/axum) 0.8, und axum matcht Pfade mit
[`matchit`](https://docs.rs/matchit), einem **Radix-Trie**-Router. Er läuft die URL
segmentweise einen Präfixbaum hinunter, sodass ein Match O(Pfadlänge) kostet und
unabhängig davon ist, wie viele Routen Sie registriert haben. Ein Regex-Router
macht das Gegenteil: Django wertet `urlpatterns` von oben nach unten aus und führt
die Regex jedes Eintrags gegen den Pfad aus, bis eine passt. Der Trie erkauft
Matching in konstanter Zeit und eine eindeutige „spezifischstes Literal
gewinnt“-Präzedenz — zum Preis, Zeichenklassen-Beschränkungen nicht *im Pfad
selbst* auszudrücken.

Rustango erbt diesen Matcher komplett. Es gibt **keinen zweiten, regex-basierten
Resolver** obendrauf, und `register_url!` erfasst bewusst *dieselben*
`{name}`-Strings, die der Router bereits versteht — es kompiliert nie eine Regex.
Regex-Pfade sind also nicht „abgeschaltet“; die Routing-Schicht war schlicht nie
eine Regex-Engine.

Die Form `{int:id}` wird nur als **Portierungshilfe** für `reverse()` akzeptiert:
Der Builder teilt den Platzhalter an `:` und behält nur den Namen, verwirft das
Typpräfix ([`urls.rs`](../crates/rustango/src/urls.rs)). Das lässt `reverse()` auf
einem Muster laufen, das wortwörtlich aus einem Django-`path("<int:id>/", …)`
kopiert wurde — aber nichts validiert, dass der gelieferte Wert tatsächlich eine
Ganzzahl ist.

### Wie man eine eingeschränkte Route ausdrückt

Matchen Sie das Segment mit einem schlichten `{placeholder}`, dann erzwingen Sie
seine Form dort, wo der Wert verwendet wird. Djangos
`re_path(r'^articles/(?P<year>[0-9]{4})/$', …)` wird zu:

```rust
register_url!("article-by-year", "/articles/{year}");
// router:
.route("/articles/{year}", get(article_by_year))

async fn article_by_year(Path(year): Path<String>) -> impl IntoResponse {
    // the router accepted any single segment; enforce [0-9]{4} here
    match year.parse::<u16>() {
        Ok(y) if (1000..=9999).contains(&y) => render_year(y).await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
```

Um *bevor* der Handler läuft abzulehnen (näher an Djangos Konverter-Semantik),
legen Sie die Prüfung in einen eigenen axum-Extractor (`FromRequestParts`) und
nehmen Sie diesen Typ statt `Path<String>` als Handler-Argument — das Framework
liefert keinen mit, aber axums Extractor-Trait ist die vorgesehene Nahtstelle. Der
`regex`-Crate ist bereits eine Abhängigkeit (das ORM nutzt ihn für
`__regex`-Lookups), sodass ein validierender Extractor eine `Regex` einmal
kompilieren und über Anfragen hinweg wiederverwenden kann.

---

## Hinweise und Grenzen

- **Registrierung erfolgt zur Link-Zeit.** Ein `register_url!` wirkt nur, wenn sein
  Modul in das Binary kompiliert ist. Ein `UnknownName`-Fehler bedeutet meist, dass
  der Name ein Tippfehler ist *oder* sein Modul nirgends referenziert wird (der
  Linker hat es also fallen gelassen).
- **Muster werden nicht gegen Ihre echten Routen validiert.** `register_url!`
  erfasst ein Mapping Name → String; es prüft nicht, ob tatsächlich ein Handler an
  diesem Muster gemountet ist. Registrieren Sie den Namen dort, wo Sie die Route
  mounten, damit sie synchron bleiben.
- **Werte werden prozentkodiert** von `reverse`, sodass sie sicher in einen
  `Location`-Header oder ein `href` fallengelassen werden können.
- **Keine Regex-/typisierten Konverter** in Mustern (Djangos `<int:pk>`);
  Platzhalter sind schlichte `{name}`, und Werte werden unverändert eingesetzt
  (nach der Kodierung). Siehe [Regex & typisierte Pfadmuster](#regex--typed-path-patterns)
  für das Warum und wie man eine Route stattdessen einschränkt.


---

## Siehe auch

- [HTML-Views](html-views.md)
- [ViewSets](viewsets.md)
- [Middleware](middleware.md)
