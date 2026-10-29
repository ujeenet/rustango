# Bases de datos de tenant en un servidor guardado

Un tenant en modo base de datos puede traer su propia URL de base de datos, o
rustango puede crearle la base de datos. Esta página recorre el segundo camino
de principio a fin: guardar un servidor, probarlo, crear tenants en él, rotar
sus contraseñas y purgarlos.

rustango crea para cada tenant una base de datos con el nombre del slug y un
login propio con contraseña aleatoria, con permisos solo sobre esa base de
datos. Registra ambos en `rustango_tenant_databases`. rustango solo elimina,
o cambia la contraseña de, una base de datos registrada.

## Tabla de contenidos

- [Antes de empezar](#antes-de-empezar)
- [Permisos](#permisos)
- [1. Configurar la clave secreta](#1-configurar-la-clave-secreta)
- [2. Guardar el servidor](#2-guardar-el-servidor)
- [3. Probarlo](#3-probarlo)
- [4. Crear un tenant](#4-crear-un-tenant)
- [5. Rotar la contraseña de un tenant](#5-rotar-la-contraseña-de-un-tenant)
- [6. Purgar un tenant](#6-purgar-un-tenant)
- [La clave secreta: copia de seguridad y rotación](#la-clave-secreta-copia-de-seguridad-y-rotación)
- [Copia de seguridad y restauración](#copia-de-seguridad-y-restauración)
- [Notas sobre PostgreSQL](#notas-sobre-postgresql)
- [Notas sobre MySQL](#notas-sobre-mysql)
- [Desarrollo local con Docker](#desarrollo-local-con-docker)

## Antes de empezar

- El servidor usa el backend del registry. Un registry Postgres crea tenants
  Postgres, y así con los demás.
- **PostgreSQL 15 o posterior.** Antes de la 15, cualquier login puede crear
  tablas en el esquema `public` de cualquier base de datos, incluida la del
  registry. `test-db-server` rechaza servidores más antiguos.
- El login de administración necesita estos permisos:

| Backend | Permisos |
|---|---|
| PostgreSQL | `CREATEDB` y `CREATEROLE`, o superusuario |
| MySQL / MariaDB | `CREATE`, `DROP` y `CREATE USER` globales, y todos los privilegios de base de datos `WITH GRANT OPTION` |
| SQLite | un directorio con escritura; los tenants son `<dir>/<slug>.db` |

Los permisos que vienen de roles de MySQL cuentan cuando el rol está activo
por defecto (`SET DEFAULT ROLE`). Consulta [Permisos](#permisos) para el
panorama completo.

## Permisos

Cada forma de crear un tenant usa un login distinto y necesita permisos
distintos. **Test connection**, `test-db-server` y la propia creación
comprueban la misma lista, nombran lo que falta e imprimen las sentencias que
lo arreglan. Una creación se rechaza antes de crear nada.

| Opción | Dónde viven los datos | Login usado | Login propio del tenant | Permisos necesarios |
|---|---|---|---|---|
| Base de datos nueva en un servidor guardado | una base de datos nueva con el nombre del slug | el login de administración del servidor, una vez | sí | crear bases de datos, crear logins, conceder permisos |
| Base de datos, URL existente | la base de datos de la URL | el login de la URL | el de la URL | ninguno: no se crea nada; el login debe poder crear tablas |
| Base de datos en el servidor del registry | una base de datos existente allí | el del registry (`DATABASE_URL`), compartido | no, sin aislamiento | ninguno: no se crea nada |
| Esquema (solo PostgreSQL) | un esquema en la base de datos del registry | el del registry (`DATABASE_URL`), compartido | no | `CREATE` sobre la base de datos del registry |

Para dar los permisos a un login, ejecuta como administrador. MySQL / MariaDB,
como `root`:

```sql
GRANT CREATE, DROP, CREATE USER ON *.* TO 'rustango'@'%';
GRANT ALL PRIVILEGES ON *.* TO 'rustango'@'%' WITH GRANT OPTION;
```

PostgreSQL, como superusuario:

```sql
ALTER ROLE rustango CREATEDB CREATEROLE;
-- schema mode: the registry's login
GRANT CREATE ON DATABASE app TO rustango;
```

El login MySQL `rustango` del docker-compose generado solo tiene permisos
sobre su propia base de datos: guarda el servidor con `root` como login de
administración. Cuando el servidor no dice qué puede hacer un login, la
comprobación informa "unknown" y deja que la creación lo intente. O sáltate
todo esto: usa la URL de una base de datos existente.

## 1. Configurar la clave secreta

Las contraseñas de los servidores y las URLs de los tenants nuevos se guardan
selladas (`enc:v1:`), con una clave derivada de `RUSTANGO_SECRET_KEY`. Usa 32
o más bytes aleatorios:

```bash
export RUSTANGO_SECRET_KEY="$(openssl rand -base64 48)"
```

Sin clave, o con una en blanco, la contraseña de un servidor se rechaza y la
URL de un tenant se guarda sin sellar con una advertencia (se rechaza con
`RUSTANGO_REQUIRE_SEALED_SECRETS=1`). Las páginas de servidores de bases de
datos de la consola y su página **New tenant** muestran un aviso mientras la
clave falta o es corta.

## 2. Guardar el servidor

Consola: **Organizations → Database servers**. CLI:

```bash
cargo run -- add-db-server main --host db.internal --user admin --tls require
```

La contraseña se pide en un prompt oculto y se guarda sellada. TLS es
`require` por defecto. Consulta [manage.md](manage.md) para todas las opciones.

## 3. Probarlo

```bash
cargo run -- test-db-server main
```

Inicia sesión e informa la versión del servidor y, una línea por cada uno, si
el administrador puede crear bases de datos, crear logins y conceder permisos.
Cuando falta uno, lo nombra e imprime las sentencias que lo arreglan (ver
[Permisos](#permisos)). En la página **New tenant** de la consola, **Test
connection** con un slug también ejecuta todas las comprobaciones que hace una
creación antes de cualquier DDL: slug libre, enrutamiento libre y ninguna base
de datos ni login con ese nombre.

## 4. Crear un tenant

Consola: **Organizations → New tenant**, modo base de datos, **Create a new
database on a saved server**. CLI:

```bash
cargo run -- create-tenant acme --server main
```

La ejecución crea la base de datos, el login y el permiso, inicia sesión con
el login nuevo, registra el tenant, ejecuta sus migraciones y lo activa. Un
fallo después de que exista la base de datos elimina lo que creó la ejecución.
La URL del tenant contiene la contraseña del login nuevo y se guarda sellada.

Para darle al tenant su primer administrador, rellena el email del
administrador en el formulario (el nombre de usuario es opcional; por defecto
es el email), o en la CLI:

```bash
cargo run -- create-tenant acme --server main --admin-email admin@example.com
```

La ejecución crea un superusuario sin contraseña utilizable antes de que el
tenant entre en servicio. El administrador fija su propia contraseña con un
enlace que funciona una sola vez, durante 72 horas y solo en este tenant. Con
`Builder::first_admin_mailer` se le envía por correo; si no, la consola lo
muestra **una vez** tras enviar el formulario, y la CLI lo imprime. Cópialo en
ese momento. Para un enlace perdido o caducado:
`cargo run -- set-password-link acme admin@example.com`. Detalles:
[Primer administrador](operator-console.md#primer-administrador).

## 5. Rotar la contraseña de un tenant

Consola: la página del tenant, **Rotate database password**. CLI:

```bash
cargo run -- rotate-tenant-password acme
```

Ambas se ejecutan como una ejecución registrada con estos pasos:

1. `check`: el tenant tiene registro, su URL sigue nombrando esa base de datos
   y ese login, la URL está guardada literal o sellada (una referencia `env://`
   o de vault se rechaza: cambia la contraseña en ese almacén), y el
   administrador puede modificar el login.
2. `alter_password`: una contraseña aleatoria nueva en el servidor.
3. `store_password`: la URL nueva, sellada, se escribe solo mientras la URL
   guardada siga siendo la que se leyó (compare-and-set). Si falla, `revert`
   restaura la contraseña antigua, pero solo mientras la URL guardada aún la
   contenga; si no, ejecuta `rotate-tenant-password` otra vez.
4. `invalidate_pools`: este proceso descarta sus pools para el tenant.
5. `discard_old_password`: ver la ventana más abajo.

**La ventana.** Los demás pods ven la URL nueva en su siguiente sondeo de la
huella de `rustango_orgs` (`RUSTANGO_HOST_GEN_CHECK_SECS`, 5 s por defecto).
Hasta entonces, sus conexiones *nuevas* usan la contraseña antigua. Las
conexiones abiertas nunca se cortan.

- En MySQL 8.0.14 y posterior, la contraseña antigua sigue siendo válida con
  `RETAIN CURRENT PASSWORD` durante dos intervalos de sondeo, y luego se
  descarta con `DISCARD OLD PASSWORD`. Ninguna conexión falla. El `CREATE
  USER` global del administrador basta para ambas cosas. Si el proceso se
  detiene durante la espera, la contraseña antigua sigue siendo válida hasta
  la siguiente rotación o purga.
- En PostgreSQL y MariaDB no hay contraseñas dobles: la antigua deja de
  funcionar al instante, así que otros pods pueden fallar al abrir conexiones
  nuevas durante hasta un intervalo de sondeo.

Un tenant SQLite no tiene contraseña. Una base de datos que rustango no creó
se rechaza.

## 6. Purgar un tenant

Consola: la página del tenant, **Purge** (escribe el slug, marca **drop the
database**). CLI:

```bash
cargo run -- purge-tenant acme --confirm acme --purge-database
```

Con una consola de aprovisionamiento, la purga es una ejecución registrada que
se sigue en vivo en su página de ejecución; la CLI imprime los mismos pasos.
Todos los rechazos van primero, en `check`, así que una purga rechazada no
cambia nada:

- la URL del tenant ya no nombra la base de datos o el login registrados, por
  ejemplo tras `edit-tenant --database-url` o `migrate-tenant-storage`. No se
  elimina nada: restaura la URL, o elimina esa base de datos a mano y borra su
  fila de `rustango_tenant_databases`;
- la URL de otro tenant nombra la misma base de datos, o no se puede leer para
  descartarlo (en MySQL y SQLite cada una se comprueba contra el servidor y el
  archivo reales, así que un alias cuenta), o es la del registry;
- hay otra purga o cambio de contraseña en curso para el tenant;
- el administrador no es dueño de la base de datos (Postgres: la creó otro
  administrador), o no puede eliminar el login (ver
  [Notas sobre PostgreSQL](#notas-sobre-postgresql)); en MySQL necesita `DROP`
  y `CREATE USER` globales;
- hay abierta en la base de datos una sesión que no abrieron los pools de este
  tenant (un operador, una copia de seguridad).

Después: `deactivate`, `purge_media`, `drop_database` (terminando antes las
sesiones propias de este tenant en cada pod), `drop_login`, `remove_org`,
`delete_record`. Un reintento tras un paso fallido se salta lo que ya no
existe. SQLite borra el archivo del tenant y sus archivos `-wal`, `-shm` y
`-journal`.

Una base de datos que trajo el operador no tiene registro y conserva el
comportamiento anterior: en Postgres se elimina a través de su propia URL; en
los demás backends se deja en su sitio y la purga lo indica.

`migrate-tenant-storage` desde una base registrada borra su registro con el
traslado. La base antigua y su login siguen en el servidor; el comando nombra
ambos, para borrarlos a mano cuando la nueva ubicación funcione.

## La clave secreta: copia de seguridad y rotación

**Haz copia de `RUSTANGO_SECRET_KEY` junto con la del registry.** Sin ella no
se abre ninguna URL de tenant ni contraseña de servidor sellada, y todos los
tenants de un servidor guardado quedan inaccesibles hasta que se vuelva a
introducir cada URL.

Para cambiar la clave:

1. Pon la clave antigua en `RUSTANGO_SECRET_KEY_PREVIOUS` y la nueva en
   `RUSTANGO_SECRET_KEY`, en cada proceso, y reinicia. La clave antigua sigue
   abriendo lo que selló.
2. `cargo run -- reseal-tenant-secrets` lista lo que cambiaría.
3. `cargo run -- reseal-tenant-secrets --apply` vuelve a sellar con la clave
   nueva las URLs de los tenants, las contraseñas de los servidores, los
   secretos TOTP de los operadores, los client secrets de SSO y cada columna
   `Cast<EncryptedString>` de los modelos (registro y tenants).
4. Quita `RUSTANGO_SECRET_KEY_PREVIOUS` y reinicia.

El comando reconoce una columna sellada por el tipo de campo que ve el derive
del modelo: `Cast<EncryptedString>` u `Option<Cast<EncryptedString>>`, escrito
completo. El derive rechaza cualquier otra forma, como un alias de tipo, así que
no se pierde ninguna columna. Cada modelo necesita una clave primaria de una sola
columna; uno sin ella se informa como fallo. Un valor que ninguna clave abre se
informa y se deja como está.

Los valores sellados con una clave en blanco, de antes de que se rechazaran
las claves en blanco, siguen abriéndose cuando `RUSTANGO_SECRET_KEY_PREVIOUS`
está definida y vacía; vuelve a sellarlos del mismo modo.

## Copia de seguridad y restauración

Un volcado de la base de datos de un tenant no incluye su login: los roles de
Postgres y los usuarios de MySQL viven fuera de cualquier base de datos
concreta. Para restaurar un tenant en otro servidor:

- Postgres: restaura primero los roles (`pg_dumpall --roles-only`), o crea el
  login del tenant, restaura la base de datos, concédele permisos y ejecuta
  `rotate-tenant-password` para que la URL guardada y el login coincidan.
- MySQL: crea el usuario y su permiso, restaura la base de datos y luego
  `rotate-tenant-password`.

Restaura el registry junto con sus filas de `rustango_tenant_databases`, y
apunta el servidor guardado al host nuevo con `edit-db-server`.

## Notas sobre PostgreSQL

- **Las bases de datos nuevas copian `template1`.** Su codificación, su locale
  y cualquier objeto o extensión que contenga llegan a todos los tenants.
  Mantén `template1` en `UTF8`.
- **Extensiones.** El login del tenant tiene todos los permisos sobre su base
  de datos, pero no es superusuario. Puede hacer `CREATE EXTENSION` de
  extensiones *trusted* (`pgcrypto`, `citext`, `hstore`, `pg_trgm`,
  `uuid-ossp`, `unaccent`, `ltree` y otras marcadas como trusted). Una no
  trusted, como PostGIS, la debe instalar un superusuario: en `template1`
  antes de crear el tenant, o en la base de datos del tenant después.
- **Cambiar el login de administración (PostgreSQL 16+).** Un login
  `CREATEROLE` solo puede modificar o eliminar roles sobre los que tiene
  `ADMIN OPTION`, y solo el administrador que creó el login de un tenant la
  tiene. Tras cambiar un servidor a otro administrador, la rotación y la purga
  de los tenants anteriores se rechazan con el `GRANT … WITH ADMIN OPTION` que
  hay que ejecutar. Una purga también necesita que el administrador sea dueño
  de la base de datos (o superusuario).
- **Terminar sesiones.** Los pools de tenant se conectan con
  `application_name=rustango-tenant:<id>`. Una purga termina esas sesiones, lo
  que requiere un superusuario, pertenencia al rol del tenant o
  `pg_signal_backend`; sin ello, la purga se rechaza mientras haya sesiones de
  ese tipo abiertas.

## Notas sobre MySQL

- El login del tenant es `'<login>'@'%'`: puede conectarse desde cualquier host
  que permita la red del servidor. Limítalo con la red o con el `bind-address`
  del servidor; aún no se ofrecen logins por host.
- Las bases de datos de tenant son `utf8mb4` con la collation de la base de
  datos del registry.
- Las contraseñas dobles necesitan MySQL 8.0.14+; MariaDB rota sin ellas.

## Desarrollo local con Docker

La imagen estándar de `postgres` no tiene TLS, así que guárdala con
`--tls disable`. `mysql:8` sirve TLS con un certificado autofirmado:
`--tls require` funciona, `verify-ca` necesita ese certificado. En MySQL usa
`root`: al login `rustango` del docker-compose generado le faltan los permisos
(ver [Permisos](#permisos)).

```bash
docker run -d --name pg -e POSTGRES_PASSWORD=pw -p 5432:5432 postgres:16
cargo run -- add-db-server local --host 127.0.0.1 --port 5432 --user postgres --tls disable

docker run -d --name my -e MYSQL_ROOT_PASSWORD=pw -p 3306:3306 mysql:8
cargo run -- add-db-server local --host 127.0.0.1 --port 3306 --user root --tls require
```

No uses nunca `--tls disable` para un servidor al que se llega por una red que
no controlas: el login de administración puede eliminar todas sus bases de
datos.
