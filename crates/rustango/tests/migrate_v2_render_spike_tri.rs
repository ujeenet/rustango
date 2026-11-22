//! SPIKE S3 (migrations engine v2): per-op rendering. Golden SQL per unit
//! for every op on PG, MySQL and SQLite, rendered with no pool, then each
//! golden applied live and checked against the after-state. Throwaway.

#![cfg(all(
    feature = "migrate-v2-spike",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::path::PathBuf;

use rustango::migrate::v2_spike::catalog::read_table;
use rustango::migrate::v2_spike::exec::run_units;
use rustango::migrate::v2_spike::ops::{
    CheckDecl, ColumnDecl, FkDecl, IndexDecl, SpikeOp, TableDecl,
};
use rustango::migrate::v2_spike::probe::{probe, Verdict};
use rustango::migrate::v2_spike::render::{
    apply, golden, render_file, ExternTable, IndexState, Op, PerDialect, State, Unit, UnitKind,
    E_CHECK_COL, E_DIALECT, E_FILL, E_FK_INDEX, E_FK_TYPE, E_RAW, E_STATE, E_UNSUPPORTED,
};
use rustango::sql::{
    raw_execute_pool, raw_query_pool, Dialect, DialectKind, MySql, Pool, Postgres, Sqlite,
};
use rustango::testkit::matrix::drop_table;
use rustango::tri_dialect_test;

/// Child tables first, so MySQL drops them before what they reference.
const TABLES: &[&str] = &[
    "s3_child",
    "s3_t",
    "s3_defaults",
    "s3_adopt",
    "s3_parent",
    "s3_renamed",
    "s3_ext",
    "_rustango_rebuild_s3_child",
    "_rustango_rebuild_s3_t",
    "_rustango_rebuild_s3_parent",
];

async fn clean(pool: &Pool) {
    for t in TABLES {
        drop_table(pool, t).await;
    }
}

const ALL: [DialectKind; 3] = [
    DialectKind::Postgres,
    DialectKind::Mysql,
    DialectKind::Sqlite,
];

fn dialect(k: DialectKind) -> &'static dyn Dialect {
    match k {
        DialectKind::Postgres => &Postgres,
        DialectKind::Mysql => &MySql,
        _ => &Sqlite,
    }
}

fn dname(k: DialectKind) -> &'static str {
    match k {
        DialectKind::Postgres => "postgres",
        DialectKind::Mysql => "mysql",
        _ => "sqlite",
    }
}

// ------------------------------------------------------------- builders

fn c(name: &str, ty: &str) -> ColumnDecl {
    ColumnDecl::new(name, ty)
}

trait Col {
    fn max(self, n: u32) -> Self;
    fn not_null(self) -> Self;
    fn default(self, d: &str) -> Self;
    fn comment(self, s: &str) -> Self;
    fn pk(self) -> Self;
    fn auto(self) -> Self;
}

impl Col for ColumnDecl {
    fn max(mut self, n: u32) -> Self {
        self.max_length = Some(n);
        self
    }
    fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }
    fn default(mut self, d: &str) -> Self {
        self.default = Some(d.into());
        self
    }
    fn comment(mut self, s: &str) -> Self {
        self.comment = Some(s.into());
        self
    }
    fn pk(mut self) -> Self {
        self.primary_key = true;
        self.nullable = false;
        self
    }
    fn auto(mut self) -> Self {
        self.auto = true;
        self
    }
}

fn table(name: &str, columns: Vec<ColumnDecl>) -> TableDecl {
    TableDecl {
        name: name.into(),
        columns,
        comment: None,
        checks: vec![],
        fks: vec![],
    }
}

fn ix(name: &str, table: &str, cols: &[&str], unique: bool) -> IndexState {
    IndexState {
        decl: IndexDecl {
            name: name.into(),
            table: table.into(),
            columns: cols.iter().map(|s| (*s).to_owned()).collect(),
            unique,
            method: "btree".into(),
            predicate: None,
            include: vec![],
        },
        skipped_on: vec![],
    }
}

fn fk(name: &str, cols: &[&str], to: &str, on: &[&str], del: Option<&str>) -> FkDecl {
    FkDecl {
        name: name.into(),
        columns: cols.iter().map(|s| (*s).to_owned()).collect(),
        ref_table: to.into(),
        ref_columns: on.iter().map(|s| (*s).to_owned()).collect(),
        on_delete: del.map(str::to_owned),
    }
}

fn check(name: &str, expr: &str) -> CheckDecl {
    CheckDecl {
        name: name.into(),
        expr: expr.into(),
    }
}

fn create(t: TableDecl, indexes: Vec<IndexState>) -> Op {
    Op::CreateTable { table: t, indexes }
}

fn parent() -> TableDecl {
    table(
        "s3_parent",
        vec![c("id", "i64").pk().auto(), c("name", "string").max(50)],
    )
}

fn child(fks: Vec<FkDecl>) -> TableDecl {
    let mut t = table(
        "s3_child",
        vec![c("id", "i64").pk(), c("parent_id", "i64").not_null()],
    );
    t.fks = fks;
    t
}

fn child_fk(del: Option<&str>) -> FkDecl {
    fk(
        "s3_child_parent_fk",
        &["parent_id"],
        "s3_parent",
        &["id"],
        del,
    )
}

fn child_idx() -> IndexState {
    ix("s3_child_parent_idx", "s3_child", &["parent_id"], false)
}

fn s3_t(cols: Vec<ColumnDecl>) -> TableDecl {
    let mut all = vec![c("id", "i64").pk()];
    all.extend(cols);
    table("s3_t", all)
}

fn all_sql(sql: &str) -> PerDialect {
    PerDialect {
        postgres: Some(vec![sql.into()]),
        mysql: Some(vec![sql.into()]),
        sqlite: Some(vec![sql.into()]),
    }
}

/// The "now" default as each backend's file writes it (2.6).
fn now(k: DialectKind) -> String {
    match k {
        DialectKind::Postgres => "now()".into(),
        DialectKind::Mysql => "(UTC_TIMESTAMP(6))".into(),
        _ => "(strftime('%Y-%m-%dT%H:%M:%f000+00:00','now'))".into(),
    }
}

/// One row per 2.6 value, written for `k`.
fn defaults_cols(k: DialectKind) -> Vec<ColumnDecl> {
    let mut v = vec![
        c("s", "string").max(20).default("'en'"),
        c("q", "string").max(20).default("'it''s'"),
        c("i", "i64").default("0"),
        c("d", "decimal").default("0.00"),
        c("b", "bool").default("TRUE"),
        c("dt", "date").default("'2026-01-01'"),
        c("ts", "datetime").default(&now(k)),
    ];
    v.extend(match k {
        DialectKind::Postgres => vec![
            c("u", "uuid").default("gen_random_uuid()"),
            c("j", "json").default("'{}'::jsonb"),
            c("ja", "json").default("'[]'::jsonb"),
        ],
        DialectKind::Mysql => vec![
            c("lt", "string").default("('en')"),
            c("u", "uuid").default("(UUID())"),
            c("j", "json").default("('{}')"),
            c("ja", "json").default("('[]')"),
        ],
        _ => vec![
            c("u", "uuid").default(&Sqlite.translate_default_expr(
                "gen_random_uuid()",
                "uuid",
                None,
            )),
            c("j", "json").default("'{}'"),
            c("ja", "json").default("'[]'"),
        ],
    });
    v
}

// ---------------------------------------------------------------- cases

struct Case {
    name: &'static str,
    setup: Vec<Op>,
    /// Tables the setup created that the chain under test does not own.
    forget: Vec<&'static str>,
    seed: Vec<&'static str>,
    ops: Vec<Op>,
}

fn case(name: &'static str, setup: Vec<Op>, seed: Vec<&'static str>, ops: Vec<Op>) -> Case {
    Case {
        name,
        setup,
        forget: vec![],
        seed,
        ops,
    }
}

const SEED_PARENT: &str = "INSERT INTO s3_parent (name) VALUES ('a')";

#[allow(clippy::too_many_lines)]
fn cases(k: DialectKind) -> Vec<Case> {
    let mut created_child = child(vec![child_fk(Some("CASCADE"))]);
    created_child.columns.extend([
        c("total", "decimal").not_null().default("0"),
        c("note", "string").max(20).comment("a note"),
    ]);
    created_child.checks = vec![check("s3_child_total_ge0", "total >= 0")];
    created_child.comment = Some("children".into());
    let check_t = || {
        let mut t = s3_t(vec![c("a", "i32")]);
        t.checks = vec![check("s3_t_a_pos", "a > 0")];
        t
    };
    let retype_parent = table("s3_t", vec![c("id", "i32").pk()]);
    let mut retype_child = table("s3_child", vec![c("id", "i64").pk(), c("pid", "i32")]);
    let retype_fk = fk("s3_child_pid_fk", &["pid"], "s3_t", &["id"], None);
    retype_child.fks = vec![retype_fk.clone()];
    let mut partial = ix("s3_t_a_pos", "s3_t", &["a"], false);
    partial.decl.predicate = Some("a > 0".into());
    partial.skipped_on = vec![DialectKind::Mysql];
    let mut hash = ix("s3_t_name_hash", "s3_t", &["name"], false);
    hash.decl.method = "hash".into();
    hash.skipped_on = vec![DialectKind::Mysql, DialectKind::Sqlite];
    let mut ext_child = table(
        "s3_child",
        vec![c("id", "i64").pk(), c("ext_id", "i64").not_null()],
    );
    ext_child.fks = vec![fk("s3_child_ext_fk", &["ext_id"], "s3_ext", &["id"], None)];
    let adopt = table(
        "s3_adopt",
        vec![c("id", "i64").pk(), c("label", "string").max(20)],
    );

    vec![
        case(
            "create_table",
            vec![create(parent(), vec![])],
            vec![],
            vec![create(created_child, vec![child_idx()])],
        ),
        case(
            "drop_table",
            vec![create(s3_t(vec![c("v", "string").max(10)]), vec![])],
            vec![],
            vec![Op::DropTable {
                was: s3_t(vec![c("v", "string").max(10)]),
            }],
        ),
        case(
            "rename_table",
            vec![create(parent(), vec![])],
            vec![SEED_PARENT],
            vec![
                Op::RenameTable {
                    from: "s3_parent".into(),
                    to: "s3_renamed".into(),
                },
                // PG would pick `_pkey1`/`_seq1`; the renames keep names derivable.
                create(parent(), vec![]),
            ],
        ),
        case(
            "alter_table_comment",
            vec![create(parent(), vec![])],
            vec![],
            vec![Op::AlterTableComment {
                table: "s3_parent".into(),
                from: None,
                to: Some("people".into()),
            }],
        ),
        case(
            "add_column",
            vec![create(parent(), vec![])],
            vec![SEED_PARENT],
            vec![
                Op::AddColumn {
                    table: "s3_parent".into(),
                    column: c("nick", "string").max(20).comment("short name"),
                    fill: None,
                },
                Op::AddColumn {
                    table: "s3_parent".into(),
                    column: c("flag", "bool").not_null().default("TRUE"),
                    fill: None,
                },
                Op::AddColumn {
                    table: "s3_parent".into(),
                    column: c("score", "i32").not_null(),
                    fill: Some("0".into()),
                },
                Op::AddColumn {
                    table: "s3_parent".into(),
                    column: c("created", "datetime").default(&now(k)),
                    fill: None,
                },
            ],
        ),
        case(
            "drop_column",
            vec![create(s3_t(vec![c("legacy", "bool").not_null().default("FALSE")]), vec![])],
            vec![],
            vec![Op::DropColumn {
                table: "s3_t".into(),
                was: c("legacy", "bool").not_null().default("FALSE"),
            }],
        ),
        case(
            "rename_column",
            vec![create(
                s3_t(vec![c("name", "string").max(30)]),
                vec![ix("s3_t_name_idx", "s3_t", &["name"], false)],
            )],
            vec![],
            vec![Op::RenameColumn {
                table: "s3_t".into(),
                from: "name".into(),
                to: "full_name".into(),
            }],
        ),
        case(
            "alter_column_widen_not_null",
            vec![create(parent(), vec![])],
            vec![SEED_PARENT],
            vec![Op::AlterColumn {
                table: "s3_parent".into(),
                from: c("name", "string").max(50),
                to: c("name", "string").max(100).not_null(),
            }],
        ),
        case(
            "alter_column_type_noncastable_default",
            vec![create(s3_t(vec![c("code", "string").max(10).default("'7'")]), vec![])],
            vec!["INSERT INTO s3_t (id, code) VALUES (1, '42')"],
            vec![Op::AlterColumn {
                table: "s3_t".into(),
                from: c("code", "string").max(10).default("'7'"),
                to: c("code", "i64").default("0"),
            }],
        ),
        case(
            "alter_column_default",
            vec![create(
                s3_t(vec![c("status", "string").max(10).not_null().default("'new'")]),
                vec![],
            )],
            vec![],
            vec![Op::AlterColumn {
                table: "s3_t".into(),
                from: c("status", "string").max(10).not_null().default("'new'"),
                to: c("status", "string").max(10).not_null().default("'open'"),
            }],
        ),
        case(
            "add_index",
            vec![create(s3_t(vec![c("a", "i32"), c("b", "string").max(20)]), vec![])],
            vec![],
            vec![
                Op::AddIndex {
                    index: ix("s3_t_a_idx", "s3_t", &["a"], false),
                    concurrently: None,
                },
                Op::AddIndex {
                    index: ix("s3_t_b_uq", "s3_t", &["b"], true),
                    concurrently: None,
                },
                Op::AddIndex {
                    index: partial,
                    concurrently: None,
                },
                Op::AddIndex {
                    index: ix("s3_t_ab_idx", "s3_t", &["a", "b"], false),
                    concurrently: Some(false),
                },
            ],
        ),
        case(
            "drop_index",
            vec![create(
                s3_t(vec![c("a", "i32")]),
                vec![ix("s3_t_a_idx", "s3_t", &["a"], false)],
            )],
            vec![],
            vec![Op::DropIndex {
                was: ix("s3_t_a_idx", "s3_t", &["a"], false),
                concurrently: None,
            }],
        ),
        case(
            "rename_index",
            vec![create(
                s3_t(vec![c("a", "i32")]),
                vec![ix("s3_t_a_idx", "s3_t", &["a"], false)],
            )],
            vec![],
            vec![Op::RenameIndex {
                table: "s3_t".into(),
                from: "s3_t_a_idx".into(),
                to: "s3_t_a_idx2".into(),
            }],
        ),
        case(
            "add_check",
            vec![create(s3_t(vec![c("a", "i32")]), vec![])],
            vec!["INSERT INTO s3_t (id, a) VALUES (1, 5)"],
            vec![Op::AddCheck {
                table: "s3_t".into(),
                check: check("s3_t_a_pos", "a > 0"),
            }],
        ),
        case(
            "drop_check",
            vec![create(check_t(), vec![])],
            vec![],
            vec![Op::DropCheck {
                table: "s3_t".into(),
                was: check("s3_t_a_pos", "a > 0"),
            }],
        ),
        case(
            "add_fk",
            vec![create(parent(), vec![]), create(child(vec![]), vec![child_idx()])],
            vec![SEED_PARENT, "INSERT INTO s3_child (id, parent_id) VALUES (1, 1)"],
            vec![Op::AddFk {
                table: "s3_child".into(),
                fk: child_fk(Some("CASCADE")),
            }],
        ),
        case(
            "drop_fk",
            vec![
                create(parent(), vec![]),
                create(child(vec![child_fk(None)]), vec![child_idx()]),
            ],
            vec![],
            vec![Op::DropFk {
                table: "s3_child".into(),
                was: child_fk(None),
            }],
        ),
        case(
            "alter_fk",
            vec![
                create(parent(), vec![]),
                create(child(vec![child_fk(None)]), vec![child_idx()]),
            ],
            vec![],
            vec![Op::AlterFk {
                table: "s3_child".into(),
                from: child_fk(None),
                to: child_fk(Some("CASCADE")),
            }],
        ),
        case(
            // 3780: drop_fk -> retype both ends -> add_fk.
            "fk_retype",
            vec![
                create(retype_parent, vec![]),
                create(retype_child, vec![ix("s3_child_pid_idx", "s3_child", &["pid"], false)]),
            ],
            vec![],
            vec![
                Op::DropFk {
                    table: "s3_child".into(),
                    was: retype_fk.clone(),
                },
                Op::AlterColumn {
                    table: "s3_child".into(),
                    from: c("pid", "i32"),
                    to: c("pid", "i64"),
                },
                Op::AlterColumn {
                    table: "s3_t".into(),
                    from: c("id", "i32").pk(),
                    to: c("id", "i64").pk(),
                },
                Op::AddFk {
                    table: "s3_child".into(),
                    fk: retype_fk,
                },
            ],
        ),
        case(
            // 3959: drop_check -> rename_column -> add_check with the new text.
            "rename_column_under_check",
            vec![create(check_t(), vec![])],
            vec![],
            vec![
                Op::DropCheck {
                    table: "s3_t".into(),
                    was: check("s3_t_a_pos", "a > 0"),
                },
                Op::RenameColumn {
                    table: "s3_t".into(),
                    from: "a".into(),
                    to: "b".into(),
                },
                Op::AddCheck {
                    table: "s3_t".into(),
                    check: check("s3_t_b_pos", "b > 0"),
                },
            ],
        ),
        case(
            "raw_sql",
            vec![create(s3_t(vec![c("name", "string").max(50)]), vec![])],
            vec![],
            vec![
                Op::RawSql {
                    up: all_sql("CREATE INDEX s3_t_name_raw ON {{t:s3_t}} ({{c:name}})"),
                    down: all_sql("DROP INDEX s3_t_name_raw"),
                    state: vec![Op::AddIndex {
                        index: ix("s3_t_name_raw", "s3_t", &["name"], false),
                        concurrently: None,
                    }],
                    autocommit: false,
                },
                Op::RawSql {
                    up: PerDialect {
                        postgres: Some(vec![
                            "CREATE INDEX CONCURRENTLY s3_t_name_hash ON {{t:s3_t}} USING hash ({{c:name}})"
                                .into(),
                        ]),
                        mysql: None,
                        sqlite: None,
                    },
                    down: PerDialect {
                        postgres: Some(vec!["DROP INDEX CONCURRENTLY IF EXISTS s3_t_name_hash".into()]),
                        mysql: None,
                        sqlite: None,
                    },
                    state: vec![Op::AddIndex {
                        index: hash,
                        concurrently: None,
                    }],
                    autocommit: true,
                },
            ],
        ),
        case(
            "state_only",
            vec![
                create(s3_t(vec![c("a", "i32")]), vec![]),
                Op::RawSql {
                    up: all_sql("CREATE INDEX s3_t_a_idx ON s3_t (a)"),
                    down: PerDialect::default(),
                    state: vec![],
                    autocommit: false,
                },
            ],
            vec![],
            vec![Op::StateOnly {
                ops: vec![Op::AddIndex {
                    index: ix("s3_t_a_idx", "s3_t", &["a"], false),
                    concurrently: None,
                }],
                reason: "index built by hand".into(),
            }],
        ),
        Case {
            name: "extern_and_adopt_table",
            setup: vec![
                create(table("s3_ext", vec![c("id", "i64").pk(), c("label", "string").max(20)]), vec![]),
                create(adopt.clone(), vec![]),
            ],
            forget: vec!["s3_ext", "s3_adopt"],
            seed: vec![],
            ops: vec![
                Op::ExternTable(ExternTable {
                    name: "s3_ext".into(),
                    columns: vec![c("id", "i64").pk()],
                    unique: vec![vec!["id".into()]],
                }),
                Op::AdoptTable {
                    table: adopt,
                    indexes: vec![],
                },
                create(ext_child, vec![ix("s3_child_ext_idx", "s3_child", &["ext_id"], false)]),
            ],
        },
        case(
            "create_extension",
            vec![],
            vec![],
            vec![Op::CreateExtension {
                name: "pg_trgm".into(),
                schema: None,
                skipped_on: vec![DialectKind::Mysql, DialectKind::Sqlite],
            }],
        ),
        case(
            "defaults_as_written",
            vec![],
            vec![],
            vec![create(
                table("s3_defaults", {
                    let mut v = vec![c("id", "i64").pk()];
                    v.extend(defaults_cols(k));
                    v
                }),
                vec![],
            )],
        ),
    ]
}

/// The setup applied, then the forgotten tables removed from state.
fn setup_state(case: &Case, d: &dyn Dialect) -> (Vec<Unit>, State) {
    let (units, mut s0) = render_file(&State::default(), &case.setup, d)
        .unwrap_or_else(|e| panic!("{} setup on {}: {e:?}", case.name, d.name()));
    for t in &case.forget {
        s0.tables.remove(*t);
    }
    (units, s0)
}

fn render_case(case: &Case, d: &dyn Dialect) -> (State, Vec<Unit>, State) {
    let (_, s0) = setup_state(case, d);
    let (units, s1) = render_file(&s0, &case.ops, d)
        .unwrap_or_else(|e| panic!("{} on {}: {e:?}", case.name, d.name()));
    (s0, units, s1)
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/migrate_v2_s3")
}

// ---------------------------------------------------- pure (no pool) tests

/// Criteria 1 and 5: every golden, rendered from dialect values alone. No
/// pool or runtime exists in this test, so no catalog can be read.
#[test]
fn goldens_render_without_a_pool() {
    let bless = std::env::var_os("MV2_BLESS").is_some();
    let names: Vec<&str> = cases(DialectKind::Postgres)
        .iter()
        .map(|c| c.name)
        .collect();
    let mut failed = Vec::new();
    for name in names {
        let mut text = String::new();
        for k in ALL {
            let all = cases(k);
            let case = all.iter().find(|c| c.name == name).unwrap();
            let (_, units, _) = render_case(case, dialect(k));
            text.push_str(&format!("-- {}\n{}", dname(k), golden(&units)));
        }
        let path = golden_dir().join(format!("{name}.sql"));
        if bless {
            std::fs::create_dir_all(golden_dir()).unwrap();
            std::fs::write(&path, &text).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing golden {}; run with MV2_BLESS=1", path.display()));
        if want != text {
            failed.push(format!("{name}:\n--- want\n{want}--- got\n{text}"));
        }
    }
    assert!(failed.is_empty(), "goldens differ:\n{}", failed.join("\n"));
}

/// Criterion 4: one statement per op on MySQL, which is one ALTER where the op alters.
/// The exceptions are named, each with the MySQL error that forces it.
#[test]
fn mysql_renders_one_statement_per_op() {
    let mut multi = Vec::new();
    for case in cases(DialectKind::Mysql) {
        let (_, units, _) = render_case(&case, &MySql);
        for i in 0..case.ops.len() {
            let stmts: Vec<&String> = units
                .iter()
                .filter(|u| u.op == i)
                .flat_map(|u| &u.sql)
                .collect();
            assert!(
                units
                    .iter()
                    .filter(|u| u.op == i)
                    .all(|u| u.kind == UnitKind::Autocommit && u.sql.len() == 1),
                "{}: op {i} has a multi-statement MySQL unit",
                case.name
            );
            if stmts.len() > 1 {
                multi.push(format!("{}#{i}", case.name));
            }
        }
    }
    // add_column + fill: ADD and ALTER DROP DEFAULT in one ALTER is 1054.
    // alter_fk: DROP FOREIGN KEY and ADD of one name in one ALTER is 1826.
    assert_eq!(multi, ["add_column#2", "alter_fk#0"]);
}

/// Criterion 6: the default goes out as written. v1 would turn MySQL's
/// `now()` into `(UTC_TIMESTAMP(6))` and wrap `'en'` on LONGTEXT in parens.
#[test]
fn defaults_render_verbatim() {
    let s0 = render_file(&State::default(), &[create(parent(), vec![])], &MySql)
        .unwrap()
        .1;
    let ops = [
        Op::AddColumn {
            table: "s3_parent".into(),
            column: c("at", "datetime").default("now()"),
            fill: None,
        },
        Op::AddColumn {
            table: "s3_parent".into(),
            column: c("bio", "string").default("'en'"),
            fill: None,
        },
    ];
    let (units, _) = render_file(&s0, &ops, &MySql).unwrap();
    assert_eq!(
        golden(&units),
        "[op 0] autocommit\n  ALTER TABLE `s3_parent` ADD COLUMN `at` DATETIME(6) DEFAULT now();\n\
         [op 1] autocommit\n  ALTER TABLE `s3_parent` ADD COLUMN `bio` LONGTEXT DEFAULT 'en';\n"
    );
    // Every 2.6 default text appears verbatim in its backend's CREATE TABLE.
    for k in ALL {
        let all = cases(k);
        let case = all
            .iter()
            .find(|c| c.name == "defaults_as_written")
            .unwrap();
        let (_, units, _) = render_case(case, dialect(k));
        let sql = &units[0].sql[0];
        for col in defaults_cols(k) {
            let d = col.default.unwrap();
            assert!(
                sql.contains(&format!(" DEFAULT {d}")),
                "{k:?}: `{d}` not verbatim in {sql}"
            );
        }
    }
}

fn expect_err(s0: &State, ops: &[Op], d: &dyn Dialect, code: &str) -> String {
    match render_file(s0, ops, d) {
        Ok((u, _)) => panic!("{}: expected {code}, rendered {}", d.name(), golden(&u)),
        Err((_, e)) => {
            assert_eq!(e.code, code, "{}: {e}", d.name());
            e.msg
        }
    }
}

fn state_of(setup: &[Op]) -> State {
    render_file(&State::default(), setup, &Postgres).unwrap().1
}

/// 3780 on every dialect: the op list is shared, so the rule is a state rule.
#[test]
fn retype_under_fk_needs_drop_fk() {
    let s0 = state_of(&[
        create(table("s3_t", vec![c("id", "i32").pk()]), vec![]),
        create(
            {
                let mut t = table("s3_child", vec![c("id", "i64").pk(), c("pid", "i32")]);
                t.fks = vec![fk("s3_child_pid_fk", &["pid"], "s3_t", &["id"], None)];
                t
            },
            vec![ix("s3_child_pid_idx", "s3_child", &["pid"], false)],
        ),
    ]);
    for k in ALL {
        let parent = [Op::AlterColumn {
            table: "s3_t".into(),
            from: c("id", "i32").pk(),
            to: c("id", "i64").pk(),
        }];
        let msg = expect_err(&s0, &parent, dialect(k), E_FK_TYPE);
        assert!(
            msg.contains("drop_fk") && msg.contains("s3_child_pid_fk"),
            "{msg}"
        );
        let child = [Op::AlterColumn {
            table: "s3_child".into(),
            from: c("pid", "i32"),
            to: c("pid", "i64"),
        }];
        expect_err(&s0, &child, dialect(k), E_FK_TYPE);
        // add_fk across two types is refused too (3780 on ADD).
        let mut s1 = s0.clone();
        apply(
            &mut s1,
            &Op::DropFk {
                table: "s3_child".into(),
                was: fk("s3_child_pid_fk", &["pid"], "s3_t", &["id"], None),
            },
        )
        .unwrap();
        apply(&mut s1, &child[0]).unwrap();
        let readd = [Op::AddFk {
            table: "s3_child".into(),
            fk: fk("s3_child_pid_fk", &["pid"], "s3_t", &["id"], None),
        }];
        expect_err(&s1, &readd, dialect(k), E_FK_TYPE);
    }
}

/// 3959 on every dialect: PG and SQLite rewrite or drop the CHECK silently,
/// which would leave state wrong; MySQL refuses.
#[test]
fn rename_or_drop_under_check_needs_drop_check() {
    let mut t = s3_t(vec![c("a", "i32"), c("b", "i32")]);
    t.checks = vec![check("s3_t_ab", "b > a")];
    let s0 = state_of(&[create(t, vec![])]);
    for k in ALL {
        let rename = [Op::RenameColumn {
            table: "s3_t".into(),
            from: "a".into(),
            to: "z".into(),
        }];
        let msg = expect_err(&s0, &rename, dialect(k), E_CHECK_COL);
        assert!(msg.contains("drop_check"), "{msg}");
        let drop = [Op::DropColumn {
            table: "s3_t".into(),
            was: c("a", "i32"),
        }];
        expect_err(&s0, &drop, dialect(k), E_CHECK_COL);
    }
}

#[test]
fn refusals_name_the_fix() {
    let s0 = state_of(&[
        create(parent(), vec![]),
        create(child(vec![child_fk(None)]), vec![child_idx()]),
    ]);
    for k in ALL {
        let d = dialect(k);
        // NOT NULL, no default, existing table: `fill` (the empty-table probe is gone).
        expect_err(
            &s0,
            &[Op::AddColumn {
                table: "s3_parent".into(),
                column: c("x", "i32").not_null(),
                fill: None,
            }],
            d,
            E_FILL,
        );
        // DROP TABLE without CASCADE needs the inbound FK gone first.
        let msg = expect_err(&s0, &[Op::DropTable { was: parent() }], d, E_STATE);
        assert!(msg.contains("drop_fk"), "{msg}");
        // `was` must equal state.
        expect_err(
            &s0,
            &[Op::DropColumn {
                table: "s3_parent".into(),
                was: c("name", "string").max(99),
            }],
            d,
            E_STATE,
        );
        // primary_key / auto changes stay a hard error.
        expect_err(
            &s0,
            &[Op::AlterColumn {
                table: "s3_parent".into(),
                from: c("id", "i64").pk().auto(),
                to: c("id", "i64").pk(),
            }],
            d,
            E_UNSUPPORTED,
        );
        expect_err(
            &s0,
            &[Op::RawSql {
                up: all_sql("SET search_path TO other"),
                down: PerDialect::default(),
                state: vec![],
                autocommit: false,
            }],
            d,
            E_RAW,
        );
    }
    // MySQL only: what it cannot build without `skipped_on`, and FKs without an index.
    let mut part = ix("s3_t_p", "s3_child", &["parent_id"], false);
    part.decl.predicate = Some("parent_id > 0".into());
    let partial = [Op::AddIndex {
        index: part,
        concurrently: None,
    }];
    expect_err(&s0, &partial, &MySql, E_DIALECT);
    render_file(&s0, &partial, &Postgres).unwrap();
    let ext = [Op::CreateExtension {
        name: "citext".into(),
        schema: None,
        skipped_on: vec![],
    }];
    expect_err(&s0, &ext, &MySql, E_DIALECT);
    expect_err(&s0, &ext, &Sqlite, E_DIALECT);
    let no_index = state_of(&[create(parent(), vec![]), create(child(vec![]), vec![])]);
    let add = [Op::AddFk {
        table: "s3_child".into(),
        fk: child_fk(None),
    }];
    expect_err(&no_index, &add, &MySql, E_FK_INDEX);
    render_file(&no_index, &add, &Postgres).unwrap();
    // Dropping the index an FK needs: MySQL 1553.
    let drop_ix = [Op::DropIndex {
        was: child_idx(),
        concurrently: None,
    }];
    expect_err(&s0, &drop_ix, &MySql, E_FK_INDEX);
}

/// Criterion 3: `alter_column` on PG is v1's `push_alter_changes` plus the v1
/// arms, so the non-castable default is dropped before the retype.
#[test]
fn pg_alter_column_reuses_v1_order() {
    let all = cases(DialectKind::Postgres);
    let case = all
        .iter()
        .find(|c| c.name == "alter_column_type_noncastable_default")
        .unwrap();
    let (_, units, _) = render_case(case, &Postgres);
    let sql = &units[0].sql;
    let pos = |needle: &str| sql.iter().position(|s| s.contains(needle)).unwrap();
    assert!(pos("DROP DEFAULT") < pos("TYPE BIGINT"));
    assert!(pos("TYPE BIGINT") < pos("SET DEFAULT 0"));
}

/// Why MySQL does not reuse the v1 arms: they restate the column once per change.
#[test]
fn v1_mysql_alter_restates_per_change() {
    use rustango::migrate::diff::{render_changes_split_with_dialect, SchemaChange};
    use rustango::migrate::snapshot::SchemaSnapshot;
    let mut snap = SchemaSnapshot::default();
    let t: rustango::migrate::snapshot::TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "t", "model": "", "fields": [
            {"name": "x", "column": "x", "ty": "i64", "nullable": false, "primary_key": false}
        ]
    }))
    .unwrap();
    snap.tables.push(t);
    let changes = [
        SchemaChange::AlterColumnType {
            table: "t".into(),
            column: "x".into(),
            from: "i32".into(),
            to: "i64".into(),
        },
        SchemaChange::AlterColumnNullable {
            table: "t".into(),
            column: "x".into(),
            nullable: false,
        },
    ];
    let b = render_changes_split_with_dialect(&changes, &snap, &MySql).unwrap();
    assert_eq!(
        b.immediate
            .iter()
            .filter(|s| s.contains("MODIFY COLUMN"))
            .count(),
        2
    );
}

// ------------------------------------------------------------ live tests

/// What the catalog holds for `after`'s tables: each table and index probes
/// Present, nothing extra (a hidden MySQL FK index, a leftover PG check),
/// and tables gone from state are gone from the catalog.
async fn verify(pool: &Pool, name: &str, before: &State, after: &State) {
    let k = pool.dialect().kind();
    for (t, ts) in &after.tables {
        let v = probe(pool, &SpikeOp::CreateTable(ts.decl.clone()))
            .await
            .unwrap()
            .verdict;
        assert_eq!(v, Verdict::Present, "{name} {k:?}: table {t}");
        let wanted: Vec<&IndexState> = ts
            .indexes
            .iter()
            .filter(|i| !i.skipped_on.contains(&k))
            .collect();
        for i in &wanted {
            let v = probe(pool, &SpikeOp::AddIndex(i.decl.clone()))
                .await
                .unwrap()
                .verdict;
            assert_eq!(v, Verdict::Present, "{name} {k:?}: index {}", i.decl.name);
        }
        let live = read_table(pool, t).await.unwrap().unwrap();
        let extra: Vec<&str> = live
            .indexes
            .iter()
            .filter(|i| !i.primary && !i.name.starts_with("sqlite_autoindex"))
            .filter(|i| !wanted.iter().any(|w| w.decl.name == i.name))
            .map(|i| i.name.as_str())
            .collect();
        assert!(
            extra.is_empty(),
            "{name} {k:?}: {t} has extra indexes {extra:?}"
        );
        let checks: Vec<_> = live
            .checks
            .iter()
            .filter(|c| {
                !ts.decl
                    .checks
                    .iter()
                    .any(|d| Some(&d.name) == c.name.as_ref())
            })
            .collect();
        assert!(
            checks.is_empty(),
            "{name} {k:?}: {t} has extra checks {checks:?}"
        );
        assert_eq!(
            live.fks.len(),
            ts.decl.fks.len(),
            "{name} {k:?}: {t} FKs {:?}",
            live.fks
        );
    }
    for t in before
        .tables
        .keys()
        .filter(|t| !after.tables.contains_key(*t))
    {
        assert!(
            read_table(pool, t).await.unwrap().is_none(),
            "{name} {k:?}: {t} still exists"
        );
    }
}

async fn exec_raw(pool: &Pool, sql: &str) -> Result<u64, String> {
    raw_execute_pool(pool, sql, Vec::new())
        .await
        .map_err(|e| e.to_string())
}

/// Criterion 7: every golden applies on its backend and the catalog then
/// matches the after-state.
async fn every_golden_applies_and_matches_state(pool: &Pool) {
    let d = pool.dialect();
    let k = d.kind();
    for case in cases(k) {
        clean(pool).await;
        let (setup, s0) = setup_state(&case, d);
        run_units(pool, &setup)
            .await
            .unwrap_or_else(|e| panic!("{} setup: {e}", case.name));
        for s in &case.seed {
            exec_raw(pool, s)
                .await
                .unwrap_or_else(|e| panic!("{} seed: {e}", case.name));
        }
        let (units, s1) = render_file(&s0, &case.ops, d).unwrap();
        run_units(pool, &units)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", case.name));
        verify(pool, case.name, &s0, &s1).await;
        match case.name {
            "add_column" => {
                // The fill reached the old row; the default it used is gone (probe above).
                let int = if k == DialectKind::Mysql {
                    "SIGNED"
                } else {
                    "BIGINT"
                };
                let sql = format!("SELECT CAST(score AS {int}) FROM s3_parent");
                let got: Vec<(i64,)> = raw_query_pool(&sql, vec![], pool).await.unwrap();
                assert_eq!(got, vec![(0,)]);
            }
            "alter_column_type_noncastable_default" => {
                let got: Vec<(i64,)> = raw_query_pool("SELECT code FROM s3_t", vec![], pool)
                    .await
                    .unwrap();
                assert_eq!(got, vec![(42,)]);
            }
            "rename_table" if k == DialectKind::Postgres => {
                let sql = "SELECT pg_get_serial_sequence('s3_renamed', 'id'), \
                           pg_get_serial_sequence('s3_parent', 'id'), \
                           (SELECT conname::text FROM pg_constraint WHERE conrelid = 's3_parent'::regclass AND contype = 'p')";
                let got: Vec<(String, String, String)> =
                    raw_query_pool(sql, vec![], pool).await.unwrap();
                assert_eq!(
                    got,
                    vec![(
                        "public.s3_renamed_id_seq".into(),
                        "public.s3_parent_id_seq".into(),
                        "s3_parent_pkey".into()
                    )]
                );
            }
            "create_extension" if k == DialectKind::Postgres => {
                let got: Vec<(String,)> = raw_query_pool(
                    "SELECT extname::text FROM pg_extension WHERE extname = 'pg_trgm'",
                    vec![],
                    pool,
                )
                .await
                .unwrap();
                assert_eq!(got.len(), 1);
            }
            _ => {}
        }
    }
    clean(pool).await;
}

/// The premise of the 3780 rule: MySQL refuses the bare retype under the FK.
async fn mysql_refuses_retype_under_fk(pool: &Pool) {
    let all = cases(pool.dialect().kind());
    let case = all.iter().find(|c| c.name == "fk_retype").unwrap();
    let (setup, _) = setup_state(case, pool.dialect());
    run_units(pool, &setup).await.unwrap();
    let r = exec_raw(pool, "ALTER TABLE s3_child MODIFY COLUMN pid BIGINT").await;
    let r2 = exec_raw(pool, "ALTER TABLE s3_t MODIFY COLUMN id BIGINT NOT NULL").await;
    if pool.dialect().kind() == DialectKind::Mysql {
        for r in [r, r2] {
            let e = r.expect_err("MySQL must refuse");
            assert!(e.contains("3780") || e.contains("incompatible"), "{e}");
        }
    }
    clean(pool).await;
}

/// The premise of the 3959 rule: MySQL refuses the bare rename, and drops a
/// one-column CHECK with its column without a word (state would keep it).
async fn mysql_check_blocks_rename(pool: &Pool) {
    let all = cases(pool.dialect().kind());
    let case = all
        .iter()
        .find(|c| c.name == "rename_column_under_check")
        .unwrap();
    let (setup, _) = setup_state(case, pool.dialect());
    run_units(pool, &setup).await.unwrap();
    let r = exec_raw(pool, "ALTER TABLE s3_t RENAME COLUMN a TO b").await;
    if pool.dialect().kind() == DialectKind::Mysql {
        let e = r.expect_err("MySQL must refuse");
        assert!(
            e.contains("3959") || e.contains("cannot be dropped or renamed"),
            "{e}"
        );
        exec_raw(pool, "ALTER TABLE s3_t DROP COLUMN a")
            .await
            .unwrap();
        let live = read_table(pool, "s3_t").await.unwrap().unwrap();
        assert!(live.checks.is_empty(), "{:?}", live.checks);
    }
    clean(pool).await;
}

/// The premise of the PG default handling v1 does: the retype alone fails.
async fn pg_retype_fails_over_old_default(pool: &Pool) {
    let all = cases(pool.dialect().kind());
    let case = all
        .iter()
        .find(|c| c.name == "alter_column_type_noncastable_default")
        .unwrap();
    let (setup, _) = setup_state(case, pool.dialect());
    run_units(pool, &setup).await.unwrap();
    if pool.dialect().kind() == DialectKind::Postgres {
        let e = exec_raw(
            pool,
            r#"ALTER TABLE "s3_t" ALTER COLUMN "code" TYPE BIGINT USING "code"::BIGINT"#,
        )
        .await
        .expect_err("PG must refuse");
        assert!(e.contains("cannot be cast automatically"), "{e}");
    }
    clean(pool).await;
}

/// The premise of the MySQL FK-index rule: an FK with no index gets a hidden
/// one, and DROP FOREIGN KEY leaves it.
async fn mysql_fk_leaves_hidden_index(pool: &Pool) {
    let d = pool.dialect();
    if d.kind() != DialectKind::Mysql {
        return;
    }
    let (setup, _) = render_file(
        &State::default(),
        &[create(parent(), vec![]), create(child(vec![]), vec![])],
        d,
    )
    .unwrap();
    run_units(pool, &setup).await.unwrap();
    exec_raw(pool, "ALTER TABLE s3_child ADD CONSTRAINT s3_child_parent_fk FOREIGN KEY (parent_id) REFERENCES s3_parent (id)").await.unwrap();
    exec_raw(
        pool,
        "ALTER TABLE s3_child DROP FOREIGN KEY s3_child_parent_fk",
    )
    .await
    .unwrap();
    let live = read_table(pool, "s3_child").await.unwrap().unwrap();
    assert!(
        live.indexes.iter().any(|i| i.name == "s3_child_parent_fk"),
        "{:?}",
        live.indexes
    );
    clean(pool).await;
}

tri_dialect_test!(
    setup: clean,
    scenarios: [
        every_golden_applies_and_matches_state,
        mysql_refuses_retype_under_fk,
        mysql_check_blocks_rename,
        pg_retype_fails_over_old_default,
        mysql_fk_leaves_hidden_index,
    ]
);
