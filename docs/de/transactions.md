# Transaktionen

Ein `atomic`-Block führt eine Gruppe von Anweisungen als eine Einheit aus:
Entweder werden alle committet oder keine. Er funktioniert auf PostgreSQL,
MySQL (und MariaDB) und SQLite. Wo sich die Backends unterscheiden, sagt diese
Seite es.

Für ein einfaches Transaktions-Handle (`transaction_pool`) und Zeilensperren
siehe den [ORM-Leitfaden](orm.md).

> **Ein Begriff hier neu für dich?** *Transaktion*, *Rollback* — siehe das
> [Glossar](glossary.md).

## Inhaltsverzeichnis

- [Ein erster Block](#ein-erster-block)
- [Was im Block läuft](#was-im-block-läuft)
- [Verschachtelte Blöcke](#verschachtelte-blöcke)
- [Nach dem Commit](#nach-dem-commit)
- [Wenn eine Anweisung fehlschlägt](#wenn-eine-anweisung-fehlschlägt)
- [SQLite nimmt die Schreibsperre beim BEGIN](#sqlite-nimmt-die-schreibsperre-beim-begin)
- [Isolationsstufen und Einstellungen](#isolationsstufen-und-einstellungen)
- [Läuft dieser Code in einem Block?](#läuft-dieser-code-in-einem-block)
- [Get or create in einer Transaktion](#get-or-create-in-einer-transaktion)
- [Die `_on`-Formen](#die-_on-formen)
- [DDL in einem Block](#ddl-in-einem-block)
- [Iterieren in einem Block](#iterieren-in-einem-block)
- [Abbruch](#abbruch)
- [Eine Transaktion pro Anfrage](#eine-transaktion-pro-anfrage)
- [Fehler](#fehler)

## Ein erster Block

Die Beispiele verwenden dieses Model und einen `pool: &Pool`:

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

`rustango::sql::atomic(&pool, closure)` beginnt eine Transaktion, führt die
Closure aus und committet, wenn sie `Ok` liefert. Ein `Err` rollt alles zurück.

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

Die Closure bekommt einen eigenen Klon des Pools, keine Leihgabe: Das Future
des Blocks darf den Aufrufer nicht ausleihen. Ein `Pool` lässt sich billig
klonen. Das Makro `rustango::atomic!(&pool, |tx| { … })` schreibt das
`Box::pin` für dich.

## Was im Block läuft

In der Closure läuft jeder ORM-Aufruf mit demselben Pool (oder einem Klon
davon) auf der Transaktion des Blocks:

- Lesezugriffe: `fetch`, `find`, `count`, `get_or_create`, …
- Schreibzugriffe: `insert`, `save`, `delete`, Bulk-Schreibvorgänge, Queryset-Updates
- rohes SQL: `raw_query_pool`, `raw_execute_pool`
- DDL (siehe [DDL in einem Block](#ddl-in-einem-block))
- Schreibvorgänge des Frameworks: Audit-Zeilen, Fixtures, ViewSet- und
  Admin-Schreibvorgänge, Medien, SSO-Registrierung und Einladungen

Sie sehen die noch nicht committeten Zeilen des Blocks, und sein Rollback macht
sie rückgängig. Auf SQLite, und auf MySQL mit seiner Standardstufe
(repeatable read), sehen alle Lesezugriffe im Block einen einzigen Snapshot.

Diese laufen **außerhalb** des Blocks, auf einer eigenen Verbindung:
`pool.acquire()`, das Abholen von Jobs, ein mit `tokio::spawn` gestarteter
Task und ein separat verbundener Pool (auch auf dieselbe Datenbank). Sie sehen
die Schreibvorgänge des Blocks nicht und können auf Sperren warten, die der
Block hält.

Ein `DatabaseCache` auf dem Pool des Blocks liest im Block und stellt seine
Schreibvorgänge bis zum Ende des Blocks zurück; dort lehnt er `incr`, `add`
und DDL ab. Eine Änderung an der Tenancy-Registry in einem Block auf dem
Registry-Pool wird abgelehnt.

Das Argument der Closure ist ein `AtomicTx`. `tx.lock().await?` liefert einen
`TxGuard`, der zu `PoolTx` dereferenziert, für die `_tx`-Helfer und die
`_on`-Formen: `acc.insert_tx(&mut *guard)`. Führe eine Anweisung nach der
anderen aus: Solange ein Guard gehalten wird oder zwei Anweisungen gleichzeitig
laufen (`join!`), liefern Aufrufe auf dem Pool `ExecError::NestedAtomic`. Gib
den Guard vor dem nächsten `&pool`-Aufruf frei.

**Kosten.** Ein Lesezugriff oder eine rohe Anweisung kostet einen Roundtrip.
Ein ORM-Schreibvorgang kostet drei, weil er in einem eigenen Savepoint läuft
(so lässt ein fehlgeschlagener Schreibvorgang den Block benutzbar). Für viele
Zeilen schreibe über den Guard (`insert_tx`) oder mit einem Bulk-Schreibvorgang.

## Verschachtelte Blöcke

`atomic` auf demselben Pool innerhalb eines Blocks öffnet einen Savepoint. Sein
`Err` rollt nur seine eigenen Schreibvorgänge zurück. Ein `Err` des äußeren
Blocks rollt alles zurück, die verschachtelte Arbeit eingeschlossen.

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

(`account(email, balance)` baut einen `Account` mit `id: Auto::Unset`.)

Verschachtelung gilt pro Pool-Objekt: Reiche den Pool des Requests weiter,
statt neu zu verbinden. Ein verworfener oder in Panik geratener verschachtelter Block
wird zurückgerollt. Schlägt eine Savepoint-Anweisung selbst fehl, rollt der
ganze Block mit `ExecError::AtomicAborted` zurück. PostgreSQL wird ab 64
Savepoints in einer Transaktion langsamer, und jeder ORM-Schreibvorgang ist
einer: Halte Blöcke mit vielen Schreibvorgängen kurz.

## Nach dem Commit

`rustango::sql::on_commit(callback)` reiht einen Callback ein, der nach dem
Commit des äußersten Blocks läuft. Bei einem Rollback läuft er nie, und ein
zurückgerollter verschachtelter Block verwirft seine Callbacks. Callbacks
laufen in der Reihenfolge, in der sie eingereiht wurden.

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

Der Callback ist synchron: Starte asynchrone Arbeit aus ihm heraus. Ein
Callback, der in Panik gerät, wird geloggt, und die anderen laufen trotzdem.
`on_commit` außerhalb eines Blocks löst eine Panik aus. Callbacks laufen auf
dem Task des Commits, mit den Task-Locals des Frameworks (Audit-Quelle,
Zeitzone, abgeschaltete Signale), die sie beim Einreihen hatten.

## Wenn eine Anweisung fehlschlägt

| Backend | Ein fehlgeschlagener Lesezugriff oder eine rohe Anweisung | Ein fehlgeschlagener ORM-Schreibvorgang |
|---|---|---|
| PostgreSQL | Bricht den Block ab: Spätere Anweisungen und `atomic` liefern `ExecError::AtomicAborted` | Allein rückgängig gemacht (sein Savepoint); der Block läuft weiter |
| MySQL, MariaDB | Allein rückgängig gemacht; der Block läuft weiter | Allein rückgängig gemacht; der Block läuft weiter |
| SQLite | Allein rückgängig gemacht; der Block läuft weiter | Allein rückgängig gemacht; der Block läuft weiter |

Eine Closure, die einen Fehler ignoriert und `Ok` liefert, committet den Rest
des Blocks (auf PostgreSQL liefert `atomic` nach einem abgebrochenen
Lesezugriff stattdessen `AtomicAborted`).

Manche Fehler beenden auf dem Server die ganze Transaktion: ein MySQL-Deadlock
(1213), ein MySQL-Lock-Wait-Timeout (1205), wenn der Server dabei zurückrollt,
ein automatisches Rollback von SQLite, eine verlorene Verbindung. Jede spätere
ORM-Anweisung im Block und `atomic` selbst liefern dann
`ExecError::AtomicAborted`. MariaDB meldet einen verpassten `nowait()` als 1205;
mit dem Standard `innodb_rollback_on_timeout = OFF` macht es nur die Anweisung
rückgängig.

**Nach einem fehlgeschlagenen Lesezugriff weitermachen.** Um einen Schritt zu
versuchen, der fehlschlagen kann (eine `nowait()`-Sperre, eine Probe auf eine
Tabelle, die es vielleicht nicht gibt), lege ihn in einen verschachtelten Block.
Sein Fehlschlag macht dann nur diesen Block rückgängig, auf jedem Backend:

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

Rohes SQL, das die Transaktion beendet (`COMMIT`, `ROLLBACK` oder eine
Anweisung, bei der MySQL committet), lässt den Block
`ExecError::AtomicEndedEarly` liefern. Anweisungen danach liefen ohne
Transaktion, also nicht blind wiederholen.

## SQLite nimmt die Schreibsperre beim BEGIN

Auf SQLite beginnt der äußerste Block mit `BEGIN IMMEDIATE`. Ein Block, der
liest und dann schreibt, scheitert nicht bei seinem ersten Schreibvorgang mit
`SQLITE_BUSY`; stattdessen wartet ein zweiter Schreiber bei seinem eigenen
`BEGIN`. Andere Verbindungen, die schreiben, warten `busy_timeout` (standardmäßig
5 Sekunden) und scheitern dann mit `SQLITE_BUSY`; auf einer In-Memory-Datenbank
warten sie ohne Timeout. Halte SQLite-Blöcke kurz.

`AtomicOptions::deferred()` beginnt stattdessen mit einem einfachen `BEGIN`:
Ein Block, der nur liest, nimmt keine Schreibsperre, und sein erster
Schreibvorgang nach einem Lesen schlägt sofort mit `SQLITE_BUSY` fehl, wenn
eine andere Verbindung inzwischen geschrieben hat. Auf PostgreSQL und MySQL,
deren `BEGIN` nie sperrt, wirkt es nicht.

## Isolationsstufen und Einstellungen

`atomic_with(&pool, options, closure)` beginnt den äußersten Block mit einer
Isolationsstufe, PostgreSQL-Einstellungen oder beidem:

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

- **Stufen:** `Isolation::ReadCommitted`, `RepeatableRead`, `Serializable`.
  PostgreSQL beginnt mit `BEGIN ISOLATION LEVEL …`. MySQL setzt die Stufe nur
  für diese Transaktion; der nächste Block auf der Verbindung hat wieder den
  Server-Standard. SQLite akzeptiert jede Stufe: Es ist immer serialisierbar.
  Ohne Stufe nutzt der Block den Server-Standard (PostgreSQL: read committed;
  MySQL: repeatable read).
- **Einstellungen** (`TxSetting`) gibt es nur auf PostgreSQL, zum Beispiel
  `app.account_id` für Row-Security-Policies. Jede wird mit
  `set_config(name, value, true)` gesetzt und ist am Ende des Blocks weg. Der
  Wert wird gebunden. Der Name muss Punkte enthalten (`prefix.name`) und jeder
  Teil ein schlichter Bezeichner sein, also liefern `search_path` oder `role`
  `ExecError::InvalidTxSetting`. Auf MySQL und SQLite liefern Einstellungen
  `ExecError::Sql(SqlError::OperatorNotSupportedInDialect)`, bevor der Block
  beginnt.
- **Verschachtelte Blöcke** können das nicht ändern: `atomic_with` mit Optionen
  in einem Block auf demselben Pool liefert `ExecError::InAtomic`.

## Läuft dieser Code in einem Block?

`rustango::sql::in_atomic(&pool)` ist `true`, solange der laufende Task in einem
Block auf `pool` (oder einem Klon davon) steckt, sodass ORM-Aufrufe mit `pool`
auf dessen Transaktion laufen. Außerhalb und für jeden anderen Pool ist es
`false`. In einem fehlgeschlagenen oder früh beendeten Block bleibt es `true`.

## Get or create in einer Transaktion

`get_or_create_tx` und `update_or_create_tx` nehmen ein offenes `PoolTx`. In
einem Block sperre den Guard und übergib ihn:

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

**Wettläufe.** Die Erzeugungs-Closure läuft in einem Savepoint. Wenn zwei
Transaktionen gleichzeitig dieselbe Zeile anlegen, trifft das Insert des
Verlierers auf den Unique-Constraint: Sein Savepoint rollt zurück, und er liest
die Zeile des Gewinners (`created == false`; `update_or_create_tx` aktualisiert
sie dann). Die Transaktion bleibt benutzbar. Das braucht einen
`UNIQUE`-Constraint auf den gefilterten Spalten. Ein `get_or_create_tx` in der
Erzeugungs-Closure eines anderen (erst eine Elternzeile, dann das Kind)
funktioniert genauso.

| Backend und Stufe | Der Verlierer bekommt |
|---|---|
| PostgreSQL read committed (Standard) | Die Zeile des Gewinners |
| PostgreSQL repeatable read | Den Unique-Violation-Fehler |
| PostgreSQL serializable | Einen Serialisierungsfehler (40001) |
| MySQL, MariaDB | Die Zeile des Gewinners (mit sperrendem Lesen neu gelesen) |
| SQLite | Kein Wettlauf: Blöcke laufen nacheinander |

Unter repeatable read oder serializable auf PostgreSQL wiederhole den ganzen
Block.

Die Pool-Formen `get_or_create` und `update_or_create` verhalten sich in einem
Block genauso: Ihre Lesezugriffe treten ihm bei, und die Erzeugungs-Closure
läuft in einem Savepoint. Halte den Guard nicht über sie hinweg.

## Die `_on`-Formen

Die `_on`-Methoden nehmen jeden Executor: einen `&Pool`, eine gehaltene
Verbindung (`&mut PoolConn` aus `pool.acquire()`) oder eine offene Transaktion
(`&mut PoolTx` aus `transaction_pool` oder `&mut *guard` in einem Block). Die
Anweisung läuft auf dem, was du übergibst.

- `fetch_on`, `insert_on`, `save_on`, `delete_on`, `upsert_on`
- `Model::bulk_insert_on`, `Model::bulk_upsert_on`, `Model::bulk_insert_or_ignore_on`
- `rustango::sql::bulk_update_on`
- `rustango::sql::fetch_dates_on`, `rustango::sql::fetch_datetimes_on`
- `rustango::sql::fetch_with_prefetch_on`
- `rustango::sql::model_shortcuts::increment_one_on`
- Queryset-Updates und -Löschungen: `.update().execute_on(..)`, `.delete_on(..)`

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

Auf einem auditierten Model liefern die abgeleiteten `_on`-Schreibvorgänge und
`increment_one_on` `ExecError::AuditUnsupported`: Sie können die Audit-Zeilen
nicht schreiben. Nutze dort die Pool-Formen.

## DDL in einem Block

- **PostgreSQL und SQLite:** DDL läuft in der Transaktion des Blocks und rollt
  mit ihr zurück.
- **MySQL:** DDL committet die Transaktion. `run_ddl_idempotent` (und die darauf
  aufbauenden `ensure_table`-Helfer) verweigert in einem Block mit einem
  `sqlx::Error::Protocol`, dessen Meldung mit `DDL on MySQL is refused inside
  an atomic block on its pool` beginnt. Rohes DDL über `raw_execute_pool` läuft,
  committet die bisherige Arbeit, und der Block liefert
  `ExecError::AtomicEndedEarly`.
- **Jedes Backend:** Eine Migration mit `"atomic": false` liefert
  `MigrateError::Exec(ExecError::InAtomic { .. })`.

Führe DDL und Migrationen vor oder nach dem Block aus.

## Iterieren in einem Block

In einem Block auf demselben Pool führt `.iterator(n)` jeden Chunk auf der
Transaktion des Blocks aus. Er sieht die Schreibvorgänge des Blocks, behält die
Zeilensperren von `select_for_update()` und blättert nach Primärschlüssel
(`WHERE pk > last`) statt mit `OFFSET`. Zeilen, die die Schleife aus dem Filter
schiebt, werden nicht übersprungen:

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

Das Queryset muss nach dem Primärschlüssel sortiert sein (in beide Richtungen)
oder gar nicht, den Primärschlüssel auswählen und darf kein `union` /
`intersection` / `difference` sein. Sonst liefert `next_chunk`
`ExecError::InAtomic`.

## Abbruch

Das Verwerfen des Futures eines Blocks (ein `tokio::time::timeout`, ein Client,
der weggeht) rollt den Block zurück, und seine `on_commit`-Callbacks laufen nie:

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

Lief beim Verwerfen gerade eine Anweisung, stoppt der Server sie nicht. Die
Verbindung bleibt belegt, bis der Server diese Anweisung beendet hat; dann läuft
das Rollback, und die Verbindung geht zurück an den Pool. In der Zwischenzeit
können einem kleinen Pool Verbindungen fehlen. Ein verworfener verschachtelter
Block wird bei der nächsten Benutzung des Blocks zurückgerollt.

## Eine Transaktion pro Anfrage

`atomic_requests` führt jede schreibende Anfrage an die Routen deiner App in
einem `atomic`-Block auf dem Pool der Anfrage aus. Eine Antwort, die nicht 2xx
oder 3xx ist, oder ein Panic rollt jeden Schreibvorgang zurück, den der
Handler dort gemacht hat.

```rust
use rustango::atomic_request::AtomicRequests;
use rustango::sql::{AtomicOptions, Isolation};

// Tenancy: each block runs on the request's tenant pool.
let app = rustango::server::Builder::from_env().await?
    .api(routes)
    .atomic_requests(AtomicRequests::new())
    .into_router()
    .await?;

// One database: each block runs on the builder's pool.
rustango::server::AppBuilder::from_env().await?
    .api(other_routes)
    .atomic_requests(AtomicRequests::new().options(
        AtomicOptions::new().isolation(Isolation::ReadCommitted),
    ))
    .serve("0.0.0.0:8080")
    .await?;
```

- Nur POST, PUT, PATCH, DELETE und andere unsichere Methoden. GET, HEAD,
  OPTIONS, TRACE und QUERY laufen ohne Transaktion; eine GET-Route, die
  schreibt (ein SSO-Callback), bleibt im Autocommit.
- Nur die eigenen Routen deiner App: Tenant-Admin, Login, SSO und
  Operator-Konsole werden nicht umhüllt, ebenso wenig Routen aus
  `.auth_routes(..)`. Hänge `JwtAuth::router()` dort ein, statt ihn in
  `.api()` zu mergen.
- Jeder Tenant-Extraktor der Anfrage (`TenantScope`, `Tenant`) liefert den
  Pool, auf dem der Block läuft, auch wenn der Pool-Cache ihn inzwischen
  verdrängt. Eine Anfrage ohne Tenant läuft durch; ein Tenant, dessen Pool
  nicht zu finden ist, bekommt 500, nie den Registry-Pool.
- Jede schreibende Anfrage hält vom Anfang bis zum Ende ihres Handlers eine
  Verbindung, also laufen höchstens `max_connections` schreibende Anfragen
  pro Pool gleichzeitig. Bemiss den Pool für deine gleichzeitigen
  Schreibvorgänge in der Spitze plus Lesevorgänge, und halte langsame
  ausgehende Aufrufe (HTTP, E-Mail) aus schreibenden Handlern heraus oder
  verlege sie in `on_commit`. Der Start lehnt `max_cached_scoped_pools = 0`
  oder `max_cached_database_pools = 0` ab und warnt, wenn
  `scoped_pool_max_connections` unter 8 liegt.

**Reihenfolge.** Der Builder setzt den Block innerhalb seiner eigenen Layer
(Panic-Schutz, Access-Log, Tracing, Tenant-Verteilung) und um deinen Router.
Layer auf deinem Router (Sessions, CSRF, Auth, Idempotenz) laufen im Block:
Was sie auf dem Pool schreiben, wird mit der Anfrage committet oder
zurückgerollt. Ein `IdempotencyLayer` im Block speichert eine Antwort erst
nach dem `COMMIT`; einen Nicht-2xx-Status, den er speichern soll, speichert er
dort also nicht, denn diese Antwort wird zurückgerollt. Auf einem selbst
gebauten Router füge
`AtomicRequestLayer::new(pool)` oder `AtomicRequestLayer::tenant()` vor den
Layern hinzu, die die endgültige Antwort sehen müssen.

**Ein fehlgeschlagener Commit.** Die Antwort des Handlers wird verworfen, mit
ihren Cookies und ihrer Weiterleitung, und eine neue wird gesendet. Nichts
wird wiederholt: Handler haben Nebenwirkungen.

| Ursache | Antwort |
|---|---|
| Serialisierungsfehler (40001), Deadlock, MySQL-Sperr-Timeout (1205), `SQLITE_BUSY`, keine freie Verbindung | 503 mit `Retry-After: 1` |
| `AtomicAborted`, `AtomicEndedEarly`, jeder andere Fehler | 500, mit der Request-ID geloggt |

Dieselbe 503 ersetzt eine Antwort, die zurückrollt, nachdem eine Anweisung im
Block einen solchen Konflikt verloren hat: ein SQLite-Schreibvorgang nach
einem Lesen, ein MySQL-Deadlock. Ein ViewSet-Schreibvorgang, der einen
verliert, antwortet selbst mit 503.

**Was dem Block entkommt.**

- Ein mit `tokio::spawn` gestarteter Task committet selbst; starte ihn aus
  `on_commit(|| ..)`.
- Ein Streaming-Body läuft nach dem `COMMIT`.
- Zwei Anweisungen gleichzeitig in einem Handler (`join!`) liefern
  `NestedAtomic`.
- `outside_atomic(work)` stellt `work` bis zum Ende des Blocks zurück und
  führt es nach dem `COMMIT` oder `ROLLBACK` auf einer eigenen Verbindung
  aus: Es übersteht ein Rollback und wartet nie auf Sperren des Blocks.
  Zurückgestellt liefert es `Ok(None)`; ein Fehler dabei wird geloggt. Das
  Framework schreibt so Sicherheits-Auditzeilen, Logout-Stempel und
  Berichte der Login-Drosselung. Ein Schreibvorgang, dessen Ergebnis sofort
  gebraucht wird, kann nicht warten: Ein `DatabaseCache` auf dem Pool der
  Anfrage lehnt dort `incr` und `add` ab, und die Login-Drosselung zählt
  dann im Prozess.

**`Tenant::conn`.** In einem Block auf dem Pool des Tenants leiht `t.conn()`
die Verbindung des Blocks aus, sodass ihre Anweisungen mit der Anfrage
committet oder zurückgerollt werden. ORM-Aufrufe auf dem Pool liefern
währenddessen `NestedAtomic`; `t.pool()` gibt sie zurück. `t.pool_conn()` und
`t.into_conn()` liefern dort `InAtomic`.

**Je Backend.** PostgreSQL: Ein Anweisungsfehler, den der Handler ignoriert,
bricht den Block ab, und die Anfrage antwortet mit 500. MySQL:
`REPEATABLE READ` hält einen Snapshot für die ganze Anfrage, daher passt
`Isolation::ReadCommitted` meist besser; DDL committet den Block vorzeitig
(500). SQLite: Blöcke beginnen verzögert, ein POST, der nur liest, nimmt also
keine Schreibsperre; ein Schreiben nach einem Lesen schlägt sofort mit
`SQLITE_BUSY` (503) fehl, wenn eine andere Verbindung inzwischen geschrieben
hat.

**Kosten.** `BEGIN` und `COMMIT` pro schreibender Anfrage, eine Prüfung vor
dem Commit und ein Savepoint um jeden ORM-Schreibvorgang. Auf einem lokalen
PostgreSQL kostet ein POST mit einem Schreibvorgang allein etwa 1 ms mehr im
p99, unter Last mehr.

## Fehler

| Fehler | Bedeutung |
|---|---|
| `ExecError::NestedAtomic` | Eine Anweisung lief, während ein `TxGuard` gehalten wurde, oder zwei liefen gleichzeitig (`join!`) |
| `ExecError::AtomicAborted` | Die Transaktion wurde nach einer fehlgeschlagenen Anweisung oder einem Savepoint zurückgerollt |
| `ExecError::AtomicEndedEarly` | Rohes SQL hat die Transaktion committet oder beendet; manche Schreibvorgänge sind womöglich committet |
| `ExecError::InAtomic` | Die Operation wird in einem Block auf ihrem Pool verweigert |
| `ExecError::InvalidTxSetting` | Ein `TxSetting`-Name ist kein `prefix.name` mit Punkt |
