# Registro de logs

Los logs son la forma en que una aplicación en ejecución cuenta lo que hizo.
**Rustango** se apoya en [`tracing`](https://docs.rs/tracing) — la misma forma
que el ajuste `LOGGING` de Django o los canales de Laravel, pero estructurada:
un evento lleva campos con nombre (`status=500`, `tenant=acme`) en lugar de una
frase ya formateada, de modo que un agregador de logs puede filtrar por ellos.

Un proyecto generado con el scaffolder ya registra logs. Esta página trata de lo
que se ajusta: qué nivel, qué subsistemas, qué formato, a dónde va la salida, y
cómo distinguir el tráfico de un inquilino del de otro.

> **¿Nuevo en **Rustango**?** El [glosario](glossary.md) cubre los bloques de
> construcción del framework. El vocabulario de logging — *nivel*, *target*,
> *span* — se explica en esta página a medida que aparece.

> **Fuente:** `rustango::logging` (`setup`, `setup_for_env`, `Setup`,
> `Rotation`, `DEFAULT_FILTER`) — el módulo no está tras una feature, pero todo
> instalador necesita la feature `runtime`. `Setup::from_settings` necesita
> además `config`. `rustango::access_log` y `rustango::tenant_log` necesitan
> `admin` **o** `tenancy`; `rustango::tracing_layer` necesita `admin`.
>
> **Versión ejecutable:** los ajustes y valores por defecto de aquí están
> fijados por
> [`logging_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_doc.rs)
> (`cargo test -p rustango --test logging_doc`), el sumidero a fichero por
> [`logging_file_appender_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_file_appender_live.rs),
> y el campo de inquilino del log de acceso por
> [`access_log_tenant_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/access_log_tenant_sqlite_live.rs).

## Tabla de contenidos

- [Lo que ya tienes](#lo-que-ya-tienes)
- [Niveles y el filtro que los elige](#niveles-y-el-filtro-que-los-elige)
- [Targets: nombrar el subsistema](#targets-nombrar-el-subsistema)
- [Elegir un formato](#elegir-un-formato)
- [Configurar el logging desde los ajustes](#configurar-el-logging-desde-los-ajustes)
- [Escribir a un fichero](#escribir-a-un-fichero)
- [El log de acceso](#el-log-de-acceso)
- [¿De qué inquilino era?](#de-qué-inquilino-era)
- [Spans de petición y OpenTelemetry](#spans-de-petición-y-opentelemetry)
- [Logging en los tests](#logging-en-los-tests)
- [No sale nada](#no-sale-nada)

---

## Lo que ya tienes

`#[rustango::main]` instala un subscriber antes de que corra tu código. Un
proyecto generado lo recibe gratis, y por eso `cargo run` imprime logs sin
ninguna llamada de configuración:

```rust,ignore
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Aquí ya hay instalado un subscriber `tracing_subscriber::fmt`.
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

Usa `RUST_LOG` cuando está definida, e `info,sqlx=warn` cuando no — el valor de
`rustango::logging::DEFAULT_FILTER`. Ese valor por defecto es deliberado: sqlx
registra cada sentencia en `info`, así que un `info` sin filtrar entierra tus
propios eventos bajo el SQL.

Para configurar algo más que el nivel, instala tú el subscriber. Todos los
instaladores son idempotentes (`try_init` por debajo), así que una llamada de
más no hace nada en lugar de provocar un panic:

```rust,ignore
fn main() {
    rustango::logging::setup();   // pretty, filtro de entorno, "info,sqlx=warn"
    // ...
}
```

## Niveles y el filtro que los elige

Cinco niveles, del más ruidoso al más silencioso: `error`, `warn`, `info`,
`debug`, `trace`. Un filtro indica el máximo detalle que quieres, global o por
módulo:

```sh
RUST_LOG=info                                  # todo desde info hacia arriba
RUST_LOG=debug,sqlx=warn,hyper=warn            # debug para ti, dependencias calladas
RUST_LOG=warn,rustango::tenancy=debug          # un subsistema, a todo volumen
RUST_LOG=rustango=info                         # solo el framework
```

Define el valor de reserva para cuando `RUST_LOG` no esté — que es la mayoría de
los despliegues en producción, donde el filtro pertenece a la configuración y no
al entorno:

```rust,ignore
rustango::logging::Setup::new()
    .with_default_env_filter("info,sqlx=warn,hyper=warn")
    .install();
```

`RUST_LOG` siempre gana a ese valor de reserva. No hay forma de configurar un
filtro que el entorno no pueda sobrescribir, por diseño — quien depura un
incidente en vivo no debería tener que desplegar una compilación.

**Qué nivel esperar y dónde.** Los eventos `warn` del framework suelen compartir
una forma que merece una alerta: *rustango hizo algo distinto de lo que pediste*.
Un `format` desconocido en los ajustes, una cláusula de índice descartada porque
el backend no puede expresarla, una ruta de admin que colisionó y se omitió, una
llamada a `update` con la lista de campos vacía — todas continúan, y el `warn` es
la única señal de que lo hicieron.

## Targets: nombrar el subsistema

Cada evento lleva un **target**, y es contra eso que casa
`RUST_LOG=<target>=<nivel>`. Los eventos del framework viven bajo la raíz
`rustango::`, así que `RUST_LOG=rustango=warn` los alcanza todos:

| Target | Qué cubre |
|---|---|
| `rustango::admin` | Rutas y registro del admin |
| `rustango::admin::audit` | Escrituras del log de auditoría |
| `rustango::admin::sso` | SSO del admin |
| `rustango::cache` | Backends de caché |
| `rustango::cache_page` | Middleware de caché de página |
| `rustango::cors` | Decisiones de política CORS |
| `rustango::email` | Envío de correo |
| `rustango::email::smtp` | Transporte SMTP |
| `rustango::humanize` | Filtros de humanize |
| `rustango::jobs` | Colas de trabajos en segundo plano |
| `rustango::logging` | Avisos del propio subsistema |
| `rustango::manage` | Verbos de `manage` |
| `rustango::messages` | Mensajes flash |
| `rustango::migrate` | Ejecutor de migraciones |
| `rustango::rate_limit` | Limitación de tasa |
| `rustango::request_timeout` | Timeout por petición |
| `rustango::scheduler` | Cron / tareas programadas |
| `rustango::server` | Arranque y apagado del servidor |
| `rustango::shutdown` | Manejo de señales y hooks de apagado |
| `rustango::sql` | Ejecución de consultas |
| `rustango::sql::lock` | Cláusulas de bloqueo de fila |
| `rustango::template_views` | Vistas basadas en plantillas |
| `rustango::tenancy` | Multi-tenancy, general |
| `rustango::tenancy::admin` | Admin de inquilino |
| `rustango::tenancy::migrate_run` | Migraciones por inquilino |
| `rustango::tenancy::operator_console` | Consola de operador |
| `rustango::tenancy::pools` | Ciclo de vida de los pools de inquilino |
| `rustango::tenancy::provision` | Aprovisionamiento de inquilinos |
| `rustango::tenancy::provision_webhook` | Webhooks de aprovisionamiento |
| `rustango::tenancy::resolver` | Resolución de inquilino |
| `rustango::tenancy::sso` | SSO de inquilino |
| `rustango::tenancy::sweep` | Barridos de retención |

Esos son los targets que el framework nombra explícitamente. Los eventos que no
nombran ninguno heredan su ruta de módulo, lo que da la misma forma:
`rustango::access_log` y `rustango::tracing_layer` se alcanzan igual que las
filas de arriba.

> **Un target es una cadena, no una ruta.** `target: "crate::cache"` compila sin
> problema y luego se queda en un espacio de nombres que ningún filtro casa.
> Cuarenta y ocho puntos de llamada habían derivado así antes de que
> [`tracing_targets.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/tracing_targets.rs)
> empezara a romper la compilación por ello. Si añades targets en tu propio
> código, usa el nombre real de tu crate.

## Elegir un formato

| Formato | Para qué | Cómo |
|---|---|---|
| `pretty` | Desarrollo — color, multilínea, legible | por defecto |
| `json` | Producción — un objeto por evento, para Loki / CloudWatch / Datadog | `.json()` |

```rust,ignore
rustango::logging::Setup::new()
    .json()
    .with_default_env_filter("info")
    .install();
```

O deja que decida el entorno. `setup_for_env()` lee `RUSTANGO_ENV` y elige JSON
cuando vale `prod` o `production`, y pretty en el resto de casos:

```rust,ignore
rustango::logging::setup_for_env();
```

Dos ajustes de presentación merecen la pena: `.with_line_numbers()` añade la
posición en el código (útil en desarrollo, ruidoso en producción), y
`.without_targets()` oculta la columna de target — hazlo solo si has renunciado
a filtrar por ella.

> Cada valor de formato llega a su propio formateador. `compact` se aceptaba y
> luego se renderizaba como el valor por defecto — corregido (#1480).

## Configurar el logging desde los ajustes

Todo lo anterior tiene equivalente en TOML, para que un despliegue cambie su
logging sin recompilar. La sección es `[logging]`:

```toml
# config/dev_settings.toml
[logging]
level             = "info,sqlx=warn"
format            = "pretty"
with_line_numbers = true
```

```toml
# config/prod_settings.toml
[logging]
level         = "info"
format        = "json"
file_dir      = "/var/log/myapp"
file_prefix   = "app"
file_rotation = "daily"
```

| Clave | Tipo | Por defecto | Notas |
|---|---|---|---|
| `level` | cadena | `info,sqlx=warn` | Sintaxis de `RUST_LOG`. Solo se usa si `RUST_LOG` no está definida |
| `format` | cadena | `full` | `full` / `pretty` / `compact` / `json`. Un valor desconocido cae en `full` con un `warn` |
| `color` | cadena | `auto` | `auto` / `always` / `never`. `auto` colorea solo una terminal y respeta `NO_COLOR` |
| `access_log` | bool | `true` | Una línea por petición, más el span que lleva `tenant` a los eventos del handler |
| `with_thread_ids` | bool | `false` | ID de hilo en cada evento |
| `with_line_numbers` | bool | `false` | Línea de código en cada evento |
| `without_targets` | bool | `false` | Ocultar la columna de target |
| `file_dir` | cadena | sin definir | Definirla activa el sumidero a fichero |
| `file_prefix` | cadena | `app` | Raíz del nombre de fichero |
| `file_rotation` | cadena | `daily` | `daily` / `hourly` / `minutely` / `never`. Un valor desconocido cae en `daily` con un `warn` |
| `file_only` | bool | `false` | Quitar stdout. No hace nada sin `file_dir` |

Se aplica con una llamada en la `Cli`:

```rust,ignore
rustango::manage::Cli::new()
    .with_settings_from_env()
    .with_logging()               // instala desde Settings.logging
    .api(urls::api())
    .run()
    .await
```

`with_logging()` es opcional — desactivado por defecto, para que un proyecto que
llama a `logging::setup()` por su cuenta no reciba un segundo instalador. El
orden en la cadena da igual: la instalación ocurre en `run()`, contra los
ajustes finales.

> **`#[rustango::main]` le gana.** La macro instala un subscriber antes incluso
> de construir el runtime, y todos los instaladores usan `try_init`: gana el
> primero y los posteriores se descartan en silencio. Un `main` que conserva la
> macro *y* llama a `with_logging()` se queda por tanto con los valores por
> defecto de la macro: tu sección `[logging]` se lee y luego no surte efecto,
> sin que nada lo indique. Para logging dirigido por los ajustes, cambia
> `#[rustango::main]` por `#[tokio::main]` — es lo único que un proyecto
> generado necesita tocar. Seguimiento en
> [#1465](https://github.com/ujeenet/rustango/issues/1465).

Cualquier clave se puede sobrescribir por despliegue con una variable de
entorno, usando la sección y la clave como segmentos de ruta:

```sh
RUSTANGO__LOGGING__LEVEL=debug
RUSTANGO__LOGGING__FORMAT=json
```

Para construir el subscriber tú mismo desde la misma sección — cuando necesitas
el guard que devuelve, ver más abajo — usa `Setup::from_settings`:

```rust,ignore
let settings = rustango::config::Settings::load_from_env()?;
let _guard = rustango::logging::Setup::from_settings(&settings.logging).install();
```

## Escribir a un fichero

Los logs van a stdout salvo que pidas otra cosa, que es lo correcto en un
contenedor. Cuando necesitas ficheros, `with_file` escribe además en un appender
rotativo:

```rust,ignore
use rustango::logging::{Rotation, Setup};

let _guard = Setup::new()
    .json()
    .with_file("/var/log/myapp", "app", Rotation::Daily)
    .install();
```

Con `Daily` los ficheros quedan en `{dir}/{prefix}.YYYY-MM-DD`, y el directorio
se crea en la primera escritura. `Rotation` es `Daily`, `Hourly`, `Minutely` o
`Never`. Añade `.file_only()` para quitar la capa de stdout — para un worker sin
terminal o un proceso demonizado, donde nadie lee stdout de todos modos.

> **Mantén vivo el guard.** `install()` devuelve
> `Option<tracing_appender::non_blocking::WorkerGuard>` — `Some` cuando hay un
> sumidero a fichero configurado. El escritor de fichero no bloquea, así que un
> disco atascado no puede detener la atención de peticiones; el precio es que
> los eventos en búfer se vuelcan cuando el guard se destruye. Átalo a la vida
> del proceso (un `static`, un `OnceLock`, o un `let` en `main` que sobreviva a
> todo). `let _ = ...install();` lo destruye de inmediato y pierdes escrituras.
> `Cli::with_logging()` lo mantiene por ti.

## El log de acceso

Un evento por petición completada, con los campos que un operador busca:

```rust,ignore
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};

let app = router.access_log(AccessLogLayer::default());
```

```text
INFO rustango::access_log: http.request.method=GET url.path=/api/posts url.query=page=2 http.response.status_code=200 duration_ms=12 client.address=192.0.2.1 tenant=acme
```

El nivel tiene significado, así que las alertas pueden basarse en él:

| Condición | Nivel |
|---|---|
| Respuesta normal | `info` |
| Estado >= 400 | `warn` |
| Más lento que `slow_threshold_ms` (1000 por defecto) | `warn`, mensaje `slow request` |

Ajustes:

```rust,ignore
AccessLogLayer::default()
    .errors_only()               // omitir 2xx/3xx por completo
    .slow_threshold_ms(250)      // qué cuenta como lento
    .without_ip()                // omitir la IP del cliente
    .trust_proxy_headers(true)   // X-Forwarded-For, solo tras un proxy de confianza
```

Los parámetros de consulta que llevan credenciales se enmascaran con
`[redacted]` antes de escribir la línea — `password`, `passwd`, `token`,
`secret`, `api_key`, `apikey`, `access_token`, `refresh_token`, `signature`,
`auth`. Amplía con `.redact_additional("session_id")`, o sustituye la lista
entera con `.redact(vec![...])`. Esto cubre **solo las cadenas de consulta**;
para el resto, ver
[security.md](security.md#mantener-los-secretos-fuera-de-tus-registros).

## ¿De qué inquilino era?

`tenant` nombra al inquilino al que se resolvió la petición, y es `-` cuando no
se resolvió ninguno — una petición al dominio apex o a la consola de operador, o
una aplicación de un solo inquilino. Nunca queda en blanco, de modo que «sin
inquilino» se lee distinto de un campo que se perdió.

No hay nada que cablear: `ChainResolver` publica la identidad cuando resuelve
una, y el log de acceso la lee de vuelta. El mecanismo es
[`rustango::tenant_log`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/src/tenant_log.rs),
una ranura por petición que existe porque la identidad del inquilino vive en un
extractor — dentro del handler, por debajo del middleware que necesita
registrarla.

El slug lo elige el operador y a menudo es el nombre del cliente. Cuando los
logs salen de tu infraestructura, etiqueta por id:

```rust,ignore
use rustango::access_log::{AccessLogLayer, TenantField};

AccessLogLayer::default().tenant_field(TenantField::Id)     // tenant=42
AccessLogLayer::default().tenant_field(TenantField::Both)   // tenant=acme#42
AccessLogLayer::default().tenant_field(TenantField::Off)    // tenant=-
```

> **Solo el camino de la petición.** Un trabajo en segundo plano corre fuera de
> la petición que lo encoló y no tiene inquilino que registrar — la ranura es
> por tarea, y `tokio::spawn` no la hereda. Seguimiento en
> [#1229](https://github.com/ujeenet/rustango/issues/1229) /
> [#1223](https://github.com/ujeenet/rustango/issues/1223).

## Spans de petición y OpenTelemetry

`TracingLayer` envuelve cada petición en un span siguiendo las
[convenciones semánticas de OpenTelemetry v1.30](https://opentelemetry.io/docs/specs/semconv/http/http-spans/),
de modo que un colector no necesita reglas de renombrado de atributos:

```rust,ignore
use rustango::tracing_layer::TracingLayer;
use tower::ServiceBuilder;

let app = ServiceBuilder::new().layer(TracingLayer::new()).service(router);
```

El span lleva `http.request.method`, `url.path`, `url.query`,
`network.protocol.version`, `user_agent.original`,
`http.response.status_code`, `http.response.body.size`, `duration_ms`, y
`tenant` / `org_id` en cuanto se resuelve un inquilino. Cuando la petición llega
con una cabecera `traceparent` de W3C, también se registran `trace_id`,
`parent_span_id` y `trace_flags`, que es lo que una capa
`tracing-opentelemetry` recoge para unirse a la traza.

Merece la pena instalar esta capa aunque solo sea por el campo de inquilino:
como los campos están en el *span*, cada evento emitido durante la petición —
incluidos los del ORM — los lleva en su contexto de span, sin que ningún
subsistema sepa qué es un inquilino. **No** se instala por defecto; ni
`server::Builder`, ni `Cli`, ni el scaffolder la añaden.

## Logging en los tests

Los instaladores usan `try_init`, así que llamar a `logging::setup()` desde un
test es seguro aunque otro test ya haya instalado uno. Para hacer aserciones
sobre la salida, prefiere un subscriber acotado a uno global:

```rust,ignore
let subscriber = tracing_subscriber::fmt()
    .with_writer(make_writer)
    .with_max_level(tracing::Level::INFO)
    .finish();
let _guard = tracing::subscriber::set_default(subscriber);
```

`set_default` es local al hilo y devuelve un guard que restaura el subscriber
anterior, de modo que los tests en paralelo no se estorban. `#[tokio::test]`
corre un runtime de hilo actual, lo que mantiene todo el future en el hilo que
cubre el guard.

## No sale nada

- **`RUST_LOG` definida y aun así silencio.** El filtro se lee una vez, al
  instalar. Definir la variable después de `setup()` no cambia nada.
- **Tu propio crate calla en `info`.** `RUST_LOG=info` aplica a todos los
  targets; si pusiste `RUST_LOG=rustango=info`, filtraste fuera tu código.
  Nombra ambos: `RUST_LOG=info,rustango=warn`.
- **Un filtro sobre `crate::algo` no casa nada.** Los targets son cadenas; ver
  la nota en [Targets](#targets-nombrar-el-subsistema).
- **Fichero vacío tras un fallo.** El guard de `install()` se destruyó, o el
  proceso murió antes de que el appender volcara. Ver
  [Escribir a un fichero](#escribir-a-un-fichero).
- **Dos subscribers, el segundo ignorado.** `try_init` significa que gana el
  primero, en silencio. Si llamas a `logging::setup()` *y* a
  `Cli::with_logging()`, pierde el de los ajustes.
