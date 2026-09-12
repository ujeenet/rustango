# Scaffolding

**Rustango** dispose de deux niveaux de génération de code, tous deux inspirés des générateurs que vous connaissez déjà avec Django et Laravel — de sorte que vous n'avez presque jamais à câbler de code répétitif à la main :

1. **Le générateur de projet** — `cargo rustango new` crée un tout nouveau projet à partir d'un template.
2. **Les générateurs internes au projet** — `manage startapp` et la famille `manage make:*` ajoutent des apps, des vues, des sérialiseurs, des jobs, et bien plus au sein d'un projet existant.

[![`cargo rustango new` scaffolds a complete, ready-to-run project — Cargo manifest, config tiers, Docker, migrations, and src — in one command](../img/scaffolding.png)](../img/scaffolding.png)

## Table des matières

- [Installer le générateur](#install-the-generator)
- [Créer un projet : `cargo rustango new`](#create-a-project-cargo-rustango-new)
- [Ce qui est généré](#what-gets-generated)
- [Ajouter un module fonctionnel : `manage startapp`](#add-a-feature-module-manage-startapp)
- [Générer des fichiers individuels : les commandes `make:*`](#generate-single-files-the-make-commands)
- [Un flux typique](#a-typical-flow)

---

## Installer le générateur

`cargo rustango` est une sous-commande Cargo. Installez-la une fois, globalement :

```sh
cargo install cargo-rustango
```

Cela place un binaire `cargo-rustango` sur votre `PATH` ; Cargo l'expose alors comme `cargo rustango` (de la même manière que `django-admin` ou l'installeur `laravel` vous donnent une commande globale).

---

## Créer un projet : `cargo rustango new`

```sh
cargo rustango new <name> [--template api|fullstack|tenant]
                          [--backend postgres|sqlite|mysql]
                          [--features <liste>]
```

- **`<name>`** — le nom du projet (et de la crate). Il doit s'agir d'un nom de crate Cargo valide (`[A-Za-z_][A-Za-z0-9_-]*`), et le répertoire cible ne doit pas déjà exister.
- **`--template` / `-t`** — quel template utiliser pour l'échafaudage (par défaut : **fullstack**).
- **`--backend` / `-b`** — sur quelle base de données le projet tourne (par défaut : **postgres**).
- **`--features` / `-F`** — fonctionnalités **Rustango** supplémentaires, séparées par des virgules ou des espaces.
- **`--interactive` / `-i`** — choisir dans des menus à la place. Un `cargo rustango new` nu sur un terminal fait de même.
- **`--help` / `-h`**, **`--version`** — usage et version.

Lancé sans argument, il pose les questions :

```text
  rustango — new project
  (enter accepts the default; ctrl-c aborts)

  Project name: shop

  Template
    1) fullstack  ORM + auto-admin + forms — the usual starting point  (default)
    2) api        bare ORM + axum, no admin UI — for JSON-only services
    3) tenant     multi-tenancy: tenant registry + operator console
  > 3

  Database
    1) postgres  every feature, including schema-mode tenancy  (default)
    2) sqlite    a file beside the project — no server to run
    3) mysql     MySQL 8.0+ / MariaDB
  > 1

  Extra features  (numbers, e.g. `1 3 4` — enter for none)
     1) csrf         CSRF protection middleware for form POSTs
     2) sso          OIDC single sign-on for application users
     …
  > 1 2

  Same thing without the wizard:
    cargo rustango new shop --template tenant --backend postgres --features csrf,sso

  Create it? [Y/n]
```

L'assistant règle exactement les champs que règlent les flags et affiche la ligne de commande équivalente avant d'écrire quoi que ce soit — s'en servir une fois vous apprend les flags, et un seul chemin de code décide de ce que contient un projet. Sans terminal (script, job CI), il échoue avec un message au lieu d'attendre une réponse que personne n'est là pour donner.

### Les trois templates

Chacun correspond à une des trois formes d'application de **Rustango** :

| Template | Ce que vous obtenez | À utiliser quand |
|---|---|---|
| `api` | ORM nu + Axum, **sans admin** | Services et microservices JSON uniquement |
| `fullstack` *(par défaut)* | ORM + l'**admin automatique** | Une application web classique avec un back-office |
| `tenant` | Multi-tenancy + console opérateur + apps par tenant | Hébergement SaaS avec de nombreux tenants isolés |

```sh
cargo rustango new myblog                      # fullstack (the default)
cargo rustango new api_demo  --template api
cargo rustango new shop      --template tenant
```

### Choisir la base de données : `--backend`

```sh
cargo rustango new edge --backend sqlite
cargo rustango new shop --backend mysql
```

`--backend` décide de ce qu'utilise `cargo run`, et façonne tout le projet en conséquence — la `DATABASE_URL` dans `.env.example`, les services de `docker-compose.yml`, l'`url` de chaque palier de configuration et les instructions de démarrage du README. Choisissez `sqlite` et il n'y a aucun service de base de données : c'est un fichier à côté du projet, créé par le premier `cargo run -- migrate`.

Les trois backends restent câblés dans les `[features]` générées, les deux autres sont donc à un flag :

```sh
cargo run --no-default-features --features sqlite
```

### Activer plus du framework : `--features`

```sh
cargo rustango new saas --template tenant --features csrf,sso,cache-redis
```

Un template active un ensemble raisonnable ; `--features` ajoute les options qu'aucun d'eux n'atteint :

| Fonctionnalité | Ce qu'elle ajoute |
|---|---|
| `tenancy` | Multi-tenancy : registry de tenants, bases par tenant, console opérateur |
| `csrf` | Middleware de protection CSRF pour les POST de formulaires |
| `sso` / `admin-sso` | Authentification unique OIDC, pour les utilisateurs / pour le site d'admin |
| `passkey` | Authentification WebAuthn / passkey |
| `cache-redis` / `cache-page` | Backend de cache Redis / mise en cache de pages entières |
| `email-smtp` | Transport SMTP pour le framework e-mail |
| `mcp` | Serveur Model Context Protocol pour les agents IA |
| `testkit` / `test_utils` | Constructeurs de schéma, fabriques et constructeurs réservés aux tests |

`cargo rustango new --help` affiche cette liste. Les backends ne sont **pas** valides ici — passez par `--backend`. En nommer un comme fonctionnalité est refusé : cela figerait le backend du framework alors que la fonctionnalité propre au projet resterait éteinte, et `#[derive(Model)]` aligne ses émissions sur les fonctionnalités du projet.

---

## Ce qui est généré

Chaque template écrit un projet Cargo autonome :

```text
<name>/
  Cargo.toml            # the rustango dependency + features for this template
  .env.example          # copy to .env (DATABASE_URL, RUSTANGO_SESSION_SECRET, …)
  .gitignore
  rust-toolchain.toml   # pins the Rust toolchain
  docker-compose.yml    # a Postgres service to develop against
  Dockerfile            # production image
  README.md
  config/
    default.toml        # settings shared across every environment
    dev_settings.toml   # per-tier overrides …
    staging_settings.toml
    prod_settings.toml
  migrations/           # JSON migration files (committed to git)
  src/
    main.rs             # the single binary — HTTP server + every manage verb
    models.rs           # your #[derive(Model)] structs
    views.rs            # request handlers ("views")
    urls.rs             # pub fn api() -> Router that aggregates your routes
```

### Un seul binaire pour tout

`src/main.rs` est le seul point d'entrée. Il démarre le serveur HTTP **et** dispatche chaque verbe `manage` — il n'y a pas de `manage.py` séparé ni de `src/bin/manage.rs` :

```rust
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .api(urls::api())
        .with_welcome()  // friendly `/` page until you add a root handler
        .with_health()   // /health + /ready endpoints (fullstack & tenant)
        .run()
        .await
}
```

Ainsi, `cargo run` démarre le serveur, et `cargo run -- <verb>` exécute les migrations, les générateurs, et le reste.

En quoi les templates diffèrent à l'intérieur de `main.rs` / `urls.rs` :

- **api** — pas d'admin ; `urls::api()` se contente d'agréger vos propres routes.
- **fullstack** — `urls.rs` expose également `admin_router(pool)` (construit à partir de `admin::Builder::new(pool).build()`) afin que l'admin automatique se monte sur `/admin`.
- **tenant** — `main.rs` ajoute `.tenancy()`, servant la console opérateur sur le domaine apex et chaque tenant sous son propre sous-domaine. Les propres tables du framework sont générées dans un dossier **`system/migrations/`** à partir des modèles compilés (à la manière de Django) lors du premier `cargo run -- migrate` — aucun JSON de bootstrap livré à la main, donc le tout premier migrate fonctionne sans configuration supplémentaire.

### Configuration en couches

Les paramètres se chargent d'abord depuis `config/default.toml`, puis depuis `config/<RUSTANGO_ENV>_settings.toml` par-dessus. `RUSTANGO_ENV` vaut `dev` par défaut, si bien qu'un `cargo run` juste après l'échafaudage fonctionne sans aucune modification ; définissez `RUSTANGO_ENV=prod` en production pour prendre en compte `prod_settings.toml`.

### Premier lancement

```sh
cd <name>
cp .env.example .env
docker compose up -d        # start Postgres
cargo run -- migrate        # apply migrations
cargo run                   # serve
cargo run -- --help         # see every manage verb
```

---

## Ajouter un module fonctionnel : `manage startapp`

C'est l'équivalent du `startapp` de Django — il échafaude un module autonome regroupant des modèles, des vues et des routes liés entre eux :

```sh
cargo run -- startapp blog
```

Cela écrit `src/blog/` contenant `mod.rs`, `models.rs` (un modèle de départ nommé d'après l'app mise au singulier — `blog` → `Blog`), `views.rs`, `urls.rs`, et `tests.rs`, puis déclare le module dans `src/main.rs` et fusionne ses routes dans `urls::api()`.

Options :

- **`--into <dir>`** — échafaude sous un répertoire de base autre que `src/` (par exemple un membre de workspace).
- **`--with-manage-bin`** — génère aussi un `bin/manage.rs` (pour les architectures qui préfèrent un binaire manage séparé).

---

## Générer des fichiers individuels : les commandes `make:*`

Au sein d'un projet, les verbes `make:*` échafaudent un fichier à la fois. La référence complète, drapeau par drapeau, se trouve dans la [référence CLI manage](manage.md) ; les formes les plus courantes sont :

| Commande | Génère | Comparable à |
|---|---|---|
| `make:viewset <Name> [--model <M>]` | Un ViewSet CRUD façon DRF | DRF `ViewSet` |
| `make:serializer <Name> [--model <M>]` | Un sérialiseur pour la mise en forme des requêtes/réponses | Sérialiseur DRF |
| `make:api_routes <app>` | Un agrégateur de routes API pour une app | — |
| `make:form <Name>` | Un formulaire HTML avec validation | `Form` Django |
| `make:job <Name>` | Un gestionnaire de job en arrière-plan | Job Laravel / Celery |
| `make:notification <Name>` | Une notification multi-canal | Notification Laravel |
| `make:middleware <Name>` | Un squelette de middleware | Middleware Django / Laravel |
| `make:test <Name>` | Un module de test utilisant le client de test in-process | — |

```sh
cargo run -- make:viewset PostViewSet --model Post
cargo run -- make:serializer PostSerializer --model Post
cargo run -- make:test post_smoke
```

---

## Un flux typique

```sh
cargo rustango new myblog                              # 1. scaffold the project
cd myblog
cargo run -- startapp blog                             # 2. add a feature module
# …add fields to src/blog/models.rs…
cargo run -- makemigrations                            # 3. generate a migration
cargo run -- migrate                                   # 4. apply it
cargo run -- make:viewset PostViewSet --model Post     # 5. expose a JSON API
cargo run                                              # 6. serve
```
