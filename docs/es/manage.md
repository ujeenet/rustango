# Referencia de la CLI `manage`

Esta es la herramienta de línea de comandos de **Rustango**, como el `manage.py`
de Django, el `artisan` de Laravel o el comando `rails` de Rails. En un proyecto
generado mediante `cargo rustango new`, un único binario ejecuta cada comando
(«verbo»):

```bash
cargo run                          # runserver (no args = boot the HTTP server)
cargo run -- migrate               # any other verb
cargo run -- --help                # full subcommand list
```

[![One binary runs every manage verb — server, migrations, scaffolders, database utilities, and system commands — like Django's manage.py or Laravel's artisan](img/manage.png)](img/manage.png)

> **Fuente:** `rustango::manage` (`Cli`, el despachador de verbos) — detrás de
> la característica `manage` (activada por defecto).
>
> **Versión ejecutable:** cada verbo aquí se ejecuta en un proyecto generado; el
> ejemplo [`getting_started_blog`](../crates/rustango/examples/getting_started_blog)
> se maneja con `cargo run -- migrate` y compañía.

> **¿Nuevo con algún término aquí?** *scaffold*, *migration*, *tenant* — consulta
> el [glosario](glossary.md).

El enrutador de comandos vive en [`rustango::manage::Cli`](https://docs.rs/rustango/latest/rustango/manage/struct.Cli.html);
tu `src/main.rs` lo conecta así:

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

Los proyectos multi-tenant añaden `.tenancy()` a la cadena. Eso cambia el
enrutador a [`rustango::tenancy::manage`](https://docs.rs/rustango/latest/rustango/tenancy/manage/index.html)
y desbloquea los comandos multi-tenant.

> **Forma más antigua** — los proyectos generados por
> `manage startapp --with-manage-bin` (o los anteriores a v0.16) todavía
> incluyen `src/bin/manage.rs`. Estos usan
> `cargo run --bin manage -- <verb>`. Ambas formas aceptan los mismos verbos.

Cada comando imprime en stdout y termina con un código distinto de cero ante
errores de validación o de E/S. Ejecuta `cargo run -- --help` (o
`<verb> --help`) para obtener la ayuda de uso en línea.

---

## Tabla de contenidos

- [Migraciones](#migrations)
- [Migraciones de datos](#data-migrations)
- [Generadores de proyecto / app](#project--app-scaffolders)
- [Generadores de archivos (`make:*`)](#file-generators-make)
- [Utilidades de base de datos](#database-utilities)
- [Comandos del sistema](#system-commands)
- [Comandos de tenancy](#tenancy-commands)
- [Subcomandos personalizados](#custom-subcommands)
- [Flujos de trabajo comunes](#common-workflows)

---

## Migraciones

### `makemigrations [name]`

Genera un archivo de migración a partir de los cambios en tus modelos — como el
`makemigrations` de Django. Compara tus modelos registrados con la última
instantánea de esquema guardada en `migrations/` y escribe un nuevo archivo JSON
con lo que haya cambiado.

```bash
cargo run -- makemigrations                          # auto-name (e.g. 0004_add_slug_to_posts)
cargo run -- makemigrations rename_status_to_state   # custom suffix
```

**Cambios detectados automáticamente:**
- `CreateTable` / `DropTable`
- `AddColumn` / `DropColumn`
- `AlterColumnType` / `AlterColumnNullable` / `AlterColumnDefault` / `AlterColumnMaxLength`
- `AlterColumnUnique`
- `CreateIndex` / `DropIndex`
- `AddCheckConstraint` / `DropCheckConstraint`
- `CreateM2MTable` / `DropM2MTable`

**NO detectados automáticamente** (renombrar vs. eliminar+añadir es ambiguo):
- `RenameTable`, `RenameColumn` — usa `--empty` y edita el JSON.

### `makemigrations --app <app>`

Limita la migración a una sola app. Escribe en el propio directorio
`<project_root>/<app>/migrations/` de esa app y solo mira los modelos que
pertenecen a ella.

```bash
cargo run -- makemigrations --app blog
cargo run -- makemigrations --app blog backfill_slugs
```

### `makemigrations --scope <registry|tenant>`

Solo multi-tenant. Escribe una única migración solo para los modelos de un
scope — aquellos cuyo atributo `#[rustango(scope = "...")]` coincide. (Las
tablas «registry» se comparten entre todos los tenants; las tablas «tenant»
viven por tenant.) Sin este flag, un `makemigrations` simple en un proyecto de
tenancy divide automáticamente los cambios en DOS archivos — uno para los
modelos de registry, otro para los modelos de tenant — de modo que las tablas
compartidas del framework (`Org`, `Operator`) no se filtren en las migraciones
por tenant que ejecuta `migrate-tenants`.

```bash
cargo run -- makemigrations                       # tenancy: writes 0NN_<auto>.json (registry) + 0MM_<auto>.json (tenant) as needed
cargo run -- makemigrations --scope tenant        # explicit single-scope diff
cargo run -- makemigrations --scope registry      # explicit single-scope diff
```

Por qué importa la división: antes de v0.24.2, un `makemigrations` simple en un
proyecto de tenancy agrupaba operaciones sobre `rustango_operators` (una tabla
de registry) dentro de una migración de tenant. Cuando `migrate-tenants`
ejecutaba ese archivo, `rustango_operators` se resolvía vía `search_path` a la
copia de registry y chocaba con la restricción que ya estaba allí.

### `makemigrations --empty <name>`

Crea una migración en blanco (sin operaciones `forward`) para que la rellenes a
mano — como el `makemigrations --empty` de Django. Úsala cuando necesites
escribir operaciones de datos u operaciones de renombrado que el autodetector no
puede generar. Edita el JSON resultante tú mismo.

```bash
cargo run -- makemigrations --empty rename_status_to_state
# Then edit migrations/0005_rename_status_to_state.json:
#   "forward": [
#     {"schema": {"RenameColumn": {"table": "posts", "old_column": "status", "new_column": "state"}}}
#   ]
```

### `makemigrations --merge`

Corrige un historial de migraciones que se ha dividido en dos ramas — la misma
idea que el `makemigrations --merge` de Django (issue #346). Esto ocurre cuando
dos personas ejecutan cada una `makemigrations` en su propia rama de
característica, de modo que ambos archivos nuevos apuntan al mismo padre. Tras
fusionar ambas ramas, el historial tiene dos «hojas» (puntos finales), y el
siguiente `makemigrations` elegiría arbitrariamente una como su padre.

`--merge` detecta esto y escribe un `NNNN_merge.json` vacío cuyo padre apunta a
la última hoja en orden alfabético, reunificando el historial en una sola
cadena. Su instantánea de esquema refleja el estado combinado, leído del
registro de modelos vivo — los modelos de ambas ramas están compilados en este
punto, así que la instantánea es precisa.

```bash
cargo run -- makemigrations --merge
# wrote migrations/0004_merge.json
#     merge node — empty `forward`, anchors the chain after divergent leaves
```

- **Ya es una sola cadena** → imprime `no merge needed` y termina limpiamente.
  Seguro de ejecutar en un historial sano.
- **Historiales realmente separados** (no una colisión de ramas) → da un error
  en lugar de inventar un padre. La misma salvaguarda que usa Django.
- **No se puede combinar** con `--empty`, `--app`, `--scope` ni un nombre
  posicional.

### `migrate`

Aplica todas las migraciones pendientes a la base de datos, en orden — como el
`migrate` de Django o el `php artisan migrate` de Laravel. Este es el comando
que ejecutas después de `makemigrations` para cambiar realmente tu esquema.

```bash
cargo run -- migrate
cargo run -- migrate --dry-run                       # print SQL without writing
```

Cada archivo se ejecuta dentro de una transacción por defecto, de modo que un
fallo revierte todo el archivo. Establece `"atomic": false` en el JSON para no
usarla — la necesitas para sentencias como `CREATE INDEX CONCURRENTLY` que no
pueden ejecutarse dentro de una transacción.

En **modo tenancy** (`Cli::tenancy()`), `migrate` es consciente del scope:
primero aplica las migraciones de registry a la base de datos de registry
compartida, luego aplica las migraciones de tenant a través de cada tenant
activo. Para un control más fino, usa [`migrate-registry`](#migrate-registry) /
[`migrate-tenants`](#migrate-tenants).

### `migrate <target>`

Migra a un punto concreto del historial, hacia adelante o hacia atrás — como el
`migrate <app> <name>` de Django. Nombra una migración para moverte a ella; el
objetivo especial `zero` deshace todo.

```bash
cargo run -- migrate 0003_add_slug      # forward to 0003
cargo run -- migrate 0001_initial       # roll back to 0001 (unapply 0002+)
cargo run -- migrate zero               # unapply EVERY migration
```

### `migrate --squash`

Colapsa cada migración **pendiente** (sin aplicar) en un único diff recién
generado — la escotilla de escape para la iteración de desarrollo, para cuando
una pila de migraciones a medio terminar es más fácil de regenerar que de
arreglar. Se niega a tocar cualquier cosa ya aplicada.

```bash
cargo run -- migrate --squash
```

El archivo regenerado registra los nombres que colapsó en su lista `replaces`.
Eso importa en el momento en que hay otra base de datos involucrada: el checkout
de tu colega, staging o CI puede que ya hayan aplicado algunos de los archivos
que acabas de borrar. Sin `replaces`, el `CREATE TABLE` del archivo nuevo
chocaría allí; con él, el runner **reconcilia** en su lugar (ver más abajo).

### Reconciliación de squash

Un squash recrea el estado final de las migraciones que reemplaza, así que lo
que el runner debería hacer depende por completo de lo que la base de datos de
destino ya contenga. Lo decide automáticamente:

| estado de la base de datos | qué ocurre |
|---|---|
| nueva — sin historial, sin tablas | el squash se ejecuta de verdad |
| cada migración reemplazada está en el ledger | registrado, predecesores marcados como obsoletos, **sin DDL** |
| las tablas existen pero el ledger no tiene historial | registrado, **sin DDL** (`--fake-initial` de Django) |
| solo están presentes *algunas* filas / tablas reemplazadas | **rechazado**, nombrando lo que falta |

El caso parcial es deliberadamente un error rotundo: ninguna elección automática
es segura ahí, así que el runner se detiene y te dice lo que encontró en lugar
de adivinar. Resuélvelo con `migrate --fake` (abajo).

Las migraciones sustituidas por un squash aplicado se tratan como aplicadas, así
que puedes dejar los archivos viejos en disco durante una o dos releases — los
despliegues que nunca las ejecutaron migran hacia adelante correctamente de
todas formas.

Las migraciones ordinarias (que no son squash) no se ven afectadas: una
migración simple cuya tabla ya existe sigue fallando ruidosamente, porque eso es
un conflicto real y no un historial conocido-equivalente.

### `migrate --fake <name>`

Marca una migración como aplicada **sin ejecutar su SQL** — la escotilla de
escape del operador para cuando la base de datos ya está en el estado de destino
pero el ledger no lo sabe (una BD configurada fuera de banda, una tabla de
ledger eliminada, una migración parcialmente exitosa, un squash parcial
rechazado). Repite el flag para reparar varias filas de una vez.

```bash
cargo run -- migrate --fake 0004_add_indexes
cargo run -- migrate --fake 0004_add_indexes --system        # framework's own chain
cargo run -- migrate --fake 0004_add_indexes --all-tenants   # every active tenant
```

El nombre se valida primero contra el directorio de migraciones, de modo que un
error de tipeo no puede colar una fila falsa. Marcar es idempotente.

`--system` apunta a la propia cadena de migraciones del framework
(`system/migrations/`, registrada en `__rustango_system_migrations__`) en lugar
de la de tu proyecto. `--all-tenants` extiende la marca a través de cada tenant
activo, informando de cada uno y continuando más allá de los fallos — las tablas
del framework viven por tenant, así que repararlas es un trabajo por tenant.

### `downgrade [N]`

Revierte las últimas N migraciones aplicadas (por defecto 1) — el
`migrate:rollback` de Laravel. Cada migración debe ser reversible: los cambios
de esquema se revierten automáticamente, pero las operaciones de datos necesitan
un `reverse_sql` definido o el rollback falla.

```bash
cargo run -- downgrade                  # one step
cargo run -- downgrade 3                # three steps
```

### `showmigrations` / `status`

Lista cada migración y si se ha aplicado — como el `showmigrations` de Django.
`[X]` significa aplicada, `[ ]` significa aún pendiente.

```bash
cargo run -- showmigrations
cargo run -- status                     # alias
```

Salida:

```
[X] 0001_initial
[X] 0002_add_status
[ ] 0003_add_slug
```

---

## Migraciones de datos

### `add-data-op`

Añade un paso de datos en SQL crudo a una migración sin editar el JSON a mano.
Recurre a esto cuando necesites transformar filas existentes — rellenar una
columna, limpiar datos — como parte de una migración. Es el equivalente a la
migración de datos `RunSQL` de Django, generada para ti desde la línea de
comandos.

```bash
# New migration with up + down
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

# Append to an existing migration
cargo run -- add-data-op \
    --to 0003_add_slug \
    --sql "UPDATE posts SET slug = id::text"

# Irreversible (no rollback)
cargo run -- add-data-op \
    --sql "DELETE FROM legacy_data" \
    --name purge_legacy
```

| Flag | Requerido | Descripción |
|---|:-:|---|
| `--sql <SQL>` | sí | SQL hacia adelante que se ejecuta en `migrate` |
| `--reverse-sql <SQL>` | no | SQL de rollback en `unapply`; omítelo para hacerla irreversible |
| `--name <name>` | no | Sufijo del nombre de la nueva migración; por defecto `data_op` |
| `--to <migration>` | no | Añadir a una migración existente en lugar de crear una |

Omite `--reverse-sql` y el paso se marca como `reversible: false` — cualquier
intento de revertirlo falla de inmediato.

---

## Generadores de proyecto / app

### `cargo rustango new <name>` *(binario aparte)*

Crea un proyecto **Rustango** completamente nuevo — como `django-admin
startproject` o `laravel new`. Esta es una herramienta aparte, así que instálala
primero con `cargo install cargo-rustango`. Elige entre tres plantillas:

```bash
cargo rustango new myblog                          # default = fullstack (ORM + admin)
cargo rustango new myapi --template api            # JSON-only, no admin
cargo rustango new shop --template tenant          # multi-tenancy
```

Escribe:

```
<name>/
  Cargo.toml
  .env.example
  .gitignore
  rust-toolchain.toml
  docker-compose.yml
  README.md
  migrations/                               (your app's migrations)
  system/migrations/                        (tenant template — framework tables, generated)
  src/{main,models,views,urls}.rs
```

La plantilla de tenant incluye una carpeta `system/migrations/` **vacía**. Las
propias tablas del framework (`rustango_orgs`, `rustango_users`,
roles/permisos, …) se generan en ella a partir de los modelos compilados en el
primer `cargo run -- migrate` — no hay un JSON de bootstrap incluido a mano.
Consulta [`migrate`](#migrate) / [`migrate-registry`](#migrate-registry).

### `startapp <name> [flags]`

Crea una nueva app (un módulo de característica) bajo `src/<name>/` — exactamente
como el `startapp` de Django. Úsalo para mantener agrupados los modelos, vistas
y URLs de una parte de tu proyecto.

```bash
cargo run -- startapp blog
cargo run -- startapp shop --with-manage-bin             # also writes src/bin/manage.rs
cargo run -- startapp shop --into apps                   # write under src/apps/shop/ instead
```

Crea:

```
src/<name>/
  mod.rs
  models.rs
  views.rs
  urls.rs
```

Seguro de volver a ejecutar — los archivos existentes se dejan intactos. Un paso
manual: añade `pub mod <name>;` a `src/lib.rs` para que Rust compile el nuevo
módulo.

---

## Generadores de archivos (`make:*`)

Estos crean archivos de inicio para bloques de construcción comunes — muy
parecido a los comandos `make:*` de Laravel (`make:controller`, `make:model`,
…). Cada generador escribe en `src/<snake_name>.rs` (o `tests/<snake_name>.rs`
para `make:test`) y:

- Comprueba que el nombre es válido (PascalCase, letras/dígitos/guion bajo).
- Lo convierte a snake_case para el nombre del archivo (`PostViewSet` →
  `post_view_set.rs`).
- No sobrescribe un archivo existente.
- Te recuerda que añadas `pub mod X;` a tu `lib.rs`.

### `make:viewset <Name> [--model <Model>]`

Genera una estructura `#[derive(ViewSet)]` — un endpoint REST para un modelo,
como un ViewSet de Django REST Framework. Las listas de campos vienen
pre-esbozadas para que las rellenes.

```bash
cargo run -- make:viewset PostViewSet --model Post
```

`src/post_view_set.rs` generado:

```rust
#[derive(ViewSet)]
#[viewset(model = Post, fields = "id, ", filter_fields = "", search_fields = "", page_size = 20)]
pub struct PostViewSet;
```

Móntalo con: `.merge(PostViewSet::router("/api/posts", pool.clone()))`.

### `make:serializer <Name> [--model <Model>]`

Genera una estructura `#[derive(Serializer)]` — controla cómo un modelo se
convierte hacia y desde JSON (como un serializer de DRF).

```bash
cargo run -- make:serializer PostSerializer --model Post
```

### `make:form <Name>`

Genera una estructura `#[derive(Form)]` para validar y procesar la entrada de un
formulario — como un `Form` de Django.

```bash
cargo run -- make:form ContactForm
```

### `make:job <Name>`

Genera un esqueleto de trabajo en segundo plano (trabajo que se ejecuta fuera
del request, como una tarea de Celery o un job de Laravel), con un ejemplo
comentado de cómo programarlo.

```bash
cargo run -- make:job EmailDigestJob
```

### `make:notification <Name>`

Genera una estructura de notificación que construye un correo electrónico — como
el `make:notification` de Laravel.

```bash
cargo run -- make:notification WelcomeEmail
```

### `make:middleware <Name>`

Genera una función de middleware — código que se ejecuta antes y después de cada
request (comprobaciones de auth, logging, etc.). «axum» es el framework web
sobre el que está construido **Rustango**, así que el stub coincide con la forma
del middleware de axum.

```bash
cargo run -- make:middleware AuditLog
```

### `make:test <Name>`

Genera un test de integración en `tests/` que usa `TestClient` para hacer
requests contra tu app.

```bash
cargo run -- make:test post_smoke
```

---

## Utilidades de base de datos

### `db:info`

Muestra con qué base de datos está configurada esta build para hablar, sin
conectarse. Imprime la versión del framework, qué drivers de base de datos
(características de Cargo `postgres`/`mysql`) están compilados, la URL de conexión
con la contraseña oculta y el backend detectado. Como nunca abre una conexión,
resulta práctico en CI o contenedores donde la base de datos aún no está
levantada pero quieres confirmar que la configuración es correcta.

```bash
cargo run -- db:info
```

### `db:dump [--out <path>] [--data-only|--schema-only] [--no-owner]`

Respalda tu base de datos ejecutando `pg_dump` contra `DATABASE_URL` — como
`php artisan db:dump`. Por defecto el SQL va a stdout (para que puedas
canalizarlo); pasa `--out <path>` (`-o`) para escribir un archivo en su lugar.
`--data-only` y `--schema-only` se corresponden directamente con los flags de
`pg_dump`, y `--no-owner` omite las líneas OWNER. Necesitas `pg_dump` instalado
y en tu `PATH`.

```bash
cargo run -- db:dump > backups/before-migrate.sql    # stdout → file
cargo run -- db:dump --out backups/before-migrate.sql
```

### `db:restore <path> [--clean]`

Carga un archivo de dump de vuelta en tu base de datos — la contraparte de
`db:dump`. Pasa el archivo por `psql` contra `DATABASE_URL` con
`ON_ERROR_STOP=1`, de modo que se detiene ante el primer error. Añade `--clean`
para borrar primero el esquema existente (antepone
`DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;`) para que la
restauración aterrice en una base de datos vacía. Necesitas `psql` en tu `PATH`.

```bash
cargo run -- db:restore backups/before-migrate.sql
cargo run -- db:restore backups/before-migrate.sql --clean
```

---

## Comandos del sistema

### `version` / `--version`

Imprime la versión del framework **Rustango**.

```bash
$ cargo run -- version
rustango 0.44.0
```

### `about`

Imprime una instantánea de tu entorno: versión del framework, modelos y apps
registrados, si la base de datos es accesible, y variables de entorno clave.
Incluye esto en los tickets de soporte cuando algo va mal.

```bash
$ cargo run -- about
rustango
  version:        0.44.0
  models:         3 registered
  apps:           1 (blog)
  RUSTANGO_ENV:   local
  DATABASE_URL:   postgres://***@localhost:5433/myblog
  db_connect:     ok
```

### `check [--deploy]`

Ejecuta comprobaciones de salud en tu proyecto — como el `check` de Django.
Añade `--deploy` para las comprobaciones más estrictas de preparación para
producción, del mismo modo que funciona el `check --deploy` de Django.

**Comprobaciones siempre activas:**
- ≥ 1 modelo registrado vía `inventory`
- BD accesible (`SELECT 1`)
- Recuento de migraciones vs. recuento de modelos

**Con `--deploy`:**
- `RUSTANGO_ENV` es `prod` o `production`
- `RUSTANGO_SESSION_SECRET` establecido y ≥ 32 bytes (la clave HMAC para
  cookies + JWTs; el framework nunca lee `SECRET_KEY`)
- `DATABASE_URL` establecido
- `RUSTANGO_APEX_DOMAIN` establecido (proyectos de tenancy)

```bash
$ cargo run -- check --deploy
running rustango system check (deploy mode)...
  [info]    3 models registered via inventory
  [info]    database reachable
  [info]    4 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET length OK
all checks passed
```

Termina con un código distinto de cero si falla cualquier comprobación de nivel
de error. Las advertencias por sí solas no provocan un fallo.

### `docs`

Abre la documentación de **Rustango** (<https://docs.rs/rustango>) en tu
navegador. Siempre imprime también la URL, de modo que sigue funcionando en un
servidor sin interfaz gráfica.

```bash
cargo run -- docs
```

### `--help` / `help`

Lista cada comando con una descripción de una línea. En modo tenancy, también se
añaden los comandos multi-tenant listados abajo.

---

## Comandos de tenancy

Estos comandos existen solo en proyectos multi-tenant (una app que sirve a
muchos clientes/orgs aislados). Aparecen solo cuando el proyecto se compila con
`features = ["tenancy"]` Y `Cli::new()` se encadena con `.tenancy()`.

### `init-tenancy`

**No-op — conservado por compatibilidad.** El framework ya no incluye
migraciones de bootstrap construidas a mano. Sus propias tablas
(`rustango_orgs`, `rustango_operators`, `rustango_users`, roles/permisos, …) se
generan en `system/migrations/` a partir de los modelos compilados — el flujo
normal de Django (modelos → `makemigrations` → `migrate`) — y las aplica
[`migrate`](#migrate) / [`migrate-registry`](#migrate-registry), que las generan
bajo demanda si los archivos faltan.

```bash
cargo run -- init-tenancy   # does nothing now; kept so old scripts don't break
```

Las versiones más antiguas escribían aquí `0001_rustango_*_initial.json`; ese
flujo codificado a mano ya no existe. **Para aprovisionar, simplemente ejecuta
`cargo run -- migrate`.** Un modelo de usuario personalizado
(`.user_model::<AppUser>()`) fluye por las mismas `system/migrations/` generadas
— consulta [Modelo de usuario personalizado](#custom-user-model-extra-columns-on-rustango_users).

### `migrate-registry`

Aplica solo las migraciones de registry — las tablas compartidas entre tenants.
El registry contiene `rustango_orgs` y `rustango_operators` más cualquier tabla
con scope de registry que definas. Las tablas de tenant quedan intactas.

```bash
cargo run -- migrate-registry
```

### `migrate-tenants`

Aplica las migraciones de tenant a cada tenant activo, uno tras otro. Cada
tenant usa su propia conexión (su propio esquema o base de datos), y si un tenant
falla, el resto se ejecuta igualmente — el comando informa del resultado por
tenant al final.

```bash
cargo run -- migrate-tenants
```

Para el caso común, un `migrate` simple ya hace primero el registry, luego los
tenants — recurre a `migrate-tenants` solo cuando necesites ese paso por sí solo.

### `runserver` / `run-server`

Arranca el servidor web multi-tenant — el `runserver` de Django. En un proyecto
de tenancy esto es lo mismo que un `cargo run` pelado; la forma con nombre existe
para que los binarios personalizados que parsean sus propios argumentos aún
puedan activarlo.

```bash
cargo run                        # implicit
cargo run -- runserver           # explicit
```

### `create-tenant <slug> [options]`

Configura un nuevo tenant (cliente/org) y le aplica las migraciones de tenant. El
`<slug>` es su identificador corto. Seguro de volver a ejecutar — llamarlo de
nuevo sobre un tenant existente no duplicará nada.

```bash
cargo run -- create-tenant acme --display-name "ACME Corp"
cargo run -- create-tenant beta --mode database --database-url postgres://...
```

| Flag | Descripción |
|---|---|
| `--display-name <name>` | Etiqueta legible por humanos que se muestra en las barras laterales del admin |
| `--mode schema \| database` | Modo de almacenamiento (por defecto: schema) |
| `--database-url <url>` | URL de BD específica del tenant (requerida para el modo database) |
| `--host-pattern <pattern>` | Anula el patrón de host usado por `SubdomainResolver` |
| `--no-migrate` | Omite aplicar las migraciones con scope de tenant tras el aprovisionamiento |

### `drop-tenant <slug> [--confirm <slug>]`

Desactiva un tenant estableciendo `active = false`. Esta es la opción suave y
reversible — los datos del tenant permanecen en disco, y volver a ejecutar
`create-tenant` lo trae de vuelta. Cuando no estás ejecutando de forma
interactiva (sin terminal adjunto), debes pasar `--confirm <slug>` con el slug
tecleado de nuevo para confirmar.

```bash
cargo run -- drop-tenant acme --confirm acme
```

### `purge-tenant <slug> [--confirm <slug>] [--purge-database]`

**Elimina un tenant permanentemente.** Borra el esquema del tenant y elimina su
fila de `rustango_orgs`, sin deshacer posible. Cuando no estás ejecutando de
forma interactiva (sin terminal adjunto), debes pasar `--confirm <slug>` con el
slug tecleado de nuevo. Para los tenants en modo database, la base de datos
subyacente se deja en su sitio a menos que también pases `--purge-database`.

```bash
cargo run -- purge-tenant acme --confirm acme
cargo run -- purge-tenant beta --confirm beta --purge-database   # database-mode: also DROP DATABASE
```

### `list-tenants`

Lista cada tenant con su modo de almacenamiento y su estado activo/inactivo.

```bash
cargo run -- list-tenants
```

### `create-operator <username> --password <pwd>`

Crea un operador — un admin global que puede gestionar cada tenant desde una
consola entre tenants. Los operadores viven en el registry compartido, no dentro
de un tenant concreto.

```bash
cargo run -- create-operator admin --password letmein
```

### `create-user <tenant> <username> --password <pwd> [--superuser]`

Crea un usuario dentro de un tenant — aproximadamente el `createsuperuser` de
Django, pero limitado a un solo tenant.

```bash
cargo run -- create-user acme alice --password hunter2 --superuser
```

`--superuser` establece `is_superuser = true` para ese usuario dentro del
tenant. Eso lo convierte en admin del tenant (acceso de escritura completo en el
admin del tenant), pero nunca otorga acceso a la consola de operador entre
tenants.

### `create-role <tenant> <name>`

Crea un rol (un paquete con nombre de permisos, como un grupo de Django) dentro
de un tenant.

```bash
cargo run -- create-role acme editor
```

### `list-roles <tenant>`

Lista los roles definidos en un tenant dado.

```bash
cargo run -- list-roles acme
```

### `assign-role <tenant> <username> <role>`

Otorga a un usuario uno de los roles del tenant.

```bash
cargo run -- assign-role acme alice editor
```

### `revoke-role <tenant> <username> <role>`

Elimina un rol de un usuario — lo inverso de `assign-role`.

```bash
cargo run -- revoke-role acme alice editor
```

### `grant-perm <tenant> <role-name|username> <codename> [--role]`

Otorga un único permiso. Por defecto, el segundo argumento es un **nombre de
usuario**, así que el permiso va directamente a ese usuario; añade `--role` para
otorgarlo a un rol en su lugar. Los codenames de permisos usan el formato
`<app>.<action>_<model>` de Django (`blog.add_post`, `blog.change_post`, …). La
característica `auto_create_permissions` crea automáticamente los cuatro
codenames CRUD estándar para cualquier modelo marcado con
`#[rustango(permissions)]`.

```bash
cargo run -- grant-perm acme alice blog.change_post           # grant to user alice
cargo run -- grant-perm acme editor blog.change_post --role   # grant to role editor
```

### `revoke-perm <tenant> <role-name|username> <codename> [--role]`

Elimina un permiso — lo inverso de `grant-perm`. Apunta a un usuario por
defecto; añade `--role` para revocarlo de un rol en su lugar.

```bash
cargo run -- revoke-perm acme alice blog.change_post
cargo run -- revoke-perm acme editor blog.change_post --role
```

### `create-api-key <tenant> <username> [--label <s>]`

Emite una clave de API para un usuario de tenant. El token completo se imprime
**una vez** y nunca más — cópialo ahora, porque solo se almacenan su prefijo y
un hash.

```bash
cargo run -- create-api-key acme alice --label "ci-bot"
```

### `audit-cleanup`

Poda las entradas antiguas del registro de auditoría (`rustango_audit_log`) para
que no crezca indefinidamente. Recorta por antigüedad (`--days`) o por cantidad
(`--keep-last`), y opcionalmente limítalo a un tenant.

```bash
cargo run -- audit-cleanup --days 90                       # delete > 90 days old
cargo run -- audit-cleanup --keep-last 50                  # keep most recent 50 per row
cargo run -- audit-cleanup --keep-last 50 --tenant acme    # scoped
```

---

## Modelo de usuario personalizado (columnas extra en `rustango_users`)

Esta es la versión de **Rustango** del «custom user model» de Django — cómo
añades tus propios campos a la tabla de usuarios. El `User` de tenant integrado
tiene siete columnas fijas: `id`, `username`, `password_hash`, `is_superuser`,
`active`, `created_at`, más una columna `data` JSONB (un blob JSON flexible) para
cualquier metadato extra por usuario. **Para la mayoría de las apps esa columna
JSONB es todo lo que necesitas** — sin migración, sin override, sin sorpresas.

Cuando quieras columnas **tipadas e indexables** en `rustango_users` en su lugar,
hay dos enfoques. No son intercambiables; elige el que encaje con dónde está tu
proyecto en su vida.

### Opción 1 — Modelo de perfil hermano con FK *(funciona en cualquier proyecto)*

Lo mejor cuando el proyecto ya existe, o cuando prefieres dejar la tabla `User`
del framework como única fuente de verdad.

```rust
#[derive(rustango::Model)]
pub struct UserProfile {
    #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>,
    #[rustango(fk = "rustango_users")] pub user_id: i64,
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
```

Ejecuta `cargo run -- makemigrations` y luego `cargo run -- migrate`, y tendrás
una tabla de extras tipada enlazada al usuario por clave foránea. Léela con el
ORM:

```rust
let profile = UserProfile::objects()
    .where_(UserProfile::user_id.eq(user.id.get().copied().unwrap()))
    .first(&pool).await?;            // Option<UserProfile>
```

Contrapartida: una fila extra y un JOIN en cada acceso. Ventaja: cero riesgo de
romper la auth del framework.

### Opción 2 — `Cli::user_model::<AppUser>()` *(solo greenfield)*

Usa esto solo en un proyecto nuevo donde quieras los campos extra directamente
en la propia tabla `rustango_users`. Como `AppUser` *es* el modelo
`rustango_users`, sus columnas fluyen por el motor ordinario de `makemigrations`
→ `migrate`: las tablas del framework se generan en `system/migrations/`, así
que las columnas de `AppUser` aterrizan en el `CREATE TABLE rustango_users`
generado.

**Paso 1.** Define tu modelo. Tiene que declarar cada columna requerida por el
framework exactamente (`id`, `username`, `password_hash`, `is_superuser`,
`active`, `created_at`, `data`), más tus extras. Cada columna extra debe permitir
`NULL` o tener un `default = "…"`.

```rust
use rustango::sql::Auto;

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "rustango_users")]
pub struct AppUser {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)] pub username: String,
    #[rustango(max_length = 255)] pub password_hash: String,
    pub is_superuser: bool,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[rustango(default = "'{}'")] pub data: serde_json::Value,
    // extras —
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
impl rustango::tenancy::TenantUserModel for AppUser {}
```

**Paso 2.** Conecta el override en `main.rs`:

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::manage::Cli::new()
        .api(my_app::urls::router())
        .tenancy()
        .user_model::<AppUser>()
        .run().await
}
```

**Paso 3.** Registra `AppUser` **en lugar del** `User` del framework — solo un
modelo puede reclamar `table = "rustango_users"`. El scaffolder no incluye JSON
de bootstrap estático (solo un `system/migrations/` vacío), así que no hay nada
que borrar; simplemente no registres también el `User` del framework.

**Paso 4.** Genera + aplica:

```bash
cargo run -- makemigrations       # generates system/migrations/ with AppUser's columns
cargo run -- migrate              # creates rustango_users with your extras
```

**Advertencias:**

- Cambiar `AppUser` más adelante es un cambio de esquema normal: vuelve a
  ejecutar `makemigrations` para emitir la migración `AddColumn`, luego
  `migrate`.
- Solo un modelo puede mapear a `rustango_users`. Registrar **ambos**, el `User`
  del framework y tu `AppUser`, hace que `makemigrations` sea ambiguo — registra
  `AppUser` solo. Esta es la razón principal por la que la Opción 2 es solo para
  proyectos nuevos; en un proyecto existente, la Opción 1 evita el problema.
- El código de auth y admin del framework lee las siete columnas núcleo por
  nombre; tus columnas extra solo son accesibles a través de
  `AppUser::objects().fetch(...)`.

`Builder::user_model::<AppUser>()` hace lo mismo para el código que construye el
`Builder` del servidor directamente, sin pasar por `Cli`.

---

## Subcomandos personalizados

Puedes añadir tus propios comandos — la versión de **Rustango** de los comandos
de gestión personalizados de Django. El truco es inspeccionar los argumentos tú
mismo y manejar tu comando antes de pasar el resto a `Cli::run`. Dos formas de
hacerlo:

**En línea en `src/main.rs`** (sin binario extra):

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("import-csv")) {
        let url = std::env::var("DATABASE_URL")?;
        let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;
        return my_csv_importer::run(&pool, &args[1..]).await;
    }
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

**Vía `--with-manage-bin`** (`src/bin/manage.rs` aparte):

```bash
cargo run -- startapp app --with-manage-bin
```

Luego en `src/bin/manage.rs`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = std::env::var("DATABASE_URL")?;
    let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;

    match args.first().map(String::as_str) {
        Some("import-csv") => my_csv_importer::run(&pool, &args[1..]).await,
        _ => rustango::migrate::manage::run(&pool, "./migrations".as_ref(), args)
            .await
            .map_err(Into::into),
    }
}
```

Ejecuta tus propios comandos igual que los integrados:
`cargo run -- import-csv path/to/file.csv` (o
`cargo run --bin manage -- import-csv …` cuando uses `--with-manage-bin`).

---

## Flujos de trabajo comunes

### Configuración inicial del proyecto (single-tenant)

```bash
cargo rustango new myapp
cd myapp
cp .env.example .env             # edit DATABASE_URL
docker compose up -d
cargo run -- migrate
cargo run                        # serve at :8080
```

### Configuración inicial del proyecto (tenancy)

```bash
cargo rustango new myapp --template tenant
cd myapp
cp .env.example .env             # edit DATABASE_URL + RUSTANGO_APEX_DOMAIN
docker compose up -d
cargo run -- migrate                                      # registry + tenants
cargo run -- create-operator admin --password letmein
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
cargo run                        # serve at :8080
```

### Añadir tenants después de que la app ya está en marcha

Una app de tenancy real normalmente acumula modelos y migraciones mucho antes de
que se registre su primer tenant. Este flujo funciona en cualquier punto de la
vida del proyecto:

```bash
# 1. (any time) develop user models — define structs with #[derive(Model)],
#    add `pub mod ...;` to src/lib.rs.
# 2. Generate scope-aware migrations. In a tenancy project this writes
#    up to TWO files: one tagged registry-scope (touches Org/Operator),
#    one tagged tenant-scope (touches User + your models). Pre-v0.24.2
#    this used to dump everything into one tenant-scoped file and
#    crash on `create-tenant` — see the changelog.
cargo run -- makemigrations

# 3. Apply migrations. `migrate` is scope-aware: it runs registry-
#    scoped files once against the registry pool first, then fans
#    tenant-scoped files across every active tenant.
cargo run -- migrate

# 4. Provision a NEW tenant whenever (could be days, weeks, many
#    migrations later). The tenancy code applies every accumulated
#    tenant-scoped migration to the new tenant's schema in one pass —
#    the new tenant arrives at the same schema state as existing ones.
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
```

Por qué esto es seguro:
- `#[rustango(scope = "registry")]` en `Org`/`Operator` mantiene los cambios en
  las tablas compartidas fuera de las migraciones por tenant.
- `migrate-tenants` visita cada tenant activo y aplica solo las migraciones de
  tenant — los archivos de registry se omiten.
- `create-tenant` ejecuta ese mismo paso de `migrate-tenants` contra el esquema
  del nuevo tenant, así que arranca completamente al día sin arreglos manuales.

### Añadir un modelo

```bash
cargo run -- startapp blog        # if not done yet
# Edit src/blog/models.rs — add #[derive(Model)]
# Add `pub mod blog;` to src/lib.rs
cargo run -- makemigrations
cargo run -- migrate
```

### Añadir una API JSON para ese modelo

```bash
cargo run -- make:viewset PostViewSet --model Post
# Edit src/post_view_set.rs — fill in field lists
# Mount in src/urls.rs
cargo run                        # GET /api/posts now works
```

### Añadir un backfill de datos

```bash
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title) WHERE slug IS NULL" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs
cargo run -- migrate
```

### Auditoría previa al despliegue

```bash
cargo run --release -- check --deploy
```

### Revertir la última migración

```bash
cargo run -- downgrade 1
```

### Aplicar una migración de tenancy a un scope específico

```bash
cargo run -- migrate-registry            # registry-scoped only
cargo run -- migrate-tenants             # tenant-scoped, fan-out across orgs
```

### Dar de baja un tenant

```bash
cargo run -- drop-tenant acme            # soft (reversible)
cargo run -- purge-tenant acme           # hard (drops schema/db)
```

---

## Ajuste fino del pool de tenants (v0.27.7+)

Los tenants en modo database obtienen su propio pool de conexiones (un `PgPool`
— un conjunto de conexiones de base de datos reutilizadas), cacheado por slug en
[`TenantPools`](../crates/rustango/src/tenancy/pools.rs). Por defecto un pool se
construye **de forma perezosa, en el primer request del tenant**, a menos que
actives el pre-calentamiento. Los ajustes viven en `TenantPoolsConfig`:

| Campo | Por defecto | Propósito |
|---|---|---|
| `max_cached_database_pools` | 64 | Tope de la caché de pools. Una vez llena, el siguiente tenant no cacheado da error (sin desalojo silencioso). |
| `database_pool_max_connections` | 4 | `max_connections` por pool. Mantenlo pequeño para que un fan-out de tenants no agote el `max_connections` de PG. |
| `database_pool_min_connections` | 0 | Mantiene N conexiones calientes en todo momento. `≥1` reduce la latencia del primer request al pagar el round-trip de TCP/TLS/auth en el arranque. |
| `database_pool_acquire_timeout` | 30s | Cuánto espera `pool.acquire()` antes de dar error `PoolTimedOut`. |
| `database_pool_idle_timeout` | 10 min | Cierra las conexiones inactivas tras esta duración. Se defiende de los cortes por load-balancer / `idle_in_transaction_session_timeout`. |
| `database_pool_max_lifetime` | 30 min | Fuerza la rotación de conexiones para que las credenciales arrendadas por vault se refresquen. |
| `prewarm_active_tenants` | false | Cuando es true, `Server::Builder::serve` llama a `prewarm_database_tenants()` en el arranque. |

### Pre-calentar en el arranque

Dos formas de activarlo:

1. **Automática** — establece `prewarm_active_tenants = true` en el
   `TenantPoolsConfig` que le pasas a `TenantPools::new(...).config(...)`.
   `Server::Builder::serve` ejecuta el pre-calentamiento antes de hacer el bind.

2. **Verbo de CLI** — `cargo run -- prewarm-pools` construye pools para cada
   tenant activo en modo database y termina. Útil como hook posterior al
   despliegue (p. ej., tras una rotación de credenciales), o para validar que
   cada tenant es accesible antes de conmutar un load-balancer.

El pre-calentamiento recorre `Org::objects().where(active = true, storage_mode =
"database")` y hace corto-circuito cuando se alcanza el tope de la caché
(reportado como `skipped_cap` en el [`PrewarmReport`]). Los fallos de
construcción por tenant registran un `tracing::warn!` pero no abortan el bucle.

### Tracing

`crate::tenancy::pools::tenant_pool_init` es un `tracing::info_span!` que envuelve
la construcción del pool en la ruta fría. Suscríbete a él para ver la latencia de
construcción por tenant:

```text
INFO crate::tenancy::pools: tenant pool connected (database mode)
     slug=acme elapsed_ms=42 min_conn=1 max_conn=4
```

### Trampa de configuración — TLDs `.local` de macOS

Si accedes al admin del tenant vía `http://acme.local:8080/admin/` en macOS y ves
una pausa de 5 segundos en cada request: eso es **Bonjour / mDNS**, no
**Rustango**. El resolver de macOS trata `.local` de forma especial y espera el
timeout completo de mDNS antes de recurrir a `/etc/hosts`. Dos soluciones:

1. **Usa un TLD diferente**: `127.0.0.1 acme.localhost` funciona sin retraso.
   `localhost` está reservado (RFC 6761) y omite mDNS.
2. **Ejecuta dnsmasq** con una zona `.local` que apunte a 127.0.0.1 para que el
   SO obtenga una respuesta inmediata.

Confírmalo con `curl -w "%{time_connect}\n"`: si `time_connect` muestra ~5s pero
cae a milisegundos con `--resolve acme.local:8080:127.0.0.1`, estás topándote con
mDNS.


---

## Véase también

- [Recetario del ORM](orm.md)
- [Scaffolding](scaffolding.md)
- [ViewSets](viewsets.md)
- [Serializers](serializers.md)
- [Guía de seguridad](security.md)
