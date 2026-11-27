//! The v2 file format: the top-level fields, the op envelope and the ops.
//!
//! Decoding is two-step: `engine`, `format` and each op's `v` are read off
//! the raw value first, so a file from a newer release says "upgrade"
//! instead of "unknown key".

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::state::{
    Backend, CheckBody, CheckDef, ColumnDef, ExternColumn, ExternOwner, FkDef, IndexDef, OnDelete,
    StateDoc, Target,
};
use super::{Code, LoadError};

/// The highest file `format` this release reads.
pub(crate) const FORMAT: u64 = 1;
/// The highest op `v` this release reads (the same for every op today).
pub(crate) const OP_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    #[default]
    Migration,
    Checkpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    #[default]
    Expand,
    Contract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FileTx {
    #[default]
    Auto,
    /// Each op its own unit.
    None,
}

/// Where the adopt checkpoint came from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Base {
    /// The last engine v1 migration the state was taken from.
    pub(crate) legacy: String,
    pub(crate) state_sha256: String,
}

/// A warning silenced, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Allow {
    pub(crate) code: String,
    pub(crate) reason: String,
}

/// The top-level fields, as written (operations decoded separately).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    #[allow(dead_code)]
    engine: u64,
    #[allow(dead_code)]
    format: u64,
    #[serde(default)]
    kind: Kind,
    id: String,
    app: String,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    requires: Vec<String>,
    target: Target,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    base: Option<Base>,
    #[serde(default)]
    phase: Phase,
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    tx: FileTx,
    #[serde(default)]
    replaces: Vec<String>,
    #[serde(default)]
    state: Option<StateDoc>,
    #[serde(default)]
    irreversible_below: Option<String>,
    #[serde(default)]
    allow: Vec<Allow>,
    #[serde(default)]
    #[allow(dead_code)]
    created_at: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    operations: Vec<Value>,
}

/// A decoded v2 file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct File {
    pub(crate) kind: Kind,
    pub(crate) id: String,
    pub(crate) app: String,
    pub(crate) parents: Vec<String>,
    pub(crate) depends_on: Vec<(String, String)>,
    pub(crate) requires: Vec<String>,
    pub(crate) target: Target,
    /// The configured database a `database` file runs on.
    pub(crate) database: Option<String>,
    pub(crate) base: Option<Base>,
    pub(crate) phase: Phase,
    pub(crate) offline: bool,
    pub(crate) tx: FileTx,
    pub(crate) state: Option<StateDoc>,
    /// A squash checkpoint's replaced ids, in its app (9.6).
    pub(crate) replaces: Vec<String>,
    /// A squash checkpoint over an op with no inverse: the id below which
    /// nothing rolls back.
    pub(crate) irreversible_below: Option<String>,
    pub(crate) allow: Vec<Allow>,
    pub(crate) operations: Vec<OpEnvelope>,
}

/// Per-op unit choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OpTx {
    Own,
    Autocommit,
}

/// One op with its envelope fields.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpEnvelope {
    pub(crate) op: Op,
    pub(crate) tx: Option<OpTx>,
    pub(crate) allow: Vec<Allow>,
}

/// SQL text: one string, or lines joined with `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sql(pub(crate) String);

impl<'de> Deserialize<'de> for Sql {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Lines(Vec<String>),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::One(s) => Self(s),
            Raw::Lines(l) => Self(l.join("\n")),
        })
    }
}

/// SQL per backend; `null` = nothing runs there, `any` = every backend.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PerBackend {
    #[serde(default, deserialize_with = "present")]
    pub(crate) any: Option<Option<Sql>>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) postgres: Option<Option<Sql>>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) mysql: Option<Option<Sql>>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) sqlite: Option<Option<Sql>>,
}

/// Tells a written `null` (`Some(None)`) from an absent key (`None`).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<Sql>>, D::Error> {
    Option::<Sql>::deserialize(d).map(Some)
}

impl PerBackend {
    /// What runs on `b`: `None` when nothing does.
    pub(crate) fn get(&self, b: Backend) -> Option<&Sql> {
        let own = match b {
            Backend::Postgres => &self.postgres,
            Backend::Mysql => &self.mysql,
            Backend::Sqlite => &self.sqlite,
        };
        match own {
            Some(sql) => sql.as_ref(),
            None => self.any.as_ref().and_then(Option::as_ref),
        }
    }

    /// The SQL text each backend runs.
    pub(crate) fn texts(&self) -> impl Iterator<Item = &str> {
        Backend::ALL
            .into_iter()
            .filter_map(|b| self.get(b))
            .map(|s| s.0.as_str())
    }
}

/// A column in `create_table.fields` or `add_column`: the state column plus
/// an optional inline FK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) column: ColumnDef,
    pub(crate) fk: Option<InlineFk>,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = Map::deserialize(d)?;
        let fk = match map.remove("fk") {
            Some(v) => Some(InlineFk::deserialize(v).map_err(serde::de::Error::custom)?),
            None => None,
        };
        let column =
            ColumnDef::deserialize(Value::Object(map)).map_err(serde::de::Error::custom)?;
        Ok(Self { column, fk })
    }
}

/// An FK written next to its column.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InlineFk {
    pub(crate) name: String,
    pub(crate) to: String,
    pub(crate) on: String,
    #[serde(default)]
    pub(crate) on_delete: OnDelete,
    #[serde(default)]
    pub(crate) to_ty: Option<String>,
}

impl InlineFk {
    pub(crate) fn state(&self, column: &str) -> FkDef {
        FkDef {
            name: self.name.clone(),
            columns: vec![column.to_owned()],
            to: self.to.clone(),
            on: vec![self.on.clone()],
            on_delete: self.on_delete,
            to_ty: self.to_ty.clone(),
        }
    }
}

/// `create_table.table` and `drop_table.was`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableDef {
    pub(crate) name: String,
    pub(crate) scope: Target,
    pub(crate) fields: Vec<Field>,
    #[serde(default)]
    pub(crate) comment: Option<String>,
}

/// `{from, to}` of one `alter_column` change.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change<T> {
    pub(crate) from: T,
    pub(crate) to: T,
}

/// The changes one `alter_column` makes; absent = unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ColumnChanges {
    pub(crate) ty: Option<Change<String>>,
    pub(crate) nullable: Option<Change<bool>>,
    pub(crate) default: Option<Change<Option<String>>>,
    pub(crate) portable_default: Option<Change<Option<super::state::PortableDefault>>>,
    pub(crate) max_length: Option<Change<Option<u32>>>,
    pub(crate) comment: Option<Change<Option<String>>>,
    pub(crate) case_insensitive: Option<Change<bool>>,
    pub(crate) primary_key: Option<Change<bool>>,
    pub(crate) auto: Option<Change<bool>>,
    pub(crate) generated_as: Option<Change<Option<String>>>,
    pub(crate) sealed: Option<Change<bool>>,
}

/// What a destructive op can restore on rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Restore {
    #[default]
    ShapeOnly,
    Data,
    /// The forward op copies key and value into an archive table; the
    /// rollback refills the column from it (7.5).
    Archive,
}

/// A raw_sql probe after an autocommit unit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum Probe {
    /// `"state"`: probe the declared state.
    Named(ProbeKind),
    /// `{"idempotent": true}` or `{"sql": ...}`.
    Spec(ProbeSpec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProbeKind {
    State,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeSpec {
    #[serde(default)]
    pub(crate) idempotent: bool,
    #[serde(default)]
    pub(crate) sql: Option<PerBackend>,
}

/// A data op's footprint: tables it reads, and `table` or `table.column`
/// it writes (a write is a read too).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Footprint {
    #[serde(default)]
    pub(crate) read: Vec<String>,
    #[serde(default)]
    pub(crate) write: Vec<String>,
}

impl Footprint {
    /// The table of each write entry.
    pub(crate) fn write_tables(&self) -> impl Iterator<Item = &str> {
        self.write
            .iter()
            .map(|w| w.split_once('.').map_or(w.as_str(), |(t, _)| t))
    }

    /// Every table the op may name.
    pub(crate) fn tables(&self) -> impl Iterator<Item = &str> {
        self.read
            .iter()
            .map(String::as_str)
            .chain(self.write_tables())
    }
}

/// One `{{?}}` value: its column type and JSON value (`null` allowed).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bind {
    pub(crate) ty: String,
    pub(crate) value: Value,
}

/// `down` of a data op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DataDown {
    /// SQL per backend, run as the reverse data unit.
    Sql(PerBackend),
    /// Nothing to undo; verified at load (7.5).
    Noop(String),
    /// Rollback stops here.
    Irreversible(String),
}

impl<'de> Deserialize<'de> for DataDown {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let reason = |k: &str| -> Option<Result<String, D::Error>> {
            let o = v.as_object()?;
            let r = o.get(k)?;
            Some(if o.len() == 1 {
                r.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| serde::de::Error::custom(format!("`{k}` is a reason string")))
            } else {
                Err(serde::de::Error::custom(format!("`{k}` stands alone")))
            })
        };
        if let Some(r) = reason("noop") {
            return r.map(Self::Noop);
        }
        if let Some(r) = reason("irreversible") {
            return r.map(Self::Irreversible);
        }
        PerBackend::deserialize(v)
            .map(Self::Sql)
            .map_err(serde::de::Error::custom)
    }
}

/// `down` of a `data_rust` op: the step's own `down`, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RustDown {
    Rust,
    Noop(String),
    Irreversible(String),
}

impl<'de> Deserialize<'de> for RustDown {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        if v == "rust" {
            return Ok(Self::Rust);
        }
        match DataDown::deserialize(v).map_err(serde::de::Error::custom)? {
            DataDown::Noop(r) => Ok(Self::Noop(r)),
            DataDown::Irreversible(r) => Ok(Self::Irreversible(r)),
            DataDown::Sql(_) => Err(serde::de::Error::custom(
                "`down` of data_rust is \"rust\", {\"noop\": ...} or {\"irreversible\": ...}",
            )),
        }
    }
}

/// A batched data op's range: the key it walks, rows per batch, and the
/// knobs that change no outcome (hashed as absent).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchSpec {
    pub(crate) table: String,
    /// One column: a composite key is refused at load.
    pub(crate) key: BatchKey,
    pub(crate) size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sleep_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_batches_per_run: Option<u64>,
}

/// The key column of a batch; a list (composite key) does not decode.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub(crate) struct BatchKey(pub(crate) String);

impl<'de> Deserialize<'de> for BatchKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) => Ok(Self(s)),
            Value::Array(_) => Err(serde::de::Error::custom(
                "a composite batch key is refused: batch on one unique, ordered column",
            )),
            _ => Err(serde::de::Error::custom("`key` is a column name")),
        }
    }
}

/// Whether a data op runs on a fresh install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnFresh {
    #[default]
    Run,
    Skip,
}

/// The ops this release runs. Each op renders and replays only itself.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Op {
    CreateTable {
        table: TableDef,
        #[serde(default)]
        indexes: Vec<IndexDef>,
        #[serde(default)]
        checks: Vec<CheckDef>,
    },
    DropTable {
        table: String,
        /// The table as replay holds it: columns, indexes, checks, FKs.
        was: super::state::TableState,
        #[serde(default)]
        restore: Restore,
    },
    RenameTable {
        from: String,
        to: String,
    },
    AlterTableComment {
        table: String,
        from: Option<String>,
        to: Option<String>,
    },
    AdoptTable {
        table: TableDef,
        #[serde(default)]
        indexes: Vec<IndexDef>,
        #[serde(default)]
        checks: Vec<CheckDef>,
    },
    AddColumn {
        table: String,
        column: Field,
        #[serde(default)]
        fill: Option<String>,
    },
    /// Take over an existing column (design item 22): verified if present,
    /// added if not; rollback forgets it, never drops.
    AdoptColumn {
        table: String,
        column: Field,
        /// Indexes on the column, taken over the same way (design item 23).
        #[serde(default)]
        indexes: Vec<IndexDef>,
    },
    DropColumn {
        table: String,
        column: String,
        was: Field,
        #[serde(default)]
        restore: Restore,
    },
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },
    AlterColumn {
        table: String,
        column: String,
        changes: ColumnChanges,
        #[serde(default)]
        using: Option<PerBackend>,
        #[serde(default)]
        using_down: Option<PerBackend>,
    },
    AddFk {
        table: String,
        column: String,
        fk: InlineFk,
    },
    DropFk {
        table: String,
        column: String,
        was: InlineFk,
    },
    /// Flat in the file (`table`, `concurrently` and the index keys);
    /// decoded by hand in `decode_op`.
    #[serde(skip_deserializing)]
    AddIndex {
        table: String,
        index: IndexDef,
        concurrently: Option<bool>,
    },
    DropIndex {
        table: String,
        name: String,
        was: IndexDef,
        #[serde(default)]
        concurrently: Option<bool>,
    },
    RenameIndex {
        table: String,
        from: String,
        to: String,
    },
    /// Flat in the file (`table` and the check keys); decoded by hand.
    #[serde(skip_deserializing)]
    AddCheck {
        table: String,
        check: CheckDef,
    },
    DropCheck {
        table: String,
        name: String,
        was: CheckBody,
    },
    RawSql {
        id: String,
        up: PerBackend,
        #[serde(default)]
        down: Option<PerBackend>,
        state: Vec<Value>,
        #[serde(default)]
        probe: Option<Probe>,
    },
    StateOnly {
        ops: Vec<Value>,
        reason: String,
    },
    CreateExtension {
        name: String,
        #[serde(default)]
        schema: Option<String>,
        #[serde(default)]
        skipped_on: Vec<Backend>,
    },
    ExternTable {
        table: String,
        columns: Vec<ExternColumn>,
        #[serde(default)]
        unique_sets: Vec<Vec<String>>,
        owner: ExternOwner,
    },
    /// SQL data step (5.1), not batched in this release.
    DataSql {
        id: String,
        tables: Footprint,
        up: PerBackend,
        #[serde(default)]
        binds: Vec<Bind>,
        down: DataDown,
        #[serde(default)]
        down_binds: Vec<Bind>,
        #[serde(default)]
        on_fresh: OnFresh,
        #[serde(default)]
        restores: Vec<String>,
        #[serde(default)]
        batch: Option<BatchSpec>,
    },
    /// A Rust data step (5.2), run by its `#[data_step]` registration.
    DataRust {
        id: String,
        tables: Footprint,
        down: RustDown,
        #[serde(default)]
        on_fresh: OnFresh,
        #[serde(default)]
        restores: Vec<String>,
        #[serde(default)]
        batch: Option<BatchSpec>,
        /// Capture undo rows (`ctx.undo()`): rollback restores from them,
        /// whatever `down` says (7.4).
        #[serde(default)]
        undo: bool,
        /// The inverse runs the step's `down`; never in a file.
        #[serde(skip_deserializing)]
        reverse: bool,
    },
    /// A rollback's refill of a re-added column from its archive; never in
    /// a file. `not_null`: the column goes back to NOT NULL after.
    #[serde(skip_deserializing)]
    ArchiveRestore {
        table: String,
        column: String,
        key: String,
        archive: String,
        not_null: bool,
    },
    /// An `adopt_table` whose table is missing, created as it declares;
    /// never in a file. It keeps the adopted table's FK rules.
    #[serde(skip_deserializing)]
    CreateAdopted {
        table: TableDef,
        indexes: Vec<IndexDef>,
        checks: Vec<CheckDef>,
    },
    /// A rollback's drop of a consumed archive; never in a file.
    #[serde(skip_deserializing)]
    ArchiveDrop {
        archive: String,
    },
}

/// Ops the format defines that this release does not run yet: each gets
/// goldens on three backends before it is enabled.
const NOT_ENABLED: [&str; 9] = [
    "alter_check",
    "rename_constraint",
    "add_exclusion",
    "drop_exclusion",
    "add_composite_fk",
    "drop_composite_fk",
    "create_m2m",
    "drop_m2m",
    "verify",
];

/// Envelope keys every op may carry; never part of the payload.
const ENVELOPE: [&str; 4] = ["v", "tx", "label", "allow"];

impl Op {
    /// Nested state ops of `raw_sql` and `state_only`, decoded.
    pub(crate) fn nested(&self, at: &str) -> Result<Vec<Op>, LoadError> {
        let (key, raw) = match self {
            Self::RawSql { state, .. } => ("state", state),
            Self::StateOnly { ops, .. } => ("ops", ops),
            _ => return Ok(Vec::new()),
        };
        raw.iter()
            .enumerate()
            .map(|(i, v)| {
                let env = decode_op(v, &format!("{at}/{key}/{i}"))?;
                if matches!(env.op, Self::RawSql { .. } | Self::StateOnly { .. }) {
                    return Err(LoadError::new(
                        Code::File,
                        format!("{at}/{key}/{i}"),
                        "nested ops may only be schema ops",
                    ));
                }
                Ok(env.op)
            })
            .collect()
    }
}

/// Decode a file's raw value.
///
/// # Errors
/// [`Code::File`] for a bad shape, [`Code::Newer`] for a newer `format` or
/// op `v`, [`Code::NotEnabled`] for an op this release does not run.
pub(crate) fn decode(raw: &Value, at: &str) -> Result<File, LoadError> {
    let Some(obj) = raw.as_object() else {
        return Err(LoadError::new(Code::File, at, "a v2 file is a JSON object"));
    };
    match obj.get("engine").and_then(Value::as_u64) {
        Some(2) => {}
        _ => {
            return Err(LoadError::new(
                Code::File,
                format!("{at}#/engine"),
                "`engine` must be 2",
            ))
        }
    }
    match obj.get("format").and_then(Value::as_u64) {
        Some(n) if n > FORMAT => {
            let v = env!("CARGO_PKG_VERSION");
            let msg = format!("written in file format {n}; rustango {v} reads up to {FORMAT}");
            return Err(LoadError::new(
                Code::Newer,
                format!("{at}#/format"),
                format!("{msg}: upgrade rustango"),
            ));
        }
        Some(n) if n >= 1 => {}
        _ => {
            return Err(LoadError::new(
                Code::File,
                format!("{at}#/format"),
                "`format` must be a positive integer",
            ))
        }
    }
    let header: Header = typed(raw.clone(), at)?;
    let operations = header
        .operations
        .iter()
        .enumerate()
        .map(|(i, v)| decode_op(v, &format!("{at}#/operations/{i}")))
        .collect::<Result<Vec<_>, _>>()?;
    let depends_on = header
        .depends_on
        .iter()
        .enumerate()
        .map(|(i, d)| match d.split_once(':') {
            Some((app, id)) if !app.is_empty() && !id.is_empty() => {
                Ok((app.to_owned(), id.to_owned()))
            }
            _ => Err(LoadError::new(
                Code::File,
                format!("{at}#/depends_on/{i}"),
                format!("`{d}` is not `<app>:<id>`"),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let file = File {
        kind: header.kind,
        id: header.id,
        app: header.app,
        parents: header.parents,
        depends_on,
        requires: header.requires,
        target: header.target,
        database: header.database,
        base: header.base,
        phase: header.phase,
        offline: header.offline,
        tx: header.tx,
        state: header.state,
        replaces: header.replaces,
        irreversible_below: header.irreversible_below,
        allow: header.allow,
        operations,
    };
    validate(&file, at)?;
    Ok(file)
}

fn typed<T: DeserializeOwned>(v: Value, at: &str) -> Result<T, LoadError> {
    serde_json::from_value(v).map_err(|e| LoadError::new(Code::File, at, e.to_string()))
}

/// One op: `v` checked before the payload, envelope keys taken off.
fn decode_op(v: &Value, at: &str) -> Result<OpEnvelope, LoadError> {
    let Some(obj) = v.as_object() else {
        return Err(LoadError::new(Code::File, at, "an op is a JSON object"));
    };
    let Some(kind) = obj.get("op").and_then(Value::as_str) else {
        return Err(LoadError::new(Code::File, at, "an op needs `op`"));
    };
    match obj.get("v").and_then(Value::as_u64) {
        Some(n) if n > OP_VERSION => {
            return Err(LoadError::new(
                Code::Newer,
                format!("{at}/v"),
                format!(
                "`{kind}` v{n} is newer than this release reads (v{OP_VERSION}): upgrade rustango"
            ),
            ))
        }
        Some(n) if n >= 1 => {}
        _ => {
            return Err(LoadError::new(
                Code::File,
                format!("{at}/v"),
                format!("`{kind}` needs `v`, a positive integer"),
            ))
        }
    }
    if NOT_ENABLED.contains(&kind) {
        return Err(LoadError::new(
            Code::NotEnabled,
            at,
            format!(
                "op `{kind}` is not enabled in rustango {}",
                env!("CARGO_PKG_VERSION")
            ),
        ));
    }
    if kind == "data_sql" || kind == "data_rust" {
        if obj.contains_key("verify") {
            return Err(LoadError::new(
                Code::NotEnabled,
                format!("{at}/verify"),
                "`verify` on a data op comes in a later release",
            ));
        }
        if kind == "data_sql" && obj.contains_key("undo") {
            return Err(LoadError::new(
                Code::File,
                format!("{at}/undo"),
                "undo capture needs a data_rust step (`ctx.undo()`)",
            ));
        }
        if obj.get("tx").and_then(Value::as_str) == Some("autocommit") {
            return Err(LoadError::new(
                Code::File,
                format!("{at}/tx"),
                "a data op always runs in a transaction",
            ));
        }
    }
    let mut payload: Map<String, Value> = obj.clone();
    let tx = payload.remove("tx");
    let allow = payload.remove("allow");
    for k in ENVELOPE {
        payload.remove(k);
    }
    let bad = |e: serde_json::Error| {
        let mut msg = format!("`{kind}`: {e}");
        if msg.contains("unknown field `min`") || msg.contains("unknown field `max`") {
            msg.push_str("; `min`/`max` are checks: write an add_check op");
        }
        LoadError::new(Code::File, at, msg)
    };
    let op = if kind == "add_index" {
        payload.remove("op");
        let table = payload.remove("table");
        let concurrently = payload.remove("concurrently");
        Op::AddIndex {
            table: serde_json::from_value(table.unwrap_or(Value::Null)).map_err(bad)?,
            concurrently: serde_json::from_value(concurrently.unwrap_or(Value::Null))
                .map_err(bad)?,
            index: serde_json::from_value(Value::Object(payload)).map_err(bad)?,
        }
    } else if kind == "add_check" {
        payload.remove("op");
        let table = payload.remove("table");
        Op::AddCheck {
            table: serde_json::from_value(table.unwrap_or(Value::Null)).map_err(bad)?,
            check: serde_json::from_value(Value::Object(payload)).map_err(bad)?,
        }
    } else {
        serde_json::from_value(Value::Object(payload)).map_err(bad)?
    };
    Ok(OpEnvelope {
        op,
        tx: tx.map(|t| typed(t, &format!("{at}/tx"))).transpose()?,
        allow: allow
            .map(|a| typed(a, &format!("{at}/allow")))
            .transpose()?
            .unwrap_or_default(),
    })
}

/// Rules of section 2 that serde cannot state.
fn validate(file: &File, at: &str) -> Result<(), LoadError> {
    let (replaces, irreversible_below) = (&file.replaces, file.irreversible_below.as_ref());
    verified_noops(file, at)?;
    if file.target == Target::Tenant {
        tenant_sql_stays_home(file, at)?;
    }
    let checkpoint = file.kind == Kind::Checkpoint;
    if !checkpoint && (file.state.is_some() || file.base.is_some()) {
        return Err(LoadError::new(
            Code::Checkpoint,
            at,
            "`state` and `base` belong on a checkpoint only",
        ));
    }
    if !checkpoint && (!replaces.is_empty() || irreversible_below.is_some()) {
        return Err(LoadError::new(
            Code::Checkpoint,
            at,
            "`replaces` and `irreversible_below` belong on a checkpoint only",
        ));
    }
    if checkpoint && !replaces.is_empty() {
        // A squash checkpoint: its state, the replaced ids, and only the
        // data ops a fresh install runs (9.6).
        if file.base.is_some() || file.state.is_none() {
            return Err(LoadError::new(
                Code::Checkpoint,
                at,
                "a squash checkpoint needs `state` and `replaces`, and no `base`",
            ));
        }
        if !file.parents.is_empty() {
            return Err(LoadError::new(
                Code::Checkpoint,
                at,
                "a squash checkpoint replaces its app's chain from the root: no parents",
            ));
        }
        if let Some(i) = file.operations.iter().position(|e| {
            !matches!(
                e.op,
                Op::DataSql {
                    on_fresh: OnFresh::Run,
                    ..
                } | Op::DataRust {
                    on_fresh: OnFresh::Run,
                    ..
                }
            )
        }) {
            return Err(LoadError::new(
                Code::Checkpoint,
                format!("{at}#/operations/{i}"),
                "a squash checkpoint keeps only data ops with `on_fresh: \"run\"`",
            ));
        }
    } else if checkpoint {
        if irreversible_below.is_some() {
            return Err(LoadError::new(
                Code::Checkpoint,
                at,
                "`irreversible_below` belongs on a squash checkpoint",
            ));
        }
        if file.base.is_none() || file.state.is_none() {
            return Err(LoadError::new(
                Code::Checkpoint,
                at,
                "an adopt checkpoint needs `base` and `state`",
            ));
        }
        if !file.operations.is_empty() || !file.parents.is_empty() {
            return Err(LoadError::new(
                Code::Checkpoint,
                at,
                "an adopt checkpoint has no parents and no operations",
            ));
        }
    }
    match (file.target, file.database.as_deref()) {
        (Target::Database, Some(db)) if !db.is_empty() => {}
        (Target::Database, _) => {
            return Err(LoadError::new(
                Code::File,
                at,
                "a `database` file names the configured database in `database`",
            ))
        }
        (_, Some(_)) => {
            return Err(LoadError::new(
                Code::File,
                format!("{at}#/database"),
                "`database` belongs on a file with `target: \"database\"`",
            ))
        }
        _ => {}
    }
    let prefix = match file.target {
        Target::Tenant => "registry:",
        Target::Registry => "tenants:",
        Target::EveryDatabase | Target::Database => {
            if !file.requires.is_empty() {
                return Err(LoadError::new(
                    Code::File,
                    format!("{at}#/requires"),
                    format!("a {} file has no `requires`", file.target.as_str()),
                ));
            }
            ""
        }
    };
    for (i, r) in file.requires.iter().enumerate() {
        if !r.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty()) {
            return Err(LoadError::new(
                Code::File,
                format!("{at}#/requires/{i}"),
                format!(
                    "a {} file requires `{prefix}<app>:<id>`, not `{r}`",
                    file.target.as_str()
                ),
            ));
        }
    }
    for (i, env) in file.operations.iter().enumerate() {
        let op_at = format!("{at}#/operations/{i}");
        if let Op::RawSql {
            up, state, probe, ..
        } = &env.op
        {
            if env.tx == Some(OpTx::Autocommit) && probe.is_none() {
                return Err(LoadError::new(
                    Code::File,
                    op_at,
                    "`tx: \"autocommit\"` needs `probe`",
                ));
            }
            let nested = env.op.nested(&op_at)?;
            // Nothing runs on `b`: a `null`, a missing key, or `"any": null`.
            for b in Backend::ALL {
                if up.get(b).is_none() && !state.is_empty() {
                    if let Some(op) = nested.iter().find(|o| !skips(o, b)) {
                        return Err(LoadError::new(
                            Code::Dialect,
                            op_at,
                            format!(
                                "no SQL for {} but its state op `{}` is not `skipped_on` it",
                                b.as_str(),
                                op_name(op)
                            ),
                        ));
                    }
                }
            }
        }
        if let Op::CreateTable { table, .. } | Op::AdoptTable { table, .. } = &env.op {
            if table.scope != file.target {
                return Err(LoadError::new(
                    Code::File,
                    op_at,
                    format!(
                        "table `{}` has scope {} in a {} file",
                        table.name,
                        table.scope.as_str(),
                        file.target.as_str()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// A `noop` down passes when the reverse of an earlier op of this file
/// drops everything the step writes; otherwise it needs `allow` (7.5).
/// Tenant SQL never names another schema (8.2): `public` anywhere, or a
/// qualified table after FROM/JOIN/UPDATE/INTO/TABLE other than
/// `extensions`. Positional, so the search path stays the real guard.
fn tenant_sql_stays_home(file: &File, at: &str) -> Result<(), LoadError> {
    use super::norm::Tok;
    let mut texts: Vec<(usize, &str)> = Vec::new();
    for (i, env) in file.operations.iter().enumerate() {
        match &env.op {
            Op::DataSql { up, down, .. } => {
                texts.extend(up.texts().map(|t| (i, t)));
                if let DataDown::Sql(d) = down {
                    texts.extend(d.texts().map(|t| (i, t)));
                }
            }
            Op::RawSql { up, down, .. } => {
                texts.extend(up.texts().map(|t| (i, t)));
                if let Some(d) = down {
                    texts.extend(d.texts().map(|t| (i, t)));
                }
            }
            Op::AddCheck { check, .. } => {
                if let CheckBody::Sql(t) = &check.body {
                    texts.push((i, t));
                }
            }
            _ => {}
        }
    }
    for (i, text) in texts {
        // Templates are not SQL yet: each becomes a plain name.
        let mut plain = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(a) = rest.find("{{") {
            plain.push_str(&rest[..a]);
            plain.push_str(" x ");
            rest = rest[a..].find("}}").map_or("", |b| &rest[a + b + 2..]);
        }
        plain.push_str(rest);
        // Comments are not SQL: `-- see public.org` names nothing, and an
        // apostrophe in one must not hide the statement after it.
        let (plain, _) = super::render::strip_comments(&plain, false);
        let Ok(toks) = super::norm::tokenize(crate::sql::DialectKind::Postgres, &plain) else {
            continue;
        };
        let ident = |k: usize| match toks.get(k).map(|t| &t.tok) {
            Some(Tok::Ident(w)) => Some(w.as_str()),
            _ => None,
        };
        let dot = |k: usize| matches!(toks.get(k).map(|t| &t.tok), Some(Tok::Op(o)) if o == ".");
        for k in 0..toks.len() {
            let Some(q) = ident(k) else { continue };
            if !dot(k + 1) || ident(k + 2).is_none() {
                continue;
            }
            let after_table_kw = k > 0
                && ident(k - 1)
                    .is_some_and(|w| ["from", "join", "update", "into", "table"].contains(&w));
            if q == "public" || (after_table_kw && q != "extensions") {
                return Err(LoadError::new(
                    Code::OtherSchema,
                    format!("{at}#/operations/{i}"),
                    format!(
                        "tenant SQL names schema `{q}`: a tenant file reaches only its own \
                         schema (and `extensions`); registry tables change through the registry \
                         chain"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn verified_noops(file: &File, at: &str) -> Result<(), LoadError> {
    let mut tables: Vec<&str> = Vec::new();
    let mut columns: Vec<(&str, &str)> = Vec::new();
    for (i, env) in file.operations.iter().enumerate() {
        match &env.op {
            Op::CreateTable { table, .. } => tables.push(&table.name),
            Op::AddColumn { table, column, .. } => columns.push((table, &column.column.column)),
            Op::DataSql {
                tables: fp,
                down: DataDown::Noop(_),
                ..
            }
            | Op::DataRust {
                tables: fp,
                down: RustDown::Noop(_),
                undo: false,
                ..
            } => {
                let dropped = |w: &String| match w.split_once('.') {
                    Some((t, c)) => tables.contains(&t) || columns.contains(&(t, c)),
                    None => tables.contains(&w.as_str()),
                };
                let allowed = env.allow.iter().any(|a| a.code == Code::Noop.as_str());
                if let Some(w) = fp.write.iter().find(|w| !dropped(w)) {
                    if !allowed {
                        return Err(LoadError::new(
                            Code::Noop,
                            format!("{at}#/operations/{i}/down"),
                            format!(
                                "`noop` down, but `{w}` is not created earlier in this file, so \
                                 rollback leaves the written data; write a down, or allow {} \
                                 with a reason",
                                Code::Noop.as_str()
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn skips(op: &Op, b: Backend) -> bool {
    match op {
        Op::AddIndex { index, .. } => index.skipped_on.contains(&b),
        Op::CreateExtension { skipped_on, .. } => skipped_on.contains(&b),
        _ => false,
    }
}

/// The `op` key of `op`, for messages.
pub(crate) fn op_name(op: &Op) -> &'static str {
    match op {
        Op::CreateTable { .. } => "create_table",
        Op::DropTable { .. } => "drop_table",
        Op::RenameTable { .. } => "rename_table",
        Op::AlterTableComment { .. } => "alter_table_comment",
        Op::AdoptTable { .. } => "adopt_table",
        Op::CreateAdopted { .. } => "adopt_table",
        Op::AddColumn { .. } => "add_column",
        Op::AdoptColumn { .. } => "adopt_column",
        Op::DropColumn { .. } => "drop_column",
        Op::RenameColumn { .. } => "rename_column",
        Op::AlterColumn { .. } => "alter_column",
        Op::AddFk { .. } => "add_fk",
        Op::DropFk { .. } => "drop_fk",
        Op::AddIndex { .. } => "add_index",
        Op::DropIndex { .. } => "drop_index",
        Op::RenameIndex { .. } => "rename_index",
        Op::AddCheck { .. } => "add_check",
        Op::DropCheck { .. } => "drop_check",
        Op::RawSql { .. } => "raw_sql",
        Op::StateOnly { .. } => "state_only",
        Op::CreateExtension { .. } => "create_extension",
        Op::ExternTable { .. } => "extern_table",
        Op::DataSql { .. } => "data_sql",
        Op::DataRust { .. } => "data_rust",
        Op::ArchiveRestore { .. } => "archive_restore",
        Op::ArchiveDrop { .. } => "archive_drop",
    }
}

/// The archive table of `table.column` (7.5): `__rustango_archive_<16 hex>`.
pub(crate) fn archive_name(table: &str, column: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = super::checksum::hex(&Sha256::digest(format!("{table}.{column}").as_bytes()));
    format!("__rustango_archive_{}", &h[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(ops: Value) -> Value {
        json!({"engine": 2, "format": 1, "id": "0001_a", "app": "project",
               "target": "tenant", "operations": ops})
    }

    fn code(v: &Value) -> Code {
        decode(v, "f.json").unwrap_err().code
    }

    #[test]
    fn example_a_decodes() {
        let v = file(json!([
            {"op": "add_column", "v": 1, "table": "user", "label": "x",
             "column": {"column": "locale", "ty": "string", "max_length": 8, "nullable": true}},
            {"op": "alter_column", "v": 1, "table": "user", "column": "locale",
             "changes": {"nullable": {"from": true, "to": false}}}
        ]));
        let f = decode(&v, "f.json").unwrap();
        assert_eq!(f.operations.len(), 2);
        assert!(matches!(f.operations[1].op, Op::AlterColumn { .. }));
    }

    /// Tenant SQL naming another schema is a load error (8.2); `extensions`
    /// and an alias qualifier pass, and registry files may name `public`.
    #[test]
    fn tenant_sql_naming_another_schema_is_refused() {
        let sql = |text: &str, target: &str| {
            let mut f = file(json!([{"op": "data_sql", "v": 1, "id": "m_v1",
                "tables": {"write": ["t.c"]},
                "up": {"any": text}, "down": {"noop": "x"},
                "allow": [{"code": "MV2-E028", "reason": "x"}]}]));
            f["target"] = json!(target);
            decode(&f, "f.json")
        };
        for bad in [
            "UPDATE public.org SET c = 1",
            "UPDATE {{t:t}} SET {{c:c}} = (SELECT 1 FROM registry.org)",
            "INSERT INTO other.t (c) VALUES (1)",
        ] {
            assert_eq!(
                sql(bad, "tenant").unwrap_err().code,
                Code::OtherSchema,
                "{bad}"
            );
        }
        for ok in [
            "UPDATE {{t:t}} SET {{c:c}} = extensions.f(1)",
            "UPDATE {{t:t}} AS a SET {{c:c}} = a.c + 1",
        ] {
            sql(ok, "tenant").unwrap();
        }
        sql("UPDATE public.org SET c = 1", "registry").unwrap();
        // Tokens, not text: strings, comments and quoted names hold no qualifier.
        for ok in [
            "UPDATE {{t:t}} SET {{c:c}} = 'see public.org'",
            "-- copied from public.org\nUPDATE {{t:t}} SET {{c:c}} = 1",
            "/* FROM other.t */ UPDATE {{t:t}} SET {{c:c}} = 1",
            "UPDATE {{t:t}} SET {{c:c}} = \"public.org\".x",
        ] {
            sql(ok, "tenant").unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        // A comment's apostrophe does not hide the statement after it.
        assert_eq!(
            sql("-- don't\nUPDATE public.org SET c = 1", "tenant")
                .unwrap_err()
                .code,
            Code::OtherSchema
        );
    }

    #[test]
    fn a_composite_batch_key_is_refused_at_load() {
        let op = |key: Value| {
            file(json!([{"op": "data_sql", "v": 1, "id": "m_v1",
                "tables": {"read": ["t"], "write": ["t.c"]},
                "up": {"any": "UPDATE {{t:t}} SET {{c:c}} = 1 WHERE {{batch_range}}"},
                "batch": {"table": "t", "key": key, "size": 10},
                "down": {"noop": "x"}, "allow": [{"code": "MV2-E028", "reason": "x"}]}]))
        };
        let e = decode(&op(json!(["a", "b"])), "f.json").unwrap_err();
        assert!(e.to_string().contains("composite batch key"), "{e}");
        decode(&op(json!("a")), "f.json").unwrap();
    }

    #[test]
    fn newer_format_and_op_version_say_upgrade() {
        let mut v = file(json!([]));
        v["format"] = json!(2);
        v["new_key"] = json!(true);
        let e = decode(&v, "f.json").unwrap_err();
        assert_eq!(e.code, Code::Newer);
        assert!(e.msg.contains("upgrade rustango"), "{e}");
        let v = file(json!([{"op": "add_check", "v": 2, "table": "t"}]));
        assert_eq!(code(&v), Code::Newer);
    }

    #[test]
    fn unknown_keys_and_ops_are_refused() {
        let mut v = file(json!([]));
        v["colour"] = json!("red");
        assert_eq!(code(&v), Code::File);
        let v = file(
            json!([{"op": "add_check", "v": 1, "table": "t", "name": "c",
                             "expr": "x", "extra": 1}]),
        );
        assert_eq!(code(&v), Code::File);
        let v = file(json!([{"op": "teleport", "v": 1}]));
        assert_eq!(code(&v), Code::File);
        let v = file(json!([{"op": "verify", "v": 1}]));
        assert_eq!(code(&v), Code::NotEnabled);
        let v = file(json!([{"op": "add_check", "table": "t", "name": "c", "expr": "x"}]));
        assert_eq!(code(&v), Code::File, "missing `v`");
    }

    #[test]
    fn engine_and_target_are_required() {
        let mut v = file(json!([]));
        v["engine"] = json!(1);
        assert_eq!(code(&v), Code::File);
        let mut v = file(json!([]));
        v.as_object_mut().unwrap().remove("target");
        assert_eq!(code(&v), Code::File);
        let mut v = file(json!([]));
        v["target"] = json!("everywhere");
        assert_eq!(code(&v), Code::File);
    }

    #[test]
    fn requires_and_depends_on_have_a_shape() {
        let mut v = file(json!([]));
        v["requires"] = json!(["tenants:x:0001_a"]);
        assert_eq!(code(&v), Code::File, "a tenant file requires registry:");
        v["requires"] = json!(["registry:0001_a"]);
        assert!(decode(&v, "f.json").is_ok());
        let mut v = file(json!([]));
        v["depends_on"] = json!(["0001_a"]);
        assert_eq!(code(&v), Code::File);
    }

    #[test]
    fn checkpoint_fields_belong_on_checkpoints() {
        let mut v = file(json!([]));
        v["state"] = json!({"tables": {}});
        assert_eq!(code(&v), Code::Checkpoint);
        let mut v = file(json!([]));
        v["kind"] = json!("checkpoint");
        assert_eq!(code(&v), Code::Checkpoint, "no base");
        v["base"] = json!({"legacy": "0010_x", "state_sha256": "00"});
        v["state"] = json!({"tables": {}});
        assert!(decode(&v, "f.json").is_ok());
        // A squash checkpoint has no `base`, and keeps fresh data ops only.
        v["replaces"] = json!(["0001_a"]);
        assert_eq!(code(&v), Code::Checkpoint);
        v.as_object_mut().unwrap().remove("base");
        assert!(decode(&v, "f.json").is_ok());
        v["operations"] = json!([{"op": "add_check", "v": 1, "table": "t", "name": "c",
                                  "expr": "x > 0"}]);
        assert_eq!(code(&v), Code::Checkpoint);
    }

    #[test]
    fn raw_sql_rules() {
        let state = json!([{"op": "add_index", "v": 1, "table": "user", "name": "user_trgm",
                            "columns": ["name"], "method": "gin", "skipped_on": ["mysql"]}]);
        let mut raw = json!({"op": "raw_sql", "v": 1, "id": "trgm",
            "up": {"postgres": "CREATE INDEX ...", "mysql": null, "sqlite": null},
            "state": state});
        assert_eq!(
            code(&file(json!([raw.clone()]))),
            Code::Dialect,
            "sqlite not skipped"
        );
        raw["state"][0]["skipped_on"] = json!(["mysql", "sqlite"]);
        assert!(decode(&file(json!([raw.clone()])), "f.json").is_ok());
        raw["tx"] = json!("autocommit");
        assert_eq!(code(&file(json!([raw.clone()]))), Code::File, "no probe");
        raw["probe"] = json!("state");
        assert!(decode(&file(json!([raw.clone()])), "f.json").is_ok());
        // A missing backend key and `"any": null` mean nothing runs there too.
        let mut missing = raw.clone();
        missing["up"] = json!({"postgres": "CREATE INDEX ..."});
        missing["state"][0]["skipped_on"] = json!(["mysql"]);
        assert_eq!(
            code(&file(json!([missing]))),
            Code::Dialect,
            "sqlite key missing"
        );
        let mut any_null = raw.clone();
        any_null["up"] = json!({"any": null, "postgres": "CREATE INDEX ..."});
        any_null["state"][0]["skipped_on"] = json!(["sqlite"]);
        assert_eq!(
            code(&file(json!([any_null]))),
            Code::Dialect,
            "mysql via any: null"
        );
        raw.as_object_mut().unwrap().remove("state");
        assert_eq!(code(&file(json!([raw]))), Code::File, "state is required");
    }

    /// `min`/`max` are not column keys: makemigrations writes checks.
    #[test]
    fn min_and_max_are_refused_at_load() {
        let v = file(json!([{"op": "add_column", "v": 1, "table": "t",
            "column": {"column": "n", "ty": "i64", "nullable": true, "min": 0}}]));
        let e = decode(&v, "f.json").unwrap_err();
        assert_eq!(e.code, Code::File);
        assert!(e.msg.contains("add_check"), "{e}");
        let v = file(
            json!([{"op": "alter_column", "v": 1, "table": "t", "column": "n",
            "changes": {"max": {"from": null, "to": 9}}}]),
        );
        assert_eq!(code(&v), Code::File);
    }

    #[test]
    fn sql_lines_join() {
        let v = file(json!([{"op": "raw_sql", "v": 1, "id": "x",
            "up": {"any": ["UPDATE t", "  SET a = 1"]}, "state": []}]));
        let f = decode(&v, "f.json").unwrap();
        let Op::RawSql { up, .. } = &f.operations[0].op else {
            panic!()
        };
        assert_eq!(up.get(Backend::Mysql).unwrap().0, "UPDATE t\n  SET a = 1");
    }

    #[test]
    fn table_scope_matches_the_target() {
        let v = file(json!([{"op": "create_table", "v": 1,
            "table": {"name": "org", "scope": "registry", "fields": []}}]));
        assert_eq!(code(&v), Code::File);
    }
}
