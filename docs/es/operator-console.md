# La consola de operador

Un proyecto multi-tenant tiene dos clases de administrador, y nunca se mezclan:

- Los **operadores** llevan el *despliegue*. Viven en el registry, entran por el dominio apex y alcanzan a cada tenant.
- Los **usuarios de tenant** llevan *un* tenant. Viven en la base de datos de ese tenant y nunca ven la consola.

La consola de operador es la interfaz web de la primera clase — aprovisionar tenants, vincular nombres de host, gestionar operadores, leer el rastro de auditoría y sacar un tenant de servicio. Casi todo eso es también un [verbo de `manage`](manage.md#comandos-de-tenancy), porque una acción que solo existe en una superficie no se puede automatizar, y una que solo existe en una shell no se puede delegar.

La diferencia que conviene conocer: la consola puede migrar **un** tenant
(`POST /orgs/{slug}/migrate`), mientras que `migrate-tenants` de la CLI no
recibe slug y recorre todo el registry. En la línea de comandos no hay migración
por tenant.

[![La lista de tenants de la consola de operador — cada tenant del registry con su modo de almacenamiento, patrón de host y estado activo, más las acciones de aprovisionar, migrar y precalentar](../img/operator-console.png)](../img/operator-console.png)

## Tabla de contenidos

- [Montarla](#montarla)
- [Qué hace cada página](#qué-hace-cada-página)
- [Nombres de host](#nombres-de-host)
- [Hosts desconocidos](#hosts-desconocidos)
- [Operadores](#operadores)
- [Inicio de sesión en dos pasos](#inicio-de-sesión-en-dos-pasos)
- [El registro de auditoría](#el-registro-de-auditoría)
- [Ejecuciones de aprovisionamiento](#ejecuciones-de-aprovisionamiento)
- [Servidores de bases de datos](#servidores-de-bases-de-datos)
- [Marca](#marca)
- [Los tres niveles de capacidad](#los-tres-niveles-de-capacidad)

---

## Montarla

La consola es un router que montas; cuánto puede hacer depende de lo que le entregues.

```rust
use rustango::tenancy::operator_console::{router, router_with_pools, router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;

// Aquí `pools` es un `Arc<TenantPools<_>>`.

// Solo lectura: recorrer tenants, operadores y el registro de auditoría.
let app = router(registry.clone(), SessionSecret::from_env_or_random());

// …más editar tenants, gestionar operadores, vincular hosts, precalentar pools.
let app = router_with_pools(registry.clone(), pools.clone().into_invalidator(), secret);

// …más aprovisionar tenants nuevos y ejecutar migraciones.
let provisioner = Provisioner::new(pools.clone(), registry_url.clone(), "migrations").erased();
let app = router_with_provisioning(
    registry.clone(),
    pools.clone().into_invalidator(),
    provisioner,
    secret,
);
```

Ahí hay dos conversiones haciendo trabajo y ninguna es opcional.
`pools` entra como `Arc<dyn TenantPoolInvalidator>`, así que necesita
`.into_invalidator()` — la consola solo invalida pools, y tomar el trait
estrecho es lo que le impide alcanzar el resto. `Provisioner::new` cierra
sobre los pools, la URL del registro y el directorio de migraciones, y
`.erased()` lo deja tras `Arc<dyn TenantProvisioner>`.

En un proyecto `tenant` generado ya viene cableado — `Cli::new().tenancy().with_tenant_provisioning("migrations")` monta la versión completa. Véase [Andamiaje](scaffolding.md).

La consola responde en el dominio **apex** (`RUSTANGO_APEX_DOMAIN`), no en un subdominio de tenant: `http://localhost:8080/login` con apex `localhost`. Una petición a `127.0.0.1` no es el apex y no encajará.

---

## Qué hace cada página

| Página | Para qué sirve |
|---|---|
| **Organizations** | Cada tenant, con modo de almacenamiento, patrón de host y estado activo |
| **Organizations → Edit** | Nombre visible, patrón de host, prefijo de ruta, puerto, URL de base, marca |
| **Hostnames** | Los dominios adicionales a los que responde un tenant |
| **Operators** | Quién puede entrar en esta consola |
| **Audit log** | Cada cambio hecho a través de la consola |
| **Runs** | Ejecuciones de aprovisionamiento y migración, en directo |
| **Database servers** | Servidores donde se crean las bases de los tenants, con su login de administración |
| **Change password** | Su propia contraseña; sus otras sesiones terminan, esta sigue abierta |
| **Two-factor sign-in** | Su propia app de autenticación (feature `totp`) |
| **Shared SSO** | Proveedores de inicio de sesión ofrecidos a todos los tenants, con la URL de callback que hay que registrar (feature `admin-sso`; ver [SSO](sso.md)) |

---

## Nombres de host

Un tenant es alcanzable en su subdominio y, opcionalmente, en los nombres de host adicionales que le vincules. Un nombre de host lleva a exactamente un tenant.

[![La página de nombres de host de un tenant — el host base marcado como tal y no borrable, hosts adicionales con acciones Aparcar y Quitar, y un formulario para añadir uno](../img/operator-console-hosts.png)](../img/operator-console-hosts.png)

Dos cosas que conviene saber:

**El host base no tiene botón de borrado.** Viene de la columna `host_pattern` del tenant, no de la tabla de hosts, así que no hay fila que quitar — cámbialo en la página de edición del tenant. Eso es estructural, no una regla de interfaz: el motor también lo rechaza, de modo que quien adivine el `POST` obtiene la misma respuesta que quien lee la página.

**Aparcar conserva la fila.** *Park* saca un host de servicio sin perder el registro — útil mientras el DNS propaga, o al retirar un dominio que quizá quieras recuperar. *Serve* lo devuelve al servicio.

Los nombres de host se normalizan a la entrada: minúsculas, sin esquema, sin puerto, sin ruta. El valor guardado se compara byte a byte con la cabecera `Host`, así que un valor que nunca podría encajar se rechaza en vez de guardarse.

Desde la CLI: [`list-hosts` / `add-host` / `remove-host` / `set-host-enabled`](manage.md#nombres-de-host).

---

## Hosts desconocidos

Por defecto, una petición cuyo host no resuelve ningún tenant va a las rutas
de tu aplicación, donde el extractor `Tenant` y el admin del tenant responden
404. Para servir otra cosa en su lugar — una página de configuración inicial, una
página «este sitio aún no está configurado» — pasa un router a
`unresolved_tenant`, por ejemplo una ruta `/` que responde «Crea tu primer sitio» mientras `TenantRegistry::has_tenants()` sea false. `Builder::unresolved_tenant` tiene un ejemplo compilado.

Se ejecuta para un host desconocido y para una ruta del apex que la consola no
sirve. Nunca se ejecuta para un host que resuelve a un tenant, y la consola
conserva sus propias rutas. Un tenant inactivo no resuelve, así que su host
también llega a este router. El router recibe el registro como
`Extension<TenantRegistry>`, nunca un pool de tenant; `has_tenants()` cuenta
también los tenants inactivos. `/health` y `/ready` siguen servidos a su lado.

**Seguridad:** cualquiera que llegue al servidor llega a este router. Comprueba
el estado tú mismo (por ejemplo, ofrece la configuración solo mientras
`has_tenants()` sea false) y protege todo lo que escriba.

El mismo setter existe en `rustango::server::Builder`.

---

## Operadores

[![La página de operadores — el operador conectado marcado como «eres tú», con un formulario para añadir otro y la opción de generar una contraseña](../img/operator-console-operators.png)](../img/operator-console-operators.png)

Los operadores se desactivan, nunca se borran: la fila permanece, así que un «¿quién hizo esto?» posterior sigue resolviéndose. La consola la relee en **cada** petición, de modo que una desactivación surte efecto en el siguiente clic y no cuando caduque la cookie.

Dos cosas que la página no permite, por la misma razón:

- **Desactivarte a ti mismo.** Tu siguiente petición sería rechazada.
- **Desactivar al último operador activo.** Eso deja a todo el mundo fuera, y solo una shell sobre el registry podría deshacerlo.

Una contraseña generada se muestra **una vez**, en el cuerpo de la respuesta — nunca mediante una redirección, que la pondría en la barra de direcciones, el historial, el referente y cada log de acceso por el camino.

Desde la CLI: [`list-operators` / `set-operator-active`](manage.md#list-operators).

---

## Inicio de sesión en dos pasos

Con la feature `totp`, un operador puede añadir una app de autenticación en **Two-factor sign-in** (`/account/totp`). La página muestra una clave de configuración y un enlace `otpauth://` hasta que un código la confirma. Desactivarla pide un código actual.

Después, la contraseña sola solo abre el paso del código (`/login/totp`); ninguna página de la consola responde hasta que el código es correcto. Cada código sirve una vez, y los códigos erróneos cuentan contra los mismos límites y el mismo bloqueo que las contraseñas erróneas. La activación, la desactivación y cada código erróneo se escriben en el [registro de auditoría](#el-registro-de-auditoría) como `totp_enroll`, `totp_disable` y `totp_failed`, nunca con el código.

Es opcional por operador. `RUSTANGO_OPERATOR_REQUIRE_TOTP=1` lo hace obligatorio: un operador sin dispositivo configura uno en el paso del código antes de su primera sesión. Se lee al arrancar.

Los secretos están en `rustango_operator_totp` en el registro, sellados con `RUSTANGO_SECRET_KEY`, que debe estar definido para activarlo. `migrate` crea la tabla, o la consola en el primer inicio de sesión. Si la tabla no existe (un rol del registro sin CREATE, antes de `migrate`), basta la contraseña salvo que TOTP sea obligatorio; si existe pero no se puede leer, el inicio de sesión se rechaza. Aún no hay códigos de recuperación: si un operador pierde el dispositivo, ejecuta `cargo run -- reset-operator-totp <username>`.

---

## El registro de auditoría

[![El registro de auditoría de la consola — quién cambió qué, en qué registro y cuándo, filtrable por entidad, id y operación](../img/operator-console-audit.png)](../img/operator-console-audit.png)

Cada mutación hecha por la consola queda anotada: ediciones de tenant, cambios de hosts, gestión de operadores, suplantación, purgas, precalentados. La columna `source` lleva `operator:<id>:<verbo>`, de forma que la actividad de los operadores se puede separar después de la de los usuarios de tenant.

Este es el registro del **registry**. El historial propio de un tenant vive en el admin de ese tenant — mezclarlos obligaría a abrir en abanico una consulta sobre cada pool de tenant para pintar una sola página.

Crece con el uso de la consola, así que recórtalo periódicamente con [`audit-cleanup`](manage.md#audit-cleanup), que barre el registro del registry y el de cada tenant activo.

Desde la CLI: [`audit-log`](manage.md#ver-qué-ha-pasado).

---

## Ejecuciones de aprovisionamiento

[![La página de ejecuciones de aprovisionamiento — cada ejecución con su tipo, tenant, estado y tiempos, enlazando a los pasos registrados](../img/operator-console-runs.png)](../img/operator-console-runs.png)

Aprovisionar un tenant son varios pasos contra una base de datos que puede ser lenta o inalcanzable, así que se registra como una **ejecución** y no como una petición que vuelve o no vuelve. Cada ejecución emite sus pasos en directo y sobrevive a una recarga, a una reconexión o a que la mire un segundo pod.

Los tenants creados desde la CLI también quedan registrados, marcados con `requested_by = cli`, así que el historial cubre ambas superficies.

### Crear un tenant

**Organizations → New tenant**. Cada opción indica en el formulario dónde
viven los datos, qué login se usa y qué permisos necesita:

- **Schema** (solo con registry PostgreSQL): un esquema en la base de datos
  del registry. Usa el login del registry (`DATABASE_URL`); los tenants lo
  comparten y no reciben login propio. Necesita `CREATE` sobre la base de
  datos del registry.

En modo base de datos, elige una:

- **Create a new database on a saved server**: elige el servidor. Usa una vez
  el login de administración del servidor; el tenant recibe su propio login.
  La base de datos se llama exactamente como el slug y recibe un login propio
  con contraseña aleatoria, con permisos solo sobre esa base de datos. La
  ejecución muestra `create_database`, `create_login`, `grant` y `check_login`
  (el login nuevo inicia sesión). Una base de datos existente con ese nombre
  se rechaza. Si falla un paso posterior, el paso `rollback` de la ejecución
  elimina la base de datos, el login y la fila del tenant nuevos, y el
  formulario vuelve con el motivo. Es la opción por defecto en cuanto hay un
  servidor guardado.
- **Use an existing database URL**: no se crea nada; la base de datos debe
  existir. Se enmascara y nunca se devuelve a la página. No hacen falta
  permisos de administración.
- **A database on the registry's server**: un nombre de base de datos; usa el
  login del registry (`DATABASE_URL`), así que los tenants no quedan aislados
  entre sí. No se crea nada.

**Test connection** comprueba lo que necesita la opción elegida y no crea
nada. En un servidor guardado ejecuta todas las comprobaciones que hace un
envío antes de crear algo (slug libre, enrutamiento, permisos de
administración, ninguna base de datos ni login con ese nombre) y lista cada
permiso como sí, no o desconocido. En modo esquema comprueba el `CREATE` del
login del registry sobre la base de datos del registry. Un permiso que falta
viene con las sentencias que lo arreglan, y la creación se rechaza con el
mismo texto antes de crear nada. Ver
[Permisos](tenant-databases.md#permisos). En los demás casos, conecta con la
base de datos. El backend es siempre el del registry.

### Primer administrador

Indica un email de administrador (y opcionalmente un nombre de usuario; por
defecto es el email) y la ejecución añade un paso `create_admin` tras las
migraciones, antes de que el tenant entre en servicio. Crea un superusuario
sin contraseña utilizable. Si el paso falla, la ejecución se revierte como con
cualquier otro paso.

El administrador fija su propia contraseña con un enlace de establecimiento de
contraseña. El enlace funciona una vez, caduca en 72 horas y solo abre la
página de cambio de contraseña de este tenant. Con
`Builder::first_admin_mailer(mailer, from)` (feature `email`) se envía por correo al
administrador. Sin mailer, la página de la ejecución te lo muestra **una
vez**, justo después de enviar; cópialo y dáselo al administrador. Nunca se
guarda, audita ni registra en logs; el registro de auditoría solo anota la
creación del usuario.

Los campos aparecen cuando el provisioner tiene un traspaso
(`Provisioner::with_first_admin_handover`; el `Builder` estándar lo
configura). Para un enlace perdido o caducado: ejecuta
`manage set-password-link <slug> <username>`.

Desde la CLI: [`list-runs` / `show-run`](manage.md#ver-qué-ha-pasado).

---

## Servidores de bases de datos

**Organizations → Database servers** guarda un servidor una sola vez: host,
puerto, usuario y contraseña de administración, modo TLS y, en Postgres, una
base de mantenimiento que usar en lugar de `postgres`. Con un registry SQLite,
un «servidor» es un directorio para los archivos de los tenants. El backend es
siempre el del registry.

La contraseña se guarda sellada (`enc:v1:`), así que `RUSTANGO_SECRET_KEY` debe
estar definida; si no, el guardado se rechaza y la página muestra un aviso.
Nunca se vuelve a mostrar: el formulario de edición deja el campo vacío y un
campo vacío conserva la guardada. Las filas de auditoría nombran los campos
cambiados, nunca sus valores.

**Test** inicia sesión y muestra la versión del servidor y, una línea por cada
uno, si el login de administración puede crear bases de datos, crear logins y
conceder permisos. Cuando no puede aprovisionar tenants dice por qué (un
permiso que falta, o PostgreSQL anterior a 15) y muestra las sentencias
`GRANT` / `ALTER ROLE` que lo arreglan. Cuentan los permisos de roles MySQL
activos.

TLS es `require` por defecto para un servidor nuevo. Una edición hecha desde
una página que otra persona cambió entretanto se rechaza; recarga y vuelve a
editar.

En el stack de desarrollo `docker compose` generado, Postgres no tiene TLS, así
que guárdalo con TLS `disable`; MySQL 8 sirve TLS, así que `require` funciona.
El usuario MySQL `rustango` del compose no puede crear usuarios ni conceder
permisos; guarda ese servidor como `root`.

Borrar se rechaza mientras la URL de un tenant en modo base de datos apunte al
host y puerto del servidor (SQLite: un archivo en su directorio) o no se pueda
resolver, mientras siga registrada una base de datos que rustango creó allí, y
en una consola construida sin pools de tenant.

Un tenant cuya base de datos se creó en un servidor guardado tiene **Rotate
database password** en su página. Eso y **Purge** se ejecutan como ejecuciones
registradas, que se siguen en vivo igual que una creación. El flujo completo
está en [Bases de datos de tenant en un servidor guardado](tenant-databases.md).

Desde la CLI: [`add-db-server` y compañía](manage.md#servidores-de-bases-de-datos).

---

## Marca

El nombre, el lema, el logo, el color de acento y el tema de la consola salen
de `[brand]` en `config/default.toml` o `config/<env>_settings.toml`:

```toml
[brand]
name          = "Acme Operator"
tagline       = "Production"
logo_url      = "https://cdn.example.com/acme-logo.png"
primary_color = "#2c6fb0"
theme_mode    = "auto"
```

| Clave | Por defecto | Qué hace |
|---|---|---|
| `name` | `Rustango` | Nombre en la cabecera y en la página de inicio de sesión |
| `tagline` | ninguno; la página de inicio de sesión muestra "Operator console" | Línea bajo el nombre |
| `logo_url` | `/__static__/rustango.png` | Imagen del logo; por defecto, el logo incluido, servido por la consola |
| `logo_path` | ninguno | Archivo de logo local que sirve la propia consola; ver [Tu propio logo](#tu-propio-logo) |
| `primary_color` | el color del tema | Tiñe los botones principales y los enlaces. Hex: `#rrggbb` o `#rgb` |
| `theme_mode` | `auto` | Tema inicial: `auto`, `light` o `dark` |

Un valor vacío cuenta como no definido. Un `primary_color` o `theme_mode` que
no se puede interpretar se ignora, y se queda el valor por defecto.

Las variables de entorno tienen prioridad sobre el archivo, así que puedes
cambiar la marca de un despliegue sin tocar la configuración:

| Variable | Sustituye a |
|---|---|
| `RUSTANGO_OPERATOR_BRAND_NAME` | `name` |
| `RUSTANGO_OPERATOR_TAGLINE` | `tagline` |
| `RUSTANGO_OPERATOR_LOGO_URL` | `logo_url` |
| `RUSTANGO_OPERATOR_LOGO_PATH` | `logo_path` |
| `RUSTANGO_OPERATOR_PRIMARY_COLOR` | `primary_color` |
| `RUSTANGO_OPERATOR_THEME_MODE` | `theme_mode` |

El orden es: variable de entorno, luego `[brand]` (el archivo del nivel por
encima de `default.toml`), luego el valor por defecto. Una variable vacía se
ignora. La consola lee todo esto una vez al arrancar, así que reiníciala tras
un cambio.

### Tu propio logo

El dominio apex solo sirve la consola: un montaje `Cli::with_static` se sirve
en los hosts de tenant, no en el apex. Así que un logo es o bien un archivo
que sirve la consola, o bien una URL completa.

`logo_path = "assets/acme-logo.png"` nombra un archivo local, relativo al
directorio de trabajo. La consola lo lee una vez al arrancar, con las mismas
comprobaciones que la subida de logo de un tenant: PNG, JPEG, WebP o ICO (sin
SVG), como mucho `RUSTANGO_BRAND_MAX_BYTES` (1 MiB). Sirve el archivo en
`/__static__/brand-logo.<ext>?v=<hash>` con una caché de un año; el hash
cambia con el archivo. Un archivo que falta o se rechaza registra una
advertencia y se mantiene el logo incluido.

`logo_url` va directamente a `<img src>`, así que el navegador debe poder
cargarlo: un CDN, un bucket de almacenamiento de objetos o tu sitio web. Si
defines `[security] csp`, permite ese host en `img-src`.

Si se definen ambos, las variables de entorno ganan al archivo y, dentro de
cada uno, `logo_url` gana a `logo_path`.

En un proyecto sin tenancy, el admin comparte host con tus rutas, así que ahí
sí funciona un montaje estático; ver [la sección de marca del admin](admin.md#configuración-admin-y-brand).

La consola no define favicon, y no hay clave para uno.

### Los lemas `dev` y `staging`

`cargo rustango new` escribe `tagline = "dev"` bajo `[brand]` en
`config/dev_settings.toml` y `tagline = "staging"` en
`staging_settings.toml`, para distinguir los niveles de un vistazo; el lema se
muestra como una insignia. `prod_settings.toml` no define ninguno. El archivo
del nivel gana a `default.toml`, así que esto sustituye un lema que hayas
definido allí.

Para cambiarlo, edita la línea, por ejemplo `tagline = "Acme dev"`. Para
quitarlo, borra la línea o pon `tagline = ""`. En un proyecto con admin, el
mismo lema es el subtítulo del admin.

### Marca por tenant

Cada tenant tiene su propia marca en su fila `Org`, que se define en
**Organizations → Edit**: `brand_name`, `brand_tagline`, `primary_color` y
`theme_mode`. El formulario **Branding assets** de la misma página sube un
logo (PNG, JPEG o WebP) y un favicon (PNG, JPEG, WebP o ICO), de hasta 1 MB cada uno
(`RUSTANGO_BRAND_MAX_BYTES`). Los archivos se guardan en
`RUSTANGO_BRAND_STORAGE_DIR`, por defecto `./var/brand`. Ese directorio es
local a cada pod y se pierde al redesplegar; con varias réplicas pase un disco
compartido con `Cli::brand_storage` o `server::Builder::brand_storage` (por
ejemplo un disco de `StorageRegistry`).

El admin del tenant y su página de inicio de sesión usan estos valores;
`[brand]` no les llega. Sin `brand_name` se muestra el nombre visible del
tenant, y sin logo subido, el logo del framework. El favicon aparece en las
páginas de inicio de sesión y de cambio de contraseña del tenant. Ver
[la sección de marca del admin](admin.md#el-admin-de-inquilino).

---

## Los tres niveles de capacidad

Qué rutas existen depende del constructor que hayas montado:

| | `router` | `router_with_pools` | `router_with_provisioning` |
|---|---|---|---|
| Recorrer tenants, operadores, auditoría | ✓ | ✓ | ✓ |
| Editar un tenant, gestionar hosts | | ✓ | ✓ |
| Añadir / desactivar operadores | | ✓ | ✓ |
| Precalentar pools | | ✓ | ✓ |
| Sacar un tenant de servicio | | ✓ | ✓ |
| Aprovisionar un tenant, ejecutar migraciones | | | ✓ |
| Ver las ejecuciones de aprovisionamiento | | | ✓ |
| Guardar y probar servidores de bases de datos | | | ✓ |

Las rutas no montadas devuelven 404 en lugar de 403 — una consola de solo lectura no anuncia lo que no puede hacer.

### Tres constructores más, de cableado y no de capacidad

Los tres de arriba son los niveles de capacidad. Los demás toman esos
mismos niveles y añaden cableado, así que no son un cuarto y un quinto
peldaño:

| Constructor | Nivel | Qué más recibe |
|---|---|---|
| `router_with_brand_storage` | solo lectura, o edición si pasas `Some(pools)` | un `BoxedStorage` para las subidas de marca, cuando el `LocalStorage` por defecto no es donde las quieres |
| `router_with_impersonation` | edición | almacenamiento de marca, el secreto de sesión del tenant y una URL de traspaso, para que un operador pueda entrar en un tenant |
| `router_full` | el que le pases | todo lo anterior como `Option` — `None` simplemente no monta esas rutas |

`rustango::server::Builder::serve` monta la suplantación por ti; un punto de
montaje propio se acoge a ella usando `router_with_impersonation` en
lugar de `router_with_pools`. Recurre a `router_full` cuando necesites
una combinación que los atajos no nombran — existe para que un
emparejamiento nuevo no requiera un constructor posicional nuevo.

**Cada operador lo puede todo.** No hay controles de permisos por operador: un operador alcanza cada tenant y cada acción que la consola ofrece. La frontera de control de acceso es la propia lista de operadores — justo por eso una desactivación surte efecto en la siguiente petición.
