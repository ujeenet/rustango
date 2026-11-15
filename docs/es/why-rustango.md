# Por qué Rustango: una comparación honesta

Rustango es un framework web para Rust que trae todo lo necesario: declaras un modelo una vez y obtienes un ORM, migraciones, un admin, una API REST y multi-tenancy. Esta página explica en qué es bueno, cómo se compara con los frameworks que se suelen elegir, los números de los benchmarks y cuándo te conviene elegir otra cosa.

## En resumen

- **Elige Rustango** si quieres la productividad de un framework completo con la velocidad, el bajo consumo de memoria y las comprobaciones en compilación de Rust, y un solo binario que desplegar.
- **Elige otra cosa** si tu equipo no escribe Rust, si hoy necesitas un ecosistema enorme de paquetes o si quieres un framework con diez años de uso en producción.

## Qué incluye

| Área | Rustango |
|---|---|
| ORM | Un modelo, SQL correcto para PostgreSQL, MySQL 8+ y SQLite; relaciones, agregados, borrado lógico |
| Migraciones | Generadas comparando tus modelos con la última instantánea, y luego aplicadas |
| Admin | Un admin generado automáticamente, con búsqueda, filtros, registro de auditoría y permisos |
| APIs | ViewSets y serializers para REST, con OpenAPI generado a partir de ellos |
| Autenticación | Sesiones, JWT, OAuth2 / OIDC, inicio de sesión único, claves de API, HMAC, TOTP, passkeys |
| Multi-tenancy | Un esquema por tenant (PostgreSQL) o una base de datos por tenant (las tres bases de datos) |
| Trabajo en segundo plano | Jobs, tareas programadas, señales, correo, almacenamiento de archivos |
| Seguridad | CSRF, límites de peticiones, freno a los intentos de inicio de sesión, cabeceras de seguridad, CORS, hosts permitidos |
| Agentes de IA | Un servidor MCP integrado |
| Herramientas | Un generador de proyectos, generadores de código y una sola línea de comandos `manage` |

Todo lo que va más allá del ORM es una feature de cargo, así que una API solo JSON compila sin el admin ni las plantillas.

## Comparación

| | Rustango | Django | Rails | Laravel | Loco | axum + sqlx |
|---|---|---|---|---|---|---|
| Lenguaje | Rust | Python | Ruby | PHP | Rust | Rust |
| ORM | Integrado | Integrado | Integrado | Integrado | SeaORM | Consultas a mano |
| Migraciones desde los modelos | Sí, por diferencia | Sí, por diferencia | Escritas con generadores | Escritas a mano | Escritas con generadores | A mano |
| Admin | Integrado | Integrado | Gems | Paquetes (Nova, Filament) | No | No |
| Capa de API REST | Integrada, con OpenAPI | Paquete (DRF) | Integrada (modo API) | Integrada | Integrada | A mano |
| Multi-tenancy | Integrado | Paquetes | Gems | Paquetes | No | A mano |
| Jobs en segundo plano | Integrados | API de tareas; worker con un paquete | Integrados | Integrados | Integrados | A mano |
| Errores en modelos y consultas | Detectados al compilar | En ejecución | En ejecución | En ejecución | Detectados al compilar | Detectados al compilar |
| Artefacto de despliegue | Un binario | App + entorno de Python | App + entorno de Ruby | App + entorno de PHP | Un binario | Un binario |
| Ecosistema | Pequeño y nuevo | Enorme | Enorme | Enorme | En crecimiento | Grande (crates de Rust) |

La lectura honesta: en funciones, Rustango juega en la misma liga que Django y Rails, no por encima. Gana en velocidad, memoria, seguridad en compilación y despliegue, y frente a las demás opciones en Rust gana en todo lo que trae de serie.

## Benchmarks

Cada app servía el mismo blog: 1 000 entradas, 10 000 comentarios, el mismo HTML comprobado byte a byte, 4 CPU, 16 conexiones a la base de datos y 50 conexiones simultáneas. El método completo, los límites y los perfiles están en el [informe de benchmarks](benchmarks.md). Las rondas se midieron por separado, así que cada tabla se queda dentro de una sola ronda.

**Ronda 2: apps compiladas** (peticiones por segundo, más es mejor):

| App | Índice | Detalle | Etiqueta | Índice, en caché | Memoria bajo carga |
|---|--:|--:|--:|--:|--:|
| Rustango · actix | **17 401** | **27 292** | 7 056 | **231 975** | **6.3 MiB** |
| Rustango · axum | 14 686 | 25 788 | **7 090** | 174 501 | 6.9 MiB |
| axum + sqlx | 14 140 | 22 842 | 6 467 | 198 669 | 7.6 MiB |
| actix + sqlx | 14 408 | 21 630 | 6 628 | 226 887 | 7.2 MiB |
| Rocket | 13 316 | 24 561 | 6 836 | 138 224 | 8.7 MiB |
| Loco | 10 290 | 15 635 | 5 604 | 153 243 | 12.0 MiB |
| Go (net/http + pgx) | 8 741 | 22 607 | 6 386 | 91 839 | 22.8 MiB |

Rustango, con su ORM, es el más rápido de los ocho en páginas sin caché, por delante de las consultas escritas a mano sobre sqlx.

**Ronda 1: entornos interpretados**, con Rustango de la misma medición (la 0.60.6, antes de las correcciones de la ronda 2):

| App | Índice sin caché, req/s | Índice en caché, req/s | Memoria bajo carga |
|---|--:|--:|--:|
| Rustango · actix | 6 341 | 200 167 | ~7 MiB |
| Laravel · Octane | 1 098 | 5 884 | 241 MiB |
| Python · gunicorn | 697 | 11 012 | 224 MiB |
| WordPress | 599 | 21 285 | 142 MiB |
| Laravel · php-fpm | 518 | 3 326 | 89 MiB |

## Las limitaciones, con honestidad

- **Go es más rápido en cálculo puro**, entre un 8 y un 17 % en la prueba de cálculo, y axum con sqlx a secas sirve el índice en caché más rápido que Rustango sobre axum (198 669 frente a 174 501 req/s).
- **Es joven.** Versión 0.x, con una superficie grande que todavía se está asentando. Las últimas versiones han añadido más que quitado, pero lee el changelog al actualizar.
- **El ecosistema es pequeño.** Django, Rails y Laravel tienen un paquete para casi todo; con Rustango lo escribirás tú más a menudo o recurrirás a un crate de Rust genérico.
- **Rust tiene curva de aprendizaje**, y los tiempos de compilación son reales: minutos para la primera compilación, segundos para un cambio.
- **Un esquema por tenant requiere PostgreSQL.** En MySQL y SQLite, cada tenant tiene su propia base de datos.
- **Los benchmarks tienen límites.** Un portátil, una VM de Docker, una carga de lectura renderizada en el servidor. Sin escrituras, sin flujos de inicio de sesión, sin lógica de negocio pesada. Tus números serán distintos.

## Para quién es

- Equipos a los que les gusta la forma de trabajar de Django o Rails, pero quieren la velocidad, la seguridad y la ligereza de Rust.
- Equipos de Rust cansados de montar la autenticación, el admin, las migraciones y la API en cada proyecto.
- Productos SaaS multi-tenant que quieren el multi-tenancy dentro del framework y no añadido después.

## Pruébalo

```bash
cargo install cargo-rustango
cargo rustango new myapp
cd myapp && cargo run
```

La [guía de inicio](getting-started.md) construye paso a paso un blog con admin, API JSON e inicio de sesión. Para sitios de contenido, [Rustango-CMS](https://cms.rustango.com) está construido encima.
