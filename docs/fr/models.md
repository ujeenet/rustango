# Modèles

Un modèle est une struct Rust qui correspond à une table de base de données. Ajoutez `#[derive(Model)]`,
annotez les champs, et **Rustango** génère le schéma, un point d'entrée de requête type-safe,
et les méthodes `save`/`find`/`delete` — les modèles de Django ou l'Eloquent de Laravel,
avec le compilateur qui vérifie vos colonnes. Ceci est la référence de **déclaration** :
chaque type de champ, chaque option de clé primaire, et chaque
attribut `#[rustango(...)]`. Pour *interroger* les modèles une fois déclarés, voir
le [cookbook ORM](orm.md).

[![Models in Rustango: a #[derive(Model)] struct maps Rust field types to per-dialect columns, the primary key can be an auto-increment Auto<i64> or a custom application-assigned key, and the derive generates SCHEMA + objects() + save/find](../img/models.png)](../img/models.png)

> **Nouveau sur un terme ici ?** *modèle*, *clé primaire*, *clé étrangère*, *migration*,
> *nullable* — voir le [glossaire](glossary.md).

> **Source :** `rustango::Model` (`#[derive(Model)]`), `rustango::core`
> (le trait `Model`, `ModelSchema`, `FieldType`, `Auto`, `ForeignKey`), et les
> correspondances de types par dialecte dans `rustango::sql::{dialect, mysql, sqlite}` — toujours
> compilées (choisissez une feature de backend : `postgres` / `mysql` / `sqlite`).
>
> **Version exécutable :** les allers-retours de types de champs, la PK personnalisée, et les
> extraits SCHEMA sont copiés depuis
> [`models_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/models_doc.rs)
> (`cargo test -p rustango --features sqlite --test models_doc`).

## Table des matières

- [Anatomie d'un modèle](#anatomy-of-a-model)
- [Types de champs](#field-types) · [Types spécifiques à PostgreSQL](#postgresql-only-types)
- [Clés primaires](#primary-keys) — [PK personnalisées](#custom-primary-keys) · [composites](#composite-primary-keys)
- [Relations](#relationships)
- [Attributs de champ courants](#common-field-attributes)
- [Index et contraintes](#indexes-and-constraints)
- [Attributs de modèle courants](#common-model-attributes)
- [L'API générée](#the-generated-api) — [save vs insert](#save-vs-insert)
- [Référence complète des attributs](#full-attribute-reference)
- [Voir aussi](#see-also)

---

## Anatomie d'un modèle

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(table = "posts", display = "title")]   // model-level attributes
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,                            // field-level attributes

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(fk = "authors", on = "id")]
    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}
```

À partir de cette seule déclaration, le derive génère :

- le **schéma** (`Post::SCHEMA` — nom de la table, colonnes, types, la PK) que
  les migrations et l'admin lisent ;
- un point d'entrée de requête, **`Post::objects()`**, retournant un `QuerySet<Post>` ;
- des **constantes de champ typées** (`Post::title`, `Post::author_id`) pour des
  filtres vérifiés à la compilation — `Post::objects().where_(Post::author_id.eq(42))` ;
- des méthodes de ligne — **`save`**, **`find`**, **`delete`**, et plus (voir
  [l'API générée](#the-generated-api)).

Le nom de la table prend par défaut le nom du modèle si vous omettez `table` ; les noms de colonnes
prennent par défaut le nom du champ en snake_case sauf si vous définissez `column`.

---

## Types de champs

Le type Rust du champ déterminele type de colonne en base de données. Rustango fait correspondre chaque
type par dialecte, si bien que le même modèle fonctionne sur PostgreSQL, MySQL et SQLite :

| Type Rust | PostgreSQL | MySQL | SQLite |
|---|---|---|---|
| `i16` | `SMALLINT` | `SMALLINT` | `INTEGER` |
| `i32` | `INTEGER` | `INT` | `INTEGER` |
| `i64` | `BIGINT` | `BIGINT` | `INTEGER` |
| `f32` | `REAL` | `FLOAT` | `REAL` |
| `f64` | `DOUBLE PRECISION` | `DOUBLE` | `REAL` |
| `bool` | `BOOLEAN` | `TINYINT(1)` | `INTEGER` (0/1) |
| `String` | `TEXT` | `TEXT` | `TEXT` |
| `String` + `max_length = N` | `VARCHAR(N)` | `VARCHAR(N)` | `TEXT` |
| `chrono::DateTime<Utc>` | `TIMESTAMPTZ` | `DATETIME(6)` | `TEXT` (ISO-8601) |
| `chrono::NaiveDate` | `DATE` | `DATE` | `TEXT` |
| `chrono::NaiveTime` | `TIME` | `TIME(6)` | `TEXT` |
| `uuid::Uuid` | `UUID` | `CHAR(36)` | `TEXT` |
| `serde_json::Value` | `JSONB` | `JSON` | `TEXT` |
| `rust_decimal::Decimal` | `NUMERIC` | `DECIMAL(38,10)` | `NUMERIC` |
| `Vec<u8>` | `BYTEA` | `LONGBLOB` | `BLOB` |
| `Option<T>` | `T NULL` | `T NULL` | `T` (nullable) |

`Option<T>` est le moyen de rendre une colonne **nullable** — un champ non-`Option` est
`NOT NULL`. Tous ces types font l'aller-retour via `save` → `find`, vérifié de bout
en bout :

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "gadget")]
pub struct Gadget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
    pub qty: i64,
    pub active: bool,
    pub note: Option<String>,        // nullable
    pub made_at: DateTime<Utc>,
    pub meta: serde_json::Value,     // JSON
}
```

> **Précision décimale.** Le `NUMERIC` de PostgreSQL est à précision arbitraire ; MySQL utilise
> `DECIMAL(38,10)` (38 chiffres, 10 décimales — l'ajustement portable le plus large) ; SQLite
> utilise l'affinité `NUMERIC`. Utilisez `rust_decimal::Decimal` pour l'argent, jamais `f64`.

### Types spécifiques à PostgreSQL

Ceux-ci correspondent à des types de colonnes natifs de PostgreSQL et n'ont **aucun équivalent
MySQL/SQLite** — le générateur de migrations émet `TEXT` sur ces backends pour rester valide, mais
lire/écrire ces types provoque une erreur à l'exécution sur ces backends. Ne les utilisez que dans
des déploiements PostgreSQL :

| Type Rust | PostgreSQL | Notes |
|---|---|---|
| `Array<T>` | `text[]` / `integer[]` / `bigint[]` | tableaux natifs |
| `Range<T>` | `int4range` / `int8range` / `numrange` / `daterange` / `tstzrange` | types de plage |
| `HStore` | `hstore` | map plate chaîne→chaîne (nécessite l'extension) |
| `Vector` + `#[rustango(vector(dims = N))]` | `vector(N)` | embeddings pgvector |
| `Point` + `#[rustango(geometry(srid = N))]` | `geometry(Point, N)` | PostGIS |

---

## Clés primaires

Chaque modèle a besoin d'une clé primaire. Marquez un champ `#[rustango(primary_key)]` ; si
vous n'en marquez aucun, le schéma recherche une colonne nommée `id`.

La PK **par défaut et la plus courante** est un entier 64 bits auto-incrémenté,
déclaré comme `Auto<i64>` :

```rust
#[rustango(primary_key)]
pub id: Auto<i64>,
```

**Sémantique de `Auto<T>`.** Un champ `Auto<T>` est soit `Unset` (la valeur que la base de données
assignera) soit `Set(v)`. À l'insertion, une PK `Unset` est omise de la liste des colonnes
pour que la base de données la génère, puis la valeur est relue (`RETURNING` sur
PostgreSQL/SQLite, `LAST_INSERT_ID()` sur MySQL) et stockée sur votre struct :

```rust
let mut g = Gadget { id: Auto::default(), /* … */ };   // Unset
g.save_pool(&pool).await?;                              // DB assigns the id
let new_id = g.id.get().copied().unwrap();             // now populated
```

Les types internes de `Auto<T>` pris en charge sont `i32`, `i64` et `Uuid`.

### Clés primaires personnalisées

La PK n'a pas besoin d'être un entier auto-incrémenté. Tout type qui correspond à une
colonne peut être la PK ; vous assignez la valeur vous-même :

| Déclaration de PK | Type de colonne | Qui l'assigne |
|---|---|---|
| `Auto<i64>` *(par défaut)* | `BIGSERIAL` / `BIGINT AUTO_INCREMENT` / `INTEGER … AUTOINCREMENT` | la base de données |
| `Auto<i32>` | `SERIAL` / `INT AUTO_INCREMENT` / `INTEGER …` | la base de données |
| `Auto<Uuid>` + `auto_uuid` | `UUID` | côté Rust (`uuid v4`) |
| `Auto<Uuid>` + `default_uuid_v7` | `UUID` | côté Rust (`uuid v7` triable) |
| `String` + `primary_key` | `VARCHAR(N)` / `TEXT` | vous (l'application) |
| `Uuid` + `primary_key` | `UUID` / `CHAR(36)` / `TEXT` | vous (l'application) |
| `i64` / `i32` + `primary_key` | `BIGINT` / `INTEGER` | vous (l'application) |

Une **clé de chaîne naturelle** (par ex. un code de coupon) — notez que vous fournissez la valeur et
insérez avec [`insert_pool`](#save-vs-insert), puisqu'il n'y a pas d'`Auto::Unset` pour que
`save` puisse le détecter :

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "coupon")]
pub struct Coupon {
    #[rustango(primary_key, max_length = 32)]
    pub code: String,        // you assign this
    pub discount: i64,
}

let c = Coupon { code: "SAVE10".into(), discount: 10 };
c.insert_pool(&pool).await?;                       // explicit INSERT
let back = Coupon::find_or_fail("SAVE10".to_string(), &pool).await?;   // look up by the string PK
```

**Renommer la colonne de la PK** avec `column` (le champ Rust reste `number`, la colonne
SQL est `account_no`) :

```rust
#[rustango(primary_key, column = "account_no")]
pub number: i64,
```

```rust
// Introspect it via the schema:
let pk = Account::SCHEMA.primary_key().unwrap();
assert_eq!(pk.name, "number");        // Rust field
assert_eq!(pk.column, "account_no");  // SQL column
```

**Les clés primaires UUID** génèrent la valeur côté Rust : `auto_uuid` donne un v4
aléatoire, `default_uuid_v7` un v7 triable dans le temps (meilleure localité d'index) :

```rust
#[rustango(primary_key, auto_uuid)]
pub id: Auto<uuid::Uuid>,
```

### Clés primaires composites

Les clés primaires multi-colonnes natives ne sont **pas prises en charge** — exactement un champ peut être
`primary_key`. Le motif fourni est une PK `Auto<i64>` substitutive plus une
contrainte d'unicité composite déclarée, ce qui est équivalent à un index :

```rust
#[derive(Model)]
#[rustango(table = "line_item", unique_together = "invoice_id, line_no")]
pub struct LineItem {
    #[rustango(primary_key)]
    pub id: Auto<i64>,        // surrogate PK
    pub invoice_id: i64,
    pub line_no: i32,         // (invoice_id, line_no) is unique together
}
```

Recherchez les lignes avec `.where_(LineItem::invoice_id.eq(..)).where_(LineItem::line_no.eq(..))`.

---

## Relations

Une clé étrangère est une colonne `_id` plus un accesseur typé optionnel :

```rust
// Plain FK column — store the parent's id:
#[rustango(fk = "authors", on = "id")]
pub author_id: i64,

// Typed FK — lazy-loads the parent on demand:
pub author: ForeignKey<Author>,
```

`ForeignKey<T>` prend par défaut `i64` comme type de clé ; si la PK du parent est d'un
type différent, précisez-le : `ForeignKey<User, String>`. Le un-à-un utilise
`#[rustango(o2o)]` ; le plusieurs-à-plusieurs est une table séparée — voir
[cookbook ORM → Plusieurs-à-plusieurs](orm.md#many-to-many). Chargez les lignes liées de manière anticipée avec
`select_related` (également dans le guide ORM).

---

## Attributs de champ courants

`#[rustango(...)]` sur un champ. Ceux que vous utiliserez constamment :

| Attribut | Exemple | Effet |
|---|---|---|
| `primary_key` | `#[rustango(primary_key)]` | marque la PK |
| `max_length = N` | `#[rustango(max_length = 200)]` | `VARCHAR(N)` + vérification de longueur à l'écriture |
| `default = "…"` | `#[rustango(default = "'draft'")]` | valeur par défaut de la colonne en base (littéral SQL) |
| `unique` | `#[rustango(unique)]` | contrainte d'unicité sur la colonne |
| `choices = "…"` | `#[rustango(choices = "draft:Draft, published:Published")]` | valeurs énumérées (`value:Label`) ; `<select>` de l'admin + validation |
| `auto_now_add` | `#[rustango(auto_now_add)]` | réglé à l'heure actuelle à l'**insertion** (sur un `Auto<DateTime<Utc>>`) |
| `auto_now` | `#[rustango(auto_now)]` | réglé à l'heure actuelle à **chaque save** |
| `column = "…"` | `#[rustango(column = "account_no")]` | renomme la colonne SQL |
| `null` / `Option<T>` | `pub note: Option<String>` | colonne nullable |
| `min` / `max` | `#[rustango(min = 0, max = 100)]` | validation de plage à l'écriture |
| `blank` / `editable` | `#[rustango(editable = false)]` | comportement formulaire/admin |
| `db_comment = "…"` | `#[rustango(db_comment = "cents")]` | COMMENT de colonne |

`choices`, `default`, `auto_now_add`, et la suppression logique ensemble (tous vérifiés) :

```rust
#[rustango(max_length = 20, default = "'draft'", choices = "draft:Draft, published:Published")]
pub status: String,
#[rustango(auto_now_add)]
pub created_at: Auto<DateTime<Utc>>,
#[rustango(soft_delete)]
pub deleted_at: Option<DateTime<Utc>>,
```

---

## Index et contraintes

Déclarés sur le **modèle** :

```rust
#[rustango(
    table = "posts",
    index("status, published_at"),                 // composite btree index
    unique_together = "author_id, slug",           // multi-column unique
    check(name = "qty_nonneg", expr = "qty >= 0"), // CHECK constraint
)]
```

- **`index(...)`** — un index btree par défaut ; choisissez une méthode pour PostgreSQL
  avec `index(columns = "body", method = "gin")` (aussi `gist`, `brin`, `hash`,
  `bloom`, `spgist`).
- **`unique_together` / `index_together`** — unicité / index non unique multi-colonnes.
- **Index partiels** — `unique_when(...)` / `index_when(...)` ajoutent une condition
  `WHERE`.
- **`check(name, expr)`** — une contrainte CHECK ; **`exclude(...)`** est une
  contrainte EXCLUDE de PostgreSQL.

---

## Attributs de modèle courants

`#[rustango(...)]` sur la struct :

| Attribut | Exemple | Effet |
|---|---|---|
| `table = "…"` | `table = "posts"` | nom de la table (par défaut le nom de la struct) |
| `display = "…"` | `display = "title"` | le champ affiché lorsqu'une ligne est référencée (libellés de FK, admin) |
| `app = "…"` | `app = "blog"` | regroupe le modèle sous une app |
| `default_order = "…"` | `default_order = "-created_at"` | tri par défaut pour les requêtes |
| `default_permissions` | `default_permissions = "add, change"` | quelles auto-permissions créer |
| `soft_delete` *(champ)* | `#[rustango(soft_delete)] deleted_at: Option<…>` | active la suppression logique (marquer, pas supprimer) |
| `audit(track = "…")` | `audit(track = "title, status")` | enregistre l'historique des modifications par ligne |
| `scope = "…"` | `scope = "tenant"` | portée multi-tenant (registre vs tenant) |
| `admin(...)` | `admin(list_display = "…")` | configuration de l'UI admin — voir [l'admin](admin.md) |

---

## L'API générée

`#[derive(Model)]` implémente le trait `Model` (`Post::SCHEMA`) et génère :

- **`Post::objects()`** (alias `Post::query()`) → un `QuerySet<Post>` pour filtrer,
  ordonner et récupérer (le [cookbook ORM](orm.md) couvre l'API de requête).
- **Des constantes de champ typées** — `Post::title`, `Post::author_id` — utilisées dans
  `.where_(Post::author_id.eq(42))` pour des filtres vérifiés à la compilation.
- **Des chercheurs (finders)** — `find(pk, &pool)` → `Option<Self>` ; `find_or_fail(pk, &pool)` →
  `Self` (erreur si absent) ; `find_many(pks, &pool)` ; `find_or_insert(...)`.
- **Des écrivains (writers)** — `save`/`save_pool`, `save_partial(&["title"], &pool)` (met à jour
  seulement certaines colonnes), `insert_pool` (insertion explicite), `delete`.
- **La suppression logique** (si activée) — `soft_delete`, `restore`, `force_delete` ;
  `QuerySet::active()` / `with_trashed()` / `only_trashed()`.

### save vs insert

Ceci embrouille souvent les gens, il vaut donc la peine de le dire clairement :

| Méthode | Comportement |
|---|---|
| `save_pool(&mut self, &pool)` | **INSERT** si la PK `Auto` est `Unset`, sinon **UPDATE** |
| `insert_pool(&self, &pool)` | toujours **INSERT** |

Pour la PK `Auto<i64>` par défaut, `save_pool` fait ce qu'il faut automatiquement.
Pour une **PK assignée par l'application** (un `String`/`Uuid` que vous définissez vous-même), il n'y a
pas d'état `Unset` — `save_pool` ferait donc un UPDATE sur une ligne (peut-être inexistante).
Utilisez **`insert_pool`** pour insérer une ligne toute neuve avec une PK personnalisée (vérifié dans le
test associé).

---

## Référence complète des attributs

Chaque clé `#[rustango(...)]` acceptée par le derive. Les plus courantes sont couvertes
ci-dessus ; voici la liste complète, y compris les avancées/spécifiques à PostgreSQL.

### Au niveau du modèle (sur la struct)

| Attribut | Valeur | Effet |
|---|---|---|
| `table` | `"name"` | nom de la table |
| `display` | `"field"` | libellé humain pour une ligne |
| `app` | `"name"` | regroupement par app |
| `default_order` | `"-field"` | tri par défaut |
| `default_permissions` | `"add, change, delete, view"` | auto-permissions à créer |
| `default_related_name` | `"posts"` | nom de l'accesseur inverse sur le parent |
| `base_manager_name` | `"all_objects"` | nom du manager de base (non filtré) |
| `manager(ext = "Trait")` | chemin de trait | génère un trait d'extension de manager personnalisé |
| `manager_fn` | `"published"` | ajoute un accesseur de manager en plus de `objects()` |
| `get_latest_by` | `"created_at"` | colonne par défaut pour `latest()`/`earliest()` |
| `order_with_respect_to` | `"parent"` | ordre relatif au parent, à la manière de Django |
| `index(...)` | `columns`, `method`, `name` | index secondaire (btree/gin/gist/brin/hash/bloom/spgist) |
| `unique_together` | `"a, b"` | contrainte d'unicité composite |
| `index_together` | `"a, b"` | index composite non unique |
| `unique_when(...)` / `index_when(...)` | colonnes + `condition` | index partiel (conditionnel) |
| `check(...)` | `name`, `expr` | contrainte CHECK |
| `exclude(...)` | spécification d'opérateur | contrainte EXCLUDE de PostgreSQL |
| `audit(track = "…")` | liste de champs | historique des modifications par ligne |
| `scope` | `"tenant"` / `"registry"` | portée multi-tenant |
| `proxy` | indicateur | modèle proxy (partage la table d'un autre) |
| `global_scope(name, apply = fn)` | nom + fonction | filtre appliqué automatiquement à toutes les requêtes |
| `through(...)` | spécification de relation | accesseur de relation « through » personnalisé |
| `reverse_has(...)` / `generic_has(...)` | spécification de relation | accesseur has-many inverse / FK générique inverse |
| `required_db_features` / `required_db_vendor` | liste / fournisseur | contraintes de validation de déploiement |
| `db_table_comment` | `"…"` | COMMENT de table |
| `admin(...)` | options admin | configuration de l'UI admin (voir [admin.md](admin.md)) |

### Au niveau du champ (sur un champ)

| Attribut | Valeur | Effet |
|---|---|---|
| `primary_key` | indicateur | marque la PK |
| `column` | `"name"` | renomme la colonne SQL |
| `max_length` | `N` | `VARCHAR(N)` + validation de longueur |
| `default` | `"sql literal"` | DEFAULT de colonne |
| `null` | indicateur | nullable (ou utilisez `Option<T>`) |
| `unique` | indicateur | contrainte d'unicité |
| `choices` | `"v:Label, …"` | valeurs énumérées |
| `min` / `max` | nombre | validation de plage |
| `blank` | indicateur | autorise le vide dans les formulaires/admin |
| `editable` | `true`/`false` | éditabilité formulaire/admin |
| `auto_now` | indicateur | réglé à l'heure actuelle à chaque save |
| `auto_now_add` | indicateur | réglé à l'heure actuelle à l'insertion |
| `auto_uuid` | indicateur | UUID v4 côté Rust (sur `Auto<Uuid>`) |
| `default_uuid_v7` | indicateur | UUID v7 triable côté Rust |
| `fk` + `on` | `"table"`, `"col"` | colonne de clé étrangère |
| `cascade` | indicateur | `ON DELETE CASCADE` |
| `o2o` | indicateur | relation un-à-un |
| `fk_composite(...)` / `generic_fk(...)` | spécification | FK composite / FK générique (content-type) |
| `generated_as` | `"expr"` | colonne calculée (générée) par la base de données |
| `citext` | indicateur | texte insensible à la casse (CITEXT PostgreSQL) |
| `vector(dims = N)` | `N` | dimension pgvector |
| `geometry(srid = N)` | `N` | identifiant de référence spatiale PostGIS |
| `db_comment` | `"…"` | COMMENT de colonne |

---

## Voir aussi

- [Cookbook ORM](orm.md) — requêtes, filtres, agrégations, jointures, transactions
  (que faire d'un modèle une fois qu'il est déclaré).
- [Sérialiseurs](serializers.md) — transformer un modèle en JSON pour une API.
- [L'admin](admin.md) — le bloc `admin(...)` et l'UI générée.
- [Scaffolding](scaffolding.md) · [CLI `manage`](manage.md) — générer un modèle
  et sa migration.
