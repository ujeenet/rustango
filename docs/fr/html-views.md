# Vues HTML — pages rendues côté serveur

Une vue HTML transforme un modèle en **pages web rendues côté serveur** — une
page de liste, une page de détail, et des formulaires de création/édition/suppression — à partir
d'une seule déclaration. C'est le **pendant des [ViewSets](viewsets.md)** : là où un ViewSet émet
du JSON pour des clients d'API, une vue HTML émet une page rendue pour un navigateur. Les deux sont
construits à partir du même `#[derive(Model)]`, et vous pouvez servir un modèle des *deux* façons à
la fois.

Chaque type de vue (`ListView`, `DetailView`, `CreateView`, `UpdateView`,
`DeleteView`) couvre un cas standard et effectue son rendu via des templates
[Tera](https://keats.github.io/tera/).

[![Les vues HTML dans Rustango : un modèle alimente ListView, DetailView et CreateView/UpdateView/DeleteView, chacune effectuant le rendu d'un template Tera en une page rendue côté serveur](../img/html-views.png)](../img/html-views.png)

> **Un terme vous est inconnu ?** Si *modèle*, *template*, *routeur* ou *rendu côté serveur* ne
> vous sont pas familiers, le [glossaire](glossary.md) explique chacun d'eux en langage simple.

> **Source :** `rustango::template_views` (`ListView`, `DetailView`, `CreateView`,
> `UpdateView`, `DeleteView`, `TemplateView`, `RedirectView`) — derrière la
> fonctionnalité `template_views` (activée par défaut).
>
> **Version exécutable :** l'exemple API-vs-HTML ci-dessous est fixé par le
> test du framework
> [`html_and_api_contrast_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs)
> (`cargo test -p rustango --features sqlite --test html_and_api_contrast_sqlite_live`).
> Les vues individuelles sont couvertes par `template_view.rs` et
> `template_views_context_object_name_sqlite_live.rs`.

## Table des matières

- [Vues API vs vues HTML — laquelle vous faut-il ?](#vues-api-vs-vues-html--laquelle-vous-faut-il-)
- [Les cinq vues de modèle](#les-cinq-vues-de-modèle)
- [ListView](#listview) · [DetailView](#detailview)
- [CreateView, UpdateView, DeleteView](#createview-updateview-deleteview)
- [Le contexte Tera](#le-contexte-tera)
- [TemplateView et RedirectView](#templateview-et-redirectview)
- [Mono-locataire vs multi-locataire](#mono-locataire-vs-multi-locataire)
- [Servir un modèle des deux façons](#servir-un-modèle-des-deux-façons)
- [Pages d'erreur](#pages-derreur)
- [Voir aussi](#voir-aussi)

---

## Vues API vs vues HTML — laquelle vous faut-il ?

C'est la première décision. Les deux transforment un modèle en points d'accès ; elles diffèrent par
*ce qui en ressort* et *qui appelle*.

| | **Vue API** — [ViewSet](viewsets.md) | **Vue HTML** — ce guide |
|---|---|---|
| Module | `rustango::viewset` | `rustango::template_views` |
| Renvoie | des **données JSON** | une **page HTML rendue côté serveur** |
| Conçue pour | SPA, applications mobiles, autres services | navigateurs, sites rendus côté serveur, CRUD façon admin |
| Une « création » | `POST` JSON → `201` + le nouvel objet | `POST` d'un formulaire → redirection `303` vers une page de succès |
| En cas d'entrée invalide | `400` + une carte d'erreurs JSON indexée par champ | re-rend le formulaire avec les erreurs affichées |
| Lit une liste comme | une enveloppe JSON paginée | un `<table>`/une boucle dans votre template |
| Authentifiée en général par | jetons / JWT / clés d'API | cookies de session |

Vous n'avez pas à choisir globalement — choisissez par ressource, et vous pouvez monter **les deux
sur le même modèle** (voir [ci-dessous](#servir-un-modèle-des-deux-façons)). Règles empiriques :

- Vous construisez un **backend JSON** pour un framework frontend ou une application mobile → ViewSet.
- Vous construisez un **site rendu côté serveur** (le serveur renvoie des pages HTML) → vues
  HTML.
- Vous avez besoin des deux (une API publique *et* des pages CRUD internes) → montez les deux.

> Vous cherchez le côté JSON ? Il a son propre approfondissement : [ViewSets — API REST
> CRUD](viewsets.md).

---

## Les cinq vues de modèle

Chaque vue est `for_model(SCHEMA)` plus un `.router(prefix, tera, pool)`. Les monter au même
`prefix` (disons `/posts`) donne l'ensemble d'URL CRUD classique :

| Vue | Rend | Routes montées | Template par défaut |
|---|---|---|---|
| [`ListView`](#listview) | une liste paginée | `GET <prefix>` | `<table>_list.html` |
| [`DetailView`](#detailview) | une ligne | `GET <prefix>/{pk}` | `<table>_detail.html` |
| [`CreateView`](#createview-updateview-deleteview) | un formulaire de nouvel enregistrement | `GET`/`POST <prefix>/new` | `<table>_form.html` |
| [`UpdateView`](#createview-updateview-deleteview) | un formulaire d'édition prérempli | `GET`/`POST <prefix>/{pk}/edit` | `<table>_form.html` |
| [`DeleteView`](#createview-updateview-deleteview) | une page de confirmation | `GET`/`POST <prefix>/{pk}/delete` | `<table>_confirm_delete.html` |

`<table>` est le nom de la table du modèle, donc un `Post` (table `posts`) recherche
`posts_list.html`, `posts_detail.html`, et ainsi de suite. Remplacez n'importe lequel d'entre eux
avec `.template("my_name.html")`.

---

## ListView

Une page de liste paginée. Vous fournissez un template qui boucle sur `object_list` ; la vue gère
la pagination, le tri, le filtrage et la recherche à partir des paramètres de requête.

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

Un `posts_list.html` correspondant — notez `object_list` et les variables de pagination que la vue
estampille pour vous :

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

`?page=`, `?status=` et `?search=` fonctionnent de la même façon que sur une liste
ViewSet — la différence tient uniquement au fait que le résultat est une page rendue plutôt qu'une
enveloppe JSON. Utilisez `.context_object_name("posts")` si vous préférez boucler sur `posts`
plutôt que sur `object_list` dans le template.

**`?ordering=` est l'exception.** La liste blanche d'un `ListView` démarre *vide*, donc
le paramètre est ignoré tant que vous ne nommez pas vous-même les colonnes triables :

```rust
fn post_list() -> ListView {
    ListView::for_model(Post::SCHEMA).ordering_fields(&["title", "published_at"])
}
```

Sans cet appel, la liste retombe sur le `.order_by(...)` du builder, ou sur PK-ASC pour
que la pagination reste déterministe. Une seule colonne est acceptée, avec un `-`
initial pour l'ordre décroissant.

---

## DetailView

Une ligne, recherchée à partir de l'URL. Par défaut elle correspond à la clé primaire
(`/posts/42`) ; pointez-la vers une autre colonne avec `.lookup_field("slug")` pour des URL jolies
(`/posts/my-first-post`). Une ligne manquante est un `404`.

```rust
use rustango::template_views::DetailView;

let app = DetailView::for_model(Post::SCHEMA)
    .lookup_field("slug")          // GET /posts/{slug} instead of /posts/{id}
    .router("/posts", Arc::new(tera), pool);
```

Le template reçoit la ligne sous le nom `object` :

```html
<h1>{{ object.title }}</h1>
<p>{{ object.body }}</p>
<small>by author #{{ object.author_id }}</small>
```

---

## CreateView, UpdateView, DeleteView

Le côté écriture. Chacune gère un `GET` (rendre un formulaire / une page de confirmation) et un
`POST` (faire le travail, puis **rediriger**). La redirection-après-POST est le motif standard
**Post/Redirect/Get** — il empêche qu'un rafraîchissement du navigateur ne re-soumette.

**CreateView** — `GET /posts/new` rend un formulaire vide ; `POST /posts/new`
insère la ligne et fait un `303` vers `success_url` :

```rust
use rustango::template_views::CreateView;

let app = CreateView::for_model(Post::SCHEMA, &["title", "body"])
    .success_url("/posts")         // where to send the browser after a save
    .router("/posts", Arc::new(tera), pool);
```

La liste des champs est obligatoire. C'est la liste blanche d'écriture : le
formulaire n'affiche que ces champs et toute autre clé envoyée (par exemple
`author_id=1`) est ignorée. Avec UpdateView, un champ listé mais absent garde sa
valeur ; seul un `bool` passe à `false`, car une case décochée n'envoie rien. Avec
CreateView, un champ listé absent qui a un défaut de colonne reçoit ce défaut.

Le template de formulaire (`posts_form.html`) est partagé avec UpdateView. `is_update`
distingue les deux, et **`form`** porte à la fois les champs et les éventuels messages de
validation :

```html
<form method="post">
  {{ csrf_input | safe }}
  {% for field in form.fields %}
    <label for="{{ field.name }}">{{ field.name }}</label>
    <input id="{{ field.name }}" name="{{ field.name }}" value="{{ field.value }}"
           {% if field.required %}required{% endif %}
           {% if field.max_length %}maxlength="{{ field.max_length }}"{% endif %}>
    {% if form.errors[field.name] %}
      <p class="error">{{ form.errors[field.name] }}</p>
    {% endif %}
  {% endfor %}
  <button>{% if is_update %}Save{% else %}Create{% endif %}</button>
</form>
```

Deux choses sont faciles à se tromper ici. `form.errors` associe un nom de champ à **une
seule chaîne**, pas à une liste — lui appliquer `join` est une erreur. Et il n'existe pas de
variable `errors` de premier niveau ; en itérer une est un échec de rendu Tera, si bien que la
page renvoie 500 au lieu d'afficher le message.

`{{ csrf_input | safe }}` a besoin du filtre : Tera échappe automatiquement les `.html`, donc
sans lui le jeton est rendu comme du texte et chaque POST est rejeté.

**Validation.** Les règles de schéma (type, `max_length`, NOT NULL…) sont appliquées
automatiquement. Ajoutez les vôtres avec un validateur en closure — en cas d'`Err`, le formulaire
est re-rendu avec les messages et un statut `422` au lieu d'être enregistré :

```rust
use rustango::forms::FormErrors;

CreateView::for_model(Post::SCHEMA, &["title", "body"])
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

Vous pouvez aussi réutiliser les validateurs d'une structure `#[derive(Form)]` avec `.form::<F>()`
(validation seulement pour l'instant — voir la documentation de l'API).

**UpdateView** — `GET /posts/{pk}/edit` rend le même formulaire prérempli à partir de la
ligne (`object` est renseigné, `is_update` vaut `true`) ; `POST` met à jour et fait un `303`.

```rust
use rustango::template_views::UpdateView;

UpdateView::for_model(Post::SCHEMA, &["title", "body"])
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

**DeleteView** — `GET /posts/{pk}/delete` rend une page de confirmation
(`posts_confirm_delete.html`, avec `object`) ; `POST` supprime et fait un `303`.

```rust
use rustango::template_views::DeleteView;

DeleteView::for_model(Post::SCHEMA)
    .success_url("/posts")
    .router("/posts", Arc::new(tera), pool);
```

Montez les cinq au même préfixe et vous avez un CRUD HTML complet :

```rust
let app = axum::Router::new()
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(CreateView::for_model(Post::SCHEMA, &["title", "body"]).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(UpdateView::for_model(Post::SCHEMA, &["title", "body"]).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(DeleteView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera, pool));
```

---

## Le contexte Tera

Chaque vue estampille un contexte cohérent afin que les templates se portent proprement de l'une à
l'autre :

| Vue | Variables disponibles dans le template |
|---|---|
| `ListView` | `object_list` (les lignes de la page), `page`, `page_size`, `total`, `total_pages`, `has_next`, `has_prev` |
| `DetailView` | `object` (la ligne) |
| `CreateView` | `form` (`.fields`, `.errors`), `is_create` (true), `is_update` (false) — **pas d'`object`** |
| `UpdateView` | `form` (`.fields`, `.errors`), `object` (la ligne), `pk`, `is_create` (false), `is_update` (true) |
| `DeleteView` | `object` (la ligne à confirmer) |

Les lignes sont exposées sous forme de simples maps indexées par nom de colonne (`{{ post.title }}`),
avec le `NULL` SQL rendu en `null`. Utilisez `.context_object_name("posts" / "post")` pour
ajouter un alias plus convivial à côté de `object_list` / `object`.

Chaque entrée de `form.fields` porte `name`, `column`, `ty`, `required`, `max_length` et
`value`. `form.errors` est indexé par nom de champ et contient un message par champ, pas une
liste. Chaque vue estampille aussi `csrf_token` et `csrf_input`, et chaque routeur de vue
rejette un POST sans jeton correspondant.

`CreateView` n'estampille aucun `object`, donc `{{ object.title }}` dans un formulaire de
création est indéfini plutôt que vide — utilisez `{{ field.value }}` depuis `form.fields`, qui
est renseigné dans les deux cas.

---

## TemplateView et RedirectView

Deux helpers sans modèle pour les pages que tout site possède :

**TemplateView** — rend un template statique avec un contexte fixe (une page « à propos », une page
d'atterrissage). Pas de modèle, pas de base de données :

```rust
use rustango::template_views::TemplateView;

let app = TemplateView::new("about.html")
    .context_value("title", "About us")
    .router("/about", Arc::new(tera));
```

**RedirectView** — une redirection permanente ou temporaire sur une URL (pour les pages déplacées) :

```rust
use rustango::template_views::RedirectView;

let app = RedirectView::to("/posts").router("/old-posts");
```

---

## Mono-locataire vs multi-locataire

Chaque vue de modèle est livrée avec deux constructeurs de routeur — même builder, choisissez celui
qui correspond à la façon dont votre application gère les connexions à la base de données :

- **`.router(prefix, tera, pool)`** — mono-locataire ; capture un pool unique au moment du montage.
  C'est ce qu'utilisent les exemples ci-dessus.
- **`.tenant_router(prefix, tera)`** — multi-locataire ; résout une connexion par requête à partir
  de l'extracteur [`Tenant`](https://docs.rs). Disponible avec les fonctionnalités
  `template_views` + `tenancy`. Les templates se portent inchangés de l'une à l'autre.

Cela reflète la séparation des ViewSets (`router` / `router_pool` vs `tenant_router`).

---

## Servir un modèle des deux façons

Vous n'êtes pas limité à une seule porte d'entrée. Montez une API JSON *et* des pages HTML sur le
même modèle et le même pool — une API publique pour les clients, des pages rendues côté serveur
pour les personnes :

```rust
use rustango::viewset::ViewSet;
use rustango::template_views::{ListView, DetailView};

let app = axum::Router::new()
    // JSON for API clients:
    .merge(ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"]).router_pool("/api/posts", pool.clone()))
    // HTML pages for browsers:
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera, pool));
```

Maintenant `GET /api/posts` renvoie l'enveloppe JSON paginée et `GET /posts`
renvoie une liste HTML rendue — mêmes lignes, même pool, deux formes. Cette configuration exacte est
ce qu'affirme le [test de support](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs).

---

## Pages d'erreur

Un layer transforme chaque 4xx/5xx de votre app en page pour les humains et
laisse les clients d'API tranquilles. Il est **désactivé par défaut** ; rien ne
change tant que vous ne l'activez pas. Nécessite la feature `template_views`.

### L'activer

Sur le serveur multi-locataire, le layer enveloppe tout : votre API, le login
du locataire, l'admin du locataire, la console opérateur et les layers du
framework.

```rust
use std::sync::Arc;
use rustango::errors::ErrorPagesLayer;

let tera = Arc::new(rustango::template_extensions::html_tera_from_glob("templates/**/*.html")?);
let app = rustango::server::Builder::from_env().await?
    .api(my_app::urls::api())
    .error_pages(ErrorPagesLayer::new().templates(tera));
```

`AppBuilder::error_pages` fait de même pour une app à pool unique. Sur votre
propre routeur, montez-le avec `ErrorPagesRouterExt` et ajoutez le fallback 404 :

```rust
use rustango::errors::{ErrorPagesLayer, ErrorPagesRouterExt as _};

let app = Router::new()
    .route("/projects", get(projects))
    .fallback(rustango::errors::not_found)
    .error_pages(ErrorPagesLayer::new().templates(tera.clone()));
```

Avec `Cli` et un fichier de config, utilisez plutôt la section `[errors]` :

```toml
[errors]
enabled = true            # default false
templates = "templates"   # template root holding errors/404.html etc.; default "templates"
htmx_target = "#flash"    # HX-Retarget for htmx errors; default "#flash", "" sends none
```

Un répertoire `templates` absent ou qui ne se parse pas est journalisé, et les
pages intégrées sont utilisées.

### Ce que reçoit chaque client

| Requête | Réponse |
|---|---|
| Navigation navigateur (`Accept: text/html`, sans `HX-Request`) | Page HTML complète |
| htmx (`HX-Request`) | Fragment HTML, plus `HX-Retarget: <htmx_target>` et `HX-Reswap: innerHTML show:window:top` |
| Tout autre client (`fetch`, curl) | Corps inchangé ; un corps vide reçoit le message en texte brut |
| JSON / problem+json | Inchangé, sauf s'il porte un `Message` et qu'un navigateur ou htmx demande |
| `HEAD` | Inchangé |
| Une réponse marquée `rustango::errors::KeepBody` | Inchangée |

Seuls les corps texte (`text/plain`, `text/html`, aucun) et le JSON avec message
sont remplacés ; images et autres types passent. Une réponse habillée garde son
statut et ses en-têtes (`Retry-After` aussi) et reçoit `Vary: Accept, HX-Request`.

### Templates

Le premier template présent dans votre Tera l'emporte :

1. `errors/<code>.html`, p. ex. `errors/404.html`
2. `errors/4xx.html` ou `errors/5xx.html`
3. `errors/default.html`
4. la page intégrée

Les fragments htmx utilisent `errors/partial.html`, puis le fragment intégré.
Un template dont le rendu échoue est journalisé et la page intégrée est
utilisée ; une page d'erreur ne revient jamais vide.

`templates/errors/404.html` :

```html
{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ code }} · {{ title }}</h1>
  <p>{{ message }}</p>
  <p>Nothing at <code>{{ path }}</code>.</p>
  {% if user %}<p>Signed in as {{ user.username }}.</p>{% endif %}
  <a href="/">Back to the start</a>
</section>
{% endblock content %}
```

`templates/errors/5xx.html` :

```html
{% extends "base.html" %}
{% block content %}
<section class="error">
  <h1>{{ title }}</h1>
  <p>{{ message }}</p>
  <form method="post" action="/feedback">{{ csrf_input | safe }}
    <button>Tell us what you were doing</button>
  </form>
</section>
{% endblock content %}
```

Contexte :

| Clé | Valeur |
|---|---|
| `code` | Le statut, p. ex. `404` |
| `title` | Un titre court par statut (« Not found ») |
| `message` | Ce qui s'est mal passé, pour l'utilisateur (voir plus bas) |
| `path` | Le chemin de la requête |
| `user` | `{ username, is_superuser }` de l'utilisateur connecté du locataire, ou null |
| `username`, `is_superuser` | La même chose, à plat |
| `csrf_token`, `csrf_input` | Un jeton CSRF et son input caché ; le cookie part avec la réponse |
| `csp_nonce` | Le nonce CSP de cette requête, ou `""` |
| `brand_name`, `brand_logo_url`, `brand_css`, `theme_mode` | Depuis `[brand]` |

`user` nécessite la feature `tenancy` et un hôte de locataire ; ailleurs il vaut
null. Les templates sont par app : chaque locataire reçoit les mêmes.

### La page intégrée

HTML autonome : pas de CDN, pas de script, CSS inline avec le nonce CSP de la
requête, les tokens de thème de l'admin (clair et sombre). Avec `[errors]` dans
les settings, elle affiche `[brand] name` et `logo_url` ; `primary_color` et
`theme_mode` fixent les couleurs (ils nécessitent la feature `tenancy`, dont elle
réutilise le code de marque). `ErrorPagesLayer::brand(&settings.brand)` fait de
même en code.

### Le message

Un handler donne des mots à la page avec `rustango::errors::Message`, une
extension de réponse, ou avec `PageError`, qui en porte un :

```rust
use rustango::errors::{Message, PageError};

async fn project(Path(slug): Path<String>) -> Result<Html<String>, PageError> {
    let project = find_project(&slug)
        .await
        .map_err(PageError::server)?            // logged; the page says something generic
        .ok_or_else(|| PageError::not_found("No project with that name."))?;
    Ok(Html(render_project(&project)))
}

async fn archived() -> impl IntoResponse {
    (StatusCode::GONE, Message::new("This project was archived."))
}
```

Sans `Message`, un corps 4xx `text/plain` de 2 Kio au plus est le message (les
rejets d'axum, la vérification CSRF), sinon le texte par défaut du statut. La
première lettre passe en majuscule.

Un 5xx ne montre jamais sa cause : la page utilise le texte générique, et
`PageError::server(e)` journalise `e` et envoie le même corps opaque que tout
autre 5xx. Ce n'est que lorsque `RUSTANGO_DISCLOSE_ERRORS` est défini qu'une
page 5xx montre son message ou son corps — la même règle que pour tout corps 5xx
du framework.

### Ce qui est couvert

Monté par `Builder::error_pages` (ou `[errors]`), le layer se place à
l'intérieur des en-têtes de sécurité et à l'extérieur de tout le reste ; il
habille donc :

- les chemins et hôtes inconnus (404), et les 405 ;
- le 500 du panic guard ;
- le 403 du layer CSRF, avec sa raison ;
- les gardes `login_required_or_401` / `*_or_403` (401, 403) ;
- la limite de corps (413), le timeout de requête (504), le rate limit et le throttle de login (429) ;
- les rejets d'axum (400, 415, 422) ;
- les refus et 404 de l'admin du locataire, et les chemins inconnus de la console opérateur.

Les pages qui portent leurs propres contrôles gardent leur aspect : la page
« admin access required » de l'admin (elle a un formulaire de déconnexion) et
sa page « table not migrated ». Marquez vos propres réponses avec `KeepBody`
pour faire de même.

### Migrer depuis un middleware d'erreur écrit à la main

Une app qui avait son propre middleware `error_pages` peut supprimer :

- le middleware (`from_fn(error_pages)`) et son `.fallback(not_found)` ;
- sa table titres/messages : `rustango::errors::error_words` est la même table ;
- son extension de message privée : renvoyez `PageError` ou `Message` ;
- le clonage d'en-têtes/extensions qu'il faisait pour poser le CSRF et trouver l'utilisateur.

Gardez `templates/error.html` comme `templates/errors/default.html` et l'alerte
htmx comme `templates/errors/partial.html` ; les clés du contexte sont les mêmes.

---

## Voir aussi

- [ViewSets — API REST CRUD](viewsets.md) — le pendant JSON/API, en profondeur.
- [Admin](admin.md) — l'admin auto-généré est construit sur ces mêmes vues.
- [URL & routage](urls.md) — comment composer ces routeurs dans votre application.
- [Sérialiseurs](serializers.md) — façonnez le JSON quand vous prenez la voie de l'API.
