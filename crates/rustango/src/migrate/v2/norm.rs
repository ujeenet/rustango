//! Normalisers that make catalog text and v2-declared text compare equal.
//! Pure functions; golden tests at the bottom.
//!
//! One canonical form for every SQL fragment (defaults, CHECK bodies,
//! partial-index predicates, generation expressions): a token stream with
//! decoded string literals, unquoted lowercase identifiers, no charset
//! introducers, no casts on literals, and no parens around the whole.

use crate::sql::DialectKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// Decoded string literal value.
    Str(String),
    Num(String),
    /// Identifier or keyword; lowercase unless it was quoted.
    Ident(String),
    Op(String),
    LParen,
    RParen,
    Comma,
}

/// A token plus its byte span in the source, so a parser can slice raw text.
#[derive(Debug, Clone)]
pub struct Spanned {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizeError(pub String);

/// Tokenize `sql` with `dialect`'s quoting rules: MySQL strings take
/// backslash escapes and `"` quotes a string; elsewhere `"` quotes an identifier.
pub fn tokenize(dialect: DialectKind, sql: &str) -> Result<Vec<Spanned>, TokenizeError> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mysql = dialect == DialectKind::Mysql;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let tok = if c == b'\'' || (mysql && c == b'"') {
            let (s, n) = read_string(sql, i, c, mysql)?;
            i = n;
            Tok::Str(s)
        } else if c == b'"' || c == b'`' || (c == b'[' && dialect == DialectKind::Sqlite) {
            let close = if c == b'[' { b']' } else { c };
            let mut s = String::new();
            i += 1;
            loop {
                let Some(&d) = b.get(i) else {
                    return Err(TokenizeError(format!("unclosed identifier at {start}")));
                };
                if d == close {
                    if b.get(i + 1) == Some(&close) && close != b']' {
                        s.push(close as char);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                let ch = sql[i..].chars().next().unwrap_or('?');
                s.push(ch);
                i += ch.len_utf8();
            }
            Tok::Ident(s)
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                i += 1;
                if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            Tok::Num(sql[start..i].to_owned())
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            Tok::Ident(sql[start..i].to_ascii_lowercase())
        } else if c == b'(' {
            i += 1;
            Tok::LParen
        } else if c == b')' {
            i += 1;
            Tok::RParen
        } else if c == b',' {
            i += 1;
            Tok::Comma
        } else {
            const OPS: [&str; 12] = [
                "::", "<>", "!=", ">=", "<=", "||", "->>", "->", "<=>", "&&", "@>", "<@",
            ];
            let op = OPS
                .iter()
                .filter(|o| sql[i..].starts_with(**o))
                .max_by_key(|o| o.len())
                .map_or_else(|| (c as char).to_string(), |o| (*o).to_owned());
            i += op.len();
            Tok::Op(op)
        };
        out.push(Spanned { tok, start, end: i });
    }
    Ok(out)
}

fn read_string(
    sql: &str,
    at: usize,
    q: u8,
    backslash: bool,
) -> Result<(String, usize), TokenizeError> {
    let b = sql.as_bytes();
    let mut s = String::new();
    let mut i = at + 1;
    loop {
        let Some(&d) = b.get(i) else {
            return Err(TokenizeError(format!("unclosed string at {at}")));
        };
        if backslash && d == b'\\' {
            let e = *b
                .get(i + 1)
                .ok_or_else(|| TokenizeError("dangling backslash".into()))?;
            s.push(match e {
                b'n' => '\n',
                b't' => '\t',
                b'r' => '\r',
                b'0' => '\0',
                other => other as char,
            });
            i += 2;
            continue;
        }
        if d == q {
            if b.get(i + 1) == Some(&q) {
                s.push(q as char);
                i += 2;
                continue;
            }
            return Ok((s, i + 1));
        }
        let ch = sql[i..].chars().next().unwrap_or('?');
        s.push(ch);
        i += ch.len_utf8();
    }
}

/// Type words that may follow `::` after the first word of a PG cast.
const CAST_CONTINUATIONS: &[&str] = &["varying", "precision", "with", "without", "time", "zone"];
const NUMERIC_CASTS: &[&str] = &[
    "integer", "bigint", "smallint", "numeric", "real", "double", "int", "int4", "int8",
];

/// Column context a default is normalised in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DefaultCtx {
    /// A boolean column: `1`/`0` (MySQL, SQLite storage) mean `TRUE`/`FALSE`.
    pub boolean: bool,
}

/// The canonical form of an expression: equal output means same SQL.
///
/// # Errors
/// Text that does not tokenize.
#[cfg(test)]
pub fn norm_expr(dialect: DialectKind, sql: &str) -> Result<String, TokenizeError> {
    norm_with(dialect, sql, DefaultCtx::default())
}

/// The canonical form of a DEFAULT, given as SQL text (declared or
/// already turned into SQL by a catalog reader).
///
/// # Errors
/// Text that does not tokenize.
pub fn norm_default(
    dialect: DialectKind,
    sql: &str,
    ctx: DefaultCtx,
) -> Result<String, TokenizeError> {
    norm_with(dialect, sql, ctx)
}

fn norm_with(dialect: DialectKind, sql: &str, ctx: DefaultCtx) -> Result<String, TokenizeError> {
    let toks: Vec<Tok> = tokenize(dialect, sql)?.into_iter().map(|s| s.tok).collect();
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        // MySQL charset introducer: `_utf8mb4'x'`.
        if let (Tok::Ident(id), Some(Tok::Str(_))) = (t, toks.get(i + 1)) {
            if id.starts_with('_') && id.len() > 1 {
                i += 1;
                continue;
            }
        }
        // PG cast on a literal: `'x'::character varying`, `'-1'::integer`.
        if matches!(t, Tok::Op(o) if o == "::")
            && matches!(out.last(), Some(Tok::Str(_) | Tok::Num(_)))
        {
            let mut j = i + 1;
            let first = match toks.get(j) {
                Some(Tok::Ident(w)) => w.clone(),
                _ => {
                    out.push(t.clone());
                    i += 1;
                    continue;
                }
            };
            j += 1;
            while let Some(Tok::Ident(w)) = toks.get(j) {
                if CAST_CONTINUATIONS.contains(&w.as_str()) {
                    j += 1;
                } else {
                    break;
                }
            }
            if toks.get(j) == Some(&Tok::LParen) {
                while j < toks.len() && toks[j] != Tok::RParen {
                    j += 1;
                }
                j += 1;
            }
            if matches!(toks.get(j), Some(Tok::Op(o)) if o == "[") {
                j += 2;
            }
            if NUMERIC_CASTS.contains(&first.as_str()) {
                if let Some(Tok::Str(s)) = out.last() {
                    if s.parse::<f64>().is_ok() {
                        let n = s.clone();
                        out.pop();
                        out.push(Tok::Num(n));
                    }
                }
            }
            i = j;
            continue;
        }
        // Unary minus on a number after an operator, `(`, `,` or at the start.
        if matches!(t, Tok::Op(o) if o == "-")
            && matches!(toks.get(i + 1), Some(Tok::Num(_)))
            && matches!(
                out.last(),
                None | Some(Tok::Op(_) | Tok::LParen | Tok::Comma)
            )
        {
            if let Some(Tok::Num(n)) = toks.get(i + 1) {
                out.push(Tok::Num(format!("-{n}")));
            }
            i += 2;
            continue;
        }
        out.push(t.clone());
        i += 1;
    }
    strip_outer_parens(&mut out);
    for t in &mut out {
        match t {
            Tok::Num(n) => *n = canon_number(n),
            Tok::Ident(w) if w == "true" || w == "false" => {}
            _ => {}
        }
    }
    if ctx.boolean && out.len() == 1 {
        let b = match &out[0] {
            Tok::Num(n) if n == "1" => Some("true"),
            Tok::Num(n) if n == "0" => Some("false"),
            Tok::Str(s)
                if s == "1" || s.eq_ignore_ascii_case("t") || s.eq_ignore_ascii_case("true") =>
            {
                Some("true")
            }
            Tok::Str(s)
                if s == "0" || s.eq_ignore_ascii_case("f") || s.eq_ignore_ascii_case("false") =>
            {
                Some("false")
            }
            _ => None,
        };
        if let Some(b) = b {
            out[0] = Tok::Ident(b.to_owned());
        }
    }
    Ok(render(&out))
}

fn canon_number(n: &str) -> String {
    if n.contains(['e', 'E']) || !n.contains('.') {
        return n.to_owned();
    }
    let t = n.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() || t == "-" {
        "0".to_owned()
    } else {
        t.to_owned()
    }
}

/// Drop parens that wrap the whole stream, repeatedly.
fn strip_outer_parens(v: &mut Vec<Tok>) {
    while v.len() >= 2 && v[0] == Tok::LParen && v[v.len() - 1] == Tok::RParen {
        let mut depth = 0i32;
        let mut wraps = true;
        for (k, t) in v.iter().enumerate() {
            match t {
                Tok::LParen => depth += 1,
                Tok::RParen => depth -= 1,
                _ => {}
            }
            if depth == 0 && k != v.len() - 1 {
                wraps = false;
                break;
            }
        }
        if !wraps {
            return;
        }
        v.pop();
        v.remove(0);
    }
}

fn render(v: &[Tok]) -> String {
    let mut s = String::new();
    for (k, t) in v.iter().enumerate() {
        if k > 0 {
            s.push(' ');
        }
        match t {
            Tok::Str(x) => {
                s.push('\'');
                s.push_str(&x.replace('\'', "''"));
                s.push('\'');
            }
            Tok::Num(x) | Tok::Ident(x) | Tok::Op(x) => s.push_str(x),
            Tok::LParen => s.push('('),
            Tok::RParen => s.push(')'),
            Tok::Comma => s.push(','),
        }
    }
    s
}

/// MySQL stores an expression DEFAULT, CHECK clause and generation
/// expression with every `\` and `'` backslash-escaped once more.
#[must_use]
pub fn mysql_unescape_catalog(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(&n) = it.peek() {
                if n == '\\' || n == '\'' {
                    out.push(n);
                    it.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// A MySQL `COLUMN_DEFAULT` as SQL text: literals are stored unquoted,
/// expressions (`DEFAULT_GENERATED`) escaped and without their parens.
#[must_use]
pub fn mysql_catalog_default_sql(raw: &str, data_type: &str, extra: &str) -> String {
    if extra.to_ascii_uppercase().contains("DEFAULT_GENERATED") {
        let e = mysql_unescape_catalog(raw);
        if e.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP") {
            e
        } else {
            format!("({e})")
        }
    } else if is_mysql_numeric(data_type) {
        raw.to_owned()
    } else {
        format!("'{}'", raw.replace('\\', "\\\\").replace('\'', "''"))
    }
}

fn is_mysql_numeric(data_type: &str) -> bool {
    matches!(
        data_type.to_ascii_lowercase().as_str(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "decimal"
            | "numeric"
            | "float"
            | "double"
            | "real"
            | "bit"
    )
}

/// The canonical spelling of a column type, so the DDL type a v2 column
/// renders to and the catalog's type compare equal.
#[must_use]
pub fn norm_type(dialect: DialectKind, raw: &str) -> String {
    let mut t = raw.trim().to_ascii_lowercase();
    // Collation is not part of the type (read and compared on its own).
    if let Some(k) = t.find(" collate ") {
        t.truncate(k);
    }
    for kw in [
        " not null",
        " auto_increment",
        " primary key",
        " autoincrement",
    ] {
        if let Some(k) = t.find(kw) {
            t.truncate(k);
        }
    }
    let t: String = {
        // Collapse whitespace; none next to parens or commas.
        let mut s = String::new();
        for w in t.split_whitespace() {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(w);
        }
        s.replace(" (", "(")
            .replace("( ", "(")
            .replace(" )", ")")
            .replace(", ", ",")
            .replace(" ,", ",")
    };
    let (base, args) = match t.find('(') {
        Some(k) => (t[..k].to_owned(), t[k..].to_owned()),
        None => (t.clone(), String::new()),
    };
    match dialect {
        DialectKind::Postgres => {
            let base = match base.as_str() {
                "varchar" => "character varying",
                "char" | "bpchar" => "character",
                "int" | "int4" | "serial" | "serial4" => "integer",
                "int8" | "bigserial" | "serial8" => "bigint",
                "int2" | "smallserial" | "serial2" => "smallint",
                "bool" => "boolean",
                "float8" => "double precision",
                "float4" => "real",
                "decimal" => "numeric",
                "timestamptz" => "timestamp with time zone",
                "timestamp" => "timestamp without time zone",
                "timetz" => "time with time zone",
                "time" => "time without time zone",
                other => other,
            };
            // `timestamp(6) with time zone` keeps args before the suffix.
            format!("{base}{args}")
        }
        DialectKind::Mysql => {
            let base = match base.as_str() {
                "integer" => "int",
                "bool" | "boolean" => return "tinyint(1)".to_owned(),
                "dec" | "numeric" => "decimal",
                "real" => "double",
                other => other,
            };
            // 8.0.19+ drops integer display widths, except `tinyint(1)`.
            let int = matches!(
                base,
                "tinyint" | "smallint" | "mediumint" | "int" | "bigint"
            );
            if int && !(base == "tinyint" && args == "(1)") {
                let unsigned = args.contains("unsigned");
                return if unsigned {
                    format!("{base} unsigned")
                } else {
                    base.to_owned()
                };
            }
            format!("{base}{args}")
        }
        DialectKind::Sqlite => format!("{base}{args}"),
    }
}

/// SQLite's column affinity for a declared type (datatype3 §3.1).
#[cfg(test)]
#[must_use]
pub fn sqlite_affinity(declared: &str) -> &'static str {
    let t = declared.to_ascii_uppercase();
    if t.contains("INT") {
        "INTEGER"
    } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        "TEXT"
    } else if t.contains("BLOB") || t.trim().is_empty() {
        "BLOB"
    } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
        "REAL"
    } else {
        "NUMERIC"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DialectKind::{Mysql, Postgres, Sqlite};

    fn same_default(d: DialectKind, declared: &str, catalog: &str, ctx: DefaultCtx) {
        assert_eq!(
            norm_default(d, declared, ctx).unwrap(),
            norm_default(d, catalog, ctx).unwrap(),
            "{d:?}: declared {declared} vs catalog {catalog}"
        );
    }

    const B: DefaultCtx = DefaultCtx { boolean: true };
    const N: DefaultCtx = DefaultCtx { boolean: false };

    /// Catalog text below is copied from the live probes (PG 16, MySQL
    /// 8.0.46, SQLite 3.4x); MySQL rows pass through the catalog reader first.
    #[test]
    fn golden_defaults_postgres() {
        for (declared, catalog, ctx) in [
            ("'en'", "'en'::character varying", N),
            ("'en'", "'en'::text", N),
            ("('en')", "'en'::text", N),
            ("'it''s'", "'it''s'::character varying", N),
            ("0", "0", N),
            ("0.00", "0.00", N),
            ("-1", "'-1'::integer", N),
            ("TRUE", "true", B),
            ("FALSE", "false", B),
            ("'2026-01-01'", "'2026-01-01'::date", N),
            ("now()", "now()", N),
            ("NOW()", "now()", N),
            ("gen_random_uuid()", "gen_random_uuid()", N),
            ("'{}'::jsonb", "'{}'::jsonb", N),
            ("'[]'::jsonb", "'[]'::jsonb", N),
            ("('{}')", "'{}'::jsonb", N),
            ("CURRENT_TIMESTAMP", "CURRENT_TIMESTAMP", N),
        ] {
            same_default(Postgres, declared, catalog, ctx);
        }
    }

    #[test]
    fn golden_defaults_mysql() {
        let cat = |raw: &str, dt: &str, extra: &str| mysql_catalog_default_sql(raw, dt, extra);
        for (declared, catalog, ctx) in [
            ("'en'", cat("en", "varchar", ""), N),
            (
                "('en')",
                cat(r"_utf8mb4\'en\'", "longtext", "DEFAULT_GENERATED"),
                N,
            ),
            ("'it''s'", cat("it's", "varchar", ""), N),
            ("0", cat("0", "bigint", ""), N),
            ("0.00", cat("0.00", "decimal", ""), N),
            ("0", cat("0.00", "decimal", ""), N),
            ("-1", cat("-1", "int", ""), N),
            ("TRUE", cat("1", "tinyint", ""), B),
            ("FALSE", cat("0", "tinyint", ""), B),
            ("'2026-01-01'", cat("2026-01-01", "date", ""), N),
            (
                "(UTC_TIMESTAMP(6))",
                cat("utc_timestamp(6)", "datetime", "DEFAULT_GENERATED"),
                N,
            ),
            (
                "CURRENT_TIMESTAMP(6)",
                cat("CURRENT_TIMESTAMP(6)", "datetime", "DEFAULT_GENERATED"),
                N,
            ),
            ("(UUID())", cat("uuid()", "char", "DEFAULT_GENERATED"), N),
            (
                "('{}')",
                cat(r"_utf8mb4\'{}\'", "json", "DEFAULT_GENERATED"),
                N,
            ),
            (
                "('[]')",
                cat(r"_latin1\'[]\'", "json", "DEFAULT_GENERATED"),
                N,
            ),
            (
                r#"('{"a": "it''s"}')"#,
                cat(
                    r#"_utf8mb4\'{"a": "it\\\'s"}\'"#,
                    "json",
                    "DEFAULT_GENERATED",
                ),
                N,
            ),
            ("(1+1)", cat("(1 + 1)", "int", "DEFAULT_GENERATED"), N),
        ] {
            same_default(Mysql, declared, &catalog, ctx);
        }
        // Not the same value: a literal is never an expression.
        assert_ne!(
            norm_default(Mysql, "'now()'", N).unwrap(),
            norm_default(Mysql, "(now())", N).unwrap()
        );
    }

    #[test]
    fn golden_defaults_sqlite() {
        let ts = "(strftime('%Y-%m-%dT%H:%M:%f000+00:00','now'))";
        for (declared, catalog, ctx) in [
            ("'en'", "'en'", N),
            ("'it''s'", "'it''s'", N),
            ("0.00", "0.00", N),
            ("TRUE", "TRUE", B),
            ("-1", "-1", N),
            // PRAGMA drops the expression's outer parens.
            (ts, "strftime('%Y-%m-%dT%H:%M:%f000+00:00','now')", N),
            ("'{}'", "'{}'", N),
            ("('{}')", "'{}'", N),
        ] {
            same_default(Sqlite, declared, catalog, ctx);
        }
    }

    #[test]
    fn golden_checks_and_predicates() {
        // PG pg_get_expr(conbin) / indpred, MySQL CHECK_CLAUSE (escaped).
        assert_eq!(
            norm_expr(Postgres, "i >= 0").unwrap(),
            norm_expr(Postgres, "(i >= 0)").unwrap()
        );
        assert_eq!(
            norm_expr(Postgres, "i>=0").unwrap(),
            norm_expr(Postgres, "((i >= 0))").unwrap()
        );
        assert_eq!(
            norm_expr(Mysql, "lower(e) = e").unwrap(),
            norm_expr(Mysql, &mysql_unescape_catalog("(lower(`e`) = `e`)")).unwrap()
        );
        assert_eq!(
            norm_expr(Mysql, "t <> 'a''b'").unwrap(),
            norm_expr(
                Mysql,
                &mysql_unescape_catalog(r"(`t` <> _utf8mb4\'a\\\'b\')")
            )
            .unwrap()
        );
        assert_eq!(
            norm_expr(Sqlite, "\"i\" > 5").unwrap(),
            norm_expr(Sqlite, "i > 5").unwrap()
        );
        // The pure form is not enough: the server rewrites these (round-trip needed).
        assert_ne!(
            norm_expr(Postgres, "n IN (1, 2)").unwrap(),
            norm_expr(Postgres, "(n = ANY (ARRAY[1, 2]))").unwrap()
        );
        assert_ne!(
            norm_expr(Mysql, "a > 0 AND b > 0").unwrap(),
            norm_expr(Mysql, &mysql_unescape_catalog("((`a` > 0) and (`b` > 0))")).unwrap()
        );
        // Grouping that changes meaning is kept.
        assert_ne!(
            norm_expr(Postgres, "(a OR b) AND c").unwrap(),
            norm_expr(Postgres, "a OR b AND c").unwrap()
        );
    }

    #[test]
    fn golden_types() {
        for (d, declared, catalog) in [
            (Postgres, "VARCHAR(20)", "character varying(20)"),
            (Postgres, "BIGINT", "bigint"),
            (Postgres, "BIGSERIAL", "bigint"),
            (Postgres, "INTEGER", "integer"),
            (Postgres, "TIMESTAMPTZ", "timestamp with time zone"),
            (Postgres, "DOUBLE PRECISION", "double precision"),
            (Postgres, "BOOLEAN", "boolean"),
            (Postgres, "NUMERIC", "numeric"),
            (Postgres, "JSONB", "jsonb"),
            (Postgres, "UUID", "uuid"),
            (Postgres, "TIME", "time without time zone"),
            (Mysql, "VARCHAR(20)", "varchar(20)"),
            (Mysql, "TINYINT(1)", "tinyint(1)"),
            (Mysql, "BOOLEAN", "tinyint(1)"),
            (Mysql, "BIGINT NOT NULL AUTO_INCREMENT", "bigint"),
            (Mysql, "INT", "int(11)"),
            (Mysql, "DECIMAL(65, 28)", "decimal(65,28)"),
            (Mysql, "DATETIME(6)", "datetime(6)"),
            (Mysql, "LONGTEXT COLLATE utf8mb4_0900_as_ci", "longtext"),
            (Mysql, "CHAR(36)", "char(36)"),
            (Sqlite, "TEXT COLLATE NOCASE", "TEXT"),
            (Sqlite, "INTEGER PRIMARY KEY AUTOINCREMENT", "INTEGER"),
            (Sqlite, "decimal(10, 2)", "decimal(10,2)"),
        ] {
            assert_eq!(
                norm_type(d, declared),
                norm_type(d, catalog),
                "{d:?} {declared} vs {catalog}"
            );
        }
        assert_ne!(
            norm_type(Mysql, "TINYINT(1)"),
            norm_type(Mysql, "tinyint(4)")
        );
        assert_ne!(
            norm_type(Postgres, "VARCHAR(20)"),
            norm_type(Postgres, "character varying(21)")
        );
        assert_eq!(sqlite_affinity("varchar(20)"), "TEXT");
        assert_eq!(sqlite_affinity("bigint"), "INTEGER");
        assert_eq!(sqlite_affinity("decimal(10,2)"), "NUMERIC");
        assert_eq!(sqlite_affinity("boolean"), "NUMERIC");
        assert_eq!(sqlite_affinity("double"), "REAL");
    }

    /// Existing inspectdb turns a MySQL expression default into SQL
    /// that does not parse: the catalog's escapes and introducer stay.
    #[cfg(feature = "mysql")]
    #[test]
    fn existing_inspectdb_mysql_default_keeps_catalog_escapes() {
        let got = crate::migrate::inspectdb::mysql_default_sql(
            r"_utf8mb4\'{}\'",
            "json",
            "DEFAULT_GENERATED",
        );
        assert_eq!(
            got, r"(_utf8mb4\'{}\')",
            "the bug this test documents is fixed; update the report"
        );
        assert_eq!(
            mysql_catalog_default_sql(r"_utf8mb4\'{}\'", "json", "DEFAULT_GENERATED"),
            "(_utf8mb4'{}')"
        );
    }
}
