# Migrations et le moteur de migration

**Rustango** livre un moteur de migration façon Django : vous éditez vos modèles,
lancez `makemigrations` pour générer un fichier JSON versionné décrivant le
changement de schéma, et `migrate` pour l'appliquer. Depuis la **0.48**, le framework
migre même **ses propres** tables `rustango_*` via le même moteur —
sans DDL de bootstrap livré à la main. Cette page explique les rouages qui
rendent une mise à niveau sûre : les deux chaînes de migration, la réconciliation de squash,
et le fake-initial gardé qui permet à une base de données préexistante d'adopter le
moteur sans collisions.

> **Nouveau aux migrations ?** Les verbes CLI du quotidien — `makemigrations`,
> `migrate`, `migrate --squash`, `migrate --fake`, `downgrade`,
> `showmigrations` — sont couverts commande par commande dans le
> [guide manage](manage.md#migrations). Cette page est le modèle
> conceptuel qui les sous-tend.

> **Source :** `rustango::migrate` (`runner`, `make`, `file`, `manage`)
> et `rustango::tenancy::migrate` — la réconciliation du runner vit dans
> `migrate::runner::reconcile`.

---

## Deux chaînes de migration

Une migration appartient à l'une de deux chaînes indépendantes, chacune avec sa propre
table de registre (la ligne des noms appliqués que le runner consulte pour sauter du travail) :

| Chaîne | Ce qu'elle gère | Table de registre |
|---|---|---|
| **Project** | vos tables `#[derive(Model)]`, dans `migrations/` | `__rustango_migrations__` |
| **System** | les propres tables `rustango_*` du framework (`Org`, `User`, rôles/permissions, agents, media, …), dans `system/migrations/` | `__rustango_system_migrations__` |

La **chaîne système** est ce qui rend le schéma du framework auto-descriptif.
Ses fichiers sont générés depuis les modèles compilés du framework — et ils sont
**conscients de `#[cfg(feature = …)]`** : une colonne ou une table protégée par feature est
retirée par le compilateur lorsque la feature est désactivée, si bien qu'activer une feature fait
émettre à `makemigrations` un `AddColumn` / `CreateTable` et la désactiver
émet un `DropColumn` / `DropTable`. Les projets tenant échafaudés livrent un
`system/migrations/` **vide** ; le premier `cargo run -- migrate`
le génère et l'applique (voir [scaffolding](scaffolding.md)).

`migrate` applique la chaîne système **avant** les migrations de votre projet.
En mode tenancy, les deux périmètres se chevauchent délibérément sur les tables partagées
du framework, si bien que c'est la chaîne à périmètre tenant qui s'exécute ; les
tables du seul registre (`rustango_orgs`, `rustango_operators`) ne signifient rien
sans tenancy. Les applications sans tenancy qui utilisent un sous-système du framework (par ex.
`media`) reçoivent la chaîne système appliquée elles aussi.

---

## Réconciliation de squash — `Migration.replaces`

Un **squash** effondre une série de migrations historiques en un seul fichier fraîchement
généré qui recrée le même état final — pratique quand une pile de
migrations à moitié terminées est plus facile à régénérer qu'à corriger. Le hic :
les `CREATE TABLE` du fichier entreraient en collision sur toute base de données ayant déjà
appliqué les migrations qu'il a effondrées (le checkout d'un collègue, la staging,
la CI).

`migrate --squash` résout cela en estampillant la liste **`replaces`** du nouveau fichier
avec les noms qu'il a effondrés :

```jsonc
{
  "name": "0007_squashed_0001_0006",
  "replaces": ["0001_initial", "0002_add_status", "0003_add_slug", "…"],
  "forward": [ /* recreates the end state */ ]
}
```

Avec `replaces` défini, le runner **réconcilie** le squash face à l'état réel
de la base de données au lieu de l'exécuter aveuglément. La décision est
automatique et dépend entièrement de ce qui est déjà présent :

| État de la base de données | Ce que fait le runner |
|---|---|
| vierge — pas d'historique, pas de tables | exécute le squash pour de vrai |
| chaque migration remplacée est dans le registre | l'enregistre, met en tombstone les prédécesseurs, **pas de DDL** |
| les tables existent mais le registre n'a pas d'historique | l'enregistre, **pas de DDL** (le `--fake-initial` inter-registres de Django) |
| seulement *certaines* lignes / tables remplacées présentes | **refusé** — nomme ce qui manque, vous dit de résoudre à la main |

Le cas **partiel** est une erreur bloquante à dessein : aucun choix automatique n'y est
sûr, donc le runner s'arrête et rapporte ce qu'il a trouvé plutôt que de
deviner. Résolvez-le avec `migrate --fake` (ci-dessous).

Les migrations supplantées par un squash appliqué comptent comme appliquées, si bien que vous pouvez
laisser les fichiers effondrés sur le disque pendant une release ou deux — les déploiements qui
ne les ont jamais exécutés migrent quand même correctement vers l'avant. Les migrations ordinaires (hors squash)
ne sont pas affectées : une migration simple dont la table existe déjà
échoue quand même bruyamment, car c'est un vrai conflit, pas un
historique connu-équivalent.

---

## La réconciliation fake-initial gardée

C'est le mécanisme qui permet à une base de données **existante** d'adopter la chaîne
système en toute transparence. Avant la 0.51, le framework construisait certaines de ses tables via
un DDL brut paresseux `ensure_table` ; ces tables existent mais ne sont pas enregistrées dans
le registre `__rustango_system_migrations__`, si bien qu'un nouveau `CREATE TABLE`
issu de la nouvelle migration système entrerait en collision (`relation "rustango_media"
already exists`, MySQL 1050, …).

La chaîne système réconcilie cela elle-même. Une migration **système** en attente est
inspectée : les opérations qui composent *la création de ses tables* —
`CreateTable`, plus `CreateIndex` / `CreateM2MTable` ciblant une table que la
même migration crée — constituent l'ensemble accepté. Si **toutes** les tables qu'elle
crée existent déjà, la migration est **enregistrée dans le registre
sans exécuter aucun DDL**, et les données existantes sont laissées intactes. Si seulement
*certaines* de ses tables existent, la chaîne crée uniquement celles qui manquent
(sémantique `CREATE TABLE IF NOT EXISTS`) et laisse le reste tranquille.

Le garde est délibérément étroit :

- **Restreint à la propre chaîne système du framework.** Les migrations utilisateur utilisent le
  runner simple et ne font jamais d'auto-fake — le faking par existence de table est réservé,
  par choix, au seul chemin système.
- **Tout ce qui n'est pas de la création de table disqualifie le faking** — un index
  sur une table préexistante, un alter / drop / opération de données / callback bascule
  vers une exécution réelle, si bien que du vrai travail n'est jamais sauté.
- **L'existence n'est interrogée que dans l'espace de noms courant** — Postgres
  `current_schema()`, MySQL `DATABASE()`, SQLite `sqlite_master` — et non
  via le `search_path`, si bien qu'en multi-tenancy en mode schéma, une table de même nom
  dans `public` ne peut pas tromper un tenant en lui faisant sauter ses propres tables.

L'état partiel d'un squash est toujours refusé (voir ci-dessus) ; seule la
chaîne système du framework effectue la réparation « créer celles qui manquent » au coup par coup.

---

## Réparer un dérive à la main — `migrate --fake`

Lorsque la base de données est déjà dans l'état cible mais que le registre ne le
sait pas (une BDD montée hors-bande, un registre supprimé, une migration partiellement
réussie, un squash partiel refusé), estampillez une migration comme appliquée
**sans exécuter son SQL** :

```bash
cargo run -- migrate --fake 0004_add_indexes
cargo run -- migrate --fake 0001_rustango_registry_initial --system       # framework's own chain
cargo run -- migrate --fake 0001_rustango_registry_initial --all-tenants  # every active tenant
```

- `--system` estampille la chaîne système du framework
  (`system/migrations/` → `__rustango_system_migrations__`) au lieu de
  celle de votre projet.
- `--all-tenants` diffuse l'estampille sur chaque tenant actif, rapportant
  chacun et poursuivant au-delà des échecs — les tables du framework vivent par
  tenant, donc les réparer est un travail par tenant. Combinez avec `--system`
  pour les tables du framework sur tous les tenants.

Le nom est validé contre le répertoire de migrations d'abord, si bien qu'une faute de frappe
ne peut pas faire atterrir une ligne bidon ; l'estampillage est idempotent, et le flag peut être
répété pour réparer une série de lignes en une seule commande.

---

## Mise à niveau vers 0.51.2

> **Les 0.51.0 et 0.51.1 ont été retirées (yanked)** — la réconciliation qu'elles promettaient ne s'est jamais
> réellement déclenchée face à de vraies bases de données 0.46–0.50 (la 0.51.0 a déplacé les tables
> media sur les migrations système et est entrée en collision ; le garde de la 0.51.1 exigeait qu'une
> migration soit *purement* `CreateTable`, ce qu'aucune migration générée n'est).
> **Mettez à niveau directement vers la 0.51.2**, qui corrige les deux.

Pour un déploiement existant, la mise à niveau est un simple déploiement — pas de
reprovisionnement, pas de DDL manuel :

```bash
cargo run -- migrate
```

Le fake-initial gardé gère les tables du framework préexistantes : le
premier `migrate` enregistre la migration système dont les tables existent déjà
dans le registre sans y toucher, crée uniquement ce qui manque réellement,
et laisse vos données tranquilles. Si une base de données est dans un état partiel véritablement
incohérent, le runner s'arrête et vous dit ce qu'il a trouvé ;
résolvez-le avec `migrate --fake` plutôt que de forcer.

---

## Voir aussi

- [`manage` guide](manage.md#migrations) — chaque verbe CLI de migration, avec
  des exemples.
- [Scaffolding](scaffolding.md) — d'où viennent `migrations/` et
  `system/migrations/`.
- [Models](models.md) — le derive à partir duquel les migrations sont générées.
