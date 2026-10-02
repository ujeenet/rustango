# Notas de la versión

Cada cambio visible para el usuario llega con una entrada en el changelog. Esta página es la ruta hacia ellas, porque hasta ahora el sitio de documentación no tenía ninguna — quien leía aquí veía encabezados etiquetados con una versión dentro de las guías y nada más, y así fue como un encabezado clavado en 0.42 acabó leyéndose como «la documentación se detuvo en 0.42» ([#1304](https://github.com/ujeenet/rustango/issues/1304)).

## Dónde viven las notas

| Qué | Dónde | Mejor para |
|---|---|---|
| Historial completo, todas las versiones | [`CHANGELOG.md`](https://github.com/ujeenet/rustango/blob/main/CHANGELOG.md) | «cuándo cambió esto, y qué más se movió con ello» |
| Una página por versión | [GitHub Releases](https://github.com/ujeenet/rustango/releases) | «qué hay en la versión a la que acabo de actualizar» |
| Qué hay en la API ahora mismo | [docs.rs/rustango](https://docs.rs/rustango) | firmas, documentación por elemento |

El changelog es la fuente; una página de release es el mismo contenido para una sola versión. Ambos están solo en inglés.

## Cómo leer una entrada

Las entradas se agrupan bajo `Added` (añadido), `Changed` (cambiado), `Fixed` (corregido), `Removed` (eliminado) y `Security` (seguridad), la versión más nueva primero, siguiendo [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Dos convenciones que conviene conocer:

- **Las entradas dicen por qué, no solo qué.** Una entrada que cambia el comportamiento explica lo que ocurría antes, porque eso es lo que te dice si te afecta.
- **Los números de issue son estructurales.** `(#1229)` enlaza la discusión que produjo el cambio, que suele tener más detalle del que cabe en la entrada.

## Versionado

El proyecto sigue [SemVer](https://semver.org/) de forma laxa, con una salvedad que importa antes de 1.0: **nada anterior a 1.0 lleva garantía de estabilidad.** Una versión menor puede cambiar una API.

En la práctica el proyecto lo evita, y un cambio incompatible recibe una entrada `Changed` que dice qué hacer. Pero la garantía todavía no está ahí, así que fija una versión que hayas probado en lugar de un rango.

Se retiraron dos versiones. `0.51.0` y `0.51.1` prometían una reconciliación de migraciones que nunca se disparó contra una base de datos real; actualiza directamente a `0.51.2` o posterior, que corrige ambas. Consulta [Migraciones](migrations.md) para saber qué hacer si estás en una de ellas.

## Actualizar

Sube el pin, ejecuta tus tests y lee las entradas `Changed` y `Removed` de todas las versiones que te saltaste — no solo la de destino. La mayoría de las actualizaciones son una subida de versión y nada más.

Si cambió un modelo, `cargo run -- makemigrations` te muestra qué se movió antes de que nada toque la base de datos.
