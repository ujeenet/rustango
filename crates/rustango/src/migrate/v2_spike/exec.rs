//! SPIKE S3: run rendered units on a live pool, as the v2 runner would
//! (no ledger, no fencing). Kept apart from `render` so rendering never
//! sees a pool.

use super::render::{Unit, UnitKind};
use crate::sql::{raw_execute_pool, raw_execute_tx, Pool};

/// Run `units` in order; stops at the first error with the unit and statement.
///
/// # Errors
/// The failing statement and the driver message.
pub async fn run_units(pool: &Pool, units: &[Unit]) -> Result<(), String> {
    for u in units {
        run_unit(pool, u)
            .await
            .map_err(|e| format!("{} unit {u:?}: {e}", pool.dialect().name()))?;
    }
    Ok(())
}

async fn run_unit(pool: &Pool, u: &Unit) -> Result<(), String> {
    match u.kind {
        UnitKind::Autocommit => {
            for s in &u.sql {
                raw_execute_pool(pool, s, Vec::new())
                    .await
                    .map_err(|e| format!("{s}: {e}"))?;
            }
            Ok(())
        }
        UnitKind::Tx => {
            let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
            let mut tx = conn.begin().await.map_err(|e| e.to_string())?;
            for s in &u.sql {
                raw_execute_tx(&mut tx, s, Vec::new())
                    .await
                    .map_err(|e| format!("{s}: {e}"))?;
            }
            tx.commit().await.map_err(|e| e.to_string())
        }
        UnitKind::Rebuild => run_rebuild(pool, u).await,
    }
}

/// v1's `RebuildConn`: FK enforcement off outside the tx, then
/// `foreign_key_check` inside it before commit.
#[cfg(feature = "sqlite")]
async fn run_rebuild(pool: &Pool, u: &Unit) -> Result<(), String> {
    #[allow(irrefutable_let_patterns)]
    let Pool::Sqlite(sq) = pool
    else {
        return Err("a rebuild unit is SQLite only".into());
    };
    let mut conn = crate::migrate::rebuild::RebuildConn::acquire(sq, true)
        .await
        .map_err(|e| e.to_string())?;
    let result: Result<(), String> = async {
        let mut tx = conn.begin().await.map_err(|e| e.to_string())?;
        for s in &u.sql {
            sqlx::query(s)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("{s}: {e}"))?;
        }
        let orphans: Vec<(String,)> =
            sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        if !orphans.is_empty() {
            return Err(format!("rebuild left FK orphans in {orphans:?}"));
        }
        tx.commit().await.map_err(|e| e.to_string())
    }
    .await;
    let finished = conn.finish().await.map_err(|e| e.to_string());
    result?;
    finished
}

#[cfg(not(feature = "sqlite"))]
async fn run_rebuild(_pool: &Pool, _u: &Unit) -> Result<(), String> {
    Err("a rebuild unit is SQLite only".into())
}
