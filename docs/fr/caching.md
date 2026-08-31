# Mise en cache

La mise en cache stocke le résultat d'un travail coûteux — une requête lourde,
un fragment rendu, un appel d'API tierce — afin que la requête suivante l'obtienne
instantanément au lieu de le recalculer. **Rustango** vous donne un unique trait
`Cache` avec des backends interchangeables (in-memory, Redis, base de données),
un helper de calcul-au-miss (`get_or_set`), et des helpers JSON typés. Changez de
backend sans toucher à un seul site d'appel — comme le framework de cache de
Django ou la façade `Cache` de Laravel.

[![Caching in Rustango: get_or_set checks the cache, runs the factory only on a miss, stores the result with a TTL, and serves hits instantly; the same Cache trait backs InMemory, Redis, and DB](../img/caching.png)](../img/caching.png)

> **Un terme vous est inconnu ?** *cache*, *TTL*, *clé*, *backend* — voir le
> [glossaire](glossary.md).

> **Source :** `rustango::cache` (`Cache`, `InMemoryCache`, `NullCache`,
> `get_or_set`, `get_json`, `set_json`, `BoxedCache`, `from_settings`) — derrière
> la feature `cache` (activée par défaut). `RedisCache` requiert la feature
> `cache-redis` (désactivée par défaut).
>
> **Version exécutable :** chaque extrait est copié depuis
> [`cache_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/cache_doc.rs)
> (`cargo test -p rustango --test cache_doc`) ; le backend base de données est
> éprouvé en dogfooding sur SQLite par
> [`cache_db_backend_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/cache_db_backend_sqlite_live.rs).

## Table des matières

- [Étape 1 — Choisir un backend](#step-1--pick-a-backend)
- [Étape 2 — get / set / delete](#step-2--get--set--delete)
- [Étape 3 — get_or_set (cache-aside)](#step-3--get_or_set-cache-aside)
- [Valeurs JSON typées](#typed-json-values)
- [TTL et expiration](#ttl-and-expiry)
- [Changer de backend](#swapping-backends)
- [La mise en cache en multi-tenancy](#caching-under-multi-tenancy)
- [Référence](#reference)
- [Voir aussi](#see-also)

---

## Étape 1 — Choisir un backend

Chaque backend implémente le même trait `Cache`, donc votre code est identique
quel que soit celui que vous choisissez. Le code applicatif détient un
**`BoxedCache`** (`Arc<dyn Cache>`) et ne nomme jamais le type concret :

```rust
use rustango::cache::{BoxedCache, InMemoryCache};
use std::sync::Arc;

let cache: BoxedCache = Arc::new(InMemoryCache::new());
```

| Backend | Feature | À utiliser pour |
|---|---|---|
| `InMemoryCache` | `cache` | dev, tests, processus unique (HashMap par processus + TTL) |
| `RedisCache` | `cache-redis` | production ; partagé entre réplicas |
| `DbCache` | `cache` | production sans Redis ; une table `rustango_cache` |
| `NullCache` | `cache` | désactiver la mise en cache (chaque lecture rate) — pratique en tests |

---

## Étape 2 — get / set / delete

Le cœur du trait est quatre méthodes async. `set` prend un TTL optionnel
(`None` = pas d'expiration) ; `get` retourne `Option<String>` (`None` sur un
miss) :

```rust
use rustango::cache::{Cache, InMemoryCache};

let cache = InMemoryCache::new();

assert_eq!(cache.get("greeting").await?, None);     // miss
cache.set("greeting", "hello", None).await?;        // store, no expiry
assert_eq!(cache.get("greeting").await?.as_deref(), Some("hello"));
assert!(cache.exists("greeting").await?);

cache.delete("greeting").await?;                    // gone
```

Il existe aussi des variantes par lots — `get_many` / `set_many` /
`delete_many`.

---

## Étape 3 — get_or_set (cache-aside)

C'est celle que vous utiliserez le plus. `get_or_set` retourne la valeur en
cache, ou — sur un miss — exécute votre factory, stocke le résultat avec un TTL,
et le retourne. La factory **ne s'exécute que sur un miss** :

```rust
use rustango::cache::get_or_set;
use std::time::Duration;

let stats: HomeStats = get_or_set(
    &*cache,                       // &dyn Cache
    "home:stats",
    || async { compute_home_stats(&pool).await },   // runs only on a miss
    Some(Duration::from_secs(60)),                   // cache for 60s
).await?;
```

Le test sous-jacent appelle `get_or_set` deux fois pour la même clé et affirme
que la factory s'est exécutée **exactement une fois** — le second appel est
servi depuis le cache.

> **Invalidez à l'écriture.** Le cache-aside signifie des données périmées
> jusqu'à ce que le TTL expire. Pour des données qui changent, faites aussi
> `delete(key)` quand vous les écrivez — p. ex. depuis un [signal
> `post_save`](orm.md) — pour que la lecture suivante recalcule.

---

## Valeurs JSON typées

`get_json` / `set_json` sérialisent n'importe quel type
`Serialize`/`Deserialize` en JSON, de sorte que vous mettez en cache des
structs et des listes, pas seulement des chaînes :

```rust
use rustango::cache::{get_json, set_json};

#[derive(serde::Serialize, serde::Deserialize)]
struct Profile { id: i64, name: String }

set_json(&*cache, "profile:7", &profile, None).await?;
let back: Option<Profile> = get_json(&*cache, "profile:7").await?;   // None on a miss
```

(`get_or_set` les utilise en interne, c'est pourquoi son type de valeur doit
être `Serialize + Deserialize`.)

---

## TTL et expiration

Passez une `Duration` à `set` (ou `get_or_set`) et l'entrée disparaît après
elle. Vérifié : une entrée de 50 ms est lisible immédiatement et disparue après
80 ms.

```rust
cache.set("flash", "x", Some(Duration::from_millis(50))).await?;
// ...50ms later...
assert_eq!(cache.get("flash").await?, None);   // expired
```

`InMemoryCache::with_default_ttl(d)` définit un TTL par défaut appliqué lorsque
vous passez `None`.

---

## Changer de backend

Parce que tout repose sur le trait `Cache`, passer de l'in-memory à Redis est un
changement d'une ligne au démarrage — habituellement piloté par la config afin
qu'il diffère selon l'environnement :

```rust
// Build the cache from `[cache]` settings (backend = "memory" | "redis" | "db" | "null").
let cache: BoxedCache = rustango::cache::from_settings(&settings.cache);
```

En production, pointez-le vers Redis (partagé entre tous vos réplicas) :

```rust
use rustango::cache::RedisCache;   // needs the `cache-redis` feature
let cache: BoxedCache = std::sync::Arc::new(RedisCache::new("redis://localhost").await?);
```

Les mêmes appels `get` / `set` / `get_or_set` — seul le constructeur a changé.

---

## La mise en cache en multi-tenancy

`Cache` est un magasin plat indexé par `&str`, ce qui en multi-tenancy fait de
la clé naturelle la clé qui fuit : un handler — ou pire une tâche d'arrière-plan,
qui n'a aucun tenant ambiant où puiser — écrit `"stats:monthly"` pour un tenant
et tous les autres le relisent.

Enveloppez le cache partagé dans un **`ScopedCache`** pour que l'espace de noms
soit appliqué à votre place et que le site d'appel ne puisse pas l'oublier :

```rust
use rustango::cache::ScopedCache;

// From the Org the resolver already produced:
let cache = ScopedCache::for_tenant(shared.clone(), &t.org.slug);

cache.set("stats:monthly", &json, ttl).await?;   // stored as tenant:acme:stats:monthly
cache.get("stats:monthly").await?;               // reads only acme's entry
cache.clear().await?;                            // drops ONLY acme's entries
```

`ScopedCache` est lui-même un `Cache`, il se glisse donc partout où un
`BoxedCache` est attendu — `cache_page`, `cache_fragment`, les rate limiters,
`DistributedLock`. Il transmet au backend interne avec les clés remappées plutôt
que de réimplémenter quoi que ce soit, si bien que les primitives natives (Redis
`INCRBY`, `SET NX`, `MGET`) gardent leur atomicité et leur traitement par lots.

Deux choses à savoir :

| | |
|---|---|
| **C'est un espace de noms, pas une frontière** | Tout vit encore dans un seul backend, et du code détenant le cache *non cadré* peut lire n'importe quelle clé. L'idée est que le chemin ergonomique soit le chemin correct. |
| **`clear()` a besoin d'énumérer les clés** | Il passe par `Cache::delete_prefix`. `InMemoryCache` filtre sa map et `DatabaseCache` émet un `DELETE … LIKE 'prefix%'`. Un backend qui ne *peut pas* énumérer — `FileCache` hache les clés en chemins — se rabat sur un vidage complet et journalise un avertissement. C'est délibéré : trop peu supprimer laisserait un autre espace de noms lire une entrée périmée, ce qui est un bug de correction ; trop supprimer ne coûte qu'un défaut de cache. |

Le `Cache::clear()` non cadré reste global au processus : utilisez la vue cadrée
dès que c'est le changement d'un seul tenant qui a déclenché l'invalidation.

---

## Référence

**Trait `Cache` :** `get` · `set(key, value, ttl)` · `delete` · `exists` ·
`get_many` / `set_many` / `delete_many` · `get_or(key, default)`.

**Helpers libres :** `get_or_set(cache, key, factory, ttl)` ·
`get_json` / `set_json` · `from_settings(&CacheSettings)`.

**Ce qui est bâti sur le cache :** les [sessions](auth-sessions.md) côté
serveur, la [limitation de débit](middleware.md) distribuée
(`CacheRateLimitLayer`), les clés d'idempotence, et les feature flags prennent
tous un `BoxedCache` — de sorte qu'une seule instance Redis les adosse tous.

---

## Voir aussi

- [Tâches d'arrière-plan](jobs.md) — l'autre moitié pour garder les requêtes
  rapides (différer le travail au lieu de mettre en cache son résultat).
- [Sessions](auth-sessions.md) — un stockage côté serveur bâti sur `Cache`.
- [Middleware](middleware.md) — `CacheRateLimitLayer` partage un compteur entre
  réplicas via le cache.
- [Cookbook de l'ORM](orm.md) — invalider les lectures en cache depuis un signal
  `post_save`.
