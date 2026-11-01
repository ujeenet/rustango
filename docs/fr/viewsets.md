# ViewSets — API REST CRUD

Un ViewSet transforme un modèle en une ressource REST complète — des endpoints pour **lister,
créer, lire, mettre à jour et supprimer** des enregistrements — à partir d'une seule déclaration. Vous
décrivez le modèle et le contrat souhaité ; le routage, la pagination et la
sérialisation sont fournis par le framework.

> **Nouveau dans les API REST ?** Ce guide suppose que vous savez ce qu'est un *endpoint*, un *verbe
> HTTP* (GET / POST / …) et une *requête et réponse JSON*. Si l'un de ces concepts
> est flou, le [glossaire](glossary.md#les-bases-des-api-web) en fait un tour d'horizon de cinq minutes —
> lisez-le d'abord, puis revenez ici.

Associez un ViewSet à un [sérialiseur](serializers.md) — la pièce qui façonne votre
JSON — et il protège **les deux directions** à la fois : le sérialiseur formate chaque
**réponse** (renommer, masquer, calculer ou imbriquer des champs) *et* régit chaque
**requête** (il valide les données entrantes et ignore silencieusement les champs qu'un client
ne devrait pas être autorisé à définir). Les entrées rejetées reviennent dans une forme facile à lire —
un objet JSON indexé par nom de champ. Tout fonctionne de la même manière sur PostgreSQL,
MySQL et SQLite.

Ce guide est avant tout un tutoriel : nous **construisons une API REST de blog complète** de bout en bout —
échafaudage, modèles, un sérialiseur, le ViewSet, les six endpoints CRUD, la validation
des entrées, le filtrage/la recherche/la pagination, et les tests — puis le reste de la page
est une référence pour chaque réglage.

[![Un ViewSet Rustango branché sur un sérialiseur : un seul bloc #[viewset(serializer = …)] fournit une sortie JSON typée et une entrée validée sur les six routes CRUD](../img/viewsets.png)](../img/viewsets.png)

> **Source :** `rustango::viewset` (`ViewSet`, `#[derive(ViewSet)]`, les
> options `#[viewset(...)]` + le builder `for_model`) — conditionné à `admin` **ou** `tenancy`. À l'intérieur, `.serializer::<S>()` exige `serializer`, le `router()` du builder exige `postgres`, `tenant_router()` / `OwnedBy` exigent `tenancy`, et l'action QUERY exige `admin`.
>
> **Version exécutable :** le blog construit ici reflète l'exemple testé et compilable
> [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog)
> (ses `Post` / `PostSerializer` / `PostViewSet`), et chaque comportement est
> verrouillé par les propres tests live du framework — `crates/rustango/tests/viewset_*.rs`
> (notamment `viewset_serializer_render_sqlite_live` et
> `viewset_serializer_input_sqlite_live`).

---

## Table des matières
- [Vues API vs vues HTML](#vues-api-vs-vues-html) — du JSON pour les clients, ou des pages HTML ?
- [Construire une API REST de blog](#construire-une-api-rest-de-blog) — la présentation complète
- [Le mariage du sérialiseur : entrée + sortie](#le-mariage-du-sérialiseur--entrée--sortie)
- [Les deux façons de définir un ViewSet](#les-deux-façons-de-définir-un-viewset)
- [Les endpoints CRUD](#les-endpoints-crud) · [Choisir lesquels exposer](#choisir-quelles-opérations-exposer)
- [Référence `#[viewset(...)]`](#référence-de-lattribut-viewset) · [Référence du builder](#référence-du-builder)
- [Filtrage, recherche & tri](#filtrage-recherche-et-tri) · [Pagination](#pagination)
- [Validation](#validation) · [Permissions & throttling](#permissions-et-throttling) · [Actions personnalisées](#actions-personnalisées-au-delà-du-crud)
- [Montage](#montage) · [Backends](#prise-en-charge-des-backends)

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
| Sur entrée invalide | `400` [`ApiError`](#formes-de-réponse-en-erreur) ; erreurs du sérialiseur `422`, champs dans `details` | re-rendu du formulaire avec les erreurs affichées |
| Un « list » est | une enveloppe JSON paginée | une boucle sur les lignes dans votre template |
| Généralement authentifiée par | tokens / JWT / clés d'API | cookies de session |

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

Les apps sont des modules de fonctionnalités autonomes :

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

Le sérialiseur définit le contrat requête/réponse de l'API.
Générez le squelette :

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
également, renvoyant toutes des `422` conviviales à l'écriture. Ajoutez les attributs de sérialiseur `max_length` / `min_length` /
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
    fields        = "id, title, body, author_id, published_at",
    serializer    = crate::blog::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
```

Le générateur écrit `src/post_view_set.rs` (il démarre en `read_only`, et sa
ligne `use` suppose `crate::models::Post`). Déplacez-le vers
`src/blog/post_view_set.rs` comme montré et ajoutez `pub mod post_view_set;` à
`src/blog/mod.rs`.

> `fields` est obligatoire : c'est la liste blanche d'écriture, donc un body ne
> peut fixer aucune autre colonne (par exemple `author_id`). Un sérialiseur la
> restreint encore ; il ne l'élargit jamais.

### Étape 6 — Monter les routes

Dans un projet mono-locataire, imbriquez le routeur du ViewSet sous un chemin, en passant le
pool :

```rust
// src/blog/urls.rs (or your urls::api aggregator)
use axum::Router;
use rustango::sql::Pool;
use crate::blog::post_view_set::PostViewSet;

pub fn api(pool: Pool) -> Router {
    Router::new()
        .merge(PostViewSet::router("/api/posts", pool))
}
```

`make:api_routes blog` échafaude la même forme dans `src/blog/api_routes.rs` si
vous préférez le générer. Le `urls::api()` de niveau supérieur n'a pas de pool,
donc montez ce routeur depuis `src/main.rs`, où la `Cli` vous donne le pool du
serveur. Un nest à la racine est refusé : imbriquez-le sous un préfixe et retirez
ce préfixe du chemin du ViewSet :
`.nest_with("/api", |pool| myapp::blog::urls::api(pool))` avec
`PostViewSet::router("/posts", pool)` à l'intérieur.

### Étape 7 — Le lancer et exercer chaque endpoint

```bash
cargo run            # listening on http://127.0.0.1:8080
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

**La validation rejette les entrées invalides** avec une `422` —
des tableaux de messages indexés par champ, sous `details` :

```bash
curl -i -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"hi","content":"x","author_id":1}'
# HTTP/1.1 422 Unprocessable Entity
# {"error":"validation_failed","message":"invalid input","status":422,
#  "details":{"title":["title must be at least 3 characters"]}}
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
use rustango::sql::Pool;
use serde_json::json;

async fn app() -> axum::Router {
    // `Pool::connect` picks the backend from the URL scheme, so the
    // same test runs against sqlite, Postgres or MySQL.
    let pool = Pool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn rejects_short_title() {
    let client = TestClient::new(app().await);
    let res = client.post("/api/posts")
        .json(&json!({"title":"hi","content":"x","author_id":1}))
        .send().await;
    assert_eq!(res.status, 422);
    assert!(res.json_value()["details"]["title"].is_array());   // field-keyed error shape
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
   `422 Unprocessable Entity` (`validation_failed`) avec la forme d'erreur indexée par champ
   dans `details` : un objet JSON indexé par nom de champ avec des tableaux de messages, p. ex. `{"title":["title must be at least 3 characters"]}`.
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
    fields        = "id, title, body, author_id, published_at",
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

let router = ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"])
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

Une barre oblique finale sur le préfixe de montage est optionnelle. Ces six verbes sont branchés,
plus une action de collection `QUERY` (RFC 10008) dès que la fonctionnalité
`admin` est activée. Les routes sont construites avec `axum::routing::get`, donc
axum répond au `HEAD` depuis le handler `GET` ; `OPTIONS` n'est pas branché. La **création en masse** est gratuite : faites un `POST` d'un
*tableau* JSON et chaque élément est inséré dans l'ordre, **au sein d'une transaction**.
Un seul élément invalide rejette tout le lot et **ne laisse rien derrière** — qu'il soit
rattrapé par la validation ou par la base de données.

> Cette seconde moitié était fausse autrefois.
> La validation était atomique ; les écritures étaient un `INSERT` chacune sans
> transaction, si bien qu'une violation d'unicité ou de clé étrangère sur l'élément 5
> validait les éléments 0–4, renvoyait `400 bulk entry 5` et ne nommait aucune des
> lignes créées. Ces violations sont précisément la classe que la validation ne peut
> pas trancher en amont.

Un lot contient au plus 1000 lignes (`max_bulk_create(n)` ; au-delà, c'est une
`413`), et chaque ligne consomme une unité de throttle `create`.

Sur un modèle `#[rustango(soft_delete)]`, `DELETE` horodate la colonne au lieu
de supprimer, et chaque action traite une ligne supprimée en douceur comme
disparue.

---

## Choisir quelles opérations exposer

Pour une ressource en **lecture seule** (list + retrieve uniquement), ajoutez `read_only` :

```rust
#[viewset(model = Post, fields = "id, title", read_only)]    // macro
ViewSet::for_model(Post::SCHEMA, &["id", "title"]).read_only() // builder
```

Il n'y a pas de bascule par verbe au-delà de read_only. Pour « tout sauf delete »,
montez le ViewSet et surchargez la route unique avec votre propre handler (voir
[Actions personnalisées](#actions-personnalisées-au-delà-du-crud)).

---

## Référence de l'attribut `#[viewset(...)]`

| Clé | Exemple | Défaut | Ce qu'elle fait |
|---|---|---|---|
| `model` | `model = Post` | **requis** | Le modèle sur lequel la ressource est construite. |
| `serializer` | `serializer = path::To::S` | aucun | Brancher un sérialiseur pour une **sortie + entrée** typées (voir [ci-dessus](#le-mariage-du-sérialiseur--entrée--sortie)). |
| `fields` | `"id, title, body"` | **obligatoire** | Les seuls champs que les réponses affichent et que les bodies peuvent écrire ; les autres clés sont ignorées. Un sérialiseur la restreint. |
| `filter_fields` | `"author_id, status"` | aucun | Champs filtrables via `?field=value` (+ lookups). |
| `search_fields` | `"title, body"` | aucun | Champs que la boîte `?search=` fait correspondre (OU insensible à la casse). |
| `ordering` | `"-published_at, id"` | aucun | Tri par défaut (`-` = DESC). |
| `page_size` | `20` | 20 | Lignes par page (le `?page_size=` du client est plafonné à 100). |
| `read_only` | *(drapeau)* | désactivé | N'exposer que GET (list + retrieve). |
| `allow_anonymous` | *(drapeau)* | désactivé | Indique que les actions d'écriture sans codename sont voulues ouvertes ; fait taire l'avertissement au montage. |
| `permissions(...)` | `permissions(create = "posts.add")` | aucun | Codenames de permission par action. |

---

## Référence du builder

`ViewSet::for_model(SCHEMA, &["id", "title"])` prend la liste des champs : les seuls
que les réponses affichent et que les bodies peuvent écrire. Chaque autre méthode renvoie `Self` :

| Méthode | But |
|---|---|
| `serializer::<S>()` | Brancher un sérialiseur pour une sortie + entrée typées (tri-dialecte). |
| `filter_fields(&["…"])` | Activer le filtrage `?field=value`. |
| `search_fields(&["…"])` | Activer `?search=`. |
| `ordering(&[("field", desc)])` | Ordre de tri par défaut. |
| `ordering_fields(&["…"])` | Liste blanche des champs que `?ordering=` peut utiliser. |
| `page_size(n)` | Taille de page par défaut (≤ 100). |
| `max_page_size(n)` | Relève ou abaisse le plafond client lui-même (100 par défaut). |
| `max_bulk_create(n)` | Nombre maximal de lignes qu'une création groupée peut porter (1000 par défaut). |
| `pk_param(name)` | Renomme le paramètre de chemin des routes de détail (`pk_param_name()` le relit). |
| `openapi_query(bool)` | Décrit la route `QUERY` dans le document OpenAPI généré (demande un outillage OpenAPI 3.2). Désactivé par défaut ; la route est montée dans tous les cas. |
| `related_filter(fk_field, backend)` | Limite les lignes cibles qu'une FK du corps peut nommer ; toute autre donne un `422`. |
| `read_only()` | GET uniquement. |
| `allow_anonymous()` | Les écritures sans codename sont voulues ; évite l'avertissement au montage. |
| `permissions(ViewSetPerms{…})` / `permissions_for_model::<T>()` | Barrières de codename par action (cette dernière sur la multi-location). |
| `cursor_pagination("id")` / `cursor_pagination_desc("id")` | Pagination par keyset (saute `COUNT(*)`). Toute colonne NOT NULL totalement ordonnée : entier, horodatage, date, uuid ou chaîne. |
| `limit_offset_pagination()` | Fenêtrage `?limit=&offset=`. |
| `pagination(PaginationStyle::…)` | Définir le style explicitement. |
| `filter_backend(closure)` | Ajouter des prédicats `WHERE` personnalisés au-delà de `filter_fields`. |
| `throttle(…)` / `throttle_all(max, secs)` | Limites de débit à fenêtre fixe par action ; `QUERY` consomme le budget de la liste. |
| `skip_object_permissions()` | Ignore les hooks de permission objet et les scopes de queryset de l'application, pour une API qui protège ses lignes elle-même. Les hooks du framework s'exécutent toujours. |
| `view_hook_scan_cap(n)` | Nombre maximal de lignes qu'une liste lit pour le hook `"view"` (10 000 par défaut) ; au-delà, `count` et `last_page` valent `null`. |
| `router(prefix, pgpool)` | Monter (Postgres uniquement, `PgPool` statique ; feature `postgres`). |
| `router_pool(prefix, pool)` | Monter en tri-dialecte (PG / SQLite / MySQL). |
| `tenant_router(prefix)` | *(multi-location)* monter avec résolution du locataire par requête. |

---

## Filtrage, recherche et tri

Tout est piloté par les paramètres de requête sur l'endpoint de **liste**.

**Filtrage** — chaque entrée de `filter_fields` accepte `?field=value` (exact) plus
les lookups via un `__suffix` :

```
?status=published
?author_id__in=1,2,3
?published_at__gte=2026-01-01
?title__icontains=rust
?body__isnull=false
```

Lookups pris en charge : `iexact`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `not_in`,
`range` (`lo,hi`), `contains`, `icontains`, `startswith`, `istartswith`,
`endswith`, `iendswith`, `isnull`, et les parties de date `year`, `month`, `day`,
`hour`, `minute`, `second`, `quarter`, `week`, `week_day`, `date`, suivies en
option d'une comparaison (`created_at__date__gte=2026-01-01`). Pas de suffixe =
exact. Une date simple sur un `__gte` / `__lte` de datetime couvre toute cette
journée UTC.

Une valeur vide n'est pas un filtre. Un lookup inconnu ou une valeur qui ne se
parse pas est une `400` qui nomme le paramètre ; avec un `filter_backend`
enregistré, un lookup inconnu est laissé au backend. Les lookups LIKE demandent
un champ chaîne. Les champs absents de `filter_fields` sont ignorés.

**Recherche** — `?search=term` fait correspondre `search_fields` avec un OU insensible à la casse.

**Tri** — `?ordering=field,-other` (`-` = DESC). Tout champ que la réponse montre
(avec un sérialiseur, les champs qu'il rend) est triable sauf si vous définissez `.ordering_fields([...])` pour le restreindre. Sans paramètre, le
tri par défaut `ordering` s'applique, sinon le `default_order` du modèle ; la
clé primaire départage toujours les égalités. Ils se composent tous.

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
pour les très grandes tables. Le champ doit être **totalement ordonné** : un
entier, un horodatage, une date, un uuid ou une chaîne. `"id"` est le choix
habituel ; un horodatage `created_at` est l'autre, et c'est ce qu'il vous faut
sur une table en ajout seul. Une colonne float, bool, json ou blob panique à la
construction plutôt que d'échouer à chaque requête.
`?cursor=<token>&page_size=20` :

```json
{ "page_size": 20, "next": "<opaque-cursor-or-null>", "results": [ … ] }
```

**Limit/offset** — `.limit_offset_pagination()`. `?limit=20&offset=40` :

```json
{ "count": 137, "limit": 20, "offset": 40, "results": [ … ] }
```

`page_size` / `limit` sont bornés à `max_page_size` (100 par défaut).

---

## Validation

Avec un **sérialiseur branché**, le chemin create/update exécute les
validateurs du sérialiseur et renvoie des `422` indexées par champ — la manière recommandée de valider (voir
[le mariage](#le-mariage-du-sérialiseur--entrée--sortie) et le
[guide des sérialiseurs](serializers.md#validation)). Trois couches s'exécutent :

- **Contraintes déclaratives** — `max_length` / `min_length` / `min` / `max`, et
  par défaut le champ **hérite des** `max_length` / `min` / `max` /
  `choices` **du modèle**. Ainsi une colonne `#[rustango(max_length = 200)]` voit sa longueur vérifiée sur
  l'API sans configuration supplémentaire, transformant des
  `500` de contrainte-BDD potentielles en `422` conviviales avec `details` comme
  `{"title":["Ensure this value has at most 200 characters."]}`.
- **Par champ** `validate = "fn"` et un hook `validate` **transversal** — vos
  règles personnalisées (formats, inter-champs, logique métier).

Indépendamment d'un sérialiseur, le chemin d'écriture applique toujours le **schéma** :

- **Les types sont coercés et vérifiés** — une valeur `i64` / `DateTime` / `Uuid` / `bool`
  invalide est une `400` nommant le champ.
- **Requis / NOT NULL** — un champ non-nullable manquant (ou une chaîne vide pour une
  `String` non-nullable) est une `400` ; les champs nullables acceptent le vide → `NULL`.
  Dans un corps JSON seul `null` vaut `NULL` : `""` sur une `String` nullable reste `""`.
- **Nombres** — NaN et l'infini donnent une `400` ; un décimal envoyé comme nombre
  JSON garde tous ses chiffres.
- **Contraintes de base de données** — une clé dupliquée est une `409` (`conflict`) ;
  les clés étrangères et les contraintes check apparaissent comme une `400` sur
  INSERT/UPDATE.

Ainsi, même sans sérialiseur, vous obtenez une validation de type + requis + contrainte-BDD ;
branchez un sérialiseur pour obtenir les vérifications déclaratives de longueur/plage/choix (héritées automatiquement)
plus vos propres règles par champ et inter-champs.

### Formes de réponse en erreur

Chaque erreur de ViewSet est un corps [`ApiError`](api-conventions.md), la même
forme que celle de vos propres handlers :

```json
{"error": "<machine code>", "message": "<sentence>", "status": 400}
```

- `error` est un code stable (`bad_request`, `unauthorized`, `not_found`,
  `validation_failed`, `rate_limited`, `internal_error`, …). Branchez dessus.
- La validation du sérialiseur donne un `422` `validation_failed`, avec la map
  par champ dans `details` :
  `{"title": ["Ensure this value has at most 200 characters."], "non_field_errors": [ … ]}`.
  Les `400` de coercition de type et de champ requis ci-dessus sont
  `bad_request`, avec la raison dans `message` ; un `400` de contrainte de base
  de données retient le texte du driver.
- Un `5xx` ne porte jamais la cause, sauf si `RUSTANGO_DISCLOSE_ERRORS` est
  défini. Elle est journalisée ; `message` reste générique.

---

## Permissions et throttling

> **Un ViewSet est public par défaut.** En monter un expose les six verbes CRUD
> à n'importe qui — il n'y a pas d'authentification intégrée. Protégez-le avec `permissions(...)`
> (ci-dessous), placez-le derrière le [middleware d'auth](auth-backends.md) (`require_auth`),
> ou les deux, avant d'exposer les écritures.

**Les permissions** protègent chaque action par des codenames (OU au sein d'une action) :

```rust
use rustango::viewset::{ViewSet, ViewSetPerms};

ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"])
    .permissions(ViewSetPerms {
        list:     vec!["posts.view".into()],
        retrieve: vec!["posts.view".into()],
        create:   vec!["posts.add".into()],
        update:   vec!["posts.change".into()],
        destroy:  vec!["posts.delete".into()],
    })
    .router_pool("/api/posts", pool);
```

Une liste d'action vide = pas de vérification. L'application lit un utilisateur authentifié depuis
la requête (l'intégration d'auth `tenancy`) ; les superusers contournent, un utilisateur manquant
est refusé. `.permissions_for_model::<Post>()` remplit automatiquement les codenames standard
`<table>.view` / `.add` / `.change` / `.delete` (`posts.view`, … pour la table
`posts`).

**Permissions objet.** Un ViewSet exécute les mêmes hooks que l'admin
(`register_object_permission!`, `register_queryset_scope!`) : `add` à la
création (un bulk create est tout ou rien, `403` avec `{"details": {"index": i}}`) ;
`view` à la lecture, la mise à jour et la suppression (masqué donne `404`), puis
`change` ou `delete` (`403`, soft delete compris) ; `view` sur les listes (les
lignes masquées sortent de la page et du `count` ; au-delà de
`view_hook_scan_cap`, `count` et `last_page` valent `null`). Les scopes de
queryset restreignent chaque lecture et écriture ; une écriture dont la ligne en
sortirait donne un `403`. Les hooks lisent la ligne verrouillée dans la
transaction d'écriture ; un identifiant d'un autre tenant donne un `403`. `change` ne vérifie que la
ligne stockée. `object_permissions::Actor::of(&parts)` indique à un hook qui
demande ; un jeton d'agent n'est jamais superuser. `ViewSet::for_model` refuse
`rustango_api_keys`, `rustango_agents` et les tables SSO.

**Le throttling** applique des limites à fenêtre fixe par client, par action :

```rust
ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"])
    .throttle_all(60, 60)              // 60 requests / 60s per client, every action
    .router_pool("/api/posts", pool);
```

Au-dessus de la limite → `429 Too Many Requests` + `Retry-After`. Les compteurs sont par processus ;
la clé du client est l'IP client de confiance (`TrustedRealIp`, sinon le socket ; voir [security.md](security.md)).

---

## Actions personnalisées au-delà du CRUD

Il n'existe aucun attribut pour greffer des routes supplémentaires sur un ViewSet — il est strictement les six routes
CRUD. Pour des endpoints supplémentaires, montez vos propres handlers aux côtés du ViewSet :

```rust
use axum::{Router, routing::{get, post}};

let api = Router::new()
    .merge(ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"]).router_pool("/api/posts", pool.clone()))
    .route("/api/posts/stats", get(post_stats))
    .route("/api/posts/bulk_archive", post(bulk_archive));
```

Pour une logique `WHERE` supplémentaire, `.filter_backend(…)` apporte des prédicats sans une
route séparée.

### Restreindre les lignes au principal authentifié

Un backend s'exécute sur **chaque** action — `list`, `retrieve`, `update`, `destroy` —
il restreint donc le queryset de base sur toutes les routes. Une ligne que le backend exclut est un
**404** sur les routes d'item, pas un 403 : un 403 confirmerait que l'id existe.

L'identité doit provenir de la credential, jamais de la chaîne de requête. Un
filtre `?owner_id=` n'est pas une portée — c'est un paramètre que l'appelant choisit.

Une portée ne restreint que la lecture. Un backend qui possède une colonne implémente
aussi `write_pins` : la création enregistre le propriétaire, la mise à jour ne peut pas le
changer, et `WritePin::Deny` refuse l'écriture avec un 403.

Les portées globales statiques d'un modèle limitent aussi ce qui est lu, pas ce qui est
écrit : une création ou une mise à jour peut sortir de la portée (201 sans corps, ou 204).
Pour une frontière de sécurité, utilisez un backend de filtre avec `write_pins`.

Une écriture ne renvoie sa ligne complète qu'à un appelant qui a la permission
`retrieve` ; les autres reçoivent seulement `{"<pk>": …}` (un par ligne d'un
lot). Une création unitaire envoie aussi un en-tête `Location` qui pointe vers
la route de détail.

Une FK définie par un corps doit nommer une ligne que les portées globales de la
cible montrent ; une mise à jour peut renvoyer la valeur que la ligne contient
déjà. Pour la restreindre davantage, ajoutez un backend pour ce champ ; il
s'exécute sur le modèle cible, et toute autre ligne est une 422 :

```rust
ViewSet::for_model(Comment::SCHEMA, &["id", "body", "post_id"])
    .related_filter("post_id", OwnedBy::column("author_id"))
```

#### `OwnedBy` — le backend fourni

La plupart des ressources possédées ont besoin d'exactement une règle : *les lignes dont la colonne de propriété est
l'appelant*. Nommez la colonne et montez-le.

```rust
use rustango::tenancy::auth_routes::{require_bearer, Config, JwtAuth};
use rustango::viewset::{OwnedBy, ViewSet};

let auth = JwtAuth::new(Config::default()); // un seul, qui sert aussi `auth.router()`
ViewSet::for_model(Note::SCHEMA, &["id", "body"])
    .filter_backend(OwnedBy::column("member_id"))
    .tenant_router("/api/notes")
    .layer(axum::middleware::from_fn_with_state(auth.clone(), require_bearer))
```

N'importe quelle colonne fonctionne — `owner_id`, `member_id`, `author_id` — parce que le backend
prend le nom plutôt que de supposer une convention. Il échoue de manière fermée sur les deux
façons dont il peut être erroné : une requête non authentifiée et une colonne que le modèle ne
possède pas correspondent toutes deux à **rien**, de sorte qu'une faute de frappe au moment du montage ne peut pas se transformer en
« aucun prédicat, renvoyer la table ».

`OwnedBy` épingle sa colonne à l'écriture : la création enregistre l'appelant quoi que dise
le corps, la mise à jour ne déplace jamais la ligne, et une écriture sans principal est un 403.

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
use std::collections::HashMap;

use axum::http::request::Parts;
use rustango::core::{Filter, ModelSchema, Op, SqlValue, WhereExpr};
use rustango::tenancy::Principal;
use rustango::viewset::{match_nothing, ViewSetFilter};

struct OwnerFilter;

impl ViewSetFilter for OwnerFilter {
    // No principal in hand — fail closed. `vec![]` here would be *no filter*,
    // not a filter matching nothing, and would widen the query to every row.
    fn filter(&self, _p: &HashMap<String, String>, schema: &'static ModelSchema) -> Vec<WhereExpr> {
        vec![match_nothing(schema)]
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _p: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        let Some(principal) = Principal::from_parts(parts) else {
            return vec![match_nothing(schema)];
        };
        vec![WhereExpr::Predicate(Filter::new(
            schema.field("owner_id").expect("owner_id").column,
            Op::Eq,
            SqlValue::from(principal.user_id),
        ))]
    }
}

ViewSet::for_model(Note::SCHEMA, &["id", "body"])
    .filter_backend(OwnerFilter)
    .tenant_router("/api/notes")
```

`filter_with` a `filter` par défaut, de sorte qu'un backend qui n'a pas besoin de la requête
— y compris la forme de closure simple — n'implémente que `filter` comme avant.

`match_nothing` est la branche fail-closed, et mieux vaut l'utiliser que la
réécrire : elle renvoie `col IS NULL AND col IS NOT NULL`, une contradiction qui
ne lie aucun paramètre et s'écrit pareil sur tous les backends. Si elle est
exportée, c'est justement parce que le substitut évident est un `Vec` vide — et
dans une API de filtres, `vec![]` signifie *aucun filtre*, l'inverse de ce à quoi
sert la branche.

---

## Montage

Composez le routeur du ViewSet dans votre app. Mono-locataire, pool statique :

```rust
let api = urls::api()
    .merge(PostViewSet::router("/api/posts", pool.clone()))                          // macro
    .merge(ViewSet::for_model(Author::SCHEMA, &["id", "name"]).router_pool("/api/authors", pool.clone())); // builder
```

Multi-locataire (aucun pool capturé — chaque requête résout sa connexion de locataire) :

```rust
let api = urls::api()
    .merge(ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"]).tenant_router("/api/posts"));
```

`make:api_routes <app>` génère un `api()` par app qui rassemble ces
lignes `.merge(...)`. Il prend le pool : montez-le depuis `main.rs` avec
`.nest_with("/api", |pool| myapp::<app>::api_routes::api(pool))` ; celui de
`--tenant` n'en prend pas et se fusionne dans `urls.rs`.

---

## Prise en charge des backends

- **Le builder + `router_pool` / `tenant_router`** est **tri-dialecte** — PostgreSQL,
  SQLite et MySQL — et c'est le chemin recommandé.
- **Le `router(prefix, pool)` de la macro derive** prend `impl Into<rustango::sql::Pool>` — un `PgPool`, `MySqlPool`, `SqlitePool` ou l'énumération `Pool`. Il n'est pas réservé à Postgres.
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
