//! ViewSet → OpenAPI: auto-generate path items for the 5 standard CRUD
//! routes a [`ViewSet`] mounts.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::openapi::{OpenApiSpec, Schema};
//! use rustango::serializer::ModelSerializer;
//! use rustango::viewset::ViewSet;
//!
//! let posts_vs = ViewSet::for_model(Post::SCHEMA, &["id", "title", "body"])
//!     .filter_fields(&["author_id"])
//!     .search_fields(&["title", "body"]);
//!
//! let mut spec = OpenApiSpec::new("Blog API", "1.0")
//!     .add_schema("Post", PostSerializer::openapi_schema());
//!
//! for (path, item) in posts_vs.openapi_paths("/api/posts", "Post") {
//!     spec = spec.add_path(path, item);
//! }
//! ```

use crate::openapi::{Operation, Parameter, PathItem, RequestBody, Response, Schema};

use super::{PaginationStyle, ViewSet};
use crate::core::{FieldType, WriteKind};
use crate::forms::{absent_is_missing, absent_takes_default, Encoding};

/// A write that hits a unique key answers 409 (`write_failure`, #2164).
fn conflict() -> Response {
    Response::new("conflicts with an existing row")
}

/// `update_inner` answers 204 when the committed row no longer matches the caller's scope.
fn moved_out_of_scope() -> Response {
    Response::new("updated; the row is now outside your scope")
}

/// `extract_create_body` reads JSON, and anything else as urlencoded.
fn write_body(schema: Schema) -> RequestBody {
    RequestBody::json(schema.clone()).and_content("application/x-www-form-urlencoded", schema)
}

/// `BodyError::Bad` and a field value that does not parse.
fn malformed_body() -> Response {
    Response::new("malformed body or a value that does not parse")
}

/// A serializer or field-rule refusal (`validation_failed`).
fn validation_failed() -> Response {
    Response::new("validation failed; `details` maps each field to its errors")
}

/// A path pk that does not parse as the key's type (`parse_pk_or_400`).
fn malformed_pk() -> Response {
    Response::new("malformed primary key")
}

/// `extract_create_body`'s cap, or an outer body limit.
fn body_too_large() -> Response {
    Response::new("request body too large")
}

/// `run_list` refuses an unknown lookup, a bad value, an oversized IN list or a bad cursor.
fn bad_list_criteria() -> Response {
    Response::new("bad filter lookup or value, IN list too long, or invalid cursor")
}

/// Which write a request schema describes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Create,
    Replace,
    /// Every key optional.
    Patch,
}

impl ViewSet {
    /// Generate the OpenAPI path items for this viewset.
    ///
    /// Returns `(path, PathItem)` pairs for the collection (`/{prefix}`) and
    /// the item (`/{prefix}/{pk}`) routes. Skips create/update/destroy when
    /// `read_only()` was called.
    ///
    /// `item_schema_ref` is the name of the schema you registered in the
    /// spec's `components.schemas` (typically the model name, e.g. `"Post"`).
    /// Responses reference it. Request bodies are inlined from the
    /// fields the ViewSet writes, so `Auto` ids and read-only fields are
    /// left out, and a PATCH body requires nothing. Filter-backend pins are
    /// per request, so a pinned field still shows (the server ignores it).
    #[must_use]
    pub fn openapi_paths(&self, prefix: &str, item_schema_ref: &str) -> Vec<(String, PathItem)> {
        let prefix = prefix.trim_end_matches('/').to_owned();
        let collection_path = prefix.clone();
        let item_path = format!("{prefix}/{{{}}}", self.pk_param_name());
        let tag = item_schema_ref.to_owned();

        let collection = self.collection_path_item(&tag, item_schema_ref);
        let item = self.item_path_item(&tag, item_schema_ref);

        vec![(collection_path, collection), (item_path, item)]
    }

    fn collection_path_item(&self, tag: &str, item_ref: &str) -> PathItem {
        let mut p = PathItem::new();

        // -------- list (GET)
        let mut list_op = Operation::new()
            .summary(format!("List {tag}"))
            .operation_id(format!("list_{}", snake(tag)))
            .tag(tag)
            .response(
                "200",
                Response::new("paginated list").json_content(self.list_response_schema(item_ref)),
            );
        for param in self.list_query_params() {
            list_op = list_op.parameter(param);
        }
        if self.list_can_refuse() {
            list_op = list_op.response("400", bad_list_criteria());
        }
        p = p.get(self.gated(list_op, &self.perms.list, "list"));

        // -------- create (POST)
        if !self.read_only {
            // The schemas stay one row, so clients built from the spec keep
            // working; the bulk array form is described in prose until 0.62.0.
            let one = self.request_schema(Body::Create);
            let body = RequestBody::json(one.clone())
                .and_content("application/x-www-form-urlencoded", one);
            let created = Schema::ref_(item_ref);
            let create_op = Operation::new()
                .summary(format!("Create {tag}"))
                .description(format!(
                    "A JSON array of these bodies creates up to {} rows in one transaction \
                     and answers an array of rows in the same order, `null` for a row \
                     outside your scope.",
                    self.max_bulk_create
                ))
                .operation_id(format!("create_{}", snake(tag)))
                .tag(tag)
                .request_body(body)
                .response(
                    "201",
                    // `create_one` answers a bare 201 when the read-back is scoped out;
                    // `fetch_many_by_pk_scoped` puts `null` in that row's place.
                    Response::new(
                        "created; empty body when the new row is outside your scope, \
                         and `null` for each such row of a bulk create",
                    )
                    .json_content(created),
                )
                .response("400", malformed_body())
                .response("409", conflict())
                .response(
                    "413",
                    Response::new(format!(
                        "request body too large, or a bulk create over {} rows or over \
                         the create throttle",
                        self.max_bulk_create
                    )),
                )
                .response("422", validation_failed());
            p = p.post(self.gated(create_op, &self.perms.create, "create"));
        }

        // -------- query (QUERY, RFC 10008)
        //
        // Gated exactly as the route is (`viewset/mod.rs` mounts
        // `handle_query` under `admin`, because the routing shim
        // lives in the admin-gated `http_query`). Emitting this
        // unconditionally would advertise an operation a
        // `tenancy`-only build does not serve — the inverse of the
        // bug being fixed.
        //
        // The ViewSet has served QUERY since #1112 and the spec never
        // mentioned it, so every generated SDK, gateway and contract
        // test was blind to it (#1401).
        #[cfg(feature = "admin")]
        if self.openapi_query {
            // `parse_query_body_params` collapses the body to
            // `HashMap<String, String>` — scalars stringified, arrays
            // comma-joined — so every criterion is a string whatever
            // the GET parameter's type is.
            let mut criteria = Schema::object();
            for param in self.list_query_params() {
                criteria = criteria.property(param.name.clone(), Schema::string());
            }
            let query_op = Operation::new()
                .summary(format!("Query {tag}"))
                .description(
                    "Same filtered list as GET, with the criteria in the body instead \
                     of the querystring — for filters too long or too structured for a \
                     URL. Values are read as strings: a JSON array is comma-joined, so \
                     `{\"status__in\": [\"a\", \"b\"]}` matches `?status__in=a,b`.",
                )
                .operation_id(format!("query_{}", snake(tag)))
                .tag(tag)
                .request_body(
                    RequestBody::json(criteria.clone())
                        .and_content("application/x-www-form-urlencoded", criteria)
                        .description("Filter criteria. An empty body lists everything."),
                )
                .response(
                    "200",
                    Response::new("paginated list")
                        .json_content(self.list_response_schema(item_ref)),
                )
                .response(
                    "400",
                    Response::new("malformed urlencoded body, bad filter value or invalid cursor"),
                )
                .response("413", Response::new("body larger than 1 MiB"))
                .response(
                    "415",
                    Response::new("content type is neither JSON nor urlencoded"),
                )
                .response(
                    "422",
                    Response::new("body is not a JSON object of criteria"),
                );
            p = p.query(self.gated(query_op, &self.perms.list, "list"));
        }

        p
    }

    fn item_path_item(&self, tag: &str, item_ref: &str) -> PathItem {
        let mut p = PathItem::new().parameter(self.pk_path_param());

        // -------- retrieve (GET)
        let mut retrieve_op = Operation::new()
            .summary(format!("Retrieve {tag}"))
            .operation_id(format!("retrieve_{}", snake(tag)))
            .tag(tag)
            .response(
                "200",
                Response::new("found").json_content(Schema::ref_(item_ref)),
            )
            .response("404", Response::new("not found"));
        if self.pk_can_be_malformed() {
            retrieve_op = retrieve_op.response("400", malformed_pk());
        }
        p = p.get(self.gated(retrieve_op, &self.perms.retrieve, "retrieve"));
        // PUT/PATCH share one 400 for the path pk and the body.
        let bad_write = if self.pk_can_be_malformed() {
            Response::new("malformed primary key or body, or a value that does not parse")
        } else {
            malformed_body()
        };

        if !self.read_only {
            let update_op = Operation::new()
                .summary(format!("Replace {tag}"))
                .operation_id(format!("update_{}", snake(tag)))
                .tag(tag)
                .request_body(write_body(self.request_schema(Body::Replace)))
                .response(
                    "200",
                    Response::new("updated").json_content(Schema::ref_(item_ref)),
                )
                .response("204", moved_out_of_scope())
                .response("400", bad_write.clone())
                .response("404", Response::new("not found"))
                .response("409", conflict())
                .response("413", body_too_large())
                .response("422", validation_failed());
            p = p.put(self.gated(update_op, &self.perms.update, "update"));

            let patch_op = Operation::new()
                .summary(format!("Partially update {tag}"))
                .operation_id(format!("partial_update_{}", snake(tag)))
                .tag(tag)
                .request_body(write_body(self.request_schema(Body::Patch)))
                .response(
                    "200",
                    Response::new("updated").json_content(Schema::ref_(item_ref)),
                )
                .response("204", moved_out_of_scope())
                .response("400", bad_write)
                .response("404", Response::new("not found"))
                .response("409", conflict())
                .response("413", body_too_large())
                .response("422", validation_failed());
            p = p.patch(self.gated(patch_op, &self.perms.update, "update"));

            let mut destroy_op = Operation::new()
                .summary(format!("Delete {tag}"))
                .operation_id(format!("destroy_{}", snake(tag)))
                .tag(tag)
                .response("204", Response::new("deleted"))
                .response("404", Response::new("not found"))
                .response("409", Response::new("still referenced by other rows"));
            if self.pk_can_be_malformed() {
                destroy_op = destroy_op.response("400", malformed_pk());
            }
            p = p.delete(self.gated(destroy_op, &self.perms.destroy, "destroy"));
        }

        p
    }

    /// What `enter()` answers before the handler: 401/403 from the
    /// action's codenames, 429 from its throttle.
    fn gated(&self, mut op: Operation, codenames: &[String], action: &str) -> Operation {
        if !codenames.is_empty() {
            // Without tenancy there is no principal, so `check_perm` only denies.
            #[cfg(feature = "tenancy")]
            {
                op = op.response("401", Response::new("authentication required"));
            }
            op = op.response("403", Response::new("permission denied"));
        }
        if self.throttle.for_action(action).is_some() {
            op = op.response(
                "429",
                Response::new("throttled; retry after `Retry-After` seconds")
                    .header("Retry-After", Schema::integer()),
            );
        }
        op
    }

    /// `parse_pk_string` takes any text for a `String` key.
    fn pk_can_be_malformed(&self) -> bool {
        self.schema
            .primary_key()
            .is_some_and(|f| f.ty != FieldType::String)
    }

    /// The body a write accepts, from the same field set the handlers
    /// read: no server-set columns, `required` only where absence is a 400 (#1922).
    fn request_schema(&self, body: Body) -> Schema {
        let kind = match body {
            Body::Create => WriteKind::Insert,
            Body::Replace | Body::Patch => WriteKind::Update,
        };
        let mut s = Schema::object();
        let mut required = Vec::new();
        for f in self.body_fields() {
            if !f.accepts_input(kind) {
                continue;
            }
            let key = self.body_key(f.name);
            let prop = field_to_schema(f);
            s = s.property(key, if f.nullable { prop.nullable() } else { prop });
            let defaulted = body == Body::Create && absent_takes_default(f, Encoding::Json);
            if body != Body::Patch && absent_is_missing(f) && !defaulted {
                required.push(key);
            }
        }
        s.required(required)
    }

    fn pk_path_param(&self) -> Parameter {
        let pk_field = self.schema.primary_key();
        let schema = pk_field.map_or_else(Schema::integer, |f| field_type_to_schema(f.ty));
        let name = pk_field.map_or("pk", |f| f.name);
        // The parameter must be spelled the same as the route capture, which
        // `ViewSet::pk_param` can rename (#1194) — otherwise the generated
        // spec documents a path variable the server never binds.
        Parameter::path(self.pk_param_name(), schema).description(format!("Primary key ({name})"))
    }

    fn list_query_params(&self) -> Vec<Parameter> {
        let mut out = Vec::new();
        // `run_list` clamps to `max_page_size` rather than refusing, so no `maximum`.
        let cap = self.max_page_size;
        let default = self.default_page_size.clamp(1, cap);
        let page_size = || {
            Parameter::query("page_size", Schema::int32()).description(format!(
                "Items per page (default {default}; values above {cap} are clamped to {cap})"
            ))
        };
        // Pagination params
        match self.pagination {
            PaginationStyle::PageNumber => {
                out.push(
                    Parameter::query("page", Schema::int32()).description("1-based page number"),
                );
                out.push(page_size());
            }
            PaginationStyle::Cursor { .. } => {
                out.push(
                    Parameter::query("cursor", Schema::string())
                        .description("Opaque token from the previous response's `next` field"),
                );
                out.push(page_size());
            }
            PaginationStyle::LimitOffset => {
                out.push(
                    Parameter::query("limit", Schema::int32()).description(format!(
                        "Items to return (default {default}; values above {cap} are \
                         clamped to {cap})"
                    )),
                );
                out.push(
                    Parameter::query("offset", Schema::int32())
                        .description("Rows to skip before the window"),
                );
            }
        }
        // Search
        if !self.search_fields.is_empty() {
            out.push(
                Parameter::query("search", Schema::string()).description(format!(
                    "Full-text search across: {}",
                    self.search_fields.join(", ")
                )),
            );
        }
        // Ordering: `run_list` always reads it from the same allow-list;
        // cursor pages always sort by the cursor column.
        let allowed: Vec<String> = self
            .ordering_allowlist()
            .into_iter()
            .filter(|n| self.schema.field(n).is_some())
            .collect();
        let cursor = matches!(self.pagination, PaginationStyle::Cursor { .. });
        if !cursor && !allowed.is_empty() {
            out.push(
                Parameter::query("ordering", Schema::string()).description(format!(
                    "Comma-separated field names from: {}. Prefix `-` for DESC. \
                     Unknown / off-whitelist names are silently dropped.",
                    allowed.join(", "),
                )),
            );
        }
        // One filter param per filter_field. The `__`-suffixed lookups
        // are not listed as separate params — the description names
        // them instead.
        for f in &self.filter_fields {
            let field_schema = self
                .schema
                .field(f)
                .map_or_else(Schema::string, field_to_schema);
            out.push(
                Parameter::query(f.clone(), field_schema).description(format!(
                    "Exact filter. Lookups: `{f}__iexact`, `__gt`, `__gte`, `__lt`, \
                         `__lte`, `__ne`, `__in`, `__not_in`, `__range`, `__contains`, \
                         `__icontains`, `__startswith`, `__istartswith`, `__endswith`, \
                         `__iendswith`, `__isnull`, and date parts (`__year`, \
                         `__date__gte`, ...). An unknown lookup or bad value is a 400."
                )),
            );
        }
        out
    }

    /// Only filter params and a cursor token can make a GET list a 400.
    fn list_can_refuse(&self) -> bool {
        !self.filter_fields.is_empty() || matches!(self.pagination, PaginationStyle::Cursor { .. })
    }

    fn list_response_schema(&self, item_ref: &str) -> Schema {
        let items = Schema::array_of(Schema::ref_(item_ref));
        match self.pagination {
            PaginationStyle::PageNumber => Schema::object()
                .property("count", Schema::integer())
                .property("page", Schema::int32())
                .property("page_size", Schema::int32())
                .property("last_page", Schema::int32())
                .property("results", items)
                .required(["count", "page", "page_size", "last_page", "results"]),
            PaginationStyle::Cursor { .. } => Schema::object()
                .property("page_size", Schema::int32())
                .property("next", Schema::string().nullable())
                .property("results", items)
                .required(["page_size", "results"]),
            PaginationStyle::LimitOffset => Schema::object()
                .property("count", Schema::integer())
                .property("limit", Schema::int32())
                .property("offset", Schema::int32())
                .property("results", items)
                .required(["count", "limit", "offset", "results"]),
        }
    }
}

/// The type schema plus the limits a write enforces (422 otherwise).
fn field_to_schema(f: &crate::core::FieldSchema) -> Schema {
    field_type_to_schema(f.ty).field_limits(Some(f), None, None, None, None)
}

fn field_type_to_schema(t: FieldType) -> Schema {
    match t {
        FieldType::I16 => {
            // JSON Schema has no `int16`. Render as `int32` and clamp
            // via min/max so the schema still describes the actual
            // bounds Postgres / MySQL enforce.
            let mut s = Schema::int32();
            s.minimum = Some(f64::from(i16::MIN));
            s.maximum = Some(f64::from(i16::MAX));
            s
        }
        FieldType::I32 => Schema::int32(),
        FieldType::I64 => Schema::integer(),
        FieldType::F32 => {
            let mut s = Schema::number();
            s.format = Some("float".into());
            s
        }
        FieldType::F64 => {
            let mut s = Schema::number();
            s.format = Some("double".into());
            s
        }
        FieldType::Bool => Schema::boolean(),
        FieldType::String => Schema::string(),
        FieldType::DateTime => Schema::datetime(),
        FieldType::Date => Schema::date(),
        FieldType::Time => Schema::time(),
        FieldType::Uuid => Schema::uuid(),
        FieldType::Json => Schema::default(), // free-form
        FieldType::Decimal => Schema::decimal(),
        // Forms read and `row_to_json` writes hex; `Schema::binary` would say base64.
        FieldType::Binary => {
            let mut s = Schema::string().description("hex-encoded bytes, even length");
            s.format = Some("hex".into());
            s
        }
        // #341 — PG array column: `type: array` with element-typed items.
        FieldType::Array(elem) => {
            use crate::core::ArrayElem;
            let items = match elem {
                ArrayElem::Text => Schema::string(),
                ArrayElem::Int => Schema::int32(),
                ArrayElem::BigInt => Schema::integer(),
            };
            Schema::array_of(items)
        }
        // #343 — PG range column: serialized as a range-literal string
        // (`"[1,10)"`), so a `string` schema is the honest shape.
        FieldType::Range(_) => Schema::string(),
        // #342 — PG hstore: a string→string map → `object` with
        // string-typed additionalProperties.
        FieldType::HStore => Schema {
            type_: Some("object".into()),
            additional_properties: Some(Box::new(Schema::string())),
            ..Default::default()
        },
        // #824 — pgvector embedding: a JSON array of numbers.
        FieldType::Vector(_) => Schema::array_of(Schema::number()),
        // #443 — PostGIS Point: a JSON object `{ x, y, srid }`.
        FieldType::Geometry(_) => {
            <crate::sql::Point as crate::openapi::OpenApiSchema>::openapi_schema()
        }
    }
}

/// "PostThing" → "post_thing"; "post" → "post".
fn snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{FieldSchema, FieldType, ModelSchema};

    fn id_field() -> FieldSchema {
        FieldSchema {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: true,
            auto_now: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
            cast: None,
        }
    }

    fn title_field() -> FieldSchema {
        FieldSchema {
            name: "title",
            column: "title",
            ty: FieldType::String,
            nullable: false,
            primary_key: false,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
            auto_now: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
            cast: None,
        }
    }

    fn author_id_field() -> FieldSchema {
        FieldSchema {
            name: "author_id",
            column: "author_id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: false,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
            auto_now: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
            cast: None,
        }
    }

    fn schema() -> &'static ModelSchema {
        static FIELDS: &[FieldSchema] = &[
            FieldSchema {
                name: "id",
                column: "id",
                ty: FieldType::I64,
                nullable: false,
                primary_key: true,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: true,
                auto_now: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                case_insensitive: false,
                fk_on_delete: None,
                validators: &[],
                cast: None,
            },
            FieldSchema {
                name: "title",
                column: "title",
                ty: FieldType::String,
                nullable: false,
                primary_key: false,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: false,
                auto_now: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                case_insensitive: false,
                fk_on_delete: None,
                validators: &[],
                cast: None,
            },
            FieldSchema {
                name: "author_id",
                column: "author_id",
                ty: FieldType::I64,
                nullable: false,
                primary_key: false,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: false,
                auto_now: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                case_insensitive: false,
                fk_on_delete: None,
                validators: &[],
                cast: None,
            },
        ];
        // Suppress unused warnings on field helpers if we keep them.
        let _ = (id_field(), title_field(), author_id_field());
        static MS: ModelSchema = ModelSchema {
            name: "Post",
            table: "posts",
            fields: FIELDS,
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            audit_track: None,
            permissions: false,
            indexes: &[],
            check_constraints: &[],
            exclusion_constraints: &[],
            default_permissions: &[],
            m2m: &[],
            composite_relations: &[],
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        };
        &MS
    }

    fn vs() -> ViewSet {
        ViewSet::for_model(schema(), &["id", "title", "author_id"])
            .filter_fields(&["author_id"])
            .search_fields(&["title"])
            .ordering(&[("id", true)])
    }

    #[test]
    fn paths_returns_collection_and_item_paths() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let names: Vec<&str> = paths.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, vec!["/api/posts", "/api/posts/{pk}"]);
    }

    #[test]
    fn collection_has_get_and_post_for_default_viewset() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let item = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        assert!(item.get.is_some(), "list");
        assert!(item.post.is_some(), "create");
    }

    #[test]
    fn read_only_omits_write_methods() {
        let vs = vs().read_only();
        let paths = vs.openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        assert!(coll.get.is_some());
        assert!(coll.post.is_none());
        let item = &paths
            .iter()
            .find(|(p, _)| p == "/api/posts/{pk}")
            .unwrap()
            .1;
        assert!(item.get.is_some());
        assert!(item.put.is_none());
        assert!(item.patch.is_none());
        assert!(item.delete.is_none());
    }

    #[test]
    fn list_response_paginated_shape() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let v = serde_json::to_value(coll.get.as_ref().unwrap()).unwrap();
        let resp = &v["responses"]["200"]["content"]["application/json"]["schema"];
        assert_eq!(resp["type"], "object");
        assert_eq!(resp["properties"]["count"]["type"], "integer");
        assert_eq!(resp["properties"]["results"]["type"], "array");
        assert_eq!(
            resp["properties"]["results"]["items"]["$ref"],
            "#/components/schemas/Post"
        );
    }

    #[test]
    fn cursor_pagination_changes_list_shape_and_params() {
        let vs = vs().cursor_pagination("id");
        let paths = vs.openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let v = serde_json::to_value(coll.get.as_ref().unwrap()).unwrap();
        // params should include `cursor`, not `page`
        let param_names: Vec<&str> = v["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(param_names.contains(&"cursor"));
        assert!(!param_names.contains(&"page"));
        // response: { page_size, next, results } — no count
        let resp = &v["responses"]["200"]["content"]["application/json"]["schema"];
        assert!(resp["properties"]["count"].is_null());
        assert_eq!(
            resp["properties"]["next"]["type"],
            serde_json::json!(["string", "null"])
        );
    }

    #[test]
    fn pk_param_uses_primary_key_field_type() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let item = &paths
            .iter()
            .find(|(p, _)| p == "/api/posts/{pk}")
            .unwrap()
            .1;
        let v = serde_json::to_value(item).unwrap();
        let pk = &v["parameters"][0];
        assert_eq!(pk["name"], "pk");
        assert_eq!(pk["in"], "path");
        assert_eq!(pk["required"], true);
        assert_eq!(pk["schema"]["type"], "integer");
        assert_eq!(pk["schema"]["format"], "int64");
    }

    #[test]
    fn create_response_references_item_schema() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let v = serde_json::to_value(coll.post.as_ref().unwrap()).unwrap();
        assert_eq!(
            v["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Post"
        );
    }

    /// The schemas stay one row; the bulk array form is in the description.
    #[test]
    fn create_documents_the_bulk_shape() {
        let paths = vs().max_bulk_create(50).openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let v = serde_json::to_value(coll.post.as_ref().unwrap()).unwrap();
        let content = &v["requestBody"]["content"];
        assert!(
            content["application/json"]["schema"]["properties"].is_object(),
            "{v}"
        );
        assert_eq!(
            content["application/x-www-form-urlencoded"]["schema"],
            content["application/json"]["schema"]
        );
        assert_eq!(
            v["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Post"
        );
        let about = v["description"].as_str().unwrap();
        assert!(
            about.contains("JSON array") && about.contains("50 rows"),
            "{about}"
        );
        let too_large = v["responses"]["413"]["description"].as_str().unwrap();
        assert!(too_large.contains("50 rows"), "{too_large}");
        for (name, op) in [("PUT", &paths[1].1.put), ("PATCH", &paths[1].1.patch)] {
            let v = serde_json::to_value(op.as_ref().unwrap()).unwrap();
            assert!(v["responses"]["413"].is_object(), "{name}: {v}");
        }
    }

    /// `parse_pk_or_400` refuses a pk that does not parse, on retrieve and DELETE too.
    #[test]
    fn item_reads_and_deletes_document_a_malformed_pk() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let item = &paths[1].1;
        for (name, op) in [("GET", &item.get), ("DELETE", &item.delete)] {
            let v = serde_json::to_value(op.as_ref().unwrap()).unwrap();
            assert_eq!(
                v["responses"]["400"]["description"], "malformed primary key",
                "{name}: {v}"
            );
        }
    }

    fn statuses(op: Option<&crate::openapi::Operation>) -> Vec<String> {
        op.unwrap().responses.keys().cloned().collect()
    }

    /// `enter()` answers 401/403 for an action with codenames, 429 when throttled.
    #[test]
    fn gated_actions_document_what_enter_returns() {
        let has = |op, code: &str| statuses(op).iter().any(|s| s == code);
        let paths = vs().openapi_paths("/api/posts", "Post");
        for code in ["401", "403", "429"] {
            assert!(!has(paths[0].1.get.as_ref(), code), "ungated list: {code}");
        }

        let perms = crate::viewset::ViewSetPerms {
            list: vec!["post.view".into()],
            ..Default::default()
        };
        let throttle = crate::viewset::ViewSetThrottle {
            destroy: Some(crate::viewset::ThrottleRule::new(5, 60)),
            ..Default::default()
        };
        let paths = vs()
            .permissions(perms)
            .throttle(throttle)
            .openapi_paths("/api/posts", "Post");
        let (coll, item) = (&paths[0].1, &paths[1].1);
        assert!(has(coll.get.as_ref(), "403"));
        assert_eq!(
            has(coll.get.as_ref(), "401"),
            cfg!(feature = "tenancy"),
            "401 needs a principal to authenticate"
        );
        assert!(!has(coll.post.as_ref(), "403"), "create has no codenames");
        assert!(!has(coll.get.as_ref(), "429"));
        let del = serde_json::to_value(item.delete.as_ref().unwrap()).unwrap();
        assert!(
            del["responses"]["429"]["headers"]["Retry-After"].is_object(),
            "{del}"
        );
        assert!(!has(item.get.as_ref(), "429"));
    }

    /// #2164: create and update map a unique violation to 409.
    #[test]
    fn writes_list_the_conflict_response() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let item = &paths
            .iter()
            .find(|(p, _)| p == "/api/posts/{pk}")
            .unwrap()
            .1;
        for (name, op) in [
            ("POST", &coll.post),
            ("PUT", &item.put),
            ("PATCH", &item.patch),
        ] {
            let v = serde_json::to_value(op.as_ref().unwrap()).unwrap();
            assert!(v["responses"]["409"].is_object(), "{name}: {v}");
        }
        // DELETE refuses a row other rows still point at.
        let v = serde_json::to_value(item.delete.as_ref().unwrap()).unwrap();
        assert!(v["responses"]["409"].is_object(), "DELETE: {v}");
    }

    /// #2207: PUT/PATCH answer 400 and 204, and create a bare 201, as the handlers do.
    #[test]
    fn writes_list_every_status_the_handlers_return() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let item = &paths
            .iter()
            .find(|(p, _)| p == "/api/posts/{pk}")
            .unwrap()
            .1;
        for (name, op) in [("PUT", &item.put), ("PATCH", &item.patch)] {
            let v = serde_json::to_value(op.as_ref().unwrap()).unwrap();
            assert!(v["responses"]["400"].is_object(), "{name}: {v}");
            assert!(v["responses"]["204"].is_object(), "{name}: {v}");
            assert!(v["responses"]["204"]["content"].is_null(), "{name}: {v}");
        }
        let v = serde_json::to_value(coll.post.as_ref().unwrap()).unwrap();
        let created = v["responses"]["201"]["description"].as_str().unwrap();
        assert!(created.contains("empty body"), "{created}");
    }

    /// The one-row body; a create's JSON body also takes an array of them.
    fn body_schema(op: Option<&crate::openapi::Operation>) -> serde_json::Value {
        let v = serde_json::to_value(op.unwrap()).unwrap();
        let s = &v["requestBody"]["content"]["application/json"]["schema"];
        match s["anyOf"].get(0) {
            Some(one) => one.clone(),
            None => s.clone(),
        }
    }

    /// #1922: the item schema as a request body required the `Auto` id
    /// and made PATCH require every field.
    #[test]
    fn write_bodies_leave_out_the_auto_id_and_patch_requires_nothing() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let item = &paths
            .iter()
            .find(|(p, _)| p == "/api/posts/{pk}")
            .unwrap()
            .1;
        let fields = serde_json::json!({"type": "object", "properties": {
            "title": {"type": "string"},
            "author_id": {"type": "integer", "format": "int64"},
        }});
        let mut full = fields.clone();
        full["required"] = serde_json::json!(["title", "author_id"]);
        assert_eq!(body_schema(coll.post.as_ref()), full, "POST");
        assert_eq!(body_schema(item.put.as_ref()), full, "PUT");
        assert_eq!(body_schema(item.patch.as_ref()), fields, "PATCH");
    }

    /// A body cannot set the soft-delete column, so the schema must not offer it.
    #[test]
    fn write_bodies_leave_out_the_soft_delete_column() {
        static FIELDS: [FieldSchema; 2] = [title_const(), deleted_at_const()];
        static MS: std::sync::OnceLock<ModelSchema> = std::sync::OnceLock::new();
        let ms = MS.get_or_init(|| ModelSchema {
            fields: &FIELDS,
            soft_delete_column: Some("deleted_at"),
            ..schema().clone()
        });
        let paths = ViewSet::for_model(ms, &["title", "deleted_at"]).openapi_paths("/p", "P");
        let item = &paths.iter().find(|(p, _)| p == "/p/{pk}").unwrap().1;
        let body = body_schema(item.patch.as_ref());
        assert!(body["properties"]["title"].is_object());
        assert!(body["properties"].get("deleted_at").is_none(), "{body}");
    }

    /// Choices, max_length and min/max reach the body and filter schemas.
    #[test]
    fn write_and_filter_schemas_carry_field_limits() {
        static FIELDS: [FieldSchema; 3] = [
            FieldSchema {
                name: "status",
                column: "status",
                nullable: true,
                max_length: Some(16),
                choices: Some(&[("draft", "Draft"), ("live", "Live")]),
                ..title_const()
            },
            FieldSchema {
                name: "rank",
                column: "rank",
                ty: FieldType::I32,
                min: Some(1),
                max: Some(5),
                ..title_const()
            },
            title_const(),
        ];
        static MS: std::sync::OnceLock<ModelSchema> = std::sync::OnceLock::new();
        let ms = MS.get_or_init(|| ModelSchema {
            fields: &FIELDS,
            ..schema().clone()
        });
        let paths = ViewSet::for_model(ms, &["status", "rank"])
            .filter_fields(&["status"])
            .openapi_paths("/p", "P");
        let coll = &paths.iter().find(|(p, _)| p == "/p").unwrap().1;
        let body = body_schema(coll.post.as_ref());
        let status = &body["properties"]["status"];
        assert_eq!(
            status["enum"],
            serde_json::json!(["draft", "live", null]),
            "{body}"
        );
        assert_eq!(status["maxLength"], 16, "{body}");
        assert_eq!(body["properties"]["rank"]["minimum"], 1.0, "{body}");
        assert_eq!(body["properties"]["rank"]["maximum"], 5.0, "{body}");
        let list = list_op_of(coll);
        let p = param(&list, "status").expect("status param");
        assert_eq!(
            p["schema"]["enum"],
            serde_json::json!(["draft", "live"]),
            "{p}"
        );
    }

    fn list_op_of(item: &crate::openapi::PathItem) -> serde_json::Value {
        serde_json::to_value(item.get.as_ref().unwrap()).unwrap()
    }

    const fn title_const() -> FieldSchema {
        FieldSchema {
            name: "title",
            column: "title",
            ty: FieldType::String,
            nullable: false,
            primary_key: false,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
            auto_now: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
            cast: None,
        }
    }

    const fn deleted_at_const() -> FieldSchema {
        FieldSchema {
            name: "deleted_at",
            column: "deleted_at",
            ty: FieldType::DateTime,
            nullable: true,
            ..title_const()
        }
    }

    #[test]
    fn filter_fields_emit_query_parameters() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let v = serde_json::to_value(coll.get.as_ref().unwrap()).unwrap();
        let names: Vec<&str> = v["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"author_id"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"ordering"));
    }

    #[test]
    fn operation_ids_are_snake_cased_per_action() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        assert_eq!(
            coll.get.as_ref().unwrap().operation_id.as_deref(),
            Some("list_post")
        );
        assert_eq!(
            coll.post.as_ref().unwrap().operation_id.as_deref(),
            Some("create_post")
        );
    }

    #[test]
    fn snake_lowers_camelcase() {
        assert_eq!(snake("Post"), "post");
        assert_eq!(snake("PostThing"), "post_thing");
        assert_eq!(snake("HTTPRequest"), "h_t_t_p_request");
        assert_eq!(snake("post"), "post");
    }

    /// #1401 — the ViewSet has mounted QUERY since #1112 and the spec
    /// never said so, so no generated client could reach it.
    /// The default must stay 3.1.0-compatible: the QUERY operation is
    /// opt-in because emitting it forces the whole document to 3.2.0,
    /// which a 3.1-pinned generator rejects outright.
    #[cfg(feature = "admin")]
    #[test]
    fn the_query_operation_is_off_by_default() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        assert!(
            coll.query.is_none(),
            "emitting this by default flips every ViewSet spec to 3.2.0 \
             without the caller asking — `getting_started_blog`'s own \
             openapi test caught exactly that",
        );

        let mut spec = crate::openapi::OpenApiSpec::new("t", "1");
        for (path, item) in vs().openapi_paths("/api/posts", "Post") {
            spec = spec.add_path(path, item);
        }
        assert_eq!(
            spec.openapi, "3.1.0",
            "the default document must stay 3.1.0"
        );
    }

    #[cfg(feature = "admin")]
    #[test]
    fn the_collection_advertises_the_query_operation_it_serves() {
        let paths = vs().openapi_query(true).openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;

        let q = coll
            .query
            .as_ref()
            .expect("the collection serves QUERY, so the spec must carry it");
        assert_eq!(q.operation_id.as_deref(), Some("query_post"));

        // Both content types the handler accepts, or a generated
        // client picks one and the other 415s.
        let body = q.request_body.as_ref().expect("QUERY takes a body");
        assert!(
            body.content.contains_key("application/json"),
            "{:?}",
            body.content.keys()
        );
        assert!(
            body.content
                .contains_key("application/x-www-form-urlencoded"),
            "{:?}",
            body.content.keys(),
        );

        // The criteria object must carry the same names as the GET
        // querystring — that equivalence is the whole point of the
        // operation, and it is what drifts if a filter is added to
        // one path only.
        let props = &body.content["application/json"].schema.properties;
        for param in vs().list_query_params() {
            assert!(
                props.contains_key(&param.name),
                "`{}` is a GET query param but not a QUERY body criterion: {:?}",
                param.name,
                props.keys().collect::<Vec<_>>(),
            );
        }
        assert!(!props.is_empty(), "criteria schema must not be empty");
    }

    fn list_op(vs: &ViewSet) -> serde_json::Value {
        let paths = vs.openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        serde_json::to_value(coll.get.as_ref().unwrap()).unwrap()
    }

    fn param<'a>(op: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
        op["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
    }

    /// The forms layer reads hex and `row_to_json` writes hex; `byte` means base64.
    #[test]
    fn binary_fields_are_documented_as_hex() {
        let v = serde_json::to_value(field_type_to_schema(FieldType::Binary)).unwrap();
        assert_eq!(v["type"], "string");
        assert_eq!(v["format"], "hex", "{v}");
    }

    /// The page cap is `max_page_size`, not a fixed 1000.
    #[test]
    fn page_size_params_name_the_configured_cap() {
        for (vs, name) in [
            (vs().max_page_size(250), "page_size"),
            (vs().max_page_size(250).cursor_pagination("id"), "page_size"),
            (vs().max_page_size(250).limit_offset_pagination(), "limit"),
        ] {
            let op = list_op(&vs);
            let desc = param(&op, name).unwrap()["description"].as_str().unwrap();
            assert!(
                desc.contains("250") && !desc.contains("1000"),
                "{name}: {desc}"
            );
        }
    }

    /// Cursor pages always sort by the cursor column.
    #[test]
    fn cursor_pagination_lists_no_ordering_param() {
        let op = list_op(&vs().cursor_pagination("id"));
        assert!(param(&op, "ordering").is_none(), "{op}");
    }

    /// `run_list` honours `?ordering=` over the rendered fields with no config at all.
    #[test]
    fn ordering_is_listed_without_any_ordering_config() {
        let op = list_op(&ViewSet::for_model(schema(), &["id", "title"]));
        let desc = param(&op, "ordering").expect("ordering")["description"]
            .as_str()
            .unwrap();
        assert!(desc.contains("id, title"), "{desc}");
        let none = list_op(&ViewSet::for_model(schema(), &["id"]).ordering_fields(&[]));
        assert!(
            param(&none, "ordering").is_none(),
            "empty allow-list sorts nothing"
        );
    }

    /// A bad filter value or cursor is a 400.
    #[test]
    fn list_documents_the_400_it_can_return() {
        assert!(list_op(&vs()).pointer("/responses/400").is_some());
        let plain = ViewSet::for_model(schema(), &["id"]);
        assert!(list_op(&plain.clone().cursor_pagination("id"))
            .pointer("/responses/400")
            .is_some());
        assert!(list_op(&plain).pointer("/responses/400").is_none());
    }

    /// Writes take urlencoded bodies too, and a field-rule refusal is a 422.
    #[test]
    fn writes_document_urlencoded_bodies_and_422() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let item = &paths
            .iter()
            .find(|(p, _)| p == "/api/posts/{pk}")
            .unwrap()
            .1;
        for (name, op) in [
            ("POST", &coll.post),
            ("PUT", &item.put),
            ("PATCH", &item.patch),
        ] {
            let v = serde_json::to_value(op.as_ref().unwrap()).unwrap();
            let content = &v["requestBody"]["content"];
            assert_eq!(
                content["application/x-www-form-urlencoded"]["schema"],
                body_schema(op.as_ref()),
                "{name}: {v}"
            );
            assert!(v["responses"]["422"].is_object(), "{name}: {v}");
        }
    }

    /// A malformed urlencoded QUERY body is a 400.
    #[cfg(feature = "admin")]
    #[test]
    fn the_query_operation_documents_400() {
        let paths = vs().openapi_query(true).openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let v = serde_json::to_value(coll.query.as_ref().unwrap()).unwrap();
        assert!(v["responses"]["400"].is_object(), "{v}");
    }

    /// The spec version has to follow the operation, or tooling
    /// rejects a 3.1.0 document containing a `query` field.
    #[cfg(feature = "admin")]
    #[test]
    fn a_spec_carrying_the_query_operation_declares_3_2_0() {
        let paths = vs().openapi_query(true).openapi_paths("/api/posts", "Post");
        let mut spec = crate::openapi::OpenApiSpec::new("t", "1");
        for (path, item) in paths {
            spec = spec.add_path(path, item);
        }
        assert_eq!(spec.openapi, "3.2.0");
    }
}
