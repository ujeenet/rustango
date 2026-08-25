# Tâches d'arrière-plan

Certains travaux ne devraient pas avoir lieu pendant une requête — envoyer un
e-mail de bienvenue, redimensionner un fichier importé, synchroniser une API
tierce. Le faire en ligne fait attendre l'utilisateur et couple la réponse à un
appel externe instable. Une **tâche d'arrière-plan** déplace ce travail sur une
file : le handler retourne immédiatement, et un pool de workers exécute la tâche
quelques instants plus tard, avec des **nouvelles tentatives automatiques** et
un chemin **dead-letter** pour les échecs. C'est Django-Q / Celery / les queues
de Laravel, en Rust.

[![Background jobs in Rustango: a handler dispatches a Job onto a queue, worker tasks run it, retryable failures back off and retry, fatal ones go to a dead-letter handler](img/jobs.png)](img/jobs.png)

> **Un terme vous est inconnu ?** *file*, *worker*, *nouvelle tentative/backoff*,
> *dead-letter* — voir le [glossaire](glossary.md).

> **Source :** `rustango::jobs` (`Job`, `JobQueue`, `InMemoryJobQueue`,
> `JobError`, `JobDeadLetter`) et `rustango::jobs::DatabaseJobQueue` (la file
> adossée à une table, alias de `pg::PgJobQueue`) — derrière la feature `jobs`
> (activée par défaut).
>
> **Version exécutable :** les extraits in-memory, nouvelle tentative et
> dead-letter sont copiés depuis [`jobs_doc.rs`](../crates/rustango/tests/jobs_doc.rs)
> (`cargo test -p rustango --test jobs_doc`) ; la file persistante est
> éprouvée en dogfooding sur SQLite par
> [`jobs_sqlite_live.rs`](../crates/rustango/tests/jobs_sqlite_live.rs)
> (`cargo test -p rustango --features sqlite,jobs-postgres --test jobs_sqlite_live`).

## Table des matières

- [Étape 1 — Définir une tâche](#step-1--define-a-job)
- [Étape 2 — Démarrer une file](#step-2--start-a-queue)
- [Étape 3 — Dispatcher depuis un handler](#step-3--dispatch-from-a-handler)
- [Étape 4 — Câbler dans votre application](#step-4--wire-it-into-your-app) — l'exemple complet
- [Faire tourner les workers (CLI + production)](#running-the-workers-cli--production)
- [Nouvelles tentatives et backoff](#retries-and-backoff)
- [Le handler dead-letter](#the-dead-letter-handler)
- [La file persistante (production)](#the-persistent-queue-production)
- [Les tâches en multi-tenancy](#jobs-under-multi-tenancy)
- [Les balayages planifiés en multi-tenancy](#scheduled-sweeps-under-multi-tenancy)
- [Référence](#reference)
- [Voir aussi](#see-also)

---

## Étape 1 — Définir une tâche

Une tâche est une structure sérialisable qui implémente `Job`. La charge utile
est ce qui est mis en file (sérialisé en JSON) ; `run()` est le travail. `NAME`
route une charge utile mise en file vers son handler, il doit donc être unique.

Générez un squelette avec la CLI — `cargo run -- make:job WelcomeEmail` — ou
écrivez-le à la main :

```rust
use rustango::jobs::{Job, JobError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct WelcomeEmail {
    user_id: i64,
}

#[async_trait::async_trait]   // add `async-trait` to your Cargo.toml
impl Job for WelcomeEmail {
    const NAME: &'static str = "welcome_email";

    async fn run(&self) -> Result<(), JobError> {
        // ... look up the user and send the email
        Ok(())
    }
}
```

`run()` retourne :

- `Ok(())` — terminé.
- `Err(JobError::Retryable(msg))` — transitoire ; le worker réessaie avec
  backoff.
- `Err(JobError::Fatal(msg))` — permanent ; passe les nouvelles tentatives,
  dead-letter immédiatement.

Surchargez `const MAX_ATTEMPTS: u32 = 3;` sur l'impl pour changer le plafond de
nouvelles tentatives (5 par défaut).

---

## Étape 2 — Démarrer une file

Pour un processus unique (et pour le dev et les tests), `InMemoryJobQueue`
exécute les tâches sur des tâches worker tokio — sans base de données.
**Enregistrez chaque type de tâche, puis `start()` les workers** (les tâches ne
sont pas prises en charge tant que vous ne le faites pas) :

```rust
use rustango::jobs::{InMemoryJobQueue, JobQueue};
use std::sync::Arc;

let queue = Arc::new(InMemoryJobQueue::with_workers(4));   // 4 worker tasks
queue.register::<WelcomeEmail>().await;                    // before start/dispatch
queue.start().await;

// ... app runs ...

queue.shutdown().await;   // on shutdown: drain in-flight jobs, then stop
```

Conservez le `Arc<InMemoryJobQueue>` dans l'état de votre application pour que
les handlers puissent l'atteindre.

> **In-memory signifie in-memory.** Les tâches en file ou en cours sont
> **perdues au redémarrage**. Pour tout ce que vous ne pouvez pas vous permettre
> de perdre, utilisez la [file persistante](#the-persistent-queue-production).

---

## Étape 3 — Dispatcher depuis un handler

Une fois la file en marche, le dispatch tient en un appel — il met en file et
retourne immédiatement, de sorte que la requête n'attend pas le travail :

```rust
queue.dispatch(&WelcomeEmail { user_id: 42 }).await?;
```

Un worker la prend en charge et exécute `run()` quelques instants plus tard.
Vérifié de bout en bout : les trois tâches dispatchées s'exécutent toutes sur
les workers.

```rust
for user_id in 1..=3 {
    queue.dispatch(&WelcomeEmail { user_id }).await.unwrap();
}
// → all three WelcomeEmail::run() calls execute on the worker pool
```

---

## Étape 4 — Câbler dans votre application

En assemblant les étapes 1 à 3 : construisez la file et enregistrez chaque tâche
**une seule fois au démarrage**, lancez les workers, remettez la file à vos
routes pour que les handlers puissent l'atteindre, puis drainez à l'arrêt. La
file vit dans votre `main.rs` :

```rust
// src/main.rs
use std::sync::Arc;
use rustango::jobs::{InMemoryJobQueue, JobQueue};
use rustango::manage::Cli;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Build the queue + register every job type BEFORE starting workers.
    let queue = Arc::new(InMemoryJobQueue::with_workers(4));
    queue.register::<WelcomeEmail>().await;
    queue
        .on_dead_letter(|dl| async move {
            tracing::error!(job = dl.name, attempts = dl.attempts, error = %dl.error,
                            "job dead-lettered");
        })
        .await;

    // 2. Start the workers — tokio tasks that run inside THIS process.
    queue.start().await;

    // 3. Share the queue with handlers (axum extension/state).
    let app = urls::api().layer(axum::Extension(queue.clone()));

    // 4. Boot the server. Cli::run() blocks until Ctrl-C / SIGTERM.
    Cli::new().api(app).with_health().run().await?;

    // 5. On shutdown: drain in-flight jobs, then stop.
    queue.shutdown().await;
    Ok(())
}
```

Un handler dispatche en relisant la file depuis la requête :

```rust
use axum::Extension;
use std::sync::Arc;
use rustango::jobs::{InMemoryJobQueue, JobQueue};

async fn signup(Extension(queue): Extension<Arc<InMemoryJobQueue>>) -> StatusCode {
    // ... create the user ...
    queue.dispatch(&WelcomeEmail { user_id: 42 }).await.ok();  // returns instantly
    StatusCode::CREATED
}
```

> **Stockez le type de file concret.** Le trait `JobQueue` n'est **pas
> object-safe** (ses `register`/`dispatch` sont génériques), donc vous ne pouvez
> pas conserver `Arc<dyn JobQueue>` dans l'état — gardez le
> `Arc<InMemoryJobQueue>` concret (ou `Arc<DatabaseJobQueue>`).

---

## Faire tourner les workers (CLI + production)

`start()` spawn les workers en tant que **tâches tokio à l'intérieur du
processus courant**. Ainsi, quand vous lancez votre application avec la CLI —
`cargo run` (qui exécute le serveur) — les workers tournent juste à côté de lui.
Pour la plupart des applications, c'est tout ce dont vous avez besoin : **un seul
processus sert les requêtes *et* draine la file** ; il n'y a pas de commande
worker distincte.

```bash
cargo run                 # starts the server AND the in-process workers
cargo run -- make:job WelcomeEmail   # scaffold a new job type
```

### Un processus worker dédié (production)

À l'échelle, vous voulez souvent des workers **séparés** de la couche web — pour
qu'un pic de trafic ne puisse pas affamer les tâches, et que vous mettiez à
l'échelle chacun indépendamment. Avec la [file
persistante](#the-persistent-queue-production), chaque processus tire de la même
table `rustango_jobs`, donc lancez simplement un second binaire, sans serveur,
qui construit la file, la démarre, et bloque jusqu'à un signal :

```rust
// src/bin/worker.rs — run with `cargo run --bin worker`
use std::sync::Arc;
use rustango::jobs::{DatabaseJobQueue, JobQueue};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = /* connect your tri-dialect Pool */;
    DatabaseJobQueue::ensure_table_pool(&pool).await?;

    let queue = Arc::new(DatabaseJobQueue::with_workers_pool(pool, 8));
    queue.register::<WelcomeEmail>().await;   // register the SAME job types
    queue.start().await;

    tokio::signal::ctrl_c().await?;            // block until Ctrl-C / SIGTERM
    queue.shutdown().await;                     // drain in-flight, then exit
    Ok(())
}
```

Déployez-le comme son propre conteneur/service et mettez-le à l'échelle sur **N
réplicas** — ils tirent tous de la table partagée en toute sécurité. Le
processus web n'a alors besoin que de `dispatch` (il n'a pas à `start()` de
workers). Associez le worker à un balayage périodique
[`reclaim_stuck_jobs_pool`](#the-persistent-queue-production) pour récupérer les
tâches d'un worker crashé.

---

## Nouvelles tentatives et backoff

Une tâche qui retourne `Retryable` est remise en file avec un **backoff
exponentiel** (1s, 2s, 4s, 8s, …) jusqu'à `MAX_ATTEMPTS`. Utilisez-le pour les
échecs transitoires — un timeout, une API rate-limitée, un deadlock :

```rust
#[async_trait::async_trait]
impl Job for FlakyImport {
    const NAME: &'static str = "flaky_import";
    async fn run(&self) -> Result<(), JobError> {
        match call_external_api().await {
            Ok(_)  => Ok(()),
            Err(e) => Err(JobError::Retryable(e.to_string())),   // try again later
        }
    }
}
```

Le test sous-jacent dispatche une tâche qui échoue une fois puis réussit, et
affirme qu'elle **s'est exécutée plus d'une fois et a fini par réussir** — la
nouvelle tentative a bien lieu.

---

## Le handler dead-letter

Quand une tâche épuise ses nouvelles tentatives — ou retourne `Fatal`
immédiatement — elle est remise au callback **dead-letter** au lieu de
disparaître. Enregistrez-en un (avant `start`) pour journaliser, alerter, ou
persister l'échec :

```rust
queue
    .on_dead_letter(|dl| async move {
        // dl: JobDeadLetter { name, payload, attempts, error }
        tracing::error!(job = dl.name, attempts = dl.attempts, error = %dl.error,
                        "job dead-lettered");
    })
    .await;
```

Une erreur `Fatal` passe les nouvelles tentatives et atterrit ici dès la
première tentative :

```rust
async fn run(&self) -> Result<(), JobError> {
    Err(JobError::Fatal("unprocessable payload".into()))   // → dead-letter now
}
```

Le test confirme que le callback se déclenche exactement une fois pour une tâche
`Fatal`, avec le `name` et l'`error` de la tâche intacts.

---

## La file persistante (production)

`InMemoryJobQueue` est mono-processus et oublie tout au redémarrage. Pour de
vrais déploiements — plusieurs workers, plusieurs réplicas, des tâches qui
doivent survivre à un crash — utilisez **`DatabaseJobQueue`** (le même type que
`PgJobQueue`). Les tâches vivent dans une table `rustango_jobs` ; les workers
les prennent en charge avec un `UPDATE … RETURNING` borné par une transaction,
donc c'est sûr entre processus. Elle est **tri-dialecte** (PostgreSQL, MySQL,
SQLite). Les définitions `Job` de l'étape 1 sont inchangées.

```rust
use rustango::jobs::DatabaseJobQueue;   // = rustango::jobs::pg::PgJobQueue
use rustango::jobs::JobQueue;
use std::sync::Arc;
use std::time::Duration;

// 1. Create the jobs table once at boot (idempotent, tri-dialect).
DatabaseJobQueue::ensure_table_pool(&pool).await?;

// 2. Build the queue over your pool + N workers, and start it.
let queue = Arc::new(
    DatabaseJobQueue::with_workers_pool(pool.clone(), 4)
        .poll_interval(Duration::from_millis(50)),
);
queue.register::<WelcomeEmail>().await;
queue.start().await;

// 3. Dispatch exactly as before — now it's durable.
queue.dispatch(&WelcomeEmail { user_id: 42 }).await?;
```

**Récupérer les workers crashés.** Si un worker meurt en cours de tâche, la
ligne reste verrouillée. Lancez un balayage périodique pour relâcher les verrous
plus anciens qu'un seuil afin qu'un autre worker reprenne la tâche :

```rust
// e.g. from a scheduled task every few minutes:
let reclaimed = DatabaseJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(300)).await?;
```

Le flux dispatch-and-run et `reclaim_stuck_jobs_pool` sont tous deux éprouvés en
dogfooding contre SQLite dans `jobs_sqlite_live.rs`.

---

## Les tâches en multi-tenancy

Un worker n'est pas une requête. Rien n'a résolu de tenant pour lui — aucun
host, aucun en-tête, aucun middleware ne s'est exécuté — donc **une tâche ne
porte aucun contexte de tenant** : `run(&self)` ne reçoit que le payload
désérialisé. L'isolation vient **du pool sur lequel la file a été construite**,
et la règle en découle :

> Une file par pool de tenant. Un id de tenant dans le payload, c'est du
> routage, pas de l'isolation.

C'est cette seule décision qui rend chaque mode sûr :

| Mode | Où vit `rustango_jobs` | Pourquoi un worker reste cadré |
|---|---|---|
| `database` | dans la base de données / le fichier SQLite propre au tenant | les lignes d'un autre tenant ne sont pas dans la table que le worker interroge — les lectures inter-tenants ne sont pas exprimables |
| `schema` (PG) | dans le schéma du tenant | `scoped_pool_dyn` renvoie un pool dont les **connect options** embarquent `search_path`, et non un `SET` par requête — chaque checkout est donc cadré pour toute la durée de vie du pool, y compris pour un worker qui vit des jours |

Construisez les files au démarrage, une par tenant actif :

```rust
use rustango::core::Column as _;
use rustango::jobs::{DatabaseJobQueue, JobQueue};
use rustango::sql::FetcherPool as _;
use rustango::tenancy::{Org, TenantPools};
use std::sync::Arc;
use std::time::Duration;

let pools = Arc::new(TenantPools::new(registry));
let registry_pool = pools.registry_pool();

// The registry holds the tenant list; each `Org` names its own
// database (or PG schema).
let orgs: Vec<Org> = Org::objects()
    .where_(Org::active.eq(true))
    .fetch(&registry_pool)
    .await?;

let mut queues = Vec::new();
for org in orgs {
    let pool = pools.scoped_pool_dyn(&org).await?;   // scoped for its lifetime
    DatabaseJobQueue::ensure_table_pool(&pool).await?;

    let queue = Arc::new(
        DatabaseJobQueue::with_workers_pool(pool, 2)
            .poll_interval(Duration::from_secs(1)),
    );
    queue.register::<WelcomeEmail>().await;
    queue.start().await;
    queues.push((org.slug, queue));
}
```

Le dispatch part alors vers la file *de ce tenant* — dans un handler de
requête, celle indexée par l'`Org` que le resolver a déjà produit.

**Ce qu'il ne faut pas faire.** Une seule file sur le pool du registry avec un
champ `org_id` dans le payload met les tâches de tous les tenants dans une
table partagée, et laisse chaque `run()` se souvenir de re-cadrer. Un seul
re-cadrage oublié est une écriture inter-tenants. S'il vous faut une file
partagée pour des raisons opérationnelles, résolvez le pool du tenant comme
*première* chose que fait `run()`, et ne touchez plus au pool du registry
ensuite.

**Limites connues.** Le framework ne fait pas le fan-out pour vous : pas de
superviseur de workers par tenant, pas de `Job::run(&ctx)` avec le tenant
attaché, et pas de balayage `reclaim_stuck_jobs_pool` conscient du tenant —
vous itérez vous-même sur les tenants, comme ci-dessus. Les tenants
provisionnés après le démarrage n'ont aucun worker jusqu'au redémarrage du
processus. Suivi dans
[#1223](https://github.com/ujeenet/rustango/issues/1223).

**Pas de contexte ambiant non plus.** Les workers sont lancés par
`tokio::spawn`, et les task-locals ne traversent pas un spawn — une tâche
s'exécute donc avec la source d'audit à `AuditSource::System` et le fuseau
horaire par défaut, quoi qu'ait posé la requête qui l'a dispatchée. Emportez ce
qu'il vous faut dans le payload, ou ré-entrez le scope dans `run()` avec
`audit::with_source`. Suivi dans
[#1229](https://github.com/ujeenet/rustango/issues/1229).

---

## Les balayages planifiés en multi-tenancy

Le même problème — « un worker n'est pas une requête » — frappe le travail
*basé sur le temps*, et les helpers de balayage du framework sont l'endroit où
ça mord : `MediaLibrary::purge_orphans`, `audit::cleanup_older_than_pool` et
`prunable::prune_all` prennent chacun **un seul pool**, et chaque table qu'ils
touchent est par tenant. Un pool veut dire un tenant — et un pool du registry en
mode schéma veut dire seulement `public`, pendant que les lignes de chaque
tenant s'accumulent et que le balayage annonce quand même une réussite.

Faites le fan-out avec **`for_each_tenant`**, qui résout le pool propre à
chaque tenant actif et continue quand l'un échoue :

```rust
use rustango::tenancy::for_each_tenant;

let sweep = for_each_tenant(&pools, |_org, pool| async move {
    rustango::prunable::prune_all(&pool, &opts).await
})
.await?;

tracing::info!(ok = sweep.succeeded(), failed = sweep.failed(), "nightly prune");
for (slug, err) in sweep.errors() {
    tracing::warn!(%slug, %err, "tenant prune failed");
}
```

Un tenant cassé — identifiant tourné, base injoignable — est consigné dans le
rapport au lieu d'interrompre le run, et ne peut donc pas priver les tenants
suivants. Les orgs inactives sont ignorées. `purge_orphans` est le seul
balayage qui sort de la base (il supprime des objets de stockage), d'où
`purge_orphans_dry_run` : la même requête, sans rien supprimer — à regarder
avant de câbler le vrai balayage.

Si vous protégez le balayage pour qu'une seule réplique l'exécute, **cadrez le
verrou par tenant** :

```rust
use rustango::distributed_lock::DistributedLock;

let lock = DistributedLock::new(cache.clone()).for_tenant(&org.slug);
lock.with_lock("nightly_prune", ttl, || async { /* … */ }).await;
```

Non cadré, tous les tenants se disputent un unique `lock:nightly_prune` : le
premier gagne et les autres sont ignorés pendant toute la TTL, en silence — un
acquire refusé est le résultat attendu, donc rien n'est journalisé. Suivi dans
[#1226](https://github.com/ujeenet/rustango/issues/1226) et
[#1228](https://github.com/ujeenet/rustango/issues/1228).

---

## Référence

**Backends**

| Backend | À utiliser pour | Survit au redémarrage ? |
|---|---|---|
| `InMemoryJobQueue` | processus unique, dev, tests | non |
| `DatabaseJobQueue` (`PgJobQueue`) | production ; multi-worker / multi-réplica | oui (table `rustango_jobs`) |

**`JobError`**

| Variante | Effet |
|---|---|
| `Retryable(String)` | remise en file avec backoff exponentiel, jusqu'à `MAX_ATTEMPTS` |
| `Fatal(String)` | passe les nouvelles tentatives → dead-letter immédiatement |
| `Queue(String)` | erreur de file interne (sérialisation/enregistrement) |

**Méthodes de `JobQueue` :** `register::<T>()` · `dispatch(&payload)` · `start()` ·
`shutdown()` · `pending_count()`. `DatabaseJobQueue` ajoute
`ensure_table_pool`, `with_workers_pool`, `poll_interval`, et
`reclaim_stuck_jobs_pool`.

---

## Voir aussi

- [Planificateur](manage.md) — pour le travail récurrent *basé sur le temps*
  (façon cron), par opposition aux tâches à la demande.
- [E-mail](email.md) — la charge de travail canonique du « faites-le dans une
  tâche ».
- [Mise en cache](caching.md) — l'autre manière de garder les handlers de
  requêtes rapides.
- [Signals](orm.md) — des hooks fire-and-forget qui *dispatchent* souvent une
  tâche.
- [Commandes de tenancy](manage.md#tenancy-commands) — le provisionnement des
  tenants sur lesquels une file par tenant fait son fan-out.
