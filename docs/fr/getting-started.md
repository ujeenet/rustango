# Bien démarrer : construire un blog avec Rustango

Ce guide vous accompagne depuis un répertoire vide jusqu'à un blog déployé : des articles, une interface d'administration, une API JSON avec inscription et connexion (JWT), et des tests. De bout en bout.

> **Durée :** ~45 minutes pour la visite complète, ~10 minutes si vous voulez juste la voir fonctionner.
>
> **Version exécutable :** chaque étape ci-dessous est reproduite dans un exemple testé et compilable disponible dans [`crates/rustango/examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog). Si une étape vous semble incorrecte, comparez-la à cet exemple.

[![Construire un blog avec Rustango : générer la migration, l'appliquer, démarrer le serveur, et interroger l'API JSON — tout depuis un seul binaire](../img/getting-started.png)](../img/getting-started.png)

---

## Ce qu'il faut savoir d'abord

Deux questions bien distinctes, et la documentation ne répondait jusqu'ici qu'à
la seconde.

**Rust est supposé acquis.** Pas un niveau expert, mais vous devez être à l'aise
avec les structs, les traits, `Result` et `?`, et connaître assez
`async`/`.await` pour lire une fonction sans rien avoir à chercher. Si ce n'est
pas encore votre cas, commencez par le [Rust Book](https://doc.rust-lang.org/book/) ;
ce guide n'enseignera pas le langage qui se trouve en dessous.

**L'expérience du back-end web n'est pas supposée acquise.** Si vous n'avez
jamais construit d'API web, commencez par
[Les bases des API web](glossary.md#les-bases-des-api-web) dans le glossaire.
C'est une introduction de cinq minutes aux requêtes, aux routes, aux handlers et
aux migrations, écrite exactement pour combler ce manque. Revenez ici ensuite.

**Aucune expérience préalable d'un autre framework web n'est supposée acquise.**
Lorsque cette documentation fait une comparaison, c'est un aparté, jamais
l'explication : si un parallèle ne vous évoque rien,
passez-le, l'étape tient debout toute seule. Là où un terme joue un vrai rôle, le
[glossaire](glossary.md) le définit en langage clair.

## Ce qu'il faut installer

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
| MySQL ou MariaDB | Générer avec `--backend mysql` | Voir [MySQL](#mysql). |

Quel que soit votre choix, la seule chose qui change est `DATABASE_URL`. Voici la
forme de chacune :

```bash
DATABASE_URL=postgres://user:password@localhost:5432/myblog_dev
DATABASE_URL=mysql://user:password@localhost:3306/myblog_dev
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

#### SQLite — zéro installation

Générez directement le projet pour SQLite : il n'y a rien à installer, rien à
démarrer et rien à modifier ensuite.

```bash
cargo rustango new myblog --backend sqlite
```

Les `.env.example`, `docker-compose.yml` et paliers de configuration générés
sont tous écrits pour SQLite, la base est un fichier créé par le premier
`cargo run -- migrate`, et vous pouvez sauter entièrement
[l'étape 4](#étape-4--démarrer-la-base-de-données).

Si vous avez déjà généré un projet Postgres et voulez en changer, chaque
template garde les trois backends câblés — c'est donc un flag plus une URL :

```bash
cargo run --no-default-features --features sqlite
```

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

> **Postgres tourne déjà en local ?** Alors le port 5432 est déjà pris, et le
> conteneur perd silencieusement la course. Votre application se connecte au
> serveur local, qui ne contient aucune de vos tables. L'erreur renvoyée est
> illisible, parce qu'un serveur non anglophone envoie son message dans son
> propre encodage. Arrêtez le service local, ou déplacez le conteneur sur un
> autre port.

#### MySQL

Générez le projet avec `--backend mysql` : les `.env.example`,
`docker-compose.yml` et paliers de configuration générés sont alors tous écrits
pour MySQL :

```bash
cargo rustango new myblog --backend mysql
```

L'exécuter nativement plutôt que dans le conteneur demande une base et un
utilisateur :

```sql
CREATE DATABASE myblog_dev CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs;
CREATE USER 'rustango'@'localhost' IDENTIFIED BY 'rustango';
GRANT ALL PRIVILEGES ON myblog_dev.* TO 'rustango'@'localhost';
```

```bash
DATABASE_URL=mysql://rustango:rustango@localhost:3306/myblog_dev
```

`utf8mb4` mérite d'être choisi délibérément : l'ancien `utf8` de MySQL tient sur
trois octets et ne peut pas stocker un emoji, ce qui ressurgit bien plus tard
sous la forme d'une écriture qui échoue sur une seule ligne. MariaDB fonctionne
avec le même pilote et le même schéma d'URL.

La collation compte aussi. Les colonnes héritent de celle de la base, et celle de
MySQL (`utf8mb4_0900_ai_ci`) ignore la casse et les accents : `.eq("/About")` trouve
`/about` et une colonne `unique` refuse `Hero` à côté de `hero`, contrairement à
PostgreSQL et SQLite. `utf8mb4_0900_as_cs` compare comme eux. Évitez une collation
`_bin` : le pilote ne peut pas lire ses colonnes texte en `String`.
`manage check --deploy` signale les deux.

---

## Étape 1 : installer le générateur de squelette

Le générateur de squelette (scaffolder) crée pour vous des squelettes de projet et d'application.

```bash
cargo install cargo-rustango
```

Ceci ajoute globalement la sous-commande `cargo rustango ...`. Vérifiez qu'elle est bien disponible :

```bash
cargo rustango --help
```

La version du générateur de squelette est celle que votre projet épingle : installer le plus récent vous donne le rustango le plus récent. Pour générer un projet sur une version plus ancienne, installez plutôt ce générateur-là (`cargo install cargo-rustango --version 0.65.2`) — voir [Échafaudage](scaffolding.md#la-version-du-générateur-est-celle-que-votre-projet-obtient).

---

## Étape 2 : créer le projet

Ceci génère un nouveau projet avec une structure, des réglages et des migrations prêts.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Lancez `cargo rustango new` sans aucun argument et il demande le template, le
backend et les fonctionnalités supplémentaires, puis affiche la ligne de
commande équivalente avant de créer quoi que ce soit — voir
[Échafaudage](scaffolding.md).

Voici ce qui a été généré :

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── Dockerfile                  # release image; Dockerfile.dev is the dev one
├── README.md                   # project-specific
├── config/                     # tiered settings (default + dev/staging/prod)
├── migrations/                 # empty — `cargo run -- makemigrations` populates
├── system/migrations/          # the framework's own tables
└── src/
    ├── lib.rs                  # the app as a library: `pub mod models; …`
    ├── main.rs                 # entry point: `Cli::new().api(urls::api()).run()`
    ├── models.rs               # every #[derive(Model)] lives here
    ├── views.rs                # axum request handlers
    └── urls.rs                 # agrégateur de routes `pub fn api()`
```

Il n'y a qu'un seul binaire : `cargo run` démarre le serveur HTTP, et chaque verbe d'administration (`migrate`, `makemigrations`, `startapp`, `check`, …) passe par ce même binaire via `cargo run -- <verb>`. Il n'y a pas de binaire `manage` séparé.

Les modules de l'application vivent dans la cible de bibliothèque (`src/lib.rs`), et `src/main.rs` les atteint par le nom de la crate (`use myblog::urls;`). C'est ce qui permet aux tests de `tests/` et aux binaires supplémentaires de `src/bin/` d'utiliser vos modèles.

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

La configuration se trouve dans un fichier `.env`. Copiez le modèle :

```bash
cp .env.example .env
```

Le fichier `.env` généré est prêt à l'emploi avec Docker. Comme nous allons exécuter `cargo` sur la machine hôte (et non dans le conteneur de développement), changez l'hôte de la base de données de `postgres` à `localhost` :

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

Les identifiants, le port et le nom de la base de données (`myblog_dev`) correspondent déjà au service Postgres du `docker-compose.yml`, donc vous n'avez pas besoin d'y toucher.

`RUSTANGO_SESSION_SECRET` signe les sessions et les jetons. Il est **laissé en commentaire** dans le `.env.example` généré, et pour le développement vous pouvez le laisser ainsi : le premier démarrage génère une clé dans `./var/` et la réutilise, si bien qu'un redémarrage ne vous déconnecte pas.

Pour la production, définissez-en une vraie — 32 octets de base64, depuis votre gestionnaire de secrets ou :

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

Une valeur qui n'est pas 32 octets de base64 ne peut pas être utilisée. Hors du palier prod, le serveur le signale au démarrage et se rabat sur la clé générée — surveillez donc cet avertissement si vous en avez défini une et que les sessions se comportent comme si ce n'était pas le cas. Avec `RUSTANGO_ENV=prod`, un secret absent ou invalide empêche le serveur de démarrer.

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

Les migrations créent les tables de votre base de données. Exécutez-les une fois pour mettre en place les propres tables du framework :

```bash
cargo run -- migrate
```

La première compilation prend ~2 minutes (Rust compile tout depuis les sources). Un projet neuf ne livre encore aucun fichier de migration à lui, mais `migrate` n'est pas sans effet : il génère d'abord les propres migrations du framework à partir des modèles compilés et les applique, vous verrez donc quelques lignes `applied …` plutôt que `nothing to migrate`. (Ce message n'apparaît qu'une fois que tout — framework et projet — est déjà à jour.) Il crée aussi la table du journal d'audit, afin que les modèles audités fonctionnent dès que vous les ajouterez. Vous générerez votre première migration de projet à l'étape 9.

Vérifiez l'état des migrations :

```bash
cargo run -- showmigrations
```

Sur un projet neuf, ceci affiche `(no migrations in ./migrations)`, puis la chaîne propre au framework sous `System migrations in ./system/migrations:`. Une fois que vous aurez créé un modèle et exécuté `makemigrations` (étape 9), chaque migration appliquée affichera un `[X]` ici.

---

## Étape 6 : premier démarrage

Démarrez le serveur pour vérifier que tout est correctement branché.

```bash
cargo run
```

Vous verrez :

```
INFO rustango::server: listening on http://127.0.0.1:8080
```

`cargo run` applique aussi les migrations en attente avant de commencer à écouter.

Ouvrez <http://localhost:8080> dans votre navigateur. Le squelette fournit un gestionnaire racine simple (`views::index`) qui vous accueille avec **Hello from Rustango!** et un lien vers l'administration — ce qui confirme que **Rustango** fonctionne. (Les projets qui ne définissent pas leur propre route `/` reçoivent à la place une page d'accueil intégrée, via `Cli::with_welcome()`.)

Appuyez sur Ctrl-C pour arrêter.

---

## Étape 7 : créer une application

Une « application » est un module fonctionnel autonome. Votre application blog contiendra le modèle Post, ses routes et ses gabarits (templates).

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

`startapp` branche le nouveau module pour vous : il déclare `pub mod blog;` dans `src/lib.rs` (ou `mod blog;` dans `src/main.rs` sur un ancien projet uniquement binaire) et insère une ligne `.merge(crate::blog::urls::api())` dans l'agrégateur `api()` de `src/urls.rs`, si bien que les routes du blog s'intègrent automatiquement à l'application. Aucun enregistrement manuel de module n'est nécessaire.

---

## Étape 8 : définir un modèle

Un modèle est une table de base de données décrite comme une structure Rust — une classe façon Active Record en Rust. Ouvrez `src/blog/models.rs` et définissez votre `Post`. (Pour la référence complète — chaque type de champ, les clés primaires personnalisées et tous les attributs — voir le [guide des modèles](models.md).)

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

Transformons maintenant ce modèle en une véritable table. Générez d'abord la migration à partir de votre modèle :

```bash
cargo run -- makemigrations
```

Vous verrez quelque chose comme :

```
wrote ./migrations/0001_create_item_and_posts.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateIndex { name: "posts_status_published_at_idx", table: "posts", columns: ["status", "published_at"], ... }
```

Cette première migration crée vos modèles — `posts`, ainsi que le modèle de démarrage `item` fourni par le squelette dans `src/models.rs`. Les tables propres au framework (utilisateurs admin, types de contenu, journal d'audit, …) n'y sont pas : elles ont leur propre chaîne dans `system/migrations/`, que le premier `migrate` écrit et applique avant la vôtre. Ouvrez le fichier JSON si vous le souhaitez : il contient les opérations ainsi qu'un instantané complet du schéma.

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

Lisons et écrivons des lignes depuis le code. L'ORM vous permet de manipuler les lignes de la base de données comme des structures Rust plutôt que du SQL brut.

Modifiez temporairement `src/main.rs` pour exécuter un rapide test de création-et-lecture avant de démarrer le serveur. Remplacez le corps du `Cli` par un test ad hoc de l'ORM (conservez le `#[rustango::main]` du générateur de squelette). Les modèles vivent dans la bibliothèque, donc `main.rs` les nomme par le nom de la crate :

```rust
use myblog::blog::models::Post;
use rustango::sql::{FetcherPool, Pool};
use rustango::{Auto, Model};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = Pool::connect(&std::env::var("DATABASE_URL")?).await?;

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
    let posts = Post::objects().fetch(&pool).await?;
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
- Pour enregistrer une ligne, appelez `.save(&pool)` sur celle-ci. Pour lire des lignes, construisez une requête avec `Post::objects()` et exécutez-la avec `.fetch(&pool)` — sans filtre, cela renvoie toutes les lignes de la table.
- `.fetch(…)` provient du trait `FetcherPool`, c'est pourquoi les imports l'incluent. Sans cette ligne, la méthode n'existe pas et le compilateur vous le signale sans expliquer pourquoi.
- Ce sont là les appels multi-backend, et tout ce qui précède compile sans modification sur les trois bases de données. Voir le [guide de l'ORM](orm.md).

Exécutez-le :

```bash
cargo run
```

Vous devriez voir l'identifiant de votre nouvel article ainsi que les lignes lues en retour. Restaurez `src/main.rs` à sa forme de serveur générée par le squelette une fois que vous avez confirmé que cela fonctionne — l'étape suivante s'appuie sur cette forme. Supprimez aussi les trois lignes `use` ci-dessus : la forme serveur ne les utilise pas, et le compilateur signale des imports inutilisés.

---

## Étape 11 : activer l'administration automatique

**Rustango** fournit une interface d'administration générée pour vos modèles — un back-office prêt à parcourir, rechercher et modifier vos données sans écrire de code. Le squelette fullstack la câble déjà : un utilitaire dans `src/urls.rs` transforme un pool en routeur d'administration, et `src/main.rs` l'imbrique sous `/admin`.

L'utilitaire place l'administration derrière une connexion. Il lit ses options dans les paramètres chargés, et `admin_prefix` doit correspondre au chemin sous lequel elle est imbriquée afin que les propres liens et actions de formulaire de l'administration se résolvent correctement. `admin_prefix(settings)` renvoie `[admin] url_prefix` (ou `[routes] admin_url`), sinon `/admin` :

```rust
use rustango::admin;
use rustango::config::Settings;
use rustango::session::{tier_from_env, try_load_session_secret_for_tier, SessionSecretError};
use rustango::sql::Pool;

pub fn admin_router(pool: Pool, settings: &Settings) -> Result<Router, SessionSecretError> {
    let secret = try_load_session_secret_for_tier(
        &tier_from_env(),
        std::path::Path::new("./var/.rustango_session.key"),
    )?;
    Ok(admin::Builder::from_settings(pool, settings)
        .admin_prefix(admin_prefix(settings)) // the path main.rs nests it at
        .with_session_auth(secret)
        .build())
}
```

> **Conservez `.with_session_auth(...)`.** Sans lui, l'administration n'a pas de connexion : quiconque atteint `/admin` peut lire, modifier et supprimer chaque modèle. `check --deploy` avertit lorsqu'une administration est construite sans lui (voir [admin.md](admin.md)).

`Builder::new` accepte le pool de n'importe quel backend : cet utilitaire ne nomme donc aucun pilote et fonctionne sur les trois.

`src/main.rs` remet l'utilitaire au `Cli`, qui construit l'administration à partir du pool avec lequel il sert : des verbes comme `makemigrations` tournent donc toujours sans base de données. Conservez la ligne `pub mod blog;` de l'étape 7 dans `src/lib.rs` — c'est elle qui enregistre votre modèle `Post` auprès de l'administration. Le `main.rs` généré ressemble à ceci (abrégé) :

```rust
use myblog::urls;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let loaded = rustango::config::Settings::load_from_env();
    let settings = loaded.as_ref().cloned().unwrap_or_default();
    let admin_settings = settings.clone();
    rustango::manage::Cli::new()
        .api(urls::api())
        .try_nest_with(urls::admin_prefix(&settings), move |pool| {
            urls::admin_router(pool, &admin_settings)
        })
        .with_health() // /health + /ready endpoints
        .with_loaded_settings(loaded)
        .run()
        .await
}
```

Créez le compte avec lequel vous vous connecterez (le mot de passe vous est demandé) :

```bash
cargo run -- create-admin alice --superuser
```

`Cli::new()...run()` est le même dispatcheur unifié généré par le squelette — il continue de servir chaque `cargo run -- <verb>` ; vous n'avez fait qu'enrichir le routeur qu'il sert au moment de `runserver`.

Exécutez-le :

```bash
cargo run
```

Ouvrez <http://localhost:8080/admin> (sans barre oblique finale) et connectez-vous en tant que `alice`. Vous verrez l'accueil de l'administration avec un lien `posts`. Cliquez sur celui-ci pour voir votre article brouillon dans la liste, cliquez sur l'article pour ouvrir son formulaire d'édition, puis enregistrez. L'onglet de piste d'audit enregistre chaque écriture.

---

## Étape 12 : construire l'API JSON

Un ViewSet expose un modèle comme une API REST avec des points de terminaison de liste, création, récupération, mise à jour et suppression — à partir d'une seule déclaration, sans écrire les routes à la main.

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
    read_only,
)]
pub struct PostViewSet;
```

Enregistrez le nouveau module en ajoutant `pub mod post_view_set;` à `src/lib.rs` (`make:viewset` affiche cette ligne pour vous).

Gardez `read_only` pour l'instant : l'API ne sert que la liste et la lecture. Sans lui, le ViewSet servirait aussi la création, la mise à jour et la suppression à tout le monde, donc il refuse de se monter tant que vous n'avez pas dit qui peut écrire. L'[étape 14](#étape-14--inscription-connexion-et-authentification-jwt) ajoute des comptes et ouvre les écritures aux utilisateurs connectés.

### 12b. Monter les routes

Attachez les routes du ViewSet à l'application. Le routeur du ViewSet a besoin du pool de base de données, et `urls::api()` n'en a pas, donc ajoutez un `nest_with` de plus à la chaîne du `Cli` dans `src/main.rs`. Il reçoit le pool du serveur, comme l'administration :

```rust
rustango::manage::Cli::new()
    .api(urls::api())
    .try_nest_with(urls::admin_prefix(&settings), move |pool| {
        urls::admin_router(pool, &admin_settings)
    })
    .nest_with("/api", |pool| myblog::post_view_set::PostViewSet::router("/posts", pool))
    .with_health()
    .with_loaded_settings(loaded)
    .run()
    .await
```

Le ViewSet répond désormais sur `/api/posts` et `/api/posts/{pk}`. Le chemin est la table du modèle ; le commentaire à la fin de `src/post_view_set.rs` montre cette même ligne, et `make:viewset` affiche l'URL.

### 12c. Essayer les points de terminaison

Démarrez le serveur :

```bash
cargo run
```

Dans un autre terminal, interrogez l'API avec `curl` :

```bash
curl http://localhost:8080/api/posts                                    # list
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

Pour l'instant, un `POST` répond `405 Method Not Allowed`. Vous écrirez des articles via l'API à l'étape 14.

---

## Étape 13 : façonner la sortie avec un Serializer

Par défaut, le ViewSet renvoie tous les champs du modèle. Un Serializer vous permet de contrôler la forme de la réponse : masquer des champs internes, les renommer, ou en marquer certains en lecture seule. C'est le contrat entre vos modèles et le JSON que sert votre API.

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

    pub author_id: i64,                                 // writable: NOT NULL with no default

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

Dès qu'un serializer est attaché, **ses champs constituent toute la surface d'écriture** : tout ce qu'un client envoie et qui n'est pas listé ici est écarté avant l'`INSERT`. C'est pourquoi `author_id` y figure. Il est `NOT NULL` et sans valeur par défaut sur le modèle, donc l'omettre fait échouer toute création sur la contrainte NOT NULL. `status` peut rester en dehors, car le modèle lui donne `default = "'draft'"`.

Le type de chaque champ du serializer reflète le champ correspondant du modèle, donc `id` et `published_at` conservent leur enveloppe `Auto<…>` héritée du modèle (un `Auto<i64>` se sérialise toujours en un simple entier JSON). Enregistrez ensuite le module en ajoutant `pub mod post_serializer;` à `src/lib.rs`.

Branchez le serializer dans le ViewSet avec l'attribut `serializer` — les réponses de liste, de récupération et de création sont alors rendues via celui-ci. `fields` reste obligatoire : un body n'écrit que les champs listés à la fois ici et dans le serializer :

```rust
#[derive(ViewSet)]
#[viewset(
    model = Post,
    fields = "id, title, body, author_id, published_at",
    serializer = crate::post_serializer::PostSerializer,
    ordering = "-published_at",
    read_only,
)]
pub struct PostViewSet;
```

Ceci fonctionne à l'identique sur PostgreSQL, MySQL et SQLite. Les redéfinitions `method` / `read_only` / `source` / `write_only` s'appliquent toutes à la réponse, et **les corps de requête sont eux aussi validés via le serializer** : `create` / `update` exécutent sa `validate()` (par champ et inter-champs), renvoyant un `422` (`validation_failed`) avec une liste de messages par champ dans `details` (`{field: [messages]}`) en cas d'échec, et les champs en lecture seule / calculés qu'un client tenterait de poster sont ignorés. (Remarque : les champs de serializer `nested` / `many` nécessitent que les lignes liées soient chargées via `select_related` ; sinon ils s'affichent avec leur valeur par défaut.) Voir le [guide des ViewSets](viewsets.md) pour le comportement complet en entrée et en sortie.

---

## Étape 14 : inscription, connexion et authentification JWT

Jusqu'ici, tout le monde peut lire les articles et personne ne peut en écrire. Dans cette étape, les gens créent un compte, se connectent et reçoivent un jeton. Avec ce jeton, ils peuvent écrire des articles. La lecture reste publique.

D'abord, quelques mots :

- Un **hachage de mot de passe** est une transformation à sens unique d'un mot de passe. Vous stockez le hachage, jamais le mot de passe. À la connexion, on hache à nouveau la tentative et on compare.
- Un **JWT** (JSON Web Token) est un petit texte signé. Le serveur le signe avec une clé secrète, donc personne ne peut le modifier sans la clé. Il contient l'id de l'utilisateur et une heure d'expiration.
- Le client renvoie le jeton à chaque requête, dans l'en-tête `Authorization: Bearer <token>`.
- Un **middleware** est du code qui s'exécute avant votre handler. Ici, il vérifie le jeton.

`rustango::passwords` hache les mots de passe (argon2id). `rustango::jwt` signe et vérifie les jetons (HS256). Les deux sont actifs par défaut. Le code final se trouve dans le projet d'exemple [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog), et son `tests/auth_flow.rs` vérifie chaque cas ci-dessous.

### 14a. Un modèle d'utilisateur

Ajoutez un second modèle dans `src/blog/models.rs`, sous `Post`. Les imports en haut du fichier restent tels quels :

```rust
/// Someone who can sign in to the API (Step 14).
#[derive(Model, Clone, Debug)]
#[rustango(table = "api_users", display = "username")]
pub struct ApiUser {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 150, unique)]
    pub username: String,

    // An argon2 hash, never the password itself.
    #[rustango(max_length = 255)]
    pub password_hash: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}
```

`unique` fait refuser par la base une seconde ligne avec le même nom d'utilisateur. Créez et appliquez la migration, comme à l'étape 9 :

```bash
cargo run -- makemigrations     # wrote ./migrations/0002_create_api_users.json
cargo run -- migrate
```

Ne mettez jamais `password_hash` dans un ViewSet ni dans un serializer : le framework refuse de monter un ViewSet qui nomme une colonne d'identifiants, donc un hachage ne peut pas fuir par l'API. L'admin affiche la nouvelle table et masque cette colonne.

### 14b. La clé de signature

Le serveur a besoin d'une clé secrète pour signer les jetons. Créez-en une d'au moins 32 octets aléatoires et mettez-la dans `.env` :

```bash
echo "JWT_SECRET=$(openssl rand -base64 32)" >> .env
```

Dans PowerShell, lancez les trois mêmes lignes que l'étape 18 utilise pour `RUSTANGO_SESSION_SECRET`, puis ajoutez la valeur à `.env` sous la forme `JWT_SECRET=...`. Utilisez une autre clé en production et gardez-la hors de git. `.env` est déjà dans `.gitignore`.

### 14c. S'inscrire, se connecter, vérifier le jeton

Créez `src/blog/auth.rs`. Ce fichier contient tout le code de connexion. Il est présenté ici en trois parties ; collez-les l'une après l'autre. Ajoutez ensuite `pub mod auth;` dans `src/blog/mod.rs`.

**Partie 1 : imports, types et petites fonctions d'aide.**

```rust
//! Sign-up, sign-in and the Bearer-token guard for the blog API (Step 14).

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::api_errors::ApiError;
use rustango::core::{ModelSchema, WhereExpr};
use rustango::jwt::{self, Claims};
use rustango::login_throttle::{self, ClientIp, LoginRefused, LoginScope};
use rustango::passwords::{self, PasswordError};
use rustango::prelude::*;
use rustango::sql::{sqlx, ExecError, Pool};
use rustango::viewset::{match_nothing, ViewSetFilter, WritePin};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::blog::models::{ApiUser, Post};

/// How long a token from `/api/login` stays valid.
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// What `/api/register` and `/api/login` read from the request body.
#[derive(Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// What `/api/login` sends back.
#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

/// The signed-in user. The guard puts it on the request for the handlers.
#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
}

/// The signing key, from the `JWT_SECRET` environment variable.
fn jwt_secret() -> Result<Vec<u8>, ApiError> {
    match std::env::var("JWT_SECRET") {
        Ok(secret) if secret.len() >= 32 => Ok(secret.into_bytes()),
        _ => Err(ApiError::logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth",
            "JWT_SECRET is unset or shorter than 32 bytes",
        )),
    }
}

/// Methods that only read. Everything else needs a token.
pub fn is_read(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Usernames are stored lower-case, so `Alice` and `alice` are one account.
fn normalize(username: &str) -> String {
    username.trim().to_lowercase()
}

/// A 500 that logs the cause and tells the client nothing about it.
fn server_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, "auth", e)
}

/// The same answer for an unknown user and a wrong password.
fn bad_credentials() -> ApiError {
    ApiError::unauthorized("invalid username or password")
}
```

`ApiError` envoie la forme d'erreur JSON habituelle du framework (`{"error": ..., "message": ..., "status": ...}`), donc vos erreurs ressemblent à celles du ViewSet.

**Partie 2 : les deux handlers.** Un handler est une `async fn` qui répond à une route. `State(pool)` lui donne le pool de la base ; `Json(body)` lit le corps JSON dans `Credentials`.

```rust
/// `POST /api/register` — create an account. 201 on success, 409 if the name is taken.
pub async fn register(
    State(pool): State<Pool>,
    Json(body): Json<Credentials>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let username = normalize(&body.username);
    let name_ok = (3..=150).contains(&username.len())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !name_ok {
        return Err(ApiError::validation(
            "username: 3-150 characters, letters, digits, '_', '-' or '.'",
        )
        .with_field("username"));
    }
    // An upper limit too: hashing a huge password costs the server.
    if body.password.len() > 128 || !passwords::strength_score(&body.password).is_empty() {
        return Err(ApiError::validation(
            "password: 12-128 characters, with a digit or a symbol, not a common one",
        )
        .with_field("password"));
    }

    // argon2 is slow on purpose; the `_async` call keeps the server responsive.
    let password_hash = passwords::hash_async(&body.password)
        .await
        .map_err(server_error)?;

    let mut user = ApiUser {
        id: Auto::default(),
        username,
        password_hash,
        created_at: Auto::default(),
    };
    match user.save(&pool).await {
        Ok(_) => {}
        // The unique index on `username` decides, so two parallel sign-ups cannot both win.
        Err(ExecError::Driver(sqlx::Error::Database(db))) if db.is_unique_violation() => {
            return Err(ApiError::conflict("username already taken").with_field("username"));
        }
        Err(e) => return Err(server_error(e)),
    }

    let id = user.id.get().copied().unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "username": user.username })),
    ))
}

/// `POST /api/login` — check the password and hand out a short-lived token.
pub async fn login(
    State(pool): State<Pool>,
    ip: ClientIp,
    Json(body): Json<Credentials>,
) -> Result<Json<TokenResponse>, Response> {
    // First: with no valid key every login is a 500, right password or not.
    let secret = jwt_secret().map_err(IntoResponse::into_response)?;
    let username = normalize(&body.username);

    // Before any lookup: 429 once this IP (or everyone) failed too often.
    // `Sso(name)`: its own total limit, a per-IP limit shared with SSO sign-ins, no account lock.
    let scope = LoginScope::Sso("blog-api".to_owned());
    let attempt = login_throttle::shared()
        .begin(&scope, &ip, &username)
        .await
        .map_err(IntoResponse::into_response)?;

    let found = ApiUser::objects()
        .where_(ApiUser::username.eq(username))
        .first(&pool)
        .await
        .map_err(|e| server_error(e).into_response())?;

    let Some(user) = found else {
        // Do the same slow work as a real check, so timing does not reveal the name is unknown.
        if passwords::verify_dummy_async(&body.password).await.is_err() {
            return Err(LoginRefused::Busy.into_response()); // 503: no hashing slot free
        }
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    };
    let password_ok = match passwords::verify_async(&body.password, &user.password_hash).await {
        Ok(ok) => ok,
        Err(PasswordError::Busy) => return Err(LoginRefused::Busy.into_response()),
        // A corrupt stored hash counts as a wrong password, not a server error.
        Err(_) => false,
    };
    if !password_ok {
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    }
    attempt.succeeded().await;

    let id = user.id.get().copied().unwrap_or_default();
    let claims = Claims::new(id.to_string()).ttl(TOKEN_TTL); // subject = user id
    let access_token =
        jwt::encode(&claims, &secret).map_err(|e| server_error(e).into_response())?;
    Ok(Json(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: TOKEN_TTL.as_secs(),
    }))
}
```

Ce que fait `login`, dans l'ordre :

- `jwt_secret()` passe en premier. Sans `JWT_SECRET` valide, chaque connexion répond `500`, donc un bon et un mauvais mot de passe se ressemblent.
- `login_throttle::shared().begin(...)` est la même barrière que celle des connexions intégrées. Elle compte les échecs par IP client et au total, et répond `429 Too Many Requests` quand il y en a trop. Vous signalez chaque résultat avec `attempt.failed()` ou `attempt.succeeded()` ; un succès rend le compte. Le scope `LoginScope::Sso("blog-api")` donne à cette connexion sa propre limite totale ; sa limite par IP est partagée avec les connexions SSO du framework, et il n'a pas de verrouillage de compte.
- Un nom d'utilisateur inconnu et un mauvais mot de passe reçoivent le même `401` avec le même message. Sinon, un attaquant pourrait essayer des noms et savoir quels comptes existent.
- `verify_dummy_async` fait une vérification complète contre un faux hachage quand l'utilisateur n'existe pas, donc les deux cas prennent à peu près le même temps.
- `Claims::new(id)` place l'id de l'utilisateur dans le jeton comme *subject*. `.ttl(TOKEN_TTL)` le fait expirer au bout de 15 minutes.

**Partie 3 : le garde et la règle des articles.**

```rust
/// The user behind an `Authorization: Bearer <token>` header, or a 401.
pub async fn authenticate(pool: &Pool, parts: &Parts) -> Result<CurrentUser, ApiError> {
    let unauthorized = || ApiError::unauthorized("missing, invalid or expired token");

    let token = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .ok_or_else(unauthorized)?;
    // `decode` checks the signature and the expiry time.
    let claims = jwt::decode(token, &jwt_secret()?).map_err(|_| unauthorized())?;
    let id: i64 = claims
        .subject()
        .and_then(|sub| sub.parse().ok())
        .ok_or_else(unauthorized)?;

    // A deleted account's tokens stop working at once.
    let user = ApiUser::objects()
        .where_(ApiUser::id.eq(id))
        .first(pool)
        .await
        .map_err(server_error)?
        .ok_or_else(unauthorized)?;
    Ok(CurrentUser {
        id,
        username: user.username,
    })
}

/// Middleware: GET, HEAD and OPTIONS pass, every other method needs a valid token.
pub async fn require_user_for_writes(
    State(pool): State<Pool>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if is_read(request.method()) {
        return Ok(next.run(request).await);
    }
    let (mut parts, body) = request.into_parts();
    let user = authenticate(&pool, &parts).await?;
    parts.extensions.insert(user);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// ViewSet rule: a new post's author is the signed-in user, and only the
/// author may change or delete it. Reads see every post.
pub struct AuthorFromToken;

impl ViewSetFilter for AuthorFromToken {
    fn filter(&self, _: &HashMap<String, String>, _: &'static ModelSchema) -> Vec<WhereExpr> {
        Vec::new()
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        if is_read(&parts.method) {
            return Vec::new();
        }
        match parts.extensions.get::<CurrentUser>() {
            Some(user) => vec![Post::author_id.eq(user.id).into()],
            // No user: match no row. An empty Vec would mean "every row".
            None => vec![match_nothing(schema)],
        }
    }

    fn write_pins(&self, parts: &Parts, _: &'static ModelSchema) -> Vec<WritePin> {
        match parts.extensions.get::<CurrentUser>() {
            // Whatever `author_id` the body sends, this value is stored.
            Some(user) => vec![WritePin::field("author_id", user.id)],
            None => vec![WritePin::Deny],
        }
    }
}
```

- `require_user_for_writes` est le middleware. Les requêtes `GET`, `HEAD` et `OPTIONS` passent (`is_read`). Pour toute autre méthode, il appelle `authenticate`, qui répond `401` si l'en-tête manque, si le jeton a été modifié ou a expiré, ou si l'utilisateur n'existe plus. En cas de succès, il place un `CurrentUser` sur la requête, où le code suivant peut le lire.
- `AuthorFromToken` est un *filter backend* du ViewSet : il peut restreindre les lignes qu'une requête voit et fixer des valeurs à l'écriture. `write_pins` enregistre l'id de l'utilisateur connecté dans `author_id` à la création, quoi que dise le corps, et une mise à jour ne peut jamais le changer. Sans le pin, un client pourrait désigner un autre utilisateur comme auteur ; c'est le pin qui l'en empêche. `filter_with` ne laisse un utilisateur modifier ou supprimer que ses propres articles ; l'article de quelqu'un d'autre répond `404`.

### 14d. Monter les routes

Ajoutez les routes de connexion dans `src/blog/urls.rs`. Elles ont besoin du pool de la base, donc elles ont leur propre fonction à côté de `api()`. Le fichier a déjà `use axum::Router;` ; ajoutez les autres imports en haut :

```rust
use std::time::Duration;

use axum::routing::post;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::sql::Pool;

use super::auth;

/// `/register` and `/login` (Step 14). They need the pool, so `main.rs` mounts them.
pub fn auth_routes(pool: Pool) -> Router {
    // Each sign-up costs a password hash: 10 a minute per client IP.
    let register = Router::new()
        .route("/register", post(auth::register))
        .rate_limit(RateLimitLayer::per_ip(10, Duration::from_secs(60)));
    Router::new()
        .route("/login", post(auth::login)) // throttled inside the handler
        .merge(register)
        .with_state(pool)
}
```

Ajoutez ensuite dans `src/urls.rs` une fonction qui rassemble tout ce qui va sous `/api` :

```rust
/// Everything `main.rs` mounts under `/api`: the posts and the sign-in routes (Step 14).
pub fn rest_api(pool: Pool) -> Router {
    crate::post_view_set::PostViewSet::router("/posts", pool.clone())
        .merge(crate::blog::urls::auth_routes(pool))
}
```

Dans `src/main.rs`, remplacez la ligne `/api` de l'étape 12b par :

```rust
.nest_with("/api", urls::rest_api)
```

### 14e. Protéger les écritures des articles

Remplacez `src/post_view_set.rs` par la version ci-dessous. Les attributs de `#[derive(ViewSet)]` n'ont pas d'option pour un filter backend, donc ce fichier utilise les mêmes options via le builder `ViewSet`. `PostViewSet::router(prefix, pool)` garde son nom, donc le reste de votre code ne change pas.

```rust
use axum::middleware::from_fn_with_state;
use axum::Router;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::viewset::ViewSet;

use crate::blog::auth::{require_user_for_writes, AuthorFromToken};
use crate::blog::models::Post;
use crate::post_serializer::PostSerializer;

/// The Post API: anyone can read, writes need a Bearer token (Step 14).
pub struct PostViewSet;

impl PostViewSet {
    pub fn router(prefix: &str, pool: impl Into<Pool>) -> Router {
        let pool = pool.into();
        ViewSet::for_model(
            Post::SCHEMA,
            &["id", "title", "body", "author_id", "published_at"],
        )
        .serializer::<PostSerializer>()
        .filter_fields(&["author_id", "status"])
        .search_fields(&["title", "body"])
        .ordering(&[("published_at", true)])
        .page_size(20)
        .filter_backend(AuthorFromToken)
        // Writes pass the framework's gate because the layer below guards them.
        .allow_anonymous()
        .router(prefix, pool.clone())
        .layer(from_fn_with_state(pool, require_user_for_writes))
    }
}
```

`allow_anonymous()` indique au framework que les écritures ne sont pas ouvertes par erreur : votre propre middleware, ajouté avec `.layer(...)`, vérifie chaque écriture avant que le ViewSet ne la voie. Les deux lignes comptent : sans `allow_anonymous()` le routeur refuse de se monter, et sans `.layer(...)` n'importe qui pourrait écrire.

Le serializer de l'étape 13 liste toujours `author_id`, donc les réponses l'affichent. Un client peut encore l'envoyer, mais le pin remplace la valeur.

### 14f. Essayer avec curl

Lancez le serveur avec `cargo run`, puis dans un autre terminal :

```bash
# 1. Create an account: 201
curl -i -X POST http://localhost:8080/api/register \
     -H "content-type: application/json" \
     -d '{"username":"alice","password":"correct-Horse-battery-9"}'

# 2. Log in: 200 with {"access_token":"eyJ...","token_type":"Bearer","expires_in":900}
curl -X POST http://localhost:8080/api/login \
     -H "content-type: application/json" \
     -d '{"username":"alice","password":"correct-Horse-battery-9"}'

# 3. Keep the token in a shell variable (paste your own value)
TOKEN=eyJ...

# 4. Write a post with the token: 201, and "author_id" is alice's id
curl -X POST http://localhost:8080/api/posts \
     -H "authorization: Bearer $TOKEN" \
     -H "content-type: application/json" \
     -d '{"title":"Hello","content":"My first signed-in post"}'

# 5. The same request without the token: 401
curl -i -X POST http://localhost:8080/api/posts \
     -H "content-type: application/json" \
     -d '{"title":"Nope","content":"x"}'

# 6. Reading stays public: 200
curl http://localhost:8080/api/posts
```

Un mauvais mot de passe, ou un nom d'utilisateur qui n'existe pas, donne le même `401 {"error":"unauthorized","message":"invalid username or password",...}`. Au bout de 15 minutes, le jeton expire et les écritures répondent de nouveau `401` : reconnectez-vous.

> **Bonnes pratiques dès le départ**
>
> - **Secret depuis l'environnement.** `JWT_SECRET` vient de `.env` ou de l'environnement du serveur, jamais du code ni de git. Utilisez 32 octets aléatoires ou plus (`openssl rand -base64 32`) et une clé différente par environnement.
> - **Jetons de courte durée.** 15 minutes limitent les dégâts d'un jeton volé. Ne montez pas à plusieurs jours pour éviter une connexion.
> - **Ne journalisez jamais de secrets.** Ne journalisez ni n'affichez de mots de passe, de jetons ou l'en-tête `Authorization`, et ne mettez jamais un jeton dans une URL. L'`AccessLogLayer` de l'étape 15 ne journalise ni en-têtes ni corps et masque les valeurs de la query.
> - **Erreurs de connexion génériques.** Le même `401` pour un utilisateur inconnu et un mauvais mot de passe, comme ci-dessus.
> - **Limitez les tentatives de connexion.** Gardez la barrière `login_throttle` dans `login` et la limite de débit sur `/register`. Derrière un proxy, déclarez les proxies de confiance (étape 18), sinon tous les clients partagent l'IP du proxy.
> - **HTTPS en production.** Un jeton envoyé en HTTP simple peut être lu en chemin. Terminez HTTPS sur votre proxy (étape 18).
> - **Hachage côté serveur seulement, avec argon2 seulement.** Utilisez `passwords::hash_async` / `verify_async` ; ne stockez jamais un mot de passe ni un hachage rapide comme SHA-256.
> - **Pas d'écritures ouvertes.** Chaque route d'écriture a besoin d'une vérification, comme la couche de 14e. Un ViewSet avec `allow_anonymous` et sans couche laisse n'importe qui modifier vos données.

### 14g. Étape suivante : jetons de rafraîchissement

`rustango::jwt` émet des jetons uniques et sans état : quand l'un expire, l'utilisateur se reconnecte, et vous ne pouvez pas annuler un jeton avant son expiration. Quand il vous faut des sessions plus longues ou une déconnexion immédiate, passez au modèle complet : des jetons d'**accès** de courte durée, un jeton de **rafraîchissement** de longue durée dans un cookie HttpOnly, la rotation et une liste noire de JTI pour la révocation. Activez la feature `tenancy` et utilisez `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, dont les méthodes `issue_pair_with` / `verify_access` / `refresh` gèrent la paire pour vous.

---

## Étape 15 : ajouter le middleware de sécurité

Le middleware englobe chaque requête pour y ajouter un comportement transversal. Ce code se place dans `src/main.rs` : il remplace la ligne `nest_with` de `/api` de l'étape 14d, afin que les couches englobent chaque route sous `/api`. Ici, vous empilez les identifiants de requête, la journalisation des accès, la limitation de débit, le CORS et les en-têtes de sécurité en une seule chaîne. Chaque `.method(...)` ajoute une couche ; l'ordre des appels détermine l'ordre de la pile. Voir le [guide du middleware](middleware.md) pour le catalogue complet des couches et les règles d'ordonnancement.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use std::time::Duration;

.nest_with("/api", |pool| urls::rest_api(pool)
    .request_id(RequestIdLayer::default())
    .access_log(AccessLogLayer::default())                      // PII-redacted
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
    .cors(CorsLayer::new()
        .allow_origins(vec!["https://app.example.com"])
        .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]))
    .security_headers(
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    ))
```

Le reste de la chaîne du `Cli` ne change pas. Chaque requête `/api` passe désormais par la pile complète de middleware. `.with_welcome()` n'a aucun effet ici : le squelette route déjà `/`, donc la page d'accueil est ignorée avec un avertissement.

---

## Étape 16 : écrire des tests

**Rustango** inclut un client de test qui pilote votre routeur en process, ce qui vous permet de faire des assertions sur de vraies réponses HTTP sans démarrer de serveur ni toucher au réseau. Générez un fichier de test :

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

Les générateurs `make:*` prennent un nom en PascalCase ; `PostSmoke` devient le fichier en snake_case `tests/post_smoke.rs`.

Modifiez `tests/post_smoke.rs`. Les tests d'intégration vivent dans une crate séparée, donc ils construisent eux-mêmes le routeur testé, avec le même `urls::rest_api` que `main.rs` monte à l'étape 14d :

```rust
use myblog::urls;
use rustango::sql::Pool;
use rustango::test_client::TestClient;
use serde_json::json;

async fn app() -> axum::Router {
    // A test in `tests/` is a separate crate and never runs `main`, so
    // nothing has loaded `.env` for it. Without this line `DATABASE_URL`
    // is unset and both tests panic before reaching the database.
    let _ = dotenvy::dotenv();

    let pool = Pool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    axum::Router::new().nest("/api", urls::rest_api(pool))
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
async fn create_post_without_a_token_returns_401() {
    let client = TestClient::new(app().await);
    let response = client.post("/api/posts")
        // The serializer's field names: `content` is `Post.body` renamed.
        .json(&json!({ "title": "Test", "content": "x" }))
        .send().await;
    assert_eq!(response.status, 401);
}
```

Deux points de cet extrait sont faciles à rater :

- **`dotenvy::dotenv()`** — si vous l'omettez, *les deux* tests échouent, avant même la première requête.
- **`content`, pas `body`** — le serializer accepte les deux ici, puisque `source = "body"` garde le nom du modèle valable en entrée, mais `content` est le nom que votre API publie réellement.

Le second test prouve que le garde de l'étape 14 est en place. Pour tester une écriture connectée, inscrivez-vous et connectez-vous d'abord avec le même client, puis envoyez le jeton dans un en-tête `authorization`. Le [`tests/auth_flow.rs`](https://github.com/ujeenet/rustango/blob/develop/crates/rustango/examples/getting_started_blog/tests/auth_flow.rs) du projet d'exemple le fait pour chaque cas de l'étape 14 : inscription, nom en double, connexion, mauvais mot de passe, utilisateur inconnu, jetons absents, modifiés et expirés, et l'auteur tiré du jeton.

`Pool` est le pool multi-backend, et `router` accepte le pool de n'importe quel backend : ce fichier compile donc tel quel sur PostgreSQL, MySQL et SQLite.

> **Attention :** les tests d'intégration dans `tests/` ne peuvent faire `use myblog::…` que pour les modules déclarés `pub mod` dans `src/lib.rs`. Un squelette neuf a cette cible de bibliothèque ; vérifiez que `pub mod post_view_set;` et `pub mod urls;` y figurent. Un projet issu d'un ancien `cargo-rustango` peut n'être qu'un binaire : ajoutez un `src/lib.rs` avec les lignes `pub mod …;` nécessaires, ou construisez le routeur en ligne dans le test, de la façon dont `make:test` génère sa fonction `app()`.

Exécutez les tests :

```bash
cargo test --test post_smoke
```

---

## Étape 17 : exécuter la vérification système

Avant de déployer, exécutez le vérificateur intégré. Il signale les erreurs de configuration courantes (comme un `RUSTANGO_SESSION_SECRET` trop faible ou une base de données inaccessible) avant qu'elles ne se manifestent en production.

```bash
cargo run -- check --deploy
```

`--deploy` traite un `RUSTANGO_SESSION_SECRET` absent comme une erreur, donc définissez-en un d'abord (étape 3), même en local. Dans votre environnement de développement local, vous verrez alors quelque chose comme :

```
running rustango system check (deploy mode)...
  [info]    6 models registered via inventory
  [info]    database reachable
  [info]    1 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET decodes to a valid ≥32-byte key
  [info]    config tier resolved to `dev`
  [warning] RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier
  [warning] DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in production (…)
```

(Le nombre exact de modèles/migrations et les notes supplémentaires dépendent de votre projet et de ses features.) Ces deux avertissements sont ceux attendus en environnement de développement. Dans une configuration de production — `RUSTANGO_ENV=prod` et un `DATABASE_URL` de base de données géré — ils disparaissent et vous verrez `all checks passed`. Corrigez tout avertissement ou erreur restant avant de déployer en production.

---

## Étape 18 : déployer en production

La façon de déployer dépend de votre plateforme (Fly, Railway, Kubernetes, ECS nu, et ainsi de suite). Les étapes côté framework sont les mêmes partout ; l'option `--release` construit un binaire optimisé :

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export RUSTANGO_SESSION_SECRET=$(openssl rand -base64 32)

# 2. Run migrations (ship `migrations/` AND `system/migrations/` with the binary)
cargo run --release -- migrate

# 3. Audit
cargo run --release -- check --deploy

# 4. Build binary
cargo build --release

# 5. Run with a process supervisor (systemd / docker / k8s)
./target/release/myblog
```

Dans PowerShell :

```powershell
$env:RUSTANGO_ENV = "prod"
$env:DATABASE_URL = "postgres://prod-host/myblog"
$bytes = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$env:RUSTANGO_SESSION_SECRET = [Convert]::ToBase64String($bytes)
cargo run --release -- migrate
```

> **Vérifiez d'abord vos routes d'écriture.** Un ViewSet ou une vue est-il encore en `allow_anonymous` sans couche d'auth devant ? Alors n'importe qui sur internet peut modifier ces données. Passez-le en `read_only` ou protégez-le comme à l'[étape 14e](#14e-protéger-les-écritures-des-articles).

Assurez-vous que votre proxy inverse :
- Termine le HTTPS
- Transmet `X-Forwarded-For`, et l'app monte `RealIpLayer::trust_proxies([...])` avec ce proxy (`Cli::with_trusted_proxies([...])?` ou `server::Builder::real_ip`), pour des IP précises dans `AccessLogLayer` et les throttles (voir [security.md](security.md))
- Transmet l'en-tête `Host` d'origine sans le modifier : le routage des tenants, la détection de l'apex et `AllowedHostsLayer` ne lisent que `Host`
- Transmet `X-Forwarded-Proto` (et `X-Forwarded-Host`, qui ne fixe que l'URL de redirection SSO derrière un proxy de confiance)

`Cli::run()` renseigne déjà `ConnectInfo` pour la limitation de débit et le filtrage par IP. Si vous servez le routeur vous-même, utilisez `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`.

---

## Où aller ensuite

| Sujet | Doc |
|---|---|
| Version exécutable de ce guide | [`examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog) |
| Tâches courantes (inscription, envoi de fichiers, e-mail, tâches, déploiement) | [`docs/common-tasks.md`](common-tasks.md) |
| Chaque sous-commande `manage` | [`docs/manage.md`](manage.md) |
| Recueil de recettes ORM (filtres avancés, agrégations, M2M, suppression douce) | [`docs/orm.md`](orm.md) |
| Middleware (le catalogue complet des couches + ordonnancement) | [`docs/middleware.md`](middleware.md) |
| Benchmarks de performance (vs Go) | [`docs/benchmarks.md`](benchmarks.md) |
| Conventions d'API (nommage, patrons de construction, feature gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Fonctionnalités de sécurité en détail | [`docs/security.md`](security.md) |
| Multi-tenancy | [`docs/tenancy.md`](tenancy.md) |
| Documentation de l'API | <https://docs.rs/rustango> |

Si vous rencontrez quelque chose qui ne fonctionne pas ou qui n'est pas clair, ouvrez un ticket.
