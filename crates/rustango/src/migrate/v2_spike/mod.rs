//! SPIKE S6 for migrations engine v2. Throwaway code that answers: can a
//! live catalog reader plus normalisers tell, per op and per dialect,
//! whether an op's effect is in the database, and are the raw-text
//! defaults of design 2.6 valid where we say they are?
//! SPIKE S3 adds `render` (pure per-op units) and `exec` (runs them live).
//! SPIKE S4 adds `lock` (`LockScope`), `tenant_sql` (load scan) and `tenancy`.

pub mod catalog;
pub mod defaults;
pub mod exec;
pub mod lock;
pub mod norm;
pub mod ops;
pub mod perf;
pub mod probe;
pub mod render;
pub mod tenancy;
pub mod tenant_sql;
