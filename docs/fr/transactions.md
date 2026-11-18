# Transactions

Un bloc `atomic` exécute un groupe d'instructions comme une seule unité : soit
toutes sont validées, soit aucune. Il fonctionne sur PostgreSQL, MySQL (et
MariaDB) et SQLite. Là où les backends diffèrent, cette page le dit.

Pour un simple handle de transaction (`transaction_pool`) et les verrous de
ligne, voir le [guide de l'ORM](orm.md).

> **Un terme vous est inconnu ?** *Transaction*, *rollback* — voir le
> [glossaire](glossary.md).

## Table des matières

- [Un premier bloc](#un-premier-bloc)
- [Ce qui s'exécute dans le bloc](#ce-qui-sexécute-dans-le-bloc)
- [Blocs imbriqués](#blocs-imbriqués)
- [Après le commit](#après-le-commit)
- [Quand une instruction échoue](#quand-une-instruction-échoue)
- [SQLite prend le verrou d'écriture au BEGIN](#sqlite-prend-le-verrou-décriture-au-begin)
- [Niveaux d'isolation et paramètres](#niveaux-disolation-et-paramètres)
- [Ce code est-il dans un bloc ?](#ce-code-est-il-dans-un-bloc-)
- [Get or create dans une transaction](#get-or-create-dans-une-transaction)
- [Les formes `_on`](#les-formes-_on)
- [DDL dans un bloc](#ddl-dans-un-bloc)
- [Itérer dans un bloc](#itérer-dans-un-bloc)
- [Annulation](#annulation)
- [Erreurs](#erreurs)

## Un premier bloc

Les exemples utilisent ce modèle et un `pool: &Pool` :

```rust
use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "accounts")]
pub struct Account {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(unique, max_length = 64)]
    pub email: String,
    pub balance: i64,
}
```

`rustango::sql::atomic(&pool, closure)` ouvre une transaction, exécute la
closure et valide quand elle renvoie `Ok`. Un `Err` annule tout.

```rust
use rustango::sql::{atomic, ExecError, Pool};

async fn transfer(pool: &Pool, from: i64, to: i64, amount: i64) -> Result<(), ExecError> {
    let p = pool.clone(); // the closure must own what it uses
    atomic(pool, move |_tx| {
        Box::pin(async move {
            let mut a = Account::find_or_fail(from, &p).await?;
            let mut b = Account::find_or_fail(to, &p).await?;
            a.balance -= amount;
            b.balance += amount;
            a.save(&p).await?;
            b.save(&p).await?;
            Ok(()) // Ok commits; an Err (or a failed `?`) rolls both saves back
        })
    })
    .await
}
```

La closure reçoit son propre clone du pool, pas un emprunt : le future du bloc
ne doit pas emprunter l'appelant. Un `Pool` se clone à peu de frais. La macro
`rustango::atomic!(&pool, |tx| { … })` écrit le `Box::pin` pour vous.

## Ce qui s'exécute dans le bloc

Dans la closure, chaque appel de l'ORM qui reçoit le même pool (ou un clone)
s'exécute sur la transaction du bloc :

- lectures : `fetch`, `find`, `count`, `get_or_create`, …
- écritures : `insert`, `save`, `delete`, écritures en masse, mises à jour de queryset
- SQL brut : `raw_query_pool`, `raw_execute_pool`
- DDL (voir [DDL dans un bloc](#ddl-dans-un-bloc))
- écritures du framework : lignes d'audit, fixtures, écritures des ViewSet et
  de l'admin, médias, inscription et invitations SSO

Elles voient les lignes non validées du bloc, et son rollback les annule. Sur
SQLite, et sur MySQL à son niveau par défaut (repeatable read), toutes les
lectures du bloc voient un seul instantané.

Ceux-ci s'exécutent **hors** du bloc, sur leur propre connexion :
`pool.acquire()`, `Tenant::conn`, le cache, la prise de jobs, les changements
du registre de tenancy, une tâche lancée avec `tokio::spawn`, et un pool
connecté à part (même vers la même base). Ils ne voient pas les écritures du
bloc et peuvent attendre des verrous qu'il détient.

L'argument de la closure est un `AtomicTx`. `tx.lock().await?` donne un
`TxGuard` qui se déréférence en `PoolTx`, pour les helpers `_tx` et les formes
`_on` : `acc.insert_tx(&mut *guard)`. Exécutez une instruction à la fois : tant
qu'un guard est tenu, ou quand deux instructions tournent en même temps
(`join!`), les appels sur le pool renvoient `ExecError::NestedAtomic`. Relâchez
le guard avant le prochain appel avec `&pool`.

**Coût.** Une lecture ou une instruction brute coûte un aller-retour. Une
écriture de l'ORM en coûte trois, car elle s'exécute dans son propre savepoint
(ainsi une écriture ratée laisse le bloc utilisable). Pour beaucoup de lignes,
écrivez via le guard (`insert_tx`) ou une écriture en masse.

## Blocs imbriqués

`atomic` sur le même pool dans un bloc ouvre un savepoint. Son `Err` n'annule
que ses propres écritures. Un `Err` du bloc externe annule tout, travail
imbriqué compris.

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        account("a@example.com", 0).insert(&p).await?;
        let q = p.clone();
        let inner = atomic(&p, move |_sp| {
            Box::pin(async move {
                account("b@example.com", 0).insert(&q).await?;
                // Stands in for any failure inside the nested block.
                Err::<(), _>(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))
            })
        })
        .await;
        assert!(inner.is_err()); // b@ is rolled back, a@ is kept
        Ok(())
    })
})
.await?;
```

(`account(email, balance)` construit un `Account` avec `id: Auto::Unset`.)

L'imbrication se fait par objet pool : transmettez le pool de la requête plutôt
que de vous reconnecter. Un bloc imbriqué abandonné ou qui panique est annulé.
Si une instruction de savepoint échoue elle-même, tout le bloc est annulé avec
`ExecError::AtomicAborted`. PostgreSQL ralentit au-delà de 64 savepoints dans
une transaction, et chaque écriture de l'ORM en est un : gardez courts les
blocs qui écrivent beaucoup.

## Après le commit

`rustango::sql::on_commit(callback)` met en file un callback qui s'exécute
après la validation du bloc le plus externe. Il ne s'exécute jamais en cas de
rollback, et un bloc imbriqué annulé abandonne ses callbacks. Les callbacks
s'exécutent dans l'ordre où ils ont été mis en file.

```rust
use rustango::sql::on_commit;

let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let mut acc = account("new@example.com", 0);
        acc.insert(&p).await?;
        on_commit(move || {
            tokio::spawn(async move { /* send the welcome email */ });
        });
        Ok(())
    })
})
.await?;
```

Le callback est synchrone : lancez le travail asynchrone depuis lui. Un
callback qui panique est journalisé et les autres s'exécutent quand même.
`on_commit` hors d'un bloc panique.

## Quand une instruction échoue

| Backend | Une lecture ou instruction brute qui échoue | Une écriture de l'ORM qui échoue |
|---|---|---|
| PostgreSQL | Interrompt le bloc : les instructions suivantes et `atomic` renvoient `ExecError::AtomicAborted` | Annulée seule (son savepoint) ; le bloc continue |
| MySQL, MariaDB | Annulée seule ; le bloc continue | Annulée seule ; le bloc continue |
| SQLite | Annulée seule ; le bloc continue | Annulée seule ; le bloc continue |

Une closure qui ignore une erreur et renvoie `Ok` valide le reste du bloc (sur
PostgreSQL, après une lecture interrompue, `atomic` renvoie plutôt
`AtomicAborted`).

Certaines erreurs terminent toute la transaction côté serveur : un deadlock
MySQL (1213), un dépassement d'attente de verrou MySQL (1205) quand le serveur
annule dessus, un rollback automatique de SQLite, une connexion perdue. Chaque
instruction de l'ORM qui suit dans le bloc, et `atomic` lui-même, renvoient
alors `ExecError::AtomicAborted`. MariaDB signale un `nowait()` manqué comme
1205 ; avec la valeur par défaut `innodb_rollback_on_timeout = OFF`, il n'annule
que l'instruction.

**Continuer après une lecture ratée.** Pour tenter une étape qui peut échouer
(un verrou `nowait()`, une sonde sur une table qui n'existe peut-être pas),
placez-la dans un bloc imbriqué. Son échec n'annule alors que ce bloc, sur tous
les backends :

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let q = p.clone();
        // A failed read in a nested block undoes only that block, even on PG.
        let locked = atomic(&p, move |_sp| {
            Box::pin(async move {
                Account::objects()
                    .filter("email", "a@example.com")
                    .nowait()
                    .fetch(&q)
                    .await
            })
        })
        .await;
        match locked {
            Ok(_rows) => { /* got the lock */ }
            Err(_) => { /* row busy: the outer block goes on */ }
        }
        Ok(())
    })
})
.await?;
```

Du SQL brut qui termine la transaction (`COMMIT`, `ROLLBACK`, ou une
instruction sur laquelle MySQL valide) fait renvoyer au bloc
`ExecError::AtomicEndedEarly`. Les instructions suivantes ont tourné sans
transaction : ne réessayez pas à l'aveugle.

## SQLite prend le verrou d'écriture au BEGIN

Sur SQLite, le bloc le plus externe commence par `BEGIN IMMEDIATE`. Un bloc qui
lit puis écrit n'échoue pas avec `SQLITE_BUSY` à sa première écriture ; c'est un
second écrivain qui attend à son propre `BEGIN`. Les autres connexions qui
écrivent attendent `busy_timeout` (5 secondes par défaut) puis échouent avec
`SQLITE_BUSY` ; sur une base en mémoire, elles attendent sans délai maximal.
Gardez les blocs SQLite courts.

`atomic_immediate` faisait cela avant `atomic`. Il est déprécié : appelez
`atomic`.

## Niveaux d'isolation et paramètres

`atomic_with(&pool, options, closure)` ouvre le bloc le plus externe avec un
niveau d'isolation, des paramètres PostgreSQL, ou les deux :

```rust
use rustango::sql::{atomic_with, raw_query_pool, AtomicOptions, Isolation, TxSetting};

let opts = AtomicOptions::new()
    .isolation(Isolation::RepeatableRead)
    .setting(TxSetting::new("app.account_id", "42")?); // PostgreSQL only
let p = pool.clone();
let seen: Vec<(Option<String>,)> = atomic_with(pool, opts, move |_tx| {
    Box::pin(async move {
        raw_query_pool("SELECT current_setting('app.account_id', true)", vec![], &p).await
    })
})
.await?;
```

- **Niveaux :** `Isolation::ReadCommitted`, `RepeatableRead`, `Serializable`.
  PostgreSQL commence par `BEGIN ISOLATION LEVEL …`. MySQL fixe le niveau pour
  cette transaction seulement ; le bloc suivant sur la connexion revient à la
  valeur par défaut du serveur. SQLite accepte tout niveau : il est toujours
  sérialisable. Sans niveau, le bloc utilise la valeur par défaut du serveur
  (PostgreSQL : read committed ; MySQL : repeatable read).
- **Les paramètres** (`TxSetting`) sont propres à PostgreSQL, par exemple
  `app.account_id` pour les politiques de sécurité de ligne. Chacun est fixé
  avec `set_config(name, value, true)` et disparaît à la fin du bloc. La valeur
  est liée. Le nom doit contenir un point (`prefix.name`), chaque partie étant un
  identifiant simple ; `search_path` ou `role` renvoient donc
  `ExecError::InvalidTxSetting`. Sur MySQL et SQLite, les paramètres renvoient
  `ExecError::Sql(SqlError::OperatorNotSupportedInDialect)` avant que le bloc ne
  commence.
- **Les blocs imbriqués** ne peuvent pas les changer : `atomic_with` avec des
  options dans un bloc sur le même pool renvoie `ExecError::InAtomic`.

## Ce code est-il dans un bloc ?

`rustango::sql::in_atomic(&pool)` vaut `true` tant que la tâche courante est
dans un bloc sur `pool` (ou un clone), si bien que les appels de l'ORM qui
reçoivent `pool` s'exécutent sur sa transaction. Il vaut `false` hors d'un bloc
et pour tout autre pool. Il reste `true` dans un bloc qui a échoué ou s'est
terminé tôt.

## Get or create dans une transaction

`get_or_create_tx` et `update_or_create_tx` prennent un `PoolTx` ouvert. Dans un
bloc, verrouillez le guard et passez-le :

```rust
use rustango::sql::get_or_create_tx;

let email = "a@example.com".to_owned();
let (acc, created) = atomic(pool, move |tx| {
    Box::pin(async move {
        let mut g = tx.lock().await?;
        let filter = Account::objects().filter("email", email.clone());
        get_or_create_tx(
            filter,
            move |t| {
                Box::pin(async move {
                    let mut acc = Account { id: Auto::Unset, email, balance: 0 };
                    acc.insert_tx(t).await?;
                    Ok(acc)
                })
            },
            &mut *g,
        )
        .await
    })
})
.await?;
```

```rust
use rustango::sql::update_or_create_tx;

let (acc, created) = atomic(pool, move |tx| {
    Box::pin(async move {
        let mut g = tx.lock().await?;
        update_or_create_tx(
            Account::objects().filter("email", "b@example.com"),
            |t, mut acc: Account| {
                Box::pin(async move {
                    acc.balance += 10;
                    acc.save_tx(t).await?;
                    Ok(acc)
                })
            },
            |t| {
                Box::pin(async move {
                    let mut acc = Account {
                        id: Auto::Unset,
                        email: "b@example.com".into(),
                        balance: 10,
                    };
                    acc.insert_tx(t).await?;
                    Ok(acc)
                })
            },
            &mut *g,
        )
        .await
    })
})
.await?;
```

**Courses.** La closure de création s'exécute dans un savepoint. Quand deux
transactions créent la même ligne en même temps, l'insert du perdant heurte la
contrainte d'unicité : son savepoint est annulé et il lit la ligne du gagnant
(`created == false` ; `update_or_create_tx` la met alors à jour). La
transaction reste utilisable. Il faut une contrainte `UNIQUE` sur les colonnes
filtrées. Un `get_or_create_tx` dans la closure de création d'un autre (une
ligne parente, puis l'enfant) fonctionne de la même façon.

| Backend et niveau | Le perdant reçoit |
|---|---|
| PostgreSQL read committed (par défaut) | La ligne du gagnant |
| PostgreSQL repeatable read | L'erreur de violation d'unicité |
| PostgreSQL serializable | Un échec de sérialisation (40001) |
| MySQL, MariaDB | La ligne du gagnant (relue avec une lecture verrouillante) |
| SQLite | Pas de course : les blocs s'exécutent un par un |

En repeatable read ou serializable sur PostgreSQL, réessayez tout le bloc.

Les formes pool `get_or_create` et `update_or_create` se comportent de même dans
un bloc : leurs lectures le rejoignent et la closure de création s'exécute dans
un savepoint. Ne tenez pas le guard pendant leur appel.

## Les formes `_on`

Les méthodes `_on` prennent n'importe quel exécuteur : un `&Pool`, une
connexion tenue (`&mut PoolConn` issu de `pool.acquire()`), ou une transaction
ouverte (`&mut PoolTx` issu de `transaction_pool`, ou `&mut *guard` dans un
bloc). L'instruction s'exécute sur ce que vous passez.

- `fetch_on`, `insert_on`, `save_on`, `delete_on`, `upsert_on`
- `Model::bulk_insert_on`, `Model::bulk_upsert_on`, `Model::bulk_insert_or_ignore_on`
- `rustango::sql::bulk_update_on`
- `rustango::sql::fetch_dates_on`, `rustango::sql::fetch_datetimes_on`
- `rustango::sql::model_shortcuts::increment_one_on`
- mises à jour et suppressions de queryset : `.update().execute_on(..)`, `.delete_on(..)`

```rust
use rustango::core::{BulkUpdateQuery, Model as _, SqlValue}; // `Model` brings `Account::SCHEMA` into scope
use rustango::sql::model_shortcuts::increment_one_on;
use rustango::sql::{bulk_update_on, transaction_pool};

let mut tx = transaction_pool(pool).await?;
let mut rows = vec![
    Account { id: Auto::Unset, email: "a@example.com".into(), balance: 0 },
    Account { id: Auto::Unset, email: "b@example.com".into(), balance: 0 },
];
Account::bulk_insert_on(&mut rows, &mut tx).await?;
// MySQL leaves `Auto` ids unset after a bulk insert: read them back.
let ids: Vec<i64> = Account::objects()
    .order_by(&[("id", false)])
    .fetch_on(&mut tx)
    .await?
    .iter()
    .filter_map(|a| a.id.get().copied())
    .collect();
let set_100 = BulkUpdateQuery::new(
    Account::SCHEMA,
    vec!["balance"],
    ids.iter()
        .map(|id| vec![SqlValue::from(*id), SqlValue::from(100_i64)])
        .collect(),
);
bulk_update_on(&mut tx, &set_100).await?;
increment_one_on::<Account, _>("id", SqlValue::from(ids[0]), "balance", 5, &mut tx).await?;
tx.commit().await?; // or drop `tx` to roll it all back
```

Sur un modèle audité, les écritures `_on` dérivées et `increment_one_on`
renvoient `ExecError::AuditUnsupported` : elles ne peuvent pas écrire les lignes
d'audit. Utilisez-y les formes pool.

## DDL dans un bloc

- **PostgreSQL et SQLite :** le DDL s'exécute dans la transaction du bloc et
  est annulé avec elle.
- **MySQL :** le DDL valide la transaction. `run_ddl_idempotent` (et les helpers
  `ensure_table` construits dessus) refuse dans un bloc avec une
  `sqlx::Error::Protocol` dont le message commence par `DDL on MySQL is refused
  inside an atomic block on its pool`. Du DDL brut via `raw_execute_pool`
  s'exécute, valide le travail fait jusque-là, et le bloc renvoie
  `ExecError::AtomicEndedEarly`.
- **Tout backend :** une migration avec `"atomic": false` renvoie
  `MigrateError::Exec(ExecError::InAtomic { .. })`.

Exécutez le DDL et les migrations avant ou après le bloc.

## Itérer dans un bloc

Dans un bloc sur le même pool, `.iterator(n)` exécute chaque chunk sur la
transaction du bloc. Il voit les écritures du bloc, garde les verrous de ligne
de `select_for_update()`, et pagine par clé primaire (`WHERE pk > last`) au lieu
d'`OFFSET`. Les lignes que la boucle fait sortir du filtre ne sont pas sautées :

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let mut it = Account::objects()
            .filter("balance", 0_i64)
            .order_by(&[("id", false)]) // primary key only, or no order
            .iterator(500)?;
        while let Some(chunk) = it.next_chunk(&p).await? {
            for mut acc in chunk {
                acc.balance = 1; // leaves the filter; paging by key still reaches every row
                acc.save(&p).await?;
            }
        }
        Ok(())
    })
})
.await?;
```

Le queryset doit être trié par la clé primaire (dans un sens ou l'autre) ou pas
du tout, sélectionner la clé primaire, et ne pas être un `union` /
`intersection` / `difference`. Sinon `next_chunk` renvoie
`ExecError::InAtomic`.

## Annulation

Abandonner le future d'un bloc (un `tokio::time::timeout`, un client qui s'en
va) annule le bloc, et ses callbacks `on_commit` ne s'exécutent jamais :

```rust
use std::time::Duration;

let p = pool.clone();
let slow = atomic(pool, move |_tx| {
    Box::pin(async move {
        Account { id: Auto::Unset, email: "late@example.com".into(), balance: 0 }
            .insert(&p)
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await; // stands in for slow work
        Ok(())
    })
});
// Dropping the block (here on timeout) rolls it back.
assert!(tokio::time::timeout(Duration::from_millis(100), slow).await.is_err());
```

Si une instruction tournait quand le bloc a été abandonné, le serveur ne
l'arrête pas. La connexion reste occupée jusqu'à ce que le serveur finisse
cette instruction ; ensuite le rollback s'exécute et la connexion retourne au
pool. Entre-temps, un petit pool peut manquer de connexions. Un bloc imbriqué
abandonné est annulé à la prochaine utilisation du bloc.

## Erreurs

| Erreur | Sens |
|---|---|
| `ExecError::NestedAtomic` | Une instruction a tourné pendant qu'un `TxGuard` était tenu, ou deux ont tourné en même temps (`join!`) |
| `ExecError::AtomicAborted` | La transaction a été annulée après une instruction ou un savepoint en échec |
| `ExecError::AtomicEndedEarly` | Du SQL brut a validé ou terminé la transaction ; certaines écritures sont peut-être validées |
| `ExecError::InAtomic` | L'opération est refusée dans un bloc sur son pool |
| `ExecError::InvalidTxSetting` | Un nom de `TxSetting` n'est pas un `prefix.name` avec point |
