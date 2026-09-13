# Archivos, subidas y medios

Casi toda aplicación almacena archivos de usuario — avatares, adjuntos, informes exportados,
imágenes. **Rustango** te ofrece un trait `Storage` con backends intercambiables (disco
local, almacenamiento de objetos compatible con S3, en memoria para pruebas), un ayudante de
**subida** multipart seguro con guardas de tamaño/tipo, y — cuando necesitas una
biblioteca de medios rastreada — un `MediaManager` respaldado por base de datos con URLs
prefirmadas. Escribe tu código una sola vez contra el trait; cambia de disco local a S3 con un
cambio de una línea.

[![Archivos en Rustango: una subida multipart se comprueba en tamaño y extensión y luego se escribe a través del trait Storage; el mismo trait respalda disco local, S3 y memoria, y url() devuelve una dirección pública](../img/files.png)](../img/files.png)

> **¿Nuevo con algún término aquí?** *backend de almacenamiento*, *multipart*, *almacenamiento de
> objetos*, *URL prefirmada* — ver el [glosario](glossary.md).

> **Fuente:** `rustango::storage` (`Storage`, `LocalStorage`, `InMemoryStorage`,
> `s3::S3Storage`, `BoxedStorage`), `rustango::uploads` (`save_uploads`,
> `UploadConfig`, `sanitize_filename`), y `rustango::media`
> (`Media`, `MediaManager`) — tras las características `storage` / `uploads` / `storage-s3` /
> `media` (todas activadas por defecto).
>
> **Versión ejecutable:** los snippets de Storage + guardas de subida se copian de
> [`files_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/files_doc.rs)
> (`cargo test -p rustango --test files_doc`); el flujo multipart `save_uploads` de
> extremo a extremo se somete a prueba mediante los tests internos en
> `crates/rustango/src/uploads.rs`, y la biblioteca de medios mediante
> [`media_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/media_sqlite_live.rs).

## Tabla de contenidos

- [Paso 1 — Elige un backend de almacenamiento](#step-1--pick-a-storage-backend)
- [Paso 2 — Guarda, carga y sirve archivos](#step-2--save-load-and-serve-files)
- [Paso 3 — Acepta una subida](#step-3--accept-an-upload)
- [Nombres de archivo seguros](#safe-filenames)
- [Producción: almacenamiento compatible con S3](#production-s3-compatible-storage)
- [La biblioteca de medios](#the-media-library)
- [Referencia](#reference)
- [Véase también](#see-also)

---

## Paso 1 — Elige un backend de almacenamiento

Cada backend implementa el mismo trait `Storage`, de modo que tu código nunca nombra el tipo
concreto — sostiene un **`BoxedStorage`** (`Arc<dyn Storage>`):

```rust
use rustango::storage::{BoxedStorage, LocalStorage};
use std::path::PathBuf;
use std::sync::Arc;

let storage: BoxedStorage = Arc::new(LocalStorage::new(PathBuf::from("./uploads")));
```

| Backend | Característica | Úsalo para |
|---|---|---|
| `LocalStorage` | `storage` | despliegues de un solo servidor — archivos en disco local |
| `S3Storage` | `storage-s3` | producción — almacenamiento de objetos S3 / R2 / B2 / MinIO |
| `InMemoryStorage` | `storage` | pruebas — un `HashMap`, nunca toca el disco |

---

## Paso 2 — Guarda, carga y sirve archivos

El trait son cuatro métodos async, indexados por una ruta en forma de cadena. `save` escribe
bytes, `load` los vuelve a leer, más `exists` / `delete`:

```rust
use rustango::storage::{Storage, InMemoryStorage};

let store = InMemoryStorage::new();
store.save("avatars/7.png", &png_bytes).await?;
assert!(store.exists("avatars/7.png").await?);
let bytes = store.load("avatars/7.png").await?;
store.delete("avatars/7.png").await?;
```

**Servir el archivo.** Adjunta una URL base (tu CDN o host estático) y `url(key)`
construye la dirección pública que almacenas en el modelo y entregas al navegador:

```rust
let store = LocalStorage::new("./uploads".into())
    .with_base_url("https://cdn.example.com/uploads");

store.url("docs/report.pdf");   // Some("https://cdn.example.com/uploads/docs/report.pdf")
```

Sin una URL base, `url()` devuelve `None` — en su lugar transmitirías los bytes a través de un
handler. `LocalStorage` también protege contra el path traversal en las claves.

---

## Paso 3 — Acepta una subida

`save_uploads` consume un cuerpo `Multipart` de axum, valida cada archivo contra un
`UploadConfig`, y escribe los supervivientes en tu `Storage` — en streaming, de modo que un archivo
sobredimensionado se rechaza a mitad de transferencia en lugar de almacenarse primero en memoria.

```rust
use rustango::uploads::{save_uploads, UploadConfig};
use axum::extract::Multipart;

async fn upload(mp: Multipart) -> Result<impl IntoResponse, UploadError> {
    let cfg = UploadConfig::new("avatars/")          // key prefix
        .max_bytes(2 * 1024 * 1024)                  // reject files over 2 MiB
        .allowed_extensions(&["png", "jpg", "jpeg", "webp"])
        .randomize_filename(true);                   // avoid collisions

    let saved = save_uploads(mp, &cfg, &storage).await?;   // Vec<SavedUpload>
    Ok(Json(saved))
}
```

Las guardas se aplican (y se verifican): `allowed_extensions` es **insensible a mayúsculas y
minúsculas** (`"PNG"` y `"png"` son lo mismo), y `max_bytes` aborta el stream en cuanto se
excede el tamaño. Los tests internos de `uploads` conducen cuerpos multipart reales y
afirman que los archivos aterrizan en el almacenamiento, que los archivos sobredimensionados se
rechazan y que las extensiones no permitidas se deniegan.

```rust
let cfg = UploadConfig::new("avatars/").allowed_extensions(&["PNG", "Jpg"]);
assert!(cfg.allowed_extensions.contains("png"));   // normalized to lowercase
assert!(cfg.allowed_extensions.contains("jpg"));
```

---

## Nombres de archivo seguros

Nunca confíes en un nombre de archivo suministrado por el cliente. `sanitize_filename` lo reduce a
un basename seguro — eliminando componentes de directorio (path traversal) y reemplazando
caracteres inseguros:

```rust
use rustango::uploads::sanitize_filename;

sanitize_filename("../../etc/passwd");   // "passwd"   — no traversal
sanitize_filename("my photo!.png");      // "my_photo_.png"
sanitize_filename("");                    // "upload"   — never empty
```

`save_uploads` lo aplica por ti; llámalo directamente solo si construyes claves a mano.

---

## Producción: almacenamiento compatible con S3

Para despliegues multi-servidor, cambia `LocalStorage` por `S3Storage` (tras la
característica `storage-s3`). Habla la API de S3 con un firmante SigV4 hecho a mano, de modo que
funciona con **AWS S3, Cloudflare R2, Backblaze B2 y MinIO**. El trait es
idéntico — solo cambia el constructor:

```rust
use rustango::storage::s3::S3Storage;   // needs the `storage-s3` feature

let storage: BoxedStorage = Arc::new(
    S3Storage::new(/* bucket, region, endpoint, credentials */)
);
// save / load / delete / url — exactly the same calls as LocalStorage
```

Tus handlers y modelos no cambian; solo cambia el cableado en el arranque.

---

## La biblioteca de medios

Cuando los archivos son registros de primera clase — rastreados en la base de datos, navegables en
el admin, con miniaturas y entrega por CDN/prefirmada — recurre a `rustango::media` en lugar de a
`Storage` en bruto. `MediaManager` persiste una fila `Media` por archivo y
admite dos flujos de subida:

- **Del lado del servidor:** `manager.save_bytes(...)` almacena los bytes y la fila en una sola
  llamada.
- **Directo al almacenamiento:** `manager.begin_upload(...)` devuelve una URL de **PUT
  prefirmado** a la que el navegador sube directamente (tu servidor nunca hace de proxy de los
  bytes), luego confirmas la fila.

```rust
use rustango::media::{Media, MediaManager};

let manager = MediaManager::new_pool(pool.clone(), registry);
// Hand the browser a short-lived download link:
let url = manager.presigned_get(&media, Duration::from_secs(3600)).await?;
```

También gestiona el borrado lógico y la purga de huérfanos. El flujo completo se somete a prueba
en `media_sqlite_live.rs`; los métodos prefirmados/de subida directa del manager están
orientados a PostgreSQL.

### Cómo se crean las tablas de medios

Las tablas de medios (`rustango_media`, `rustango_media_collections`,
`rustango_media_tags`, `rustango_media_tag_links`) son modelos gestionados. Su
esquema se envía como una **migración de sistema** y se crea — por inquilino, cuando la
característica `media` está activada — de la misma forma que el resto de las propias tablas del
framework, cada vez que ejecutas `migrate` / aprovisionas un inquilino. No hay un paso perezoso de
«crear en el primer uso»; si la característica está desactivada, las tablas nunca se crean.

> **Actualización desde antes de 0.51.** Las versiones anteriores creaban las tablas de medios de
> forma perezosa mediante una llamada DDL `ensure_table` en lugar de una migración. En el primer
> `migrate` (o aprovisionamiento de inquilino) tras la actualización, el framework **reconcilia
> automáticamente**:
> como las tablas ya existen, la migración de medios generada se registra en
> el libro mayor de migraciones de sistema *sin* volver a ejecutar su `CREATE TABLE`, y tus
> filas existentes quedan intactas. **No se requiere ningún paso manual** — la actualización
> que de otro modo fallaría con `relation already exists` / `table already exists`
> ahora simplemente funciona. (Introducido en 0.51.1; ver el CHANGELOG.)

---

## Referencia

**Trait `Storage`:** `save(key, &bytes)` · `load(key)` · `delete(key)` ·
`exists(key)` · `url(key) -> Option<String>`.

**`UploadConfig`:** `new(prefix)` · `.max_bytes(n)` · `.allowed_extensions(&[..])`
(insensible a mayúsculas/minúsculas) · `.randomize_filename(bool)`. Usado por
`save_uploads(multipart, &cfg, &storage)`.

**Backends:** `LocalStorage` (disco) · `S3Storage` (almacenamiento de objetos, `storage-s3`)
· `InMemoryStorage` (pruebas). Todos devuelven un `BoxedStorage`.

---

## Véase también

- [El admin](admin.md) — los medios y los widgets de FK muestran los archivos subidos en la UI.
- [Trabajos en segundo plano](jobs.md) — redimensiona/transcodifica una subida fuera de la petición.
- [Caché](caching.md) — el mismo patrón de trait «cambia-el-backend».
- [Guía de seguridad](security.md) — validar entradas de subida no confiables.
