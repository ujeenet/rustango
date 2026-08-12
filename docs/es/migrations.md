# Migraciones y el motor de migraciones

**Rustango** incluye un motor de migraciones al estilo de Django: editas tus
modelos, ejecutas `makemigrations` para generar un archivo JSON versionado que
describe el cambio de esquema, y `migrate` para aplicarlo. Desde la **0.48** el
framework incluso migra **sus propias** tablas `rustango_*` a través del mismo
motor — sin DDL de arranque escrito a mano. Esta página explica las piezas
móviles que hacen que una actualización sea segura: las dos cadenas de
migración, la reconciliación de squash y el fake-initial protegido que permite
que una base de datos preexistente adopte el motor sin colisiones.

> **¿Nuevo en las migraciones?** Los verbos de la CLI del día a día —
> `makemigrations`, `migrate`, `migrate --squash`, `migrate --fake`,
> `downgrade`, `showmigrations` — se tratan comando por comando en la
> [guía de manage](manage.md#migrations). Esta página es el modelo conceptual
> que hay detrás de ellos.

> **Fuente:** `rustango::migrate` (`runner`, `make`, `file`, `manage`)
> y `rustango::tenancy::migrate` — la reconciliación del runner vive en
> `migrate::runner::reconcile`.

---

## Dos cadenas de migración

Una migración pertenece a una de dos cadenas independientes, cada una con su
propia tabla de registro (la fila de nombres aplicados que el runner consulta
para omitir trabajo):

| Cadena | Qué gestiona | Tabla de registro |
|---|---|---|
| **Proyecto** | tus tablas `#[derive(Model)]`, en `migrations/` | `__rustango_migrations__` |
| **Sistema** | las tablas `rustango_*` propias del framework (`Org`, `User`, roles/permisos, agents, media, …), en `system/migrations/` | `__rustango_system_migrations__` |

La **cadena del sistema** es lo que hace que el esquema del framework sea
autodescriptivo. Sus archivos se generan a partir de los modelos compilados del
framework — y son **conscientes de `#[cfg(feature = …)]`**: una columna o tabla
protegida por una feature es eliminada por el compilador cuando la feature está
desactivada, de modo que activar una feature hace que `makemigrations` emita un
`AddColumn` / `CreateTable` y desactivarla emite un `DropColumn` / `DropTable`.
Los proyectos de tenant generados por scaffolding incluyen un `system/migrations/`
**vacío**; el primer `cargo run -- migrate` lo genera y aplica (consulta
[scaffolding](scaffolding.md)).

`migrate` aplica la cadena del sistema **antes** que las migraciones de tu
proyecto. En modo de tenancy, los dos ámbitos se solapan deliberadamente en las
tablas compartidas del framework, por lo que la cadena de ámbito de tenant es la
que se ejecuta; las tablas exclusivas del registro (`rustango_orgs`,
`rustango_operators`) no significan nada sin tenancy. Las aplicaciones sin
tenancy que usan un subsistema del framework (por ejemplo `media`) también
reciben la aplicación de la cadena del sistema.

---

## Reconciliación de squash — `Migration.replaces`

Un **squash** colapsa una serie de migraciones históricas en un único archivo
recién generado que recrea el mismo estado final — útil cuando una pila de
migraciones a medio terminar es más fácil de regenerar que de arreglar. El
inconveniente: los `CREATE TABLE` del archivo colisionarían en cualquier base de
datos que ya aplicó las migraciones que colapsó (el checkout de un colega,
staging, CI).

`migrate --squash` resuelve esto estampando la lista **`replaces`** del nuevo
archivo con los nombres que colapsó:

```jsonc
{
  "name": "0007_squashed_0001_0006",
  "replaces": ["0001_initial", "0002_add_status", "0003_add_slug", "…"],
  "forward": [ /* recreates the end state */ ]
}
```

Con `replaces` definido, el runner **reconcilia** el squash contra el estado
real de la base de datos en lugar de ejecutarlo a ciegas. La decisión es
automática y depende por completo de lo que ya esté presente:

| Estado de la base de datos | Qué hace el runner |
|---|---|
| nueva — sin historial, sin tablas | ejecuta el squash de verdad |
| todas las migraciones reemplazadas están en el registro | lo registra, marca como tombstone a las predecesoras, **sin DDL** |
| las tablas existen pero el registro no tiene historial | lo registra, **sin DDL** (el `--fake-initial` entre registros de Django) |
| solo *algunas* de las filas/tablas reemplazadas presentes | **rechazado** — indica qué falta y te dice que lo resuelvas a mano |

El caso **parcial** es un error grave a propósito: ninguna elección automática
es segura ahí, así que el runner se detiene y reporta lo que encontró en lugar
de adivinar. Resuélvelo con `migrate --fake` (más abajo).

Las migraciones sustituidas por un squash aplicado cuentan como aplicadas, así
que puedes dejar los archivos colapsados en disco durante una o dos releases —
los despliegues que nunca las ejecutaron migran hacia adelante correctamente de
todos modos. Las migraciones ordinarias (que no son squash) no se ven afectadas:
una migración corriente cuya tabla ya existe sigue fallando ruidosamente, porque
eso es un conflicto real, no un historial equivalente conocido.

---

## El fake-initial protegido de reconciliación

Este es el mecanismo que permite que una base de datos **existente** adopte la
cadena del sistema sin problemas. Antes de la 0.51 el framework construía
algunas de sus tablas mediante DDL en crudo con `ensure_table` de forma
perezosa; esas tablas existen pero no están registradas en el registro
`__rustango_system_migrations__`, así que un nuevo `CREATE TABLE` de la nueva
migración de sistema colisionaría (`relation "rustango_media" already exists`,
MySQL 1050, …).

La cadena del sistema reconcilia esto por sí misma. Se inspecciona una migración
de **sistema** pendiente: las operaciones que componen *la creación de sus
tablas* — `CreateTable`, más `CreateIndex` / `CreateM2MTable` que apuntan a una
tabla que la misma migración crea — son el conjunto aceptado. Si **todas** las
tablas que crea ya existen, la migración se **registra en el registro sin
ejecutar ningún DDL**, y los datos existentes se dejan intactos. Si solo
*algunas* de sus tablas existen, la cadena crea únicamente las que faltan
(semántica `CREATE TABLE IF NOT EXISTS`) y deja las demás en paz.

La protección es deliberadamente estrecha:

- **Limitada a la propia cadena del sistema del framework.** Las migraciones de
  usuario usan el runner corriente y nunca hacen auto-fake — el faking por
  existencia de tabla es opt-in solo para la ruta del sistema.
- **Cualquier cosa que no sea creación de tablas descalifica el faking** — un
  índice sobre una tabla preexistente, un alter / drop / operación de datos /
  callback recae en una ejecución real, así que nunca se omite trabajo genuino.
- **La existencia se pregunta únicamente al namespace actual** — `current_schema()`
  en Postgres, `DATABASE()` en MySQL, `sqlite_master` en SQLite — no a través del
  `search_path`, de modo que en la multi-tenancy de modo esquema una tabla con el
  mismo nombre en `public` no puede engañar a un tenant para que omita sus propias
  tablas.

El estado parcial de un squash sigue siendo rechazado (ver arriba); solo la
cadena del sistema del framework hace la reparación por partes de "crear las que
faltan".

---

## Reparar drift a mano — `migrate --fake`

Cuando la base de datos ya está en el estado objetivo pero el registro no lo
sabe (una BD configurada fuera de banda, un registro eliminado, una migración
parcialmente exitosa, un squash parcial rechazado), estampa una migración como
aplicada **sin ejecutar su SQL**:

```bash
cargo run -- migrate --fake 0004_add_indexes
cargo run -- migrate --fake 0001_rustango_registry_initial --system       # framework's own chain
cargo run -- migrate --fake 0001_rustango_registry_initial --all-tenants  # every active tenant
```

- `--system` estampa la cadena del sistema del framework
  (`system/migrations/` → `__rustango_system_migrations__`) en lugar de la de tu
  proyecto.
- `--all-tenants` propaga el sello a través de cada tenant activo, reportando
  cada uno y continuando tras los fallos — las tablas del framework viven por
  tenant, así que repararlas es un trabajo por tenant. Combínalo con `--system`
  para las tablas del framework en todos los tenants.

El nombre se valida primero contra el directorio de migraciones, así que un
error tipográfico no puede colar una fila falsa; el estampado es idempotente, y
la opción puede repetirse para reparar una serie de filas en un solo comando.

---

## Actualizar a la 0.51.2

> **La 0.51.0 y la 0.51.1 fueron retiradas (yanked)** — la reconciliación que
> prometían nunca llegó a dispararse de verdad contra bases de datos reales de la
> 0.46–0.50 (la 0.51.0 movió las tablas de media a las migraciones de sistema y
> colisionó; la protección de la 0.51.1 exigía que una migración fuera
> *puramente* `CreateTable`, cosa que ninguna migración generada es).
> **Actualiza directamente a la 0.51.2**, que corrige ambos.

Para un despliegue existente, la actualización es un despliegue corriente — sin
reaprovisionamiento, sin DDL manual:

```bash
cargo run -- migrate
```

El fake-initial protegido gestiona las tablas preexistentes del framework: el
primer `migrate` registra en el registro la migración de sistema cuyas tablas ya
existen sin tocarlas, crea únicamente lo que realmente falta, y deja tus datos en
paz. Si una base de datos está en un estado parcial verdaderamente inconsistente,
el runner se detiene y te dice lo que encontró; resuélvelo con `migrate --fake`
en lugar de forzarlo.

---

## Véase también

- [Guía de `manage`](manage.md#migrations) — cada verbo de la CLI de migración,
  con ejemplos.
- [Scaffolding](scaffolding.md) — de dónde vienen `migrations/` y
  `system/migrations/`.
- [Modelos](models.md) — el derive a partir del cual se generan las migraciones.
