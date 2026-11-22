//! SPIKE S6: the declared side (what a v2 file holds) and a minimal
//! renderer, so tests build each op's effect with raw defaults as written.
//! S3 owns the real per-op renderer; this one only feeds the probes.

use crate::migrate::snapshot::FieldSnapshot;
use crate::sql::{Dialect, DialectKind};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnDecl {
    pub name: String,
    /// v1/v2 type token: `i64`, `string`, `datetime`, `json`, ...
    pub ty: String,
    pub max_length: Option<u32>,
    pub nullable: bool,
    /// Raw SQL text after DEFAULT (decision 8), rendered as written.
    pub default: Option<String>,
    /// STORED generation expression.
    pub generated: Option<String>,
    pub comment: Option<String>,
    pub primary_key: bool,
    pub auto: bool,
}

impl ColumnDecl {
    #[must_use]
    pub fn new(name: &str, ty: &str) -> Self {
        Self {
            name: name.into(),
            ty: ty.into(),
            max_length: None,
            nullable: true,
            default: None,
            generated: None,
            comment: None,
            primary_key: false,
            auto: false,
        }
    }

    pub(crate) fn snapshot(&self) -> FieldSnapshot {
        FieldSnapshot {
            name: self.name.clone(),
            column: self.name.clone(),
            ty: self.ty.clone(),
            nullable: self.nullable,
            primary_key: self.primary_key,
            max_length: self.max_length,
            min: None,
            max: None,
            default: self.default.clone(),
            auto: self.auto,
            unique: false,
            case_insensitive: false,
            generated_as: self.generated.clone(),
            db_comment: self.comment.clone(),
            fk: None,
        }
    }

    /// The DDL type this column renders to, through the v1 type mapping.
    #[must_use]
    pub fn sql_type(&self, d: &dyn Dialect) -> String {
        crate::migrate::diff::sql_type_with_dialect(&self.snapshot(), d)
    }

    #[must_use]
    pub fn is_bool(&self) -> bool {
        self.ty == "bool"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexDecl {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub method: String,
    pub predicate: Option<String>,
    pub include: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckDecl {
    pub name: String,
    pub expr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FkDecl {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    /// SQL token; `None` = NO ACTION.
    pub on_delete: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TableDecl {
    pub name: String,
    pub columns: Vec<ColumnDecl>,
    pub comment: Option<String>,
    pub checks: Vec<CheckDecl>,
    pub fks: Vec<FkDecl>,
}

/// The ops S6 gives probe specs for. A unique is a unique `AddIndex` (3.3).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum SpikeOp {
    CreateTable(TableDecl),
    DropTable {
        was: TableDecl,
    },
    AddColumn {
        table: String,
        column: ColumnDecl,
    },
    DropColumn {
        table: String,
        was: ColumnDecl,
    },
    AlterColumn {
        table: String,
        from: ColumnDecl,
        to: ColumnDecl,
    },
    AddIndex(IndexDecl),
    DropIndex {
        was: IndexDecl,
    },
    AddCheck {
        table: String,
        check: CheckDecl,
    },
    DropCheck {
        table: String,
        was: CheckDecl,
    },
    AddFk {
        table: String,
        fk: FkDecl,
    },
    DropFk {
        table: String,
        was: FkDecl,
    },
}

fn q(d: &dyn Dialect, s: &str) -> String {
    d.quote_ident(s)
}

fn list(d: &dyn Dialect, cols: &[String]) -> String {
    cols.iter().map(|c| q(d, c)).collect::<Vec<_>>().join(", ")
}

/// One column's definition, default as written.
#[must_use]
pub fn column_sql(d: &dyn Dialect, c: &ColumnDecl) -> String {
    let mut s = format!("{} {}", q(d, &c.name), c.sql_type(d));
    if let Some(g) = &c.generated {
        s.push_str(&format!(" GENERATED ALWAYS AS ({g}) STORED"));
    }
    if c.primary_key && !(c.auto && d.kind() == DialectKind::Sqlite) {
        s.push_str(" PRIMARY KEY");
    }
    if !c.nullable && !c.primary_key {
        s.push_str(" NOT NULL");
    }
    if let Some(def) = &c.default {
        s.push_str(" DEFAULT ");
        s.push_str(def);
    }
    if let (Some(cm), DialectKind::Mysql) = (&c.comment, d.kind()) {
        s.push_str(&format!(" COMMENT {}", d.quote_literal(cm)));
    }
    s
}

fn fk_sql(d: &dyn Dialect, fk: &FkDecl) -> String {
    format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {}",
        q(d, &fk.name),
        list(d, &fk.columns),
        q(d, &fk.ref_table),
        list(d, &fk.ref_columns),
        fk.on_delete.as_deref().unwrap_or("NO ACTION")
    )
}

/// The statements that apply `op`. SQLite ops that need a rebuild are
/// out of scope here (S3): tests declare them in `CreateTable`.
#[must_use]
pub fn render(d: &dyn Dialect, op: &SpikeOp) -> Vec<String> {
    let k = d.kind();
    match op {
        SpikeOp::CreateTable(t) => {
            let mut parts: Vec<String> = t.columns.iter().map(|c| column_sql(d, c)).collect();
            parts.extend(
                t.checks
                    .iter()
                    .map(|c| format!("CONSTRAINT {} CHECK ({})", q(d, &c.name), c.expr)),
            );
            parts.extend(t.fks.iter().map(|f| fk_sql(d, f)));
            let mut s = format!("CREATE TABLE {} ({})", q(d, &t.name), parts.join(", "));
            if let (Some(cm), DialectKind::Mysql) = (&t.comment, k) {
                s.push_str(&format!(" COMMENT = {}", d.quote_literal(cm)));
            }
            let mut out = vec![s];
            if k == DialectKind::Postgres {
                if let Some(cm) = &t.comment {
                    out.push(format!(
                        "COMMENT ON TABLE {} IS {}",
                        q(d, &t.name),
                        d.quote_literal(cm)
                    ));
                }
                for c in &t.columns {
                    if let Some(cm) = &c.comment {
                        out.push(format!(
                            "COMMENT ON COLUMN {}.{} IS {}",
                            q(d, &t.name),
                            q(d, &c.name),
                            d.quote_literal(cm)
                        ));
                    }
                }
            }
            out
        }
        SpikeOp::DropTable { was } => vec![format!("DROP TABLE {}", q(d, &was.name))],
        SpikeOp::AddColumn { table, column } => {
            vec![format!(
                "ALTER TABLE {} ADD COLUMN {}",
                q(d, table),
                column_sql(d, column)
            )]
        }
        SpikeOp::DropColumn { table, was } => {
            vec![format!(
                "ALTER TABLE {} DROP COLUMN {}",
                q(d, table),
                q(d, &was.name)
            )]
        }
        SpikeOp::AlterColumn { table, to, .. } => match k {
            DialectKind::Mysql => vec![format!(
                "ALTER TABLE {} MODIFY {}",
                q(d, table),
                column_sql(d, to)
            )],
            _ => {
                let (t, c) = (q(d, table), q(d, &to.name));
                vec![
                    format!("ALTER TABLE {t} ALTER COLUMN {c} TYPE {}", to.sql_type(d)),
                    format!(
                        "ALTER TABLE {t} ALTER COLUMN {c} {} NOT NULL",
                        if to.nullable { "DROP" } else { "SET" }
                    ),
                    match &to.default {
                        Some(def) => format!("ALTER TABLE {t} ALTER COLUMN {c} SET DEFAULT {def}"),
                        None => format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT"),
                    },
                ]
            }
        },
        SpikeOp::AddIndex(ix) => {
            // The legacy method clause; PG puts it before the key list, MySQL after.
            let method = d.index_method_clause(&ix.method);
            let (pre, post) = if k == DialectKind::Postgres {
                (method, String::new())
            } else {
                (String::new(), method)
            };
            let mut s = format!(
                "CREATE {}INDEX {} ON {}{pre} ({}){post}",
                if ix.unique { "UNIQUE " } else { "" },
                q(d, &ix.name),
                q(d, &ix.table),
                list(d, &ix.columns)
            );
            if !ix.include.is_empty() && k == DialectKind::Postgres {
                s.push_str(&format!(" INCLUDE ({})", list(d, &ix.include)));
            }
            if let Some(p) = &ix.predicate {
                s.push_str(&format!(" WHERE {p}"));
            }
            vec![s]
        }
        SpikeOp::DropIndex { was } => match k {
            DialectKind::Mysql => vec![format!(
                "DROP INDEX {} ON {}",
                q(d, &was.name),
                q(d, &was.table)
            )],
            _ => vec![format!("DROP INDEX {}", q(d, &was.name))],
        },
        SpikeOp::AddCheck { table, check } => vec![format!(
            "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({})",
            q(d, table),
            q(d, &check.name),
            check.expr
        )],
        SpikeOp::DropCheck { table, was } => d
            .drop_check_constraint_sql(crate::sql::TableTarget::in_schema(None, table), &was.name)
            .into_iter()
            .collect(),
        SpikeOp::AddFk { table, fk } => {
            vec![format!("ALTER TABLE {} ADD {}", q(d, table), fk_sql(d, fk))]
        }
        SpikeOp::DropFk { table, was } => d
            .drop_foreign_key_sql(crate::sql::TableTarget::in_schema(None, table), &was.name)
            .into_iter()
            .collect(),
    }
}
