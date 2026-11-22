//! SPIKE S4: the load-time scan of a tenant file's SQL text (design 8.2,
//! `MV2-E031`). The search path is the real guarantee; this catches a
//! schema-qualified name early. Throwaway.

/// Load error code for a tenant file naming another schema.
pub const E031: &str = "MV2-E031";
/// Load error code for `{{t:name}}` naming a table outside tenant state.
pub const E_UNKNOWN_TABLE: &str = "MV2-E032";

/// What a tenant file's SQL may qualify names with.
#[derive(Debug, Clone)]
pub struct Allowed {
    /// Configured extension schemas, plus `pg_catalog` / `information_schema`.
    pub schemas: Vec<String>,
    /// Always refused as a qualifier, in any position: `public`, the registry schema.
    pub denied: Vec<String>,
}

impl Allowed {
    #[must_use]
    pub fn new(extension_schemas: &[&str], registry_schema: &str) -> Self {
        let mut schemas: Vec<String> = extension_schemas.iter().map(|s| (*s).to_owned()).collect();
        schemas.push("pg_catalog".into());
        schemas.push("information_schema".into());
        let mut denied = vec!["public".to_owned()];
        if registry_schema != "public" {
            denied.push(registry_schema.to_owned());
        }
        Self { schemas, denied }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Name as written when quoted, ASCII-lowercased when not.
    Ident(String),
    Dot,
    Cast,
    Punct(char),
    Other,
}

/// Lex `sql`, skipping string literals and comments. Dollar-quoted bodies
/// (DO blocks, function bodies) are lexed as SQL: they run on the tenant too.
fn lex(sql: &str) -> Vec<Tok> {
    let b: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            _ if c.is_whitespace() => i += 1,
            '-' if b.get(i + 1) == Some(&'-') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '#' => {
                // MySQL line comment.
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            '\'' => {
                // Backslash escapes cover E'' and MySQL; '' doubles in both.
                i += 1;
                while i < b.len() {
                    if b[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == '\'' {
                        if b.get(i + 1) == Some(&'\'') {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
                out.push(Tok::Other);
            }
            '"' | '`' => {
                let q = c;
                let mut name = String::new();
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            name.push(q);
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    name.push(b[i]);
                    i += 1;
                }
                i += 1;
                out.push(Tok::Ident(name));
            }
            '$' => {
                // `$tag$` opener: drop the delimiters, keep lexing the body.
                let mut j = i + 1;
                while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                    j += 1;
                }
                if b.get(j) == Some(&'$') {
                    i = j + 1;
                } else {
                    // `$1` placeholder.
                    i = j.max(i + 1);
                    out.push(Tok::Other);
                }
            }
            ':' if b.get(i + 1) == Some(&':') => {
                out.push(Tok::Cast);
                i += 2;
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let mut name = String::new();
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$') {
                    name.push(b[i].to_ascii_lowercase());
                    i += 1;
                }
                out.push(Tok::Ident(name));
            }
            _ if c.is_ascii_digit() => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '.') {
                    i += 1;
                }
                out.push(Tok::Other);
            }
            _ => {
                out.push(Tok::Punct(c));
                i += 1;
            }
        }
    }
    out
}

const RELATION_KEYWORDS: &[&str] = &[
    "from",
    "join",
    "into",
    "update",
    "table",
    "references",
    "only",
    "using",
    "truncate",
    "sequence",
    "view",
    "type",
    "function",
    "procedure",
    "trigger",
    "domain",
];

/// Refuse a schema-qualified name `sql` may not use in a tenant file.
///
/// `q.x` is refused when `q` is denied (any position), or when it is in a
/// relation, type or function-call position and `q` is not allowed.
/// `alias.column` in an expression passes: the search path guards it.
///
/// # Errors
/// `MV2-E031` with the qualified name.
pub fn scan(sql: &str, allowed: &Allowed) -> Result<(), String> {
    let toks = lex(sql);
    for i in 0..toks.len() {
        let (Some(Tok::Ident(q)), Some(Tok::Dot), Some(Tok::Ident(x))) =
            (toks.get(i), toks.get(i + 1), toks.get(i + 2))
        else {
            continue;
        };
        // Skip the tail of `a.b.c`: only the first qualifier counts.
        if i > 0 && toks[i - 1] == Tok::Dot {
            continue;
        }
        let refuse = || {
            Err(format!(
                "{E031}: tenant SQL names `{q}.{x}`; tenant files may not name another schema"
            ))
        };
        if allowed.denied.iter().any(|d| d == q) {
            return refuse();
        }
        if allowed.schemas.iter().any(|s| s == q) {
            continue;
        }
        let three_part = toks.get(i + 3) == Some(&Tok::Dot);
        let prev = i.checked_sub(1).map(|p| &toks[p]);
        let next = toks.get(i + 3);
        let relation =
            matches!(prev, Some(Tok::Ident(k)) if RELATION_KEYWORDS.contains(&k.as_str()));
        let type_pos = prev == Some(&Tok::Cast);
        let call = next == Some(&Tok::Punct('('));
        // `CREATE INDEX .. ON s.t (..)` / `ON s.t USING ..`, not `JOIN .. ON a.x = b.y`.
        let index_on = matches!(prev, Some(Tok::Ident(k)) if k == "on")
            && matches!(next, Some(Tok::Punct('(')) | Some(Tok::Ident(_)));
        if three_part || relation || type_pos || call || index_on {
            return refuse();
        }
    }
    Ok(())
}

/// Resolve `{{t:name}}` placeholders against tenant state; a registry table
/// is not in it, so it fails at load.
///
/// # Errors
/// `MV2-E032` naming the table.
pub fn check_placeholders(sql: &str, tenant_tables: &[&str]) -> Result<(), String> {
    let mut rest = sql;
    while let Some(start) = rest.find("{{t:") {
        let after = &rest[start + 4..];
        let end = after.find("}}").ok_or("unterminated {{t: placeholder")?;
        let name = &after[..end];
        if !tenant_tables.contains(&name) {
            return Err(format!(
                "{E_UNKNOWN_TABLE}: `{{{{t:{name}}}}}` is not a table in tenant state \
                 (registry tables never are)"
            ));
        }
        rest = &after[end + 2..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Allowed {
        Allowed::new(&["extensions"], "registry")
    }

    #[test]
    fn refuses_qualified_registry_names() {
        for sql in [
            "UPDATE public.org SET x = 1",
            "INSERT INTO \"public\".\"org\" (id) VALUES (1)",
            "SELECT * FROM registry.org",
            "SELECT o.id FROM users u JOIN public.org o ON o.id = u.org_id",
            "DELETE FROM users WHERE org_id IN (SELECT id FROM public.org)",
            "SELECT 1 FROM users WHERE public.org.id = 1",
            "ALTER TABLE users ADD CHECK (email::public.citext <> '')",
            "DO $$ BEGIN UPDATE public.org SET x = 1; END $$",
            "SELECT * FROM other_tenant.users",
            "SELECT other_tenant.f(1)",
            "SELECT 1 FROM mydb.users",
            "CREATE INDEX i ON other.users (email)",
            "SELECT * FROM `registry_db`.`org`",
            "SELECT a.b.c FROM t",
        ] {
            assert!(
                scan(sql, &allowed()).is_err_and(|e| e.starts_with(E031)),
                "{sql}"
            );
        }
    }

    #[test]
    fn passes_tenant_sql() {
        for sql in [
            "UPDATE users SET email = lower(email)",
            "SELECT u.id, u.email FROM users u WHERE u.id = $1",
            "SELECT 'public.org' AS s",
            "SELECT 1 -- public.org\nFROM users",
            "SELECT 1 /* registry.org */ FROM users",
            "SELECT extensions.similarity(name, 'x') FROM users",
            "SELECT name::extensions.citext FROM users",
            "SELECT pg_catalog.now()",
            "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = excluded.a",
            "SELECT 1.5, t.x FROM t JOIN s ON t.id = s.id",
            "CREATE INDEX i ON users (email)",
        ] {
            assert_eq!(scan(sql, &allowed()), Ok(()), "{sql}");
        }
    }

    #[test]
    fn placeholder_for_a_registry_table_fails_at_load() {
        assert!(check_placeholders("UPDATE {{t:users}} SET x = 1", &["users"]).is_ok());
        let e = check_placeholders("SELECT * FROM {{t:org}}", &["users"]).unwrap_err();
        assert!(e.starts_with(E_UNKNOWN_TABLE), "{e}");
    }
}
