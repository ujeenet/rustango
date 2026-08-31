# API-Konventionen

> **Für wen diese Seite gedacht ist.** Dies ist eine **fortgeschrittene Referenz für Rust-Entwickler**, die *mit* oder *am* Code des Frameworks arbeiten — sie erklärt die Namens-, Rückgabetyp- und Modulkonventionen hinter der Rust-API von Rustango. Sie ist **keine** Anleitung zum *Aufrufen* der REST-API einer Rustango-Anwendung über HTTP. Wenn Sie das suchen, beginnen Sie mit den [ViewSets](viewsets.md) (eine REST-API bauen) und dem [Glossar](glossary.md) (Begriffe in Klartext); kommen Sie hierher zurück, sobald Sie Rust gegen das Framework schreiben.

Diese Seite erklärt die Muster, denen die API von **Rustango** folgt, damit Sie das Verhalten jeder Methode vorhersagen können, bevor Sie deren Dokumentation lesen. Wenn Sie zu einem Feature beitragen oder es auditieren, sind dies die Regeln.

[![Namenskonvention von Rustango: das Methodensuffix sagt Ihnen, was sie entgegennimmt — `*_on` für einen typisierten Pool, blank für den Multi-Backend-Pool und pool-freie Signale](../img/api-conventions.png)](../img/api-conventions.png)

## Inhaltsverzeichnis

- [Naming](#naming)
- [Constructors](#constructors)
- [Return types](#return-types)
- [Async vs sync](#async-vs-sync)
- [The pool argument](#the-pool-argument)
- [Filtering](#filtering)
- [Errors](#errors)
- [Module naming](#module-naming)
- [Builders vs config structs](#builders-vs-config-structs)
- [Feature flags](#feature-flags)
- [Macros vs runtime](#macros-vs-runtime)
- [Contributing](#contributing)

---

## Namensgebung

Der Name einer Methode sagt Ihnen, was sie tut. Sobald Sie diese Suffixe gelernt haben, können Sie den Großteil der API erraten.

### Funktionen

- **`save_on(executor)`, `delete_on(executor)`** — Schreibmethoden nehmen einen *Executor* entgegen (einen Pool, eine Verbindung oder eine Transaktion — das, was mit der Datenbank spricht). Das Suffix `_on` bedeutet „führe dies gegen den Executor aus, den ich dir übergebe“.
- **`fetch_on(executor)`, `count_on(executor)`** — dasselbe `_on`-Suffix, für Lesevorgänge.
- **`save()`, `fetch()`, `count()`** ohne `_on` — Kurzform, die die `_on`-Variante mit einem Standard-`&pool` aufruft. Funktioniert nur dort, wo das Queryset oder Modell bereits eine Pool-Referenz hält (selten im Anwendungscode).
- **`from_X(value)`** — konvertiert AUS einem anderen Wert (z. B. `from_model(post)`, `from_base32(s)`).
- **`with_X(value)`** — eine Builder-Methode, die eine Option setzt und das Objekt zurückgibt, sodass Sie Aufrufe verketten können (z. B. `with_default_ttl(d)`, `with_access_ttl(secs)`).
- **`new()`** — der minimale Konstruktor. Alle Argumente, die er entgegennimmt, sind erforderliche Abhängigkeiten (z. B. `RedisCache::new(url)` — Sie können den Cache nicht ohne URL bauen).

### Typen

Diese folgen der Standard-Groß-/Kleinschreibung von Rust, genau wie die Aufteilung von Pythons PEP 8 zwischen Klassen und Funktionen:

- **`PascalCase`** — Typen, Traits und Enum-Varianten (wie Python-Klassen).
- **`snake_case`** — Module, Funktionen, Felder und lokale Variablen.
- **`SCREAMING_SNAKE_CASE`** — Konstanten, dazu die Konstante `Model::SCHEMA`, die das Derive-Makro für jedes Modell generiert.
- **`Boxed*`** — ein Alias für `Arc<dyn Trait>`, ein thread-sicherer geteilter Zeiger auf ein Trait-Objekt (die Rust-Art, „irgendeine Implementierung dieser Schnittstelle“ zu halten). Zum Beispiel `BoxedCache = Arc<dyn Cache>`. Dies ist der Standardtyp für ein austauschbares Backend, das Sie ersetzen können.

### Module

- **Singular**, wenn das Modul EINEN Haupttyp oder ein Hauptkonzept enthält: `cache`, `email`, `storage`, `signed_url`, `request_id`.
- **Plural**, wenn das Modul eine SAMMLUNG von Elementen enthält: `bulk_actions`, `api_keys`, `passwords`, `forms`, `signals`.

---

## Konstruktoren

Wie Sie ein Objekt bauen, hängt davon ab, was es benötigt. Es gibt einige Standardformen:

| Muster | Wann | Beispiel |
|---|---|---|
| `T::new()` | Minimal — keine erforderlichen Abhängigkeiten | `InMemoryCache::new()`, `Validator::new()` |
| `T::new(arg)` | Eine erforderliche Abhängigkeit | `EnvSecrets::with_prefix(s)`, `RedisCache::new(url)` |
| `T::with_X(arg)` | Builder-artige Überschreibung nach `new()` | `InMemoryCache::with_default_ttl(d)`, `JwtLifecycle::new(s).with_access_ttl(60)` |
| `T::from_X(arg)` | Konvertieren AUS Y | `TotpSecret::from_base32(s)`, `Locale::new(s)` (manchmal `from_str`) |
| `T::for_Y(arg)` | Ein auf ein bestimmtes Y beschränktes T bauen | `ViewSet::for_model(schema)` |

**Vermeiden Sie dies:** `T::with_X_and_Y_and_Z(a, b, c)` — ein Konstruktor, der alles entgegennimmt. Teilen Sie ihn stattdessen in `new(...)` plus verkettete `.with_*()`-Aufrufe auf.

---

## Rückgabetypen

Der Rückgabetyp einer Methode sagt Ihnen, wie sie fehlschlagen kann. Rust hat keine Exceptions, daher ist der Fehlschlag Teil des Rückgabewerts. Es gibt drei Formen.

**`Result<T, E>`** — wie eine Funktion, die entweder einen Wert zurückgibt oder eine Exception wirft. Sie erhalten entweder den Wert `T` oder einen Fehler `E` mit Details. Verwenden Sie es für Operationen, die fehlschlagen können und bei denen das *Warum* zählt:
- I/O: `pool.fetch(...).await -> Result<_, sqlx::Error>`
- Validierung: `Form::parse(data) -> Result<Self, FormErrors>`
- Ausstellung: `JwtLifecycle::issue_pair_with(uid, claims) -> Result<_, JwtIssueError>`

**`Option<T>`** — entweder ein Wert (`Some`) oder nichts (`None`), wie ein nullbares Feld. Verwenden Sie es, wenn „nichts gefunden“ ein normales Ergebnis ist und Sie keine Fehlermeldung benötigen, die erklärt, warum:
- Nachschlagen: `cache.get(k) -> Result<Option<String>, _>` (das `Result` deckt den I/O-Fehler ab; die `Option` deckt „Schlüssel nicht vorhanden“ ab)
- Verifizierung: `async JwtLifecycle::verify_access(token) -> Option<Claims>` („abgelaufen oder ungültig“ ist ein erwartetes Ergebnis, also genügt `None`)
- Optionale Config-Lesevorgänge: `env::optional("FOO") -> Result<Option<T>, _>`

**`bool`** — ein schlichtes Ja/Nein, wenn kein weiteres Detail benötigt wird:
- `cache.exists(k) -> Result<bool, _>` (das `Result` deckt das I/O ab; der `bool` ist die Antwort)
- `JwtLifecycle::revoke(token) -> bool` (true = zur Blacklist hinzugefügt)
- `disconnect_pre_save(id) -> bool` (true = ein Eintrag wurde entfernt)

**`Result<Option<T>>` oder `Result<T>` mit einem `NotFound`-Fehler?** Beide können „Nachschlagen fehlgeschlagen“ ausdrücken, wählen Sie also danach, wie außergewöhnlich „nicht gefunden“ ist:
- Verwenden Sie `Result<Option<T>>`, wenn „nicht gefunden“ die Regel ist — Ihr Code verzweigt ohnehin fast immer auf `Some`/`None`.
- Verwenden Sie `Result<T>` mit einer `NotFound`-Fehlervariante, wenn „nicht gefunden“ außergewöhnlich ist — etwas, das Sie als Warnung protokollieren oder in ein 404 verwandeln würden.

---

## Async vs sync

Die Faustregel: Wenn eine Methode auf etwas wartet (die Datenbank, das Netzwerk oder die Festplatte), ist sie `async` und Sie müssen sie `.await`en. Wenn sie nur rechnet, ist es ein normaler synchroner Aufruf. Diese Tabelle buchstabiert es aus.

| Operation | Sync oder async? |
|---|---|
| Trait-Methode, die I/O berührt (DB, Netzwerk, Datei) | **async** |
| Trait-Methode, die reine Berechnung ist (`hash`, `verify`, `encode`) | **sync** |
| Builder-Methoden (`with_X`, verkettbare Setter) | **sync** |
| Makros (`derive(Model)`, `derive(Serializer)`) | **N/A** (zur Kompilierzeit) |
| Signal `connect_*` (registriert einen Empfänger) | **sync** |
| Signal `send_*` (dispatcht an async-Empfänger) | **async** |

**Ausnahme:** `Cache::set` ist `async`, obwohl die In-Memory-Variante (`InMemoryCache::set`) niemals tatsächlich wartet. Der Trait ist für den Redis-Fall geformt, der es tut. Das ist beabsichtigt: eine Trait-Methode sollte `async` sein, wenn *irgendeine* sinnvolle Implementierung warten muss, damit alle Backends eine Signatur teilen.

---

## Das Pool-Argument

Jeder ORM-Aufruf nimmt einen Pool oder Executor (das Datenbank-Handle) als **letztes** Argument entgegen. Sie übergeben die Verbindung jedes Mal, anstatt sich auf einen versteckten globalen Zustand zu verlassen:

```rust
post.save_on(&pool).await?
Post::objects().filter(...).fetch_on(&pool).await?
send_post_save(&post, ctx).await                  // ⚠️ no pool — signals are pool-free
```

**Eine Ausnahme:** Signale nehmen keinen Pool entgegen, weil sie die Datenbank niemals berühren. Die Regel hält: Alles, was die DB erreicht, nimmt den Pool; alles, was das nicht tut, nicht.

**Warum jedes Mal übergeben?** Rust bevorzugt sichtbare Abhängigkeiten gegenüber verstecktem globalem Zustand. Django hält die Verbindung im Thread-Local-Speicher, aber das bricht in Rusts async-Welt zusammen, wo eine Task mitten in einer Anfrage zwischen Threads springen kann. Der Nachteil ist mehr Tipparbeit; der Vorteil ist, dass Sie nach jeder Stelle greppen können, die die Datenbank berührt.

Wenn Sie feststellen, dass Sie `&pool` durch zehn Schichten von Funktionsaufrufen durchreichen, akzeptieren Sie einmal `impl Executor` am öffentlichen Einstiegspunkt und lassen Sie die internen Helfer diese eine Verbindung teilen.

---

## Filtern

Es gibt drei Möglichkeiten, ein Queryset zu filtern, und sie kombinieren sich alle in einer Abfrage. Wählen Sie danach, woher der Filter kommt.

```rust
// 1. HTTP query string (set via ViewSet filter_fields, parsed at request time)
//    GET /api/posts?author_id=42&status__ne=archived

// 2. String-keyed (lookup at compile of the queryset; runtime field name resolution)
Post::objects().filter("author_id", Op::Eq, SqlValue::I64(42));

// 3. Typed columns (compile-time field check)
Post::objects().where_(Post::author_id.eq(42));
```

| Syntax | Verwenden, wenn |
|---|---|
| HTTP-Query | Öffentliche API-Endpunkte — das ViewSet parst diese für Sie, wie die Filter-Backends von DRF |
| String-basiertes `.filter` | Generischer CRUD- oder Admin-Code, wo Feldnamen aus der Config stammen und zur Kompilierzeit nicht bekannt sind |
| Typisiertes `.where_` | Ihr Anwendungscode — der bevorzugte Standard. Der Compiler prüft, dass das Feld existiert und die Typen übereinstimmen |

Sie können **alle drei mischen** in einem einzigen Queryset.

---

## Fehler

**Rustango** hat **über 20 Fehlertypen** — einen pro Modul — anstelle einer einzigen Auffang-Exception-Klasse. Sie bilden eine lose Hierarchie, und ein Typ auf oberster Ebene verbindet sie, sodass Sie selten einzeln mit ihnen umgehen.

| Schicht | Modul | Fehlertyp |
|---|---|---|
| ORM-I/O | `sql::*` | `ExecError` |
| ORM-SQL-Writer | `sql::*` | `SqlError` (Variante von `ExecError::Sql`) |
| Migrationen | `migrate::*` | `MigrateError` |
| Formulare | `forms::*` | `FormError` (einzeln) + `FormErrors` (mehrfach) + `ModelFormError` |
| Cache | `cache::*` | `CacheError` |
| E-Mail | `email::*` | `MailError` |
| Speicher | `storage::*` | `StorageError` |
| Auth-Backends | `tenancy::auth_backends` | `AuthError` |
| JWT | `tenancy::jwt_lifecycle` | `JwtIssueError` |
| API-Schlüssel | `api_keys::*` | `ApiKeyError` |
| Passwörter | `passwords::*` | `PasswordError` |
| Webhooks | `webhook::*` | (gibt bool zurück, kein dedizierter Fehler) |
| Signierte URLs | `signed_url::*` | `SignedUrlError` |
| Bulk-Aktionen | `bulk_actions::*` | `BulkActionError` |
| Fixtures | `fixtures::*` | `FixtureError` |
| IP-Filter | `ip_filter::*` | `IpFilterError` |
| i18n | `i18n::*` | `I18nError` |
| Env | `env::*` | `EnvError` |
| Secrets | `secrets::*` | `SecretsError` |
| API-Antworten | `api_errors::*` | `ApiError` (HTTP-geformt, nicht intern) |

**Der eine für Handler:** Es gibt eine `RustangoError`-Enum auf oberster Ebene (aus `lib.rs` exportiert, zusammen mit dem Alias `RustangoResult<T> = Result<T, RustangoError>`). Sie umschließt jeden der obigen Fehler mit `From`-Konvertierungen, sodass der `?`-Operator jeden Modulfehler automatisch in sie hochstuft. Sie implementiert außerdem `IntoResponse`, was bedeutet, dass jede Variante auf einen sinnvollen HTTP-Status abgebildet wird, wenn sie aus einem Handler zurückgegeben wird. Die Aufteilung ist einfach: Verwenden Sie die spezifischen Fehler pro Modul tief in Ihrem Code und `RustangoError` / `RustangoResult` an der Handler-Grenze. Für Fehler aus Drittanbieter-Crates umschließen `RustangoError::other(msg)` / `RustangoError::other_from(e)` jeden `std::error::Error + Send + Sync + 'static`.

**Ein Handler-Beispiel:**

```rust
use rustango::api_errors::ApiError;

async fn handler() -> Result<Json<X>, ApiError> {
    let post = Post::objects().get(&pool, 1).await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(post))
}
```

`ApiError` implementiert `IntoResponse`, sodass die Rückgabe automatisch die standardmäßige JSON-Fehlerform erzeugt.

---

## Modul-Namensgebung

Der Name eines Moduls sollte Sie **die darin enthaltenen Typnamen erraten** lassen, ohne die Datei zu öffnen.

| Modul | Beherbergt | Nachschlage-Zuversicht |
|---|---|---|
| `cache` | den `Cache`-Trait, die `*Cache`-Impls | hoch |
| `email` | den `Mailer`-Trait, `Email`, die `*Mailer`-Impls | hoch |
| `storage` | den `Storage`-Trait, die `*Storage`-Impls | hoch |
| `signed_url` | die freien Funktionen `sign`, `verify` | mittel |
| `text` | die freien Funktionen `slugify`, `html_escape`, `truncate` | mittel |
| `bulk_actions` | `BulkActionRegistry`, `BulkAction`, die `Bulk*Action`-Impls | hoch |
| `api_keys` | die freien Funktionen `generate_key`, `verify_key`, `split_token` | mittel |

**Vermeiden Sie dies:** ein Modul, das ein zusammenhangloses Sammelsurium enthält (`utils`, `helpers`, `common`). Wenn Sie das einzige Konzept, das es abdeckt, nicht benennen können, sollte es kein Modul sein.

---

## Builder vs Config-Structs

Es gibt zwei Möglichkeiten, ein konfiguriertes Objekt zu übergeben. Wählen Sie danach, wie Benutzer es einrichten werden.

### Builder: verkettete Setter, kein `Default`

```rust
let l = SecurityHeadersLayer::strict()
    .csp(...)
    .header("x-extra", "v");
```

Verwenden, wenn:
- Die meisten Benutzer von einem Preset ausgehen und es anpassen
- Setter Absicht ausdrücken (z. B. liest sich `.errors_only()` besser als `.log_success(false)`)
- Das Struct viele optionale Felder hat (10+)

### Config-Struct: Felder direkt setzen, auf `Default` zurückfallen

```rust
let l = AccessLogLayer {
    log_success: false,
    include_ip: true,
    slow_threshold_ms: 500,
    ..Default::default()
};
```

Verwenden, wenn:
- Benutzer bei jedem Feld explizit sein möchten
- Reflection / Serialisierung von Bedeutung ist
- In-Place-Aktualisierung üblich ist (`config.field = ...`)

**Als Regel verwendet **Rustango** Builder** für HTTP-Middleware (`security_headers`, `cors`, `rate_limit` und so weiter) und Config-Structs für einfache Datenträger (`Email`, `AccessLogLayer`, den internen Zustand von `RateLimitLayer`).

---

## Feature-Flags

Ein *Feature* ist ein Cargo-Build-Flag (das `[features]` von `Cargo.toml`), das einen Teil des Crates ein- oder ausschaltet — ähnlich der Package-Discovery von Laravel oder den `INSTALLED_APPS` von Django, aber zur Kompilierzeit aufgelöst. Jedes Modul, das eine zusätzliche Abhängigkeit hereinzieht, sitzt hinter einem. Der Standardsatz lautet „die wollen Sie mit ziemlicher Sicherheit“:

```toml
default = [
    "postgres", "manage", "admin", "config", "forms", "serializer",
    "cache", "signals", "email", "storage", "scheduler", "secrets", "totp",
    "webhook", "webhook-delivery", "api_keys", "passwords", "signed_url",
    "notifications", "casts", "jobs", "jobs-postgres", "auth_flows", "sse",
    "websocket", "oauth2", "http-client", "compression", "openapi",
    "csp-nonce", "sessions", "hmac-auth", "jwt", "uploads", "storage-s3",
    "media", "runserver", "template_views",
]
```

**Standardmäßig aus:** Features, die schwere Abhängigkeiten oder externe Dienste hereinziehen:
- `tenancy` — fügt `argon2`, `hmac`, `sha2`, `cookie`, `tower` hinzu (die meisten Anwendungen brauchen es nicht)
- `cache-redis` — fügt die `redis`-Crate hinzu (die meisten Anwendungen kommen mit dem In-Memory-Cache aus)
- `csrf` — wird von `admin` automatisch eingeschaltet, ist aber auch einzeln verfügbar

Um ein Binary zu verschlanken, das nicht alles braucht, deaktivieren Sie die Standardwerte und listen Sie nur auf, was Sie verwenden:

```toml
rustango = { version = "0.44", default-features = false, features = ["postgres", "admin"] }
```

---

## Makros vs Laufzeit

Ein *Makro* ist Code, der zur Kompilierzeit Code generiert (`#[derive(Model)]` und Verwandte) — ungefähr das, was ein Rails-Generator tut, außer dass es bei jedem Build läuft und der Compiler das Ergebnis prüft. Die Aufteilung unten entscheidet, was von einem Makro versus schlichtem Laufzeitcode erledigt wird.

| Anliegen | Makro oder Laufzeit? |
|---|---|
| Schema-Metadaten für `inventory` | Makro (`#[derive(Model)]`) |
| Schema-gesteuerter Abfragebau | Laufzeit (nutzt das `&'static ModelSchema` aus dem Makro) |
| Formular-Parsing | Makro für das Struct (`#[derive(Form)]`); Laufzeit für die Parsing-Logik |
| Serializer-Feldauswahl | Makro (`#[derive(Serializer)]`) — erzeugt ein `from_model` + eine eigene `Serialize`-Impl |
| Migrations-Operationen | Laufzeit (`SchemaSnapshot`-Diff) |
| Signal-Dispatch | Laufzeit (`TypeId`-basiertes Registry, kein Makro pro Modell) |
| Pattern-Matching der Auth-Backends | Laufzeit (`#[async_trait]` auf `AuthBackend`) |

**Regel:** Verwenden Sie ein Makro für alles, was der Compiler vorab verifizieren kann (Feldnamen müssen existieren, Typen müssen übereinstimmen). Verwenden Sie Laufzeitcode für alles, was pro Anfrage oder pro Deployment variiert.

---

## Mitwirken

Wenn Sie ein neues Feature hinzufügen, befolgen Sie diese Schritte:

1. **Ein Modul pro Konzept**, in `crates/rustango/src/<name>.rs` oder `<name>/mod.rs`.
2. **Fügen Sie Rustdoc auf Modulebene hinzu** mit einem „Quick start“-Beispiel in einem `// ignore`-Block.
3. **Fügen Sie ein Feature-Flag hinzu, wenn Sie eine neue Abhängigkeit hereinziehen** — benennen Sie es nach dem Modul (`feature = "<name>"`).
4. **Re-exportieren Sie das Modul aus `lib.rs`** mit einer einzeiligen Rustdoc.
5. **Platzieren Sie Unit-Tests in derselben Datei**, hinter `#[cfg(test)] mod tests` — keine Datenbank, es sei denn, Sie brauchen wirklich eine.
6. **Platzieren Sie Integrationstests in `crates/rustango/tests/<name>.rs`** für die End-to-End-Geschichte.
7. **Fügen Sie keinen neuen Fehlertyp hinzu, es sei denn, die bestehenden passen nicht** — erweitern Sie zuerst eine bestehende Enum.
8. **Folgen Sie dem [Rückgabetyp-Leitfaden](#return-types)** bei der Wahl zwischen `Result`, `Option` oder `bool`.
9. **Fügen Sie einen `manage`-Unterbefehl hinzu?** Verdrahten Sie ihn in den `match cmd`-Dispatcher und `print_help`, fügen Sie einen Test in `crates/rustango/tests/migrate_manage.rs` hinzu und dokumentieren Sie eine Zeile in `docs/manage.md`.
10. **Aktualisieren Sie `CHANGELOG.md`** mit einem `Added`-Eintrag unter der nächsten Version.

Wenn Sie die API brechen:
- Markieren Sie das alte Element mit `#[deprecated(since = "...", note = "use X instead")]` und behalten Sie es eine volle Minor-Version lang, bevor Sie es entfernen.
- Halten Sie es in `CHANGELOG.md` unter `Breaking changes` fest.
- Verlinken Sie den Migrationspfad aus den Release Notes.
