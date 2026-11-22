//! SPIKE S6: the 14.1 rule. One raw DEFAULT string per column; refused,
//! naming the backend, when it is not valid on every configured backend.
//!
//! Three layers: an offline per-dialect classifier (always), an in-memory
//! SQLite run (when built), and a rolled-back run on the connected backend.

use super::norm::{self, Tok};
use super::ops::ColumnDecl;
use crate::sql::{DialectKind, Pool};

/// The configured backends (`[migrate] backends`): non-empty, no repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backends(Vec<DialectKind>);

impl Backends {
    /// # Errors
    /// An empty list.
    pub fn new(list: &[DialectKind]) -> Result<Self, &'static str> {
        let mut v: Vec<DialectKind> = Vec::new();
        for k in list {
            if !v.contains(k) {
                v.push(*k);
            }
        }
        if v.is_empty() {
            return Err("at least one backend");
        }
        Ok(Self(v))
    }

    #[must_use]
    pub fn iter(&self) -> impl Iterator<Item = DialectKind> + '_ {
        self.0.iter().copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub backend: DialectKind,
    pub reason: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "default is not valid on {}: {}",
            self.backend.name(),
            self.reason
        )
    }
}

/// `MV2-W020`: not verified offline on this backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unverified {
    pub backend: DialectKind,
    pub function: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub refusals: Vec<Refusal>,
    pub unverified: Vec<Unverified>,
}

impl Report {
    #[must_use]
    pub fn refused_on(&self, k: DialectKind) -> bool {
        self.refusals.iter().any(|r| r.backend == k)
    }
}

const PG_ONLY: &[&str] = &[
    "gen_random_uuid",
    "nextval",
    "uuid_generate_v4",
    "clock_timestamp",
    "statement_timestamp",
];
const MYSQL_ONLY: &[&str] = &["utc_timestamp", "uuid", "uuid_to_bin", "curdate", "sysdate"];
const SQLITE_ONLY: &[&str] = &["strftime", "randomblob", "julianday"];
/// Valid on the backend and known not to need a check.
const KNOWN: &[(&str, &[DialectKind])] = &[
    ("now", &[DialectKind::Postgres, DialectKind::Mysql]),
    (
        "current_timestamp",
        &[
            DialectKind::Postgres,
            DialectKind::Mysql,
            DialectKind::Sqlite,
        ],
    ),
    ("gen_random_uuid", &[DialectKind::Postgres]),
    ("utc_timestamp", &[DialectKind::Mysql]),
    ("uuid", &[DialectKind::Mysql]),
    ("uuid_to_bin", &[DialectKind::Mysql]),
    ("strftime", &[DialectKind::Sqlite]),
    ("randomblob", &[DialectKind::Sqlite]),
    ("hex", &[DialectKind::Mysql, DialectKind::Sqlite]),
    ("unhex", &[DialectKind::Mysql, DialectKind::Sqlite]),
    (
        "substr",
        &[
            DialectKind::Postgres,
            DialectKind::Mysql,
            DialectKind::Sqlite,
        ],
    ),
    (
        "abs",
        &[
            DialectKind::Postgres,
            DialectKind::Mysql,
            DialectKind::Sqlite,
        ],
    ),
    ("random", &[DialectKind::Postgres, DialectKind::Sqlite]),
    (
        "lower",
        &[
            DialectKind::Postgres,
            DialectKind::Mysql,
            DialectKind::Sqlite,
        ],
    ),
    (
        "upper",
        &[
            DialectKind::Postgres,
            DialectKind::Mysql,
            DialectKind::Sqlite,
        ],
    ),
];

fn is_lob_on_mysql(c: &ColumnDecl) -> bool {
    matches!(c.ty.as_str(), "json" | "binary") || (c.ty == "string" && c.max_length.is_none())
}

/// The offline classifier for `col.default` on every configured backend.
#[must_use]
pub fn check_offline(backends: &Backends, col: &ColumnDecl) -> Report {
    let mut r = Report::default();
    let Some(text) = col.default.as_deref() else {
        return r;
    };
    for k in backends.iter() {
        classify(k, col, text, &mut r);
    }
    // Same text, different value: MySQL reads `\` in a string as an escape.
    let has_mysql = backends.iter().any(|k| k == DialectKind::Mysql);
    let has_other = backends.iter().any(|k| k != DialectKind::Mysql);
    if has_mysql && has_other {
        if let Ok(toks) = norm::tokenize(DialectKind::Postgres, text) {
            if toks
                .iter()
                .any(|s| matches!(&s.tok, Tok::Str(v) if v.contains('\\')))
            {
                r.refusals.push(Refusal {
                    backend: DialectKind::Mysql,
                    reason: "a backslash in a string is an escape on MySQL, so the stored value differs from the other backends".into(),
                });
            }
        }
    }
    r
}

fn classify(k: DialectKind, col: &ColumnDecl, text: &str, r: &mut Report) {
    let mut refuse = |reason: String| r.refusals.push(Refusal { backend: k, reason });
    let toks: Vec<Tok> = match norm::tokenize(k, text) {
        Ok(t) => t.into_iter().map(|s| s.tok).collect(),
        Err(e) => {
            refuse(format!("does not tokenize: {}", e.0));
            return;
        }
    };
    let wrapped = toks.first() == Some(&Tok::LParen) && toks.last() == Some(&Tok::RParen) && {
        let mut depth = 0;
        toks.iter().enumerate().all(|(i, t)| {
            match t {
                Tok::LParen => depth += 1,
                Tok::RParen => depth -= 1,
                _ => {}
            }
            depth > 0 || i == toks.len() - 1
        })
    };
    let inner: &[Tok] = if wrapped {
        &toks[1..toks.len() - 1]
    } else {
        &toks
    };
    let literal = match inner {
        [Tok::Str(_) | Tok::Num(_)] => true,
        [Tok::Op(m), Tok::Num(_)] if m == "-" => true,
        [Tok::Ident(w)] => matches!(w.as_str(), "true" | "false" | "null"),
        _ => false,
    };
    if toks.iter().any(|t| matches!(t, Tok::Op(o) if o == "::")) && k != DialectKind::Postgres {
        refuse("`::` casts are PostgreSQL-only".into());
    }
    let mut functions: Vec<&str> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if let Tok::Ident(w) = t {
            if toks.get(i + 1) == Some(&Tok::LParen) || w == "current_timestamp" {
                functions.push(w);
            }
        }
    }
    for f in &functions {
        let only = [
            (PG_ONLY, DialectKind::Postgres),
            (MYSQL_ONLY, DialectKind::Mysql),
            (SQLITE_ONLY, DialectKind::Sqlite),
        ]
        .into_iter()
        .find(|(list, _)| list.contains(f));
        if let Some((_, owner)) = only {
            if owner != k {
                refuse(format!("`{f}()` exists only on {}", owner.name()));
                continue;
            }
        }
        if !KNOWN.iter().any(|(n, on)| n == f && on.contains(&k)) && only.is_none() {
            r.unverified.push(Unverified {
                backend: k,
                function: (*f).to_owned(),
            });
        }
    }
    let now_kw = matches!(inner, [Tok::Ident(w)] if w == "current_timestamp")
        || matches!(inner, [Tok::Ident(w), Tok::LParen, .., Tok::RParen] if w == "current_timestamp" || w == "now");
    match k {
        DialectKind::Mysql => {
            if is_lob_on_mysql(col) && !wrapped {
                refuse(
                    "JSON, TEXT and BLOB columns take only the parenthesised `(expr)` form".into(),
                );
            } else if !literal && !wrapped && !now_kw {
                refuse("an expression default needs parentheses".into());
            }
        }
        DialectKind::Sqlite => {
            if functions.iter().any(|f| *f == "current_timestamp") {
                refuse(
                    "CURRENT_TIMESTAMP text does not compare with the values the bind path writes"
                        .into(),
                );
            } else if !literal && !wrapped {
                refuse("a non-constant default needs parentheses".into());
            }
        }
        DialectKind::Postgres => {}
    }
}

/// [`check_offline`], then a real run: SQLite in memory (when built and
/// configured) and the connected backend (rolled back).
pub async fn check(backends: &Backends, col: &ColumnDecl, connected: Option<&Pool>) -> Report {
    let mut r = check_offline(backends, col);
    if col.default.is_none() {
        return r;
    }
    #[cfg(feature = "sqlite")]
    if backends.iter().any(|k| k == DialectKind::Sqlite) && !r.refused_on(DialectKind::Sqlite) {
        if let Ok(mem) = Pool::connect("sqlite::memory:").await {
            if let Err(e) = run_default(&mem, col).await {
                r.refusals.push(Refusal {
                    backend: DialectKind::Sqlite,
                    reason: e,
                });
            }
        }
    }
    if let Some(pool) = connected {
        let k = pool.dialect().kind();
        let skip = k == DialectKind::Sqlite && cfg!(feature = "sqlite");
        if backends.iter().any(|b| b == k) && !r.refused_on(k) && !skip {
            if let Err(e) = run_default(pool, col).await {
                r.refusals.push(Refusal {
                    backend: k,
                    reason: e,
                });
            }
        }
    }
    r
}

/// CREATE a temp table with the column, INSERT DEFAULT VALUES, roll back,
/// on a pinned connection closed afterwards.
///
/// # Errors
/// The driver's message.
pub async fn run_default(pool: &Pool, col: &ColumnDecl) -> Result<(), String> {
    use crate::sql::raw_execute_tx;
    let d = pool.dialect();
    let k = d.kind();
    let (temp, tail) = match k {
        DialectKind::Postgres => ("TEMP", " ON COMMIT DROP"),
        DialectKind::Mysql => ("TEMPORARY", ""),
        DialectKind::Sqlite => ("TEMP", ""),
    };
    let name = d.quote_ident("_rgo_s6_default");
    let id = d.quote_ident("_rgo_id");
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    conn.close_on_drop();
    let mut tx = conn.begin().await.map_err(|e| e.to_string())?;
    let res = async {
        raw_execute_tx(
            &mut tx,
            &format!(
                "CREATE {temp} TABLE {name} ({id} INTEGER, {}){tail}",
                super::ops::column_sql(d, col)
            ),
            Vec::new(),
        )
        .await?;
        raw_execute_tx(
            &mut tx,
            &format!("INSERT INTO {name} ({id}) VALUES (1)"),
            Vec::new(),
        )
        .await
    }
    .await;
    let _ = tx.rollback().await;
    res.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use DialectKind::{Mysql, Postgres, Sqlite};

    fn col(ty: &str, max: Option<u32>, default: &str) -> ColumnDecl {
        let mut c = ColumnDecl::new("c", ty);
        c.max_length = max;
        c.default = Some(default.into());
        c
    }

    fn refused(b: &[DialectKind], c: &ColumnDecl) -> Vec<DialectKind> {
        let mut v: Vec<DialectKind> = check_offline(&Backends::new(b).unwrap(), c)
            .refusals
            .iter()
            .map(|r| r.backend)
            .collect();
        v.dedup();
        v
    }

    /// Every 2.6 row passes offline on its own backend.
    #[test]
    fn table_rows_pass_offline_on_their_backend() {
        use crate::sql::Dialect as _;
        let sqlite_uuid =
            crate::sql::Sqlite.translate_default_expr("gen_random_uuid()", "uuid", None);
        let rows: &[(DialectKind, &str, Option<u32>, &str)] = &[
            (Postgres, "string", Some(20), "'en'"),
            (Postgres, "string", Some(20), "'it''s'"),
            (Postgres, "i64", None, "0"),
            (Postgres, "decimal", None, "0.00"),
            (Postgres, "bool", None, "TRUE"),
            (Postgres, "date", None, "'2026-01-01'"),
            (Postgres, "datetime", None, "now()"),
            (Postgres, "uuid", None, "gen_random_uuid()"),
            (Postgres, "json", None, "'{}'::jsonb"),
            (Mysql, "string", Some(20), "'en'"),
            (Mysql, "string", None, "('en')"),
            (Mysql, "i64", None, "0"),
            (Mysql, "bool", None, "FALSE"),
            (Mysql, "datetime", None, "(UTC_TIMESTAMP(6))"),
            (Mysql, "uuid", None, "(UUID())"),
            (Mysql, "json", None, "('{}')"),
            (Sqlite, "string", None, "'en'"),
            (Sqlite, "bool", None, "TRUE"),
            (
                Sqlite,
                "datetime",
                None,
                "(strftime('%Y-%m-%dT%H:%M:%f000+00:00','now'))",
            ),
            (Sqlite, "uuid", None, &sqlite_uuid),
            (Sqlite, "json", None, "'{}'"),
        ];
        for (k, ty, max, text) in rows {
            assert!(
                refused(&[*k], &col(ty, *max, text)).is_empty(),
                "{k:?} {text}"
            );
        }
    }

    #[test]
    fn non_portable_defaults_are_refused_naming_the_backend() {
        // A PG-only cast in a PG+SQLite project.
        assert_eq!(
            refused(&[Postgres, Sqlite], &col("json", None, "'{}'::jsonb")),
            vec![Sqlite]
        );
        assert_eq!(
            refused(&[Postgres, Sqlite], &col("datetime", None, "now()")),
            vec![Sqlite]
        );
        assert_eq!(
            refused(&[Postgres, Mysql], &col("uuid", None, "gen_random_uuid()")),
            vec![Mysql]
        );
        assert_eq!(
            refused(
                &[Postgres, Mysql],
                &col("datetime", None, "(UTC_TIMESTAMP(6))")
            ),
            vec![Postgres]
        );
        assert_eq!(refused(&[Mysql], &col("string", None, "'en'")), vec![Mysql]);
        assert_eq!(refused(&[Mysql], &col("json", None, "'{}'")), vec![Mysql]);
        assert_eq!(
            refused(&[Sqlite], &col("datetime", None, "CURRENT_TIMESTAMP")),
            vec![Sqlite]
        );
        assert_eq!(
            refused(&[Postgres, Mysql], &col("string", Some(9), "'a\\b'")),
            vec![Mysql]
        );
        // Portable on all three.
        let all = [Postgres, Mysql, Sqlite];
        for (ty, max, text) in [
            ("json", None, "('{}')"),
            ("json", None, "('[]')"),
            ("string", None, "('en')"),
            ("string", Some(20), "'en'"),
            ("bool", None, "TRUE"),
            ("i64", None, "-1"),
            ("decimal", None, "0.00"),
            ("date", None, "'2026-01-01'"),
        ] {
            assert!(refused(&all, &col(ty, max, text)).is_empty(), "{text}");
        }
    }
}
