//! `dumpdata` and `loaddata`.

use super::*;

/// `manage dumpdata [--model app.Name] [--indent N]` — fixture
/// export. Iterates every model registered in `inventory` and
/// emits a JSON array:
///
/// ```json
/// [
///   {"model": "blog.Article", "pk": 1, "fields": {"title": "...", ...}},
///   {"model": "blog.Article", "pk": 2, "fields": {"title": "...", ...}}
/// ]
/// ```
///
/// `model` defaults to the resolved app label + schema name. Pass
/// `--model <name>` to limit to a single model (full `app.Model`
/// name OR bare model name); pass multiple times to limit to a set.
/// `--indent N` controls JSON formatting (default `2`).
///
/// Output goes to stdout, or with `--output <file>` to that file as
/// UTF-8: PowerShell 5.1's `>` writes UTF-16.
///
/// `loaddata` is the companion verb that re-applies a fixture
/// export — queued as a follow-up.
#[derive(Debug, Default, PartialEq)]
pub(super) struct DumpdataArgs {
    /// Limit to these `app.Model` or `Model` names. Empty = every model.
    pub(super) model_filters: Vec<String>,
    /// `--exclude`: models left out, same name forms as `--model`.
    excludes: Vec<String>,
    /// JSON indent. `0` = compact single-line; otherwise pretty (2-space).
    pub(super) indent: usize,
    /// `true` when the user passed `--help`; cmd short-circuits to help.
    pub(super) help: bool,
    /// `--output`: write UTF-8 to this file instead of the writer.
    output: Option<String>,
}

pub(super) fn parse_dumpdata_args(args: &[String]) -> Result<DumpdataArgs, MigrateError> {
    let mut out = DumpdataArgs {
        indent: 2,
        ..Default::default()
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                out.help = true;
                return Ok(out);
            }
            "--model" => {
                let v = iter.next().ok_or_else(|| missing_value("--model"))?;
                out.model_filters.push(v.clone());
            }
            "--exclude" => {
                let v = iter.next().ok_or_else(|| missing_value("--exclude"))?;
                out.excludes.push(v.clone());
            }
            "--output" | "-o" => {
                let v = iter.next().ok_or_else(|| missing_value("--output"))?;
                out.output = Some(v.clone());
            }
            "--indent" => {
                let v = iter.next().ok_or_else(|| missing_value("--indent"))?;
                out.indent = v.parse().map_err(|e| {
                    MigrateError::Validation(format!("--indent: not an integer ({e})"))
                })?;
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                return Err(unexpected_positional(other));
            }
        }
    }
    Ok(out)
}

/// `dumpdata` over every model, or only those of `scope`.
pub(crate) async fn dumpdata_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    scope: DataScope<'_>,
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_dumpdata_args(args)?;
    if parsed.help {
        writeln!(
            w,
            "dumpdata [--model app.Name] [--exclude app.Name] [--indent N] [--output FILE]"
        )?;
        writeln!(w)?;
        writeln!(
            w,
            "  Export every registered model's rows as JSON in fixture"
        )?;
        writeln!(
            w,
            "  shape: `[{{\"model\": \"app.Model\", \"pk\": N, \"fields\": {{...}}}}]`."
        )?;
        writeln!(w)?;
        writeln!(
            w,
            "  --model <name>   Limit to a single model. Accepts either the full"
        )?;
        writeln!(
            w,
            "                   `app.Model` shape or the bare model name. Pass"
        )?;
        writeln!(
            w,
            "                   the flag multiple times to limit to a set."
        )?;
        writeln!(
            w,
            "  --indent <N>     JSON indent (default 2; 0 emits compact single-line)."
        )?;
        writeln!(
            w,
            "  --exclude <name> Leave a model out (repeatable; same forms as --model)."
        )?;
        writeln!(
            w,
            "  --output, -o <file>  Write UTF-8 to <file>, replaced only once the dump is complete;"
        )?;
        writeln!(w, "                   PowerShell's `>` writes UTF-16.")?;
        return Ok(());
    }
    let model_filters = &parsed.model_filters;
    let indent = parsed.indent;
    // A typo would otherwise dump `[]` and exit 0.
    for name in model_filters.iter().chain(&parsed.excludes) {
        let mut named = inventory::iter::<crate::core::ModelEntry>()
            .filter(|e| model_matches(name, &dotted_model_name(e), e.schema))
            .peekable();
        let Some(owner) = named.peek().map(|e| e.schema.scope) else {
            return Err(MigrateError::Validation(format!(
                "dumpdata: unknown model `{name}` (use `app.Model` or `Model`)"
            )));
        };
        if !named.any(|e| in_scope(e.schema, scope)) {
            return Err(MigrateError::Validation(format!(
                "dumpdata: {}",
                out_of_scope(name, owner)
            )));
        }
    }

    // Filter: if the user specified --model, only emit models
    // whose `app.Model` or bare `Model` matches.
    let selected: Vec<&crate::core::ModelEntry> = inventory::iter::<crate::core::ModelEntry>()
        .filter(|e| in_scope(e.schema, scope))
        .filter(|e| {
            let dotted = dotted_model_name(e);
            let matches = |f: &String| model_matches(f, &dotted, e.schema);
            (model_filters.is_empty() || model_filters.iter().any(matches))
                && !parsed.excludes.iter().any(matches)
        })
        .collect();
    let schemas: Vec<_> = selected.iter().map(|e| e.schema).collect();
    require_tenant_tables(pool, scope, &schemas, "dumpdata").await?;

    let mut sink = match &parsed.output {
        Some(path) => Sink::File(
            PendingFile::create(Path::new(path))
                .map_err(|e| MigrateError::Validation(format!("dumpdata: create `{path}`: {e}")))?,
        ),
        None => Sink::Out(w),
    };
    // Walk every selected model, writing its rows a page at a time.
    let mut out = FixtureWriter::new(&mut sink, indent);
    for entry in selected {
        let schema = entry.schema;
        let dotted_name = dotted_model_name(entry);
        // Unmanaged tables such as TOTP are made on first use: none yet, no rows.
        if !schema.managed && !runner::try_table_resolves(pool, schema.table).await? {
            continue;
        }

        // These decode as `null`, so a reload would wipe them (#1911).
        if let Some(f) = schema.scalar_fields().find(|f| !dumpable(f.ty)) {
            return Err(MigrateError::Validation(format!(
                "dumpdata: `{dotted_name}.{}` is a {:?} column, which dumpdata cannot \
                 export yet — leave it out with `--exclude {dotted_name}`",
                f.name, f.ty
            )));
        }

        // Identify the PK column for fixture `pk` extraction.
        let pk_field = schema.primary_key();
        let fields: Vec<&'static crate::core::FieldSchema> = schema.scalar_fields().collect();
        // Pages in pk order, so neither rows nor links are held whole.
        // `pk > last` skips ties, so only a real one-column key pages.
        let paged = match pk_field {
            Some(pk) => is_sole_pk(pool, schema.table, pk.column).await?,
            None => false,
        };
        let mut after: Option<crate::core::SqlValue> = None;
        loop {
            let mut query = crate::core::SelectQuery::new(schema);
            if let Some(pk) = pk_field {
                query.order_by = vec![crate::core::OrderClause {
                    column: pk.column,
                    desc: false,
                }
                .into()];
            }
            if let Some(pk) = pk_field.filter(|_| paged) {
                query.limit = Some(DUMP_PAGE);
                if let Some(last) = after.take() {
                    query.where_clause = crate::core::WhereExpr::Predicate(crate::core::Filter {
                        column: pk.column,
                        op: crate::core::Op::Gt,
                        value: last,
                    });
                }
            }
            let rows = crate::sql::select_rows_as_json_with(pool, &query, &fields, DUMP_DECODE)
                .await
                .map_err(|e| {
                    MigrateError::Validation(format!(
                        "dumpdata: select from `{}` failed: {e}",
                        schema.table
                    ))
                })?;
            let full = paged && rows.len() == DUMP_PAGE as usize;
            let pks: Vec<crate::core::SqlValue> = match pk_field {
                Some(pk) => rows
                    .iter()
                    .filter_map(|r| r.get(pk.name))
                    .map(|v| json_to_sql_value(v, pk).map_err(MigrateError::Validation))
                    .collect::<Result<_, _>>()?,
                None => Vec::new(),
            };
            let mut links: Vec<(&str, LinksBySource)> = Vec::new();
            for chunk in pks.chunks(DUMP_PAGE as usize) {
                for (name, by_src) in m2m_links(pool, schema, chunk).await? {
                    match links.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, all)) => all.extend(by_src),
                        None => links.push((name, by_src)),
                    }
                }
            }

            for mut row in rows {
                // Pop the PK column off `fields` into the outer fixture
                // entry's `pk` slot: a fixture separates identity from
                // payload.
                let pk_value = match pk_field {
                    Some(pk) => row
                        .as_object_mut()
                        .and_then(|m| m.remove(pk.name))
                        .unwrap_or(serde_json::Value::Null),
                    None => serde_json::Value::Null,
                };
                if let Some(map) = row.as_object_mut() {
                    for (name, by_src) in &links {
                        let dsts = by_src.get(&pk_value.to_string()).cloned();
                        map.insert((*name).to_owned(), dsts.unwrap_or_default().into());
                    }
                }
                out.entry(&serde_json::json!({
                    "model": dotted_name,
                    "pk": pk_value,
                    "fields": row,
                }))?;
            }
            if !full {
                break;
            }
            after = pks.last().cloned();
        }
    }
    out.finish()?;
    if let Sink::File(file) = sink {
        file.commit()?;
    }
    Ok(())
}

/// A file written beside `dest` and renamed over it only once complete, so
/// a failed dump leaves the old fixture as it was. Owner-only: a dump holds
/// every row, credentials included.
pub(super) struct PendingFile {
    file: std::fs::File,
    tmp: std::path::PathBuf,
    dest: std::path::PathBuf,
    done: bool,
}

impl PendingFile {
    fn create(dest: &Path) -> std::io::Result<Self> {
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        // Unguessable, and never opened through a link planted there.
        let rand = uuid::Uuid::new_v4().simple();
        let tmp = dest.with_file_name(format!(".{name}.{rand}.tmp"));
        Ok(Self {
            file: crate::private_file::create(&tmp)?,
            tmp,
            dest: dest.to_owned(),
            done: false,
        })
    }

    fn commit(mut self) -> std::io::Result<()> {
        self.file.sync_all()?;
        std::fs::rename(&self.tmp, &self.dest)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Where `dumpdata` writes: the command's writer, or an `--output` file.
pub(super) enum Sink<'a, W: Write> {
    Out(&'a mut W),
    File(PendingFile),
}

impl<W: Write> Write for Sink<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Out(w) => w.write(buf),
            Self::File(f) => f.file.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Out(w) => w.flush(),
            Self::File(f) => f.file.flush(),
        }
    }
}

/// Tags a JSON column value in a fixture, so a JSON `null` stays apart
/// from SQL NULL (plain `null`).
pub(super) const JSON_TAG: &str = "$json";

/// A JSON column value as dumpdata writes it: `null`, and any value that
/// looks like a tag, go inside one.
pub(super) fn fixture_json(v: serde_json::Value) -> serde_json::Value {
    if v.is_null() || json_tagged(&v).is_some() {
        serde_json::json!({ JSON_TAG: v })
    } else {
        v
    }
}

/// dumpdata's read: a cell it cannot decode fails the dump, never `null`.
pub(super) const DUMP_DECODE: crate::sql::CellDecode = crate::sql::CellDecode {
    json: fixture_json,
    on_bad: crate::sql::OnBadCell::Fail,
};

/// The value inside a [`JSON_TAG`] wrapper.
pub(super) fn json_tagged(v: &serde_json::Value) -> Option<&serde_json::Value> {
    v.as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.get(JSON_TAG))
}

/// [`json_to_sql_value`] for a dumpdata fixture: a JSON column reads the
/// [`JSON_TAG`] wrapper, and a bare `null` on a NOT NULL one is JSON `null`.
/// Seed fixtures keep the plain reading.
pub(super) fn loaddata_value(
    v: &serde_json::Value,
    field: &crate::core::FieldSchema,
) -> Result<crate::core::SqlValue, String> {
    use crate::core::{FieldType, SqlValue};
    if matches!(field.ty, FieldType::Json) {
        if let Some(inner) = json_tagged(v) {
            return Ok(SqlValue::Json(inner.clone()));
        }
        // Older dumps wrote JSON `null` untagged, as SQL NULL.
        if v.is_null() && !field.nullable {
            return Ok(SqlValue::Json(serde_json::Value::Null));
        }
    }
    json_to_sql_value(v, field)
}

/// Rows per dumpdata page.
pub(super) const DUMP_PAGE: i64 = 500;

/// Which models `dumpdata` / `loaddata` cover.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DataScope<'a> {
    /// Every model: single-tenant `manage`.
    All,
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Registry,
    /// Tenant models. `schema` names a schema-mode tenant, whose tables must
    /// be there: `search_path` would fall through to `public`.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Tenant { schema: Option<&'a str> },
}

/// Whether a data verb run for `scope` covers `s`.
pub(super) fn in_scope(s: &crate::core::ModelSchema, scope: DataScope<'_>) -> bool {
    match scope {
        DataScope::All => true,
        DataScope::Registry => s.scope == crate::core::ModelScope::Registry,
        DataScope::Tenant { .. } => s.scope == crate::core::ModelScope::Tenant,
    }
}

/// Refuse when one of `models`' tables (or junctions) is missing from a
/// schema-mode tenant's schema, before anything is read or written.
pub(super) async fn require_tenant_tables(
    pool: &Pool,
    scope: DataScope<'_>,
    models: &[&'static crate::core::ModelSchema],
    verb: &str,
) -> Result<(), MigrateError> {
    let DataScope::Tenant {
        schema: Some(schema),
    } = scope
    else {
        return Ok(());
    };
    // Unmanaged tables and views may live elsewhere on purpose.
    let tables = models
        .iter()
        .filter(|s| s.managed && !s.is_view)
        .flat_map(|s| {
            std::iter::once(s.table)
                .chain(s.m2m.iter().filter(|r| r.auto_create).map(|r| r.through))
        });
    for table in tables {
        if crate::migrate::inspectdb::list_tables(pool, schema, Some(table))
            .await?
            .is_empty()
        {
            return Err(MigrateError::Validation(format!(
                "{verb}: table `{table}` is not in tenant schema `{schema}`; migrate the tenant first"
            )));
        }
    }
    Ok(())
}

/// The refusal for a model that lives on the other side of the registry.
pub(super) fn out_of_scope(name: &str, owner: crate::core::ModelScope) -> String {
    let hint = match owner {
        crate::core::ModelScope::Registry => "a registry model: run without `--tenant`",
        _ => "a tenant model: pass `--tenant <slug>`",
    };
    format!("`{name}` is {hint}")
}

/// Whether `column` alone is `table`'s primary key in the database: inspectdb
/// marks one column of a composite key, and a view has none.
pub(super) async fn is_sole_pk(
    pool: &Pool,
    table: &str,
    column: &str,
) -> Result<bool, MigrateError> {
    let schema = crate::migrate::ensure::creation_schema(pool)
        .await?
        .unwrap_or_default();
    let pk = crate::migrate::inspectdb::list_pk_columns(pool, &schema, table).await?;
    Ok(matches!(pk.as_slice(), [c] if c == column))
}

/// Writes a fixture array an entry at a time, as `serde_json` would whole.
/// Buffered: stdout flushes on every newline, twice per pretty entry.
pub(super) struct FixtureWriter<'w, W: Write> {
    w: std::io::BufWriter<&'w mut W>,
    pretty: bool,
    started: bool,
}

impl<'w, W: Write> FixtureWriter<'w, W> {
    pub(super) fn new(w: &'w mut W, indent: usize) -> Self {
        Self {
            w: std::io::BufWriter::new(w),
            pretty: indent != 0,
            started: false,
        }
    }

    pub(super) fn entry(&mut self, v: &serde_json::Value) -> Result<(), MigrateError> {
        let json = |e| MigrateError::Validation(format!("dumpdata: serialize JSON: {e}"));
        let sep = match (self.started, self.pretty) {
            (false, false) => "[",
            (false, true) => "[\n",
            (true, false) => ",",
            (true, true) => ",\n",
        };
        self.started = true;
        self.w.write_all(sep.as_bytes())?;
        if self.pretty {
            // JSON strings escape newlines, so every line is structure.
            let text = serde_json::to_string_pretty(v).map_err(json)?;
            let lines: Vec<String> = text.lines().map(|l| format!("  {l}")).collect();
            self.w.write_all(lines.join("\n").as_bytes())?;
        } else {
            serde_json::to_writer(&mut self.w, v).map_err(json)?;
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<(), MigrateError> {
        let end = match (self.started, self.pretty) {
            (false, _) => "[]",
            (true, false) => "]",
            (true, true) => "\n]",
        };
        writeln!(self.w, "{end}")?;
        self.w.flush()?;
        Ok(())
    }
}

/// `app.Model`, or the bare name for a model with no app.
pub(super) fn dotted_model_name(entry: &crate::core::ModelEntry) -> String {
    match entry.resolved_app_label() {
        Some(app) if !app.is_empty() => format!("{app}.{}", entry.schema.name),
        _ => entry.schema.name.to_owned(),
    }
}

/// The one rule `--model`, `--exclude` and the unknown-name check share.
pub(super) fn model_matches(filter: &str, dotted: &str, schema: &crate::core::ModelSchema) -> bool {
    filter == dotted || filter == schema.name
}

/// Target pks keyed by source pk (its JSON text).
pub(super) type LinksBySource = std::collections::HashMap<String, Vec<serde_json::Value>>;

/// `rel`'s auto-created junction as the emitters see it, and its two ends.
pub(super) type Junction = (
    &'static crate::core::ModelSchema,
    [&'static crate::core::FieldSchema; 2],
);

/// Both ends take their model's pk type, so a text or UUID key decodes.
pub(super) fn junction_of(
    src: &'static crate::core::ModelSchema,
    rel: &crate::core::M2MRelation,
) -> Result<Junction, MigrateError> {
    let pk_ty = |schema: Option<&'static crate::core::ModelSchema>, table: &str| {
        schema.and_then(crate::sql::m2m::end_type).ok_or_else(|| {
            MigrateError::Validation(format!(
                "M2M `{}`: `{table}` has no registered primary key",
                rel.name
            ))
        })
    };
    let dst = crate::core::ModelEntry::for_table(rel.to).map(|e| e.schema);
    let junction = crate::sql::m2m::typed_junction(
        rel.through,
        &[
            (rel.src_col, pk_ty(Some(src), src.table)?),
            (rel.dst_col, pk_ty(dst, rel.to)?),
        ],
    );
    let end = |col: &str| {
        junction.field_by_column(col).ok_or_else(|| {
            MigrateError::Validation(format!("junction `{}` has no `{col}` column", rel.through))
        })
    };
    Ok((junction, [end(rel.src_col)?, end(rel.dst_col)?]))
}

/// Each auto-created M2M relation of `schema` and the links of `src_pks`.
/// Those junctions are no model, so the links ride on the source rows.
pub(super) async fn m2m_links(
    pool: &Pool,
    schema: &'static crate::core::ModelSchema,
    src_pks: &[crate::core::SqlValue],
) -> Result<Vec<(&'static str, LinksBySource)>, MigrateError> {
    let mut out = Vec::new();
    if src_pks.is_empty() {
        return Ok(out);
    }
    for rel in schema.m2m.iter().filter(|r| r.auto_create) {
        let (junction, [src, dst]) = junction_of(schema, rel)?;
        let query = crate::core::SelectQuery::by_pk_in(junction, rel.src_col, src_pks.to_vec());
        let rows = crate::sql::select_rows_as_json_with(pool, &query, &[src, dst], DUMP_DECODE)
            .await
            .map_err(|e| {
                MigrateError::Validation(format!(
                    "dumpdata: select from `{}` failed: {e}",
                    rel.through
                ))
            })?;
        let mut by_src = LinksBySource::new();
        for row in rows {
            if let (Some(s), Some(d)) = (row.get(src.name), row.get(dst.name)) {
                by_src.entry(s.to_string()).or_default().push(d.clone());
            }
        }
        for dsts in by_src.values_mut() {
            dsts.sort_by(|a, b| (a.as_i64(), a.as_str()).cmp(&(b.as_i64(), b.as_str())));
        }
        out.push((rel.name, by_src));
    }
    Ok(out)
}

/// One fixture row's links of one relation: its entry line, pk and targets.
pub(super) type RowLinks = (usize, serde_json::Value, Vec<serde_json::Value>);

/// Insert every row's links of `rel` in bind-sized batches; a link already
/// there is kept.
pub(super) async fn load_links(
    pool: &Pool,
    schema: &'static crate::core::ModelSchema,
    rel: &crate::core::M2MRelation,
    rows: &[RowLinks],
    on_error: OnRowError,
) -> Result<Vec<String>, String> {
    let (junction, [s, d]) = junction_of(schema, rel).map_err(|e| e.to_string())?;
    let insert = |values| async move {
        let query =
            crate::core::BulkInsertQuery::new(junction, vec![rel.src_col, rel.dst_col], values)
                .on_conflict(crate::core::ConflictClause::DoNothing);
        crate::sql::bulk_insert_pool(pool, &query).await
    };
    // Each failed row's message; a bad row skips only its own links.
    let mut failed = Vec::new();
    let mut converted = Vec::new();
    for (line, src, dsts) in rows {
        let row: Result<Vec<_>, String> = json_to_sql_value(src, s).and_then(|src| {
            dsts.iter()
                .map(|dst| Ok(vec![src.clone(), json_to_sql_value(dst, d)?]))
                .collect()
        });
        match row {
            Ok(values) => converted.push((line, values)),
            Err(e) => failed.push(format!("entry #{line}: {e}")),
        }
        if on_error == OnRowError::Abort && !failed.is_empty() {
            return Ok(failed);
        }
    }
    let batch: Vec<_> = converted.iter().flat_map(|(_, v)| v.clone()).collect();
    if batch.is_empty() || insert(batch).await.is_ok() {
        return Ok(failed);
    }
    // The batch failed: find the rows that did, one at a time.
    for (line, values) in converted {
        if let Err(e) = insert(values).await {
            failed.push(format!("entry #{line}: {e}"));
            if on_error == OnRowError::Abort {
                break;
            }
        }
    }
    Ok(failed)
}

/// What a load does with a row it cannot write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OnRowError {
    /// Warn, count it skipped, and go on; the load still fails at the end.
    Skip,
    /// Stop at the first one (`--fail-fast`).
    Abort,
}

#[derive(Debug, PartialEq)]
pub(super) struct LoaddataArgs {
    /// Path to the fixture JSON file.
    pub(super) file: String,
    pub(super) on_error: OnRowError,
    /// Help short-circuit.
    pub(super) help: bool,
}

pub(super) fn parse_loaddata_args(args: &[String]) -> Result<LoaddataArgs, MigrateError> {
    let mut file: Option<String> = None;
    let mut on_error = OnRowError::Skip;
    let mut help = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                help = true;
                break;
            }
            "--fail-fast" => on_error = OnRowError::Abort,
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                if file.is_some() {
                    return Err(unexpected_positional(other));
                }
                file = Some(other.to_owned());
            }
        }
    }
    if help {
        return Ok(LoaddataArgs {
            file: String::new(),
            on_error: OnRowError::Skip,
            help: true,
        });
    }
    let file = file.ok_or_else(|| {
        MigrateError::Validation("loaddata: missing <fixture.json> argument".into())
    })?;
    Ok(LoaddataArgs {
        file,
        on_error,
        help: false,
    })
}

/// `manage loaddata <fixture.json> [--fail-fast]` — companion to
/// `dumpdata`. Reads a fixture array and inserts each
/// row via [`crate::sql::insert_pool`]. Models are resolved by
/// `inventory` lookup against the `"app.Model"` name in the fixture.
///
/// Default behaviour: log + skip rows that fail (unknown model,
/// JSON shape mismatch, INSERT error). `--fail-fast` aborts on the
/// first failure.
///
/// JSON values map onto rustango's `SqlValue` types by field kind
/// declared in the target model's schema:
/// - integer / float JSON → numeric `SqlValue` per `FieldType`
/// - boolean → `SqlValue::Bool`
/// - string → parsed per `FieldType` (Decimal / Date / DateTime /
///   Time / Uuid / Binary [hex] / String)
/// - null → `SqlValue::Null`; on a NOT NULL JSON field, JSON `null`
/// - object / array → `SqlValue::Json`; `{"$json": v}` → `v`
/// `loaddata` into every model, or only those of `scope`.
pub(crate) async fn loaddata_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    scope: DataScope<'_>,
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_loaddata_args(args)?;
    if parsed.help {
        writeln!(w, "loaddata <fixture.json> [--fail-fast]")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Insert every row in `fixture.json` via the registered model schemas."
        )?;
        writeln!(
            w,
            "  Fixture shape: `[{{\"model\": \"app.Model\", \"pk\": N, \"fields\": {{...}}}}]`"
        )?;
        writeln!(w, "  — the same shape `manage dumpdata` produces.")?;
        writeln!(w)?;
        writeln!(
            w,
            "  --fail-fast   Abort on the first error instead of skipping the row."
        )?;
        writeln!(
            w,
            "  A failed or partial load is not rolled back: rows already inserted stay."
        )?;
        return Ok(());
    }

    let raw = crate::text_file::read(Path::new(&parsed.file))
        .map_err(|e| MigrateError::Validation(format!("loaddata: read `{}`: {e}", parsed.file)))?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).map_err(|e| {
        MigrateError::Validation(format!("loaddata: parse `{}` as JSON: {e}", parsed.file))
    })?;

    // Build a lookup map: "app.Model" + bare "Model" → &'static ModelSchema.
    let mut schemas: std::collections::HashMap<String, &'static crate::core::ModelSchema> =
        std::collections::HashMap::new();
    // Models of the other scope, refused rather than written to this pool.
    let mut elsewhere = std::collections::HashMap::new();
    for entry in inventory::iter::<crate::core::ModelEntry>() {
        let schema = entry.schema;
        let app = entry.resolved_app_label().unwrap_or("");
        let names = std::iter::once(schema.name.to_owned())
            .chain((!app.is_empty()).then(|| format!("{app}.{}", schema.name)));
        if in_scope(schema, scope) {
            schemas.extend(names.map(|n| (n, schema)));
        } else {
            elsewhere.extend(names.map(|n| (n, schema.scope)));
        }
    }

    // Parents first: fixtures come in registration order, and a child
    // inserted before its parent fails its FK (#1911). A nullable link in a
    // cycle goes in as NULL and is set once every row is in.
    let (named, used) = fixture_tables(&entries, &schemas);
    let LoadPlan { order, deferred } = load_plan(&named, &used);
    let entries = in_fk_order(entries, &schemas, &order);
    require_tenant_tables(pool, scope, &named, "loaddata").await?;

    let mut loaded = 0_usize;
    let mut skipped = 0_usize;
    let mut touched: Vec<&'static crate::core::ModelSchema> = Vec::new();
    // `--fail-fast` stops here, but the rows already in still need their
    // sequences moved, so the error is raised after the reset.
    let mut abort: Option<String> = None;
    // M2M links load after every row, so both ends exist, one
    // batched insert per relation.
    let mut links: Vec<(
        &'static crate::core::ModelSchema,
        &'static crate::core::M2MRelation,
        String,
        Vec<RowLinks>,
    )> = Vec::new();
    let mut relinks: Vec<(String, crate::core::UpdateQuery)> = Vec::new();
    for (idx, entry) in entries.into_iter().enumerate() {
        let line = idx + 1;
        let model_name = entry.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let schema = match schemas.get(model_name) {
            Some(s) => *s,
            None => {
                let msg = match elsewhere.get(model_name) {
                    Some(owner) => format!(
                        "loaddata: entry #{line}: {}",
                        out_of_scope(model_name, *owner)
                    ),
                    None => format!(
                        "loaddata: entry #{line}: unknown model `{model_name}` (registered: {} models)",
                        schemas.len() / 2, // dotted + bare keys
                    ),
                };
                if parsed.on_error == OnRowError::Abort {
                    abort = Some(msg);
                    break;
                }
                tracing::warn!("{msg}");
                skipped += 1;
                continue;
            }
        };

        // Build column/value vectors. PK rides at the top level of
        // the fixture entry, so we re-glue it into the fields map.
        let mut fields_obj: serde_json::Map<String, serde_json::Value> = entry
            .get("fields")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(pk_value) = entry.get("pk").cloned() {
            if let Some(pk_field) = schema.primary_key() {
                fields_obj.insert(pk_field.name.to_owned(), pk_value);
            }
        }

        let mut columns: Vec<&'static str> = Vec::new();
        let mut values: Vec<crate::core::SqlValue> = Vec::new();
        let mut row_err: Option<String> = None;
        // The database computes a generated column; an INSERT naming it fails.
        for f in schema.scalar_fields().filter(|f| f.generated_as.is_none()) {
            // An absent key takes the column default, not NULL; a null pk is absent.
            let Some(raw) = fields_obj
                .get(f.name)
                .filter(|v| !(f.primary_key && v.is_null()))
            else {
                continue;
            };
            match loaddata_value(raw, f) {
                Ok(v) => {
                    columns.push(f.column);
                    values.push(v);
                }
                Err(e) => {
                    row_err = Some(format!("field `{}`: {e}", f.name));
                    break;
                }
            }
        }
        let mut row_links = Vec::new();
        for rel in schema.m2m.iter().filter(|r| r.auto_create) {
            match fields_obj.get(rel.name) {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::Array(dsts)) if !dsts.is_empty() => {
                    row_links.push((rel, dsts.clone()));
                }
                Some(serde_json::Value::Array(_)) => {}
                Some(_) => row_err = Some(format!("field `{}`: expected a list of pks", rel.name)),
            }
        }
        let src_pk = entry.get("pk").cloned().unwrap_or_default();
        if !row_links.is_empty() && src_pk.is_null() {
            row_err = Some("M2M links need the row's `pk`".to_owned());
        }
        if let Some(e) = row_err {
            let msg = format!("loaddata: entry #{line} (`{model_name}`): {e}");
            if parsed.on_error == OnRowError::Abort {
                abort = Some(msg);
                break;
            }
            tracing::warn!("{msg}");
            skipped += 1;
            continue;
        }

        // A row without its pk is relinked through the one the insert returns.
        let pk_field = schema.primary_key();
        let relink = match pk_field {
            Some(_) => take_deferred(schema, &columns, &mut values, &deferred),
            None => Vec::new(),
        };
        let returning = match pk_field {
            Some(pk) if !relink.is_empty() && !columns.contains(&pk.column) => vec![pk.column],
            _ => vec![],
        };
        // MySQL hands back only an AUTO_INCREMENT id; refuse before the write.
        if let Some(pk) = pk_field.filter(|pk| {
            pool.dialect().kind() == crate::sql::DialectKind::Mysql
                && !returning.is_empty()
                && !pk.is_serial()
        }) {
            let msg = format!(
                "loaddata: entry #{line} (`{model_name}`): MySQL cannot return the \
                 database-generated `{}`, so this row's cycle link can't be set; give it a `pk`",
                pk.name
            );
            if parsed.on_error == OnRowError::Abort {
                abort = Some(msg);
                break;
            }
            tracing::warn!("{msg}");
            skipped += 1;
            continue;
        }
        let query = crate::core::InsertQuery {
            model: schema,
            columns,
            values,
            returning,
            on_conflict: None,
            generated: vec![],
        };
        let inserted = match pk_field.filter(|_| !relink.is_empty()) {
            None => crate::sql::insert_pool(pool, &query).await.map(|()| None),
            Some(pk) => {
                async {
                    let returned = if query.returning.is_empty() {
                        crate::sql::insert_pool(pool, &query).await?;
                        None
                    } else {
                        Some(crate::sql::insert_returning_pool(pool, &query).await?)
                    };
                    let value = match &returned {
                        Some(r) => crate::sql::inserted_pk(&query, r, pk)?,
                        None => {
                            let i = query.columns.iter().position(|c| *c == pk.column);
                            i.map(|i| query.values[i].clone())
                                .unwrap_or(crate::core::SqlValue::Null)
                        }
                    };
                    Ok(Some(crate::core::UpdateQuery {
                        model: schema,
                        set: relink,
                        where_clause: crate::core::WhereExpr::Predicate(crate::core::Filter {
                            column: pk.column,
                            op: crate::core::Op::Eq,
                            value,
                        }),
                    }))
                }
                .await
            }
        };
        match inserted {
            Ok(relink) => {
                loaded += 1;
                if let Some(update) = relink {
                    relinks.push((format!("entry #{line} (`{model_name}`)"), update));
                }
                if !touched.iter().any(|t| t.table == schema.table) {
                    touched.push(schema);
                }
                for (rel, dsts) in row_links {
                    let row = (line, src_pk.clone(), dsts);
                    match links
                        .iter_mut()
                        .find(|(s, r, _, _)| s.table == schema.table && r.name == rel.name)
                    {
                        Some((_, _, _, rows)) => rows.push(row),
                        None => links.push((schema, rel, model_name.to_owned(), vec![row])),
                    }
                }
            }
            Err(e) => {
                let msg = format!("loaddata: entry #{line} (`{model_name}`) insert: {e}");
                if parsed.on_error == OnRowError::Abort {
                    abort = Some(msg);
                    break;
                }
                tracing::warn!("{msg}");
                skipped += 1;
            }
        }
    }

    // Even after a `--fail-fast` stop: the rows already in get their links.
    for (what, update) in relinks {
        let res = match crate::sql::update_pool(pool, &update).await {
            Ok(0) => Err("no row has the inserted pk".to_owned()),
            r => r.map(drop).map_err(|e| e.to_string()),
        };
        if let Err(e) = res {
            let msg = format!("loaddata: {what} link: {e}");
            tracing::warn!("{msg}");
            loaded -= 1;
            skipped += 1;
            if parsed.on_error == OnRowError::Abort && abort.is_none() {
                abort = Some(msg);
            }
        }
    }
    if abort.is_none() {
        'rels: for (schema, rel, model_name, rows) in links {
            let (failed, lost) = match load_links(pool, schema, rel, &rows, parsed.on_error).await {
                Ok(failed) => {
                    let n = failed.len();
                    (failed, n)
                }
                // No junction to write: every row's links are lost.
                Err(e) => (vec![e], rows.len()),
            };
            for e in failed {
                let msg = format!("loaddata: `{model_name}` `{}`: {e}", rel.name);
                if parsed.on_error == OnRowError::Abort {
                    abort = Some(msg);
                    break 'rels;
                }
                tracing::warn!("{msg}");
            }
            skipped += lost;
        }
    }

    reset_sequences(pool, &touched).await?;
    if let Some(msg) = abort {
        return Err(MigrateError::Validation(msg));
    }

    writeln!(w, "loaddata: {loaded} loaded, {skipped} skipped")?;
    if skipped > 0 {
        // A partial load is a failure; exit 0 read as a clean restore (#1911).
        return Err(MigrateError::Validation(format!(
            "loaddata: {skipped} row(s) skipped — see the warnings above"
        )));
    }
    Ok(())
}

/// `false` for the column types `select_rows_as_json` reads back as `null`.
pub(super) fn dumpable(ty: crate::core::FieldType) -> bool {
    use crate::core::FieldType as T;
    !matches!(
        ty,
        T::Array(_) | T::Range(_) | T::HStore | T::Vector(_) | T::Geometry(_)
    )
}

/// The order `loaddata` inserts the fixture's tables in, parents first, and
/// the nullable links it sets after every row is in.
pub(super) struct LoadPlan {
    order: Vec<&'static crate::core::ModelSchema>,
    deferred: DeferredLinks,
}

/// `(table, column)` of each FK column at least one row fills.
pub(super) type UsedLinks = std::collections::HashSet<(&'static str, &'static str)>;

/// The fixture's tables in first-seen order, and the FK columns its rows fill.
pub(super) fn fixture_tables(
    entries: &[serde_json::Value],
    schemas: &std::collections::HashMap<String, &'static crate::core::ModelSchema>,
) -> (Vec<&'static crate::core::ModelSchema>, UsedLinks) {
    let mut nodes: Vec<&'static crate::core::ModelSchema> = Vec::new();
    let mut used = UsedLinks::new();
    for e in entries {
        let Some(s) = schemas
            .get(e.get("model").and_then(|v| v.as_str()).unwrap_or(""))
            .copied()
        else {
            continue;
        };
        if !nodes.iter().any(|n| n.table == s.table) {
            nodes.push(s);
        }
        let Some(fields) = e.get("fields") else {
            continue;
        };
        used.extend(
            s.fields
                .iter()
                .filter(|f| f.relation.is_some())
                .filter(|f| fields.get(f.name).is_some_and(|v| !v.is_null()))
                .map(|f| (s.table, f.column)),
        );
    }
    (nodes, used)
}

/// Plan over `nodes` and the links `used` says some row sets: a nullable
/// link no row fills never orders. A link is deferred only when the tables
/// still form a cycle, and then only one that points against the order the
/// NOT NULL links give (as `flush_plan` breaks cycles). Self-links are left
/// to [`self_parents_first`].
pub(super) fn load_plan(nodes: &[&'static crate::core::ModelSchema], used: &UsedLinks) -> LoadPlan {
    let used = |s: &crate::core::ModelSchema, col: Option<&'static str>| {
        col.is_none_or(|c| used.contains(&(s.table, c)))
    };
    let (mut children_first_order, cyclic) = children_first(nodes, used);
    let (rest, stuck) = children_first(&cyclic, |_, col| col.is_none());
    children_first_order.extend(rest);
    children_first_order.extend(stuck);
    let order: Vec<_> = children_first_order.into_iter().rev().collect();
    let at = |t: &str| order.iter().position(|s| s.table == t);
    let in_cycle = |t: &str| cyclic.iter().any(|c| c.table == t);
    let deferred = cyclic
        .iter()
        .flat_map(|s| {
            fk_edges(s)
                .filter(|(to, col)| {
                    *to != s.table && in_cycle(to) && used(s, *col) && at(to) > at(s.table)
                })
                .filter_map(|(_, col)| col.map(|c| (s.table, c)))
        })
        .collect();
    LoadPlan { order, deferred }
}

/// Nullable links [`loaddata_cmd`] inserts as NULL and sets afterwards.
pub(super) type DeferredLinks = std::collections::HashSet<(&'static str, &'static str)>;

/// Swap this row's [`DeferredLinks`] values for NULL, returning them.
pub(super) fn take_deferred(
    schema: &'static crate::core::ModelSchema,
    columns: &[&'static str],
    values: &mut [crate::core::SqlValue],
    deferred: &DeferredLinks,
) -> Vec<crate::core::Assignment> {
    use crate::core::{Assignment, SqlValue};
    columns
        .iter()
        .zip(values.iter_mut())
        .filter(|(c, v)| deferred.contains(&(schema.table, **c)) && !matches!(v, SqlValue::Null))
        .map(|(c, v)| Assignment::new(c, std::mem::replace(v, SqlValue::Null)))
        .collect()
}

/// Stable-sort fixture entries into `order`'s table order, and a self-FK
/// parent before its child. Best effort: tables in a cycle of NOT NULL
/// links keep their fixture order.
pub(super) fn in_fk_order(
    entries: Vec<serde_json::Value>,
    schemas: &std::collections::HashMap<String, &'static crate::core::ModelSchema>,
    order: &[&'static crate::core::ModelSchema],
) -> Vec<serde_json::Value> {
    let mut keyed: Vec<(usize, serde_json::Value)> = entries
        .into_iter()
        .map(|e| {
            let rank = e
                .get("model")
                .and_then(|v| v.as_str())
                .and_then(|m| schemas.get(m))
                .and_then(|s| order.iter().position(|o| o.table == s.table))
                .map_or(0, |i| i + 1);
            (rank, e)
        })
        .collect();
    keyed.sort_by_key(|(rank, _)| *rank);
    let mut out = Vec::with_capacity(keyed.len());
    let mut group: Vec<serde_json::Value> = Vec::new();
    let mut current = None;
    for (rank, e) in keyed {
        if current != Some(rank) {
            out.extend(self_parents_first(std::mem::take(&mut group), schemas));
            current = Some(rank);
        }
        group.push(e);
    }
    out.extend(self_parents_first(group, schemas));
    out
}

/// Within one FK depth: a row whose self-FK names a row not yet emitted
/// waits for it (tree tables). Rows left in a cycle keep their order.
pub(super) fn self_parents_first(
    entries: Vec<serde_json::Value>,
    schemas: &std::collections::HashMap<String, &'static crate::core::ModelSchema>,
) -> Vec<serde_json::Value> {
    struct Pending {
        entry: serde_json::Value,
        key: Option<(&'static str, String)>,
        parents: Vec<(&'static str, String)>,
    }
    let mut pending: Vec<Pending> = entries
        .into_iter()
        .map(|entry| {
            let schema = entry
                .get("model")
                .and_then(|v| v.as_str())
                .and_then(|m| schemas.get(m));
            let Some(schema) = schema else {
                return Pending {
                    entry,
                    key: None,
                    parents: Vec::new(),
                };
            };
            let key = entry
                .get("pk")
                .filter(|v| !v.is_null())
                .map(|pk| (schema.table, pk.to_string()));
            let parents = schema
                .scalar_fields()
                .filter(|f| {
                    matches!(
                        f.relation,
                        Some(crate::core::Relation::Fk { to, .. }
                            | crate::core::Relation::O2O { to, .. }) if to == schema.table
                    )
                })
                .filter_map(|f| entry.get("fields")?.get(f.name))
                .filter(|v| !v.is_null())
                .map(|v| (schema.table, v.to_string()))
                .filter(|p| Some(p) != key.as_ref())
                .collect();
            Pending {
                entry,
                key,
                parents,
            }
        })
        .collect();
    let mut out = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let waiting: std::collections::HashSet<(&'static str, String)> =
            pending.iter().filter_map(|p| p.key.clone()).collect();
        let (ready, rest): (Vec<Pending>, Vec<Pending>) = pending
            .into_iter()
            .partition(|p| p.parents.iter().all(|k| !waiting.contains(k)));
        if ready.is_empty() {
            out.extend(rest.into_iter().map(|p| p.entry));
            break;
        }
        out.extend(ready.into_iter().map(|p| p.entry));
        pending = rest;
    }
    out
}

/// Move each loaded model's serial counter past the ids the fixture wrote,
/// or the next plain insert collides on Postgres (#1911).
pub(super) async fn reset_sequences(
    pool: &Pool,
    schemas: &[&'static crate::core::ModelSchema],
) -> Result<(), MigrateError> {
    for schema in schemas {
        let Some((sql, binds)) = reset_sequence_stmt(pool.dialect(), schema) else {
            continue;
        };
        crate::sql::raw_execute_pool(pool, &sql, binds)
            .await
            .map_err(|e| {
                MigrateError::Validation(format!(
                    "loaddata: resetting `{}`'s id sequence failed: {e}",
                    schema.table
                ))
            })?;
    }
    Ok(())
}

/// The serial-counter reset for `schema`, with its binds; `None` when
/// the PK is not an auto integer or the dialect needs none.
pub(crate) fn reset_sequence_stmt(
    dialect: &dyn crate::sql::Dialect,
    schema: &crate::core::ModelSchema,
) -> Option<(String, Vec<crate::core::SqlValue>)> {
    use crate::core::FieldType as T;
    let pk = schema.primary_key()?;
    if !pk.auto || !matches!(pk.ty, T::I16 | T::I32 | T::I64) {
        return None;
    }
    let sql = dialect.reset_sequence_sql(schema.table, pk.column)?;
    let binds = vec![
        crate::core::SqlValue::String(dialect.quote_ident(schema.table)),
        crate::core::SqlValue::String(pk.column.to_owned()),
    ];
    Some((sql, binds))
}

/// Map a JSON value onto an [`crate::core::SqlValue`] using the target
/// field's declared [`crate::core::FieldType`].
///
/// String inputs are reparsed for typed fields:
/// - Decimal: `rust_decimal::Decimal::from_str`
/// - Date: `chrono::NaiveDate::parse_from_str("%Y-%m-%d")`
/// - DateTime: RFC 3339 / `%Y-%m-%dT%H:%M:%S`
/// - Time: `%H:%M:%S%.f` then `%H:%M`
/// - Uuid: `Uuid::parse_str`
/// - Binary: lowercase hex
///
/// Object / Array JSON nodes always land as `SqlValue::Json`.
pub(crate) fn json_to_sql_value(
    v: &serde_json::Value,
    field: &crate::core::FieldSchema,
) -> Result<crate::core::SqlValue, String> {
    use crate::core::{FieldType, SqlValue};
    if v.is_null() {
        return Ok(SqlValue::Null);
    }
    match field.ty {
        FieldType::I16 => v
            .as_i64()
            .and_then(|n| i16::try_from(n).ok())
            .map(SqlValue::I16)
            .ok_or_else(|| format!("expected i16, got {v}")),
        FieldType::I32 => v
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .map(SqlValue::I32)
            .ok_or_else(|| format!("expected i32, got {v}")),
        // Integer-shaped strings too (e.g. SQLite NUMERIC).
        FieldType::I64 => v
            .as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
            .map(SqlValue::I64)
            .ok_or_else(|| format!("expected i64, got {v}")),
        FieldType::F32 => v
            .as_f64()
            .map(|n| SqlValue::F32(n as f32))
            .ok_or_else(|| format!("expected f32, got {v}")),
        FieldType::F64 => v
            .as_f64()
            .map(SqlValue::F64)
            .ok_or_else(|| format!("expected f64, got {v}")),
        FieldType::Bool => v
            .as_bool()
            .map(SqlValue::Bool)
            .ok_or_else(|| format!("expected bool, got {v}")),
        FieldType::String => v
            .as_str()
            .map(|s| SqlValue::String(s.to_owned()))
            .ok_or_else(|| format!("expected string, got {v}")),
        FieldType::DateTime => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for DateTime, got {v}"))?;
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| SqlValue::DateTime(d.with_timezone(&chrono::Utc)))
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                        .map(|ndt| SqlValue::DateTime(ndt.and_utc()))
                })
                .map_err(|e| format!("DateTime parse: {e}"))
        }
        FieldType::Date => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for Date, got {v}"))?;
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(SqlValue::Date)
                .map_err(|e| format!("Date parse: {e}"))
        }
        FieldType::Time => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for Time, got {v}"))?;
            // `%.f` too: dumpdata writes `12:34:56.789` (#1911).
            chrono::NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
                .or_else(|_| chrono::NaiveTime::parse_from_str(s, "%H:%M"))
                .map(SqlValue::Time)
                .map_err(|e| format!("Time parse: {e}"))
        }
        FieldType::Uuid => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for Uuid, got {v}"))?;
            uuid::Uuid::parse_str(s)
                .map(SqlValue::Uuid)
                .map_err(|e| format!("Uuid parse: {e}"))
        }
        FieldType::Json => Ok(SqlValue::Json(v.clone())),
        FieldType::Decimal => {
            let s = v
                .as_str()
                .map(str::to_owned)
                .or_else(|| v.as_f64().map(|n| n.to_string()))
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .ok_or_else(|| format!("expected number/string for Decimal, got {v}"))?;
            s.parse::<rust_decimal::Decimal>()
                .map(SqlValue::Decimal)
                .map_err(|e| format!("Decimal parse: {e}"))
        }
        FieldType::Binary => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected lowercase-hex string for Binary, got {v}"))?;
            if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Binary: not a valid lowercase-hex string".into());
            }
            let bytes: Vec<u8> = s
                .as_bytes()
                .chunks_exact(2)
                .map(|c| {
                    let h = (c[0] as char).to_digit(16).unwrap_or(0) as u8;
                    let l = (c[1] as char).to_digit(16).unwrap_or(0) as u8;
                    (h << 4) | l
                })
                .collect();
            Ok(SqlValue::Binary(bytes))
        }
        // #341 — PG array column from a fixture JSON array.
        FieldType::Array(elem) => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("expected JSON array for Array column, got {v}"))?;
            let items = arr
                .iter()
                .map(|el| match elem {
                    crate::core::ArrayElem::Text => el
                        .as_str()
                        .map(|s| SqlValue::String(s.to_owned()))
                        .ok_or_else(|| format!("expected string array element, got {el}")),
                    crate::core::ArrayElem::Int => el
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok())
                        .map(SqlValue::I32)
                        .ok_or_else(|| format!("expected i32 array element, got {el}")),
                    crate::core::ArrayElem::BigInt => el
                        .as_i64()
                        .map(SqlValue::I64)
                        .ok_or_else(|| format!("expected i64 array element, got {el}")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SqlValue::Array(items))
        }
        // #343 — PG range column from a fixture: a range-literal string
        // (`"[1,10)"`), bound as-is and implicit-cast by PG.
        FieldType::Range(_) => v
            .as_str()
            .map(|s| SqlValue::RangeLiteral(s.to_owned()))
            .ok_or_else(|| format!("expected range-literal string for Range column, got {v}")),
        // #342 — PG hstore column from a fixture: a JSON object whose
        // values are strings or null.
        FieldType::HStore => {
            let obj = v
                .as_object()
                .ok_or_else(|| format!("expected JSON object for HStore column, got {v}"))?;
            let pairs = obj
                .iter()
                .map(|(k, val)| match val {
                    serde_json::Value::String(s) => Ok((k.clone(), Some(s.clone()))),
                    serde_json::Value::Null => Ok((k.clone(), None)),
                    other => Err(format!("hstore value must be string or null, got {other}")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SqlValue::HStore(pairs))
        }
        // #824 — pgvector column from a fixture: a JSON array of numbers.
        FieldType::Vector(_) => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("expected JSON array for Vector column, got {v}"))?;
            let items = arr
                .iter()
                .map(|el| {
                    el.as_f64()
                        .map(|n| n as f32)
                        .ok_or_else(|| format!("expected numeric vector element, got {el}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SqlValue::Vector(items))
        }
        // #443 — PostGIS geometry from a fixture: a JSON Point object
        // (`{"x": .., "y": .., "srid": ..}`; `srid` defaults to 4326).
        FieldType::Geometry(_) => {
            let p: crate::sql::Point = serde_json::from_value(v.clone())
                .map_err(|e| format!("expected JSON {{x, y, srid?}} for geometry column: {e}"))?;
            Ok(p.into())
        }
    }
}
