# Dateien, Uploads & Medien

Fast jede App speichert Nutzerdateien — Avatare, Anhänge, exportierte Berichte,
Bilder. **Rustango** gibt dir ein `Storage`-Trait mit austauschbaren Backends (lokale
Festplatte, S3-kompatibler Objektspeicher, In-Memory für Tests), einen sicheren Multipart-**Upload**-Helfer
mit Größen-/Typ-Schutzvorkehrungen und — wenn du eine nachverfolgte Mediathek brauchst — einen
datenbankgestützten `MediaManager` mit vorsignierten URLs. Schreibe deinen Code
einmal gegen das Trait; wechsle von lokaler Festplatte zu S3 mit einer einzeiligen Änderung.

[![Dateien in Rustango: Ein Multipart-Upload wird größen- und erweiterungsgeprüft und dann durch das Storage-Trait geschrieben; dasselbe Trait unterlegt lokale Festplatte, S3 und In-Memory, und url() gibt eine öffentliche Adresse zurück](img/files.png)](img/files.png)

> **Neu bei einem Begriff hier?** *Storage-Backend*, *Multipart*, *Objektspeicher*,
> *vorsignierte URL* — siehe das [Glossar](glossary.md).

> **Quelle:** `rustango::storage` (`Storage`, `LocalStorage`, `InMemoryStorage`,
> `s3::S3Storage`, `BoxedStorage`), `rustango::uploads` (`save_uploads`,
> `UploadConfig`, `sanitize_filename`) und `rustango::media`
> (`Media`, `MediaManager`) — hinter den Features `storage` / `uploads` / `storage-s3` /
> `media` (alle standardmäßig aktiviert).
>
> **Lauffähige Version:** Die Storage- + Upload-Schutzvorkehrungs-Snippets sind kopiert aus
> [`files_doc.rs`](../crates/rustango/tests/files_doc.rs)
> (`cargo test -p rustango --test files_doc`); der End-to-End-Multipart-`save_uploads`-Ablauf
> wird von den In-File-Tests in
> `crates/rustango/src/uploads.rs` selbst erprobt, und die Mediathek von
> [`media_sqlite_live.rs`](../crates/rustango/tests/media_sqlite_live.rs).

## Inhaltsverzeichnis

- [Schritt 1 — Ein Storage-Backend wählen](#step-1--pick-a-storage-backend)
- [Schritt 2 — Dateien speichern, laden und ausliefern](#step-2--save-load-and-serve-files)
- [Schritt 3 — Einen Upload annehmen](#step-3--accept-an-upload)
- [Sichere Dateinamen](#safe-filenames)
- [Produktion: S3-kompatibler Speicher](#production-s3-compatible-storage)
- [Die Mediathek](#the-media-library)
- [Referenz](#reference)
- [Siehe auch](#see-also)

---

## Schritt 1 — Ein Storage-Backend wählen

Jedes Backend implementiert dasselbe `Storage`-Trait, sodass dein Code den konkreten Typ nie
benennt — er hält ein **`BoxedStorage`** (`Arc<dyn Storage>`):

```rust
use rustango::storage::{BoxedStorage, LocalStorage};
use std::path::PathBuf;
use std::sync::Arc;

let storage: BoxedStorage = Arc::new(LocalStorage::new(PathBuf::from("./uploads")));
```

| Backend | Feature | Verwenden für |
|---|---|---|
| `LocalStorage` | `storage` | Ein-Server-Deployments — Dateien auf lokaler Festplatte |
| `S3Storage` | `storage-s3` | Produktion — S3 / R2 / B2 / MinIO Objektspeicher |
| `InMemoryStorage` | `storage` | Tests — eine `HashMap`, berührt nie die Festplatte |

---

## Schritt 2 — Dateien speichern, laden und ausliefern

Das Trait besteht aus vier async-Methoden, indiziert durch einen String-Pfad. `save` schreibt
Bytes, `load` liest sie zurück, dazu `exists` / `delete`:

```rust
use rustango::storage::{Storage, InMemoryStorage};

let store = InMemoryStorage::new();
store.save("avatars/7.png", &png_bytes).await?;
assert!(store.exists("avatars/7.png").await?);
let bytes = store.load("avatars/7.png").await?;
store.delete("avatars/7.png").await?;
```

**Die Datei ausliefern.** Hänge eine Basis-URL an (dein CDN oder statischer Host) und `url(key)`
baut die öffentliche Adresse, die du am Modell speicherst und dem Browser übergibst:

```rust
let store = LocalStorage::new("./uploads".into())
    .with_base_url("https://cdn.example.com/uploads");

store.url("docs/report.pdf");   // Some("https://cdn.example.com/uploads/docs/report.pdf")
```

Ohne Basis-URL gibt `url()` `None` zurück — du würdest die Bytes stattdessen durch einen
Handler streamen. `LocalStorage` schützt außerdem vor Path-Traversal in Keys.

---

## Schritt 3 — Einen Upload annehmen

`save_uploads` konsumiert einen axum-`Multipart`-Body, validiert jede Datei gegen eine
`UploadConfig` und schreibt die Überlebenden in dein `Storage` — als Stream, sodass eine
überdimensionierte Datei mitten in der Übertragung abgelehnt wird, statt zuerst in den Speicher
gepuffert zu werden.

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

Die Schutzvorkehrungen werden erzwungen (und verifiziert): `allowed_extensions` ist
**case-insensitiv** (`"PNG"` und `"png"` sind dasselbe), und `max_bytes` bricht den Stream ab,
sobald die Größe überschritten wird. Die In-File-`uploads`-Tests treiben echte Multipart-Bodies an
und behaupten, dass Dateien im Speicher landen, überdimensionierte Dateien abgelehnt und
nicht erlaubte Erweiterungen verweigert werden.

```rust
let cfg = UploadConfig::new("avatars/").allowed_extensions(&["PNG", "Jpg"]);
assert!(cfg.allowed_extensions.contains("png"));   // normalized to lowercase
assert!(cfg.allowed_extensions.contains("jpg"));
```

---

## Sichere Dateinamen

Vertraue nie einem vom Client gelieferten Dateinamen. `sanitize_filename` reduziert ihn auf einen
sicheren Basenamen — es entfernt Verzeichniskomponenten (Path-Traversal) und ersetzt unsichere
Zeichen:

```rust
use rustango::uploads::sanitize_filename;

sanitize_filename("../../etc/passwd");   // "passwd"   — no traversal
sanitize_filename("my photo!.png");      // "my_photo_.png"
sanitize_filename("");                    // "upload"   — never empty
```

`save_uploads` wendet dies für dich an; rufe es nur direkt auf, wenn du Keys von Hand baust.

---

## Produktion: S3-kompatibler Speicher

Für Multi-Server-Deployments tausche `LocalStorage` gegen `S3Storage` (hinter dem
`storage-s3`-Feature). Es spricht die S3-API mit einem selbstgebauten SigV4-Signierer, sodass es
mit **AWS S3, Cloudflare R2, Backblaze B2 und MinIO** funktioniert. Das Trait ist
identisch — nur der Konstruktor ändert sich:

```rust
use rustango::storage::s3::S3Storage;   // needs the `storage-s3` feature

let storage: BoxedStorage = Arc::new(
    S3Storage::new(/* bucket, region, endpoint, credentials */)
);
// save / load / delete / url — exactly the same calls as LocalStorage
```

Deine Handler und Modelle ändern sich nicht; nur die Verdrahtung beim Start.

---

## Die Mediathek

Wenn Dateien erstklassige Datensätze sind — in der Datenbank nachverfolgt, im Admin durchstöberbar,
mit Thumbnails und CDN-/vorsignierter Auslieferung — greife zu `rustango::media` statt zu rohem
`Storage`. `MediaManager` persistiert eine `Media`-Zeile pro Datei und
unterstützt zwei Upload-Abläufe:

- **Serverseitig:** `manager.save_bytes(...)` speichert die Bytes und die Zeile in einem
  Aufruf.
- **Direkt-in-den-Speicher:** `manager.begin_upload(...)` gibt eine **vorsignierte PUT**-URL
  zurück, zu der der Browser direkt hochlädt (dein Server proxyt die Bytes nie),
  dann bestätigst du die Zeile.

```rust
use rustango::media::{Media, MediaManager};

let manager = MediaManager::new_pool(pool.clone(), registry);
// Hand the browser a short-lived download link:
let url = manager.presigned_get(&media, Duration::from_secs(3600)).await?;
```

Er kümmert sich außerdem um Soft-Delete und das Bereinigen von Waisen. Der vollständige Ablauf wird
in `media_sqlite_live.rs` erprobt; die vorsignierten/Direct-Upload-Methoden des Managers sind
PostgreSQL-orientiert.

### Wie die Medientabellen angelegt werden

Die Medientabellen (`rustango_media`, `rustango_media_collections`,
`rustango_media_tags`, `rustango_media_tag_links`) sind verwaltete Modelle. Ihr
Schema wird als **System-Migration** ausgeliefert und angelegt — pro Tenant, wenn das
`media`-Feature aktiviert ist — auf dieselbe Weise wie die übrigen frameworkeigenen
Tabellen, immer wenn du `migrate` ausführst / einen Tenant provisionierst. Es gibt keinen faulen
Schritt „beim ersten Gebrauch anlegen"; ist das Feature aus, werden die Tabellen nie angelegt.

> **Upgrade von vor 0.51.** Frühere Versionen legten die Medientabellen faul über einen
> `ensure_table`-DDL-Aufruf statt über eine Migration an. Beim ersten
> `migrate` (oder Tenant-Provisionierung) nach dem Upgrade **rekonziliert** das Framework
> **automatisch**:
> Weil die Tabellen bereits existieren, wird die generierte Medien-Migration im
> System-Migrations-Ledger verzeichnet, *ohne* ihr `CREATE TABLE` erneut auszuführen, und deine
> bestehenden Zeilen bleiben unberührt. **Kein manueller Schritt ist erforderlich** — das Upgrade,
> das andernfalls mit `relation already exists` / `table already exists` fehlschlagen würde,
> funktioniert jetzt einfach. (Eingeführt in 0.51.1; siehe das CHANGELOG.)

---

## Referenz

**`Storage`-Trait:** `save(key, &bytes)` · `load(key)` · `delete(key)` ·
`exists(key)` · `url(key) -> Option<String>`.

**`UploadConfig`:** `new(prefix)` · `.max_bytes(n)` · `.allowed_extensions(&[..])`
(case-insensitiv) · `.randomize_filename(bool)`. Verwendet von
`save_uploads(multipart, &cfg, &storage)`.

**Backends:** `LocalStorage` (Festplatte) · `S3Storage` (Objektspeicher, `storage-s3`)
· `InMemoryStorage` (Tests). Alle geben ein `BoxedStorage` zurück.

---

## Siehe auch

- [Der Admin](admin.md) — Medien- und FK-Widgets bringen hochgeladene Dateien in der UI zur Anzeige.
- [Hintergrundjobs](jobs.md) — skaliere/transcodiere einen Upload außerhalb des Requests.
- [Caching](caching.md) — dasselbe Muster „Backend-austauschen" per Trait.
- [Sicherheitsleitfaden](security.md) — Validieren nicht vertrauenswürdiger Upload-Eingaben.
