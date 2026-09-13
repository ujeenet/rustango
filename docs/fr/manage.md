# Référence CLI `manage`

Ceci est l'outil en ligne de commande de **Rustango**, comme `manage.py` de
Django, `artisan` de Laravel, ou la commande `rails` de Rails. Dans un projet
généré via `cargo rustango new`, un seul binaire exécute chaque commande
(« verbe ») :

```bash
cargo run                          # runserver (no args = boot the HTTP server)
cargo run -- migrate               # any other verb
cargo run -- --help                # full subcommand list
```

[![One binary runs every manage verb — server, migrations, scaffolders, database utilities, and system commands — like Django's manage.py or Laravel's artisan](../img/manage.png)](../img/manage.png)

> **Source :** `rustango::manage` (`Cli`, le répartiteur de verbes) — derrière
> la fonctionnalité `manage` (activée par défaut).
>
> **Version exécutable :** chaque verbe présenté ici s'exécute dans un projet
> généré ; l'exemple
> [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog)
> est piloté par `cargo run -- migrate` et consorts.

> **Nouveau terme rencontré ici ?** *scaffold* (générateur de squelette),
> *migration*, *tenant* (locataire) — voir le [glossaire](glossary.md).

Le routeur de commandes se trouve dans [`rustango::manage::Cli`](https://docs.rs/rustango/latest/rustango/manage/struct.Cli.html) ;
votre `src/main.rs` le branche ainsi :

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

Les projets multi-tenant ajoutent `.tenancy()` à la chaîne. Cela fait basculer
le routeur vers [`rustango::tenancy::manage`](https://docs.rs/rustango/latest/rustango/tenancy/manage/index.html)
et débloque les commandes multi-tenant.

> **Forme plus ancienne** — les projets générés par
> `manage startapp --with-manage-bin` (ou datant d'avant la v0.16) livrent
> encore un `src/bin/manage.rs`. Ceux-ci utilisent
> `cargo run --bin manage -- <verb>`. Les deux formes acceptent les mêmes
> verbes.

Chaque commande affiche sa sortie sur stdout et se termine avec un code de
retour non nul en cas d'erreur de validation ou d'E/S. Exécutez
`cargo run -- --help` (ou `<verb> --help`) pour l'aide intégrée.

---

## Table des matières

- [Migrations](#migrations)
- [Migrations de données](#data-migrations)
- [Générateurs de projet / d'app](#project--app-scaffolders)
- [Générateurs de fichiers (`make:*`)](#file-generators-make)
- [Utilitaires de base de données](#database-utilities)
- [Commandes système](#system-commands)
- [Commandes de tenancy](#tenancy-commands)
- [Sous-commandes personnalisées](#custom-subcommands)
- [Flux de travail courants](#common-workflows)

---

## Migrations

### `makemigrations [name]`

Génère un fichier de migration à partir des changements de vos modèles —
comme le `makemigrations` de Django. Elle compare vos modèles enregistrés au
dernier instantané de schéma sauvegardé dans `migrations/` et écrit un
nouveau fichier JSON avec ce qui a changé.

```bash
cargo run -- makemigrations                          # auto-name (e.g. 0004_add_slug_to_posts)
cargo run -- makemigrations rename_status_to_state   # custom suffix
```

**Changements détectés automatiquement :**
- `CreateTable` / `DropTable`
- `AddColumn` / `DropColumn`
- `AlterColumnType` / `AlterColumnNullable` / `AlterColumnDefault` / `AlterColumnMaxLength`
- `AlterColumnUnique`
- `CreateIndex` / `DropIndex`
- `AddCheckConstraint` / `DropCheckConstraint`
- `CreateM2MTable` / `DropM2MTable`

**NON détectés automatiquement** (renommage vs suppression+ajout est ambigu) :
- `RenameTable`, `RenameColumn` — utilisez `--empty` et modifiez le JSON.

### `makemigrations --app <app>`

Limite la migration à une seule app. Elle écrit dans le répertoire
`migrations/` propre à cette app, sous
`<project_root>/<app>/migrations/`, et ne regarde que les modèles
appartenant à cette app.

```bash
cargo run -- makemigrations --app blog
cargo run -- makemigrations --app blog backfill_slugs
```

### `makemigrations --scope <registry|tenant>`

Réservé au multi-tenant. Écrit une seule migration pour uniquement les
modèles d'un scope donné — ceux dont l'attribut
`#[rustango(scope = "...")]` correspond. (Les tables « registry » sont
partagées entre tous les tenants ; les tables « tenant » vivent par
tenant.) Sans ce drapeau, un simple `makemigrations` dans un projet de
tenancy scinde automatiquement les changements en DEUX fichiers — un pour
les modèles registry, un pour les modèles tenant — afin que les tables
partagées du framework (`Org`, `Operator`) ne se retrouvent pas dans les
migrations par tenant exécutées par `migrate-tenants`.

```bash
cargo run -- makemigrations                       # tenancy: writes 0NN_<auto>.json (registry) + 0MM_<auto>.json (tenant) as needed
cargo run -- makemigrations --scope tenant        # explicit single-scope diff
cargo run -- makemigrations --scope registry      # explicit single-scope diff
```

Pourquoi cette scission compte : avant la v0.24.2, un simple
`makemigrations` sur un projet de tenancy regroupait les opérations sur
`rustango_operators` (une table registry) dans une migration tenant.
Lorsque `migrate-tenants` exécutait ce fichier, `rustango_operators` se
résolvait via `search_path` vers la copie registry et entrait en conflit
avec la contrainte déjà présente là-bas.

### `makemigrations --empty <name>`

Crée une migration vide (sans opérations `forward`) que vous remplissez
vous-même à la main — comme `makemigrations --empty` de Django. Utilisez-la
lorsque vous devez écrire des opérations de données ou de renommage que le
détecteur automatique ne peut pas générer. Modifiez le JSON résultant
vous-même.

```bash
cargo run -- makemigrations --empty rename_status_to_state
# Then edit migrations/0005_rename_status_to_state.json:
#   "forward": [
#     {"schema": {"RenameColumn": {"table": "posts", "old_column": "status", "new_column": "state"}}}
#   ]
```

### `makemigrations --merge`

Corrige un historique de migrations qui s'est scindé en deux branches —
même principe que le `makemigrations --merge` de Django (issue #346). Cela
se produit quand deux personnes exécutent chacune `makemigrations` sur leur
propre branche de fonctionnalité, de sorte que les deux nouveaux fichiers
pointent vers le même parent. Une fois les deux branches fusionnées,
l'historique compte deux « feuilles » (points de fin), et le prochain
`makemigrations` en choisirait une arbitrairement comme parent.

`--merge` détecte cette situation et écrit un `NNNN_merge.json` vide dont
le parent pointe vers la dernière feuille par ordre alphabétique,
réunissant l'historique en une seule chaîne. Son instantané de schéma
reflète l'état combiné, lu depuis le registre de modèles en direct — les
modèles des deux branches sont compilés à ce stade, donc l'instantané est
exact.

```bash
cargo run -- makemigrations --merge
# wrote migrations/0004_merge.json
#     merge node — empty `forward`, anchors the chain after divergent leaves
```

- **Déjà une seule chaîne** → affiche `no merge needed` et se termine
  proprement. Sûr à exécuter sur un historique sain.
- **Historiques vraiment séparés** (pas une collision de branches) →
  échoue au lieu d'inventer un parent. Même garde-fou que Django.
- **Ne peut pas être combiné** avec `--empty`, `--app`, `--scope`, ou un
  nom positionnel.

### `migrate`

Applique toutes les migrations en attente à la base de données, dans
l'ordre — comme le `migrate` de Django ou le `php artisan migrate` de
Laravel. C'est la commande à exécuter après `makemigrations` pour changer
réellement votre schéma.

```bash
cargo run -- migrate
cargo run -- migrate --dry-run                       # print SQL without writing
```

Chaque fichier s'exécute dans une transaction par défaut, donc un échec
annule tout le fichier. Réglez `"atomic": false` dans le JSON pour
désactiver ce comportement — nécessaire pour des instructions comme
`CREATE INDEX CONCURRENTLY` qui ne peuvent pas s'exécuter dans une
transaction.

En **mode tenancy** (`Cli::tenancy()`), `migrate` connaît le scope : elle
applique d'abord les migrations registry à la base de données registry
partagée, puis applique les migrations tenant à travers chaque tenant
actif. Pour un contrôle plus fin, utilisez
[`migrate-registry`](#migrate-registry) /
[`migrate-tenants`](#migrate-tenants).

### `migrate <target>`

Migre vers un point précis de l'historique, en avant ou en arrière —
comme `migrate <app> <name>` de Django. Nommez une migration pour y
accéder ; la cible spéciale `zero` défait tout.

```bash
cargo run -- migrate 0003_add_slug      # forward to 0003
cargo run -- migrate 0001_initial       # roll back to 0001 (unapply 0002+)
cargo run -- migrate zero               # unapply EVERY migration
```

### `migrate --squash`

Regroupe chaque migration **en attente** (non appliquée) en un seul diff
nouvellement généré — l'échappatoire pour l'itération de développement,
utile quand une pile de migrations à moitié terminées est plus simple à
régénérer qu'à corriger. Elle refuse de toucher à tout ce qui est déjà
appliqué.

```bash
cargo run -- migrate --squash
```

Le fichier régénéré enregistre les noms qu'il a regroupés dans sa liste
`replaces`. Cela compte dès qu'une autre base de données entre en jeu :
le checkout de votre collègue, le staging, ou la CI ont peut-être déjà
appliqué certains des fichiers que vous venez de supprimer. Sans
`replaces`, le `CREATE TABLE` du nouveau fichier entrerait en conflit
là-bas ; avec, le runner **réconcilie** au lieu de cela (voir ci-dessous).

### Réconciliation du squash

Un squash recrée l'état final des migrations qu'il remplace, donc ce que
le runner doit faire dépend entièrement de ce que contient déjà la base
de données cible. La décision est automatique :

| état de la base de données | ce qui se passe |
|---|---|
| fraîche — aucun historique, aucune table | le squash s'exécute réellement |
| chaque migration remplacée est dans le journal | enregistrée, prédécesseurs mis en sommeil, **aucun DDL** |
| des tables existent mais le journal n'a aucun historique | enregistrée, **aucun DDL** (`--fake-initial` de Django) |
| seulement *certaines* des lignes/tables remplacées sont présentes | **refusé**, en précisant ce qui manque |

Le cas partiel est délibérément une erreur bloquante : aucun choix
automatique n'y est sûr, donc le runner s'arrête et vous indique ce qu'il
a trouvé plutôt que de deviner. Résolvez-le avec `migrate --fake`
(ci-dessous).

Les migrations remplacées par un squash appliqué sont considérées comme
appliquées, donc vous pouvez laisser les anciens fichiers sur le disque
pour une ou deux versions — les déploiements qui ne les ont jamais
exécutés migrent tout de même correctement vers l'avant.

Les migrations ordinaires (non-squash) ne sont pas affectées : une
migration classique dont la table existe déjà échoue toujours
bruyamment, car il s'agit d'un vrai conflit et non d'un historique
équivalent connu.

### `migrate --fake <name>`

Marque une migration comme appliquée **sans exécuter son SQL** —
l'échappatoire opérateur pour quand la base de données est déjà dans
l'état cible mais que le journal ne le sait pas (une base de données
configurée hors bande, une table de journal supprimée, une migration
partiellement réussie, un squash partiel refusé). Répétez le drapeau
pour réparer plusieurs lignes à la fois.

```bash
cargo run -- migrate --fake 0004_add_indexes
cargo run -- migrate --fake 0004_add_indexes --system        # framework's own chain
cargo run -- migrate --fake 0004_add_indexes --all-tenants   # every active tenant
```

Le nom est d'abord validé par rapport au répertoire de migrations, donc
une faute de frappe ne peut pas créer une fausse ligne. Le marquage est
idempotent.

`--system` cible la propre chaîne de migrations du framework
(`system/migrations/`, enregistrée dans
`__rustango_system_migrations__`) plutôt que celle de votre projet.
`--all-tenants` diffuse le marquage à travers chaque tenant actif, en
rapportant chacun et en continuant malgré les échecs — les tables du
framework vivent par tenant, donc les réparer est une tâche par tenant.

### `downgrade [N]`

Annule les N dernières migrations appliquées (par défaut 1) — le
`migrate:rollback` de Laravel. Chaque migration doit être réversible :
les changements de schéma s'annulent automatiquement, mais les
opérations de données nécessitent un `reverse_sql` défini, sinon
l'annulation échoue.

```bash
cargo run -- downgrade                  # one step
cargo run -- downgrade 3                # three steps
```

### `showmigrations` / `status`

Liste chaque migration et indique si elle a été appliquée — comme le
`showmigrations` de Django. `[X]` signifie appliquée, `[ ]` signifie
encore en attente.

```bash
cargo run -- showmigrations
cargo run -- status                     # alias
```

Sortie :

```
[X] 0001_initial
[X] 0002_add_status
[ ] 0003_add_slug
```

---

## Migrations de données

### `add-data-op`

Ajoute une étape de données en SQL brut à une migration sans modifier de
JSON à la main. Utilisez-la quand vous devez transformer des lignes
existantes — remplir rétroactivement une colonne, nettoyer des données —
dans le cadre d'une migration. C'est l'équivalent de la migration de
données `RunSQL` de Django, générée pour vous depuis la ligne de
commande.

```bash
# New migration with up + down
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

# Append to an existing migration
cargo run -- add-data-op \
    --to 0003_add_slug \
    --sql "UPDATE posts SET slug = id::text"

# Irreversible (no rollback)
cargo run -- add-data-op \
    --sql "DELETE FROM legacy_data" \
    --name purge_legacy
```

| Flag | Requis | Description |
|---|:-:|---|
| `--sql <SQL>` | oui | SQL avant exécuté lors de `migrate` |
| `--reverse-sql <SQL>` | non | SQL de retour en arrière lors de `unapply` ; omettez-le pour une opération irréversible |
| `--name <name>` | non | Suffixe de nom pour la nouvelle migration ; par défaut `data_op` |
| `--to <migration>` | non | Ajoute à une migration existante au lieu d'en créer une |

Omettez `--reverse-sql` et l'étape est marquée `reversible: false` —
toute tentative de l'annuler échoue immédiatement.

---

## Générateurs de projet / d'app

### `cargo rustango new <name>` *(binaire séparé)*

Crée un nouveau projet **Rustango** — comme `django-admin startproject`
ou `laravel new`. C'est un outil séparé, donc installez-le d'abord avec
`cargo install cargo-rustango`. Choisissez parmi trois modèles :

```bash
cargo rustango new myblog                          # default = fullstack (ORM + admin)
cargo rustango new myapi --template api            # JSON-only, no admin
cargo rustango new shop --template tenant          # multi-tenancy
```

Écrit :

```
<name>/
  Cargo.toml
  .env.example
  .gitignore
  rust-toolchain.toml
  docker-compose.yml
  README.md
  migrations/                               (your app's migrations)
  system/migrations/                        (tenant template — framework tables, generated)
  src/{main,models,views,urls}.rs
```

Le modèle tenant fournit un dossier `system/migrations/` **vide**. Les
propres tables du framework (`rustango_orgs`, `rustango_users`,
rôles/permissions, …) sont générées dans ce dossier à partir des modèles
compilés lors du premier `cargo run -- migrate` — il n'y a pas de JSON
d'amorçage livré à la main. Voir [`migrate`](#migrate) /
[`migrate-registry`](#migrate-registry).

### `startapp <name> [flags]`

Crée une nouvelle app (un module de fonctionnalité) sous `src/<name>/` —
exactement comme le `startapp` de Django. Utilisez-la pour regrouper les
modèles, vues et URLs d'une partie de votre projet.

```bash
cargo run -- startapp blog
cargo run -- startapp shop --with-manage-bin             # also writes src/bin/manage.rs
cargo run -- startapp shop --into apps                   # write under src/apps/shop/ instead
```

Crée :

```
src/<name>/
  mod.rs
  models.rs
  views.rs
  urls.rs
```

Sûr à réexécuter — les fichiers existants sont laissés intacts. Une
étape manuelle : ajouter `pub mod <name>;` à `src/lib.rs` pour que Rust
compile le nouveau module.

---

## Générateurs de fichiers (`make:*`)

Ceux-ci créent des fichiers de démarrage pour des briques courantes —
à l'image des commandes `make:*` de Laravel (`make:controller`,
`make:model`, …). Chaque générateur écrit dans `src/<snake_name>.rs`
(ou `tests/<snake_name>.rs` pour `make:test`) et :

- Vérifie que le nom est valide (PascalCase, lettres/chiffres/underscore).
- Le convertit en snake_case pour le nom de fichier (`PostViewSet` →
  `post_view_set.rs`).
- Ne remplace pas un fichier existant.
- Vous rappelle d'ajouter `pub mod X;` à votre `lib.rs`.

### `make:viewset <Name> [--model <Model>]`

Génère une structure `#[derive(ViewSet)]` — un point d'accès REST pour
un modèle, comme un ViewSet de Django REST Framework. Les listes de
champs sont pré-ébauchées pour que vous les complétiez.

```bash
cargo run -- make:viewset PostViewSet --model Post
```

`src/post_view_set.rs` généré :

```rust
#[derive(ViewSet)]
#[viewset(model = Post, fields = "id, ", filter_fields = "", search_fields = "", page_size = 20)]
pub struct PostViewSet;
```

Montez-le avec : `.merge(PostViewSet::router("/api/posts", pool.clone()))`.

### `make:serializer <Name> [--model <Model>]`

Génère une structure `#[derive(Serializer)]` — contrôle la façon dont un
modèle est converti vers et depuis JSON (comme un serializer DRF).

```bash
cargo run -- make:serializer PostSerializer --model Post
```

### `make:form <Name>`

Génère une structure `#[derive(Form)]` pour valider et traiter une saisie
de formulaire — comme un `Form` de Django.

```bash
cargo run -- make:form ContactForm
```

### `make:job <Name>`

Génère un squelette de tâche en arrière-plan (travail qui s'exécute hors
de la requête, comme une tâche Celery ou un job Laravel), avec un exemple
commenté montrant comment la planifier.

```bash
cargo run -- make:job EmailDigestJob
```

### `make:notification <Name>`

Génère une structure de notification qui construit un email — comme le
`make:notification` de Laravel.

```bash
cargo run -- make:notification WelcomeEmail
```

### `make:middleware <Name>`

Génère une fonction middleware — du code qui s'exécute avant et après
chaque requête (vérifications d'authentification, journalisation, etc.).
« axum » est le framework web sur lequel **Rustango** est construit, donc
l'ébauche correspond à la forme du middleware d'axum.

```bash
cargo run -- make:middleware AuditLog
```

### `make:test <Name>`

Génère un test d'intégration dans `tests/` qui utilise `TestClient` pour
effectuer des requêtes contre votre app.

```bash
cargo run -- make:test post_smoke
```

---

## Utilitaires de base de données

### `db:info`

Affiche à quelle base de données cette build est configurée pour parler,
sans se connecter. Elle imprime la version du framework, les pilotes de
base de données compilés (fonctionnalités Cargo `postgres`/`mysql`),
l'URL de connexion avec le mot de passe masqué, et le backend détecté.
Comme elle n'ouvre jamais de connexion, elle est pratique en CI ou dans
des conteneurs où la base de données n'est pas encore prête mais où vous
voulez confirmer que les réglages sont corrects.

```bash
cargo run -- db:info
```

### `db:dump [--out <path>] [--data-only|--schema-only] [--no-owner]`

Sauvegarde votre base de données en exécutant `pg_dump` contre
`DATABASE_URL` — comme `php artisan db:dump`. Par défaut, le SQL part
sur stdout (pour pouvoir le rediriger dans un pipe) ; passez
`--out <path>` (`-o`) pour écrire un fichier à la place. `--data-only`
et `--schema-only` correspondent directement aux drapeaux de `pg_dump`,
et `--no-owner` supprime les lignes OWNER. Vous devez avoir `pg_dump`
installé et dans votre `PATH`.

```bash
cargo run -- db:dump > backups/before-migrate.sql    # stdout → file
cargo run -- db:dump --out backups/before-migrate.sql
```

### `db:restore <path> [--clean]`

Recharge un fichier de sauvegarde dans votre base de données — l'inverse
de `db:dump`. Elle exécute le fichier via `psql` contre `DATABASE_URL`
avec `ON_ERROR_STOP=1`, donc elle s'arrête à la première erreur. Ajoutez
`--clean` pour effacer le schéma existant au préalable (elle préfixe
`DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;`) afin que
la restauration se fasse sur une base de données vide. Vous devez avoir
`psql` dans votre `PATH`.

```bash
cargo run -- db:restore backups/before-migrate.sql
cargo run -- db:restore backups/before-migrate.sql --clean
```

---

## Commandes système

### `version` / `--version`

Affiche la version du framework **Rustango**.

```bash
$ cargo run -- version
rustango 0.44.0
```

### `about`

Affiche un instantané de votre environnement : version du framework,
modèles et apps enregistrés, si la base de données est accessible, et
les variables d'environnement clés. Utile à joindre aux tickets de
support en cas de problème.

```bash
$ cargo run -- about
rustango
  version:        0.44.0
  models:         3 registered
  apps:           1 (blog)
  RUSTANGO_ENV:   local
  DATABASE_URL:   postgres://***@localhost:5433/myblog
  db_connect:     ok
```

### `check [--deploy]`

Exécute des vérifications de santé sur votre projet — comme le `check`
de Django. Ajoutez `--deploy` pour les vérifications plus strictes de
préparation à la production, de la même façon que fonctionne
`check --deploy` de Django.

**Vérifications toujours actives :**
- ≥ 1 modèle enregistré via `inventory`
- Base de données accessible (`SELECT 1`)
- Nombre de migrations vs nombre de modèles

**Avec `--deploy` :**
- `RUSTANGO_ENV` vaut `prod` ou `production`
- `RUSTANGO_SESSION_SECRET` défini et ≥ 32 octets (la clé HMAC pour les
  cookies + JWT ; `SECRET_KEY` n'est jamais lu par le framework)
- `DATABASE_URL` défini
- `RUSTANGO_APEX_DOMAIN` défini (projets de tenancy)

```bash
$ cargo run -- check --deploy
running rustango system check (deploy mode)...
  [info]    3 models registered via inventory
  [info]    database reachable
  [info]    4 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET length OK
all checks passed
```

Se termine avec un code non nul si une vérification de niveau erreur
échoue. Les avertissements seuls ne provoquent pas d'échec.

### `docs`

Ouvre la documentation de **Rustango** (<https://docs.rs/rustango>) dans
votre navigateur. Elle affiche toujours aussi l'URL, afin de rester
utile sur un serveur sans interface graphique.

```bash
cargo run -- docs
```

### `--help` / `help`

Liste chaque commande avec une description d'une ligne. En mode
tenancy, les commandes multi-tenant listées ci-dessous sont ajoutées
également.

---

## Commandes de tenancy

Ces commandes n'existent que dans les projets multi-tenant (une
application servant de nombreux clients/organisations isolés). Elles
n'apparaissent que lorsque le projet est compilé avec
`features = ["tenancy"]` ET que `Cli::new()` est chaîné avec
`.tenancy()`.

### `init-tenancy`

**Ne fait rien — conservée pour compatibilité.** Le framework ne fournit
plus de migrations d'amorçage écrites à la main. Ses propres tables
(`rustango_orgs`, `rustango_operators`, `rustango_users`,
rôles/permissions, …) sont générées dans `system/migrations/` à partir
des modèles compilés — le flux Django habituel (modèles →
`makemigrations` → `migrate`) — et appliquées par [`migrate`](#migrate) /
[`migrate-registry`](#migrate-registry), qui les génèrent à la demande
si les fichiers sont absents.

```bash
cargo run -- init-tenancy   # does nothing now; kept so old scripts don't break
```

Les anciennes versions écrivaient ici `0001_rustango_*_initial.json` ;
ce flux figé a disparu. **Pour provisionner, exécutez simplement
`cargo run -- migrate`.** Un modèle utilisateur personnalisé
(`.user_model::<AppUser>()`) passe par le même `system/migrations/`
généré — voir
[Modèle utilisateur personnalisé](#custom-user-model-extra-columns-on-rustango_users).

### `migrate-registry`

Applique uniquement les migrations registry — les tables partagées,
inter-tenant. Le registry contient `rustango_orgs` et
`rustango_operators` plus toute table de scope registry que vous
définissez. Les tables tenant ne sont pas touchées.

```bash
cargo run -- migrate-registry
```

### `migrate-tenants`

Applique les migrations tenant à chaque tenant actif, l'un après
l'autre. Chaque tenant utilise sa propre connexion (son propre schéma ou
sa propre base de données), et si un tenant échoue, les autres
continuent tout de même — la commande rapporte le résultat par tenant à
la fin.

```bash
cargo run -- migrate-tenants
```

Pour le cas courant, un simple `migrate` fait déjà registry en premier,
puis tenants — n'utilisez `migrate-tenants` que lorsque vous avez besoin
de cette étape seule.

### `runserver` / `run-server`

Démarre le serveur web multi-tenant — le `runserver` de Django. Dans un
projet de tenancy, c'est équivalent au simple `cargo run` ; la forme
nommée existe pour que des binaires personnalisés qui analysent leurs
propres arguments puissent tout de même la déclencher.

```bash
cargo run                        # implicit
cargo run -- runserver           # explicit
```

### `create-tenant <slug> [options]`

Met en place un nouveau tenant (client/organisation) et applique les
migrations tenant à celui-ci. Le `<slug>` est son identifiant court.
Sûr à réexécuter — l'appeler à nouveau sur un tenant existant ne
duplique rien.

```bash
cargo run -- create-tenant acme --display-name "ACME Corp"
cargo run -- create-tenant beta --mode database --database-url postgres://...
```

| Flag | Description |
|---|---|
| `--display-name <name>` | Libellé lisible affiché dans les barres latérales d'administration |
| `--mode schema \| database` | Mode de stockage (par défaut : schema) |
| `--database-url <url>` | URL de base de données propre au tenant (requise en mode database) |
| `--host-pattern <pattern>` | Remplace le motif d'hôte utilisé par `SubdomainResolver` |
| `--no-migrate` | Ignore l'application des migrations à scope tenant après le provisionnement |

### `drop-tenant <slug> [--confirm <slug>]`

Désactive un tenant en réglant `active = false`. C'est l'option souple
et réversible — les données du tenant restent sur le disque, et
réexécuter `create-tenant` les fait revenir. Quand vous n'êtes pas en
mode interactif (aucun terminal attaché), vous devez passer
`--confirm <slug>` avec le slug retapé pour confirmer.

```bash
cargo run -- drop-tenant acme --confirm acme
```

### `purge-tenant <slug> [--confirm <slug>] [--purge-database]`

**Supprime définitivement un tenant.** Elle supprime le schéma du
tenant et retire sa ligne de `rustango_orgs`, sans possibilité
d'annulation. Quand vous n'êtes pas en mode interactif (aucun terminal
attaché), vous devez passer `--confirm <slug>` avec le slug retapé.
Pour les tenants en mode database, la base de données sous-jacente est
laissée en place sauf si vous passez aussi `--purge-database`.

```bash
cargo run -- purge-tenant acme --confirm acme
cargo run -- purge-tenant beta --confirm beta --purge-database   # database-mode: also DROP DATABASE
```

### `list-tenants`

Liste chaque tenant avec son mode de stockage et son statut
actif/inactif.

```bash
cargo run -- list-tenants
```

### `create-operator <username> --password <pwd>`

Crée un opérateur — un administrateur global qui peut gérer chaque
tenant depuis une console inter-tenant. Les opérateurs vivent dans le
registry partagé, pas dans un tenant en particulier.

```bash
cargo run -- create-operator admin --password letmein
```

### `create-user <tenant> <username> --password <pwd> [--superuser]`

Crée un utilisateur au sein d'un tenant — à peu près le
`createsuperuser` de Django, mais limité à un seul tenant.

```bash
cargo run -- create-user acme alice --password hunter2 --superuser
```

`--superuser` règle `is_superuser = true` pour cet utilisateur au sein
du tenant. Cela en fait un administrateur du tenant (accès en écriture
complet dans l'admin du tenant), mais cela ne lui donne jamais accès à
la console opérateur inter-tenant.

### `create-role <tenant> <name>`

Crée un rôle (un ensemble nommé de permissions, comme un groupe Django)
au sein d'un tenant.

```bash
cargo run -- create-role acme editor
```

### `list-roles <tenant>`

Liste les rôles définis dans un tenant donné.

```bash
cargo run -- list-roles acme
```

### `assign-role <tenant> <username> <role>`

Donne à un utilisateur l'un des rôles du tenant.

```bash
cargo run -- assign-role acme alice editor
```

### `revoke-role <tenant> <username> <role>`

Retire un rôle à un utilisateur — l'inverse d'`assign-role`.

```bash
cargo run -- revoke-role acme alice editor
```

### `grant-perm <tenant> <role-name|username> <codename> [--role]`

Accorde une seule permission. Par défaut, le deuxième argument est un
**nom d'utilisateur**, donc la permission va directement à cet
utilisateur ; ajoutez `--role` pour l'accorder à un rôle à la place.
Les noms de code de permission suivent le format `<app>.<action>_<model>`
de Django (`blog.add_post`, `blog.change_post`, …). La fonctionnalité
`auto_create_permissions` crée automatiquement les quatre noms de code
CRUD standards pour tout modèle marqué `#[rustango(permissions)]`.

```bash
cargo run -- grant-perm acme alice blog.change_post           # grant to user alice
cargo run -- grant-perm acme editor blog.change_post --role   # grant to role editor
```

### `revoke-perm <tenant> <role-name|username> <codename> [--role]`

Retire une permission — l'inverse de `grant-perm`. Cible un utilisateur
par défaut ; ajoutez `--role` pour la retirer à un rôle à la place.

```bash
cargo run -- revoke-perm acme alice blog.change_post
cargo run -- revoke-perm acme editor blog.change_post --role
```

### `create-api-key <tenant> <username> [--label <s>]`

Émet une clé API pour un utilisateur du tenant. Le jeton complet est
affiché **une seule fois** et jamais plus — copiez-le maintenant, car
seuls son préfixe et un hachage sont stockés.

```bash
cargo run -- create-api-key acme alice --label "ci-bot"
```

### `audit-cleanup`

Élague les anciennes entrées du journal d'audit (`rustango_audit_log`)
pour l'empêcher de grossir indéfiniment. Réduisez par âge (`--days`) ou
par nombre (`--keep-last`), et limitez éventuellement à un seul tenant.

```bash
cargo run -- audit-cleanup --days 90                       # delete > 90 days old
cargo run -- audit-cleanup --keep-last 50                  # keep most recent 50 per row
cargo run -- audit-cleanup --keep-last 50 --tenant acme    # scoped
```

---

## Modèle utilisateur personnalisé (colonnes supplémentaires sur `rustango_users`)

C'est la version **Rustango** du « modèle utilisateur personnalisé » de
Django — comment ajouter vos propres champs à la table utilisateur. Le
`User` de tenant intégré possède sept colonnes fixes : `id`, `username`,
`password_hash`, `is_superuser`, `active`, `created_at`, plus une
colonne JSONB `data` (un blob JSON flexible) pour toute métadonnée
supplémentaire par utilisateur. **Pour la plupart des apps, cette
colonne JSONB est tout ce dont vous avez besoin** — pas de migration,
pas de surcharge, pas de surprise.

Quand vous voulez des colonnes **typées, indexables** sur
`rustango_users` à la place, il existe deux approches. Elles ne sont pas
interchangeables ; choisissez celle qui correspond à l'étape de vie de
votre projet.

### Option 1 — Modèle de profil compagnon avec clé étrangère *(fonctionne sur tout projet)*

Idéale quand le projet existe déjà, ou quand vous préférez laisser la
table `User` du framework comme source unique de vérité.

```rust
#[derive(rustango::Model)]
pub struct UserProfile {
    #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>,
    #[rustango(fk = "rustango_users")] pub user_id: i64,
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
```

Exécutez `cargo run -- makemigrations` puis `cargo run -- migrate`, et
vous avez une table d'extras typée liée à l'utilisateur par clé
étrangère. Lisez-la avec l'ORM :

```rust
let profile = UserProfile::objects()
    .where_(UserProfile::user_id.eq(user.id.get().copied().unwrap()))
    .first(&pool).await?;            // Option<UserProfile>
```

Compromis : une ligne supplémentaire et une jointure à chaque accès.
Avantage : zéro risque de casser l'authentification du framework.

### Option 2 — `Cli::user_model::<AppUser>()` *(uniquement pour un projet neuf)*

N'utilisez ceci que sur un projet neuf où vous voulez les champs
supplémentaires directement sur la table `rustango_users` elle-même.
Comme `AppUser` *est* le modèle `rustango_users`, ses colonnes passent
par le moteur ordinaire `makemigrations` → `migrate` : les tables du
framework sont générées dans `system/migrations/`, donc les colonnes
d'`AppUser` se retrouvent dans le `CREATE TABLE rustango_users` généré.

**Étape 1.** Définissez votre modèle. Il doit déclarer exactement chaque
colonne requise par le framework (`id`, `username`, `password_hash`,
`is_superuser`, `active`, `created_at`, `data`), plus vos extras. Chaque
colonne supplémentaire doit soit autoriser `NULL`, soit avoir un
`default = "…"`.

```rust
use rustango::sql::Auto;

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "rustango_users")]
pub struct AppUser {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)] pub username: String,
    #[rustango(max_length = 255)] pub password_hash: String,
    pub is_superuser: bool,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[rustango(default = "'{}'")] pub data: serde_json::Value,
    // extras —
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
impl rustango::tenancy::TenantUserModel for AppUser {}
```

**Étape 2.** Branchez la surcharge dans `main.rs` :

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::manage::Cli::new()
        .api(my_app::urls::router())
        .tenancy()
        .user_model::<AppUser>()
        .run().await
}
```

**Étape 3.** Enregistrez `AppUser` **au lieu du** `User` du framework —
un seul modèle peut revendiquer `table = "rustango_users"`. Le
générateur de squelette ne livre aucun JSON d'amorçage statique
(seulement un `system/migrations/` vide), donc il n'y a rien à
supprimer ; il faut juste ne pas enregistrer aussi le `User` du
framework.

**Étape 4.** Générez + appliquez :

```bash
cargo run -- makemigrations       # generates system/migrations/ with AppUser's columns
cargo run -- migrate              # creates rustango_users with your extras
```

**Mises en garde :**

- Modifier `AppUser` plus tard est un changement de schéma ordinaire :
  réexécutez `makemigrations` pour émettre la migration `AddColumn`,
  puis `migrate`.
- Un seul modèle peut correspondre à `rustango_users`. Enregistrer **à
  la fois** le `User` du framework et votre `AppUser` rend
  `makemigrations` ambigu — enregistrez `AppUser` seul. C'est la
  raison principale pour laquelle l'option 2 est réservée aux projets
  neufs ; sur un projet existant, l'option 1 évite le problème.
- Le code d'authentification et d'administration du framework lit les
  sept colonnes essentielles par leur nom ; vos colonnes
  supplémentaires ne sont accessibles que via
  `AppUser::objects().fetch(...)`.

`Builder::user_model::<AppUser>()` fait la même chose pour le code qui
construit directement le `Builder` du serveur, sans passer par `Cli`.

---

## Sous-commandes personnalisées

Vous pouvez ajouter vos propres commandes — la version **Rustango** des
commandes de gestion personnalisées de Django. L'astuce consiste à
inspecter vous-même les arguments et à traiter votre commande avant de
passer le reste à `Cli::run`. Deux façons de le faire :

**En ligne dans `src/main.rs`** (pas de binaire supplémentaire) :

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("import-csv")) {
        let url = std::env::var("DATABASE_URL")?;
        let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;
        return my_csv_importer::run(&pool, &args[1..]).await;
    }
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

**Via `--with-manage-bin`** (`src/bin/manage.rs` séparé) :

```bash
cargo run -- startapp app --with-manage-bin
```

Puis dans `src/bin/manage.rs` :

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = std::env::var("DATABASE_URL")?;
    let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;

    match args.first().map(String::as_str) {
        Some("import-csv") => my_csv_importer::run(&pool, &args[1..]).await,
        _ => rustango::migrate::manage::run(&pool, "./migrations".as_ref(), args)
            .await
            .map_err(Into::into),
    }
}
```

Exécutez vos propres commandes exactement comme les commandes
intégrées : `cargo run -- import-csv path/to/file.csv` (ou
`cargo run --bin manage -- import-csv …` en utilisant
`--with-manage-bin`).

---

## Flux de travail courants

### Mise en place d'un projet pour la première fois (mono-tenant)

```bash
cargo rustango new myapp
cd myapp
cp .env.example .env             # edit DATABASE_URL
docker compose up -d
cargo run -- migrate
cargo run                        # serve at :8080
```

### Mise en place d'un projet pour la première fois (tenancy)

```bash
cargo rustango new myapp --template tenant
cd myapp
cp .env.example .env             # edit DATABASE_URL + RUSTANGO_APEX_DOMAIN
docker compose up -d
cargo run -- migrate                                      # registry + tenants
cargo run -- create-operator admin --password letmein
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
cargo run                        # serve at :8080
```

### Ajout de tenants une fois l'app déjà en fonctionnement

Une véritable application de tenancy accumule généralement des modèles
et des migrations bien avant l'arrivée de son premier tenant. Ce flux
fonctionne à n'importe quel moment de la vie du projet :

```bash
# 1. (any time) develop user models — define structs with #[derive(Model)],
#    add `pub mod ...;` to src/lib.rs.
# 2. Generate scope-aware migrations. In a tenancy project this writes
#    up to TWO files: one tagged registry-scope (touches Org/Operator),
#    one tagged tenant-scope (touches User + your models). Pre-v0.24.2
#    this used to dump everything into one tenant-scoped file and
#    crash on `create-tenant` — see the changelog.
cargo run -- makemigrations

# 3. Apply migrations. `migrate` is scope-aware: it runs registry-
#    scoped files once against the registry pool first, then fans
#    tenant-scoped files across every active tenant.
cargo run -- migrate

# 4. Provision a NEW tenant whenever (could be days, weeks, many
#    migrations later). The tenancy code applies every accumulated
#    tenant-scoped migration to the new tenant's schema in one pass —
#    the new tenant arrives at the same schema state as existing ones.
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
```

Pourquoi c'est sûr :
- `#[rustango(scope = "registry")]` sur `Org`/`Operator` maintient les
  changements aux tables partagées hors des migrations par tenant.
- `migrate-tenants` visite chaque tenant actif et applique uniquement
  les migrations tenant — les fichiers registry sont ignorés.
- `create-tenant` exécute ce même passage `migrate-tenants` contre le
  schéma du nouveau tenant, qui démarre donc entièrement à jour sans
  correctif manuel.

### Ajouter un modèle

```bash
cargo run -- startapp blog        # if not done yet
# Edit src/blog/models.rs — add #[derive(Model)]
# Add `pub mod blog;` to src/lib.rs
cargo run -- makemigrations
cargo run -- migrate
```

### Ajouter une API JSON pour ce modèle

```bash
cargo run -- make:viewset PostViewSet --model Post
# Edit src/post_view_set.rs — fill in field lists
# Mount in src/urls.rs
cargo run                        # GET /api/posts now works
```

### Ajouter un remplissage rétroactif de données

```bash
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title) WHERE slug IS NULL" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs
cargo run -- migrate
```

### Audit avant déploiement

```bash
cargo run --release -- check --deploy
```

### Annuler la dernière migration

```bash
cargo run -- downgrade 1
```

### Appliquer une migration de tenancy à un scope spécifique

```bash
cargo run -- migrate-registry            # registry-scoped only
cargo run -- migrate-tenants             # tenant-scoped, fan-out across orgs
```

### Décommissionner un tenant

```bash
cargo run -- drop-tenant acme            # soft (reversible)
cargo run -- purge-tenant acme           # hard (drops schema/db)
```

---

## Réglage du pool par tenant (v0.27.7+)

Les tenants en mode database obtiennent leur propre pool de connexions
(un `PgPool` — un ensemble de connexions de base de données réutilisées),
mis en cache par slug dans
[`TenantPools`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/src/tenancy/pools.rs). Par défaut, un
pool est construit **de façon paresseuse, à la première requête du
tenant**, sauf si vous activez le préchauffage. Les réglages se trouvent
sur `TenantPoolsConfig` :

| Champ | Défaut | Objet |
|---|---|---|
| `max_cached_database_pools` | 64 | Plafond du cache de pools. Une fois plein, le prochain tenant non mis en cache échoue (pas d'éviction silencieuse). |
| `database_pool_max_connections` | 4 | `max_connections` par pool. À garder petit pour qu'un éparpillement de tenants n'épuise pas le `max_connections` de PG. |
| `database_pool_min_connections` | 0 | Garde N connexions chaudes en permanence. `≥1` réduit la latence de la première requête en payant l'aller-retour TCP/TLS/auth au démarrage. |
| `database_pool_acquire_timeout` | 30s | Durée d'attente de `pool.acquire()` avant l'erreur `PoolTimedOut`. |
| `database_pool_idle_timeout` | 10 min | Ferme les connexions inactives après cette durée. Protège contre les coupures dues à l'équilibreur de charge / à `idle_in_transaction_session_timeout`. |
| `database_pool_max_lifetime` | 30 min | Force la rotation des connexions pour que les identifiants louvés via vault soient renouvelés. |
| `prewarm_active_tenants` | false | Si vrai, `Server::Builder::serve` appelle `prewarm_database_tenants()` au démarrage. |

### Préchauffage au démarrage

Deux façons de le déclencher :

1. **Automatique** — réglez `prewarm_active_tenants = true` sur le
   `TenantPoolsConfig` que vous passez à
   `TenantPools::new(...).config(...)`. `Server::Builder::serve`
   exécute le préchauffage avant de se lier au port.

2. **Verbe CLI** — `cargo run -- prewarm-pools` construit les pools
   pour chaque tenant actif en mode database et se termine. Utile
   comme hook post-déploiement (par exemple après une rotation
   d'identifiants), ou pour vérifier que chaque tenant est accessible
   avant de basculer un équilibreur de charge.

Le préchauffage parcourt `Org::objects().where(active = true,
storage_mode = "database")` et s'arrête court quand le plafond du
cache est atteint (rapporté comme `skipped_cap` dans le
[`PrewarmReport`]). Les échecs de construction par tenant journalisent
un `tracing::warn!` mais n'interrompent pas la boucle.

### Traçage

`crate::tenancy::pools::tenant_pool_init` est un
`tracing::info_span!` qui enveloppe la construction du pool sur le
chemin froid. Abonnez-vous-y pour voir la latence de construction par
tenant :

```text
INFO crate::tenancy::pools: tenant pool connected (database mode)
     slug=acme elapsed_ms=42 min_conn=1 max_conn=4
```

### Piège de configuration — TLD `.local` sur macOS

Si vous accédez à l'admin du tenant via
`http://acme.local:8080/admin/` sur macOS et constatez une pause de 5
secondes à chaque requête : c'est **Bonjour / mDNS**, pas
**Rustango**. Le résolveur de macOS traite `.local` de façon spéciale
et attend le délai complet de mDNS avant de retomber sur
`/etc/hosts`. Deux corrections :

1. **Utiliser un TLD différent** : `127.0.0.1 acme.localhost`
   fonctionne sans délai. `localhost` est réservé (RFC 6761) et évite
   mDNS.
2. **Exécuter dnsmasq** avec une zone `.local` pointant vers 127.0.0.1
   pour que l'OS obtienne une réponse immédiate.

Confirmez avec `curl -w "%{time_connect}\n"` : si `time_connect`
affiche environ 5s mais tombe à quelques millisecondes avec
`--resolve acme.local:8080:127.0.0.1`, vous êtes bien confronté à mDNS.


---

## Voir aussi

- [Recueil ORM](orm.md)
- [Générateurs de squelette](scaffolding.md)
- [ViewSets](viewsets.md)
- [Serializers](serializers.md)
- [Guide de sécurité](security.md)
