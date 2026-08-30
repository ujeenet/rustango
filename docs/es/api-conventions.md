# Convenciones de la API

> **Para quién es esta página.** Esta es una **referencia avanzada para desarrolladores de Rust** que trabajan *con* o *en* el código del framework — explica las convenciones de nombres, tipos de retorno y módulos detrás de la API de Rust de Rustango. **No** es una guía para *llamar* a la API REST de una aplicación Rustango por HTTP. Si eso es lo que buscas, empieza por los [ViewSets](viewsets.md) (construir una API REST) y el [glosario](glossary.md) (términos en lenguaje llano); vuelve aquí cuando estés escribiendo Rust contra el framework.

Esta página explica los patrones que sigue la API de **Rustango**, para que puedas predecir cómo se comporta cualquier método antes de leer su documentación. Si estás contribuyendo o auditando una funcionalidad, estas son las reglas.

[![Convención de nombres de Rustango: el sufijo del método te dice qué recibe — `*_on` para un pool tipado, sin sufijo para el pool multi-backend, y señales sin pool](../img/api-conventions.png)](../img/api-conventions.png)

## Tabla de contenidos

- [Naming](#naming)
- [Constructors](#constructors)
- [Return types](#return-types)
- [Async vs sync](#async-vs-sync)
- [The pool argument](#the-pool-argument)
- [Filtering](#filtering)
- [Errors](#errors)
- [Module naming](#module-naming)
- [Builders vs config structs](#builders-vs-config-structs)
- [Feature flags](#feature-flags)
- [Macros vs runtime](#macros-vs-runtime)
- [Contributing](#contributing)

---

## Nombres

El nombre de un método te dice lo que hace. Una vez que aprendes estos sufijos, puedes adivinar la mayor parte de la API.

### Funciones

- **`save_on(executor)`, `delete_on(executor)`** — los métodos de escritura reciben un *executor* (un pool, una conexión o una transacción — lo que habla con la base de datos). El sufijo `_on` significa «ejecuta esto contra el executor que te entrego».
- **`fetch_on(executor)`, `count_on(executor)`** — el mismo sufijo `_on`, para las lecturas.
- **`save()`, `fetch()`, `count()`** sin `_on` — atajo que llama a la versión `_on` con un `&pool` por defecto. Solo funciona donde el queryset o el modelo ya guardan una referencia al pool (raro en código de aplicación).
- **`from_X(value)`** — convierte DESDE otro valor (p. ej. `from_model(post)`, `from_base32(s)`).
- **`with_X(value)`** — un método de builder que establece una opción y devuelve el objeto, de modo que puedes encadenar llamadas (p. ej. `with_default_ttl(d)`, `with_access_ttl(secs)`).
- **`new()`** — el constructor mínimo. Cualquier argumento que reciba es una dependencia obligatoria (p. ej. `RedisCache::new(url)` — no puedes construir la caché sin una URL).

### Tipos

Estos siguen la convención de mayúsculas estándar de Rust, la misma división de PEP 8 de Python entre clases y funciones:

- **`PascalCase`** — tipos, traits y variantes de enum (como las clases de Python).
- **`snake_case`** — módulos, funciones, campos y variables locales.
- **`SCREAMING_SNAKE_CASE`** — constantes, más la constante `Model::SCHEMA` que la macro derive genera para cada modelo.
- **`Boxed*`** — un alias de `Arc<dyn Trait>`, un puntero compartido thread-safe a un objeto de trait (la forma en Rust de sostener «cualquier implementación de esta interfaz»). Por ejemplo `BoxedCache = Arc<dyn Cache>`. Este es el tipo estándar para un backend intercambiable que puedes sustituir.

### Módulos

- **Singular** cuando el módulo alberga UN tipo o concepto principal: `cache`, `email`, `storage`, `signed_url`, `request_id`.
- **Plural** cuando el módulo alberga una COLECCIÓN de elementos: `bulk_actions`, `api_keys`, `passwords`, `forms`, `signals`.

---

## Constructores

Cómo construyes un objeto depende de lo que necesita. Hay unas pocas formas estándar:

| Patrón | Cuándo | Ejemplo |
|---|---|---|
| `T::new()` | Mínimo — sin dependencias obligatorias | `InMemoryCache::new()`, `Validator::new()` |
| `T::new(arg)` | Una dependencia obligatoria | `EnvSecrets::with_prefix(s)`, `RedisCache::new(url)` |
| `T::with_X(arg)` | Sobrescritura estilo builder tras `new()` | `InMemoryCache::with_default_ttl(d)`, `JwtLifecycle::new(s).with_access_ttl(60)` |
| `T::from_X(arg)` | Convertir DESDE Y | `TotpSecret::from_base32(s)`, `Locale::new(s)` (a veces `from_str`) |
| `T::for_Y(arg)` | Construir un T acotado a un Y específico | `ViewSet::for_model(schema)` |

**Evita esto:** `T::with_X_and_Y_and_Z(a, b, c)` — un único constructor que recibe todo. Divídelo en `new(...)` más llamadas encadenadas `.with_*()`.

---

## Tipos de retorno

El tipo de retorno de un método te dice cómo puede fallar. Rust no tiene excepciones, así que el fallo forma parte del valor de retorno. Hay tres formas.

**`Result<T, E>`** — como una función que o bien devuelve un valor o bien lanza una excepción. Obtienes o el valor `T` o un error `E` con detalles. Úsalo para operaciones que pueden fallar y donde el *porqué* importa:
- E/S: `pool.fetch(...).await -> Result<_, sqlx::Error>`
- Validación: `Form::parse(data) -> Result<Self, FormErrors>`
- Emisión: `JwtLifecycle::issue_pair_with(uid, claims) -> Result<_, JwtIssueError>`

**`Option<T>`** — o un valor (`Some`) o nada (`None`), como un campo anulable. Úsalo cuando «no se encontró nada» es un resultado normal y no necesitas un mensaje de error que explique por qué:
- Búsquedas: `cache.get(k) -> Result<Option<String>, _>` (el `Result` cubre el fallo de E/S; el `Option` cubre «clave ausente»)
- Verificación: `async JwtLifecycle::verify_access(token) -> Option<Claims>` («expirado o inválido» es un resultado esperado, así que `None` basta)
- Lecturas de configuración opcionales: `env::optional("FOO") -> Result<Option<T>, _>`

**`bool`** — un simple sí/no cuando no se necesita más detalle:
- `cache.exists(k) -> Result<bool, _>` (el `Result` cubre la E/S; el `bool` es la respuesta)
- `JwtLifecycle::revoke(token) -> bool` (true = añadido a la lista negra)
- `disconnect_pre_save(id) -> bool` (true = se eliminó una entrada)

**¿`Result<Option<T>>` o `Result<T>` con un error `NotFound`?** Ambos pueden expresar «la búsqueda falló», así que elige según lo excepcional que sea «no encontrado»:
- Usa `Result<Option<T>>` cuando «no encontrado» es rutinario — tu código casi siempre se ramifica sobre `Some`/`None` de todos modos.
- Usa `Result<T>` con una variante de error `NotFound` cuando «no encontrado» es excepcional — algo que registrarías como una advertencia o convertirías en un 404.

---

## Async vs sync

La regla general: si un método espera algo (la base de datos, la red o el disco), es `async` y debes hacerle `.await`. Si solo calcula, es una llamada síncrona normal. Esta tabla lo detalla.

| Operación | ¿Sync o async? |
|---|---|
| Método de trait que toca E/S (BD, red, archivo) | **async** |
| Método de trait de puro cómputo (`hash`, `verify`, `encode`) | **sync** |
| Métodos de builder (`with_X`, setters encadenables) | **sync** |
| Macros (`derive(Model)`, `derive(Serializer)`) | **N/A** (en tiempo de compilación) |
| Señal `connect_*` (registra un receptor) | **sync** |
| Señal `send_*` (despacha a receptores async) | **async** |

**Excepción:** `Cache::set` es `async` aunque la versión en memoria (`InMemoryCache::set`) nunca espera realmente. El trait está modelado para el caso de Redis, que sí lo hace. Es intencional: un método de trait debería ser `async` si *cualquier* implementación razonable necesita esperar, de modo que todos los backends compartan una misma firma.

---

## El argumento pool

Cada llamada al ORM recibe un pool o executor (el handle de base de datos) como **último** argumento. Pasas la conexión cada vez, en lugar de depender de un estado global oculto:

```rust
post.save_on(&pool).await?
Post::objects().filter(...).fetch_on(&pool).await?
send_post_save(&post, ctx).await                  // ⚠️ no pool — signals are pool-free
```

**Una excepción:** las señales no reciben un pool, porque nunca tocan la base de datos. La regla se mantiene: todo lo que llega a la BD recibe el pool; todo lo que no, no.

**¿Por qué pasarlo cada vez?** Rust prefiere las dependencias que puedes ver sobre el estado global oculto. Django mantiene la conexión en almacenamiento thread-local, pero eso se desmorona en el mundo async de Rust, donde una tarea puede saltar entre hilos a mitad de una petición. La desventaja es más tecleo; la ventaja es que puedes hacer grep de cada lugar que toca la base de datos.

Si te encuentras pasando `&pool` a través de diez capas de llamadas a funciones, acepta `impl Executor` una sola vez en el punto de entrada público y deja que los helpers internos compartan esa única conexión.

---

## Filtrado

Hay tres maneras de filtrar un queryset, y todas se combinan en una misma consulta. Elige según de dónde venga el filtro.

```rust
// 1. HTTP query string (set via ViewSet filter_fields, parsed at request time)
//    GET /api/posts?author_id=42&status__ne=archived

// 2. String-keyed (lookup at compile of the queryset; runtime field name resolution)
Post::objects().filter("author_id", Op::Eq, SqlValue::I64(42));

// 3. Typed columns (compile-time field check)
Post::objects().where_(Post::author_id.eq(42));
```

| Sintaxis | Úsala cuando |
|---|---|
| Query HTTP | Endpoints de API públicos — el ViewSet los analiza por ti, como los backends de filtro de DRF |
| `.filter` por clave-cadena | Código CRUD genérico o de admin, donde los nombres de campos vienen de la configuración y no se conocen en tiempo de compilación |
| `.where_` tipado | El código de tu aplicación — la opción por defecto recomendada. El compilador comprueba que el campo existe y que los tipos coinciden |

Puedes **mezclar las tres** en un mismo queryset.

---

## Errores

**Rustango** tiene **más de 20 tipos de error** — uno por módulo — en lugar de una única clase de excepción para todo. Forman una jerarquía laxa, y un tipo de nivel superior los une de modo que rara vez lidias con ellos individualmente.

| Capa | Módulo | Tipo de error |
|---|---|---|
| E/S del ORM | `sql::*` | `ExecError` |
| Escritor SQL del ORM | `sql::*` | `SqlError` (variante de `ExecError::Sql`) |
| Migraciones | `migrate::*` | `MigrateError` |
| Formularios | `forms::*` | `FormError` (único) + `FormErrors` (múltiple) + `ModelFormError` |
| Caché | `cache::*` | `CacheError` |
| Email | `email::*` | `MailError` |
| Almacenamiento | `storage::*` | `StorageError` |
| Backends de autenticación | `tenancy::auth_backends` | `AuthError` |
| JWT | `tenancy::jwt_lifecycle` | `JwtIssueError` |
| Claves de API | `api_keys::*` | `ApiKeyError` |
| Contraseñas | `passwords::*` | `PasswordError` |
| Webhooks | `webhook::*` | (devuelve bool, sin error dedicado) |
| URLs firmadas | `signed_url::*` | `SignedUrlError` |
| Acciones masivas | `bulk_actions::*` | `BulkActionError` |
| Fixtures | `fixtures::*` | `FixtureError` |
| Filtro de IP | `ip_filter::*` | `IpFilterError` |
| i18n | `i18n::*` | `I18nError` |
| Env | `env::*` | `EnvError` |
| Secrets | `secrets::*` | `SecretsError` |
| Respuestas de API | `api_errors::*` | `ApiError` (con forma HTTP, no interno) |

**El que hay que usar en los handlers:** existe un enum `RustangoError` de nivel superior (exportado desde `lib.rs`, junto con el alias `RustangoResult<T> = Result<T, RustangoError>`). Envuelve cada uno de los errores anteriores con conversiones `From`, de modo que el operador `?` promueve automáticamente cualquier error de módulo hacia él. También implementa `IntoResponse`, lo que significa que cada variante se mapea a un estado HTTP sensato cuando se devuelve desde un handler. La división es simple: usa los errores específicos por módulo en lo profundo de tu código, y `RustangoError` / `RustangoResult` en la frontera del handler. Para errores de crates de terceros, `RustangoError::other(msg)` / `RustangoError::other_from(e)` envuelven cualquier `std::error::Error + Send + Sync + 'static`.

**Un ejemplo de handler:**

```rust
use rustango::api_errors::ApiError;

async fn handler() -> Result<Json<X>, ApiError> {
    let post = Post::objects().get(&pool, 1).await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(post))
}
```

`ApiError` implementa `IntoResponse`, así que devolverlo produce automáticamente la forma JSON de error estándar.

---

## Nombres de módulos

El nombre de un módulo debería permitirte **adivinar los nombres de los tipos que contiene** sin abrir el archivo.

| Módulo | Alberga | Confianza de búsqueda |
|---|---|---|
| `cache` | el trait `Cache`, las impls `*Cache` | alta |
| `email` | el trait `Mailer`, `Email`, las impls `*Mailer` | alta |
| `storage` | el trait `Storage`, las impls `*Storage` | alta |
| `signed_url` | las funciones libres `sign`, `verify` | media |
| `text` | las funciones libres `slugify`, `html_escape`, `truncate` | media |
| `bulk_actions` | `BulkActionRegistry`, `BulkAction`, las impls `Bulk*Action` | alta |
| `api_keys` | las funciones libres `generate_key`, `verify_key`, `split_token` | media |

**Evita esto:** un módulo que alberga un cajón de sastre heterogéneo (`utils`, `helpers`, `common`). Si no puedes nombrar el único concepto que cubre, no debería ser un módulo.

---

## Builders vs structs de configuración

Hay dos maneras de entregar un objeto configurado. Elige según cómo lo configurarán los usuarios.

### Builder: setters encadenados, sin `Default`

```rust
let l = SecurityHeadersLayer::strict()
    .csp(...)
    .header("x-extra", "v");
```

Úsalo cuando:
- La mayoría de los usuarios parten de un preajuste y lo modifican
- Los setters expresan intención (p. ej. `.errors_only()` se lee mejor que `.log_success(false)`)
- El struct tiene muchos campos opcionales (10+)

### Struct de configuración: establecer campos directamente, recurrir a `Default`

```rust
let l = AccessLogLayer {
    log_success: false,
    include_ip: true,
    slow_threshold_ms: 500,
    ..Default::default()
};
```

Úsalo cuando:
- Los usuarios quieren ser explícitos sobre cada campo
- La reflexión / serialización importa
- Actualizar en el sitio es común (`config.field = ...`)

**Como regla, **Rustango** usa builders** para el middleware HTTP (`security_headers`, `cors`, `rate_limit`, etc.) y structs de configuración para los simples portadores de datos (`Email`, `AccessLogLayer`, el estado interno de `RateLimitLayer`).

---

## Feature flags

Un *feature* es un flag de compilación de Cargo (el `[features]` de `Cargo.toml`) que activa o desactiva una parte del crate — similar al package discovery de Laravel o a los `INSTALLED_APPS` de Django, pero resuelto en tiempo de compilación. Cada módulo que arrastra una dependencia extra queda detrás de uno. El conjunto por defecto es «casi seguro que quieres estos»:

```toml
default = [
    "postgres", "manage", "admin", "config", "forms", "serializer",
    "cache", "signals", "email", "storage", "scheduler", "secrets", "totp",
    "webhook", "webhook-delivery", "api_keys", "passwords", "signed_url",
    "notifications", "casts", "jobs", "jobs-postgres", "auth_flows", "sse",
    "websocket", "oauth2", "http-client", "compression", "openapi",
    "csp-nonce", "sessions", "hmac-auth", "jwt", "uploads", "storage-s3",
    "media", "runserver", "template_views",
]
```

**Desactivados por defecto:** los features que arrastran dependencias pesadas o servicios externos:
- `tenancy` — añade `argon2`, `hmac`, `sha2`, `cookie`, `tower` (la mayoría de las aplicaciones no lo necesitan)
- `cache-redis` — añade el crate `redis` (a la mayoría de las aplicaciones les basta con la caché en memoria)
- `csrf` — se activa automáticamente con `admin`, pero también está disponible por su cuenta

Para adelgazar un binario que no necesita todo, desactiva los valores por defecto y lista solo lo que uses:

```toml
rustango = { version = "0.44", default-features = false, features = ["postgres", "admin"] }
```

---

## Macros vs runtime

Una *macro* es código que genera código en tiempo de compilación (`#[derive(Model)]` y compañía) — aproximadamente lo que hace un generador de Rails, salvo que se ejecuta en cada compilación y el compilador comprueba el resultado. La división de abajo decide qué hace una macro frente a código de runtime plano.

| Aspecto | ¿Macro o runtime? |
|---|---|
| Metadatos de esquema para `inventory` | macro (`#[derive(Model)]`) |
| Construcción de consultas guiada por el esquema | runtime (usa el `&'static ModelSchema` de la macro) |
| Parseo de formularios | macro para el struct (`#[derive(Form)]`); runtime para la lógica de parseo |
| Selección de campos del serializer | macro (`#[derive(Serializer)]`) — emite un `from_model` + una impl `Serialize` personalizada |
| Operaciones de migración | runtime (diff de `SchemaSnapshot`) |
| Despacho de señales | runtime (registro indexado por `TypeId`, sin macro por modelo) |
| Coincidencia de patrones de los backends de autenticación | runtime (`#[async_trait]` sobre `AuthBackend`) |

**Regla:** usa una macro para todo lo que el compilador pueda verificar de antemano (los nombres de campos deben existir, los tipos deben coincidir). Usa código de runtime para todo lo que varíe por petición o por despliegue.

---

## Contribuir

Cuando añadas una nueva funcionalidad, sigue estos pasos:

1. **Un módulo por concepto**, en `crates/rustango/src/<name>.rs` o `<name>/mod.rs`.
2. **Añade rustdoc a nivel de módulo** con un ejemplo «Quick start» en un bloque `// ignore`.
3. **Añade un feature flag si arrastras una nueva dependencia** — nómbralo según el módulo (`feature = "<name>"`).
4. **Reexporta el módulo desde `lib.rs`** con una rustdoc de una línea.
5. **Pon los tests unitarios en el mismo archivo**, tras `#[cfg(test)] mod tests` — sin base de datos a menos que realmente necesites una.
6. **Pon los tests de integración en `crates/rustango/tests/<name>.rs`** para la historia de extremo a extremo.
7. **No añadas un nuevo tipo de error a menos que los existentes no encajen** — extiende primero un enum existente.
8. **Sigue la [guía de tipos de retorno](#return-types)** al elegir entre `Result`, `Option` o `bool`.
9. **¿Añades un subcomando de `manage`?** Conéctalo en el dispatcher `match cmd` y en `print_help`, añade un test en `crates/rustango/tests/migrate_manage.rs`, y documenta una fila en `docs/manage.md`.
10. **Actualiza `CHANGELOG.md`** con una entrada `Added` bajo la próxima versión.

Cuando rompas la API:
- Marca el elemento antiguo con `#[deprecated(since = "...", note = "use X instead")]` y consérvalo durante una versión minor completa antes de retirarlo.
- Regístralo en `CHANGELOG.md` bajo `Breaking changes`.
- Enlaza la ruta de migración desde las notas de la versión.
