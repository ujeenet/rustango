//! Schema state per target: what replay builds and checkpoints store.
//!
//! Tables sit behind `Arc`, so a state clone copies pointers and an op copies
//! only the table it changes. Only database-relevant fields are kept.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::checksum;

fn is_false(b: &bool) -> bool {
    !*b
}

/// Which DAG and database a file runs against.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Target {
    /// Once, on the registry database.
    Registry,
    /// Every tenant (or the single database).
    #[default]
    Tenant,
}

impl Target {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Tenant => "tenant",
        }
    }
}

/// A backend, as files name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Backend {
    Postgres,
    Mysql,
    Sqlite,
}

impl Backend {
    pub(crate) const ALL: [Self; 3] = [Self::Postgres, Self::Mysql, Self::Sqlite];

    /// The backend of a dialect.
    pub(crate) const fn of(kind: crate::sql::DialectKind) -> Self {
        match kind {
            crate::sql::DialectKind::Postgres => Self::Postgres,
            crate::sql::DialectKind::Mysql => Self::Mysql,
            crate::sql::DialectKind::Sqlite => Self::Sqlite,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }
}

/// One column, as stored in state (FKs live on the table).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ColumnDef {
    pub(crate) column: String,
    /// The type token (`i64`, `string`, `datetime`, ...).
    pub(crate) ty: String,
    pub(crate) nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_length: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) primary_key: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) auto: bool,
    /// Raw SQL text after DEFAULT, as written; never translated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) generated_as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) comment: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) case_insensitive: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) sealed: bool,
}

/// An index; a unique constraint is a unique index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexDef {
    pub(crate) name: String,
    pub(crate) columns: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) unique: bool,
    #[serde(default = "btree", skip_serializing_if = "is_btree")]
    pub(crate) method: String,
    /// Partial-index predicate.
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub(crate) predicate: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skipped_on: Vec<Backend>,
    /// A UNIQUE constraint, not a plain unique index: PG drops it with
    /// `DROP CONSTRAINT`, SQLite keeps it in the table's DDL.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) constraint: bool,
}

impl IndexDef {
    /// Equality and hashes compare this form: `skipped_on` sorted.
    pub(crate) fn normalised(&self) -> Self {
        let mut ix = self.clone();
        ix.skipped_on.sort_unstable();
        ix
    }
}

/// `a` and `b` name the same columns, in any order.
pub(crate) fn same_column_set<A: AsRef<str>, B: AsRef<str>>(a: &[A], b: &[B]) -> bool {
    let mut a: Vec<&str> = a.iter().map(AsRef::as_ref).collect();
    let mut b: Vec<&str> = b.iter().map(AsRef::as_ref).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

fn btree() -> String {
    "btree".to_owned()
}

#[allow(clippy::ptr_arg)]
fn is_btree(m: &String) -> bool {
    m == "btree"
}

/// A CHECK: `{name, expr}` with SQL as written, or `{name, column, min,
/// max}`, a range each backend renders with its own quoting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CheckDef {
    pub(crate) name: String,
    pub(crate) body: CheckBody,
}

/// What a CHECK tests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CheckBody {
    /// SQL text, run as written on every backend.
    Sql(String),
    /// `column >= min AND column <= max`, at least one bound.
    Range {
        column: String,
        min: Option<i64>,
        max: Option<i64>,
    },
}

impl CheckBody {
    /// The SQL inside `CHECK (…)` on `d`.
    pub(crate) fn sql(&self, d: &dyn crate::sql::Dialect) -> String {
        match self {
            Self::Sql(s) => s.clone(),
            Self::Range { column, min, max } => {
                let q = d.quote_ident(column);
                let mut parts = Vec::new();
                if let Some(min) = min {
                    parts.push(format!("{q} >= {min}"));
                }
                if let Some(max) = max {
                    parts.push(format!("{q} <= {max}"));
                }
                parts.join(" AND ")
            }
        }
    }

    /// Whether the check reads `col`.
    pub(crate) fn mentions(&self, col: &str) -> bool {
        match self {
            Self::Sql(s) => s
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|tok| tok.eq_ignore_ascii_case(col)),
            Self::Range { column, .. } => column == col,
        }
    }
}

/// The flat file form of a check body: `expr`, or `column` with bounds.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCheckBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max: Option<i64>,
}

impl RawCheckBody {
    pub(crate) fn of(b: &CheckBody) -> Self {
        match b {
            CheckBody::Sql(s) => Self {
                expr: Some(s.clone()),
                ..Self::default()
            },
            CheckBody::Range { column, min, max } => Self {
                column: Some(column.clone()),
                min: *min,
                max: *max,
                ..Self::default()
            },
        }
    }

    /// # Errors
    /// Both forms, neither, or a range with no bound.
    pub(crate) fn body(self) -> Result<CheckBody, String> {
        match (self.expr, self.column) {
            (Some(e), None) if self.min.is_none() && self.max.is_none() => Ok(CheckBody::Sql(e)),
            (None, Some(column)) if self.min.is_some() || self.max.is_some() => {
                Ok(CheckBody::Range {
                    column,
                    min: self.min,
                    max: self.max,
                })
            }
            _ => Err("a check is `expr`, or `column` with `min` and/or `max`".to_owned()),
        }
    }
}

impl Serialize for CheckBody {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        RawCheckBody::of(self).serialize(s)
    }
}

impl<'de> Deserialize<'de> for CheckBody {
    /// `was` of `drop_check`: the SQL string, or the range object.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Was {
            Sql(String),
            Range(RawCheckBody),
        }
        match Was::deserialize(d)? {
            Was::Sql(s) => Ok(Self::Sql(s)),
            Was::Range(r) if r.expr.is_none() => r.body().map_err(serde::de::Error::custom),
            Was::Range(_) => Err(serde::de::Error::custom(
                "`was` is the SQL string or a range",
            )),
        }
    }
}

/// `{name, ...RawCheckBody}`, spelled out: `flatten` drops `deny_unknown_fields`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<i64>,
}

impl Serialize for CheckDef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let b = RawCheckBody::of(&self.body);
        RawCheck {
            name: self.name.clone(),
            expr: b.expr,
            column: b.column,
            min: b.min,
            max: b.max,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for CheckDef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawCheck::deserialize(d)?;
        let body = RawCheckBody {
            expr: raw.expr,
            column: raw.column,
            min: raw.min,
            max: raw.max,
        };
        Ok(Self {
            name: raw.name,
            body: body.body().map_err(serde::de::Error::custom)?,
        })
    }
}

/// FK delete action. RESTRICT and NO ACTION stay apart: PG treats them
/// differently for deferred checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnDelete {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl OnDelete {
    /// The SQL words, `NO ACTION` included.
    pub(crate) const fn sql(self) -> &'static str {
        match self {
            Self::NoAction => "NO ACTION",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_no_action(o: &OnDelete) -> bool {
    *o == OnDelete::NoAction
}

/// An FK as stored in state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FkDef {
    pub(crate) name: String,
    pub(crate) columns: Vec<String>,
    pub(crate) to: String,
    pub(crate) on: Vec<String>,
    #[serde(default, skip_serializing_if = "is_no_action")]
    pub(crate) on_delete: OnDelete,
    /// The target column type, for FKs into framework tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) to_ty: Option<String>,
}

/// One owned table.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableState {
    pub(crate) name: String,
    pub(crate) scope: Target,
    pub(crate) columns: Vec<ColumnDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) indexes: Vec<IndexDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) checks: Vec<CheckDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) fks: Vec<FkDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) comment: Option<String>,
    /// PG primary key constraint name, as PG picks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pg_pkey: Option<String>,
    /// PG sequence owned by the auto primary key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pg_sequence: Option<String>,
}

impl TableState {
    /// Every collection sorted by key: equality and hashes use this form.
    /// Column order is kept in the live state (it is the CREATE order).
    pub(crate) fn normalised(&self) -> Self {
        let mut t = self.clone();
        t.columns.sort_by(|a, b| a.column.cmp(&b.column));
        t.indexes = t.indexes.iter().map(IndexDef::normalised).collect();
        t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        t.checks.sort_by(|a, b| a.name.cmp(&b.name));
        t.fks.sort_by(|a, b| a.name.cmp(&b.name));
        t
    }

    pub(crate) fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.column == name)
    }

    /// Primary key columns, in table order.
    pub(crate) fn primary_key(&self) -> impl Iterator<Item = &str> {
        self.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.column.as_str())
    }
}

/// A column of a table this chain does not own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternColumn {
    pub(crate) name: String,
    pub(crate) ty: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) unique: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ExternOwner {
    /// The framework's system chain.
    System,
    /// A `keep_unmanaged` model.
    Unmanaged,
}

/// A table in the same database that this chain does not own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternTable {
    pub(crate) table: String,
    pub(crate) columns: Vec<ExternColumn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) unique_sets: Vec<Vec<String>>,
    pub(crate) owner: ExternOwner,
}

impl ExternTable {
    /// Equality and hashes compare this form: columns, and each unique set,
    /// sorted.
    pub(crate) fn normalised(&self) -> Self {
        let mut e = self.clone();
        e.columns.sort_by(|a, b| a.name.cmp(&b.name));
        for u in &mut e.unique_sets {
            u.sort();
        }
        e.unique_sets.sort();
        e
    }
}

/// One target's schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) tables: BTreeMap<Arc<str>, Arc<TableState>>,
    pub(crate) externs: BTreeMap<Arc<str>, Arc<ExternTable>>,
    pub(crate) extensions: BTreeSet<String>,
}

/// What a checkpoint file stores: tables in normal form, keyed by name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateDoc {
    #[serde(default)]
    pub(crate) tables: BTreeMap<String, TableState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) externs: BTreeMap<String, ExternTable>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) extensions: BTreeSet<String>,
}

impl State {
    /// The normal form: what equality and hashes compare.
    pub(crate) fn normalised(&self) -> StateDoc {
        StateDoc {
            tables: self
                .tables
                .iter()
                .map(|(k, t)| (k.to_string(), t.normalised()))
                .collect(),
            externs: self
                .externs
                .iter()
                .map(|(k, e)| (k.to_string(), e.normalised()))
                .collect(),
            extensions: self.extensions.clone(),
        }
    }

    pub(crate) fn from_doc(doc: StateDoc) -> Self {
        Self {
            tables: doc
                .tables
                .into_iter()
                .map(|(k, t)| (Arc::from(k), Arc::new(t)))
                .collect(),
            externs: doc
                .externs
                .into_iter()
                .map(|(k, e)| (Arc::from(k), Arc::new(e)))
                .collect(),
            extensions: doc.extensions,
        }
    }

    /// The state hash, recomputing no table `memo` has seen.
    pub(crate) fn hash(&self, memo: &mut HashMemo) -> String {
        let mut h = Sha256::new();
        for (name, t) in &self.tables {
            h.update(b"t\0");
            h.update(name.as_bytes());
            h.update(b"\0");
            h.update(memo.table(t));
        }
        for (name, e) in &self.externs {
            h.update(b"e\0");
            h.update(name.as_bytes());
            h.update(b"\0");
            h.update(digest(&e.normalised()));
        }
        for x in &self.extensions {
            h.update(b"x\0");
            h.update(x.as_bytes());
            h.update(b"\0");
        }
        checksum::hex(&h.finalize())
    }
}

fn digest<T: Serialize>(v: &T) -> [u8; 32] {
    let value = serde_json::to_value(v).expect("state serialises");
    let text = checksum::canonical(&value).expect("state holds integers only");
    Sha256::digest(text.as_bytes()).into()
}

/// Per-table hashes keyed by `Arc` identity: an op re-hashes only the table
/// it copied. The memo holds each `Arc`, so an address is never reused.
#[derive(Default)]
pub(crate) struct HashMemo(HashMap<usize, (Arc<TableState>, [u8; 32])>);

impl HashMemo {
    fn table(&mut self, t: &Arc<TableState>) -> [u8; 32] {
        let key = Arc::as_ptr(t) as usize;
        self.0
            .entry(key)
            .or_insert_with(|| (Arc::clone(t), digest(&t.normalised())))
            .1
    }
}

/// PG's name for a constraint or sequence: `<name1>_<name2>_<label>`, each
/// part cut to fit 63 bytes, then a counter on the label until free
/// (PG `makeObjectName` / `ChooseRelationName`).
pub(crate) fn pg_object_name(
    name1: &str,
    name2: Option<&str>,
    label: &str,
    taken: impl Fn(&str) -> bool,
) -> String {
    let mut pass = 0u32;
    loop {
        let label = if pass == 0 {
            label.to_owned()
        } else {
            format!("{label}{pass}")
        };
        let name = pg_make_object_name(name1, name2, &label);
        if !taken(&name) {
            return name;
        }
        pass += 1;
    }
}

fn pg_make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    let overhead = label.len() + 1 + usize::from(name2.is_some());
    let avail = super::MAX_IDENT - overhead;
    let mut n1 = name1.len();
    let mut n2 = name2.map_or(0, str::len);
    // Shorten the longer part one byte at a time, as PG does.
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let mut out = clip(name1, n1).to_owned();
    if let Some(n) = name2 {
        out.push('_');
        out.push_str(clip(n, n2));
    }
    out.push('_');
    out.push_str(label);
    out
}

/// The longest prefix of `s` of at most `n` bytes on a char boundary.
fn clip(s: &str, mut n: usize) -> &str {
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_names_match_pg() {
        let free = |_: &str| false;
        assert_eq!(
            pg_object_name("invoice", None, "pkey", free),
            "invoice_pkey"
        );
        assert_eq!(
            pg_object_name("invoice", Some("id"), "seq", free),
            "invoice_id_seq"
        );
        let taken = |n: &str| n == "invoice_pkey";
        assert_eq!(
            pg_object_name("invoice", None, "pkey", taken),
            "invoice_pkey1"
        );
        // 60-byte table: the table part is cut, the label kept.
        let long = "t".repeat(60);
        let name = pg_object_name(&long, None, "pkey", free);
        assert_eq!(name.len(), 63);
        assert!(name.ends_with("_pkey"), "{name}");
        // Two long parts: the longer one shrinks first.
        let seq = pg_object_name(&"a".repeat(50), Some(&"b".repeat(30)), "seq", free);
        assert_eq!(seq.len(), 63);
        assert_eq!(seq, format!("{}_{}_seq", "a".repeat(29), "b".repeat(29)));
    }

    fn table(on_delete: OnDelete) -> TableState {
        TableState {
            name: "t".into(),
            scope: Target::Tenant,
            columns: vec![],
            indexes: vec![],
            checks: vec![],
            fks: vec![FkDef {
                name: "t_fk".into(),
                columns: vec!["p".into()],
                to: "p".into(),
                on: vec!["id".into()],
                on_delete,
                to_ty: None,
            }],
            comment: None,
            pg_pkey: None,
            pg_sequence: None,
        }
    }

    /// RESTRICT and NO ACTION are different states with different hashes.
    #[test]
    fn restrict_and_no_action_stay_apart() {
        let a = table(OnDelete::Restrict);
        let b = table(OnDelete::NoAction);
        assert_ne!(a.normalised(), b.normalised());
        let state = |t: TableState| State {
            tables: [(Arc::from("t"), Arc::new(t))].into(),
            ..State::default()
        };
        let mut memo = HashMemo::default();
        assert_ne!(state(a).hash(&mut memo), state(b).hash(&mut memo));
    }

    /// The memo keeps each `Arc` alive: a freed address cannot alias a new table.
    #[test]
    fn the_hash_memo_never_reuses_an_address() {
        let mut memo = HashMemo::default();
        let mut seen = BTreeSet::new();
        for i in 0..200 {
            let mut t = table(OnDelete::Cascade);
            t.name = format!("t{i}");
            let st = State {
                tables: [(Arc::from("t"), Arc::new(t))].into(),
                ..State::default()
            };
            assert!(seen.insert(st.hash(&mut memo)), "hash {i} reused");
        }
    }

    /// A checkpoint stores the normal form; loading it gives the same hash.
    #[test]
    fn the_normal_form_round_trips() {
        let mut t = table(OnDelete::Restrict);
        t.columns = vec![ColumnDef {
            column: "b".into(),
            ty: "i64".into(),
            nullable: false,
            max_length: None,
            primary_key: false,
            auto: false,
            default: Some("0".into()),
            generated_as: None,
            comment: None,
            case_insensitive: false,
            sealed: false,
        }];
        let st = State {
            tables: [(Arc::from("t"), Arc::new(t))].into(),
            ..State::default()
        };
        let doc = st.normalised();
        let text = serde_json::to_string(&doc).unwrap();
        let back = State::from_doc(serde_json::from_str(&text).unwrap());
        let mut memo = HashMemo::default();
        assert_eq!(back.hash(&mut memo), st.hash(&mut memo));
        assert!(text.contains("\"restrict\""), "{text}");
    }

    /// Extern tables hash by their normal form too.
    #[test]
    fn extern_tables_are_normalised() {
        let col = |n: &str| ExternColumn {
            name: n.into(),
            ty: "i64".into(),
            unique: false,
        };
        let ext = |cols: Vec<ExternColumn>, sets: Vec<Vec<String>>| State {
            externs: [(
                Arc::from("e"),
                Arc::new(ExternTable {
                    table: "e".into(),
                    columns: cols,
                    unique_sets: sets,
                    owner: ExternOwner::System,
                }),
            )]
            .into(),
            ..State::default()
        };
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let a = ext(vec![col("a"), col("b")], vec![s(&["a", "b"]), s(&["b"])]);
        let b = ext(vec![col("b"), col("a")], vec![s(&["b"]), s(&["b", "a"])]);
        let mut memo = HashMemo::default();
        assert_eq!(a.hash(&mut memo), b.hash(&mut memo));
        assert_eq!(a.normalised(), b.normalised());
    }

    /// Order of columns, indexes and the rest does not change the hash.
    #[test]
    fn hashes_compare_normal_forms() {
        let mut a = table(OnDelete::Cascade);
        a.checks = vec![
            CheckDef {
                name: "b".into(),
                body: CheckBody::Sql("x".into()),
            },
            CheckDef {
                name: "a".into(),
                body: CheckBody::Sql("y".into()),
            },
        ];
        let mut b = a.clone();
        b.checks.reverse();
        assert_eq!(a.normalised(), b.normalised());
        let mut memo = HashMemo::default();
        let st = |t: TableState| State {
            tables: [(Arc::from("t"), Arc::new(t))].into(),
            ..State::default()
        };
        assert_eq!(st(a).hash(&mut memo), st(b).hash(&mut memo));
    }
}
