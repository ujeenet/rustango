# Bien démarrer : construire un blog avec Rustango

Ce guide vous accompagne depuis un répertoire vide jusqu'à un blog déployé : des articles, une interface d'administration, une API JSON, une authentification JWT et des tests. De bout en bout. Si vous avez déjà utilisé Django, Laravel ou Rails, la plupart des étapes vous sembleront familières ; nous soulignons les parallèles au fil du texte.

> **Durée :** ~45 minutes pour la visite complète, ~10 minutes si vous voulez juste la voir fonctionner.
>
> **Version exécutable :** chaque étape ci-dessous est reproduite dans un exemple testé et compilable disponible dans [`crates/rustango/examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog). Si une étape vous semble incorrecte, comparez-la à cet exemple.

[![Construire un blog avec Rustango : générer la migration, l'appliquer, démarrer le serveur, et interroger l'API JSON — tout depuis un seul binaire](../img/getting-started.png)](../img/getting-started.png)

---

## Ce dont vous avez besoin d'abord

| Outil | Pourquoi | Installation |
|---|---|---|
| Rust 1.88+ | Compilateur | <https://rustup.rs> |
| Une base de données | Ce guide utilise Postgres | voir [Choisir une base de données](#choisir-une-base-de-données) ci-dessous |
| `psql` (optionnel) | Inspecter la BDD | `brew install libpq` / `apt install postgresql-client` |

```bash
rustc --version    # should print 1.88+
```

### Choisir une base de données

**Docker n'est pas obligatoire.** Ce guide y a recours parce qu'une seule
commande fournit un Postgres jetable, mais rien dans rustango n'en dépend.
Choisissez la ligne qui correspond à votre machine — tout le reste est identique :

| Vous voulez | Faites ceci | Remarques |
|---|---|---|
| **Aucun serveur de base de données** | Lancer avec SQLite (ci-dessous) | Rien à installer. Idéal pour apprendre. |
| Postgres **sans** Docker | Installer Postgres nativement et pointer `DATABASE_URL` sur `localhost` | Voir [Postgres natif](#postgres-natif-sans-docker). |
| Postgres **avec** Docker | `docker compose up -d` dans le projet généré | Ce que suppose la suite de ce guide. |

#### SQLite — zéro installation

Les projets générés embarquent une fonctionnalité `sqlite` : vous pouvez donc en
lancer un sans rien installer ni démarrer :

```bash
cargo run --no-default-features --features sqlite
```

avec une URL sur fichier dans `.env` à la place de celle de Postgres :

```bash
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

`mode=rwc` demande à SQLite de créer le fichier s'il n'existe pas. Tout ce que
couvre ce guide — modèles, migrations, l'admin, l'ORM — fonctionne à l'identique ;
seules les fonctionnalités propres à Postgres (opérateurs `JSONB`, multi-tenancy
en mode schéma) ne s'appliquent pas.

#### Postgres natif (sans Docker)

Installez Postgres via votre gestionnaire de paquets (`brew install postgresql@16`,
`apt install postgresql`, ou l'installateur Windows sur
<https://www.postgresql.org/download/windows/>), puis créez le rôle et la base
que la configuration générée attend :

```bash
createuser -s rustango          # ou : CREATE ROLE rustango LOGIN SUPERUSER PASSWORD 'rustango';
createdb myblog_dev -O rustango
```

Le `.env.example` généré pointe sur le nom du service Docker. Remplacez l'hôte
par `localhost` :

```bash
# .env  —  `postgres` est le nom du service docker-compose ; en natif, localhost
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

> **Sous Windows ?** Le backend Hyper-V / WSL2 de Docker Desktop est une cause
> fréquente d'échecs au démarrage. S'il vous résiste, prenez la voie SQLite
> ci-dessus pour apprendre le framework et revenez à Docker au moment du
> déploiement — c'est à cela que sert vraiment la configuration conteneurisée.

---

## Étape 1 : installer le générateur de squelette

Le générateur de squelette (scaffolder) crée pour vous des squelettes de projet et d'application, comme `django-admin` ou `rails new`.

```bash
cargo install cargo-rustango
```

Ceci ajoute globalement la sous-commande `cargo rustango ...`. Vérifiez qu'elle est bien disponible :

```bash
cargo rustango --help
```

---

## Étape 2 : créer le projet

Ceci génère un nouveau projet, l'équivalent chez **Rustango** de `rails new` ou `composer create-project`.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Voici ce qui a été généré :

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── README.md                   # project-specific
├── config/                     # tiered settings (default + dev/staging/prod)
├── migrations/                 # empty — `cargo run -- makemigrations` populates
└── src/
    ├── main.rs                 # entry point: `Cli::new().api(urls::api()).run()`
    ├── models.rs               # every #[derive(Model)] lives here
    ├── views.rs                # axum request handlers
    └── urls.rs                 # `pub fn api()` route aggregator + `admin_router(pool)`
```

Il n'y a qu'un seul binaire : `cargo run` démarre le serveur HTTP, et chaque verbe de style Django (`migrate`, `makemigrations`, `startapp`, `check`, …) passe par ce même binaire via `cargo run -- <verb>`. Il n'y a pas de binaire `manage` séparé.

`Cargo.toml` est le manifeste de dépendances (comme un `composer.json` ou un `Gemfile`). Ouvrez-le et vérifiez que `rustango` figure bien dans `[dependencies]`.

> **Vérifiez le bloc `[features]` — choisissez un backend de base de données.** `#[derive(Model)]`
> conditionne (via `cfg`) ses implémentations générées de `FromRow` / `LoadRelated` aux
> features de **votre** crate (un `cfg` à l'intérieur d'une macro derive se résout par rapport à la crate de destination,
> pas celle de **Rustango**), donc une feature de backend doit être activée ici, sinon le premier modèle
> ne compilera pas. Un squelette actuel inclut :
>
> ```toml
> [features]
> default  = ["postgres"]            # the backend `cargo run` uses
> postgres = ["rustango/postgres"]
> sqlite   = ["rustango/sqlite"]
> mysql    = ["rustango/mysql"]
> ```
>
> Si votre `Cargo.toml` généré n'a **aucun** bloc `[features]` (un `cargo-rustango`
> plus ancien), ajoutez celui ci-dessus à la main — cela résout toujours le problème. Sans cela,
> la compilation échoue avec *« the trait bound `…: MaybePgFromRow` is not satisfied »*
> ainsi qu'un révélateur `warning: unexpected cfg condition value: postgres`.

---

## Étape 3 : configurer votre environnement

La configuration se trouve dans un fichier `.env`, tout comme dans Django ou Laravel. Copiez le modèle :

```bash
cp .env.example .env
```

Le fichier `.env` généré est prêt à l'emploi avec Docker. Comme nous allons exécuter `cargo` sur la machine hôte (et non dans le conteneur de développement), changez l'hôte de la base de données de `postgres` à `localhost` :

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
RUSTANGO_BIND=0.0.0.0:8080
RUSTANGO_APEX_DOMAIN=localhost
RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more
```

Les identifiants, le port et le nom de la base de données (`myblog_dev`) correspondent déjà au service Postgres du `docker-compose.yml`, donc vous n'avez pas besoin d'y toucher.

`RUSTANGO_SESSION_SECRET` signe les sessions et les jetons, donc ne déployez pas la valeur d'exemple. Générez-en une vraie et collez-la :

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

---

## Étape 4 : démarrer la base de données

> **Vous utilisez SQLite ?** Passez cette étape — il n'y a aucun serveur à
> démarrer. Vérifiez que `.env` contient `DATABASE_URL=sqlite://myblog_dev.db?mode=rwc`
> et ajoutez `--no-default-features --features sqlite` à chaque `cargo run` ci-dessous.
>
> **Vous utilisez un Postgres natif ?** Il tourne déjà comme service ; vérifiez
> simplement que `psql "$DATABASE_URL" -c "SELECT version();"` aboutit, puis
> passez à la suite.


Le projet inclut un `docker-compose.yml` qui exécute Postgres dans un conteneur, afin que vous n'ayez pas à installer une base de données à la main. Nous allons exécuter l'application elle-même avec `cargo` sur l'hôte, donc démarrez uniquement le service `postgres` en arrière-plan (le fichier compose définit aussi un conteneur de développement `rust` optionnel qui occuperait sinon le port 8080) :

```bash
docker compose up -d postgres
```

Vérifiez qu'il fonctionne :

```bash
docker compose ps
psql "$DATABASE_URL" -c "SELECT version();"   # should print Postgres version
```

---

## Étape 5 : exécuter les migrations intégrées

Les migrations créent les tables de votre base de données, la même idée que `php artisan migrate` ou `rails db:migrate`. Exécutez-les une fois pour mettre en place les propres tables du framework :

```bash
cargo run -- migrate
```

La première compilation prend ~2 minutes (Rust compile tout depuis les sources). Un projet neuf n'expose encore aucun fichier de migration, donc vous verrez `nothing to migrate (already up to date)` — `migrate` met néanmoins en place la table de journal d'audit du framework afin que les modèles audités fonctionnent dès que vous les ajouterez. Vous générerez votre première vraie migration à l'étape 9.

Vérifiez l'état des migrations :

```bash
cargo run -- showmigrations
```

Sur un projet neuf, ceci affiche `(no migrations in ./migrations)`. Une fois que vous aurez créé un modèle et exécuté `makemigrations` (étape 9), chaque migration appliquée affichera un `[X]` ici.

---

## Étape 6 : premier démarrage

Démarrez le serveur pour vérifier que tout est correctement branché.

```bash
cargo run
```

Vous verrez :

```
listening on http://0.0.0.0:8080
```

Ouvrez <http://localhost:8080> dans votre navigateur. Le squelette fournit un gestionnaire racine simple (`views::index`) qui vous accueille avec **Hello from Rustango!** et un lien vers l'administration — ce qui confirme que **Rustango** fonctionne. (Les projets qui ne définissent pas leur propre route `/` reçoivent à la place une page d'accueil intégrée, via `Cli::with_welcome()`.)

Appuyez sur Ctrl-C pour arrêter.

---

## Étape 7 : créer une application

Une « application » est un module fonctionnel autonome, exactement comme une application Django. Votre application blog contiendra le modèle Post, ses routes et ses gabarits (templates).

```bash
cargo run -- startapp blog
```

Ceci écrit :

```
src/blog/
├── mod.rs
├── models.rs              # a starter model named after the app (you'll replace it)
├── views.rs               # axum handlers
├── urls.rs                # blog-specific routes (pub fn api())
└── tests.rs               # in-process router + inventory smoke tests
```

`startapp` branche le nouveau module pour vous (de manière similaire à l'ajout d'une entrée dans `INSTALLED_APPS` de Django) : il déclare `mod blog;` dans `src/main.rs` et insère une ligne `.merge(crate::blog::urls::api())` dans l'agrégateur `api()` de `src/urls.rs`, si bien que les routes du blog s'intègrent automatiquement à l'application. Aucun enregistrement manuel de module n'est nécessaire.

---

## Étape 8 : définir un modèle

Un modèle est une table de base de données décrite comme une structure Rust, comme un modèle Django ou une classe Eloquent/Active Record. Ouvrez `src/blog/models.rs` et définissez votre `Post`. (Pour la référence complète — chaque type de champ, les clés primaires personnalisées et tous les attributs — voir le [guide des modèles](models.md).)

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display  = "id, title, status, published_at",
        search_fields = "title, body",
        list_filter   = "status, author_id",
        ordering      = "-published_at",
    ),
    audit(track = "title, body, status"),
    index("status, published_at"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,                  // draft | published

    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,

    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}
```

Quelques points Rust à noter :

- `#[derive(Model, ...)]` est une **macro derive** : elle génère automatiquement du code pour la structure, à la manière d'un décorateur de classe ou d'une classe de base dans d'autres frameworks. Dériver `Model` est ce qui donne à la structure ses méthodes de requête.
- `Auto<i64>` marque un champ que la base de données remplit pour vous (un entier `i64` auto-incrémenté), comme une clé primaire automatique.
- `Option<...>` signifie « cette valeur peut être absente ». `Option<DateTime<Utc>>` est un horodatage qui peut être nul, donc `deleted_at` est vide jusqu'à ce que la ligne soit supprimée en douceur (soft-delete).
- Les attributs `#[rustango(...)]` configurent chaque champ (longueur maximale, valeurs par défaut, index) et le bloc `admin(...)` définit les colonnes et filtres de l'interface d'administration.

---

## Étape 9 : créer et appliquer la migration

Transformons maintenant ce modèle en une véritable table. Générez d'abord la migration à partir de votre modèle (comme `makemigrations` dans Django) :

```bash
cargo run -- makemigrations
```

Vous verrez quelque chose comme :

```
wrote ./migrations/0001_create_item_and_posts_and_rustango_admin_users_etc.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateTable("rustango_admin_users")
    + CreateTable("rustango_content_types")
    + CreateIndex { table: "posts", columns: ["status", "published_at"], ... }
```

Cette première migration crée vos modèles — `posts`, ainsi que le modèle de démarrage `item` fourni par le squelette dans `src/models.rs` — en même temps que les propres tables d'administration et de types de contenu du framework. Ouvrez le fichier JSON si vous le souhaitez : il contient les opérations ainsi qu'un instantané complet du schéma.

Appliquez-la à la base de données :

```bash
cargo run -- migrate
```

Vérifiez que la table existe :

```bash
psql "$DATABASE_URL" -c "\d posts"
```

---

## Étape 10 : essayer l'ORM

Lisons et écrivons des lignes depuis le code. L'ORM vous permet de manipuler les lignes de la base de données comme des structures Rust plutôt que du SQL brut, comme l'ORM de Django, Eloquent, ou Active Record.

Modifiez temporairement `src/main.rs` pour exécuter un rapide test de création-et-lecture avant de démarrer le serveur. Remplacez le corps du `Cli` par un test ad hoc de l'ORM (conservez le `#[rustango::main]` du générateur de squelette ainsi que les déclarations `mod` en haut du fichier) :

```rust
mod blog;
mod models;
mod urls;
mod views;

use crate::blog::models::Post;
use rustango::{Auto, Model};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    // CREATE
    let mut p = Post {
        id: Auto::default(),
        title: "First post".into(),
        body: "Hello, world.".into(),
        status: "draft".into(),
        author_id: 1,
        published_at: Auto::default(),
        deleted_at: None,
    };
    p.save(&pool).await?;
    println!("created post id = {}", p.id.get().copied().unwrap());

    // READ
    let posts = Post::objects().fetch_on(&pool).await?;
    for post in &posts {
        println!("- {}", post.title);
    }

    Ok(())
}
```

Ce qui se passe ici, en termes simples :

- `pool` est le pool de connexions à la base de données partagé. Vous passez une référence à celui-ci (`&pool`) dans les appels de requête plutôt que d'ouvrir une nouvelle connexion chaque fois.
- Les appels à la base de données sont asynchrones, donc chacun se termine par `.await` — cela met en pause jusqu'à ce que le résultat revienne, puis continue. Le `?` après un `.await` signifie « si ceci a échoué, arrête et renvoie l'erreur ».
- `main` renvoie un `Result`, le type succès-ou-erreur de Rust, ce qui explique pourquoi `?` et le `Ok(())` final fonctionnent.
- Pour enregistrer une ligne, appelez `.save(&pool)` sur celle-ci. Pour lire des lignes, construisez une requête avec `Post::objects()` et exécutez-la avec `.fetch_on(&pool)` — l'équivalent approximatif du `Post.objects.all()` de Django. (`.save(&pool)` / `.fetch_on(&pool)` prennent un `sqlx::PgPool` ; la variante nue `.fetch(&pool)` prend à la place un `rustango::sql::Pool` multi-backend — voir le [guide de l'ORM](orm.md).)

Exécutez-le :

```bash
cargo run
```

Vous devriez voir l'identifiant de votre nouvel article ainsi que les lignes lues en retour. Restaurez `src/main.rs` à sa forme de serveur générée par le squelette une fois que vous avez confirmé que cela fonctionne — l'étape suivante s'appuie sur cette forme.

---

## Étape 11 : activer l'administration automatique

**Rustango** fournit une interface d'administration générée pour vos modèles, tout comme l'administration Django. Le générateur de squelette vous a déjà donné un utilitaire `admin_router(pool)` dans `src/urls.rs` qui construit l'administration automatique à partir d'un pool — vous n'avez qu'à l'imbriquer sous `/admin` et à l'injecter dans le `Cli`.

Donnez d'abord un titre à l'administration dans `src/urls.rs`. Le `admin_prefix` doit correspondre au chemin sous lequel vous l'imbriquerez à l'étape suivante (`/admin`) afin que les propres liens et actions de formulaire de l'administration se résolvent correctement :

```rust
pub fn admin_router(pool: PgPool) -> Router {
    admin::Builder::new(pool)
        .title("Myblog Admin")
        .admin_prefix("/admin") // must match the `.nest("/admin", …)` below
        .build()
}
```

Ensuite, connectez un pool dans `src/main.rs` et imbriquez l'administration dans le routeur de l'API avant de la remettre au `Cli`. Conservez la ligne `mod blog;` de l'étape 7 — c'est elle qui enregistre votre modèle `Post` auprès de l'administration :

```rust
mod blog;
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    let api = urls::api().nest("/admin", urls::admin_router(pool));

    rustango::manage::Cli::new()
        .api(api)
        .with_health() // /health + /ready endpoints
        .run()
        .await
}
```

`Cli::new()...run()` est le même dispatcheur unifié généré par le squelette — il continue de servir chaque `cargo run -- <verb>` ; vous n'avez fait qu'enrichir le routeur qu'il sert au moment de `runserver`.

Exécutez-le :

```bash
cargo run
```

Ouvrez <http://localhost:8080/admin> (sans barre oblique finale). Vous verrez l'accueil de l'administration avec un lien `posts`. Cliquez sur celui-ci pour voir votre article brouillon dans la liste, cliquez sur l'article pour ouvrir son formulaire d'édition, puis enregistrez. L'onglet de piste d'audit enregistre chaque écriture.

---

## Étape 12 : construire l'API JSON

Un ViewSet expose un modèle comme une API REST avec des points de terminaison de liste, création, récupération, mise à jour et suppression, tout comme un ViewSet Django REST Framework ou un contrôleur de ressource API Laravel.

### 12a. Générer le ViewSet

Générez le fichier, puis renseignez les champs et comportements à exposer :

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Modifiez `src/post_view_set.rs` :

```rust
use rustango::ViewSet;
use crate::blog::models::Post;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    fields        = "id, title, body, status, author_id, published_at",
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
```

Enregistrez le nouveau module en ajoutant `mod post_view_set;` avec les autres déclarations `mod` en haut de `src/main.rs`.

### 12b. Monter les routes

Attachez les routes du ViewSet au routeur de l'application (la version **Rustango** d'un fichier `urls.py` ou d'un `routes/api.php`). Le routeur du ViewSet a besoin du pool de base de données, construisez-le donc dans `src/main.rs`, là où vit le pool, et fusionnez-le dans l'agrégateur `urls::api()` :

```rust
let api = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool));

rustango::manage::Cli::new()
    .api(api)
    .with_health()
    .run()
    .await
```

(`urls::api()` est l'agrégateur généré par le générateur de squelette ; `manage startapp` fusionne les routes de toute sous-application de la même façon.)

### 12c. Essayer les points de terminaison

Démarrez le serveur :

```bash
cargo run
```

Dans un autre terminal, interrogez l'API avec `curl` :

```bash
curl http://localhost:8080/api/posts                                    # list
curl -X POST http://localhost:8080/api/posts \
     -H "content-type: application/json" \
     -d '{"title":"From API","body":"Yo","status":"published","author_id":1}'
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

---

## Étape 13 : façonner la sortie avec un Serializer

Par défaut, le ViewSet renvoie tous les champs du modèle. Un Serializer vous permet de contrôler la forme de la réponse : masquer des champs internes, les renommer, ou en marquer certains en lecture seule. Il joue le même rôle qu'un serializer DRF ou une ressource API Laravel.

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Modifiez `src/post_serializer.rs` :

```rust
use rustango::{Auto, Serializer};
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,

    #[serializer(source = "body")]                      // rename in API
    pub content: String,

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

Le type de chaque champ du serializer reflète le champ correspondant du modèle, donc `id` et `published_at` conservent leur enveloppe `Auto<…>` héritée du modèle (un `Auto<i64>` se sérialise toujours en un simple entier JSON). Enregistrez ensuite le module en ajoutant `mod post_serializer;` avec les autres déclarations `mod` dans `src/main.rs`.

Branchez le serializer dans le ViewSet avec l'attribut `serializer` — les réponses de liste, de récupération et de création sont alors rendues via celui-ci (la projection de champs `fields` est alors contournée au profit de la forme du serializer) :

```rust
#[derive(ViewSet)]
#[viewset(
    model = Post,
    serializer = crate::post_serializer::PostSerializer,
    ordering = "-published_at",
)]
pub struct PostViewSet;
```

Ceci fonctionne à l'identique sur PostgreSQL, MySQL et SQLite. Les redéfinitions `method` / `read_only` / `source` / `write_only` s'appliquent toutes à la réponse, et **les corps de requête sont eux aussi validés via le serializer** : `create` / `update` exécutent sa `validate()` (par champ et inter-champs), renvoyant un `400` de forme DRF (`{field: [messages]}`) en cas d'échec, et les champs en lecture seule / calculés qu'un client tenterait de poster sont ignorés. (Remarque : les champs de serializer `nested` / `many` nécessitent que les lignes liées soient chargées via `select_related` ; sinon ils s'affichent avec leur valeur par défaut.) Voir le [guide des ViewSets](viewsets.md) pour le comportement complet en entrée et en sortie.

---

## Étape 14 : ajouter l'authentification JWT

Les JWT sont des jetons signés que vous remettez à un client après la connexion et que vous vérifiez à chaque requête, un schéma courant pour l'authentification d'API. Le module `rustango::jwt` de **Rustango** les émet et les vérifie (HS256), et il est actif par défaut — aucune feature supplémentaire à activer.

### 14a. Émettre un jeton à la connexion

Intégrez l'identifiant de l'utilisateur (le « sujet » du jeton) et toute revendication (claim) personnalisée, comme les rôles, dans un jeton signé, puis remettez-le au client :

```rust
use rustango::jwt::{encode, Claims};
use std::time::Duration;

// Derive the signing key from your session secret.
let secret = std::env::var("RUSTANGO_SESSION_SECRET")?.into_bytes();

let mut claims = Claims::new(user_id.to_string());   // subject = user id
claims.set("roles", vec!["editor"]);
let token = encode(&claims.ttl(Duration::from_secs(900)), &secret)?;

// Send `token` to the client (e.g. in the login response body).
```

### 14b. Vérifier le jeton à chaque requête

Décodez le jeton — ceci vérifie la signature et l'expiration — puis relisez les revendications (claims). S'il est absent ou invalide, rejetez la requête comme non autorisée :

```rust
use rustango::jwt::decode;

let claims = decode(&access_token, &secret)
    .map_err(|_| StatusCode::UNAUTHORIZED)?;

let user_id = claims.subject().ok_or(StatusCode::UNAUTHORIZED)?;
let roles: Vec<String> = claims.get("roles").unwrap_or_default();
```

### 14c. Cycle de vie accès + rafraîchissement

`rustango::jwt` émet des jetons uniques sans état. Pour le schéma complet — des jetons d'**accès** de courte durée, un jeton de **rafraîchissement** de longue durée dans un cookie HttpOnly, la rotation, et une liste noire de JTI pour la révocation — activez la feature `tenancy` et utilisez `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, dont les méthodes `issue_pair_with` / `verify_access` / `refresh` gèrent la paire pour vous.

---

## Étape 15 : ajouter le middleware de sécurité

Le middleware englobe chaque requête pour y ajouter un comportement transversal. Ici, vous empilez les identifiants de requête, la journalisation des accès, la limitation de débit, le CORS et les en-têtes de sécurité en une seule chaîne. Chaque `.method(...)` ajoute une couche, de manière similaire au middleware Django ou à la pile de middleware de Laravel. Voir le [guide du middleware](middleware.md) pour le catalogue complet des couches et les règles d'ordonnancement.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use rustango::health::health_router;
use std::time::Duration;

let app = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool.clone()))
    .merge(health_router(pool.clone()))                        // /health, /ready
    .request_id(RequestIdLayer::default())
    .access_log(AccessLogLayer::default())                      // PII-redacted
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
    .cors(CorsLayer::new()
        .allow_origins(vec!["https://app.example.com"])
        .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]))
    .security_headers(
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    );
```

Remettez le `app` fini au `Cli` exactement comme avant — `rustango::manage::Cli::new().api(app).with_welcome().run().await` — et chaque requête passe désormais par la pile complète de middleware.

---

## Étape 16 : écrire des tests

**Rustango** inclut un client de test qui pilote votre routeur en process, ce qui vous permet de faire des assertions sur de vraies réponses HTTP sans démarrer de serveur, tout comme le client de test de Django ou les tests HTTP de Laravel. Générez un fichier de test :

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

Les générateurs `make:*` prennent un nom en PascalCase ; `PostSmoke` devient le fichier en snake_case `tests/post_smoke.rs`.

Modifiez `tests/post_smoke.rs`. Les tests d'intégration vivent dans une crate séparée, donc ils construisent le routeur testé directement à partir du ViewSet (le même appel `router(...)` que celui monté à l'étape 12b) :

```rust
use rustango::test_client::TestClient;
use myblog::post_view_set::PostViewSet;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn list_posts_returns_200() {
    let client = TestClient::new(app().await);
    let response = client.get("/api/posts").send().await;
    assert_eq!(response.status, 200);
    let v = response.json_value();
    assert!(v["results"].is_array());
}

#[tokio::test]
async fn create_post_returns_the_new_object() {
    let client = TestClient::new(app().await);
    let response = client.post("/api/posts")
        .json(&json!({
            "title": "Test",
            "body":  "x",
            "status": "draft",
            "author_id": 1,
        }))
        .send().await;
    assert_eq!(response.status, 201);
    let v: serde_json::Value = response.json();
    assert_eq!(v["title"], "Test");
}
```

> **Attention :** les tests d'intégration dans `tests/` ne peuvent faire `use myblog::…` que si la crate expose une cible de bibliothèque. Un squelette neuf n'est composé que d'un binaire (`src/main.rs`, sans `src/lib.rs`), donc ajoutez une simple ligne `src/lib.rs` qui réexporte les modules que vous voulez tester — `pub mod models; pub mod post_view_set; pub mod urls;` — et conservez les lignes `mod …;` correspondantes dans `src/main.rs`. (Si vous préférez ne pas ajouter de cible de bibliothèque, construisez plutôt le routeur entièrement en ligne dans le test, de la façon dont `make:test` génère sa fonction `app()`.)

Exécutez les tests :

```bash
cargo test --test post_smoke
```

---

## Étape 17 : exécuter la vérification système

Avant de déployer, exécutez le vérificateur intégré. Il signale les erreurs de configuration courantes (comme un `RUSTANGO_SESSION_SECRET` trop faible ou une base de données inaccessible), de façon similaire au `check --deploy` de Django.

```bash
cargo run -- check --deploy
```

Dans votre environnement de développement local, vous verrez quelque chose comme :

```
running rustango system check (deploy mode)...
  [info]    6 models registered via inventory
  [info]    database reachable
  [info]    1 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET length OK
  [info]    config tier resolved to `dev`
  [warning] RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier
  [warning] DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in production
  [warning] RUSTANGO_APEX_DOMAIN is unset / `localhost` — set it for tenancy projects
```

(Le nombre exact de modèles/migrations dépend de votre projet.) Ces trois avertissements sont ceux attendus en environnement de développement. Dans une configuration de production — `RUSTANGO_ENV=prod`, un `DATABASE_URL` de base de données géré, un domaine apex défini — ils disparaissent et vous verrez `all checks passed`. Corrigez tout avertissement ou erreur restant avant de déployer en production.

---

## Étape 18 : déployer en production

La façon de déployer dépend de votre plateforme (Fly, Railway, Kubernetes, ECS nu, et ainsi de suite). Les étapes côté framework sont les mêmes partout ; l'option `--release` construit un binaire optimisé :

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export RUSTANGO_SESSION_SECRET=$(openssl rand -base64 32)

# 2. Run migrations
cargo run --release -- migrate

# 3. Audit
cargo run --release -- check --deploy

# 4. Build binary
cargo build --release

# 5. Run with a process supervisor (systemd / docker / k8s)
./target/release/myblog
```

Assurez-vous que votre proxy inverse :
- Termine le HTTPS
- Transmet `X-Forwarded-For` pour des IP précises dans `AccessLogLayer`
- Transmet `X-Forwarded-Host`, `X-Forwarded-Proto`
- Utilise `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())` afin que `ConnectInfo` soit renseigné pour la limitation de débit et le filtrage par IP

---

## Où aller ensuite

| Sujet | Doc |
|---|---|
| Version exécutable de ce guide | [`examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog) |
| Chaque sous-commande `manage` | [`docs/manage.md`](manage.md) |
| Recueil de recettes ORM (filtres avancés, agrégations, M2M, suppression douce) | [`docs/orm.md`](orm.md) |
| Middleware (le catalogue complet des couches + ordonnancement) | [`docs/middleware.md`](middleware.md) |
| Benchmarks de performance (vs Go) | [`docs/benchmarks.md`](benchmarks.md) |
| Conventions d'API (nommage, patrons de construction, feature gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Fonctionnalités de sécurité en détail | [`docs/security.md`](security.md) |
| Audit de parité avec Django | [`docs/django-parity-audit-2026-05-21.md`](https://github.com/ujeenet/rustango/blob/main/docs/django-parity-audit-2026-05-21.md) |
| Multi-tenancy | [README — section Multi-tenancy](https://github.com/ujeenet/rustango/blob/main/README.md#multi-tenancy) |
| Documentation de l'API | <https://docs.rs/rustango> |

Si vous rencontrez quelque chose qui ne fonctionne pas ou qui n'est pas clair, ouvrez un ticket.
