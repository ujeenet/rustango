//! OpenAPI 3.1 spec builder + optional Swagger UI router.
//!
//! Two flavors of usage:
//!
//! 1. **Hand-build** the spec with [`OpenApiSpec::new`] + the `add_*`
//!    methods. Useful when your API is hand-rolled or you want full
//!    control over the wire format.
//! 2. **Generate** from [`Schema::for_serializer`] (when the
//!    `serializer` feature is on) — pulls field metadata from any
//!    `#[derive(Serializer)]` type. Future slice will auto-extract
//!    paths from ViewSet too.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::openapi::{OpenApiSpec, PathItem, Operation, Response, Schema};
//!
//! let spec = OpenApiSpec::new("My API", "1.0.0")
//!     .description("Customer-facing public API")
//!     .server("https://api.example.com", "Production")
//!     .add_schema("Post", Schema::object()
//!         .property("id", Schema::integer())
//!         .property("title", Schema::string())
//!         .required(["id", "title"]))
//!     .add_path("/posts", PathItem::new()
//!         .get(Operation::new()
//!             .summary("List posts")
//!             .response("200", Response::new("OK")
//!                 .json_content(Schema::array_of(Schema::ref_("Post"))))));
//!
//! // Mount /openapi.json + /docs (Swagger UI from a CDN):
//! # #[cfg(feature = "admin")]
//! let app = axum::Router::new().merge(rustango::openapi::router::openapi_router(spec));
//! ```

#[cfg(feature = "admin")]
pub mod router;

use indexmap::IndexMap;
use serde::Serialize;

// =====================================================================
// Top-level spec
// =====================================================================

#[derive(Debug, Clone, Serialize)]
pub struct OpenApiSpec {
    pub openapi: String,
    pub info: Info,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<Server>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub paths: IndexMap<String, PathItem>,
    #[serde(skip_serializing_if = "Components::is_empty")]
    pub components: Components,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<Tag>,
    #[serde(skip_serializing_if = "Vec::is_empty", rename = "security")]
    pub security_requirements: Vec<IndexMap<String, Vec<String>>>,
}

impl OpenApiSpec {
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            openapi: "3.1.0".into(),
            info: Info {
                title: title.into(),
                version: version.into(),
                description: None,
                contact: None,
                license: None,
            },
            servers: Vec::new(),
            paths: IndexMap::new(),
            components: Components::default(),
            tags: Vec::new(),
            security_requirements: Vec::new(),
        }
    }

    #[must_use]
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.info.description = Some(d.into());
        self
    }

    #[must_use]
    pub fn contact(mut self, name: impl Into<String>, email: Option<String>) -> Self {
        self.info.contact = Some(Contact {
            name: Some(name.into()),
            email,
            url: None,
        });
        self
    }

    #[must_use]
    pub fn license(mut self, name: impl Into<String>, url: Option<String>) -> Self {
        self.info.license = Some(License {
            name: name.into(),
            url,
        });
        self
    }

    #[must_use]
    pub fn server(mut self, url: impl Into<String>, description: impl Into<String>) -> Self {
        self.servers.push(Server {
            url: url.into(),
            description: Some(description.into()),
        });
        self
    }

    #[must_use]
    pub fn add_path(mut self, path: impl Into<String>, item: PathItem) -> Self {
        self.paths.insert(path.into(), item);
        self
    }

    #[must_use]
    pub fn add_schema(mut self, name: impl Into<String>, schema: Schema) -> Self {
        self.components
            .schemas
            .insert(name.into(), schema);
        self
    }

    #[must_use]
    pub fn add_security_scheme(
        mut self,
        name: impl Into<String>,
        scheme: SecurityScheme,
    ) -> Self {
        self.components
            .security_schemes
            .insert(name.into(), scheme);
        self
    }

    /// Add a global security requirement — every operation requires this
    /// scheme unless the operation overrides with its own `security`.
    #[must_use]
    pub fn require_security(
        mut self,
        scheme_name: impl Into<String>,
        scopes: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut req = IndexMap::new();
        req.insert(scheme_name.into(), scopes.into_iter().collect());
        self.security_requirements.push(req);
        self
    }

    #[must_use]
    pub fn add_tag(mut self, name: impl Into<String>, description: Option<String>) -> Self {
        self.tags.push(Tag {
            name: name.into(),
            description,
        });
        self
    }

    /// Render the spec as pretty JSON.
    ///
    /// # Panics
    /// Panics only if the OpenAPI types have a serialization bug — none
    /// of them do (round-tripped in tests).
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("OpenApiSpec serialize")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub title: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<Contact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<License>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct License {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Server {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tag {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// =====================================================================
// Components
// =====================================================================

#[derive(Debug, Clone, Default, Serialize)]
pub struct Components {
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub schemas: IndexMap<String, Schema>,
    #[serde(skip_serializing_if = "IndexMap::is_empty", rename = "securitySchemes")]
    pub security_schemes: IndexMap<String, SecurityScheme>,
}

impl Components {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty() && self.security_schemes.is_empty()
    }
}

// =====================================================================
// Path + Operation
// =====================================================================

#[derive(Debug, Clone, Default, Serialize)]
pub struct PathItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Operation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
}

impl PathItem {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = Some(s.into());
        self
    }

    #[must_use]
    pub fn get(mut self, op: Operation) -> Self {
        self.get = Some(op);
        self
    }
    #[must_use]
    pub fn post(mut self, op: Operation) -> Self {
        self.post = Some(op);
        self
    }
    #[must_use]
    pub fn put(mut self, op: Operation) -> Self {
        self.put = Some(op);
        self
    }
    #[must_use]
    pub fn patch(mut self, op: Operation) -> Self {
        self.patch = Some(op);
        self
    }
    #[must_use]
    pub fn delete(mut self, op: Operation) -> Self {
        self.delete = Some(op);
        self
    }
    #[must_use]
    pub fn parameter(mut self, p: Parameter) -> Self {
        self.parameters.push(p);
        self
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Operation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "operationId")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "requestBody")]
    pub request_body: Option<RequestBody>,
    pub responses: IndexMap<String, Response>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<IndexMap<String, Vec<String>>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
}

impl Operation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = Some(s.into());
        self
    }

    #[must_use]
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    #[must_use]
    pub fn operation_id(mut self, id: impl Into<String>) -> Self {
        self.operation_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn tag(mut self, t: impl Into<String>) -> Self {
        self.tags.push(t.into());
        self
    }

    #[must_use]
    pub fn parameter(mut self, p: Parameter) -> Self {
        self.parameters.push(p);
        self
    }

    #[must_use]
    pub fn request_body(mut self, body: RequestBody) -> Self {
        self.request_body = Some(body);
        self
    }

    #[must_use]
    pub fn response(mut self, status: impl Into<String>, response: Response) -> Self {
        self.responses.insert(status.into(), response);
        self
    }

    /// Drop global security for this operation. (Empty list = "no auth required".)
    #[must_use]
    pub fn no_security(mut self) -> Self {
        self.security = Some(Vec::new());
        self
    }

    #[must_use]
    pub fn require_security(
        mut self,
        scheme_name: impl Into<String>,
        scopes: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut req = IndexMap::new();
        req.insert(scheme_name.into(), scopes.into_iter().collect());
        self.security.get_or_insert_with(Vec::new).push(req);
        self
    }

    #[must_use]
    pub fn deprecated(mut self) -> Self {
        self.deprecated = Some(true);
        self
    }
}

// =====================================================================
// Parameters / Request body / Response
// =====================================================================

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParameterIn {
    Query,
    Header,
    Path,
    Cookie,
}

#[derive(Debug, Clone, Serialize)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: ParameterIn,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    pub schema: Schema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<serde_json::Value>,
}

impl Parameter {
    pub fn path(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            location: ParameterIn::Path,
            description: None,
            required: true,
            schema,
            example: None,
        }
    }
    pub fn query(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            location: ParameterIn::Query,
            description: None,
            required: false,
            schema,
            example: None,
        }
    }
    pub fn header(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            location: ParameterIn::Header,
            description: None,
            required: false,
            schema,
            example: None,
        }
    }

    #[must_use]
    pub fn required(mut self, r: bool) -> Self {
        self.required = r;
        self
    }
    #[must_use]
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
    #[must_use]
    pub fn example(mut self, v: impl Serialize) -> Self {
        self.example = serde_json::to_value(v).ok();
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub content: IndexMap<String, MediaType>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

impl RequestBody {
    #[must_use]
    pub fn json(schema: Schema) -> Self {
        let mut content = IndexMap::new();
        content.insert(
            "application/json".to_owned(),
            MediaType {
                schema,
                example: None,
            },
        );
        Self {
            description: None,
            content,
            required: true,
        }
    }

    #[must_use]
    pub fn form(schema: Schema) -> Self {
        let mut content = IndexMap::new();
        content.insert(
            "application/x-www-form-urlencoded".to_owned(),
            MediaType {
                schema,
                example: None,
            },
        );
        Self {
            description: None,
            content,
            required: true,
        }
    }

    #[must_use]
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
    #[must_use]
    pub fn required(mut self, r: bool) -> Self {
        self.required = r;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub description: String,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub content: IndexMap<String, MediaType>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, Header>,
}

impl Response {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            content: IndexMap::new(),
            headers: IndexMap::new(),
        }
    }

    #[must_use]
    pub fn json_content(mut self, schema: Schema) -> Self {
        self.content.insert(
            "application/json".to_owned(),
            MediaType { schema, example: None },
        );
        self
    }

    #[must_use]
    pub fn header(mut self, name: impl Into<String>, schema: Schema) -> Self {
        self.headers.insert(
            name.into(),
            Header {
                description: None,
                schema,
            },
        );
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaType {
    pub schema: Schema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Header {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: Schema,
}

// =====================================================================
// Schema (a useful subset of JSON Schema 2020-12 / OpenAPI 3.1)
// =====================================================================

/// A JSON Schema fragment — subset used by OpenAPI 3.1.
///
/// Construct via the builder methods ([`Schema::object`], [`Schema::string`],
/// etc.) and chain modifiers (`.required(...)`, `.format(...)`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Schema {
    #[serde(skip_serializing_if = "Option::is_none", rename = "$ref")]
    pub ref_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub type_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<String, Schema>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<Schema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty", rename = "enum")]
    pub enum_: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "minLength")]
    pub min_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "maxLength")]
    pub max_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "additionalProperties")]
    pub additional_properties: Option<Box<Schema>>,
}

impl Schema {
    /// Reference an existing schema in `components.schemas`.
    #[must_use]
    pub fn ref_(name: impl AsRef<str>) -> Self {
        Self {
            ref_: Some(format!("#/components/schemas/{}", name.as_ref())),
            ..Default::default()
        }
    }

    fn typed(t: &'static str) -> Self {
        Self {
            type_: Some(t.into()),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn string() -> Self {
        Self::typed("string")
    }
    #[must_use]
    pub fn integer() -> Self {
        let mut s = Self::typed("integer");
        s.format = Some("int64".into());
        s
    }
    #[must_use]
    pub fn int32() -> Self {
        let mut s = Self::typed("integer");
        s.format = Some("int32".into());
        s
    }
    #[must_use]
    pub fn number() -> Self {
        Self::typed("number")
    }
    #[must_use]
    pub fn boolean() -> Self {
        Self::typed("boolean")
    }
    #[must_use]
    pub fn object() -> Self {
        Self::typed("object")
    }
    #[must_use]
    pub fn array_of(items: Schema) -> Self {
        Self {
            type_: Some("array".into()),
            items: Some(Box::new(items)),
            ..Default::default()
        }
    }
    /// Free-form JSON object (`{}`) — no constraints.
    #[must_use]
    pub fn any_object() -> Self {
        Self {
            type_: Some("object".into()),
            additional_properties: Some(Box::new(Self::default())),
            ..Default::default()
        }
    }
    #[must_use]
    pub fn datetime() -> Self {
        let mut s = Self::string();
        s.format = Some("date-time".into());
        s
    }
    #[must_use]
    pub fn date() -> Self {
        let mut s = Self::string();
        s.format = Some("date".into());
        s
    }
    #[must_use]
    pub fn uuid() -> Self {
        let mut s = Self::string();
        s.format = Some("uuid".into());
        s
    }
    #[must_use]
    pub fn email() -> Self {
        let mut s = Self::string();
        s.format = Some("email".into());
        s
    }
    #[must_use]
    pub fn uri() -> Self {
        let mut s = Self::string();
        s.format = Some("uri".into());
        s
    }

    #[must_use]
    pub fn property(mut self, name: impl Into<String>, schema: Schema) -> Self {
        self.properties.insert(name.into(), schema);
        self
    }

    #[must_use]
    pub fn required<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.required.extend(fields.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn nullable(mut self) -> Self {
        self.nullable = Some(true);
        self
    }

    #[must_use]
    pub fn enum_<I, V>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Serialize,
    {
        self.enum_ = values
            .into_iter()
            .filter_map(|v| serde_json::to_value(v).ok())
            .collect();
        self
    }

    #[must_use]
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    #[must_use]
    pub fn format(mut self, f: impl Into<String>) -> Self {
        self.format = Some(f.into());
        self
    }

    #[must_use]
    pub fn example(mut self, v: impl Serialize) -> Self {
        self.example = serde_json::to_value(v).ok();
        self
    }

    /// Set the JSON Schema `default` value. Renamed from `default()` to
    /// avoid colliding with the `Default::default` constructor used to
    /// build empty schemas.
    #[must_use]
    pub fn default_value(mut self, v: impl Serialize) -> Self {
        self.default = serde_json::to_value(v).ok();
        self
    }

    #[must_use]
    pub fn min_length(mut self, n: u64) -> Self {
        self.min_length = Some(n);
        self
    }
    #[must_use]
    pub fn max_length(mut self, n: u64) -> Self {
        self.max_length = Some(n);
        self
    }
    #[must_use]
    pub fn minimum(mut self, n: f64) -> Self {
        self.minimum = Some(n);
        self
    }
    #[must_use]
    pub fn maximum(mut self, n: f64) -> Self {
        self.maximum = Some(n);
        self
    }
}

// =====================================================================
// Security
// =====================================================================

/// Security scheme definition.
///
/// Helpers: [`SecurityScheme::bearer`], [`SecurityScheme::api_key_header`],
/// [`SecurityScheme::basic`], [`SecurityScheme::oauth2_authorization_code`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SecurityScheme {
    #[serde(rename = "http")]
    Http {
        scheme: String,
        #[serde(skip_serializing_if = "Option::is_none", rename = "bearerFormat")]
        bearer_format: Option<String>,
    },
    #[serde(rename = "apiKey")]
    ApiKey {
        name: String,
        #[serde(rename = "in")]
        location: ParameterIn,
    },
    #[serde(rename = "oauth2")]
    OAuth2 {
        flows: OAuth2Flows,
    },
    #[serde(rename = "openIdConnect")]
    OpenIdConnect {
        #[serde(rename = "openIdConnectUrl")]
        open_id_connect_url: String,
    },
}

impl SecurityScheme {
    #[must_use]
    pub fn bearer(format: impl Into<String>) -> Self {
        Self::Http {
            scheme: "bearer".into(),
            bearer_format: Some(format.into()),
        }
    }

    #[must_use]
    pub fn basic() -> Self {
        Self::Http {
            scheme: "basic".into(),
            bearer_format: None,
        }
    }

    #[must_use]
    pub fn api_key_header(name: impl Into<String>) -> Self {
        Self::ApiKey {
            name: name.into(),
            location: ParameterIn::Header,
        }
    }

    #[must_use]
    pub fn api_key_query(name: impl Into<String>) -> Self {
        Self::ApiKey {
            name: name.into(),
            location: ParameterIn::Query,
        }
    }

    #[must_use]
    pub fn oauth2_authorization_code(
        authorization_url: impl Into<String>,
        token_url: impl Into<String>,
        scopes: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self::OAuth2 {
            flows: OAuth2Flows {
                authorization_code: Some(OAuth2Flow {
                    authorization_url: Some(authorization_url.into()),
                    token_url: Some(token_url.into()),
                    refresh_url: None,
                    scopes: scopes.into_iter().collect(),
                }),
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OAuth2Flows {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implicit: Option<OAuth2Flow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<OAuth2Flow>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "clientCredentials")]
    pub client_credentials: Option<OAuth2Flow>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "authorizationCode")]
    pub authorization_code: Option<OAuth2Flow>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OAuth2Flow {
    #[serde(skip_serializing_if = "Option::is_none", rename = "authorizationUrl")]
    pub authorization_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tokenUrl")]
    pub token_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "refreshUrl")]
    pub refresh_url: Option<String>,
    pub scopes: IndexMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_spec_serializes() {
        let spec = OpenApiSpec::new("Test API", "1.0.0");
        let json = spec.to_json();
        assert!(json.contains("\"openapi\": \"3.1.0\""));
        assert!(json.contains("\"title\": \"Test API\""));
        assert!(json.contains("\"version\": \"1.0.0\""));
        assert!(!json.contains("paths")); // empty paths skipped
    }

    #[test]
    fn schema_object_with_required_fields() {
        let s = Schema::object()
            .property("id", Schema::integer())
            .property("title", Schema::string())
            .property("draft", Schema::boolean())
            .required(["id", "title"]);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "object");
        assert_eq!(v["properties"]["id"]["type"], "integer");
        assert_eq!(v["properties"]["id"]["format"], "int64");
        assert_eq!(v["properties"]["title"]["type"], "string");
        assert_eq!(v["required"], serde_json::json!(["id", "title"]));
    }

    #[test]
    fn schema_ref_uses_components_path() {
        let s = Schema::ref_("Post");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["$ref"], "#/components/schemas/Post");
        // refs MUST NOT have other fields per JSON Schema (or other fields
        // are ignored — we still skip them when None / empty).
        assert!(v.get("type").is_none());
    }

    #[test]
    fn schema_array_of_includes_items() {
        let s = Schema::array_of(Schema::ref_("Post"));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "array");
        assert_eq!(v["items"]["$ref"], "#/components/schemas/Post");
    }

    #[test]
    fn schema_format_helpers() {
        assert_eq!(serde_json::to_value(Schema::datetime()).unwrap()["format"], "date-time");
        assert_eq!(serde_json::to_value(Schema::uuid()).unwrap()["format"], "uuid");
        assert_eq!(serde_json::to_value(Schema::email()).unwrap()["format"], "email");
    }

    #[test]
    fn schema_enum_serializes_under_enum_key() {
        let s = Schema::string().enum_(["draft", "published", "archived"]);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["enum"], serde_json::json!(["draft", "published", "archived"]));
    }

    #[test]
    fn nullable_field() {
        let s = Schema::string().nullable();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["nullable"], true);
    }

    #[test]
    fn full_path_operation_serializes() {
        let spec = OpenApiSpec::new("API", "1.0")
            .add_schema(
                "Post",
                Schema::object()
                    .property("id", Schema::integer())
                    .property("title", Schema::string())
                    .required(["id", "title"]),
            )
            .add_path(
                "/posts",
                PathItem::new().get(
                    Operation::new()
                        .summary("List posts")
                        .operation_id("list_posts")
                        .tag("posts")
                        .response(
                            "200",
                            Response::new("OK").json_content(Schema::array_of(Schema::ref_("Post"))),
                        ),
                ),
            );
        let v: serde_json::Value = serde_json::from_str(&spec.to_json()).unwrap();
        assert_eq!(v["paths"]["/posts"]["get"]["summary"], "List posts");
        assert_eq!(v["paths"]["/posts"]["get"]["operationId"], "list_posts");
        assert_eq!(v["paths"]["/posts"]["get"]["tags"][0], "posts");
        assert_eq!(
            v["paths"]["/posts"]["get"]["responses"]["200"]["content"]
                ["application/json"]["schema"]["items"]["$ref"],
            "#/components/schemas/Post"
        );
    }

    #[test]
    fn path_parameter_is_required_by_default() {
        let p = Parameter::path("id", Schema::integer());
        assert!(p.required);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["in"], "path");
        assert_eq!(v["required"], true);
        assert_eq!(v["name"], "id");
    }

    #[test]
    fn query_parameter_is_optional_by_default() {
        let p = Parameter::query("page", Schema::integer());
        assert!(!p.required);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["in"], "query");
        assert!(v.get("required").is_none() || v["required"] == false);
    }

    #[test]
    fn request_body_json_helper() {
        let body = RequestBody::json(Schema::ref_("Post"));
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(
            v["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Post"
        );
        assert_eq!(v["required"], true);
    }

    #[test]
    fn security_bearer_jwt() {
        let s = SecurityScheme::bearer("JWT");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "http");
        assert_eq!(v["scheme"], "bearer");
        assert_eq!(v["bearerFormat"], "JWT");
    }

    #[test]
    fn security_api_key_header() {
        let s = SecurityScheme::api_key_header("X-API-Key");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "apiKey");
        assert_eq!(v["name"], "X-API-Key");
        assert_eq!(v["in"], "header");
    }

    #[test]
    fn security_oauth2_authorization_code() {
        let s = SecurityScheme::oauth2_authorization_code(
            "https://idp/auth",
            "https://idp/token",
            [("read".to_owned(), "Read access".to_owned())],
        );
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "oauth2");
        assert_eq!(v["flows"]["authorizationCode"]["authorizationUrl"], "https://idp/auth");
        assert_eq!(v["flows"]["authorizationCode"]["tokenUrl"], "https://idp/token");
        assert_eq!(v["flows"]["authorizationCode"]["scopes"]["read"], "Read access");
    }

    #[test]
    fn global_security_propagates() {
        let spec = OpenApiSpec::new("API", "1.0")
            .add_security_scheme("bearerAuth", SecurityScheme::bearer("JWT"))
            .require_security("bearerAuth", []);
        let v: serde_json::Value = serde_json::from_str(&spec.to_json()).unwrap();
        assert_eq!(v["security"][0]["bearerAuth"], serde_json::json!([]));
        assert_eq!(v["components"]["securitySchemes"]["bearerAuth"]["type"], "http");
    }

    #[test]
    fn operation_no_security_overrides_global() {
        let op = Operation::new()
            .summary("Public")
            .no_security()
            .response("200", Response::new("OK"));
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["security"], serde_json::json!([]));
    }

    #[test]
    fn operation_require_security_with_scopes() {
        let op = Operation::new()
            .require_security("oauth2", ["read".to_owned(), "write".to_owned()])
            .response("200", Response::new("OK"));
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["security"][0]["oauth2"], serde_json::json!(["read", "write"]));
    }

    #[test]
    fn server_and_contact_and_license() {
        let spec = OpenApiSpec::new("API", "1.0")
            .description("My great API")
            .server("https://api.example.com", "Production")
            .server("https://staging.example.com", "Staging")
            .contact("Devs", Some("devs@example.com".into()))
            .license("MIT", Some("https://opensource.org/license/mit".into()));
        let v: serde_json::Value = serde_json::from_str(&spec.to_json()).unwrap();
        assert_eq!(v["info"]["description"], "My great API");
        assert_eq!(v["servers"][0]["url"], "https://api.example.com");
        assert_eq!(v["servers"][1]["description"], "Staging");
        assert_eq!(v["info"]["contact"]["email"], "devs@example.com");
        assert_eq!(v["info"]["license"]["name"], "MIT");
    }

    #[test]
    fn deprecated_operation_flagged() {
        let op = Operation::new().deprecated().response("200", Response::new("OK"));
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["deprecated"], true);
    }
}
