# Cookbook de l'ORM

Des patterns pour l'ORM de **Rustango** au-delà des bases. Si vous venez de l'ORM de Django, d'Eloquent (Laravel) ou d'ActiveRecord (Rails), les formes présentées ici vous sembleront familières. La plupart des exemples supposent que vous disposez déjà d'un modèle `Post` issu de `Getting Started`.

[![Type-checked ORM queries: chained filters, ordering, limits, and aggregation — all without raw SQL](img/orm.png)](img/orm.png)

> **Source :** `rustango::sql` (`QuerySet`, la macro `Q!` / le builder `Qb`) et l'API de requête
> `#[derive(Model)]` — toujours compilée ; choisissez une feature de backend
> (`postgres` / `mysql` / `sqlite`).
>
> **Version exécutable :** les patterns présentés ici s'exécutent dans l'exemple testé
> [`orm_cookbook`](../crates/rustango/examples/orm_cookbook).
>
> **Nouveau sur un terme ici ?** Le [glossaire](glossary.md) définit *modèle*, *queryset*,
> *pool* et *migration* en langage simple.

Quelques termes Rust reviennent tout au long du texte. `&pool` est une référence partagée vers le pool de connexions à la base de données ; vous la passez aux méthodes qui exécutent réellement du SQL. `.await` exécute un appel asynchrone et attend le résultat. `Option<T>` est une valeur qui peut être présente (`Some`) ou absente (`None`) — le null de Rust. `Result` représente succès-ou-erreur ; le `?` final sur un appel retourne immédiatement en cas d'erreur. `Auto<i64>` est une clé primaire auto-incrémentée qui est soit `Set` (chargée depuis la base) soit `Unset` (pas encore insérée).

## Nouveautés (v0.41 / v0.42)

Les dernières versions ont ajouté un lot de fonctionnalités à parité avec Django qui ne sont pas encore intégrées dans chaque section ci-dessous. Repères rapides :

- **Macro `Q!` + builder `Qb` au runtime** (#269, #263) — filtres de forme Django, sûrs à la compilation. `User::objects().where_(Q!(User.email__icontains = "alice"))` échoue à la compilation en cas de faute de frappe sur un nom de champ. Variante composable au runtime pour les puces de filtre de l'admin : `let q = Qb::eq("active", true) & Qb::gt("age", 18i64);`.
- **`.distinct_on(&["author_id"])`** (#264) — natif PG ; repli portable par fonction fenêtrée sur MySQL / SQLite. Patterns « dernier par groupe ».
- **`bulk_upsert_pool(rows, unique_fields, update_fields, &pool)`** (#267) — équivalent de `bulk_create(update_conflicts=True)` de Django. ON CONFLICT / ON DUPLICATE KEY UPDATE tri-dialecte.
- **`explain_pool()`** (#272) — EXPLAIN tri-dialecte. PG `EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS)` / MySQL `EXPLAIN ANALYZE` / SQLite `EXPLAIN QUERY PLAN`.
- **Bibliothèque de fonctions DB** (#266) — `Cast`, `LPad`, `RPad`, `MD5`, `SHA1`, `SHA256`, `Position`, `Repeat`, `Reverse`, `Sign`, `Mod`, `Power`, `Sqrt`. Émission par dialecte avec des erreurs claires là où SQLite ne dispose pas de la fonction.
- **Types de champs** — `rust_decimal::Decimal` (natif PG/MySQL, via un shim Decode sur SQLite), `chrono::NaiveTime`, `Vec<u8>` (`FieldType::Binary`) désormais acceptés par `#[derive(Model)]` (#524, v0.42).
- **`ModelForm::prepare_save()` / `PreparedSave`** (#375, v0.42) — équivalent de `save(commit=False)` de Django. Validez maintenant, modifiez l'ensemble d'écriture préparé, validez (commit) quand vous êtes prêt.
- **`#[rustango(unique_when(columns = "...", condition = "..."))]`** (#265) — contraintes d'unicité partielles. « Email unique par ligne non supprimée » / « Slug unique par tenant ».
- **`#[rustango(manager(ext = "FooManagerExt"))]`** (#271) — trait d'extension de manager personnalisé de forme Django, émis à côté du modèle. (C'est aussi la forme Rust des modèles proxy de Django — même table physique, plusieurs « personnalités » via des méthodes par trait. Voir `inheritance.rs:98-127`.)
- **`manage makemigrations --merge`** (#346, v0.42) — nœud de fusion de forme Django pour les chaînes de branches divergentes. Voir [`docs/manage.md`](manage.md#makemigrations---merge).

Le CHANGELOG contient l'index complet des tickets pour chaque version.

## Table des matières

- [Requêtage](#querying)
- [Valeurs calculées & fonctions de base de données](#computed-values--database-functions)
- [Agrégations](#aggregations)
- [Jointures & préchargement des lignes liées](#joins--preloading-related-rows)
- [Opérations en masse](#bulk-operations)
- [Insertion ou mise à jour (upsert)](#insert-or-update-upsert)
- [Transactions](#transactions)
- [Relations plusieurs-à-plusieurs](#many-to-many)
- [JSON / JSONB](#json--jsonb)
- [Suppression logique](#soft-delete)
- [Piste d'audit](#audit-trail)
- [Échappatoire SQL brut](#raw-sql-escape-hatch)
- [Chargement paresseux des FK](#lazy-fk-loading)
- [Quatre façons de filtrer](#four-ways-to-filter)
- [Requêtes cloisonnées par tenant](#tenant-scoped-queries)
- [Signaux](#signals)
- [Conseils de performance](#performance-tips)

---

## Requêtage

Lire des lignes depuis la base de données. `Post::objects()` démarre une requête (comme `Post.objects` de Django) ; vous enchaînez des filtres et un tri, puis appelez `.fetch(&pool).await?` pour l'exécuter et récupérer un `Vec<Post>`. `.where_(...)` ajoute une condition jointe par un AND.

```rust
use rustango::core::Column as _;
use rustango::core::{Op, SqlValue, WhereExpr};   // for filter_op / where_raw below
use rustango::sql::FetcherPool as _;

// Simplest — fetch all
let posts = Post::objects().fetch(&pool).await?;

// Single equality filter
let drafts = Post::objects()
    .where_(Post::status.eq("draft"))
    .fetch(&pool).await?;

// Chained filters (AND)
let recent_drafts = Post::objects()
    .where_(Post::status.eq("draft"))
    .where_(Post::author_id.eq(42))
    .where_(Post::deleted_at.is_null())
    .order_by(&[("created_at", true)])        // true = DESC
    .limit(20)
    .fetch(&pool).await?;

// String-keyed filter (validated at compile of the queryset)
let by_id = Post::objects()
    .filter_op("id", Op::Eq, SqlValue::I64(42))
    .fetch(&pool).await?;

// OR / nested
let qs = Post::objects().where_raw(WhereExpr::Or(vec![
    Post::status.eq("draft").into(),
    Post::status.eq("review").into(),
]));

// XOR — Django 4.1+ `Q(a) ^ Q(b)`. Matches rows where an odd number
// of operands evaluate to true (binary case = "exactly one is true").
// Issue #27.
let either_but_not_both = Post::objects()
    .where_(Post::status.eq("draft").xor(Post::author_id.eq(42)))
    .fetch(&pool).await?;
// Tri-dialect emission: native logical XOR exists on MySQL but not PG
// or SQLite, so the writer emits a portable rewrite uniformly —
// `(a AND NOT b) OR (NOT a AND b)` for the binary form, or a
// CASE-WHEN-1/0 tally `% 2 = 1` for N-ary chains.
```

### Filtres de comparaison

Les méthodes de filtre courantes, une par opérateur SQL. Ce sont les lookups de champs de Django (`__gt`, `__in`, `__icontains`, etc.) sous une forme typée.

```rust
Post::objects().where_(Post::view_count.gt(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.gte(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.lt(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.lte(100)).fetch(&pool).await?;
Post::objects().where_(Post::status.ne("archived")).fetch(&pool).await?;
Post::objects().where_(Post::id.is_in([1, 2, 3])).fetch(&pool).await?;
Post::objects().where_(Post::status.not_in(["draft", "deleted"])).fetch(&pool).await?;
Post::objects().where_(Post::title.like("%draft%")).fetch(&pool).await?;          // case-sensitive contains
Post::objects().where_(Post::title.ilike("%draft%")).fetch(&pool).await?;         // case-insensitive contains
Post::objects().where_(Post::title.ilike("Hello%")).fetch(&pool).await?;          // case-insensitive starts-with
Post::objects().where_(Post::deleted_at.is_null()).fetch(&pool).await?;
Post::objects().where_(Post::published_at.between(start, end)).fetch(&pool).await?;
```

### Tri des résultats

Triez les lignes par une ou plusieurs colonnes, par une expression, ou avec un contrôle explicite de l'emplacement des NULL. Au-delà du `.order_by(&[("col", desc)])` basique, vous disposez de trois dimensions supplémentaires :

```rust
use rustango::core::funcs::lower;
use rustango::core::{F, NullsOrder};

// 1. Plain field + ASC/DESC (back-compat — implicit NULLS handling
//    differs between dialects; see the dialect note below).
Post::objects()
    .order_by(&[("published_at", true), ("id", false)])
    .fetch(&pool).await?;

// 2. Explicit NULLS FIRST/LAST control — portable across PG, MySQL,
//    and SQLite. MySQL has no native `NULLS …` keyword; the writer
//    emulates with an `<col> IS NULL` pre-sort term so the on-wire
//    ordering matches PG/SQLite.
Post::objects()
    .order_by_with_nulls(&[("score", true, NullsOrder::Last)])
    .fetch(&pool).await?;

// 3. Arbitrary Expr in the ORDER BY position — case-insensitive
//    title sort via `LOWER(title)`, computed sort keys via
//    `case() / when() / value()`, arithmetic via `F("a") + F("b")`.
Post::objects()
    .order_by_expr(lower(F("title")), false)
    .order_by_expr_with_nulls(F("score") + 1_i64, true, NullsOrder::Last)
    .fetch(&pool).await?;
```

**Gestion des NULL par dialecte (sans `NullsOrder` explicite) :**

| Dialecte | Défaut ASC | Défaut DESC |
|---|---|---|
| PostgreSQL | NULLS LAST | NULLS FIRST |
| SQLite | NULLS LAST | NULLS FIRST |
| MySQL | NULL en premier (sémantique « plus petite valeur ») | NULL en dernier |

Utilisez `.order_by_with_nulls(...)` / `.order_by_expr_with_nulls(...)` pour fixer le placement ; sinon c'est le comportement natif par défaut de la base qui s'applique. Sur MySQL, le writer émet `<col> IS NULL <asc|desc>` avant le tri réel pour l'émuler ; le SQL émis comporte deux termes ORDER BY par colonne épinglée, mais la sémantique correspond à PG/SQLite.

**Composition de la chaîne.** `.order_by(...)`, `.order_by_with_nulls(...)` et `.order_by_expr(...)` s'accumulent dans une liste unifiée, **dans l'ordre d'enregistrement**. `.replace_order_by(&[...])` efface tous les appels order-by précédents. `.flip_order_by()` inverse chaque direction ET échange `NullsOrder::First` ↔ `NullsOrder::Last` afin que la sémantique « les NULL restent du même côté » survive à une inversion (pour `First` / `Last` explicites ; le comportement par défaut du dialecte sous `Default` continue de suivre la direction).

### Tri aléatoire

Retournez les lignes dans un ordre aléatoire — le `.order_by('?')` de Django. Utilisez `.order_random()`. Il émet `ORDER BY RANDOM()` sur PG et SQLite, `ORDER BY RAND()` sur MySQL. Pratique pour la rotation de bannières, l'échantillonnage, ou l'attribution de buckets de test A/B sans charger les lignes côté application pour les mélanger.

```rust
// Three random posts.
Post::objects()
    .order_random()
    .limit(3)
    .fetch(&pool).await?;

// Random tie-breaker after a primary sort: posts ordered by score
// descending, with ties shuffled.
Post::objects()
    .order_by(&[("score", true)])
    .order_random()
    .fetch(&pool).await?;
```

La variante IR ne porte aucune direction ni clause NULLS : un tri aléatoire est non ordonné par définition, et la clé aléatoire est calculée par ligne (non NULL).

**Avertissement de performance.** `ORDER BY RANDOM()` force un **balayage complet de la table + un tri en mémoire par une clé aléatoire par ligne**. Le planificateur de requêtes ne peut pas utiliser d'index. Pour des tables bien plus grandes que la mémoire, préférez le pattern compatible index :

```rust
// Coin-flip offset; range-scans the PK index.
let max_id: i64 = Post::objects().max::<i64>("id", &pool).await?.unwrap_or(0);
let offset = rand::random::<u32>() as i64 % max_id.max(1);
Post::objects()
    .where_(Post::id.gte(offset))
    .order_by(&[("id", false)])
    .limit(1)
    .fetch(&pool).await?;
```

Le compromis : l'adjacence dans les lignes du résultat reflète l'adjacence des PK, ce n'est donc pas « uniformément aléatoire » au sens strict — mais c'est exempt du coût d'un balayage complet de table.

### Pagination

Récupérez une page de résultats à la fois. `.limit(size).offset(...)` est la forme simple par numéro de page ; la forme par curseur (« tout ce qui vient après le dernier id vu ») s'adapte mieux sur les grandes tables.

```rust
// Page-number — page 2 of 50-row pages = LIMIT 50 OFFSET 50.
let page = Post::objects().limit(50).offset(50).fetch(&pool).await?;

// Cursor (manual — no auto-next-token from QuerySet)
let next = Post::objects()
    .where_(Post::id.gt(last_id))
    .order_by(&[("id", false)])
    .limit(50)
    .fetch(&pool).await?;
```

Pour la pagination par curseur côté HTTP, utilisez plutôt `ViewSet::cursor_pagination("id")`.

### Récupérer des lignes dans une map

Recherchez plusieurs lignes à partir d'une liste de valeurs et récupérez-les sous forme de `HashMap` indexée par cette colonne. C'est le `in_bulk(ids, field_name=)` de Django. Utilisez `.in_bulk(...)` pour « récupérer ces N lignes en un aller-retour, indexées par id ». Un `HashMap<K, V>` est le dictionnaire / la table de hachage de Rust.

```rust
use std::collections::HashMap;
use rustango::sql::Auto;

// Default Django shape: keyed by the Auto<i64> PK.
let books: HashMap<i64, Book> = Book::objects()
    .in_bulk(Book::id, [1_i64, 2, 3], |b| match b.id {
        Auto::Set(v) => v,
        Auto::Unset  => unreachable!("fetched row has Auto::Set PK"),
    }, &pool)
    .await?;
assert_eq!(books[&1].title, "The Rust Programming Language");

// `field_name=` equivalent — key by any unique column.
let by_isbn: HashMap<String, Book> = Book::objects()
    .in_bulk(Book::isbn, ["isbn-1".to_string()], |b| b.isbn.clone(), &pool)
    .await?;
```

Se compose avec les filtres `.where_()` précédents — la liste `IN` est jointe par un AND avec le WHERE existant. Un `ids` vide court-circuite avec une map vide (aucun SQL n'est émis). La closure gère explicitement le déballage `Auto<T>` / `ForeignKey<T, K>`, donnant à l'appelant le contrôle sur la façon dont la clé se matérialise.

Pendant cloisonnée par tenant : `in_bulk_on(column, ids, extract, &executor)` prend n'importe quel executor sqlx — à combiner avec `tenant.conn()` pour les tenants en mode schéma.

### Verrouiller des lignes pour mise à jour

Verrouillez les lignes sélectionnées afin qu'aucune autre transaction ne puisse les modifier avant votre commit — la manière standard de réserver du travail ou d'éviter les mises à jour perdues. C'est le `select_for_update(skip_locked=, nowait=, of=, no_key=)` de Django. Appelez `.select_for_update()` ; cela ajoute `SELECT … FOR UPDATE` (ou une variante) et le verrou dure pendant toute la transaction englobante.

```rust
// Canonical "claim next available row" pattern. Worker A grabs the
// lowest-priority pending job; concurrent worker B with SKIP LOCKED
// skips A's row and grabs the next instead — no blocking.
let mut tx = pool.begin().await?;
let claim: Vec<Job> = Job::objects()
    .where_(Job::status.eq("pending"))
    .order_by(&[("priority", false)])
    .limit(1)
    .select_for_update()
    .skip_locked()
    .fetch_on(&mut *tx).await?;
// ... mark claim[0] as in-progress, do work ...
tx.commit().await?;
```

**Méthodes du builder** — à chaîner pour les activer :

- `.select_for_update()` — simple `FOR UPDATE`.
- `.skip_locked()` — ajoute `SKIP LOCKED` ; les lignes détenues par une autre transaction sont silencieusement écartées au lieu de bloquer.
- `.nowait()` — ajoute `NOWAIT` ; remonte immédiatement une erreur du driver si une ligne correspondante est verrouillée. Mutuellement exclusif avec `skip_locked` (le writer choisit le plus permissif, `SKIP LOCKED`, si les deux sont définis).
- `.no_key()` — émet `FOR NO KEY UPDATE` à la place (PG 9.3+). Verrou plus faible qui ne bloque pas les écrivains touchant uniquement des colonnes non-clé.
- `.of(&["table_or_alias", …])` — restreint le verrou à des tables spécifiques quand la requête fait des JOIN.

Appeler `.skip_locked()` / `.nowait()` / `.no_key()` / `.of(…)` sans `.select_for_update()` préalable active implicitement le verrou, ce qui correspond à l'ergonomie de Django.

**Comportement tri-dialecte :**

| Dialecte | Comportement |
|---|---|
| PostgreSQL | Support complet — chaque flag émet sa syntaxe native. |
| MySQL 8.0.1+ | Prend tout en charge sauf `NO KEY` — ce flag retombe sur le simple `FOR UPDATE` (le verrou le plus strict). |
| SQLite | Aucune syntaxe de verrou au niveau ligne. Le writer n'émet aucune clause ; les transactions détiennent un verrou d'écriture implicite pour la base entière. Utilisez une autre stratégie pour SQLite (typiquement une boucle d'attente active sur la transaction elle-même). |

**Doit s'exécuter dans une transaction.** `FOR UPDATE` hors transaction est un no-op sur PostgreSQL (la transaction implicite à instruction unique libère le verrou immédiatement) et une erreur sur MySQL. À combiner avec `pool.begin()` (ou `rustango::sql::atomic`).

### Combiner des requêtes (union, intersection, différence)

Fusionnez deux requêtes ou plus sur le même modèle avec les opérateurs d'ensemble SQL. Ce sont les `.union()`, `.intersection()` et `.difference()` de Django.

```rust
// Posts that are EITHER drafts OR currently in review.
let inbox: Vec<Post> = Post::objects()
    .where_(Post::status.eq("draft"))
    .union(Post::objects().where_(Post::status.eq("review")))
    .order_by(&[("created_at", true)])
    .limit(50)
    .fetch(&pool).await?;
```

**Méthodes du builder** :

| Méthode | SQL | Sémantique |
|---|---|---|
| `.union(other)` | `UNION` | Combine + déduplique |
| `.union_all(other)` | `UNION ALL` | Combine, conserve les doublons (moins coûteux, pas de passe DISTINCT) |
| `.intersection(other)` | `INTERSECT` | Lignes présentes dans les DEUX querysets |
| `.difference(other)` | `EXCEPT` | Lignes du premier queryset mais PAS des autres |

Chaque méthode prend un `QuerySet<T>` — les deux branches doivent cibler le même modèle `T`, de sorte que la forme des colonnes correspond par construction (vérifié à la compilation grâce aux génériques de Rust). Les appels s'accumulent ; mélanger des opérateurs dans une même chaîne est autorisé (`a.union(b).intersection(c)` s'évalue de gauche à droite conformément à la norme SQL).

**Les modificateurs externes s'appliquent au résultat fusionné** :

```rust
// Outer .order_by() / .limit() / .offset() / .select_for_update()
// set AFTER the union apply to the combined resultset, NOT per-branch.
let page: Vec<Post> = qs_a
    .union(qs_b)
    .union(qs_c)
    .order_by(&[("id", false)])    // sorts the merged rows
    .limit(20)                     // caps the merged count
    .offset(40)                    // skips into the merged result
    .fetch(&pool).await?;

// Per-branch ORDER BY / LIMIT stay INSIDE the branch's parens:
let mixed = qs_a
    .union(qs_b.order_by(&[("id", true)]).limit(5))   // branch picks its top 5
    .fetch(&pool).await?;
```

**Tri-dialecte** : PostgreSQL + SQLite prennent en charge les quatre opérateurs sur chaque version que **Rustango** supporte. MySQL 8.0+ prend en charge `UNION`/`UNION ALL` ; `INTERSECT`/`EXCEPT` sont arrivés dans MySQL 8.0.31. Les versions plus anciennes de MySQL font remonter l'erreur de syntaxe du driver au moment du fetch — il n'y a pas de garde côté client.

**Chemin d'erreur sur le builder typé** : `.union(other_qs)` (ainsi que `.intersection()` / `.difference()`) compile la branche de manière eager et panique si la branche échoue à compiler (colonne mal orthographiée, etc.). Pour une composition faillible où l'appelant veut un `Result`, compilez d'abord la branche et passez-la via `.with_compound(SetOp::Union, branch)` — un point d'entrée générique unique couvre chaque opérateur. La forme de la panique correspond à celle de Django : une mauvaise branche est une erreur de programmation, pas une condition de données au runtime.

### Streamer de grands ensembles de résultats

Traitez une table volumineuse sans la charger entièrement en mémoire. C'est le `.iterator(chunk_size=2000)` de Django. Appelez `.iterator(chunk_size)` ; il récupère `chunk_size` lignes à la fois (via `LIMIT N OFFSET M`) et ne met jamais en tampon l'ensemble du résultat. À utiliser pour les exports de millions de lignes, les pipelines ETL, et les jobs batch.

```rust
// 1. Whole-chunk loop — process N rows at a time.
let mut iter = Post::objects()
    .where_(Post::published.eq(true))
    .order_by(&[("id", false)])
    .iterator(2_000)?;
while let Some(chunk) = iter.next_chunk(&pool).await? {
    for post in chunk { /* … */ }
}

// 2. Row-by-row loop — buffer one chunk internally, yield one row.
let mut iter = Post::objects().order_by(&[("id", false)]).iterator(2_000)?;
while let Some(post) = iter.next_row(&pool).await? {
    /* … */
}
```

**Définissez un `order_by`.** `OFFSET` sur une requête sans tri stable retourne des lignes imprévisibles d'un chunk à l'autre — typiquement `.order_by(&[("pk", false)])` afin que chaque chunk reprenne proprement. La méthode n'impose pas de tri (certaines requêtes veulent légitimement aucun tri, par exemple une vidange en une seule fois), mais itérer sans tri est un piège classique.

**Compromis vs curseurs côté serveur.** C'est un simple découpeur par LIMIT/OFFSET. Sur une colonne de tri indexée par btree, PostgreSQL balaie les N premières lignes avant de retourner la (N+1)-ième — donc une pagination profonde coûte `O(n²)` au total. Pour une vidange de 10M de lignes, cela compte ; pour 100k lignes, généralement pas. Le découpeur l'emporte sur la portabilité (fonctionne sur tous les backends sans surcharge de transaction) et la simplicité (aucune gestion de cycle de vie de curseur). Pour une lecture réellement en streaming sur PG, passez par `pool.begin()` + l'API Stream `sqlx::query(...).fetch(&mut *tx)` en SQL brut directement — le protocole étendu streame depuis le serveur sans re-cherche par offset.

**Mélanger `next_chunk` et `next_row` sur le même itérateur est sûr.** Le tampon interne `VecDeque` se vide dans l'ordre des lignes avant tout nouveau fetch en base, donc `next_chunk` après un `next_row` partiel retourne d'abord les lignes déjà en tampon, puis continue avec des chunks frais.

`.rows_seen()` (compteur cumulé) et `.is_exhausted()` (indicateur de fin de vidange) sont tous deux disponibles pour le suivi de progression et les vérifications de terminaison.

**Risque d'écriture concurrente.** Chaque chunk est une requête séparée, donc des lignes insérées/supprimées entre les chunks peuvent être ignorées ou dupliquées (le classique problème de « fenêtrage » de la pagination par OFFSET). Pour des tables en lecture seule / uniquement en ajout — le cas d'usage typique de l'export — ce n'est pas un problème. Pour des tables écrites de manière concurrente, il vous faut une transaction en isolation snapshot afin que chaque chunk voie la même vue. **`ChunkedIter` prend un `&Pool`, pas une `&mut Transaction`, donc l'API du découpeur ne peut pas être utilisée directement dans la transaction** — codez à la main le SELECT découpé contre la transaction :

```rust
let mut tx = pool.begin().await?;
sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
    .execute(&mut *tx).await?;

// Hand-loop LIMIT/OFFSET chunks against the tx with `.fetch_on(&mut *tx)`,
// so every chunk reads from the same snapshot.
let chunk_size = 2_000_i64;
let mut offset = 0_i64;
loop {
    let rows: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .limit(chunk_size)
        .offset(offset)
        .fetch_on(&mut *tx)
        .await?;
    if rows.is_empty() { break; }
    for post in &rows { /* … */ }
    if (rows.len() as i64) < chunk_size { break; }
    offset += rows.len() as i64;
}
tx.commit().await?;
```

**`select_for_update()` ne se propage pas d'un chunk à l'autre.** Les verrous de ligne détenus par `.select_for_update()` sont relâchés à la fin de la transaction implicite de chaque chunk. Il n'existe pas de correctif au niveau du découpeur : le builder `.iterator()` prend un `&Pool`, les variantes de verrouillage ont besoin d'une `&mut Transaction`, et les deux ne se combinent pas. Pour une vidange verrouillée, vous avez deux options, chacune avec un compromis :

- **`.fetch_on(&mut *tx)` sur tout le résultat** — un seul aller-retour, un `Vec<T>` complet en mémoire. Correct quand le résultat tient en mémoire.
- **LIMIT/OFFSET codé à la main dans la transaction** — même forme que l'extrait en isolation snapshot ci-dessus ; les chunks restent streamés mais vous êtes hors de l'API `ChunkedIter`.

Un futur compagnon `iterator_on(&mut *tx, chunk_size)` (suivi d'issue) comblerait cet écart. Hors périmètre pour l'issue #23.

**`chunk_size` doit être > 0.** Les valeurs nulles ou négatives paniquent. Choisissez une valeur adaptée à votre budget de taille de ligne (le défaut de Django est `2000` ; raisonnable pour des lignes étroites, plus bas pour de larges colonnes TEXT/JSONB).

### Sélectionner des colonnes spécifiques

Récupérez seulement quelques colonnes au lieu de structs `Post` entières — les `.values('col')` et `.values_list('col', flat=True)` de Django. Utilisez-les quand vous n'avez besoin que de quelques colonnes d'une table large, ou quand le résultat alimente du code dynamique (templates, export CSV, JSON). Vous récupérez des maps, des tuples, ou une liste typée plate au lieu d'instances de modèle.

```rust
use rustango::core::SqlValue;
use std::collections::HashMap;

// 1. Column-keyed map per row — Django's `.values('id', 'title')`.
let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
    .where_(Post::published.eq(true))
    .order_by(&[("id", false)])
    .values_dict(&["id", "title"])
    .fetch(&pool).await?;

// 2. Ordered tuple per row — Django's `.values_list('id', 'title')`.
//    Cell ordering matches the column-list argument.
let rows: Vec<Vec<SqlValue>> = Post::objects()
    .values_list(&["title", "id"])  // title first, id second
    .fetch(&pool).await?;

// 3. Single-column typed scalar — Django's `.values_list('id', flat=True)`.
//    Returns Vec<U> directly via sqlx's typed scalar path.
let ids: Vec<i64> = Post::objects()
    .where_(Post::published.eq(true))
    .values_list_flat("id")
    .fetch::<i64>(&pool).await?;
```

**Trois builders, une seule IR.** Les trois définissent `SelectQuery::projection` sur la liste de colonnes validée — le SQL est identique dans les trois formes terminales ; seul le décodage des lignes diffère :

| Builder | Forme SQL | Retourne |
|---|---|---|
| `.values_dict(&[cols])` | `SELECT col1, col2 FROM …` | `Vec<HashMap<String, SqlValue>>` |
| `.values_list(&[cols])` | `SELECT col1, col2 FROM …` | `Vec<Vec<SqlValue>>` (ordonné selon `cols`) |
| `.values_list_flat(col)` | `SELECT col FROM …` | `Vec<U>` (typé, via `fetch::<U>(...)`) |

**Fonctionne avec le reste de la chaîne de requête.** `.where_()`, `.filter()`, `.order_by()`, `.limit()`, `.offset()`, et les opérateurs d'ensemble (`.union()` / `.intersection()` / `.difference()`) — toute méthode appelée AVANT `.values_*` est conservée. Les builders values sont terminaux (rien ne s'enchaîne après eux), donc définissez la forme de la requête d'abord, puis exécutez le fetch.

**Validation au moment de `.compile()` / `.fetch()` :**
- Liste de colonnes vide (`.values_dict(&[])`) → [`QueryError::EmptyValuesProjection`].
- Nom de colonne mal orthographié (`.values_dict(&["nope"])`) → [`QueryError::UnknownField`].

**Tri-dialecte : émission de la projection identique sur PG / MySQL / SQLite** (seul le guillemetage des identifiants diffère). Pour `.values_list_flat::<U>(...)`, `U` doit implémenter `Decode + Type` de sqlx sur chaque backend ciblé par le binaire ; les choix courants (`i64`, `i32`, `String`, `bool`, `f64`) fonctionnent universellement.

**Pourquoi ne pas changer le `.values()` existant pour faire une projection pure ?** `QuerySet::values(cols)` est déjà promu vers [`AggregateBuilder`] pour le chemin d'auto-inférence de GROUP BY (issue #75). Le renommer casserait ~20 sites d'appel existants. La nouvelle chaîne de méthodes `.values_dict()` / `.values_list()` / `.values_list_flat()` se place à côté, en laissant le chemin d'agrégation intact. L'erreur préexistante `QueryError::ValuesRequiresAggregate` se déclenche toujours pour `.values(cols).compile()` sans `.annotate(...)` ultérieur — son message oriente désormais les appelants vers les nouvelles méthodes de projection pure.

### Inclure ou exclure des colonnes

Même idée que la section précédente, mais dans la forme include/exclude de Django : `.only('id', 'name')` ne conserve que les colonnes nommées, `.defer('big_field')` conserve tout sauf elles. Utilisez-les sur des tables larges où de grandes colonnes TEXT / BLOB / JSONB rendent les vues de liste coûteuses à lire :

```rust
// .only(...) — fetch only the named columns.
let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
    .where_(Post::published.eq(true))
    .only(&["id", "title"])
    .fetch(&pool).await?;

// .defer(...) — fetch everything except the named columns.
// Useful for "list view: skip body / metadata / large JSON".
let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
    .defer(&["body", "raw_html"])
    .fetch(&pool).await?;
```

**Sémantique** : `.only(&[cols])` est un synonyme de `.values_dict(cols)` — même IR, même forme de retour, point d'entrée séparé pour une lisibilité de forme Django. `.defer(&[cols])` calcule le complément par rapport au schéma du modèle (chaque colonne scalaire du modèle SAUF celles listées) et route vers le même chemin.

**Avertissement — le type de retour diffère de Django.** Les `.only()` / `.defer()` de Django retournent des instances de `Model` partiellement hydratées où les champs différés se chargent paresseusement à l'accès à l'attribut. **Rustango** n'a pas d'équivalent à la magie des descripteurs de Python ; la forme de retour est `Vec<HashMap<String, SqlValue>>` (ou `Vec<Vec<SqlValue>>` si vous substituez `.values_list(...)` à la place). Le décodage de ligne partielle typé est prévu pour une future itération.

**Sécurité vis-à-vis des fautes de frappe** : `.defer(&["nope_col"])` fait remonter `QueryError::UnknownField` au moment de `.compile()` — la faute de frappe ne se transforme pas silencieusement en « projeter toutes les colonnes ». `.only(&[])` fait remonter `QueryError::EmptyValuesProjection` ; `.defer(&[])` est un no-op sémantique (projette chaque colonne).

### Filtrer avec des expressions régulières

Comparez une colonne à un motif regex — les `__regex` / `__iregex` de Django. `.regex()` est sensible à la casse, `.iregex()` insensible à la casse, et `.not_regex()` / `.not_iregex()` en sont les formes négées.

```rust
use rustango::core::Column as _;

// Names starting with "al" (case-sensitive).
User::objects()
    .where_(User::name.regex("^al.*"))
    .fetch(&pool).await?;

// Names starting with "al" — case-insensitive.
User::objects()
    .where_(User::name.iregex("^al.*"))
    .fetch(&pool).await?;

// Negated: exclude names starting with "admin" (case-sensitive).
User::objects()
    .where_(User::name.not_regex("^admin"))
    .fetch(&pool).await?;

// Django-shape lookup-suffix form.
User::objects()
    .filter("name__iregex", "^bob")
    .fetch(&pool).await?;
```

**Émission tri-dialecte** :

| Dialecte | Sensible à la casse | Insensible à la casse | Notes |
|---|---|---|---|
| PostgreSQL | `<col> ~ ?` / `<col> !~ ?` | `<col> ~* ?` / `<col> !~* ?` | Opérateurs POSIX natifs |
| MySQL | `` `col` REGEXP ? `` / `` `col` NOT REGEXP ? `` | `LOWER(`col`) REGEXP LOWER(?)` (la forme négée enveloppe avec `NOT`) | Repli via LOWER() pour `i*` |
| SQLite | `"col" REGEXP ?` / `"col" NOT REGEXP ?` | `LOWER("col") REGEXP LOWER(?)` (la forme négée enveloppe avec `NOT`) | Nécessite la fonction utilisateur `regexp` chargée sur la connexion |

**SQLite exige une fonction utilisateur `regexp` enregistrée** — elle n'est pas intégrée. sqlx-sqlite 0.8 n'en enregistre **pas** une par défaut. Deux façons de l'activer :

1. **Simple** — activez la feature cargo `regexp` de sqlx-sqlite, puis activez-la sur la connexion :
   ```rust
   use sqlx::sqlite::SqliteConnectOptions;
   let opts = SqliteConnectOptions::new()
       .filename("app.db")
       .with_regexp();  // gated on sqlx-sqlite/regexp
   ```
2. **Manuelle** — enregistrez une closure Rust via `SqliteConnection::lock_handle()` + FFI brut (`sqlite3_create_function_v2`).

Sans cela, la requête émet du SQL `REGEXP` valide que SQLite rejette à l'exécution avec `no such function: regexp` (propre à l'analyse syntaxique — `tests/regex_sqlite_live.rs` fixe ce comportement).

**Le dialecte du motif diffère selon les backends.** PostgreSQL utilise les regex POSIX étendues ; MySQL utilise des regex basées sur ICU avec sa propre saveur ; SQLite délègue à ce que la fonction utilisateur implémente (typiquement la crate `regex` de Rust). Les motifs qui s'appuient sur une syntaxe spécifique à un dialecte (par exemple les frontières de mots `\m` / `\M` de PG) ne se transposent pas d'un dialecte à l'autre — restez sur le sous-ensemble portable (`^`, `$`, `.`, `*`, `+`, `?`, `[...]`, `()`, `|`) si le même modèle est interrogé depuis plusieurs backends.

**Les valeurs non-chaîne sont rejetées au moment de `.compile()`** — passer `SqlValue::I64(42)` à `__regex` fait remonter `QueryError::InvalidLookupValue { suffix: "regex", expected: "SqlValue::String(<regex pattern>)", … }` plutôt qu'une conversion silencieuse.

---

## Valeurs calculées & fonctions de base de données

Laissez la base de données calculer les choses au lieu de charger des lignes dans l'application, les modifier, puis les réécrire. `F("col")` référence une colonne par son nom (l'objet `F()` de Django), et les builders `funcs::*` enveloppent des fonctions SQL scalaires comme `LOWER` ou `COALESCE`. Ensemble, ils débloquent trois patterns que le simple `.set()` / `.where_()` basé sur des valeurs ne peut pas exprimer :

### Incréments atomiques (pas de race lecture-modification-écriture)

Le bug classique de compteur — récupérer une ligne, incrémenter un champ, sauvegarder — perd des mises à jour quand deux requêtes s'exécutent en même temps. `F("col") + 1` réduit l'aller-retour à un seul `UPDATE`, de sorte que la base détient le verrou de ligne pour vous :

```rust
use rustango::core::F;

Post::objects()
    .eq("id", post_id)
    .update()
    .set_expr("view_count", F("view_count") + 1_i64)
    .execute(&pool).await?;
```

Tri-dialecte : émet `views = ("views" + $1)` sur PG, ``views = (`views` + ?)`` sur MySQL, identique sur SQLite. L'arithmétique est parenthésée afin que les opérations imbriquées restent non ambiguës : `F("a") + F("b") * 2`.

Opérateurs pris en charge : `+ - * / %` plus `& | ^ << >>` (opérations sur bits ; le XOR sur SQLite émet une erreur claire `OpNotSupportedInDialect` puisque SQLite n'a pas de symbole XOR).

### Comparer deux colonnes dans un filtre

Filtrez une colonne par rapport à une autre, pas par rapport à un littéral — par exemple `Reservation start_date < end_date` pour vérifier la cohérence d'une ligne, ou `Inventory available > reserved` pour trouver les lignes disposant de capacité :

```rust
use rustango::core::Column as _;

// `start_date < end_date` for every selected row.
let valid = Reservation::objects()
    .where_(Reservation::start_date.lt_expr(F("end_date")))
    .fetch(&pool).await?;

// Combine with literal predicates.
let oversold = Inventory::objects()
    .where_(Inventory::available.lt_expr(F("reserved")))
    .where_(Inventory::active.eq(true))
    .fetch(&pool).await?;
```

La famille `*_expr` — `eq_expr`, `ne_expr`, `lt_expr`, `lte_expr`, `gt_expr`, `gte_expr` — reflète les méthodes littérales `eq`, `ne`, … mais accepte n'importe quel `impl Into<Expr>` sur le côté droit : des références de colonne nues (`F("col")`), de l'arithmétique (`F("price") * 2`), ou des résultats de fonction (section suivante).

### Fonctions scalaires — texte, mathématiques, gestion des NULL

`rustango::core::funcs` fournit des builders pour les fonctions SQL les plus utilisées. Les 17 disponibles à ce jour :

| Groupe | Builders |
|---|---|
| **Texte** | `lower`, `upper`, `length`, `trim`, `ltrim`, `rtrim`, `concat`, `substr`, `replace` |
| **Mathématiques** | `abs`, `ceil`, `floor`, `round` (1 argument) / `round_to` (2 arguments, précision) |
| **NULL** | `coalesce`, `greatest`, `least`, `nullif` |

```rust
use rustango::core::funcs::{lower, upper, concat, coalesce, trim, abs, round};
use rustango::core::F;

// Normalize on write.
User::objects()
    .eq("id", id)
    .update()
    .set_expr("email", lower(trim(F("email"))))
    .execute(&pool).await?;

// Build a derived column from two FKs + a literal.
User::objects()
    .update()
    .set_expr(
        "display_name",
        concat([F("first").into(), " ".into(), F("last").into()]),
    )
    .execute(&pool).await?;

// First non-NULL fallback.
User::objects()
    .update()
    .set_expr(
        "label",
        coalesce([F("nickname").into(), F("username").into(), "anonymous".into()]),
    )
    .execute(&pool).await?;

// Function on the WHERE rhs.
User::objects()
    .where_(User::email_norm.eq_expr(lower(F("email_norm"))))
    .fetch(&pool).await?;

// Functions compose freely — `abs(round(F("score") * 100))` is one Expr.
Player::objects()
    .update()
    .set_expr("score_int", abs(round(F("score") * 100_f64)))
    .execute(&pool).await?;
```

### Comportement tri-dialecte

La plupart des fonctions émettent un SQL identique sur PG / MySQL / SQLite. Les formes divergentes sont gérées par dialecte, de manière transparente :

| Builder | PG | MySQL | SQLite |
|---|---|---|---|
| `concat([a, b])` | `CONCAT(a, b)` | `CONCAT(a, b)` | `(a \|\| b)` |
| `substr(s, 1, 3)` | `SUBSTRING(s FROM 1 FOR 3)` | `SUBSTRING(s, 1, 3)` | `SUBSTR(s, 1, 3)` |
| `greatest([a, b])` | `GREATEST(a, b)` | `GREATEST(a, b)` | `MAX(a, b)` scalaire |
| `least([a, b])` | `LEAST(a, b)` | `LEAST(a, b)` | `MIN(a, b)` scalaire |

### Passer des arguments mixtes à une fonction

Les fonctions qui prennent une liste d'arguments (comme `concat`) acceptent n'importe quel itérable d'`Expr`. Les tableaux Rust doivent contenir un seul type, donc un mélange de `F` (colonne) et `&str` (littéral) ne compile pas tel quel — appelez `.into()` une fois par élément pour élever chacun en `Expr` :

```rust
concat([F("first").into(), " ".into(), F("last").into()])
//          ^^^^^^ each element lifted to Expr
```

Ou construisez un `Vec<Expr>` et passez-le directement — même forme, même résultat.

### Avertissements

- **`length` octets-vs-caractères** : PG retourne des caractères sur `TEXT`/`VARCHAR`, MySQL retourne des **octets** (utilisez le futur builder `CharLength` du framework ou enveloppez manuellement dans `CHAR_LENGTH` si vous avez besoin de comptages de caractères multi-dialecte).
- **`round(x, n)` sur PG** : la forme à 2 arguments de PG exige un `numeric`, pas un `double`. Passez soit une colonne entière, soit convertissez d'abord le float ; MySQL et SQLite acceptent les deux types.
- **`greatest([single_arg])` / `least([single_arg])` sur SQLite** : non pris en charge — le `MAX(x)` à un seul argument de SQLite est la forme *agrégat*, pas la forme scalaire. Le writer retourne `OpNotSupportedInDialect`. PG et MySQL acceptent la forme à un seul argument comme un no-op retournant `x`. Enveloppez avec au moins un littéral pour rester portable.
- **`substr` avec un début négatif** : PG traite un négatif comme « commencer à la position de caractère N » (avec un effet de clamp à 0) ; MySQL et SQLite traitent un négatif comme « compter depuis la fin ». Évitez les débuts négatifs dans du code portable.

### Fonctions de date & heure

Les builders `now()`, `extract_*` et `trunc_*` fonctionnent sur les dates et timestamps. Utilisez-les pour les requêtes de cohortes, les agrégats par tranche temporelle, et l'estampillage de l'heure courante à l'écriture — tout cela dans la base de données, sans faire d'aller-retour de lignes vers l'application.

```rust
use rustango::core::funcs::{
    now, trunc_date, trunc_month,
    extract_year, extract_month, extract_weekday,
};
use rustango::core::F;

// 1. Stamp server-side current time on write.
Post::objects()
    .eq("id", id)
    .update()
    .set_expr("published_at", now())
    .execute(&pool).await?;

// 2. Extract year / month / weekday into denormalized indexable
// columns so cohort + day-of-week queries are cheap.
Signup::objects()
    .update()
    .set_expr("bucket_year", extract_year(F("created_at")))
    .set_expr("bucket_month", extract_month(F("created_at")))
    .set_expr("weekday", extract_weekday(F("created_at")))
    .execute(&pool).await?;

// 3. Filter on the stored bucket — typed integer comparison, uses
// the index, portable across all three dialects.
let friday_signups = Signup::objects()
    .where_(Signup::weekday.eq(5_i64))            // 5 = Friday (0=Sun)
    .fetch(&pool).await?;

// 4. For range filters where you'd be tempted to write
// `created_at >= trunc_year(now())` directly: don't. The function
// builders for `Trunc*` return text on MySQL/SQLite (see caveats
// below), so a column-vs-trunc comparison in WHERE only behaves
// well on PG. Compute the boundary in Rust instead and pass it as a
// typed literal — works the same on every backend and uses the
// index on `created_at`:
use chrono::{Datelike, TimeZone};
let this_year = chrono::Utc::now().year();
let year_start = chrono::Utc.with_ymd_and_hms(this_year, 1, 1, 0, 0, 0).unwrap();

let recent = Order::objects()
    .where_(Order::created_at.gte(year_start))
    .fetch(&pool).await?;

// 5. `Trunc*` shines on the *write* side. `trunc_date` is the
// one trunc-family builder with identical SQL on every dialect
// (`DATE(x)`) — handy for grouping by day without the type-divergence
// caveat the year/month variants carry.
Order::objects()
    .update()
    .set_expr("day_bucket", trunc_date(F("created_at")))     // DATE column on every backend
    .set_expr("month_bucket", trunc_month(F("created_at")))  // see caveat
    .execute(&pool).await?;
// `month_bucket` should be `TIMESTAMPTZ` on PG and `VARCHAR(10)` /
// `TEXT` on MySQL/SQLite — parse client-side when reading if you
// need a typed `chrono::NaiveDate`.
```

**Émission par dialecte :**

| Builder | PG | MySQL | SQLite |
|---|---|---|---|
| `now()` | `NOW()` | `NOW()` | `CURRENT_TIMESTAMP` |
| `extract_year(x)` | `CAST(EXTRACT(YEAR FROM x) AS INTEGER)` | `YEAR(x)` | `CAST(strftime('%Y', x) AS INTEGER)` |
| `extract_week(x)` ⚠ | `EXTRACT(WEEK FROM x)` — ISO 8601, plage 1–53 | `WEEK(x)` — début dimanche, plage **0**–53 | `strftime('%W', x)` — début lundi, plage 00–53 |
| `extract_weekday(x)` | `CAST(EXTRACT(DOW FROM x) AS INTEGER)` | `(DAYOFWEEK(x) - 1)` | `CAST(strftime('%w', x) AS INTEGER)` |
| `extract_quarter(x)` | `EXTRACT(QUARTER FROM x)` | `QUARTER(x)` | **non pris en charge** — erreur |
| `trunc_date(x)` | `DATE(x)` | `DATE(x)` | `DATE(x)` |
| `trunc_year(x)` | `DATE_TRUNC('year', x)` → timestamp | `DATE_FORMAT(x, '%Y-01-01')` → **chaîne** | `strftime('%Y-01-01', x)` → **chaîne** |
| `trunc_month(x)` | `DATE_TRUNC('month', x)` → timestamp | `DATE_FORMAT(x, '%Y-%m-01')` → **chaîne** | `strftime('%Y-%m-01', x)` → **chaîne** |
| `trunc_day(x)` | `DATE_TRUNC('day', x)` → timestamp | `DATE(x)` → date | `date(x)` → texte |

**Avertissements spécifiques aux dates/heures :**

- **Le type de retour de `trunc_year/month` diverge** : timestamp sur PG, texte sur MySQL/SQLite. Effectuez la conversion côté application à la lecture si vous avez besoin d'un `chrono::NaiveDate` typé — ou stockez le bucket comme un simple entier (`extract_year` + `extract_month`) et reconstruisez-le dans le code.
- **`extract_weekday` est normalisé à 0 = dimanche** sur les trois dialectes. Le `DAYOFWEEK()` natif de MySQL retourne 1=dimanche, donc le writer soustrait 1.
- **⚠ `extract_week` n'est PAS portable.** PG retourne des numéros de semaine ISO 8601 (début lundi, plage 1–53) ; le `WEEK(x)` par défaut de MySQL débute le dimanche avec une plage **0**–53 ; le `strftime('%W')` de SQLite débute le lundi avec une plage 00–53. Pour le 2024-01-01 (un lundi), les trois backends retournent respectivement `1`, `0` et `01`. Le code mono-backend peut l'utiliser librement ; le code multi-dialecte devrait calculer la limite de semaine comme un `chrono::DateTime` typé en Rust et filtrer sur la colonne timestamp à la place.
- **`extract_quarter` génère une erreur sur SQLite** avec `OpNotSupportedInDialect` — SQLite n'a pas de jeton trimestre natif. Soit protégez la fonctionnalité derrière `cfg(not(sqlite))`, soit calculez via `((extract_month - 1) / 3) + 1` côté application.
- **Gestion des fuseaux horaires** : `EXTRACT` de PG opère dans le fuseau horaire de la colonne ; `YEAR()` de MySQL opère dans le fuseau horaire de la session (`SET time_zone = ...`) ; SQLite n'a pas de vrai support de fuseau horaire — traitez tout comme UTC. Utilisez `TIMESTAMPTZ` sur PG, `DATETIME` sur MySQL avec le fuseau horaire de session défini, des chaînes ISO-8601 sur SQLite.

### Expressions CASE WHEN

Construisez un `CASE WHEN … THEN … ELSE … END` SQL avec les builders `case()` / `.when()` / `value()` — le `Case`/`When` de Django. Utilisez-le pour des tris personnalisés, des colonnes dérivées dans `annotate`, des valeurs par défaut calculées dans `update`, et (combiné avec `Sum`) des agrégats conditionnels.

```rust
use rustango::core::case::{case, value};
use rustango::core::{Column as _, F};
use rustango::core::funcs::lower;

// Custom ordering — published posts first, drafts last.
Post::objects()
    .update()
    .set_expr(
        "priority",
        case()
            .when(Post::status.eq("published"), 0_i64)
            .when(Post::status.eq("review"), 1_i64)
            .when(Post::status.eq("draft"), 2_i64)
            .default(99_i64),
    )
    .execute(&pool).await?;

let ordered = Post::objects()
    .order_by(&[("priority", false), ("id", false)])
    .fetch(&pool).await?;

// Computed default on update — drafts get a lowercased title for
// the label, everything else uses the title verbatim.
Post::objects()
    .update()
    .set_expr(
        "label",
        case()
            .when(Post::status.eq("draft"), lower(F("title")))
            .default(F("title")),
    )
    .execute(&pool).await?;

// AND / OR composition in the WHEN predicate.
let viral = Post::status.eq("published").and(Post::views.gt(1_000_i64));
Post::objects()
    .update()
    .set_expr(
        "label",
        case()
            .when(viral, value("viral"))
            .when(Post::status.eq("published"), value("live"))
            .default(value("pending")),
    )
    .execute(&pool).await?;
```

**Forme du builder :**

- `case()` — démarre un builder.
- `.when(condition, then)` — ajoute une branche. `condition` est n'importe quoi implémentant `Into<WhereExpr>` (typiquement `Column::eq()`, `.and()`, `.or()`) ; `then` est n'importe quoi implémentant `Into<Expr>` (littéral, `F()`, appel de fonction, `case()` imbriqué).
- `.default(expr)` — définit la branche `ELSE` optionnelle. L'omettre produit un `CASE` qui retourne `NULL` pour les lignes non appariées (norme SQL).
- `.build()` ou `.into()` — finalise en un `Expr` pour `set_expr` / `eq_expr` / `annotate`.
- `value(literal)` — sucre syntaxique de style Django pour `Expr::Literal(...)`. Optionnel — les littéraux nus se convertissent via `Into<Expr>`, mais `value("…")` se lit explicitement comme « c'est un littéral chaîne, pas une référence de colonne ».

**Émission tri-dialecte :**

`CASE WHEN … THEN … [ELSE …] END` fait partie de la norme SQL-92 — émis de manière identique sur PG, MySQL et SQLite. Aucune répartition par dialecte dans le writer.

**Avertissements :**

- **Branches vides** : `case().build()` sans appel `.when(...)` est rejeté à l'émission avec `SqlError::EmptyCaseBranches`. SQL exige au moins une clause `WHEN`. Une condition `WHEN` vide (par exemple `WhereExpr::And(vec![])`) est rejetée avec `SqlError::EmptyCaseWhenCondition` pour la même raison.
- **Unification des types entre branches** : chaque dialecte choisit un type commun à partir des valeurs `THEN` et `ELSE`. Mélanger les types (`THEN 1_i64` + `ELSE "string"`) peut lever une erreur de conversion au runtime ou coercer de manière surprenante. Restez sur un seul type par `CASE`.
- **Performance** : chaque ligne évalue les prédicats `WHEN` dans l'ordre jusqu'à ce qu'un corresponde (premier qui correspond gagne, par ligne). Le coût croît avec le nombre de branches et le coût des prédicats. Pour de nombreuses correspondances de chaînes fixes, une jointure avec une petite table de correspondance peut être moins coûteuse et plus lisible.

### Sous-requêtes (EXISTS, IN, scalaire)

Intégrez une requête dans une autre — les `Exists`, `Subquery` et `OuterRef` de Django. Ces builders couvrent la plupart des patterns « existe-t-il au moins une ligne liée ? » et « cette valeur est-elle dans cet ensemble ? » :

| Builder | Forme | À utiliser pour |
|---|---|---|
| `exists(qs)` | `EXISTS (SELECT … FROM …)` | « Auteurs ayant au moins un livre » |
| `not_exists(qs)` | `NOT EXISTS (SELECT …)` | « Auteurs sans aucun livre » (anti-jointure) |
| `in_subquery(col, qs)` | `<col> IN (SELECT …)` | « Posts dans n'importe quelle catégorie publique » |
| `not_in_subquery(col, qs)` | `<col> NOT IN (SELECT …)` | Inverse du précédent |
| `subquery(qs)` | `(SELECT …)` en tant que scalaire | Valeur par défaut calculée dans `set_expr` |
| `outer_ref(col)` | `"<outer_table>"."<col>"` | Référencer la ligne externe depuis l'intérieur de l'un des éléments ci-dessus |

```rust
use rustango::core::subquery::{exists, not_exists, in_subquery, outer_ref};
use rustango::core::{Column as _, WhereExpr};

// "Authors with no books" — the canonical anti-join. Build the inner
// queryset first so its compile() catches typos; embed via not_exists.
let no_books = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .compile()?;
let orphans = Author::objects()
    .where_raw(not_exists(no_books))
    .fetch(&pool).await?;

// "Authors who have a published book of more than 100 pages" — the
// inner predicate combines a correlation (outer_ref) with literal
// filters in the same WHERE.
let inner = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .where_(Book::status.eq("published"))
    .where_(Book::pages.gt(100_i64))
    .compile()?;
let long_writers = Author::objects()
    .where_raw(exists(inner))
    .fetch(&pool).await?;

// Compose EXISTS with an OR.
let inner = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .compile()?;
let featured = Author::objects()
    .where_raw(WhereExpr::Or(vec![
        Author::name.eq("Carol").into(),
        exists(inner),
    ]))
    .fetch(&pool).await?;
```

**La corrélation imbriquée fonctionne.** Un OuterRef à l'intérieur d'une sous-requête doublement imbriquée se résout vers la portée englobante *immédiate* — le writer maintient une pile de portées au fur et à mesure qu'il descend, donc `EXISTS (Book WHERE id = outer.id AND EXISTS (Comment WHERE book_id = outer.id))` résout le `outer.id` interne vers `Book.id`, pas vers le `Author.id` le plus externe. Utilisez `outer_ref(...)` deux fois si vous avez réellement besoin d'atteindre deux portées plus haut.

**Erreurs :**

- **`OuterRefOutsideSubquery`** — émettre `outer_ref("col")` au niveau supérieur (pas à l'intérieur d'un wrapper de sous-requête) est une erreur de programmation. Le writer la signale bruyamment avec le nom de la colonne pour repérer facilement le site d'appel.

**Avertissements :**

- **Restriction de projection pour `IN (SELECT …)`** : PG exige strictement que le SELECT interne projette exactement une colonne pour la forme `<col> IN (…)`. **Rustango** ne fournit pas encore de restriction de projection de type `.values("col")` (issue #62), donc le queryset interne projette toujours chaque colonne du modèle — ce qui fait que `in_subquery` ne fonctionne aujourd'hui qu'avec des tables dont le modèle n'a qu'une seule colonne. Pour le cas multi-colonnes, utilisez plutôt `exists(inner.where_(<outer col>.eq_expr(outer_ref(...))))` — cela a la même sémantique et ne dépend pas de la forme de projection.
- **Le `subquery(...)` scalaire exige un interne à une colonne-une ligne** : le SQL émis est `SET col = (SELECT …)` — si l'interne produit plus d'une ligne, la base lève une erreur au runtime. Contraignez via `.limit(1)` et soit restreignez la projection (une fois disponible), soit concevez l'interne autour d'un invariant d'unicité.
- **La validation à la compilation des sous-requêtes vit sur le queryset interne** : les fautes de frappe sur les colonnes remontent à l'appel `queryset.compile()?` interne, pas à `compile()` de la requête externe. Construisez d'abord l'interne et propagez `?`.

### Quand passer au SQL brut à la place

Les builders ci-dessus couvrent les cas courants. Pour ce qu'ils n'expriment pas encore — `Cast`, la recherche plein texte, les opérateurs de chemin JSON, les fonctions de hachage, la trigonométrie, les fonctions fenêtrées — voir la section [Échappatoire SQL brut](#raw-sql-escape-hatch) ci-dessous, ou attendez les issues de suivi qui étendent le même arbre d'expressions.

---

## Agrégations

Comptez, sommez, moyennez, et groupez des lignes. `.count()`, `.sum()`, `.avg()`, `.min()` et `.max()` retournent un seul nombre ; `.annotate(...)` combiné avec `.values(...)` construit des requêtes GROUP BY (l'`aggregate` / `annotate` de Django). Les résultats d'agrégation reviennent sous forme de `Vec<HashMap<String, SqlValue>>` plutôt que de structs typées, car la forme est dynamique.

```rust
use rustango::sql::CounterPool as _;

// COUNT
let n = Post::objects()
    .where_(Post::status.eq("published"))
    .count(&pool).await?;

// SUM / AVG / MIN / MAX — string column name; each returns Option<U>
// (None when the filtered result set is empty).
let total_views = Post::objects().sum::<i64>("view_count", &pool).await?;
let avg_views = Post::objects().avg::<f64>("view_count", &pool).await?;
let max_views = Post::objects().max::<i64>("view_count", &pool).await?;

// Annotate + GROUP BY (issue #75 — Django-shape auto-inference)
use rustango::core::aggregates::{count_all, sum};

// "Posts per author" — `.values()` lists the GROUP BY columns.
let by_author = Sale::objects()
    .values(&["author_id"])
    .annotate("n", count_all().into())
    .compile()?;
let rows = rustango::sql::fetch_aggregate_dict(&pool, &by_author).await?;
// rows: Vec<HashMap<String, SqlValue>> — { author_id: 1, n: 3 }, …
```

### Comment le GROUP BY est inféré

Vous n'avez presque jamais besoin d'écrire vous-même `GROUP BY` — **Rustango** l'infère à partir de la forme de la requête, tout comme Django. Vous n'appelez `.group_by(...)` que pour surcharger cette inférence. Le tableau montre ce que chaque forme produit :

| Forme | Builder | `GROUP BY` résultant |
|---|---|---|
| **2 — values + agrégat** | `.values(&["author_id"]).annotate("n", count_all().into())` | `GROUP BY "author_id"` |
| **3 — agrégat nu** | `.annotate("n", count_all().into())` | `GROUP BY` chaque colonne scalaire non agrégée du modèle |
| **Fenêtré uniquement** | `.aggregate().annotate("rn", row_number()…)` | (pas de `GROUP BY` — les fonctions fenêtrées sont par ligne) |
| **Surcharge explicite** | `.aggregate().group_by("month").annotate(...)` | `GROUP BY "month"` — l'explicite gagne |

Le classificateur `AggregateExpr::is_aggregating()` distingue les variantes qui réduisent les lignes (`Count` / `Sum` / `Avg` / `Max` / `Min` / `CountDistinct` / `StdDev*` / `Variance*` — plus les wrappers récursifs `Filtered` / `Coalesced`) de `Window`, qui est par ligne. Seules les variantes agrégeantes déclenchent l'inférence de la Forme 3.

```rust
use rustango::core::aggregates::{count_all, sum};

// Shape 2 — "monthly revenue per author".
Sale::objects()
    .where_(Sale::status.eq("paid"))
    .values(&["author_id", "month"])
    .annotate("total", sum("amount").into())
    .compile()?;
// → SELECT "author_id", "month", SUM("amount")::bigint AS "total"
//   FROM "sale" WHERE "status" = $1
//   GROUP BY "author_id", "month"

// Shape 3 — a bare .annotate() with no .values(): rustango adds every
// non-aggregate scalar column of the model to the GROUP BY.
Post::objects()
    .annotate("n", count_all().into())
    .compile()?;
// → SELECT <every Post column>, COUNT(*) AS "n"
//   FROM "post" GROUP BY <every Post column>
```

**Avertissement sur la projection pure.** `.values(cols)` *seul* (sans annotation d'agrégat) n'est **pas** pris en charge en v0.40 — `compile()` retourne `QueryError::ValuesRequiresAggregate`. La projection pure sous forme de dicts nécessite un chemin de writer séparé (c'est un SELECT sans GROUP BY, décodé en `Vec<HashMap>`) et est prévue pour un suivi. Pour l'instant, utilisez le `QuerySet::fetch(...)` typé pour lire des lignes entières.

### Agrégats conditionnels & statistiques

Comptez ou sommez seulement les lignes qui correspondent à une condition, fournissez une valeur de repli pour les résultats vides, et calculez l'écart-type / la variance. Ils reflètent le `Count('id', filter=...)`, le `Sum('price', default=0)` et le `StdDev` de Django. Enchaînez `.filter(...)` et `.default(...)` sur n'importe quel builder d'agrégat.

```rust
use rustango::core::aggregates::{avg, count, count_all, stddev, sum};
use rustango::core::Column as _;

let rows = Post::objects()
    .aggregate()
    // COUNT(*) FILTER (WHERE is_active AND status = 'published')
    .annotate(
        "active_published",
        count_all()
            .filter(Post::is_active.eq(true).and(Post::status.eq("published")))
            .into(),
    )
    // COALESCE(SUM(price) FILTER (WHERE status = 'published'), 0)
    //   — returns 0 instead of NULL when the queryset is empty.
    .annotate(
        "revenue_or_zero",
        sum("price")
            .filter(Post::status.eq("published"))
            .default(0_i64)
            .into(),
    )
    .annotate("avg_pages", avg("pages").into())
    .annotate("page_stddev", stddev("pages").into())
    .compile()?;
let result = rustango::sql::fetch_aggregate_dict(&pool, &rows).await?;
```

**Builders** dans `rustango::core::aggregates` :

| Builder | SQL |
|---|---|
| `count(col)` | `COUNT(col)` |
| `count_all()` | `COUNT(*)` |
| `count_distinct(col)` | `COUNT(DISTINCT col)` |
| `sum(col)` / `avg(col)` / `max(col)` / `min(col)` | l'habituel |
| `stddev(col)` / `stddev_pop(col)` | `STDDEV_SAMP` / `STDDEV_POP` |
| `variance(col)` / `variance_pop(col)` | `VAR_SAMP` / `VAR_POP` |

Chacun retourne un `AggregateBuilder` avec deux modificateurs chaînables :

- `.filter(predicate)` — enveloppe dans `FILTER (WHERE predicate)`. Le prédicat est n'importe quel `WhereExpr` (typé via `.eq()` / `.and()` / brut via `WhereExpr::Or(...)`), il se compose donc comme un WHERE normal.
- `.default(value)` — enveloppe dans `COALESCE(..., value)` afin qu'un queryset vide retourne la valeur par défaut au lieu de `NULL`.

En chaînant les deux, `Coalesced` enveloppe `Filtered` : `COALESCE(SUM(col) FILTER (WHERE p), 0)`. L'ordre de chaînage n'a pas d'importance — `.filter(p).default(0)` et `.default(0).filter(p)` produisent la même IR.

**Émission tri-dialecte :**

| Fonctionnalité | PG | MySQL | SQLite |
|---|---|---|---|
| `Count` / `Sum` / `Avg` / `Max` / `Min` / `CountDistinct` | ✓ | ✓ | ✓ |
| `StdDev` / `StdDevPop` / `Variance` / `VariancePop` | ✓ | ✓ (8.0+) | ✗ `SqlError::AggregateNotSupported` |
| `.filter(...)` — `FILTER (WHERE …)` natif | ✓ | ✗ réécrit | ✓ (3.30+) |
| `.filter(...)` — repli `CASE WHEN` | — | ✓ `<agg>(CASE WHEN … THEN <arg> END)` | — |
| `.default(...)` — `COALESCE` | ✓ | ✓ | ✓ |

Le writer applique la conversion int/float du dialecte (`::bigint`, `CAST(... AS SIGNED)`, etc.) autour de toute l'expression `FILTER` — `SUM(col)::bigint FILTER (...)` est une erreur d'analyse syntaxique sur PG, donc la forme émise est `(SUM(col) FILTER (...))::bigint`. Même forme pour `STDDEV_SAMP` / `VAR_SAMP` (ils retournent NUMERIC sur PG pour une entrée bigint).

**SQLite + StdDev/Variance :** SQLite n'a pas d'agrégats statistiques intégrés, donc le writer rejette avec `SqlError::AggregateNotSupported { aggregate, dialect: "sqlite" }`. Calculez la formule de variance côté application si des statistiques portables sont nécessaires (même posture que Django).

### Fonctions fenêtrées

Calculez des totaux courants, des classements, et des deltas ligne-à-ligne sans réduire les lignes — le `Window(expression, partition_by=, order_by=, frame=)` de Django. Huit fonctions (`row_number`, `rank`, `dense_rank`, `lag`, `lead`, `first_value`, `last_value`, `ntile`) plus les frames ROWS/RANGE. Chaque backend supporté par **Rustango** (PG ≥ 9.0, MySQL ≥ 8.0, SQLite ≥ 3.25) fournit une syntaxe `OVER (…)` native, donc l'émission est uniforme.

```rust
use rustango::core::aggregates::max;
use rustango::core::window::{lag, rank, row_number};

// "Rank users by score within each tenant" — the canonical
// integration target.
let q = User::objects()
    .aggregate()
    .group_by("id")
    .group_by("tenant_id")
    .group_by("name")
    .group_by("score")
    .annotate("_a", max("id").into())  // satisfies GROUP BY on the projection
    .annotate(
        "tenant_rank",
        rank().partition_by("tenant_id").order_by(&[("score", true)]).into(),
    )
    .order_by(&[("tenant_id", false), ("score", true)])
    .compile()?;
let rows = rustango::sql::fetch_aggregate_dict(&pool, &q).await?;

// Day-over-day delta via LAG with a default for the first row.
let q = Event::objects()
    .aggregate()
    .group_by("id")
    .group_by("day")
    .group_by("count")
    .annotate("_a", max("id").into())
    .annotate(
        "prev_count",
        lag("count", 1, Some(SqlValue::I64(0)))
            .partition_by("user_id")
            .order_by(&[("day", false)])
            .into(),
    )
    .compile()?;

// Stable row index per group for "show me row N" pagination.
let q = Post::objects()
    .aggregate()
    .group_by("id")
    .group_by("status")
    .group_by("created_at")
    .annotate("_a", max("id").into())
    .annotate(
        "rn",
        row_number()
            .partition_by("status")
            .order_by(&[("created_at", true)])
            .into(),
    )
    .compile()?;
```

**Builders** dans `rustango::core::window` :

| Builder | SQL | Arguments |
|---|---|---|
| `row_number()` | `ROW_NUMBER()` | — |
| `rank()` | `RANK()` | — |
| `dense_rank()` | `DENSE_RANK()` | — |
| `ntile(buckets)` | `NTILE(buckets)` | nombre de buckets |
| `lag(col, offset, default)` | `LAG(col, offset, default?)` | colonne + offset + défaut optionnel |
| `lead(col, offset, default)` | `LEAD(col, offset, default?)` | colonne + offset + défaut optionnel |
| `first_value(col)` | `FIRST_VALUE(col)` | colonne |
| `last_value(col)` | `LAST_VALUE(col)` | colonne |

Chacun retourne un `WindowBuilder` avec trois modificateurs chaînables :

- `.partition_by("col")` — ajoute une colonne `PARTITION BY`. Appelez-le plusieurs fois pour un partitionnement multi-colonnes.
- `.order_by(&[("col", desc)])` — ajoute des colonnes `ORDER BY` (`desc = true` → DESC).
- `.frame(WindowFrame { kind, start, end })` — définit la clause de frame `ROWS`/`RANGE` optionnelle. `FrameBoundary::UnboundedPreceding` / `Preceding(n)` / `CurrentRow` / `Following(n)` / `UnboundedFollowing`.

Le builder se convertit via `Into<AggregateExpr>` afin que les fonctions fenêtrées se composent avec `annotate()`. `Into<Expr>` est aussi implémenté (le slot au niveau IR pour les expressions fenêtrées), mais **chaque backend supporté par Rustango restreint les fonctions fenêtrées à la liste `SELECT` et à la clause `ORDER BY` d'une requête** — elles ne peuvent pas apparaître dans `WHERE` / `HAVING` / `GROUP BY` / `UPDATE SET` / `JOIN ON` / `RETURNING`. Le writer ne verrouille pas l'émission sur ce point, donc `set_expr("col", row_number())` compile en SQL que la base rejette à l'exécution. Construisez les expressions fenêtrées via `annotate()` ; passez par une sous-requête si vous devez alimenter un résultat de fonction fenêtrée dans un filtre WHERE ou un UPDATE.

**Le piège de la frame par défaut de `LAST_VALUE` :**

Un `last_value(col).order_by(&[("x", false)])` nu émet `LAST_VALUE("col") OVER (ORDER BY "x")` et semble devoir retourner le dernier `col` de la partition. Ce n'est pas le cas — la frame de fenêtre *par défaut* de SQL est `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`, donc `LAST_VALUE` retourne la valeur **de la ligne courante**, pas de la dernière ligne de la partition. Pour obtenir le comportement intuitif « dernière ligne de la partition », passez une frame illimitée explicite :

```rust
use rustango::core::{FrameBoundary, FrameKind, WindowFrame};

last_value("score")
    .partition_by("tenant_id")
    .order_by(&[("created_at", true)])
    .frame(WindowFrame {
        kind: FrameKind::Rows,
        start: FrameBoundary::UnboundedPreceding,
        end: Some(FrameBoundary::UnboundedFollowing),
    })
```

`first_value` n'a pas ce piège — le début de la frame par défaut correspond au début de la partition, la réponse intuitive tombe donc naturellement.

**Avertissement sur annotate (jusqu'à l'arrivée de l'issue #75) :**

`annotate()` vit sur le builder d'agrégat qui exige `GROUP BY` pour projeter des colonnes scalaires par ligne à côté des agrégats. Pour projeter des résultats de fonction fenêtrée à côté de colonnes de ligne aujourd'hui, listez chaque colonne de ligne que vous voulez retourner dans des appels `.group_by(...)` et utilisez `annotate("_a", max("id").into())` comme espace réservé no-op pour garder l'identité de ligne stable. L'issue #75 (auto-inférence de GROUP BY) apportera une forme plus propre.

**Clauses de frame :**

```rust
use rustango::core::{FrameBoundary, FrameKind, WindowFrame};

// Running total over the last 7 rows:
let frame = WindowFrame {
    kind: FrameKind::Rows,
    start: FrameBoundary::Preceding(6),
    end: Some(FrameBoundary::CurrentRow),
};

// Centered 11-row window:
let frame = WindowFrame {
    kind: FrameKind::Rows,
    start: FrameBoundary::Preceding(5),
    end: Some(FrameBoundary::Following(5)),
};
```

**Émission tri-dialecte :**

`<fn>(args) OVER (PARTITION BY … ORDER BY … [frame])` fait partie de la norme SQL — identique sur PG, MySQL 8+ et SQLite 3.25+. La seule bizarrerie : `LAG` / `LEAD` / `NTILE` exigent des offsets/buckets entiers sur PG (les lier comme un paramètre bigint `$N` provoque `function lag(bigint, bigint, bigint) does not exist`). Le writer intègre directement des littéraux entiers dans le SQL pour ces emplacements ; les arguments de valeur par défaut se lient normalement.

**Avertissements :**

- **`FILTER` + `Window` pas encore pris en charge** : combiner `.filter(...)` avec une fonction fenêtrée lève `SqlError::NestedAggregateWrapper { wrapper: "Filtered(Window)" }` — la syntaxe sous-jacente varie selon le type de fonction (PG autorise `agg_fn() FILTER (WHERE …) OVER (…)` pour les fonctions fenêtrées agrégeantes mais pas pour les fonctions de classement), et le writer n'a pas encore été enseigné cette répartition. Consigné pour un suivi si la demande se manifeste.
- **`PercentRank` / `CumeDist` / `NthValue`** ne sont pas dans la v1 — l'ensemble complet de Django est plus large. La v1 fournit les 8 variantes les plus utilisées ; les trois manquantes peuvent être ajoutées progressivement avec la même forme de builder.

### Filtrer sur des agrégats (HAVING)

Un appel `.filter(...)` après `.annotate(...)` atterrit soit dans `WHERE` soit dans `HAVING`, selon que le nom correspond à un alias d'agrégat — exactement le comportement de Django. Ainsi, filtrer sur une vraie colonne ajoute un `WHERE`, tandis que filtrer sur une annotation comme `post_count` ajoute un `HAVING` :

```rust
use rustango::core::aggregates::count_all;
use rustango::core::Op;

// "Authors with > 10 published posts" — the canonical pattern.
// status='published' is on the model       → routes to WHERE.
// post_count > 10 references the annotation → routes to HAVING.
let q = Post::objects()
    .aggregate()
    .group_by("author_id")
    .annotate("post_count", count_all().into())
    .filter("status",     Op::Eq, "published")
    .filter("post_count", Op::Gt, 10_i64)
    .compile()?;
let rows = rustango::sql::fetch_aggregate_dict(&pool, &q).await?;
```

Émet, sur PG :

```sql
SELECT "author_id", COUNT(*) AS "post_count"
FROM "post"
WHERE "status" = $1
GROUP BY "author_id"
HAVING COUNT(*) > $2
```

**L'expression d'agrégat est remontée dans HAVING, pas l'alias du SELECT.** PG interdit strictement les alias dans HAVING (seule l'expression se résout) ; MySQL + SQLite sont plus permissifs. Le writer émet la forme remontée de manière uniforme sur les trois afin que la même requête fonctionne partout.

**L'ordre de la chaîne compte en v1.** Appelez `.annotate(alias, ...)` AVANT le `.filter(alias, ...)` correspondant. Si l'ordre est inversé, `filter()` consulte un registre d'annotations vide et route vers `WHERE` — et le validateur `resolve_pending` fait remonter `UnknownField` à `compile()` parce que l'alias n'est pas une vraie colonne du modèle. Django diffère cette résolution au moment de la construction de la requête ; un suivi v0.50 pourrait adopter cette posture.

**Lacune du validateur (correspond à la posture existante des agrégats)** : les prédicats HAVING routés par alias ne parcourent pas le schéma du modèle. Les alias mal orthographiés remontent à la base de données, pas à `compile()`. Même lacune que `Sum("typo_col")` — préexistante et orthogonale.

**Opérateurs pris en charge sur `.filter()` routé par alias** (issue #87) : l'ensemble des comparaisons binaires (`Op::Eq` / `Ne` / `Lt` / `Lte` / `Gt` / `Gte`) **plus** les prédicats standard SQL-92 qui se composent avec un LHS d'agrégat de manière uniforme sur chaque backend — `Op::In` / `NotIn`, `Between`, `IsNull`, `Like` / `NotLike`, `ILike` / `NotILike`. Chacun émet la forme prévisible :

```rust
use rustango::core::{Op, SqlValue};

// HAVING COUNT(*) IN ($1, $2, $3)
Post::objects()
    .aggregate()
    .group_by("author_id")
    .annotate("post_count", count_all().into())
    .filter("post_count", Op::In, SqlValue::List(vec![5_i64.into(), 10_i64.into(), 20_i64.into()]))
    .compile()?;

// HAVING COUNT(*) BETWEEN $1 AND $2
.filter("post_count", Op::Between, SqlValue::List(vec![5_i64.into(), 10_i64.into()]))

// HAVING COUNT(*) IS NULL  /  IS NOT NULL  (bool: true = IS NULL)
.filter("post_count", Op::IsNull, SqlValue::Bool(false))

// HAVING MAX("name") LIKE $1  /  ILIKE $1 (PG) / LOWER(MAX("name")) LIKE LOWER(?) (MySQL/SQLite)
.filter("max_name", Op::ILike, "SMITH%")
```

Les opérateurs restants — la famille JSON (`JsonContains` / `JsonContainedBy` / `JsonHasKey` / `JsonHasAnyKey` / `JsonHasAllKeys`) et l'égalité null-safe (`IsDistinctFrom` / `IsNotDistinctFrom`) — nécessitent encore des writers spécifiques au dialecte prenant un `&str` pour le LHS, ils sont donc rejetés à `compile()` avec `QueryError::HavingOpNotSupported { alias, op }`. Pour ceux-là, passez par la forme typée `.having(<TypedExpr>)` avec un prédicat pré-construit.

**Gonflement du vecteur de paramètres avec des agrégats non triviaux** : quand l'alias cible une annotation `Filtered { Count, filter: pred }` ou `Coalesced { Sum, default: 0 }`, le writer remonte l'**expression d'agrégat entière** dans HAVING — y compris ses prédicats internes et ses valeurs par défaut. Leurs littéraux liés obtiennent de nouveaux emplacements de paramètres dans HAVING, séparés de l'émission dans la liste SELECT. Concrètement :

```text
SELECT … COUNT(*) FILTER (WHERE "status" = $1) AS "published_count" …
HAVING COUNT(*) FILTER (WHERE "status" = $2) > $3
              -- "published" bound twice (once at $1, once at $2)
```

La sémantique SQL est inchangée (les mêmes nombres de lignes reviennent), mais `stmt.params.len()` croît à chaque appel `.filter()` ciblant un alias non trivial. Pour les alias `COUNT(*)` (aucun littéral interne), le gonflement est nul. À documenter si votre suite de tests fixe des comptes de paramètres.

---

## Jointures & préchargement des lignes liées

Récupérez la cible d'une clé étrangère en même temps que la ligne principale, en une seule requête, afin de ne pas tirer une requête supplémentaire par ligne (le problème N+1). `.select_related("author")` est le `select_related` de Django / le chargement anticipé d'Eloquent. Un champ `ForeignKey<T>` arrive alors déjà rempli sans avoir besoin d'une recherche séparée.

```rust
let posts = Post::objects()
    .select_related("author")              // JOIN posts.author -> authors.id
    .fetch(&pool).await?;

for post in &posts {
    let author = post.author.value().unwrap();   // already loaded, no DB round-trip
    println!("{} by {}", post.title, author.name);
}
```

`select_related` résout les champs FK au moment de la compilation du queryset. Le champ `ForeignKey<T>` sur le parent passe de `Unloaded(pk)` à `Loaded { pk, value }`.

Pour les FK inversées (parent.enfants), utilisez la méthode `_set` générée par la macro :

```rust
let author_posts = author.post_set(&pool).await?;
```

### Jointures personnalisées

Quand la jointure n'est pas pilotée par une clé étrangère — un prédicat personnalisé, une jointure non-équi, INNER au lieu de LEFT, une auto-jointure, ou une jointure sur une colonne non-PK — utilisez `.join(Join { … })`. Son champ `on` accepte n'importe quel `WhereExpr`, donc `and()` / `or()` / `Not` / appels de fonction / colonne-vs-colonne / filtres littéraux se composent tous librement.

```rust
use rustango::core::joins::aliased;
use rustango::core::{Join, JoinKind, Op, WhereExpr};

// "Posts that have at least one APPROVED comment" — INNER JOIN with
// an extra predicate inside the ON. Posts with no approved comment
// drop out; LEFT JOIN would keep them.
Post::objects()
    .join(Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::And(vec![
            // Column-on-column condition — both sides aliased.
            WhereExpr::ExprCompare {
                lhs: aliased("c", "post_id"),
                op: Op::Eq,
                rhs: aliased("post", "id"),
            },
            // Bare Filter — unqualified columns inside `on` resolve
            // to the joined alias ("c"), so this becomes
            // `"c"."is_approved" = $N`.
            Comment::is_approved.eq(true).into(),
        ]),
        project: vec![],
    })
    .fetch(&pool).await?;
```

**Règles de qualification des colonnes à l'intérieur de `on` :**

- **Les colonnes `Filter` / `ColumnFilter` nues + les références de colonne `F()`** se résolvent vers l'alias joint (`<alias>` que vous avez passé). C'est la lecture naturelle car la majeure partie d'un prédicat ON concerne la table jointe.
- **`aliased(alias, col)`** émet explicitement `"<alias>"."<col>"` — utilisez-le pour des références croisées vers la table externe (`aliased("<outer_table>", "<col>")`) ou vers un alias précédemment joint.
- **`WhereExpr::ExprCompare { lhs, op, rhs }`** est la forme adéquate pour des comparaisons colonne-vs-colonne entre tables, puisque les deux côtés acceptent n'importe quel `Expr`.

> ⚠️ **PATTERN DANGEREUX — filtres typés du modèle EXTERNE à l'intérieur de `on`.**
> `Post::status.eq("draft").into()` produit un `WhereExpr::Predicate(Filter { column: "status", ... })` et **perd le tag du modèle `Post`** à la frontière `Into<WhereExpr>`. La règle d'auto-qualification ci-dessus route alors ce filtre par erreur vers l'**alias joint**, pas vers `Post`. Vous obtenez `"<joined_alias>"."status" = $N` — la mauvaise table — et le compilateur ne peut pas le détecter. **Utilisez [`joins::col_filter`] pour les prédicats portant sur une colonne dont la table n'est pas l'alias par défaut de la jointure :**
>
> ```rust
> use rustango::core::joins::{aliased, col_filter};
> use rustango::core::Op;
>
> // SAFE: explicit alias on the LHS.
> col_filter("post", "status", Op::Eq, "draft")
> ```
>
> Réservez les filtres typés nus (`Comment::is_approved.eq(true).into()`) aux colonnes du modèle JOINT uniquement — jamais pour les colonnes du modèle externe.

**Support tri-dialecte de JoinKind :**

| Type | PG | MySQL | SQLite |
|---|---|---|---|
| `Inner` | ✓ | ✓ | ✓ |
| `Left` (par défaut) | ✓ | ✓ | ✓ |
| `Right` | ✓ | ✓ | ✗ `SqlError::JoinKindNotSupported` |
| `Full` | ✓ | ✗ | ✗ |

`Right` est facile à contourner — échangez les opérandes et utilisez `Left`. `Full` sur MySQL est habituellement émulé avec `(LEFT JOIN) UNION (RIGHT JOIN)` si vous en avez vraiment besoin.

**Autres erreurs à l'émission :**

- **Prédicat `on` vide** (`WhereExpr::And(vec![])` ou aucun `ExprCompare`) est rejeté avec `SqlError::EmptyJoinOnCondition`. SQL exige au moins un prédicat booléen à l'intérieur de `ON` ; le raccourci auto-`true` du WHERE de premier niveau ne s'applique pas ici.

**`project` est actuellement une donnée morte sur les jointures ad hoc.**

Le champ `Join.project` indique au writer d'émettre des colonnes `<alias>"."<col>" AS "<alias>__<col>"` dans la liste SELECT. Aujourd'hui, seul `select_related` décode réellement ces colonnes (via le décodeur de ligne complète de la cible FK). Les jointures ad hoc émettent les colonnes mais le décodeur `Vec<MainModel>` les ignore, donc renseigner `project` sur une jointure ad hoc ne fait qu'ajouter des octets sur le fil. Laissez-le à `vec![]` jusqu'à ce que la restriction de projection + le décodage de tuples arrivent.

**Quand utiliser des jointures ad hoc :**

| Besoin | Outil |
|---|---|
| Récupérer les lignes liées avec la ligne principale | `select_related` (forme Django) |
| Filtrer les lignes principales par un prédicat de table liée | `exists(...)` / `not_exists(...)` |
| Filtrer via INNER au lieu de LEFT, ou avec des prédicats ON supplémentaires | `.join(...)` |
| Auto-jointure (par exemple `employee.manager_id = manager.id`) | `.join(...)` |
| Anti-jointure (lignes de A sans AUCUNE correspondance dans B) | `not_exists(...)` |

`select_related` reste l'outil approprié quand la jointure consiste à « suivre cette FK et projeter toutes ses colonnes ». Les jointures ad hoc sont l'échappatoire quand vous avez besoin : d'une clé de jointure non-FK, d'INNER au lieu de LEFT, d'un prédicat supplémentaire à l'intérieur du ON, ou d'une auto-jointure.

[`joins::col_filter`]: https://docs.rs/rustango/latest/rustango/core/joins/fn.col_filter.html

[`WhereExpr`]: https://docs.rs/rustango/latest/rustango/core/enum.WhereExpr.html

---

## Sauvegarder seulement certains champs

Écrivez uniquement les champs modifiés au lieu de toutes les colonnes — le `save(update_fields=[...])` de Django. Une sauvegarde normale réécrit toutes les colonnes non-PK ; `save_partial(&[...], &pool)` ne réécrit que celles que vous nommez.

```rust
let mut post = Post::objects().fetch(&pool).await?.pop().unwrap();
post.title = "new title".into();
post.save_partial(&["title"], &pool).await?;  // SET "title" = $1
                                                  // — leaves body, status, views untouched
```

Deux motivations :

* **Performance.** Les lignes larges avec des colonnes `TEXT` / `JSON` / `bytea` coûtent à re-lier et à réécrire à chaque `save()` même quand un seul champ a été modifié. `save_partial` limite la clause `SET` exactement à ce qui a changé.
* **Sécurité de la concurrence.** Quand deux écrivains divergent après une lecture partagée, le perdant écrase silencieusement les modifications du gagnant sur les champs qu'il n'a pas touchés. Ne nommer que le champ réellement modifié préserve le travail de l'autre écrivain partout ailleurs.

```rust
// Writer A — flips title.
a.title = "from-A".into();
a.save_partial(&["title"], &pool).await?;

// Writer B — started from the same read, flips status.
// B's local `title` is stale, but it's not in the list, so A's
// write survives.
b.status = "from-B".into();
b.save_partial(&["status"], &pool).await?;
```

**Les noms de champs sont des champs de struct côté Rust**, pas des colonnes SQL — `["author_id"]` (pas `["author"]` pour un champ typé FK). Les noms de champs inconnus retournent `ExecError::Query(QueryError::UnknownField)`. Une liste vide est un no-op (retourne `Ok(())` et journalise un `tracing::warn!`), ce qui correspond à la sémantique « rien à faire » de Django. Les modèles audités (`#[rustango(audit(...))]`) restreignent l'instantané du journal d'audit au même ensemble de colonnes — le journal reflète exactement ce qui a été écrit.

**Note sur les PK auto.** `save_partial` est UPDATE uniquement ; l'appeler sur une PK `Auto::Unset` est une erreur utilisateur (utilisez `insert_pool` / `save_pool` pour ce cas). Contrairement à `save_pool` qui dispatche automatiquement `Unset → insert_pool`, cette méthode suppose que vous avez déjà inséré la ligne.

### Liste de champs vérifiée à la compilation

La forme à clés en chaînes ci-dessus convient aux listes de champs dynamiques (formulaires admin, payloads d'API). Quand la liste est fixe dans votre code, `save_partial_typed((Post::title, ...), &pool)` détecte les champs mal orthographiés ou renommés à la **compilation** plutôt qu'au runtime :

```rust
post.save_partial_typed((Post::title, Post::slug), &pool).await?;
//                       ──────────  ──────────
//                       title_col   slug_col   ← distinct ZSTs
```

Chaque `Post::<field>` est son propre type de taille zéro — une slice homogène (`&[Post::title, Post::slug]`) ne compile pas en Rust, donc l'API prend un **tuple** à la place. Les appels à un seul champ utilisent l'idiome de la virgule finale : `(Post::title,)`. Les tuples sont pris en charge de l'arité 1 jusqu'à 12 — au-delà, retombez sur `save_partial(&[&str], _)`.

Les tuples entre modèles différents sont une **erreur de compilation** — `(Post::title, Author::name)` échoue à la contrainte de trait `TypedFieldList<Post>` car `Author::name` a `Column::Model = Author`. C'est l'apport principal par rapport à la forme à clés en chaînes : les refactorisations de renommage sur un nom de colonne remontent au site d'appel typé, pas au runtime.

Se réduit en interne à `save_partial` — même restriction d'audit, même contrainte `Auto::Unset`, même sémantique de no-op sur liste vide.

---

## Opérations en masse

> **Piège — les opérations en masse contournent les hooks par ligne.** `bulk_insert`, `.update().execute()` d'un queryset,
> et `.delete()` s'exécutent comme du SQL orienté ensemble : ils ne déclenchent **pas**
> les signaux, n'écrivent pas la piste d'audit, ne passent pas par la suppression logique, et n'exécutent pas
> de validation par ligne. Utilisez-les pour la vitesse ; retombez sur le `save()` / `delete()` par ligne
> quand vous avez besoin de ces effets de bord.

Insérez, mettez à jour, ou supprimez de nombreuses lignes en une seule instruction plutôt qu'une par ligne — le `bulk_create`, `QuerySet.update()` et `QuerySet.delete()` de Django. L'import `as _` place les méthodes d'un trait dans la portée sans nommer le trait directement.

```rust
// Bulk INSERT — rows FIRST (a `&mut [Self]`), executor/pool second.
let mut rows = [p1, p2, p3];
Post::bulk_insert_on(&mut rows, &pool).await?;

// Bulk UPDATE — applies the same set to every matched row. `.set`
// takes a string column name.
Post::objects()
    .where_(Post::status.eq("draft"))
    .where_(Post::created_at.lt(thirty_days_ago))
    .update()
    .set("status", "archived")
    .execute_on(&pool).await?;

// Bulk DELETE
Post::objects()
    .where_(Post::deleted_at.is_not_null())
    .delete_on(&pool).await?;
```

---

## Insertion ou mise à jour (upsert)

Insérez une ligne, ou mettez-la à jour si une ligne avec la même clé existe déjà — le `update_or_create` de Django / l'`upsert` de Rails. Cela émet le `ON CONFLICT … DO UPDATE` natif de la base.

Le `.upsert_on(executor)` sur une seule instance entre en conflit sur la **clé primaire** : avec une PK `Auto::Unset`, le serveur attribue une nouvelle clé (équivalent à `insert`) ; avec une PK `Auto::Set`, la ligne est insérée si absente ou toutes les colonnes non-PK sont écrasées si présente.

```rust
// Upsert on the PK — INSERT, or UPDATE every non-PK column if the
// PK already exists.
post.upsert_on(&pool).await?;
```

Pour faire un upsert sur une clé unique arbitraire (le `bulk_create(update_conflicts=True, unique_fields=…, update_fields=…)` de Django), utilisez le helper en masse — il prend les lignes, les colonnes cibles du conflit, les colonnes à mettre à jour en cas de conflit, et le pool EN DERNIER :

```rust
// ON CONFLICT (external_id) DO UPDATE SET title = EXCLUDED.title
Post::bulk_upsert_pool(
    &[post],
    &["external_id"],          // conflict target (unique key)
    &["title"],                // columns to overwrite on conflict
    &pool,
).await?;
```

---

## Transactions

> **Piège — ne mélangez pas les appels `&pool` à l'intérieur d'une transaction.** Chaque appel
> entre `pool.begin()` et `commit` doit cibler le handle de transaction
> (`&mut *tx`). Un `&pool` / `fetch()` / `save_on(&pool)` isolé récupère une
> *deuxième* connexion et peut créer un deadlock du pool sous charge. Faites passer la `tx`
> partout, ou utilisez `rustango::sql::atomic`.

Exécutez plusieurs écritures comme une unité qui réussit entièrement ou est entièrement annulée — le `transaction.atomic()` de Django. Ouvrez-en une avec `pool.begin()` et exécutez chaque instruction sur la connexion de la transaction via les méthodes `_on` (`fetch_on`, `save_on`), afin que le travail atterrisse sur la transaction en cours plutôt que sur une connexion fraîchement tirée du pool.

```rust
let mut tx = pool.begin().await?;

let mut a = Account::objects()
    .where_(Account::id.eq(1))
    .fetch_on(&mut *tx).await?
    .pop().unwrap();
let mut b = Account::objects()
    .where_(Account::id.eq(2))
    .fetch_on(&mut *tx).await?
    .pop().unwrap();

a.balance -= 100;
b.balance += 100;
a.save_on(&mut *tx).await?;
b.save_on(&mut *tx).await?;

tx.commit().await?;
```

Abandonnez la `tx` sans appeler `commit()` (par exemple lors d'un retour anticipé par `?`) et la transaction est annulée (rollback). Pour un hook après-commit (le `transaction.on_commit` de Django), utilisez le helper de style closure `rustango::sql::atomic(&pool, |tx| Box::pin(async move { … }))`, qui valide automatiquement (auto-commit) en cas de `Ok` et annule automatiquement en cas de `Err`.

---

## Relations plusieurs-à-plusieurs

Reliez de nombreuses lignes à de nombreuses autres via une table de jonction — le `ManyToManyField` de Django. Déclarez la relation sur le modèle, puis utilisez l'accesseur généré pour ajouter, retirer, définir, ou lister les ids liés.

```rust
#[rustango(
    table = "posts",
    m2m(name = "tags", to = "tags", through = "post_tags",
        src = "post_id", dst = "tag_id"),
)]
pub struct Post { ... }
```

Utilisez l'accesseur auto-généré :

```rust
let tag_ids: Vec<i64> = post.tags_m2m().all(&pool).await?;
post.tags_m2m().add(42, &pool).await?;
post.tags_m2m().remove(42, &pool).await?;
post.tags_m2m().set(&[1, 2, 3], &pool).await?;        // replace all
post.tags_m2m().clear(&pool).await?;
let has = post.tags_m2m().contains(42, &pool).await?;
```

La table de jonction (`post_tags`) est auto-créée par `make_migrations` avec une PK composite + deux FK `ON DELETE CASCADE`. Actuellement, la jonction n'a que les deux colonnes FK — pour des colonnes supplémentaires (added_by, order, created_at), vous devrez définir un Model séparé et traverser manuellement jusqu'à ce que le « modèle through personnalisé » soit disponible.

---

## JSON / JSONB

Stockez et interrogez un document JSON dans une colonne — le `JSONField` de Django. Déclarez le champ comme `serde_json::Value` (le type JSON générique), puis interrogez-le avec `json_contains` ou un filtre de chemin.

```rust
#[derive(Model)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(default = r#"'{}'::jsonb"#)]
    pub data: serde_json::Value,
}
```

Interroger le contenu JSON :

```rust
use rustango::core::{Expr, Op, SqlValue, WhereExpr};
use rustango::core::funcs::json_path;
use rustango::core::F;

let with_email = Event::objects()
    .where_(Event::data.json_contains(serde_json::json!({"email_set": true})))
    .fetch(&pool).await?;

// Path extract — `json_path(F("data"), &["type"], true)` builds the
// `data ->> 'type'` text-extract LHS; compare it via `where_raw`.
let typed = Event::objects()
    .where_raw(WhereExpr::ExprCompare {
        lhs: json_path(F("data"), &["type"], true),
        op: Op::Eq,
        rhs: Expr::Literal(SqlValue::String("user.created".into())),
    })
    .fetch(&pool).await?;
```

Lisez/écrivez des types Rust via `serde_json::from_value` / `to_value`.

---

## Suppression logique

Marquez une ligne comme supprimée en définissant un timestamp au lieu de la retirer — comme `django-safedelete` de Django ou `SoftDeletes` de Laravel. Marquez la colonne de timestamp avec l'attribut `#[rustango(soft_delete)]` (une annotation de derive qui indique à la macro comment traiter le champ) :

```rust
#[derive(Model)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}
```

Utilisation :

```rust
post.soft_delete_on(&pool).await?;     // sets deleted_at = NOW()
post.restore_on(&pool).await?;          // sets deleted_at = NULL

// Default queries DO include soft-deleted rows. Filter explicitly:
let live = Post::objects().where_(Post::deleted_at.is_null()).fetch(&pool).await?;
```

Le bouton « Supprimer » de l'admin route automatiquement vers `soft_delete_on` pour tout modèle possédant la colonne. Le filtre automatique (exclusion par défaut) est sur la feuille de route de la v0.21.

---

## Piste d'audit

Enregistrez qui a modifié quels champs et quand, automatiquement à chaque sauvegarde et suppression — comme `django-simple-history` de Django ou les paquets d'audit de Laravel. Annotez le modèle avec les champs à suivre :

```rust
#[derive(Model)]
#[rustango(audit(track = "title, body, status"))]
pub struct Post { ... }
```

Chaque sauvegarde/suppression écrit une ligne dans `rustango_audit_log` avec un diff JSONB `before / after` pour les champs listés. Définissez la source par requête :

```rust
use rustango::audit::{with_source, AuditSource};

with_source(
    AuditSource::User { id: user_id.to_string() },
    async {
        post.save_on(&pool).await
    },
).await?;
```

Le panneau d'historique par ligne de l'admin lit depuis cette table ; le flux inter-modèles se trouve sur `/__audit`.

Nettoyage :

```rust
rustango::audit::cleanup_older_than(&pool, 90).await?;       // delete > 90 days
rustango::audit::cleanup_keep_last_n(&pool, 50).await?;      // keep most recent 50/row

// CLI
manage audit-cleanup --days 90
manage audit-cleanup --keep-last 50 --tenant acme
```

---

## Échappatoire SQL brut

Passez au SQL écrit à la main quand le query builder ne peut pas exprimer ce dont vous avez besoin — le `Model.objects.raw()` / `connection.cursor()` de Django. Les macros `sqlx` exécutent une requête et décodent le résultat en un tuple, un `Model` typé, ou rien :

```rust
use rustango::sql::sqlx;

// Raw query → typed rows
let rows = sqlx::query_as::<_, (i64, String)>("SELECT id, title FROM posts WHERE views > $1 ORDER BY views DESC")
    .bind(1000)
    .fetch_all(&pool)
    .await?;

// Raw with model decoding
let posts: Vec<Post> = sqlx::query_as::<_, Post>("SELECT * FROM posts WHERE complicated_condition")
    .fetch_all(&pool)
    .await?;

// Raw without rows (DDL / DML)
sqlx::query("REINDEX TABLE posts").execute(&pool).await?;
```

Pour du SQL brut programmatique au sein de la couche de requête **Rustango** (tri-dialecte ; prend le SQL, un `Vec<SqlValue>` de valeurs liées, puis le pool EN DERNIER, et retourne `Vec<T>`) :

```rust
use rustango::sql::raw_query_pool;

let rows = raw_query_pool::<(i64,)>(
    "SELECT COUNT(*) FROM posts WHERE complicated",
    vec![],
    &pool,
).await?;
let count = rows.first().map(|r| r.0).unwrap_or(0);
```

---

## Chargement paresseux des FK

Une clé étrangère commence par ne détenir que l'id lié (`Unloaded`), et vous ne récupérez la ligne liée complète que lorsque vous la demandez — l'accès paresseux aux objets liés de Django. Faites un `match` sur la `ForeignKey` pour gérer les deux états, ou appelez `.get(&pool)` pour la charger à la demande. Pour un lot entier, utilisez `select_related` (ci-dessus) pour les précharger en une seule requête et éviter le fetch par ligne.

```rust
let mut post = Post::objects().find_or_fail(1, &pool).await?;

// FK starts Unloaded — just the PK. `Loaded` is a struct variant
// `{ pk, value }`; `value` is a `Box<Author>`.
match &post.author {
    ForeignKey::Unloaded(pk) => println!("author id = {pk}"),
    ForeignKey::Loaded { pk, value } => println!("author = {}", value.name),
}

// Force-load
let author = post.author.get(&pool).await?;          // fetches if Unloaded
```

Utilisez `select_related("author")` sur le queryset pour précharger un lot.

---

## Quatre façons de filtrer

Il existe quatre façons d'exprimer un filtre ; choisissez selon le contexte. Les colonnes typées sont vérifiées à la compilation et sont les mieux adaptées au code applicatif ; la forme en chaîne `field__lookup` est la syntaxe familière de Django pour l'admin et le CRUD générique ; `filter_op` sert lorsque vous détenez déjà un `Op` ; la chaîne de requête HTTP pilote l'API publique.

```rust
// 1. HTTP query string (set via ViewSet filter_fields)
//    GET /api/posts?author_id=42&status__ne=archived

// 2. Django-shape string lookup (the same `field__lookup` grammar your
//    URL parser uses, but inside Rust). Suffix decides the operator
//    and value-shape; bare key is exact-eq. Field name is validated
//    at `.compile()`.
Post::objects()
    .filter("status", "published")                 // exact-eq
    .filter("title__icontains", "rust")            // ILIKE %rust%
    .filter("views__gt", 100_i64);

// 3. Explicit operator (legacy 3-arg shape — when you want to pass
//    an Op directly without parsing a suffix)
Post::objects().filter_op("author_id", Op::Eq, SqlValue::I64(42));

// 4. Typed columns (compile-time field check; preferred in app code)
Post::objects().where_(Post::author_id.eq(42));
```

**Convention :** typé dans le code applicatif, forme Django dans l'admin / le CRUD générique, `filter_op` seulement quand vous avez déjà calculé un `Op` (par exemple depuis un parseur de requête), la chaîne de requête HTTP pour la surface d'API publique.

### Suffixes de lookup pris en charge

| Suffixe | Opérateur SQL | Forme de la valeur | Notes |
|---|---|---|---|
| *(aucun)* / `__exact` | `=` | scalaire | la clé nue est exact-eq |
| `__ne` | `<>` | scalaire | |
| `__gt` / `__gte` / `__lt` / `__lte` | `>` `>=` `<` `<=` | scalaire | |
| `__contains` | `LIKE` | chaîne | enveloppe la valeur en `%v%` |
| `__icontains` | `ILIKE` | chaîne | enveloppe la valeur en `%v%` ; émulé sur MySQL via `LOWER()` |
| `__startswith` | `LIKE` | chaîne | enveloppe en `v%` |
| `__istartswith` | `ILIKE` | chaîne | enveloppe en `v%` |
| `__endswith` | `LIKE` | chaîne | enveloppe en `%v` |
| `__iendswith` | `ILIKE` | chaîne | enveloppe en `%v` |
| `__iexact` | `ILIKE` | chaîne | pas d'enveloppement par joker — correspondance exacte insensible à la casse |
| `__in` | `IN (…)` | `SqlValue::List` | rejette les valeurs non-liste |
| `__isnull` | `IS NULL` / `IS NOT NULL` | `bool` | `true` → IS NULL, `false` → IS NOT NULL |
| `__between` / `__range` | `BETWEEN … AND …` | `SqlValue::List` à 2 éléments | inclusif aux deux bornes |
| `__regex` / `__iregex` | PG `~` / `~*`, MySQL/SQLite `REGEXP` | chaîne | insensibilité à la casse émulée sur MySQL/SQLite via `LOWER()` ; SQLite nécessite une fonction utilisateur `regexp` |

**Les erreurs remontent à `.compile()`, pas au moment de l'appel `.filter()`** — les incohérences de forme de valeur (par exemple `__in` avec un scalaire, `__isnull` avec un non-bool, `__between` avec une arité incorrecte) et les suffixes inconnus (`status__nope`) retournent `QueryError::UnknownLookup` / `QueryError::InvalidLookupValue` depuis `.compile()` afin que la chaîne fluide reste propre côté types. Les traversées chaînées (`author__name__icontains`) ne sont **pas** prises en charge en v0.39 — le découpeur prend le suffixe après le premier `__`, donc toute la queue `name__icontains` est traitée comme un suffixe inconnu.

Chaque appel de filtre se joint par un AND à ceux qui précèdent ; mélangez librement la forme Django, `filter_op`, et `where_` sur le même queryset.

---

## Requêtes cloisonnées par tenant

Dans une application multi-tenant, exécutez chaque requête sur la connexion du tenant courant plutôt que sur le pool partagé. Récupérez une connexion par requête et passez-la à `fetch_on` (qui accepte n'importe quel executor de base de données) au lieu de `fetch` (qui utilise toujours `&pool`).

```rust
use rustango::extractors::Tenant;

async fn handler(mut t: Tenant) -> Result<...> {
    let conn = t.conn();        // &mut PgConnection for this tenant
    let posts = Post::objects().fetch_on(&mut *conn).await?;
    Ok(...)
}
```

`fetch_on` fonctionne avec n'importe quel `sqlx::Executor` ; `fetch` est du sucre syntaxique pour `fetch_on(&pool)`.

---

## Signaux

Exécutez un callback quand quelque chose se produit — les signaux de Django. Il existe deux registres indépendants : un pour les écritures de modèles, un pour les requêtes HTTP.

### Cycle de vie du modèle

Déclenchez un hook avant ou après qu'un modèle soit sauvegardé ou supprimé : `pre_save`, `post_save`, `pre_delete`, `post_delete`. Enregistrez-en un avec `connect_post_save::<Post, _, _>(...)`.

```rust
use rustango::signals::{connect_post_save, PostSaveContext};

connect_post_save::<Post, _, _>(|post, ctx| async move {
    if ctx.created {
        tracing::info!("new post #{}", post.id.get().copied().unwrap_or(0));
    }
});
```

`T: Clone + 'static` est requis (le dispatcher remet à chaque récepteur un clone `Arc<T>`). Les récepteurs s'exécutent séquentiellement dans l'ordre d'enregistrement. Déconnectez via le `ReceiverId` retourné par `connect_*`. Les quatre types de signaux + leurs formes de contexte sont documentés en ligne dans `rustango::signals`.

### Cycle de vie de la requête

Déclenchez un hook autour de chaque requête HTTP : `request_started`, `request_finished`, `got_request_exception`. Ajoutez le middleware `RequestSignalsLayer` à votre routeur, puis connectez des callbacks. Utile pour le traçage, l'audit, les métriques de temps de requête, et le reporting d'erreurs de style Django.

```rust
use axum::Router;
use rustango::signals::request::{
    connect_request_started, connect_request_finished, RequestSignalsLayer,
};

connect_request_started(|ctx| Box::pin(async move {
    tracing::info!(method = %ctx.method, path = %ctx.path, "started");
}));
connect_request_finished(|ctx| Box::pin(async move {
    metrics::histogram!("http_request_ms").record(ctx.elapsed_ms);
}));

let app: Router = Router::new()
    .route("/", get(home))
    .layer(RequestSignalsLayer::new());  // outermost — sees request first / response last
```

| Signal | Champs de contexte |
|---|---|
| `request_started` | `method`, `path`, `query` |
| `request_finished` | `method`, `path`, `status`, `elapsed_ms` |
| `got_request_exception` | `method`, `path`, `error` |

Les récepteurs s'exécutent séquentiellement dans l'ordre d'enregistrement ; enveloppez un corps dans `tokio::spawn` pour un fan-out parallèle ou une isolation des panics. Les registres de requêtes et de modèles sont indépendants — connecter / déconnecter / vider l'un ne touche pas l'autre.

---

## Conseils de performance

Une checklist rapide pour garder les requêtes rapides à mesure que les données croissent :

- **Utilisez toujours des index pour les colonnes `WHERE` et `ORDER BY`.** Déclarez-les via `#[rustango(index)]` afin qu'ils figurent dans les migrations.
- **`select_related` pour l'affichage des FK dans les listes** — élimine le N+1 dans les vues de liste admin/publiques.
- **`page` plutôt que `fetch().drain()`** — ne chargez jamais des tables entières.
- **Pagination par curseur pour les tables volumineuses** — évite un `COUNT(*)` par page.
- **`bulk_insert_on` pour les lots** — un seul aller-retour au lieu de N.
- **`upsert_on` pour les imports idempotents** — `ON CONFLICT` est plus rapide qu'un SELECT-puis-INSERT.
- **`transaction` pour les écritures liées** — réduit la surcharge de commit et préserve la cohérence.
- **Mettez en cache les lectures fréquentes** avec `cache::get_or_set` — invalidez sur un gestionnaire de signal `connect_post_save<T>(...)`.

---

## Voir aussi

- [Models](models.md) — déclarer un modèle : types de champs, clés primaires, chaque attribut (le complément à ce guide de requêtage).
- [Serializers](serializers.md) — mettre en forme les lignes de modèle en JSON.
- [ViewSets](viewsets.md) — transformer un modèle en une API CRUD JSON.
- [The admin](admin.md) — une interface utilisateur auto-générée sur les mêmes modèles.
- [`manage` CLI](manage.md) — `makemigrations` / `migrate` pour les changements de schéma.
