# Vues HTML — pages rendues côté serveur

Une vue HTML transforme un modèle en **pages web rendues côté serveur** — une
page de liste, une page de détail, et des formulaires de création/édition/suppression — à partir
d'une seule déclaration. C'est le **pendant des [ViewSets](viewsets.md)** : là où un ViewSet émet
du JSON pour des clients d'API, une vue HTML émet une page rendue pour un navigateur. Les deux sont
construits à partir du même `#[derive(Model)]`, et vous pouvez servir un modèle des *deux* façons à
la fois.

Ce sont l'équivalent dans **Rustango** des vues génériques basées sur les classes de Django
(`ListView`, `DetailView`, `CreateView`, `UpdateView`, `DeleteView`) ou des contrôleurs de
ressources de Laravel qui renvoient des vues Blade. Elles effectuent leur rendu via des templates
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

- [Vues API vs vues HTML — laquelle vous faut-il ?](#api-views-vs-html-views--which-do-you-want)
- [Les cinq vues de modèle](#the-five-model-views)
- [ListView](#listview) · [DetailView](#detailview)
- [CreateView, UpdateView, DeleteView](#createview-updateview-deleteview)
- [Le contexte Tera](#the-tera-context)
- [TemplateView et RedirectView](#templateview-and-redirectview)
- [Mono-locataire vs multi-locataire](#single-tenant-vs-multi-tenant)
- [Servir un modèle des deux façons](#serving-one-model-both-ways)
- [Voir aussi](#see-also)

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
| Équivalent Django | DRF `ModelViewSet` | vues génériques basées sur les classes |

Vous n'avez pas à choisir globalement — choisissez par ressource, et vous pouvez monter **les deux
sur le même modèle** (voir [ci-dessous](#serving-one-model-both-ways)). Règles empiriques :

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

`?page=`, `?status=`, `?search=` et `?ordering=` fonctionnent de la même façon que sur une liste
ViewSet — la différence tient uniquement au fait que le résultat est une page rendue plutôt qu'une
enveloppe JSON. Utilisez `.context_object_name("posts")` si vous préférez boucler sur `posts`
plutôt que sur `object_list` dans le template.

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

let app = CreateView::for_model(Post::SCHEMA)
    .success_url("/posts")         // where to send the browser after a save
    .router("/posts", Arc::new(tera), pool);
```

Le template de formulaire (`posts_form.html`) est partagé avec UpdateView. `is_update`
distingue les deux, et `errors` ramène les éventuels messages de validation :

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

**Validation.** Les règles de schéma (type, `max_length`, NOT NULL…) sont appliquées
automatiquement. Ajoutez les vôtres avec un validateur en closure — en cas d'`Err`, le formulaire
est re-rendu avec les messages et un statut `422` au lieu d'être enregistré :

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

Vous pouvez aussi réutiliser les validateurs d'une structure `#[derive(Form)]` avec `.form::<F>()`
(validation seulement pour l'instant — voir la documentation de l'API).

**UpdateView** — `GET /posts/{pk}/edit` rend le même formulaire prérempli à partir de la
ligne (`object` est renseigné, `is_update` vaut `true`) ; `POST` met à jour et fait un `303`.

```rust
use rustango::template_views::UpdateView;

UpdateView::for_model(Post::SCHEMA)
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
    .merge(CreateView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
    .merge(UpdateView::for_model(Post::SCHEMA).success_url("/posts").router("/posts", tera.clone(), pool.clone()))
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
| `CreateView` / `UpdateView` | `object` (vide à la création, prérempli à la mise à jour), `is_update` (bool), `errors`, `values` |
| `DeleteView` | `object` (la ligne à confirmer) |

Les lignes sont exposées sous forme de simples maps indexées par nom de colonne (`{{ post.title }}`),
avec le `NULL` SQL rendu en `null`. Utilisez `.context_object_name("posts" / "post")` pour
ajouter un alias plus convivial à côté de `object_list` / `object`.

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
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    // HTML pages for browsers:
    .merge(ListView::for_model(Post::SCHEMA).router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA).router("/posts", tera, pool));
```

Maintenant `GET /api/posts` renvoie l'enveloppe JSON paginée et `GET /posts`
renvoie une liste HTML rendue — mêmes lignes, même pool, deux formes. Cette configuration exacte est
ce qu'affirme le [test de support](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/html_and_api_contrast_sqlite_live.rs).

---

## Voir aussi

- [ViewSets — API REST CRUD](viewsets.md) — le pendant JSON/API, en profondeur.
- [Admin](admin.md) — l'admin auto-généré est construit sur ces mêmes vues.
- [URL & routage](urls.md) — comment composer ces routeurs dans votre application.
- [Sérialiseurs](serializers.md) — façonnez le JSON quand vous prenez la voie de l'API.
