//! The live catalog reader, one table at a time, on PG, MySQL and SQLite.
//!
//! Every value comes back as SQL text (defaults, expressions) so the
//! normalisers in [`super::norm`] compare it with what a v2 file declares.
//! Catalog SQL is per backend by nature, like `inspectdb`.

use super::norm::{self, Tok};
use crate::core::SqlValue;
use crate::sql::{raw_query_pool, DialectKind, Pool};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTable {
    pub name: String,
    /// `None` on SQLite: no table comments (`probe: none`).
    pub comment: Option<String>,
    pub columns: Vec<LiveColumn>,
    pub indexes: Vec<LiveIndex>,
    pub checks: Vec<LiveCheck>,
    pub fks: Vec<LiveFk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveColumn {
    pub name: String,
    /// The catalog's type text (PG `format_type`, MySQL `COLUMN_TYPE`,
    /// SQLite declared type).
    pub raw_type: String,
    pub nullable: bool,
    /// DEFAULT as SQL text (MySQL literals re-quoted, escapes removed).
    pub default: Option<String>,
    pub generated: Option<LiveGenerated>,
    pub comment: Option<String>,
    /// Identity, serial, AUTO_INCREMENT or a ROWID alias.
    pub auto: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveGenerated {
    pub expr: String,
    pub stored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveIndex {
    pub name: String,
    /// Key columns, or the expression text for an expression key.
    pub columns: Vec<String>,
    pub include: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// Access method, lowercase. MySQL InnoDB reports `btree` for `USING HASH`.
    pub method: String,
    pub predicate: Option<String>,
    /// PG `indisvalid AND indisready`; always `true` elsewhere.
    pub valid: bool,
    /// Backs a UNIQUE / PRIMARY KEY constraint (PG `conindid`, SQLite origin `u`/`pk`).
    pub constraint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCheck {
    /// `None` for an unnamed SQLite CHECK.
    pub name: Option<String>,
    pub expr: String,
    /// PG `convalidated` (`NOT VALID` → false); MySQL `ENFORCED`.
    pub validated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveFk {
    /// `None` for an unnamed SQLite FK.
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    /// SQL token; `RESTRICT` and `NO ACTION` stay distinct here.
    pub on_delete: String,
    pub on_update: String,
    pub validated: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("catalog read: {0}")]
pub struct CatalogError(pub String);

impl From<crate::sql::ExecError> for CatalogError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self(e.to_string())
    }
}

impl From<norm::TokenizeError> for CatalogError {
    fn from(e: norm::TokenizeError) -> Self {
        Self(e.0)
    }
}

/// Read `table` from the pool's current schema/database; `None` when absent.
///
/// # Errors
/// Driver failures, or SQLite DDL text that does not parse.
pub async fn read_table(pool: &Pool, table: &str) -> Result<Option<LiveTable>, CatalogError> {
    match pool.dialect().kind() {
        DialectKind::Postgres => pg::read(pool, table).await,
        DialectKind::Mysql => my::read(pool, table).await,
        DialectKind::Sqlite => lite::read(pool, table).await,
        #[allow(unreachable_patterns)]
        other => Err(CatalogError(format!("no catalog reader for {other:?}"))),
    }
}

const SEP: char = '\u{1f}';

fn split(s: Option<String>) -> Vec<String> {
    s.map(|s| s.split(SEP).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn t(s: &str) -> Vec<SqlValue> {
    vec![SqlValue::from(s.to_owned())]
}

mod pg {
    use super::*;

    const OID: &str = "(SELECT c.oid FROM pg_class c WHERE c.relname = $1 \
        AND c.relnamespace = current_schema()::regnamespace AND c.relkind IN ('r', 'p'))";

    fn action(sql_col: &str) -> String {
        format!(
            "CASE {sql_col} WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' \
             WHEN 'r' THEN 'RESTRICT' ELSE 'NO ACTION' END"
        )
    }

    pub(super) async fn read(pool: &Pool, table: &str) -> Result<Option<LiveTable>, CatalogError> {
        let head: Vec<(Option<String>,)> = raw_query_pool(
            &format!("SELECT obj_description(o.oid, 'pg_class') FROM (SELECT {OID} AS oid) o WHERE o.oid IS NOT NULL"),
            t(table),
            pool,
        )
        .await?;
        let Some((comment,)) = head.into_iter().next() else {
            return Ok(None);
        };
        let cols: Vec<(String, String, i64, Option<String>, Option<String>, i64, Option<String>, i64)> = raw_query_pool(
            &format!(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), \
                 CASE WHEN a.attnotnull THEN 0 ELSE 1 END::int8, \
                 CASE WHEN a.attgenerated = '' THEN pg_get_expr(d.adbin, d.adrelid) END, \
                 CASE WHEN a.attgenerated <> '' THEN pg_get_expr(d.adbin, d.adrelid) END, \
                 CASE WHEN a.attgenerated = 's' THEN 1 ELSE 0 END::int8, \
                 col_description(a.attrelid, a.attnum), \
                 CASE WHEN a.attidentity <> '' OR pg_get_expr(d.adbin, d.adrelid) LIKE 'nextval(%' THEN 1 ELSE 0 END::int8 \
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = {OID} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
            ),
            t(table),
            pool,
        )
        .await?;
        let columns = cols
            .into_iter()
            .map(
                |(name, raw_type, nullable, default, gen, stored, comment, auto)| LiveColumn {
                    name,
                    raw_type,
                    nullable: nullable == 1,
                    // A serial's nextval is the auto flag, not a declared default.
                    default: default.filter(|_| auto == 0),
                    generated: gen.map(|expr| LiveGenerated {
                        expr,
                        stored: stored == 1,
                    }),
                    comment,
                    auto: auto == 1,
                },
            )
            .collect();
        let idx: Vec<(String, i64, i64, String, Option<String>, Option<String>, Option<String>, i64, i64)> = raw_query_pool(
            &format!(
                "SELECT ic.relname::text, i.indisunique::int::int8, i.indisprimary::int::int8, am.amname::text, \
                 (SELECT string_agg(pg_get_indexdef(i.indexrelid, k, true), chr(31) ORDER BY k) \
                    FROM generate_series(1, i.indnkeyatts) k), \
                 (SELECT string_agg(pg_get_indexdef(i.indexrelid, k, true), chr(31) ORDER BY k) \
                    FROM generate_series(i.indnkeyatts + 1, i.indnatts) k), \
                 pg_get_expr(i.indpred, i.indrelid), (i.indisvalid AND i.indisready)::int::int8, \
                 EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = i.indexrelid \
                    AND k.conrelid = i.indrelid AND k.contype IN ('u', 'p', 'x'))::int::int8 \
                 FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid JOIN pg_am am ON am.oid = ic.relam \
                 WHERE i.indrelid = {OID} ORDER BY 1"
            ),
            t(table),
            pool,
        )
        .await?;
        let indexes = idx
            .into_iter()
            .map(
                |(name, unique, primary, method, keys, include, predicate, valid, constraint)| {
                    LiveIndex {
                        name,
                        columns: split(keys).into_iter().map(|c| unquote_pg(&c)).collect(),
                        include: split(include).into_iter().map(|c| unquote_pg(&c)).collect(),
                        unique: unique == 1,
                        primary: primary == 1,
                        method,
                        predicate,
                        valid: valid == 1,
                        constraint: constraint == 1,
                    }
                },
            )
            .collect();
        let checks: Vec<(String, String, i64)> = raw_query_pool(
            &format!(
                "SELECT conname::text, pg_get_expr(conbin, conrelid), convalidated::int::int8 FROM pg_constraint \
                 WHERE conrelid = {OID} AND contype = 'c' ORDER BY 1"
            ),
            t(table),
            pool,
        )
        .await?;
        let checks = checks
            .into_iter()
            .map(|(name, expr, v)| LiveCheck {
                name: Some(name),
                expr,
                validated: v == 1,
            })
            .collect();
        let fks: Vec<(String, Option<String>, String, Option<String>, String, String, i64)> = raw_query_pool(
            &format!(
                "SELECT c.conname::text, \
                 (SELECT string_agg(a.attname::text, chr(31) ORDER BY k.ord) FROM unnest(c.conkey) WITH ORDINALITY k(n, ord) \
                    JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n), \
                 r.relname::text, \
                 (SELECT string_agg(a.attname::text, chr(31) ORDER BY k.ord) FROM unnest(c.confkey) WITH ORDINALITY k(n, ord) \
                    JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.n), \
                 {}, {}, c.convalidated::int::int8 \
                 FROM pg_constraint c JOIN pg_class r ON r.oid = c.confrelid \
                 WHERE c.conrelid = {OID} AND c.contype = 'f' ORDER BY 1",
                action("c.confdeltype"),
                action("c.confupdtype")
            ),
            t(table),
            pool,
        )
        .await?;
        let fks = fks
            .into_iter()
            .map(
                |(name, cols, ref_table, ref_cols, on_delete, on_update, v)| LiveFk {
                    name: Some(name),
                    columns: split(cols),
                    ref_table,
                    ref_columns: split(ref_cols),
                    on_delete,
                    on_update,
                    validated: v == 1,
                },
            )
            .collect();
        Ok(Some(LiveTable {
            name: table.to_owned(),
            comment,
            columns,
            indexes,
            checks,
            fks,
        }))
    }

    fn unquote_pg(s: &str) -> String {
        match s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            Some(inner) => inner.replace("\"\"", "\""),
            None => s.to_owned(),
        }
    }
}

mod my {
    use super::*;

    const WHERE: &str = "TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?";

    pub(super) async fn read(pool: &Pool, table: &str) -> Result<Option<LiveTable>, CatalogError> {
        let head: Vec<(String,)> = raw_query_pool(
            &format!(
                "SELECT CAST(TABLE_COMMENT AS CHAR) FROM information_schema.TABLES WHERE {WHERE}"
            ),
            t(table),
            pool,
        )
        .await?;
        let Some((comment,)) = head.into_iter().next() else {
            return Ok(None);
        };
        type ColRow = (
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            String,
        );
        let cols: Vec<ColRow> = raw_query_pool(
            &format!(
                "SELECT CAST(COLUMN_NAME AS CHAR), CAST(COLUMN_TYPE AS CHAR), CAST(DATA_TYPE AS CHAR), \
                 CAST(IS_NULLABLE AS CHAR), CAST(COLUMN_DEFAULT AS CHAR), CAST(EXTRA AS CHAR), \
                 CAST(GENERATION_EXPRESSION AS CHAR), CAST(COLUMN_COMMENT AS CHAR) \
                 FROM information_schema.COLUMNS WHERE {WHERE} ORDER BY ORDINAL_POSITION"
            ),
            t(table),
            pool,
        )
        .await?;
        let columns = cols
            .into_iter()
            .map(
                |(name, raw_type, data_type, nullable, default, extra, gen, comment)| {
                    let up = extra.to_ascii_uppercase();
                    let generated = (up.contains("STORED GENERATED")
                        || up.contains("VIRTUAL GENERATED"))
                    .then(|| LiveGenerated {
                        expr: norm::mysql_unescape_catalog(gen.as_deref().unwrap_or_default()),
                        stored: up.contains("STORED"),
                    });
                    LiveColumn {
                        name,
                        raw_type,
                        nullable: nullable.eq_ignore_ascii_case("YES"),
                        default: default
                            .filter(|_| generated.is_none())
                            .map(|d| norm::mysql_catalog_default_sql(&d, &data_type, &extra)),
                        generated,
                        comment: Some(comment).filter(|c| !c.is_empty()),
                        auto: up.contains("AUTO_INCREMENT"),
                    }
                },
            )
            .collect();
        let stats: Vec<(String, i64, Option<String>, Option<String>, String)> = raw_query_pool(
            &format!(
                "SELECT CAST(INDEX_NAME AS CHAR), CAST(NON_UNIQUE AS SIGNED), CAST(COLUMN_NAME AS CHAR), \
                 CAST(EXPRESSION AS CHAR), CAST(INDEX_TYPE AS CHAR) \
                 FROM information_schema.STATISTICS WHERE {WHERE} ORDER BY INDEX_NAME, SEQ_IN_INDEX"
            ),
            t(table),
            pool,
        )
        .await?;
        let mut indexes: Vec<LiveIndex> = Vec::new();
        for (name, non_unique, col, expr, ty) in stats {
            let key = col.unwrap_or_else(|| {
                norm::mysql_unescape_catalog(expr.as_deref().unwrap_or_default())
            });
            if let Some(ix) = indexes.last_mut().filter(|ix| ix.name == name) {
                ix.columns.push(key);
                continue;
            }
            indexes.push(LiveIndex {
                primary: name == "PRIMARY",
                constraint: non_unique == 0,
                name,
                columns: vec![key],
                include: Vec::new(),
                unique: non_unique == 0,
                method: ty.to_ascii_lowercase(),
                predicate: None,
                valid: true,
            });
        }
        let checks: Vec<(String, String, i64)> = raw_query_pool(
            "SELECT CAST(tc.CONSTRAINT_NAME AS CHAR), CAST(cc.CHECK_CLAUSE AS CHAR), \
             CAST(tc.ENFORCED = 'YES' AS SIGNED) \
             FROM information_schema.TABLE_CONSTRAINTS tc JOIN information_schema.CHECK_CONSTRAINTS cc \
               ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
             WHERE tc.TABLE_SCHEMA = DATABASE() AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK' ORDER BY 1",
            t(table),
            pool,
        )
        .await?;
        let checks = checks
            .into_iter()
            .map(|(name, expr, enforced)| LiveCheck {
                name: Some(name),
                expr: norm::mysql_unescape_catalog(&expr),
                validated: enforced == 1,
            })
            .collect();
        let fk_rows: Vec<(String, String, String, String, String, String)> = raw_query_pool(
            "SELECT CAST(k.CONSTRAINT_NAME AS CHAR), CAST(k.COLUMN_NAME AS CHAR), \
             CAST(k.REFERENCED_TABLE_NAME AS CHAR), CAST(k.REFERENCED_COLUMN_NAME AS CHAR), \
             CAST(r.DELETE_RULE AS CHAR), CAST(r.UPDATE_RULE AS CHAR) \
             FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
               ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
               AND r.TABLE_NAME = k.TABLE_NAME \
             WHERE k.TABLE_SCHEMA = DATABASE() AND k.TABLE_NAME = ? \
             ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
            t(table),
            pool,
        )
        .await?;
        let mut fks: Vec<LiveFk> = Vec::new();
        for (name, col, rt, rc, on_delete, on_update) in fk_rows {
            if let Some(fk) = fks
                .last_mut()
                .filter(|f| f.name.as_deref() == Some(name.as_str()))
            {
                fk.columns.push(col);
                fk.ref_columns.push(rc);
                continue;
            }
            fks.push(LiveFk {
                name: Some(name),
                columns: vec![col],
                ref_table: rt,
                ref_columns: vec![rc],
                on_delete,
                on_update,
                validated: true,
            });
        }
        Ok(Some(LiveTable {
            name: table.to_owned(),
            comment: Some(comment).filter(|c| !c.is_empty()),
            columns,
            indexes,
            checks,
            fks,
        }))
    }
}

mod lite {
    use super::*;

    pub(super) async fn read(pool: &Pool, table: &str) -> Result<Option<LiveTable>, CatalogError> {
        let head: Vec<(String,)> = raw_query_pool(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
            t(table),
            pool,
        )
        .await?;
        let Some((create,)) = head.into_iter().next() else {
            return Ok(None);
        };
        let parsed = parse_create_table(&create)?;
        let cols: Vec<(String, String, i64, Option<String>, i64, i64)> = raw_query_pool(
            "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?) ORDER BY cid",
            t(table),
            pool,
        )
        .await?;
        let pk_count = cols.iter().filter(|c| c.4 > 0).count();
        let autoinc = create.to_ascii_uppercase().contains("AUTOINCREMENT");
        let columns = cols
            .into_iter()
            .map(|(name, raw_type, notnull, default, pk, hidden)| {
                let p = parsed
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&name));
                let generated = (hidden == 2 || hidden == 3).then(|| LiveGenerated {
                    expr: p.and_then(|c| c.generated.clone()).unwrap_or_default(),
                    stored: hidden == 3,
                });
                let rowid = pk == 1 && pk_count == 1 && raw_type.eq_ignore_ascii_case("INTEGER");
                LiveColumn {
                    name,
                    raw_type,
                    nullable: notnull == 0 && pk == 0,
                    default,
                    generated,
                    comment: None,
                    auto: rowid || (autoinc && pk == 1),
                }
            })
            .collect();
        let list: Vec<(String, i64, String, i64)> = raw_query_pool(
            "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?) ORDER BY name",
            t(table),
            pool,
        )
        .await?;
        let mut indexes = Vec::new();
        for (name, unique, origin, _partial) in list {
            let keys: Vec<(Option<String>,)> = raw_query_pool(
                "SELECT name FROM pragma_index_xinfo(?) WHERE key = 1 ORDER BY seqno",
                t(&name),
                pool,
            )
            .await?;
            let sql: Vec<(Option<String>,)> = raw_query_pool(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?",
                t(&name),
                pool,
            )
            .await?;
            let parsed_ix = sql
                .into_iter()
                .next()
                .and_then(|(s,)| s)
                .map(|s| parse_create_index(&s))
                .transpose()?;
            let columns = match &parsed_ix {
                Some(p) => p.columns.clone(),
                None => keys.into_iter().map(|(k,)| k.unwrap_or_default()).collect(),
            };
            indexes.push(LiveIndex {
                name,
                columns,
                include: Vec::new(),
                unique: unique == 1,
                primary: origin == "pk",
                method: "btree".to_owned(),
                predicate: parsed_ix.and_then(|p| p.predicate),
                valid: true,
                constraint: origin != "c",
            });
        }
        // A UNIQUE constraint's index is `sqlite_autoindex_*`; its name lives in the DDL.
        for u in &parsed.uniques {
            if let Some(ix) = indexes.iter_mut().find(|ix| {
                ix.constraint
                    && !ix.primary
                    && ix.columns == u.1
                    && ix.name.starts_with("sqlite_autoindex_")
            }) {
                if let Some(n) = &u.0 {
                    ix.name.clone_from(n);
                }
            }
        }
        let checks = parsed
            .checks
            .iter()
            .map(|(name, expr)| LiveCheck {
                name: name.clone(),
                expr: expr.clone(),
                validated: true,
            })
            .collect();
        let fk_rows: Vec<(i64, i64, String, String, Option<String>, String, String)> = raw_query_pool(
            "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list(?) \
             ORDER BY id, seq",
            t(table),
            pool,
        )
        .await?;
        let mut fks: Vec<(i64, LiveFk)> = Vec::new();
        for (id, _seq, rt, from, to, on_update, on_delete) in fk_rows {
            if let Some((_, fk)) = fks.iter_mut().find(|(i, _)| *i == id) {
                fk.columns.push(from);
                fk.ref_columns.push(to.unwrap_or_default());
                continue;
            }
            fks.push((
                id,
                LiveFk {
                    name: None,
                    columns: vec![from],
                    ref_table: rt,
                    ref_columns: vec![to.unwrap_or_default()],
                    on_delete,
                    on_update,
                    validated: true,
                },
            ));
        }
        let fks = fks
            .into_iter()
            .map(|(_, mut fk)| {
                fk.name = parsed
                    .fks
                    .iter()
                    .find(|(_, cols)| *cols == fk.columns)
                    .and_then(|(n, _)| n.clone());
                fk
            })
            .collect();
        Ok(Some(LiveTable {
            name: table.to_owned(),
            comment: None,
            columns,
            indexes,
            checks,
            fks,
        }))
    }
}

/// What the SQLite catalog does not report, read back from the stored DDL.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedCreate {
    pub columns: Vec<ParsedColumn>,
    /// Table and column CHECKs, `(name, expr)`.
    pub checks: Vec<(Option<String>, String)>,
    /// `(name, local columns)`.
    pub fks: Vec<(Option<String>, Vec<String>)>,
    pub uniques: Vec<(Option<String>, Vec<String>)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedColumn {
    pub name: String,
    pub generated: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedIndex {
    pub columns: Vec<String>,
    pub predicate: Option<String>,
}

fn ident(t: &Tok) -> Option<&str> {
    match t {
        Tok::Ident(s) => Some(s),
        _ => None,
    }
}

/// Index of the `)` matching the `(` at `open`.
fn close_of(toks: &[norm::Spanned], open: usize) -> Result<usize, CatalogError> {
    let mut depth = 0i32;
    for (k, s) in toks.iter().enumerate().skip(open) {
        match s.tok {
            Tok::LParen => depth += 1,
            Tok::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Ok(k);
                }
            }
            _ => {}
        }
    }
    Err(CatalogError("unbalanced parens in stored DDL".into()))
}

/// Split `toks[from..to]` on depth-0 commas.
fn elements(toks: &[norm::Spanned], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, from);
    for k in from..to {
        match toks[k].tok {
            Tok::LParen => depth += 1,
            Tok::RParen => depth -= 1,
            Tok::Comma if depth == 0 => {
                out.push((start, k));
                start = k + 1;
            }
            _ => {}
        }
    }
    if start < to {
        out.push((start, to));
    }
    out
}

fn names_in(toks: &[norm::Spanned], open: usize, close: usize) -> Vec<String> {
    toks[open + 1..close]
        .iter()
        .filter_map(|s| ident(&s.tok).map(str::to_owned))
        .collect()
}

/// Parse SQLite's stored `CREATE TABLE` text for names and expressions
/// the PRAGMAs leave out.
///
/// # Errors
/// Text that does not tokenize or has unbalanced parens.
pub fn parse_create_table(sql: &str) -> Result<ParsedCreate, CatalogError> {
    let toks = norm::tokenize(DialectKind::Sqlite, sql)?;
    let open = toks
        .iter()
        .position(|s| s.tok == Tok::LParen)
        .ok_or_else(|| CatalogError("no column list".into()))?;
    let close = close_of(&toks, open)?;
    let body = |a: usize, b: usize| sql[toks[a].end..toks[b].start].trim().to_owned();
    let mut out = ParsedCreate::default();
    for (a, b) in elements(&toks, open + 1, close) {
        let first = toks.get(a).and_then(|s| ident(&s.tok)).unwrap_or_default();
        let table_level = matches!(
            first,
            "constraint" | "check" | "unique" | "primary" | "foreign"
        );
        let mut col = (!table_level).then(|| ParsedColumn {
            name: first.to_owned(),
            generated: None,
        });
        let mut pending: Option<String> = None;
        let mut k = if table_level { a } else { a + 1 };
        while k < b {
            match ident(&toks[k].tok) {
                Some("constraint") => {
                    pending = toks
                        .get(k + 1)
                        .and_then(|s| ident(&s.tok))
                        .map(str::to_owned);
                    k += 2;
                    continue;
                }
                Some("check") if toks.get(k + 1).is_some_and(|s| s.tok == Tok::LParen) => {
                    let c = close_of(&toks, k + 1)?;
                    out.checks.push((pending.take(), body(k + 1, c)));
                    k = c + 1;
                    continue;
                }
                Some("as") if toks.get(k + 1).is_some_and(|s| s.tok == Tok::LParen) => {
                    let c = close_of(&toks, k + 1)?;
                    if let Some(col) = col.as_mut() {
                        col.generated = Some(body(k + 1, c));
                    }
                    k = c + 1;
                    continue;
                }
                Some("unique") => {
                    let cols = if toks.get(k + 1).is_some_and(|s| s.tok == Tok::LParen) {
                        let c = close_of(&toks, k + 1)?;
                        let n = names_in(&toks, k + 1, c);
                        k = c;
                        n
                    } else {
                        col.as_ref()
                            .map(|c| vec![c.name.clone()])
                            .unwrap_or_default()
                    };
                    out.uniques.push((pending.take(), cols));
                }
                Some("foreign") => {
                    // FOREIGN KEY (cols) REFERENCES ...
                    if let Some(o) = (k..b).find(|&j| toks[j].tok == Tok::LParen) {
                        let c = close_of(&toks, o)?;
                        out.fks.push((pending.take(), names_in(&toks, o, c)));
                        k = c;
                    }
                }
                Some("references") if col.is_some() => {
                    let name = col.as_ref().map(|c| c.name.clone()).unwrap_or_default();
                    out.fks.push((pending.take(), vec![name]));
                }
                Some("primary") => {
                    pending = None;
                }
                _ => {}
            }
            k += 1;
        }
        if let Some(col) = col {
            out.columns.push(col);
        }
    }
    Ok(out)
}

/// Parse SQLite's stored `CREATE INDEX` text: key texts and the predicate.
///
/// # Errors
/// Text that does not tokenize or has unbalanced parens.
pub fn parse_create_index(sql: &str) -> Result<ParsedIndex, CatalogError> {
    let toks = norm::tokenize(DialectKind::Sqlite, sql)?;
    let open = toks
        .iter()
        .position(|s| s.tok == Tok::LParen)
        .ok_or_else(|| CatalogError("no key list".into()))?;
    let close = close_of(&toks, open)?;
    let columns = elements(&toks, open + 1, close)
        .into_iter()
        .map(|(a, b)| {
            if b == a + 1 {
                ident(&toks[a].tok)
                    .map_or_else(|| sql[toks[a].start..toks[a].end].to_owned(), str::to_owned)
            } else {
                sql[toks[a].start..toks[b - 1].end].to_owned()
            }
        })
        .collect();
    let predicate = toks
        .get(close + 1)
        .filter(|s| ident(&s.tok) == Some("where"))
        .map(|s| sql[s.end..].trim().to_owned());
    Ok(ParsedIndex { columns, predicate })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_create_table_parser_reads_what_pragmas_drop() {
        let p = parse_create_table(
            "CREATE TABLE \"p\" (\"id\" INTEGER PRIMARY KEY, \"i\" bigint DEFAULT 0 CHECK (i > -1), \
             \"g\" bigint GENERATED ALWAYS AS (i*2) STORED, gv AS (i + (1)), \
             \"o\" bigint CONSTRAINT p_o_fk REFERENCES o(id) ON DELETE CASCADE, \
             CONSTRAINT \"p_i_pos\" CHECK (\"i\" >= 0), CONSTRAINT p_s_uq UNIQUE (i, g), \
             CONSTRAINT p_fk2 FOREIGN KEY (i, g) REFERENCES q(a, b))",
        )
        .unwrap();
        assert_eq!(
            p.checks,
            vec![
                (None, "i > -1".to_owned()),
                (Some("p_i_pos".into()), "\"i\" >= 0".into())
            ]
        );
        assert_eq!(p.columns[2].generated.as_deref(), Some("i*2"));
        assert_eq!(p.columns[3].generated.as_deref(), Some("i + (1)"));
        assert_eq!(
            p.fks,
            vec![
                (Some("p_o_fk".into()), vec!["o".into()]),
                (Some("p_fk2".into()), vec!["i".into(), "g".into()])
            ]
        );
        assert_eq!(
            p.uniques,
            vec![(Some("p_s_uq".into()), vec!["i".into(), "g".into()])]
        );
        let ix = parse_create_index(
            "CREATE UNIQUE INDEX \"x\" ON \"p\" (\"i\", lower(s)) WHERE i > 5 AND (g < 2)",
        )
        .unwrap();
        assert_eq!(ix.columns, vec!["i".to_owned(), "lower(s)".to_owned()]);
        assert_eq!(ix.predicate.as_deref(), Some("i > 5 AND (g < 2)"));
    }
}
