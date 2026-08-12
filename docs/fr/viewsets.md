# ViewSets — API REST CRUD

Un ViewSet transforme un modèle en une ressource REST complète — des endpoints pour **lister,
créer, lire, mettre à jour et supprimer** des enregistrements — à partir d'une seule déclaration. (C'est
l'équivalent dans **Rustango** d'un `ModelViewSet` de Django REST Framework ou d'un
contrôleur de ressource d'API Laravel, si vous en avez déjà utilisé.)

> **Nouveau dans les API REST ?** Ce guide suppose que vous savez ce qu'est un *endpoint*, un *verbe
> HTTP* (GET / POST / …) et une *requête et réponse JSON*. Si l'un de ces concepts
> est flou, le [glossaire](glossary.md#web-api-basics) en fait un tour d'horizon de cinq minutes —
> lisez-le d'abord, puis revenez ici.

Associez un ViewSet à un [sérialiseur](serializers.md) — la pièce qui façonne votre
JSON — et il protège **les deux directions** à la fois : le sérialiseur formate chaque
**réponse** (renommer, masquer, calculer ou imbriquer des champs) *et* régit chaque
**requête** (il valide les données entrantes et ignore silencieusement les champs qu'un client
ne devrait pas être autorisé à définir). Les entrées rejetées reviennent dans la forme familière de DRF —
un objet JSON indexé par nom de champ. Tout fonctionne de la même manière sur PostgreSQL,
MySQL et SQLite.

Ce guide est avant tout un tutoriel : nous **construisons une API REST de blog complète** de bout en bout —
échafaudage, modèles, un sérialiseur, le ViewSet, les six endpoints CRUD, la validation
des entrées, le filtrage/la recherche/la pagination, et les tests — puis le reste de la page
est une référence pour chaque réglage.

[![Un ViewSet Rustango branché sur un sérialiseur : un seul bloc #[viewset(serializer = …)] fournit une sortie JSON typée et une entrée validée sur les six routes CRUD](img/viewsets.png)](img/viewsets.png)

> **Source :** `rustango::viewset` (`ViewSet`, `#[derive(ViewSet)]`, les
> options `#[viewset(...)]` + le builder `for_model`) — toujours compilé.
>
> **Version exécutable :** le blog construit ici reflète l'exemple testé et compilable
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog)
> (ses `Post` / `PostSerializer` / `PostViewSet`), et chaque comportement est
> verrouillé par les propres tests live du framework — `crates/rustango/tests/viewset_*.rs`
> (notamment `viewset_serializer_render_sqlite_live` et
> `viewset_serializer_input_sqlite_live`).

---

## Table des matières
- [Vues API vs vues HTML](#api-views-vs-html-views) — du JSON pour les clients, ou des pages HTML ?
- [Construire une API REST de blog](#build-a-rest-blog-api) — la présentation complète
- [Le mariage du sérialiseur : entrée + sortie](#the-serializer-marriage-input--output)
- [Les deux façons de définir un ViewSet](#the-two-ways-to-define-a-viewset)
- [Les endpoints CRUD](#the-crud-endpoints) · [Choisir lesquels exposer](#choosing-which-operations-to-expose)
- [Référence `#[viewset(...)]`](#viewset-attribute-reference) · [Référence du builder](#builder-reference)
- [Filtrage, recherche & tri](#filtering-search-and-ordering) · [Pagination](#pagination)
- [Validation](#validation) · [Permissions & throttling](#permissions-and-throttling) · [Actions personnalisées](#custom-actions-beyond-crud)
- [Montage](#mounting) · [Backends](#backend-support)

---

## Vues API vs vues HTML

Avant le tutoriel, une bifurcation. **Rustango** a deux façons de transformer un
modèle en endpoints, et un ViewSet est l'une d'elles :

- Un **ViewSet** (ce guide) est une **vue API** — il parle **JSON**, pour les
  frameworks frontend, les applications mobiles et les autres services.
- Une **vue template** ([vues HTML](html-views.md)) est une **vue HTML** — elle
  rend des **pages côté serveur** via Tera, pour les navigateurs et les sites
  rendus côté serveur.

Le même modèle en dessous ; ce qui diffère, c'est ce qui en ressort et qui appelle.

| | **Vue API** — ViewSet (ici) | **Vue HTML** — [vues template](html-views.md) |
|---|---|---|
| Module | `rustango::viewset` | `rustango::template_views` |
| Renvoie | **des données JSON** | une **page HTML rendue côté serveur** |
| Conçue pour | les SPA, le mobile, les autres services | les navigateurs, les sites rendus côté serveur, le CRUD de type admin |
| Un « create » | `POST` JSON → `201` + l'objet | `POST` d'un formulaire → redirection `303` (Post/Redirect/Get) |
| Sur entrée invalide | `400` + une carte d'erreurs JSON indexée par champ | re-rendu du formulaire avec les erreurs affichées |
| Un « list » est | une enveloppe JSON paginée | une boucle sur les lignes dans votre template |
| Généralement authentifiée par | tokens / JWT / clés d'API | cookies de session |
| Équivalent Django | `ModelViewSet` de DRF | vues génériques basées sur des classes |

Choisissez par ressource — et vous pouvez monter **les deux sur le même modèle** (une API JSON
publique *et* des pages CRUD internes). Le reste de ce guide concerne le côté JSON/API ; pour
le côté HTML, voir [Vues HTML — pages rendues côté serveur](html-views.md).

---

## Construire une API REST de blog

Nous allons construire un blog avec deux modèles — `Author` et `Post` — et exposer `Post` comme
une ressource REST à `/api/posts` dont la forme JSON et la validation sont pilotées par un
sérialiseur. À la fin, vous pourrez faire un `curl` sur chaque verbe CRUD et observer le sérialiseur
façonner la sortie et rejeter les entrées invalides.

Cette présentation suppose un projet créé avec `cargo rustango new myblog`
(voir [Prise en main](getting-started.md) pour la configuration du projet et de la base de données).
Chaque étape est une commande ou un fichier réel.

### Étape 1 — Créer l'app blog

Les apps sont des modules de fonctionnalités autonomes (le `startapp` de Django) :

```bash
cargo run -- startapp blog
```

Cela écrit `src/blog/{mod,models,views,urls,tests}.rs` et branche le module
dans `main.rs` + l'agrégateur `urls::api()`.

### Étape 2 — Définir les modèles

`src/blog/models.rs` — un `Author` et un `Post` (une clé étrangère les relie) :

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

### Étape 3 — Migrer

Générez et appliquez la migration (comme `makemigrations` + `migrate`) :

```bash
cargo run -- makemigrations
cargo run -- migrate
```

### Étape 4 — Échafauder le sérialiseur

Le sérialiseur est ce qui fait de ceci une API *DRF* — il définit le
contrat requête/réponse. Générez le squelette :

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Puis complétez-le. Celui-ci exerce toute la surface entrée+sortie — un renommage,
un champ calculé en lecture seule, un champ serveur en lecture seule, et un validateur de champ :

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

Enregistrez le module — ajoutez `pub mod post_serializer;` à `src/blog/mod.rs`.

Notez que nous n'avons écrit qu'un seul validateur (`title_min_3`) ; les champs **héritent aussi
automatiquement des contraintes du modèle** — `title` voit sa longueur vérifiée contre le
`max_length = 200` du modèle, et une colonne `choices`/`min`/`max` serait vérifiée
également, renvoyant toutes des `400` conviviales à l'écriture. Ajoutez les attributs de sérialiseur `max_length` / `min_length` /
`min` / `max` pour surcharger la borne d'un champ. (Voir le
[guide des sérialiseurs](serializers.md#validation) pour l'histoire complète de la validation.)

### Étape 5 — Échafauder le ViewSet et brancher le sérialiseur

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Modifiez-le pour déclarer la ressource et **brancher le sérialiseur avec l'attribut
`serializer`** — cette seule ligne active la sortie *et* l'entrée pilotées par le sérialiseur :

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

Ajoutez `pub mod post_view_set;` à `src/blog/mod.rs`.

> Avec un sérialiseur branché, vous n'avez pas besoin de `fields = "..."` — le sérialiseur est
> la projection. N'utilisez `fields` que lorsque vous voulez la projection de champs par défaut
> (non-sérialiseur) à la place.

### Étape 6 — Monter les routes

Dans un projet mono-locataire, imbriquez le routeur du ViewSet sous un chemin, en passant le
pool :

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

`make:api_routes blog` échafaude exactement cet agrégateur si vous préférez le
générer. Branchez `blog::urls::api(pool)` dans votre `urls.rs` de niveau supérieur.

### Étape 7 — Le lancer et exercer chaque endpoint

```bash
cargo run            # listening on http://0.0.0.0:8080
```

**Créer** (`POST`). Le sérialiseur valide d'abord, puis n'écrit que les
champs qu'il accepte :

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
La réponse est la forme du **sérialiseur** : `body` est revenu sous `content`, le
`summary` calculé est apparu, et `published_at` (en lecture seule, défini par le serveur) est
présent.

**La validation rejette les entrées invalides** avec une `400` en forme DRF —
des tableaux de messages indexés par champ :

```bash
curl -i -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"hi","content":"x","author_id":1}'
# HTTP/1.1 400 Bad Request
# {"title":["title must be at least 3 characters"]}
```

**Les champs en lecture seule / calculés qu'un client poste sont ignorés** — il ne peut pas injecter
`published_at` ni `summary` :

```bash
curl -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"Sneaky","content":"x","author_id":1,"published_at":"1999-01-01T00:00:00Z","summary":"hax"}'
# → published_at is the server value, not 1999; summary is recomputed from body.
```

**Lister** (`GET`) — paginé, chaque ligne dans la forme du sérialiseur :

```bash
curl localhost:8080/api/posts
```
```json
{ "count": 1, "page": 1, "page_size": 20, "last_page": 1, "results": [ { "id": 1, "title": "Hello Rustango", … } ] }
```

**Récupérer / mettre à jour / mise à jour partielle / supprimer :**

```bash
curl localhost:8080/api/posts/1                       # retrieve  → 200
curl -X PUT   localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Edited","content":"new body","status":"published","author_id":1}'   # full update → 200
curl -X PATCH localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Just the title"}'                   # partial update → 200 (other fields untouched)
curl -X DELETE localhost:8080/api/posts/1              # destroy → 204
```

La validation du `PATCH` s'exécute sur ce que vous envoyez ; les champs en lecture seule restent à leur valeur
serveur même s'ils sont postés.

### Étape 8 — Filtrer, rechercher, trier, paginer

Tout sur l'endpoint de liste, sans code supplémentaire (vous avez déclaré les champs à l'Étape 5) :

```bash
curl 'localhost:8080/api/posts?status=published&author_id=1'      # filter
curl 'localhost:8080/api/posts?status__in=published,archived'     # lookup
curl 'localhost:8080/api/posts?search=rustango'                   # search title+body
curl 'localhost:8080/api/posts?ordering=title'                    # sort (asc)
curl 'localhost:8080/api/posts?page=2&page_size=10'               # paginate
```

### Étape 9 — Le tester

Le framework fournit un client de test in-process — faites des assertions sur de vraies réponses HTTP
sans démarrer de serveur :

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

Voilà une ressource REST complète et validée. Le reste de cette page est la
référence derrière chaque étape.

---

## Le mariage du sérialiseur : entrée + sortie

Brancher un sérialiseur (via `serializer = …` sur le derive, ou `.serializer::<S>()`
sur le builder) change **les deux** directions. Cela fonctionne sur PostgreSQL, MySQL et
SQLite de la même manière.

### Sortie — les réponses sont rendues à travers le sérialiseur

Les réponses de `list`, `retrieve`, `create` et `update` sont produites par
`S::from_model(&row)`, de sorte que les surcharges du sérialiseur façonnent le JSON :

| Champ du sérialiseur | Effet sur la réponse |
|---|---|
| `#[serializer(source = "body")]` | la colonne `body` est émise sous le nom du champ (p. ex. `content`) |
| `#[serializer(method = "fn")]` | un champ calculé apparaît (depuis `Self::fn(&model)`) |
| `#[serializer(read_only)]` | inclus dans la sortie |
| `#[serializer(write_only)]` | **omis** de la sortie |

> **Mise en garde `nested` / `many`.** Les champs de sérialiseur imbriqués et de collection ne sont rendus
> que lorsque les lignes liées ont été chargées (via `select_related` / une récupération
> anticipée) ; sinon ils reviennent à leur valeur par défaut. La requête de liste automatique du ViewSet
> charge la ligne de base — branchez les relations explicitement si un champ imbriqué doit
> être renseigné.

### Entrée — les requêtes sont validées et filtrées

Sur `create` et `update`, lorsqu'un sérialiseur est enregistré :

1. **La validation s'exécute.** Le `validate()` du sérialiseur — chaque
   `#[serializer(validate = "fn")]` par champ plus le `validate` transversal au niveau
   du conteneur — s'exécute contre le corps JSON. En cas d'échec, la requête est rejetée
   `400 Bad Request` avec la forme d'erreur DRF : un objet JSON indexé par nom de champ
   avec des tableaux de messages, p. ex. `{"title":["title must be at least 3 characters"]}`.
2. **Filtrage des champs modifiables.** Seuls les champs modifiables du sérialiseur sont
   persistés ; les champs `read_only` et `method`/calculés qu'un client poste sont
   **ignorés** (non écrits), et les renommages `source` sont résolus vers la colonne du
   modèle. Ainsi un client ne peut pas définir un champ contrôlé par le serveur en l'incluant dans
   le corps.

> **Les corps form-urlencoded** (par opposition à JSON) sautent `validate()` — il n'y a pas de valeur
> typée à valider — mais bénéficient tout de même du filtrage des champs modifiables.

Sous le capot, ce sont les méthodes `validate()`,
`writable_source_fields()` et `from_writable_json()` du trait `ModelSerializer`, toutes générées par
`#[derive(Serializer)]`. Voir le [guide des sérialiseurs](serializers.md) pour savoir comment
écrire les validateurs.

---

## Les deux façons de définir un ViewSet

Les deux produisent un `axum::Router` des mêmes routes CRUD.

**1. La macro derive** — déclarative, mono-locataire ; branchez un sérialiseur avec
`serializer = …` :

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

**2. Le builder** — `ViewSet::for_model(...)`, programmatique, tri-dialecte
(PostgreSQL / SQLite / MySQL) et conscient de la multi-location ; branchez un sérialiseur avec
`.serializer::<S>()` :

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

Optez pour le builder lorsque vous avez besoin de SQLite/MySQL, de la multi-location, d'une
config construite au runtime, ou des extras (throttling, backends de filtre personnalisés, pagination par curseur).

---

## Les endpoints CRUD

Le montage sur `/api/posts` branche les six opérations REST :

| Verbe | Chemin | Action | Succès | Corps |
|---|---|---|---|---|
| `GET` | `/api/posts` | **list** | 200 | enveloppe paginée (voir [Pagination](#pagination)) |
| `POST` | `/api/posts` | **create** | 201 | l'objet créé — *ou un tableau, pour la création en masse* |
| `GET` | `/api/posts/{pk}` | **retrieve** | 200 | l'objet |
| `PUT` | `/api/posts/{pk}` | **update** (complète) | 200 | l'objet mis à jour |
| `PATCH` | `/api/posts/{pk}` | **partial update** | 200 | l'objet mis à jour (seuls les champs fournis changent) |
| `DELETE` | `/api/posts/{pk}` | **destroy** | 204 | vide |

Une barre oblique finale sur le préfixe de montage est optionnelle. Seuls ces six verbes sont
branchés — pas de `HEAD`/`OPTIONS` automatique. La **création en masse** est gratuite : faites un `POST` d'un
*tableau* JSON et chaque élément est inséré dans l'ordre, validé de manière atomique (un seul élément invalide
rejette tout le lot).

---

## Choisir quelles opérations exposer

Pour une ressource en **lecture seule** (list + retrieve uniquement), ajoutez `read_only` :

```rust
#[viewset(model = Post, read_only)]            // macro
ViewSet::for_model(Post::SCHEMA).read_only()   // builder
```

Il n'y a pas de bascule par verbe au-delà de read_only. Pour « tout sauf delete »,
montez le ViewSet et surchargez la route unique avec votre propre handler (voir
[Actions personnalisées](#custom-actions-beyond-crud)).

---

## Référence de l'attribut `#[viewset(...)]`

| Clé | Exemple | Défaut | Ce qu'elle fait |
|---|---|---|---|
| `model` | `model = Post` | **requis** | Le modèle sur lequel la ressource est construite. |
| `serializer` | `serializer = path::To::S` | aucun | Brancher un sérialiseur pour une **sortie + entrée** typées (voir [ci-dessus](#the-serializer-marriage-input--output)). |
| `fields` | `"id, title, body"` | tous les champs scalaires | Liste blanche pour la projection par défaut (non-sérialiseur) + les champs modifiables. |
| `filter_fields` | `"author_id, status"` | aucun | Champs filtrables via `?field=value` (+ lookups). |
| `search_fields` | `"title, body"` | aucun | Champs que la boîte `?search=` fait correspondre (OU insensible à la casse). |
| `ordering` | `"-published_at, id"` | aucun | Tri par défaut (`-` = DESC). |
| `page_size` | `20` | 20 | Lignes par page (le `?page_size=` du client est plafonné à 1000). |
| `read_only` | *(drapeau)* | désactivé | N'exposer que GET (list + retrieve). |
| `permissions(...)` | `permissions(create = "post.add")` | aucun | Codenames de permission par action. |

---

## Référence du builder

Chaque méthode sur `ViewSet::for_model(SCHEMA)` (chacune renvoie `Self`) :

| Méthode | But |
|---|---|
| `serializer::<S>()` | Brancher un sérialiseur pour une sortie + entrée typées (tri-dialecte). |
| `fields(&["…"])` | Liste blanche de la projection par défaut + des champs modifiables (sans sérialiseur). |
| `filter_fields(&["…"])` | Activer le filtrage `?field=value`. |
| `search_fields(&["…"])` | Activer `?search=`. |
| `ordering(&[("field", desc)])` | Ordre de tri par défaut. |
| `ordering_fields(&["…"])` | Liste blanche des champs que `?ordering=` peut utiliser. |
| `page_size(n)` | Taille de page par défaut (≤ 1000). |
| `read_only()` | GET uniquement. |
| `permissions(ViewSetPerms{…})` / `permissions_for_model::<T>()` | Barrières de codename par action (cette dernière sur la multi-location). |
| `cursor_pagination("id")` / `cursor_pagination_desc("id")` | Pagination par keyset (saute `COUNT(*)`). |
| `limit_offset_pagination()` | Fenêtrage `?limit=&offset=`. |
| `pagination(PaginationStyle::…)` | Définir le style explicitement. |
| `filter_backend(closure)` | Ajouter des prédicats `WHERE` personnalisés au-delà de `filter_fields`. |
| `throttle(…)` / `throttle_all(max, secs)` | Limites de débit à fenêtre fixe par action. |
| `router(prefix, pgpool)` | Monter (Postgres, pool statique). |
| `router_pool(prefix, pool)` | Monter en tri-dialecte (PG / SQLite / MySQL). |
| `tenant_router(prefix)` | *(multi-location)* monter avec résolution du locataire par requête. |

---

## Filtrage, recherche et tri

Tout est piloté par les paramètres de requête sur l'endpoint de **liste**.

**Filtrage** — chaque entrée de `filter_fields` accepte `?field=value` (exact) plus
les lookups de style Django via un `__suffix` :

```
?status=published
?author_id__in=1,2,3
?published_at__gte=2026-01-01
?title__icontains=rust
?body__isnull=false
```

Lookups pris en charge : `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `not_in`, `contains`,
`icontains`, `startswith`, `istartswith`, `endswith`, `iendswith`, `isnull`
(pas de suffixe = exact). Les champs absents de `filter_fields` sont ignorés.

**Recherche** — `?search=term` fait correspondre `search_fields` avec un OU insensible à la casse.

**Tri** — `?ordering=field,-other` (`-` = DESC). N'importe quel champ est triable
sauf si vous définissez `.ordering_fields([...])` pour le restreindre. Sans paramètre, le
tri par défaut `ordering` s'applique. Ils se composent tous.

---

## Pagination

> **Piège — paginez sur un ordre déterministe.** La pagination par numéro de page et par
> limit/offset suppose un tri stable ; trier sur une colonne non unique
> (ou aucune) laisse les lignes se décaler entre les pages — dupliquées ou sautées. Ajoutez toujours
> un départageur unique, p. ex. `ordering = "-published_at, id"`. (Les deux exécutent aussi
> un `COUNT(*)` par appel ; la pagination par curseur le saute pour les grandes tables.)

Trois styles ; le numéro de page est le défaut. L'enveloppe de liste diffère selon le style :

**Numéro de page** (défaut) — `?page=2&page_size=20` :

```json
{ "count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ] }
```

**Curseur** — `.cursor_pagination("id")` (ou `_desc`) ; saute `COUNT(*)`, idéal
pour les très grandes tables. `?cursor=<token>&page_size=20` :

```json
{ "page_size": 20, "next": "<opaque-cursor-or-null>", "results": [ … ] }
```

**Limit/offset** — `.limit_offset_pagination()`. `?limit=20&offset=40` :

```json
{ "count": 137, "limit": 20, "offset": 40, "results": [ … ] }
```

`page_size` / `limit` sont bornés à 1000.

---

## Validation

Avec un **sérialiseur branché**, le chemin create/update exécute les
validateurs du sérialiseur et renvoie des `400` en forme DRF — la manière recommandée de valider (voir
[le mariage](#the-serializer-marriage-input--output) et le
[guide des sérialiseurs](serializers.md#validation)). Trois couches s'exécutent :

- **Contraintes déclaratives** — `max_length` / `min_length` / `min` / `max`, et
  par défaut le champ **hérite des** `max_length` / `min` / `max` /
  `choices` **du modèle**. Ainsi une colonne `#[rustango(max_length = 200)]` voit sa longueur vérifiée sur
  l'API sans configuration supplémentaire (comportement du `ModelSerializer` de DRF), transformant des
  `500` de contrainte-BDD potentielles en `400` conviviales comme
  `{"title":["Ensure this value has at most 200 characters."]}`.
- **Par champ** `validate = "fn"` et un hook `validate` **transversal** — vos
  règles personnalisées (formats, inter-champs, logique métier).

Indépendamment d'un sérialiseur, le chemin d'écriture applique toujours le **schéma** :

- **Les types sont coercés et vérifiés** — une valeur `i64` / `DateTime` / `Uuid` / `bool`
  invalide est une `400` nommant le champ.
- **Requis / NOT NULL** — un champ non-nullable manquant (ou une chaîne vide pour une
  `String` non-nullable) est une `400` ; les champs nullables acceptent le vide → `NULL`.
- **Contraintes de base de données** — unique, clés étrangères et contraintes check apparaissent
  comme une `400` sur INSERT/UPDATE.

Ainsi, même sans sérialiseur, vous obtenez une validation de type + requis + contrainte-BDD ;
branchez un sérialiseur pour obtenir les vérifications déclaratives de longueur/plage/choix (héritées automatiquement)
plus vos propres règles par champ et inter-champs.

---

## Permissions et throttling

> **Un ViewSet est public par défaut.** En monter un expose les six verbes CRUD
> à n'importe qui — il n'y a pas d'authentification intégrée. Protégez-le avec `permissions(...)`
> (ci-dessous), placez-le derrière le [middleware d'auth](auth-backends.md) (`require_auth`),
> ou les deux, avant d'exposer les écritures.

**Les permissions** protègent chaque action par des codenames (OU au sein d'une action) :

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

Une liste d'action vide = pas de vérification. L'application lit un utilisateur authentifié depuis
la requête (l'intégration d'auth `tenancy`) ; les superusers contournent, un utilisateur manquant
est refusé. `.permissions_for_model::<Post>()` remplit automatiquement les codenames standard
`post.view`/`add`/`change`/`delete`.

**Le throttling** applique des limites à fenêtre fixe par client, par action :

```rust
ViewSet::for_model(Post::SCHEMA)
    .throttle_all(60, 60)              // 60 requests / 60s per client, every action
    .router_pool("/api/posts", pool);
```

Au-dessus de la limite → `429 Too Many Requests` + `Retry-After`. Les compteurs sont par processus ;
la clé du client est l'IP de connexion (ou `X-Forwarded-For` / `X-Real-IP`).

---

## Actions personnalisées au-delà du CRUD

Il n'y a pas de décorateur `@action` de DRF — le ViewSet est strictement les six routes
CRUD. Pour des endpoints supplémentaires, montez vos propres handlers aux côtés du ViewSet :

```rust
use axum::{Router, routing::{get, post}};

let api = Router::new()
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    .route("/api/posts/stats", get(post_stats))
    .route("/api/posts/bulk_archive", post(bulk_archive));
```

Pour une logique `WHERE` supplémentaire, `.filter_backend(…)` apporte des prédicats sans une
route séparée.

### Restreindre les lignes au principal authentifié

Un backend s'exécute sur **chaque** action — `list`, `retrieve`, `update`, `destroy` —
il se comporte donc comme le `get_queryset()` de DRF. Une ligne que le backend exclut est un
**404** sur les routes d'item, pas un 403 : un 403 confirmerait que l'id existe.

L'identité doit provenir de la credential, jamais de la chaîne de requête. Un
filtre `?owner_id=` n'est pas une portée — c'est un paramètre que l'appelant choisit.

#### `OwnedBy` — le backend fourni

La plupart des ressources possédées ont besoin d'exactement une règle : *les lignes dont la colonne de propriété est
l'appelant*. Nommez la colonne et montez-le.

```rust
use rustango::viewset::{OwnedBy, ViewSet};

ViewSet::for_model(Note::SCHEMA)
    .filter_backend(OwnedBy::column("member_id"))
    .tenant_router("/api/notes")
    .layer(axum::middleware::from_fn(
        rustango::tenancy::auth_routes::require_bearer,
    ))
```

N'importe quelle colonne fonctionne — `owner_id`, `member_id`, `author_id` — parce que le backend
prend le nom plutôt que de supposer une convention. Il échoue de manière fermée sur les deux
façons dont il peut être erroné : une requête non authentifiée et une colonne que le modèle ne
possède pas correspondent toutes deux à **rien**, de sorte qu'une faute de frappe au moment du montage ne peut pas se transformer en
« aucun prédicat, renvoyer la table ».

Les superusers ne sont pas spéciaux par défaut ; `.superuser_sees_all()` active l'option, parce que
« les admins voient tout » est une décision produit, pas une décision du framework.

#### D'où vient l'identité

[`Principal`] est le seul type d'identité, résolu depuis ce qui a vérifié la
requête — un `Principal` explicite, un `AuthenticatedUser` laissé par une session ou
un middleware Bearer, ou un token d'agent MCP (qui agit en tant que l'utilisateur qui l'a émis).
Il n'authentifie rien lui-même ; il ne lit que ce qu'un middleware de vérification
a déjà prouvé, de sorte que rien ne peut en insérer un sans vérifier d'abord une credential.

`require_bearer` est ce middleware pour une API JSON : il vérifie le token d'accès
contre le locataire résolu, relit la ligne utilisateur (un compte désactivé cesse
de fonctionner à la prochaine requête, pas à l'expiration du token), et insère à la fois
`AuthenticatedUser` et `Principal`. Utilisez-le comme extracteur n'importe où :

```rust
use rustango::tenancy::{OptionalPrincipal, Principal};

async fn mine(principal: Principal) -> String {          // 401 when absent
    format!("user {}", principal.user_id)
}

async fn home(OptionalPrincipal(who): OptionalPrincipal) -> String {
    who.map_or("anonymous".into(), |p| format!("user {}", p.user_id))
}
```

#### Écrire votre propre backend

Quand la propriété n'est pas une colonne unique — une équipe partagée, une ligne en suppression douce, une
fenêtre de dates — implémentez le trait et surchargez `filter_with`, qui reçoit
les `Parts` de la requête :

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

`filter_with` a `filter` par défaut, de sorte qu'un backend qui n'a pas besoin de la requête
— y compris la forme de closure simple — n'implémente que `filter` comme avant.

---

## Montage

Composez le routeur du ViewSet dans votre app. Mono-locataire, pool statique :

```rust
let api = urls::api()
    .merge(PostViewSet::router("/api/posts", pool.clone()))                          // macro
    .merge(ViewSet::for_model(Author::SCHEMA).router_pool("/api/authors", pool.clone())); // builder
```

Multi-locataire (aucun pool capturé — chaque requête résout sa connexion de locataire) :

```rust
let api = urls::api()
    .merge(ViewSet::for_model(Post::SCHEMA).tenant_router("/api/posts"));
```

`make:api_routes <app>` génère un `api()` par app qui rassemble ces
lignes `.merge(...)` ; branchez-le dans votre `urls.rs` de niveau supérieur.

---

## Prise en charge des backends

- **Le builder + `router_pool` / `tenant_router`** est **tri-dialecte** — PostgreSQL,
  SQLite et MySQL — et c'est le chemin recommandé.
- **Le `router(prefix, PgPool)` de la macro derive** capture un `PgPool` (PostgreSQL).
- **L'entrée + sortie du sérialiseur** fonctionne désormais sur **les trois backends** (le
  rendu par ligne est tri-dialecte ; l'ancienne barrière PG-uniquement a disparu).
- Le filtrage, la recherche, le tri, les trois modes de pagination, les permissions,
  le throttling et la création en masse fonctionnent tous à travers les backends pris en charge sur le
  chemin builder.

---

## Essayez-le

Le flux de bout en bout ci-dessus reflète l'exemple compilable `getting_started_blog`
(Étapes 12–13 du [guide de prise en main](getting-started.md)). Les
propres tests live du framework sous `crates/rustango/tests/viewset_*.rs` sont la
référence exécutable la plus complète — y compris les tests d'entrée/sortie du sérialiseur.
Ils s'exécutent sur SQLite en mémoire mais nécessitent les feature flags correspondants, p. ex. :

```bash
cd crates/rustango
cargo test --features sqlite,tenancy --test viewset_serializer_render_sqlite_live
cargo test --features sqlite,tenancy --test viewset_serializer_input_sqlite_live
cargo test --features sqlite,tenancy --test viewset_sqlite_live
```

---

## Voir aussi

- [Sérialiseurs](serializers.md) — façonner le JSON qu'un ViewSet envoie et valide.
- [Vues HTML](html-views.md) — la contrepartie rendue côté serveur de cette API JSON.
- [OpenAPI](openapi.md) — générer une spec + Swagger UI depuis vos ViewSets.
- [URLs & routage](urls.md) — composer les routeurs de ViewSet dans votre app.
