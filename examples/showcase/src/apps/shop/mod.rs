//! `shop` sub-app — exercises `FieldType::Decimal` (money fields)
//! round-tripping through PG NUMERIC / MySQL DECIMAL / SQLite TEXT,
//! plus per-query filtering. Phase 3 of the E2E plan.

pub mod models;
pub mod urls;
