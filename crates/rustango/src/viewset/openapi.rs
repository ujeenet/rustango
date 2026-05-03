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
//! let posts_vs = ViewSet::for_model(Post::SCHEMA)
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
use crate::core::FieldType;

impl ViewSet {
    /// Generate the OpenAPI path items for this viewset.
    ///
    /// Returns `(path, PathItem)` pairs for the collection (`/{prefix}`) and
    /// the item (`/{prefix}/{pk}`) routes. Skips create/update/destroy when
    /// `read_only()` was called.
    ///
    /// `item_schema_ref` is the name of the schema you registered in the
    /// spec's `components.schemas` (typically the model name, e.g. `"Post"`).
    /// Both create/update bodies and retrieve responses reference it.
    #[must_use]
    pub fn openapi_paths(
        &self,
        prefix: &str,
        item_schema_ref: &str,
    ) -> Vec<(String, PathItem)> {
        let prefix = prefix.trim_end_matches('/').to_owned();
        let collection_path = prefix.clone();
        let item_path = format!("{prefix}/{{pk}}");
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
        p = p.get(list_op);

        // -------- create (POST)
        if !self.read_only {
            let create_op = Operation::new()
                .summary(format!("Create {tag}"))
                .operation_id(format!("create_{}", snake(tag)))
                .tag(tag)
                .request_body(RequestBody::json(Schema::ref_(item_ref)))
                .response(
                    "201",
                    Response::new("created").json_content(Schema::ref_(item_ref)),
                )
                .response("400", Response::new("validation error"));
            p = p.post(create_op);
        }

        p
    }

    fn item_path_item(&self, tag: &str, item_ref: &str) -> PathItem {
        let mut p = PathItem::new().parameter(self.pk_path_param());

        // -------- retrieve (GET)
        let retrieve_op = Operation::new()
            .summary(format!("Retrieve {tag}"))
            .operation_id(format!("retrieve_{}", snake(tag)))
            .tag(tag)
            .response("200", Response::new("found").json_content(Schema::ref_(item_ref)))
            .response("404", Response::new("not found"));
        p = p.get(retrieve_op);

        if !self.read_only {
            let update_op = Operation::new()
                .summary(format!("Replace {tag}"))
                .operation_id(format!("update_{}", snake(tag)))
                .tag(tag)
                .request_body(RequestBody::json(Schema::ref_(item_ref)))
                .response("200", Response::new("updated").json_content(Schema::ref_(item_ref)))
                .response("404", Response::new("not found"));
            p = p.put(update_op);

            let patch_op = Operation::new()
                .summary(format!("Partially update {tag}"))
                .operation_id(format!("partial_update_{}", snake(tag)))
                .tag(tag)
                .request_body(RequestBody::json(Schema::ref_(item_ref)))
                .response("200", Response::new("updated").json_content(Schema::ref_(item_ref)))
                .response("404", Response::new("not found"));
            p = p.patch(patch_op);

            let destroy_op = Operation::new()
                .summary(format!("Delete {tag}"))
                .operation_id(format!("destroy_{}", snake(tag)))
                .tag(tag)
                .response("204", Response::new("deleted"))
                .response("404", Response::new("not found"));
            p = p.delete(destroy_op);
        }

        p
    }

    fn pk_path_param(&self) -> Parameter {
        let pk_field = self.schema.primary_key();
        let schema = pk_field.map_or_else(Schema::integer, |f| field_type_to_schema(f.ty));
        let name = pk_field.map_or("pk", |f| f.name);
        Parameter::path("pk", schema).description(format!("Primary key ({name})"))
    }

    fn list_query_params(&self) -> Vec<Parameter> {
        let mut out = Vec::new();
        // Pagination params
        match self.pagination {
            PaginationStyle::PageNumber => {
                out.push(
                    Parameter::query("page", Schema::int32())
                        .description("1-based page number"),
                );
                out.push(
                    Parameter::query("page_size", Schema::int32())
                        .description("Items per page (max 1000)"),
                );
            }
            PaginationStyle::Cursor { .. } => {
                out.push(
                    Parameter::query("cursor", Schema::string())
                        .description("Opaque token from the previous response's `next` field"),
                );
                out.push(
                    Parameter::query("page_size", Schema::int32())
                        .description("Items per page (max 1000)"),
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
        // Ordering (only when default_ordering is configured — otherwise sort
        // is always default and irrelevant).
        if !self.default_ordering.is_empty() {
            out.push(
                Parameter::query("ordering", Schema::string())
                    .description("Comma-separated field names; prefix `-` for DESC"),
            );
        }
        // One filter param per filter_field. We don't enumerate all the
        // Django-style lookups individually — the description tells the
        // reader they're available.
        for f in &self.filter_fields {
            let field_schema = self
                .schema
                .field(f)
                .map_or_else(Schema::string, |fs| field_type_to_schema(fs.ty));
            out.push(
                Parameter::query(f.clone(), field_schema)
                    .description(format!(
                        "Exact filter. Lookups: `{f}__gt`, `__gte`, `__lt`, `__lte`, \
                         `__ne`, `__in`, `__not_in`, `__contains`, `__icontains`, \
                         `__startswith`, `__istartswith`, `__endswith`, `__iendswith`, \
                         `__isnull`."
                    )),
            );
        }
        out
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
        }
    }
}

fn field_type_to_schema(t: FieldType) -> Schema {
    match t {
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
        FieldType::Uuid => Schema::uuid(),
        FieldType::Json => Schema::default(), // free-form
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
            unique: false,
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
            unique: false,
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
            unique: false,
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
                unique: false,
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
                unique: false,
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
                unique: false,
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
            m2m: &[],
            composite_relations: &[],
        };
        &MS
    }

    fn vs() -> ViewSet {
        ViewSet::for_model(schema())
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
        assert_eq!(resp["properties"]["next"]["nullable"], true);
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
    fn create_body_and_response_reference_item_schema() {
        let paths = vs().openapi_paths("/api/posts", "Post");
        let coll = &paths.iter().find(|(p, _)| p == "/api/posts").unwrap().1;
        let v = serde_json::to_value(coll.post.as_ref().unwrap()).unwrap();
        assert_eq!(
            v["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Post"
        );
        assert_eq!(
            v["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Post"
        );
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
}
