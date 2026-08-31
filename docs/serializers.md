# Serializers

A serializer turns a model instance into a typed, JSON-ready shape — and back
again on the way in. It's **Rustango**'s answer to a Django REST Framework
`ModelSerializer` or a Laravel API Resource: declare a struct, annotate its
fields, and you get controlled output (rename, hide, compute, nest), field- and
object-level validation, and a clean hook into ViewSets.

One thing to internalise up front, because it differs from DRF: a Rustango
serializer **shapes data, it doesn't persist it**. There's no `serializer.save()`
that writes to the database — the ORM does that. The serializer maps a model to
JSON (`from_model` → `to_value`), declares which fields are writable, and
validates. You compose it with the ORM and ViewSets rather than routing writes
*through* it.

> **New to a term here?** — *serializer*, *model*, *ORM*, *DRF*? The
> [glossary](glossary.md) defines each in plain language.

[![A Rustango serializer: read_only, source rename, a computed method field, a nested FK, and a write_only field — declared on one struct](img/serializers.png)](img/serializers.png)

> **Source:** `rustango::serializer` (`ModelSerializer`, `#[derive(Serializer)]`,
> the `#[serializer(...)]` field attributes) — behind the `serializer` feature
> (on by default).
>
> **Runnable versions:** the minimal serializer ships in the tested
> [`getting_started_blog`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/getting_started_blog/src/post_serializer.rs)
> example, and the derive's full behavior is covered by the framework's own
> unit tests — `crates/rustango/tests/serializer_derive.rs` and
> `serializer_cross_validate.rs`. If a snippet looks off, diff against them.

---

## Table of contents
- [Quick start](#quick-start) · [The `ModelSerializer` trait](#the-modelserializer-trait)
- [Field attributes](#field-attributes) — the full reference
- [Computed fields](#computed-fields) · [Nested serializers](#nested-serializers) · [Collections](#collections-many) · [Slug fields](#slug-related-fields)
- [Validation](#validation) · [Unique-together](#unique-together-validation)
- [Hyperlinked output](#hyperlinked-output) · [Serializing lists](#serializing-lists)
- [Using a serializer with a ViewSet](#using-a-serializer-with-a-viewset) · [Validating in a custom handler](#validating-in-a-custom-handler)
- [OpenAPI](#openapi-schemas) · [Scaffolding](#scaffolding) · [Tweaks & limits](#tweaks-and-current-limits)

---

## Quick start

A serializer is a plain struct with `#[derive(Serializer)]` and a
`#[serializer(model = …)]` pointing at the model it maps from. It needs two
companion derives: `serde::Deserialize` (so it can also parse incoming JSON) and
`Default` (so excluded/optional fields can be initialised).

```rust
use rustango::Serializer;
use rustango::serializer::ModelSerializer;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,

    #[serializer(source = "body")]      // JSON key `content`, read from model.body
    pub content: String,

    #[serializer(read_only)]            // in output, never accepted on write
    pub published_at: Auto<DateTime<Utc>>,
}
```

Use it:

```rust
let post = Post::objects().find(42, &pool).await?.expect("post 42");

let one  = PostSerializer::from_model(&post).to_value();   // a JSON object
let many = PostSerializer::many_to_value(&posts);          // a JSON array
```

`from_model` clones the model's fields into the struct (honouring the attributes
below); `to_value` serialises it (skipping `write_only` fields). That's the
whole core loop.

---

## The `ModelSerializer` trait

`#[derive(Serializer)]` implements `ModelSerializer` (plus a `serde::Serialize`
that respects `write_only`, and an `OpenApiSchema` impl under the `openapi`
feature). The trait surface:

| Method | Signature | Notes |
|---|---|---|
| `from_model` | `fn(model: &Self::Model) -> Self` | Map a model → serializer. Generated; not overridable. |
| `to_value` | `fn(&self) -> serde_json::Value` | Serialise to JSON (skips `write_only`). Overridable. |
| `many` | `fn(&[Self::Model]) -> Vec<Self>` | Batch `from_model`. Overridable. |
| `many_to_value` | `fn(&[Self::Model]) -> serde_json::Value` | Batch → JSON array. Overridable. |
| `writable_fields` | `fn() -> &'static [&'static str]` | Serializer field names accepted on write (excludes `read_only`, `skip`, `method`, `nested`, `many`, `slug`). |
| `writable_source_fields` | `fn() -> &'static [&'static str]` | The **model columns** of the writable fields (`source`-resolved). The ViewSet write path persists only these. Generated. |
| `from_writable_json` | `fn(&Value) -> Result<Self, FormErrors>` | Build an instance from a request body using only the writable fields (the rest default); per-field parse errors → `FormErrors`. Generated. |
| `validate` | `fn(&self) -> Result<(), FormErrors>` | Runs declared per-field + cross-field validators. No-op when none are declared; overridable. |

There is deliberately **no** `create` / `update` / `save` on the trait — writes
go through the ORM (`model.save(&pool)`). When a serializer is wired into a
[ViewSet](viewsets.md), the create/update path uses `from_writable_json()` +
`validate()` + `writable_source_fields()` to validate and filter the request
before saving.

---

## Field attributes

Everything is controlled by `#[serializer(...)]` on each field. The full set:

| Attribute | `from_model` does | In JSON output? | Writable? |
|---|---|---|---|
| *(none)* | maps from the model | yes | yes |
| `read_only` | maps from the model | yes | **no** |
| `write_only` | `Default::default()` | **no** | yes |
| `source = "x"` | maps from `model.x` (renames) | yes | yes |
| `skip` | `Default::default()` — set it yourself | yes | no |
| `method = "fn"` | calls `Self::fn(&model)` | yes | no |
| `nested` | walks an FK → `Child::from_model(parent)` | yes | no |
| `nested(strict)` | same, but panics if the FK wasn't loaded | yes | no |
| `many = ChildSer` | inits `Vec::new()`; fill via `set_<field>(&[Child])` | yes | no |
| `slug = "name"` | clones `model.<source>.value()?.name` | yes | no |
| `validate = "fn"` | per-field validator run by `validate(&self)` | n/a | n/a |

**Mutually exclusive** (compile errors if combined): `read_only` + `write_only`;
`method` + `source`; `slug` + any of `method` / `nested` / `many`.

**Declarative validators.** `max_length = N`, `min_length = N`, `min = N`, and
`max = N` add write-time validation to a field without changing its output shape
(and a field with none of them inherits the model's bounds). See
[Validation](#validation).

`write_only` is for inbound-only data (a password, a one-time token): present in
`writable_fields()`, absent from output. `skip` is the opposite escape hatch —
the field isn't read from the model and isn't writable, so you populate it by
hand after `from_model` (e.g. a list of tag ids you fetch separately).

> **`write_only` does not transform the value.** A `write_only` field is
> accepted on write and persisted **verbatim** — the serializer never hashes or
> encrypts it. For a password, hash it yourself (see [Passwords](auth-passwords.md))
> before `save()`; `read_only` fields, conversely, are silently ignored on write
> rather than rejected.

---

## Computed fields

`method = "fn"` is DRF's `SerializerMethodField`. Declare the field, then write
an associated function `fn(&Model) -> FieldType`; it's called during
`from_model`:

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub title: String,
    #[serializer(method = "excerpt")]
    pub excerpt: String,
}

impl PostSerializer {
    fn excerpt(model: &Post) -> String {
        model.body.chars().take(80).collect::<String>() + "…"
    }
}
```

Computed fields are output-only (excluded from `writable_fields()`).

---

## Nested serializers

`nested` embeds another serializer by walking a loaded foreign key. The field's
type is the child serializer:

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Comment)]
pub struct CommentSerializer {
    pub id: Auto<i64>,
    pub body: String,
    #[serializer(nested)]               // reads the loaded `author` FK
    pub author: AuthorBrief,
}
```

The FK must already be loaded (via `select_related` / an eager fetch). If it
**wasn't** loaded, the field falls back to `Default::default()` rather than
panicking — production degrades gracefully on a missing prefetch. In tests, use
`#[serializer(nested(strict))]` to turn that fallback into a panic so a dropped
prefetch is caught. Point at a differently-named FK with `source`:

```rust
#[serializer(nested, source = "owner")]
pub author: AuthorBrief,
```

Nested fields are **read-only** in the output shape — writable nested objects
aren't supported yet (see [limits](#tweaks-and-current-limits)).

---

## Collections (`many`)

For one-to-many or M2M children, `many = ChildSerializer` declares a `Vec<…>`
field. Because the M2M/related accessor is async, the macro can't auto-load it;
it initialises the vec empty and emits a `set_<field>(&[ChildModel])` helper you
call after fetching the children:

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostWithTags {
    pub id: Auto<i64>,
    pub title: String,
    #[serializer(many = TagBrief)]
    pub tags: Vec<TagBrief>,
}

// usage
let tags = post.tags_m2m().all(&pool).await?;
let mut s = PostWithTags::from_model(&post);
s.set_tags(&tags);                       // generated setter, named set_<field>
let json = s.to_value();
```

---

## Slug related fields

`slug = "name"` is DRF's `SlugRelatedField`: instead of an FK id or a full
nested object, emit a single named field pulled from the loaded parent.

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,
    #[serializer(slug = "name", source = "author")]   // author.name as a flat field
    pub author_name: String,
}
```

Like nested, it reads from a loaded FK and falls back to the default when
unloaded; it's display-only (not writable).

---

## Validation

Three layers, all surfacing as `rustango::forms::FormErrors` (and, on a ViewSet
write, a DRF-shape `400`). They run in this order: declarative constraints, then
per-field validators, then the cross-field hook.

**Declarative constraints (DRF `validators`, auto-inherited).** `max_length`,
`min_length`, `min`, and `max` are field attributes — and when you omit them a
field **inherits the model's** `max_length` / `min` / `max` / `choices`. So a
`#[rustango(max_length = 200)]` column is length-checked with no serializer
attribute at all (DRF `ModelSerializer` behaviour). They're checked on every
writable field, turning would-be database-constraint `500`s into friendly `400`s:

```rust
#[serializer(model = Widget)]
struct WidgetSerializer {
    pub code: String,               // inherits the model's max_length
    #[serializer(max_length = 4)]   // overrides the model's bound
    pub note: String,
    pub priority: i64,              // inherits the model's min / max
    pub status: String,             // inherits the model's choices
}
```

Messages match Django/DRF: `"Ensure this value has at most N characters."`,
`"Ensure this value has at least N characters."`, `"Ensure this value is ≥ N."` /
`"≤ N"`, and `"Select a valid choice."`. (`min_length` is serializer-only;
`choices` is inherited from the model — there's no `choices` attribute.)

**Per-field** (custom) — declare `validate = "fn"` and write
`fn(value: &FieldType) -> Result<(), String>`:

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    #[serializer(validate = "title_min_3")]
    pub title: String,
    pub body: String,
}

impl PostSerializer {
    fn title_min_3(t: &String) -> Result<(), String> {
        if t.chars().count() < 3 { Err("title must be at least 3 chars".into()) } else { Ok(()) }
    }
}
```

The derive generates a `validate(&self)` that runs each per-field validator and
collects failures into a `FormErrors` keyed by field name.

**Cross-field** — declare a struct-level hook and the validators merge. Either
add `#[serializer(validate = "cross_validate")]` on the struct (returning
`Result<(), FormErrors>`), or simply implement `validate(&self)` yourself when
there are no per-field validators to generate it:

```rust
impl PostSerializer {
    pub fn validate(&self) -> Result<(), rustango::forms::FormErrors> {
        let mut errors = rustango::forms::FormErrors::default();
        if self.title.is_empty() {
            errors.add("title", "title cannot be empty");          // field error
        }
        if self.body.starts_with(&self.title) {
            errors.add_non_field("body must not repeat the title"); // object-level error
        }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}
```

`FormErrors` separates **field** errors (`add(field, msg)`, a
`HashMap<String, Vec<String>>`) from **non-field** errors
(`add_non_field(msg)`). Inspect with `.fields()`, `.non_field()`, `.get(field)`,
`.is_empty()`, and combine with `.merge(other)`. Beyond the declarative
constraints above (`max_length` / `min_length` / `min` / `max` / inherited
`choices`), custom rules are plain functions — there's no `email`/regex magic,
which keeps custom validation explicit and testable. Outside a ViewSet the
framework doesn't auto-render `FormErrors` to an HTTP body; map it to your 400
response (the field/non-field split lines up with DRF's error JSON).

---

## Unique-together validation

For Django's `UniqueTogetherValidator` — a pre-save check that a candidate row
won't collide on a multi-column unique index — call
`check_unique_together_pool` before saving:

```rust
use std::collections::HashMap;
use rustango::core::SqlValue;
use rustango::serializer::check_unique_together_pool;

let mut values: HashMap<&'static str, SqlValue> = HashMap::new();
values.insert("org_id",  SqlValue::I64(self.org_id));
values.insert("user_id", SqlValue::I64(self.user_id));

// None on insert; Some(&pk) on update so the row doesn't clash with itself.
check_unique_together_pool(&pool, Membership::SCHEMA, &values, None).await?;
```

It walks the model's declared multi-column unique indexes and returns
`Err(FormErrors)` with a non-field error per collision
(`"The fields a, b must be unique together."`). Single-column `unique` is left
to the insert's conflict handling; partial (`unique_when`) indexes are skipped.

---

## Hyperlinked output

For a `HyperlinkedModelSerializer`-style shape (resource URLs instead of bare
ids), two helpers post-process the JSON:

```rust
use rustango::serializer::{hyperlink_url, hyperlinked_to_value};
use std::collections::HashMap;

let base = PostSerializer::from_model(&post).to_value();

let mut fk_templates = HashMap::new();
fk_templates.insert("author_id", "/api/users/{pk}");

let out = hyperlinked_to_value(base, "/api/posts/{pk}", "id", &fk_templates);
// → { "url": "/api/posts/42", "author_id_url": "/api/users/7", "id": 42, ... }
```

`hyperlink_url(template, &pk)` does a one-off `{pk}` substitution;
`hyperlinked_to_value` adds a top-level `url` plus a `<fk>_url` per template
(null FK → null URL). The original id/`<fk>_id` keys are kept (remove them after
if you want them gone).

---

## Serializing lists

`many_to_value(&models)` returns a JSON array of serialised objects. ViewSets
wrap a page of them in the standard envelope:

```json
{ "count": 100, "page": 1, "page_size": 20, "last_page": 5, "results": [ { … }, { … } ] }
```

(That's the default page-number envelope; see [Pagination](viewsets.md#pagination)
for the cursor and limit/offset shapes.)

---

## Using a serializer with a ViewSet

Wire a serializer into a [ViewSet](viewsets.md) and it drives the whole REST
resource — **output and input**, on every backend (PostgreSQL, MySQL, SQLite):

```rust
#[derive(ViewSet)]
#[viewset(model = Post, serializer = crate::PostSerializer, ordering = "-published_at")]
pub struct PostViewSet;
// or, on the builder: ViewSet::for_model(Post::SCHEMA).serializer::<PostSerializer>()…
```

- **Output** — `list` / `retrieve` / `create` / `update` responses render
  through `from_model`, so `source` / `method` / `read_only` / `write_only`
  shape the JSON.
- **Input** — `create` / `update` run the serializer's `validate()` (a failure
  is a DRF-shape `400`, `{field: [msgs]}`), and only writable fields are
  written — `read_only` / computed fields a client posts are ignored,
  `source`-resolved to the model column.

The ViewSet drives this through three `ModelSerializer` methods the derive
generates: `validate()`, `writable_source_fields()`, and `from_writable_json()`.
See the [ViewSets guide](viewsets.md#the-serializer-marriage-input--output) for
the full behavior and a worked example.

You can also use a serializer **standalone** — map a row and emit its JSON from
any handler:

```rust
let post = Post::objects().find(42, &pool).await?.expect("post 42");
let body = PostSerializer::from_model(&post).to_value();   // shaped JSON
```

---

## Validating in a custom handler

Outside a ViewSet, the serializer derives `serde::Deserialize`, so you can parse
a request body into it, run `.validate()`, and — on success — map the data onto
a model and `save(&pool)`. `from_writable_json()` builds an instance from only
the writable keys (read-only / computed fields default), and `writable_fields()`
/ `writable_source_fields()` tell you which keys are accepted — the same
machinery the ViewSet uses internally.

---

## OpenAPI schemas

With the `openapi` feature on, the derive also emits an `OpenApiSchema` impl:
field types map to JSON-schema types, `Option<T>` becomes nullable-and-not-
required, and `write_only` fields are excluded from the response schema. This is
what feeds the generated API docs — no separate schema to maintain.

> **Deep dive:** [OpenAPI](openapi.md) — turn this schema (plus your ViewSet's
> CRUD paths) into a full OpenAPI 3.1 spec served with Swagger UI / Redoc.

---

## Scaffolding

Generate a serializer skeleton with the manage CLI:

```bash
cargo run -- make:serializer PostSerializer --model Post
```

It writes a starter module you fill in:

```rust
//! Auto-scaffolded by `manage make:serializer PostSerializer`.

use rustango::Serializer;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: i64,
    // pub title: String,
    // #[serializer(read_only)]
    // pub created_at: chrono::DateTime<chrono::Utc>,
}
```

Then register the module (`mod post_serializer;`) alongside your others.

---

## Tweaks and current limits

A few sharp edges and escape hatches worth knowing:

- **Conditional fields.** There's no runtime field selection (fields are fixed
  at compile time). For "include only when present", use `Option<T>` plus
  `#[serde(skip_serializing_if = "Option::is_none")]` on the field — the custom
  `Serialize` impl honours serde attributes.
- **Custom output shape.** Override `to_value(&self)` on your struct for a fully
  bespoke JSON object when the attributes aren't enough.
- **Writable nested objects** aren't supported — `nested` / `many` / `slug`
  fields are output-only. Accept writes as scalar ids and resolve them yourself.
- **Built-in validators are length/range/choice only** — `max_length` /
  `min_length` / `min` / `max` (and inherited `choices`) are declarative; other
  rules (`email`, regex, …) are functions you write (see
  [Validation](#validation)).
- **One per-field validator per field.** For multiple rules on a field, combine
  them in that field's function, or add a cross-field `validate(&self)`.
- **The serializer doesn't persist.** Map → validate → hand the data to the ORM;
  there's no `serializer.save()`.

---

## Try it

The minimal serializer ships in the
[`getting_started_blog`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/getting_started_blog/src/post_serializer.rs)
example (Step 13 of the getting-started guide). The derive's full behavior — the
field attributes, computed/nested/many fields, and both validation layers — is
covered by the framework's own unit tests (no database needed):

```bash
cd crates/rustango
cargo test --test serializer_derive          # field attrs, method, nested, many, slug, OpenAPI
cargo test --test serializer_cross_validate  # per-field + cross-field validation aggregation
```

---

## See also

- [ViewSets](viewsets.md) — wire a serializer into a JSON CRUD API.
- [HTML views](html-views.md) — the server-rendered alternative to a JSON API.
- [OpenAPI](openapi.md) — a serializer's fields become a component schema.
- [ORM cookbook](orm.md) — the models serializers map from.
