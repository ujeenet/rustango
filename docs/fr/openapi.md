# OpenAPI

**Rustango** génère une spécification **OpenAPI 3.1** pour votre API et la sert
avec Swagger UI / Redoc — aucune annotation à maintenir à la main. Pointer le
générateur vers les serializers et les ViewSets que vous avez déjà écrits : les
schémas de champ proviennent de `#[derive(Serializer)]`, les chemins CRUD de votre
ViewSet, et un routeur d'une ligne monte `/openapi.json` + une page de
documentation interactive. Lorsqu'un contrôle total est nécessaire, ces mêmes
types permettent de construire n'importe quelle spécification à la main.

[![OpenAPI Rustango : les champs du serializer deviennent un schéma de composant, le ViewSet devient des chemins CRUD, et openapi_router sert /openapi.json + Swagger UI](img/openapi.png)](img/openapi.png)

> **Source :** `rustango::openapi` (`OpenApiSpec`, `Schema`, `OpenApiSchema`,
> `PathItem`, `Operation`, `SecurityScheme`, `router::openapi_router`) et
> `ViewSet::openapi_paths` — derrière la feature `openapi` (activée par défaut ; le
> routeur du visualiseur nécessite aussi `admin`).
>
> **Version exécutable :** chaque extrait ci-dessous est copié de l'exemple testé
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog/tests/openapi.rs)
> — `cargo test -p getting_started_blog --test openapi`. Le builder, le schéma et
> les types de routeur sont en outre couverts par les tests unitaires du framework
> lui-même (`crates/rustango/src/openapi/`).

> **Un terme nouveau ici ?** *OpenAPI*, *schema*, *serializer*, *ViewSet* — voir le
> [glossaire](glossary.md).

---

## Table des matières
- [Démarrage rapide](#quick-start) — générer + servir en un seul écran
- [Schémas depuis les serializers](#schemas-from-serializers) · [Chemins depuis les ViewSets](#paths-from-viewsets)
- [Le servir](#serving-the-spec-swagger-ui--redoc) · [Construire une spec à la main](#hand-building-a-spec)
- [Schémas de sécurité](#security-schemes) · [Le builder `Schema`](#the-schema-builder)
- [Notes et limites](#notes-and-limits)

---

## Démarrage rapide

Générer un schéma de composant depuis un serializer, des chemins CRUD depuis un
ViewSet, puis monter le visualiseur :

```rust
use rustango::core::Model;                  // brings `Post::SCHEMA` into scope
use rustango::openapi::{OpenApiSpec, OpenApiSchema, SecurityScheme};
use rustango::openapi::router::openapi_router;
use rustango::viewset::ViewSet;

let mut spec = OpenApiSpec::new("Blog API", "1.0.0")
    .description("Demo of rustango's OpenAPI generation")
    .server("https://api.example.com", "Production")
    .add_security_scheme("bearerAuth", SecurityScheme::bearer("JWT"))
    .require_security("bearerAuth", [])                  // global default
    .add_schema("Post", PostSerializer::openapi_schema()); // from #[derive(Serializer)]

// CRUD path items straight off the ViewSet:
for (path, item) in ViewSet::for_model(Post::SCHEMA).openapi_paths("/api/posts", "Post") {
    spec = spec.add_path(path, item);
}

// Mount /openapi.json + /docs (Swagger UI) + /redoc:
let app = axum::Router::new().merge(openapi_router(spec));
```

Visiter `/docs` pour Swagger UI, `/redoc` pour Redoc, ou récupérer le
`/openapi.json` brut pour la génération de code (`openapi-generator`,
`oapi-codegen`, etc.).

---

## Schémas depuis les serializers

Avec la feature `openapi` activée, `#[derive(Serializer)]` émet aussi une impl
[`OpenApiSchema`], si bien que tout serializer devient un schéma de composant :

```rust
use rustango::openapi::OpenApiSchema;

let schema = PostSerializer::openapi_schema();   // -> rustango::openapi::Schema
spec = spec.add_schema("Post", schema);
```

Le schéma reflète la **forme de l'API**, non la table brute : les champs renommés
(`#[serializer(source = "body")]` → `content`), la visibilité `read_only` /
`write_only`, les champs méthode calculés et les serializers imbriqués sont tous
répercutés. Les types de champ sont mis en correspondance automatiquement :

| Champ Rust | OpenAPI |
|---|---|
| `i32` / `i64` | `integer` (`int32` / `int64`) |
| `f32` / `f64` | `number` (`float` / `double`) |
| `String`, `&str` | `string` |
| `bool` | `boolean` |
| `Vec<T>` / `[T; N]` | `array` de `T` |
| `Option<T>` | `T`, marqué `nullable` |
| `chrono::DateTime<Utc>` | `string` / `date-time` |
| `chrono::NaiveDate` | `string` / `date` |
| `uuid::Uuid` | `string` / `uuid` |
| `Auto<T>` | identique à `T` (assigné par le serveur) |
| `HashMap<String, V>` | `object` avec `additionalProperties` |
| `serde_json::Value` | forme libre (any) |

Pour un **type de champ personnalisé**, implémenter `OpenApiSchema` une fois et il
fonctionne partout où ce type apparaît :

```rust
struct Money { cents: i64 }

impl rustango::openapi::OpenApiSchema for Money {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::integer().description("amount in cents")
    }
}
```

> Voir le [guide des Serializers](serializers.md#openapi-schemas) pour les
> attributs de champ qui façonnent la sortie.

---

## Chemins depuis les ViewSets

`ViewSet::openapi_paths(prefix, schema_ref)` retourne les paires
`(path, PathItem)` pour les cinq routes CRUD standard — list, create, retrieve,
update, partial update, delete — câblées pour référencer un schéma de composant
enregistré :

```rust
for (path, item) in ViewSet::for_model(Post::SCHEMA)
    .filter_fields(&["author_id", "status"])
    .search_fields(&["title", "body"])
    .openapi_paths("/api/posts", "Post")
{
    spec = spec.add_path(path, item);
}
```

Il produit `/api/posts` (GET list, POST create) et `/api/posts/{pk}` (GET, PUT,
PATCH, DELETE), et il reste synchronisé avec la configuration du ViewSet :

- **Pagination** → les bons paramètres de requête + l'enveloppe de réponse de liste
  pour le style configuré (`page`/`page_size`, `cursor`, ou `limit`/`offset`).
- **Filtrage / recherche / tri** → un paramètre de requête `?field=` par
  `filter_fields` (avec les lookups `__gt`/`__in`/… disponibles décrits), plus
  `?search=` et `?ordering=` lorsqu'ils sont configurés.
- **`read_only()`** → les opérations d'écriture (POST/PUT/PATCH/DELETE) sont
  omises.
- **Paramètre de chemin** → `{pk}` typé d'après la clé primaire du modèle.
- **`operationId`** → `list_post`, `create_post`, … (en snake_case par action).

> Le ViewSet lui-même est documenté dans le [guide des ViewSets](viewsets.md).

---

## Servir la spec (Swagger UI / Redoc)

`openapi_router(spec)` monte trois routes :

| Route | Sert |
|---|---|
| `GET /openapi.json` | la spec au format JSON (`application/json`) |
| `GET /docs` | Swagger UI (essai interactif try-it-out) |
| `GET /redoc` | Redoc (référence épurée à trois volets) |

```rust
let app = axum::Router::new()
    .merge(PostViewSet::router("/api/posts", pool.clone()))
    .merge(openapi_router(spec));        // adds /openapi.json, /docs, /redoc
```

Les pages du visualiseur sont de minuscules coquilles HTML qui chargent Swagger
UI / Redoc depuis un CDN (`unpkg` / `jsdelivr`) — aucun JS n'est empaqueté dans
**Rustango**. Pour un déploiement isolé du réseau, écrire `spec.to_json()` dans un
fichier au démarrage et héberger soi-même les assets du visualiseur depuis votre
propre répertoire statique.

---

## Construire une spec à la main

Lorsque votre API n'a pas la forme d'un ViewSet, construire la spec directement —
les mêmes types, la fidélité complète d'OpenAPI 3.1 :

```rust
use rustango::openapi::{OpenApiSpec, PathItem, Operation, Response, Parameter, Schema};

let spec = OpenApiSpec::new("My API", "1.0.0")
    .description("Customer-facing public API")
    .add_schema("Post", Schema::object()
        .property("id", Schema::integer())
        .property("title", Schema::string())
        .required(["id", "title"]))
    .add_path("/posts/{id}", PathItem::new()
        .parameter(Parameter::path("id", Schema::integer()))
        .get(Operation::new()
            .summary("Get a post")
            .operation_id("get_post")
            .tag("posts")
            .response("200", Response::new("OK")
                .json_content(Schema::ref_("Post")))
            .response("404", Response::new("not found"))));

let json = spec.to_json();   // pretty-printed OpenAPI 3.1
```

`OpenApiSpec` accepte aussi `.contact(...)`, `.license(...)`, `.add_tag(...)`, et
plusieurs entrées `.server(...)`.

---

## Schémas de sécurité

Déclarer comment l'API s'authentifie, puis exiger un schéma globalement (les
surcharges par opération l'emportent) :

```rust
use rustango::openapi::SecurityScheme;

let spec = OpenApiSpec::new("API", "1.0")
    .add_security_scheme("bearerAuth", SecurityScheme::bearer("JWT"))
    .add_security_scheme("apiKey", SecurityScheme::api_key_header("X-API-Key"))
    .require_security("bearerAuth", []);     // default for every operation
```

Assistants : `SecurityScheme::bearer(fmt)`, `basic()`, `api_key_header(name)`,
`api_key_query(name)`, `oauth2_authorization_code(auth_url, token_url, scopes)`.
Sur une `Operation`, `.no_security()` marque un endpoint public et
`.require_security(scheme, scopes)` surcharge la valeur globale par défaut. Ceux-ci
s'alignent sur l'authentification propre de **Rustango** (voir le
[guide de Sécurité](security.md#authenticating-users)) : JWT → `bearer`, clés
d'API → `apiKey`, OAuth2 → `oauth2`.

---

## Le builder `Schema`

`Schema` est un builder fluide de JSON-Schema (sous-ensemble d'OpenAPI 3.1) :

```rust
Schema::object()
    .property("id", Schema::integer())                       // int64
    .property("email", Schema::email())                      // string/email
    .property("status", Schema::string().enum_(["draft", "published"]))
    .property("tags", Schema::array_of(Schema::string()))
    .property("created", Schema::datetime().nullable())
    .required(["id", "email"]);
```

Constructeurs de type : `string` · `integer` / `int32` · `number` · `boolean` ·
`object` · `array_of(..)` · `any_object` · `datetime` / `date` / `time` · `uuid`
· `decimal` · `binary` · `email` · `uri` · `ref_("Name")`. Modificateurs :
`.property`, `.required`, `.nullable`, `.enum_`, `.format`, `.description`,
`.example`, `.default_value`, `.min_length` / `.max_length`, `.minimum` /
`.maximum`.

---

## Notes et limites

- **C'est de l'OpenAPI 3.1** (`"openapi": "3.1.0"`), qui s'aligne sur JSON Schema
  2020-12 — la plupart des outils modernes le consomment directement.
- **Le routeur du visualiseur nécessite la feature `admin`** (pour axum) ;
  `openapi` seul suffit à *construire* et sérialiser une spec (`spec.to_json()`).
- **Les visualiseurs chargent depuis un CDN** — convient pour la documentation
  interne/dev ; héberger soi-même les assets pour les déploiements hors ligne ou
  verrouillés par CSP.
- **`openapi_paths` couvre la forme CRUD standard.** Les actions personnalisées, ou
  les endpoints faits main, s'ajoutent avec `add_path` (section « construire à la
  main » ci-dessus).
- Régénérer les SDK clients depuis `/openapi.json` en CI afin qu'ils ne dérivent
  jamais du serveur.
