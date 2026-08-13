# Scaffolding

**Rustango** dispose de deux niveaux de génération de code, tous deux inspirés des générateurs que vous connaissez déjà avec Django et Laravel — de sorte que vous n'avez presque jamais à câbler de code répétitif à la main :

1. **Le générateur de projet** — `cargo rustango new` crée un tout nouveau projet à partir d'un template.
2. **Les générateurs internes au projet** — `manage startapp` et la famille `manage make:*` ajoutent des apps, des vues, des sérialiseurs, des jobs, et bien plus au sein d'un projet existant.

[![`cargo rustango new` scaffolds a complete, ready-to-run project — Cargo manifest, config tiers, Docker, migrations, and src — in one command](img/scaffolding.png)](img/scaffolding.png)

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
```

- **`<name>`** — le nom du projet (et de la crate). Il doit s'agir d'un nom de crate Cargo valide (`[A-Za-z_][A-Za-z0-9_-]*`), et le répertoire cible ne doit pas déjà exister.
- **`--template` / `-t`** — quel template utiliser pour l'échafaudage (par défaut : **fullstack**).
- **`--help` / `-h`**, **`--version`** — usage et version.

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

Au sein d'un projet, les verbes `make:*` échafaudent un fichier à la fois. La référence complète, drapeau par drapeau, se trouve dans la [référence CLI manage](manage) ; les formes les plus courantes sont :

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
