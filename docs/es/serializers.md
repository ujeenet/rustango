# Serializadores

Un serializador convierte una instancia de modelo en una forma tipada, lista para
JSON — y de vuelta a la entrada. Es la respuesta de **Rustango** a un
`ModelSerializer` de Django REST Framework o a un API Resource de Laravel: declara
una struct, anota sus campos y obtienes una salida controlada (renombrar, ocultar,
calcular, anidar), validación a nivel de campo y de objeto, y un enganche limpio
con los ViewSets.

Una cosa que conviene interiorizar de entrada, porque difiere de DRF: un
serializador de Rustango **da forma a los datos, no los persiste**. No hay ningún
`serializer.save()` que escriba en la base de datos — de eso se encarga el ORM. El
serializador mapea un modelo a JSON (`from_model` → `to_value`), declara qué campos
son escribibles y valida. Lo compones con el ORM y los ViewSets en lugar de enrutar
escrituras *a través* de él.

> **¿Algún término nuevo aquí?** — *serializer*, *model*, *ORM*, *DRF*? El
> [glosario](glossary.md) define cada uno en lenguaje llano.

[![Un serializador de Rustango: read_only, renombrado con source, un campo de método calculado, una FK anidada y un campo write_only — declarados en una sola struct](img/serializers.png)](img/serializers.png)

> **Fuente:** `rustango::serializer` (`ModelSerializer`, `#[derive(Serializer)]`,
> los atributos de campo `#[serializer(...)]`) — tras la característica `serializer`
> (activada por defecto).
>
> **Versiones ejecutables:** el serializador mínimo se incluye en el ejemplo probado
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog/src/post_serializer.rs),
> y el comportamiento completo del derive está cubierto por las propias pruebas
> unitarias del framework — `crates/rustango/tests/serializer_derive.rs` y
> `serializer_cross_validate.rs`. Si algún fragmento parece desalineado, compáralo
> con ellas.

---

## Tabla de contenidos
- [Inicio rápido](#quick-start) · [El trait `ModelSerializer`](#the-modelserializer-trait)
- [Atributos de campo](#field-attributes) — la referencia completa
- [Campos calculados](#computed-fields) · [Serializadores anidados](#nested-serializers) · [Colecciones](#collections-many) · [Campos slug](#slug-related-fields)
- [Validación](#validation) · [Validación unique-together](#unique-together-validation)
- [Salida con hipervínculos](#hyperlinked-output) · [Serializar listas](#serializing-lists)
- [Usar un serializador con un ViewSet](#using-a-serializer-with-a-viewset) · [Validar en un handler personalizado](#validating-in-a-custom-handler)
- [OpenAPI](#openapi-schemas) · [Scaffolding](#scaffolding) · [Ajustes y límites](#tweaks-and-current-limits)

---

## Inicio rápido

Un serializador es una struct simple con `#[derive(Serializer)]` y un
`#[serializer(model = …)]` que apunta al modelo del que mapea. Necesita dos derives
acompañantes: `serde::Deserialize` (para que también pueda parsear el JSON entrante)
y `Default` (para que los campos excluidos/opcionales puedan inicializarse).

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

Úsalo:

```rust
let post = Post::objects().find(42, &pool).await?.expect("post 42");

let one  = PostSerializer::from_model(&post).to_value();   // a JSON object
let many = PostSerializer::many_to_value(&posts);          // a JSON array
```

`from_model` clona los campos del modelo dentro de la struct (respetando los
atributos de más abajo); `to_value` la serializa (omitiendo los campos
`write_only`). Ese es todo el bucle central.

---

## El trait `ModelSerializer`

`#[derive(Serializer)]` implementa `ModelSerializer` (más un `serde::Serialize`
que respeta `write_only`, y una impl de `OpenApiSchema` bajo la característica
`openapi`). La superficie del trait:

| Método | Firma | Notas |
|---|---|---|
| `from_model` | `fn(model: &Self::Model) -> Self` | Mapea un modelo → serializador. Generado; no sobrescribible. |
| `to_value` | `fn(&self) -> serde_json::Value` | Serializa a JSON (omite `write_only`). Sobrescribible. |
| `many` | `fn(&[Self::Model]) -> Vec<Self>` | `from_model` por lotes. Sobrescribible. |
| `many_to_value` | `fn(&[Self::Model]) -> serde_json::Value` | Lote → array JSON. Sobrescribible. |
| `writable_fields` | `fn() -> &'static [&'static str]` | Nombres de campo del serializador aceptados en escritura (excluye `read_only`, `skip`, `method`, `nested`, `many`, `slug`). |
| `writable_source_fields` | `fn() -> &'static [&'static str]` | Las **columnas del modelo** de los campos escribibles (resueltas por `source`). La ruta de escritura del ViewSet persiste solo estas. Generado. |
| `from_writable_json` | `fn(&Value) -> Result<Self, FormErrors>` | Construye una instancia a partir del cuerpo de una petición usando solo los campos escribibles (el resto toma su valor por defecto); los errores de parseo por campo → `FormErrors`. Generado. |
| `validate` | `fn(&self) -> Result<(), FormErrors>` | Ejecuta los validadores declarados por campo + entre campos. No hace nada cuando no se declara ninguno; sobrescribible. |

Deliberadamente **no** hay `create` / `update` / `save` en el trait — las
escrituras pasan por el ORM (`model.save(&pool)`). Cuando un serializador se conecta
a un [ViewSet](viewsets.md), la ruta de create/update usa `from_writable_json()` +
`validate()` + `writable_source_fields()` para validar y filtrar la petición antes
de guardar.

---

## Atributos de campo

Todo se controla con `#[serializer(...)]` en cada campo. El conjunto completo:

| Atributo | `from_model` hace | ¿En la salida JSON? | ¿Escribible? |
|---|---|---|---|
| *(ninguno)* | mapea desde el modelo | sí | sí |
| `read_only` | mapea desde el modelo | sí | **no** |
| `write_only` | `Default::default()` | **no** | sí |
| `source = "x"` | mapea desde `model.x` (renombra) | sí | sí |
| `skip` | `Default::default()` — lo asignas tú mismo | sí | no |
| `method = "fn"` | llama a `Self::fn(&model)` | sí | no |
| `nested` | recorre una FK → `Child::from_model(parent)` | sí | no |
| `nested(strict)` | igual, pero entra en pánico si la FK no fue cargada | sí | no |
| `many = ChildSer` | inicializa `Vec::new()`; rellena vía `set_<field>(&[Child])` | sí | no |
| `slug = "name"` | clona `model.<source>.value()?.name` | sí | no |
| `validate = "fn"` | validador por campo ejecutado por `validate(&self)` | n/a | n/a |

**Mutuamente excluyentes** (errores de compilación si se combinan): `read_only` +
`write_only`; `method` + `source`; `slug` + cualquiera de `method` / `nested` /
`many`.

**Validadores declarativos.** `max_length = N`, `min_length = N`, `min = N` y
`max = N` añaden validación en tiempo de escritura a un campo sin cambiar su forma de
salida (y un campo sin ninguno de ellos hereda los límites del modelo). Consulta
[Validación](#validation).

`write_only` es para datos solo de entrada (una contraseña, un token de un solo uso):
presente en `writable_fields()`, ausente de la salida. `skip` es la escotilla de
escape opuesta — el campo no se lee del modelo y no es escribible, así que lo pueblas
a mano tras `from_model` (p. ej. una lista de ids de etiquetas que obtienes por
separado).

> **`write_only` no transforma el valor.** Un campo `write_only` se acepta en
> escritura y se persiste **tal cual** — el serializador nunca lo hashea ni lo cifra.
> Para una contraseña, hashéala tú mismo (consulta [Contraseñas](auth-passwords.md))
> antes de `save()`; los campos `read_only`, en cambio, se ignoran silenciosamente en
> escritura en lugar de rechazarse.

---

## Campos calculados

`method = "fn"` es el `SerializerMethodField` de DRF. Declara el campo, luego escribe
una función asociada `fn(&Model) -> FieldType`; se llama durante `from_model`:

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

Los campos calculados son solo de salida (excluidos de `writable_fields()`).

---

## Serializadores anidados

`nested` incrusta otro serializador recorriendo una clave foránea cargada. El tipo
del campo es el serializador hijo:

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

La FK ya debe estar cargada (vía `select_related` / una obtención eager). Si **no**
fue cargada, el campo recurre a `Default::default()` en lugar de entrar en pánico —
en producción se degrada con elegancia ante un prefetch faltante. En pruebas, usa
`#[serializer(nested(strict))]` para convertir ese fallback en un pánico, de modo que
un prefetch olvidado se detecte. Apunta a una FK con otro nombre con `source`:

```rust
#[serializer(nested, source = "owner")]
pub author: AuthorBrief,
```

Los campos anidados son **de solo lectura** en la forma de salida — los objetos
anidados escribibles todavía no están soportados (consulta
[límites](#tweaks-and-current-limits)).

---

## Colecciones (`many`)

Para hijos uno-a-muchos o M2M, `many = ChildSerializer` declara un campo `Vec<…>`.
Como el accesor M2M/relacionado es async, la macro no puede cargarlo automáticamente;
inicializa el vec vacío y emite un helper `set_<field>(&[ChildModel])` que llamas tras
obtener los hijos:

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

## Campos slug relacionados

`slug = "name"` es el `SlugRelatedField` de DRF: en lugar de un id de FK o un objeto
anidado completo, emite un único campo con nombre extraído del padre cargado.

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

Como nested, lee de una FK cargada y recurre al valor por defecto cuando no está
cargada; es solo para visualización (no escribible).

---

## Validación

Tres capas, todas aflorando como `rustango::forms::FormErrors` (y, en una escritura
de ViewSet, un `400` con forma de DRF). Se ejecutan en este orden: restricciones
declarativas, luego validadores por campo, luego el enganche entre campos.

**Restricciones declarativas (los `validators` de DRF, auto-heredadas).**
`max_length`, `min_length`, `min` y `max` son atributos de campo — y cuando los
omites un campo **hereda del modelo** su `max_length` / `min` / `max` / `choices`.
Así que una columna `#[rustango(max_length = 200)]` recibe la comprobación de
longitud sin ningún atributo de serializador en absoluto (comportamiento del
`ModelSerializer` de DRF). Se comprueban en cada campo escribible, convirtiendo los
`500` de restricción de base de datos que habría en `400` amables:

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

Los mensajes coinciden con Django/DRF: `"Ensure this value has at most N characters."`,
`"Ensure this value has at least N characters."`, `"Ensure this value is ≥ N."` /
`"≤ N"`, y `"Select a valid choice."`. (`min_length` es solo de serializador;
`choices` se hereda del modelo — no existe un atributo `choices`.)

**Por campo** (personalizado) — declara `validate = "fn"` y escribe
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

El derive genera un `validate(&self)` que ejecuta cada validador por campo y recoge
los fallos en un `FormErrors` indexado por nombre de campo.

**Entre campos** — declara un enganche a nivel de struct y los validadores se
fusionan. O bien añade `#[serializer(validate = "cross_validate")]` en la struct
(devolviendo `Result<(), FormErrors>`), o simplemente implementa `validate(&self)` tú
mismo cuando no hay validadores por campo que lo generen:

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

`FormErrors` separa los errores de **campo** (`add(field, msg)`, un
`HashMap<String, Vec<String>>`) de los errores **no de campo**
(`add_non_field(msg)`). Inspecciónalos con `.fields()`, `.non_field()`,
`.get(field)`, `.is_empty()`, y combínalos con `.merge(other)`. Más allá de las
restricciones declarativas de arriba (`max_length` / `min_length` / `min` / `max` /
`choices` heredados), las reglas personalizadas son funciones simples — no hay magia
de `email`/regex, lo que mantiene la validación personalizada explícita y verificable.
Fuera de un ViewSet, el framework no renderiza automáticamente `FormErrors` a un
cuerpo HTTP; mapéalo a tu respuesta 400 (la separación campo/no-campo coincide con el
JSON de errores de DRF).

---

## Validación unique-together

Para el `UniqueTogetherValidator` de Django — una comprobación previa al guardado de
que una fila candidata no colisionará en un índice único multicolumna — llama a
`check_unique_together_pool` antes de guardar:

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

Recorre los índices únicos multicolumna declarados del modelo y devuelve
`Err(FormErrors)` con un error no de campo por colisión
(`"The fields a, b must be unique together."`). El `unique` de una sola columna se
deja al manejo de conflictos del insert; los índices parciales (`unique_when`) se
omiten.

---

## Salida con hipervínculos

Para una forma al estilo `HyperlinkedModelSerializer` (URLs de recursos en lugar de
ids desnudos), dos helpers post-procesan el JSON:

```rust
use rustango::serializer::{hyperlink_url, hyperlinked_to_value};
use std::collections::HashMap;

let base = PostSerializer::from_model(&post).to_value();

let mut fk_templates = HashMap::new();
fk_templates.insert("author_id", "/api/users/{pk}");

let out = hyperlinked_to_value(base, "/api/posts/{pk}", "id", &fk_templates);
// → { "url": "/api/posts/42", "author_id_url": "/api/users/7", "id": 42, ... }
```

`hyperlink_url(template, &pk)` hace una sustitución puntual de `{pk}`;
`hyperlinked_to_value` añade una `url` de nivel superior más un `<fk>_url` por
plantilla (FK null → URL null). Las claves originales id/`<fk>_id` se conservan
(elimínalas después si quieres que desaparezcan).

---

## Serializar listas

`many_to_value(&models)` devuelve un array JSON de objetos serializados. Los ViewSets
envuelven una página de ellos en el envoltorio estándar:

```json
{ "count": 100, "page": 1, "page_size": 20, "last_page": 5, "results": [ { … }, { … } ] }
```

(Ese es el envoltorio de número de página por defecto; consulta
[Paginación](viewsets.md#pagination) para las formas de cursor y limit/offset.)

---

## Usar un serializador con un ViewSet

Conecta un serializador a un [ViewSet](viewsets.md) y dirige todo el recurso REST —
**salida y entrada**, en cada backend (PostgreSQL, MySQL, SQLite):

```rust
#[derive(ViewSet)]
#[viewset(model = Post, serializer = crate::PostSerializer, ordering = "-published_at")]
pub struct PostViewSet;
// or, on the builder: ViewSet::for_model(Post::SCHEMA).serializer::<PostSerializer>()…
```

- **Salida** — las respuestas de `list` / `retrieve` / `create` / `update` se
  renderizan a través de `from_model`, así que `source` / `method` / `read_only` /
  `write_only` dan forma al JSON.
- **Entrada** — `create` / `update` ejecutan el `validate()` del serializador (un
  fallo es un `400` con forma de DRF, `{field: [msgs]}`), y solo se escriben los
  campos escribibles — los campos `read_only` / calculados que un cliente envíe se
  ignoran, resueltos por `source` a la columna del modelo.

El ViewSet dirige esto a través de tres métodos de `ModelSerializer` que el derive
genera: `validate()`, `writable_source_fields()` y `from_writable_json()`. Consulta la
[guía de ViewSets](viewsets.md#the-serializer-marriage-input--output) para el
comportamiento completo y un ejemplo trabajado.

También puedes usar un serializador **de forma independiente** — mapea una fila y
emite su JSON desde cualquier handler:

```rust
let post = Post::objects().find(42, &pool).await?.expect("post 42");
let body = PostSerializer::from_model(&post).to_value();   // shaped JSON
```

---

## Validar en un handler personalizado

Fuera de un ViewSet, el serializador deriva `serde::Deserialize`, así que puedes
parsear el cuerpo de una petición dentro de él, ejecutar `.validate()` y — en caso de
éxito — mapear los datos sobre un modelo y `save(&pool)`. `from_writable_json()`
construye una instancia solo a partir de las claves escribibles (los campos de solo
lectura / calculados toman su valor por defecto), y `writable_fields()` /
`writable_source_fields()` te dicen qué claves se aceptan — la misma maquinaria que el
ViewSet usa internamente.

---

## Esquemas OpenAPI

Con la característica `openapi` activada, el derive también emite una impl de
`OpenApiSchema`: los tipos de campo se mapean a tipos de JSON-schema, `Option<T>` pasa
a ser nullable-y-no-requerido, y los campos `write_only` se excluyen del esquema de
respuesta. Esto es lo que alimenta la documentación de API generada — sin ningún
esquema separado que mantener.

> **Análisis en profundidad:** [OpenAPI](openapi.md) — convierte este esquema (más las
> rutas CRUD de tu ViewSet) en una especificación OpenAPI 3.1 completa servida con
> Swagger UI / Redoc.

---

## Scaffolding

Genera un esqueleto de serializador con la CLI de manage:

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Escribe un módulo inicial que rellenas:

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

Luego registra el módulo (`mod post_serializer;`) junto a los demás.

---

## Ajustes y límites actuales

Unos cuantos filos afilados y escotillas de escape que conviene conocer:

- **Campos condicionales.** No hay selección de campos en tiempo de ejecución (los
  campos están fijados en tiempo de compilación). Para "incluir solo cuando esté
  presente", usa `Option<T>` más
  `#[serde(skip_serializing_if = "Option::is_none")]` en el campo — la impl de
  `Serialize` personalizada respeta los atributos de serde.
- **Forma de salida personalizada.** Sobrescribe `to_value(&self)` en tu struct para
  un objeto JSON totalmente a medida cuando los atributos no basten.
- **Los objetos anidados escribibles** no están soportados — los campos `nested` /
  `many` / `slug` son solo de salida. Acepta las escrituras como ids escalares y
  resuélvelas tú mismo.
- **Los validadores integrados son solo de longitud/rango/opción** — `max_length` /
  `min_length` / `min` / `max` (y `choices` heredados) son declarativos; otras reglas
  (`email`, regex, …) son funciones que escribes tú (consulta
  [Validación](#validation)).
- **Un validador por campo por cada campo.** Para varias reglas en un campo,
  combínalas en la función de ese campo, o añade un `validate(&self)` entre campos.
- **El serializador no persiste.** Mapea → valida → entrega los datos al ORM; no hay
  `serializer.save()`.

---

## Pruébalo

El serializador mínimo se incluye en el ejemplo
[`getting_started_blog`](../crates/rustango/examples/getting_started_blog/src/post_serializer.rs)
(Paso 13 de la guía de primeros pasos). El comportamiento completo del derive — los
atributos de campo, los campos calculados/anidados/many, y ambas capas de validación —
está cubierto por las propias pruebas unitarias del framework (sin necesidad de base
de datos):

```bash
cd crates/rustango
cargo test --test serializer_derive          # field attrs, method, nested, many, slug, OpenAPI
cargo test --test serializer_cross_validate  # per-field + cross-field validation aggregation
```

---

## Véase también

- [ViewSets](viewsets.md) — conecta un serializador a una API CRUD JSON.
- [Vistas HTML](html-views.md) — la alternativa renderizada en el servidor a una API JSON.
- [OpenAPI](openapi.md) — los campos de un serializador se convierten en un esquema de componente.
- [Recetario del ORM](orm.md) — los modelos desde los que mapean los serializadores.
