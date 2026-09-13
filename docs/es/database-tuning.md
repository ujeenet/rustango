# Ajuste de la base de datos

Cada conexión a la base de datos que hace tu aplicación sale de un
**pool**: un conjunto de conexiones que se mantienen abiertas y se
reparten entre las peticiones. Los valores por defecto del pool los
elige el driver, no tu carga de trabajo, y en la mayoría de despliegues
en producción son incorrectos en al menos un aspecto.

Esta página explica qué hace cada parámetro, qué falla si lo dejas como
está, y dónde configurarlo.

## Por qué ajustarlo

Cuatro fallos, cada uno causado por un valor por defecto distinto:

**Tu décima petición simultánea espera.** El pool por defecto del driver
mantiene **10** conexiones. La petición once no falla: *hace cola*, y tu
latencia p99 sube mientras la CPU está ociosa. Nada en los logs dice
«pool agotado»; solo ves peticiones lentas.

**Una base de datos inalcanzable tumba el servidor entero.** Cuando no se
puede obtener una conexión, quien la pide espera. El valor por defecto
del driver son 30 segundos, que es una cifra de herramienta por lotes: en
la ruta de una petición significa que un worker queda bloqueado medio
minuto por petición, así que la caída de *una* base de datos satura todos
los workers y se lleva por delante superficies que ni la tocan. Por eso
rustango usa **5 segundos**.

**Las conexiones mueren en silencio y la paga la petición siguiente.** Un
balanceador, un cortafuegos o el propio
`idle_in_transaction_session_timeout` de PostgreSQL cierra una conexión
que lleva rato inactiva. Tu pool no se entera. La siguiente petición que
la toma prestada recibe un *broken pipe*, de forma intermitente y con
poco tráfico, que es el tipo de error más difícil de reproducir.

**Un failover o una rotación de credenciales no surte efecto.** Las
conexiones en pool son de vida larga por diseño. Tras un failover, o tras
rotar una contraseña, un pool puede seguir usando conexiones abiertas
contra el servidor antiguo o con las credenciales antiguas hasta que algo
las cierre.

## Dónde configurarlo

Tres sitios. **Gana el que está más arriba**, de modo que una anulación
de emergencia en el despliegue nunca necesite un *push* de configuración
ni reiniciar tu canalización de configuración.

| | dónde | para qué |
|---|---|---|
| 1 | variables de entorno | anulaciones por despliegue, gestores de secretos, reajuste de emergencia |
| 2 | `[database]` en tu nivel de settings | los valores con los que tu proyecto funciona normalmente, en control de versiones |
| 3 | valores por defecto del framework | lo que obtienes si no configuras nada |

### En un nivel de settings

Los settings viven en `config/*_settings.toml`, un archivo por entorno.
Pon los valores en el nivel al que pertenecen: los de desarrollo en
`dev_settings.toml`, los de producción en `prod_settings.toml`:

```toml
[database]
pool_max_size             = 50
pool_min_size             = 5
pool_acquire_timeout_secs = 5
pool_idle_timeout_secs    = 600
pool_max_lifetime_secs    = 1800
```

### Como variables de entorno

Cada parámetro tiene una anulación por entorno, que tiene prioridad sobre
el TOML:

```bash
RUSTANGO_DB_MAX_CONNECTIONS=50
RUSTANGO_DB_MIN_CONNECTIONS=5
RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS=5
RUSTANGO_DB_IDLE_TIMEOUT_SECS=600
RUSTANGO_DB_MAX_LIFETIME_SECS=1800
```

Un valor que no sea un entero positivo se **ignora con una advertencia**
en lugar de obedecerse: una errata no debe poner un límite a cero en
silencio, ni tampoco abortar el arranque.

### Una regla de orden

Los settings los aplica a los pools `Cli::with_settings(...)`. Llámalo
**antes** de que nada abra una conexión. Un pool construido antes
funciona con los valores del entorno, y el framework registra una
advertencia diciendo a cuántos pools le ha pasado, en vez de dejar que lo
descubras bajo carga.

## Los parámetros

| setting | variable de entorno | por defecto | qué hace |
|---|---|---|---|
| `pool_max_size` | `RUSTANGO_DB_MAX_CONNECTIONS` | 10 (driver) | Máximo de conexiones que abrirá el pool. Las peticiones de más hacen cola. |
| `pool_min_size` | `RUSTANGO_DB_MIN_CONNECTIONS` | 0 (driver) | Conexiones que siguen abiertas aunque estén ociosas. |
| `pool_acquire_timeout_secs` | `RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS` | 5 | Cuánto espera quien pide una conexión antes de fallar. |
| `pool_idle_timeout_secs` | `RUSTANGO_DB_IDLE_TIMEOUT_SECS` | por defecto del driver | Cierra conexiones que llevan este tiempo ociosas. |
| `pool_max_lifetime_secs` | `RUSTANGO_DB_MAX_LIFETIME_SECS` | por defecto del driver | Cierra conexiones con esta antigüedad, se usen o no. |

Dejar un parámetro sin configurar no es lo mismo que ponerlo a cero. Sin
configurar significa *se aplica el valor por defecto del driver*: el
comportamiento que tenías antes de que el parámetro existiera.

### `pool_max_size`

El techo del trabajo simultáneo contra la base de datos. Súbelo cuando
las peticiones hagan cola esperando conexiones; el síntoma es latencia
que crece con el tráfico mientras la base de datos no está ocupada.

**Es un techo, no un objetivo**: el pool abre conexiones según demanda y
solo hasta ese número.

La restricción importante está al *otro* lado: tu servidor de base de
datos tiene su propio límite de conexiones (`max_connections` de
PostgreSQL, típicamente 100). La suma de todos los pools de todas las
réplicas debe quedar por debajo, o se rechazarán conexiones nuevas. Diez
pods con `pool_max_size = 50` son 500 conexiones contra un servidor que
permite 100.

### `pool_min_size`

Conexiones que siguen abiertas aunque nadie las use. El objetivo es la
latencia: con `0`, la primera petición tras un rato de calma paga el
viaje completo de TCP, TLS y autenticación antes de ejecutar consulta
alguna.

Dimensiónalo para tu carga base, no para el pico. Las conexiones abiertas
también cuestan recursos en el servidor.

### `pool_acquire_timeout_secs`

Cuánto espera quien pide una conexión antes de rendirse, cubriendo
**tanto** abrir una nueva **como** hacer cola por una libre.

Este parámetro decide cómo se comporta tu aplicación cuando la base de
datos no responde. Demasiado alto y los workers se bloquean contra una
base de datos que nunca contestará; demasiado bajo y un pico de tráfico
legítimo —donde hacer cola es trabajo real y productivo— se convierte en
errores.

Los 5 segundos por defecto son deliberadamente más estrictos que los 30
del driver. Súbelo si tienes consultas largas y un pool saturado pero
sano; bájalo si prefieres rechazar carga rápido.

### `pool_idle_timeout_secs`

Cierra conexiones que han estado sin usar. Ponlo **por debajo** del
tiempo de inactividad más corto que haya en el camino entre tu aplicación
y la base de datos: un balanceador, un proxy, la tabla de conexiones de
un cortafuegos, o los propios timeouts del servidor. Si algo cierra la
conexión antes, tu pool reparte una conexión muerta.

10 minutos es un punto de partida habitual.

### `pool_max_lifetime_secs`

Cierra conexiones a partir de cierta antigüedad, estén sanas o no. Es el
parámetro que hace que los failover y las rotaciones de credenciales
surtan efecto de verdad: sin él, un pool puede seguir hablando con el
servidor al que se conectó al arrancar.

30 minutos es un punto de partida habitual. Menos si obtienes
credenciales de un gestor de secretos con una vigencia corta.

## Los backends no son iguales

**SQLite no es un servidor**, y dimensionar el pool no significa lo
mismo. SQLite serializa los escritores de forma global —un escritor cada
vez, sea cual sea el tamaño del pool—, así que subir `pool_max_size` añade
concurrencia de lectura pero nunca de escritura. Con una base de datos en
memoria, las conexiones adicionales son peor que inútiles: cada una es
una *base de datos vacía distinta* salvo que la URL indique
`cache=shared`.

**PostgreSQL y MySQL** imponen un límite de conexiones del lado del
servidor. Dimensiona tus pools contra ese presupuesto sumando todas las
réplicas, y recuerda que los *poolers* como PgBouncer cambian las
cuentas.

## Las opciones de conexión son otra cosa

Los parámetros anteriores son del *pool*. Las opciones de una *conexión*
concreta —TLS, timeouts de conexión, el nombre de aplicación que ve el
servidor— viajan en la URL de conexión:

```
postgres://user:pw@host:5432/db?sslmode=require&connect_timeout=10&application_name=myapp
mysql://user:pw@host:3306/db?ssl-mode=REQUIRED
sqlite://./dev.db?mode=rwc
```

Se pasan tal cual al driver, así que funciona todo lo que acepte su
analizador de URL.

## Pools de tenant

En una aplicación multi-tenant, cada tenant en modo base de datos tiene
su propio pool, y esos se configuran aparte mediante `TenantPoolsConfig`
—consulta [Ajuste de pools de tenant](manage.md) en la guía de `manage`—.
Sus valores por defecto difieren de los del pool principal; en concreto,
el timeout de adquisición de los tenants es más generoso, algo que
conviene revisar si ejecutas muchos tenants contra bases de datos que
pueden caerse de forma independiente.
