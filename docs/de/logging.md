# Logging

Logs sind die Art, wie eine laufende Anwendung berichtet, was sie getan hat.
**Rustango** baut auf [`tracing`](https://docs.rs/tracing) auf — derselbe
Zuschnitt wie Djangos `LOGGING`-Einstellung oder Laravels Channels, aber
strukturiert: ein Event trägt benannte Felder (`status=500`, `tenant=acme`)
statt eines formatierten Satzes, sodass ein Log-Aggregator danach filtern kann.

Ein per Scaffolder erzeugtes Projekt loggt bereits. Auf dieser Seite geht es um
das, was Sie einstellen: welches Level, welche Subsysteme, welches Format, wohin
die Ausgabe geht — und wie Sie den Verkehr eines Mandanten von dem eines anderen
unterscheiden.

> **Neu bei **Rustango**?** Das [Glossar](glossary.md) erklärt die Bausteine des
> Frameworks. Das Logging-Vokabular — *Level*, *Target*, *Span* — wird auf
> dieser Seite erklärt, wo es auftaucht.

> **Quelle:** `rustango::logging` (`setup`, `setup_for_env`, `Setup`,
> `Rotation`, `DEFAULT_FILTER`) — das Modul ist ungegated, aber jeder Installer
> braucht das Feature `runtime`. `Setup::from_settings` braucht zusätzlich
> `config`. `rustango::access_log` und `rustango::tenant_log` brauchen `admin`
> **oder** `tenancy`; `rustango::tracing_layer` braucht `admin`.
>
> **Ausführbare Version:** die Settings und Defaults hier sind abgesichert durch
> [`logging_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_doc.rs)
> (`cargo test -p rustango --test logging_doc`), die Datei-Senke durch
> [`logging_file_appender_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_file_appender_live.rs)
> und das Mandantenfeld des Access-Logs durch
> [`access_log_tenant_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/access_log_tenant_sqlite_live.rs).

## Inhaltsverzeichnis

- [Was Sie bereits haben](#was-sie-bereits-haben)
- [Level und der Filter, der sie auswählt](#level-und-der-filter-der-sie-auswählt)
- [Targets: das Subsystem benennen](#targets-das-subsystem-benennen)
- [Ein Format wählen](#ein-format-wählen)
- [Logging aus den Settings konfigurieren](#logging-aus-den-settings-konfigurieren)
- [In eine Datei schreiben](#in-eine-datei-schreiben)
- [Das Access-Log](#das-access-log)
- [Welcher Mandant war das?](#welcher-mandant-war-das)
- [Request-Spans und OpenTelemetry](#request-spans-und-opentelemetry)
- [Logging in Tests](#logging-in-tests)
- [Es kommt nichts heraus](#es-kommt-nichts-heraus)

---

## Was Sie bereits haben

`#[rustango::main]` installiert einen Subscriber, bevor Ihr Code läuft. Ein per
Scaffolder erzeugtes Projekt bekommt das geschenkt — deshalb gibt `cargo run`
ohne jeden Setup-Aufruf Logs aus:

```rust,ignore
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Ein `tracing_subscriber::fmt`-Subscriber ist hier bereits installiert.
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

Er verwendet `RUST_LOG`, wenn gesetzt, sonst `info,sqlx=warn` — den Wert von
`rustango::logging::DEFAULT_FILTER`. Dieser Default ist Absicht: sqlx loggt
jedes Statement auf `info`, ein ungefiltertes `info` begräbt Ihre eigenen Events
also unter SQL.

Um mehr als das Level einzustellen, installieren Sie den Subscriber selbst.
Jeder Installer ist idempotent (darunter `try_init`), ein zusätzlicher Aufruf
ist also ein No-op und kein Panic:

```rust,ignore
fn main() {
    rustango::logging::setup();   // pretty, Env-Filter, "info,sqlx=warn"
    // ...
}
```

## Level und der Filter, der sie auswählt

Fünf Level, das leiseste zuletzt: `error`, `warn`, `info`, `debug`, `trace`. Ein
Filter benennt die maximale Ausführlichkeit — global oder pro Modul:

```sh
RUST_LOG=info                                  # alles ab info
RUST_LOG=debug,sqlx=warn,hyper=warn            # debug für Sie, ruhige Deps
RUST_LOG=warn,rustango::tenancy=debug          # ein Subsystem, laut
RUST_LOG=rustango=info                         # nur das Framework
```

Setzen Sie den Fallback für den Fall, dass `RUST_LOG` fehlt — das ist in den
meisten Produktionsumgebungen so, wo der Filter in die Konfiguration gehört und
nicht in die Umgebung:

```rust,ignore
rustango::logging::Setup::new()
    .with_default_env_filter("info,sqlx=warn,hyper=warn")
    .install();
```

`RUST_LOG` gewinnt immer gegen diesen Fallback. Einen Filter, den die Umgebung
nicht überschreiben kann, gibt es bewusst nicht — wer einen laufenden Vorfall
debuggt, soll dafür keinen Build ausliefern müssen.

**Welches Level wo zu erwarten ist.** `warn`-Events des Frameworks haben meist
dieselbe Form, und die ist einen Alarm wert: *rustango hat etwas anderes getan
als das, worum Sie gebeten haben*. Ein unbekanntes `format` in den Settings,
eine Index-Klausel, die das Backend nicht ausdrücken kann und die deshalb
entfällt, eine Admin-Route, die kollidierte und übersprungen wurde, ein
`update`-Aufruf mit leerer Feldliste — all das läuft weiter, und das `warn` ist
das einzige Zeichen dafür.

## Targets: das Subsystem benennen

Jedes Event trägt ein **Target**, und genau darauf passt
`RUST_LOG=<target>=<level>`. Events des Frameworks liegen unter der Wurzel
`rustango::`, `RUST_LOG=rustango=warn` erreicht also alle:

| Target | Wofür |
|---|---|
| `rustango::admin` | Admin-Routing und -Registrierung |
| `rustango::admin::audit` | Audit-Log-Schreibvorgänge |
| `rustango::admin::sso` | Admin-SSO |
| `rustango::cache` | Cache-Backends |
| `rustango::cache_page` | Page-Cache-Middleware |
| `rustango::cors` | CORS-Entscheidungen |
| `rustango::email` | Mail-Versand |
| `rustango::email::smtp` | SMTP-Transport |
| `rustango::error` | Die Ursache eines 5xx, die der Response-Body zurückhält |
| `rustango::humanize` | Humanize-Filter |
| `rustango::jobs` | Hintergrund-Job-Queues |
| `rustango::logging` | Warnungen dieses Subsystems selbst |
| `rustango::manage` | `manage`-Verben |
| `rustango::media::auth` | Ablehnungen der Media-Router-Autorisierung |
| `rustango::messages` | Flash-Messages |
| `rustango::migrate` | Migrations-Runner |
| `rustango::rate_limit` | Rate-Limiting |
| `rustango::request_timeout` | Request-Timeout |
| `rustango::scheduler` | Cron / geplante Tasks |
| `rustango::server` | Serverstart und -shutdown |
| `rustango::shutdown` | Signalbehandlung und Shutdown-Hooks |
| `rustango::sql` | Query-Ausführung |
| `rustango::sql::lock` | Row-Lock-Klauseln |
| `rustango::template_views` | Template-basierte Views |
| `rustango::tenancy` | Mandantenfähigkeit, allgemein |
| `rustango::tenancy::admin` | Mandanten-Admin |
| `rustango::tenancy::migrate_run` | Migrationen pro Mandant |
| `rustango::tenancy::operator_console` | Operator-Konsole |
| `rustango::tenancy::pools` | Lebenszyklus der Mandanten-Pools |
| `rustango::tenancy::provision` | Mandanten-Provisionierung |
| `rustango::tenancy::provision_webhook` | Provisionierungs-Webhooks |
| `rustango::tenancy::resolver` | Mandanten-Auflösung |
| `rustango::tenancy::sso` | Mandanten-SSO |
| `rustango::tenancy::sweep` | Aufbewahrungs-Sweeps |

Das sind die Targets, die das Framework explizit benennt. Events ohne eigenes
Target erben ihren Modulpfad, was dieselbe Form ergibt —
`rustango::access_log` und `rustango::tracing_layer` sind genauso erreichbar wie
die Zeilen oben.

> **Ein Target ist ein String, kein Pfad.** `target: "crate::cache"` kompiliert
> anstandslos und liegt dann in einem Namensraum, auf den kein Filter passt.
> Achtundvierzig Aufrufstellen waren so abgedriftet, bevor
> [`tracing_targets.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/tracing_targets.rs)
> anfing, den Build daran scheitern zu lassen. Wenn Sie in eigenem Code Targets
> vergeben, nehmen Sie den echten Namen Ihres Crates.

## Ein Format wählen

| Format | Wofür | Wie |
|---|---|---|
| `pretty` | Entwicklung — farbig, mehrzeilig, lesbar | Default |
| `json` | Produktion — ein Objekt pro Event, für Loki / CloudWatch / Datadog | `.json()` |

```rust,ignore
rustango::logging::Setup::new()
    .json()
    .with_default_env_filter("info")
    .install();
```

Oder überlassen Sie die Wahl der Stufe. `setup_for_env()` liest `RUSTANGO_ENV`
und nimmt JSON, wenn der Wert `prod` oder `production` ist, sonst pretty:

```rust,ignore
rustango::logging::setup_for_env();
```

Zwei Darstellungs-Schalter lohnen sich: `.with_line_numbers()` ergänzt
Quellpositionen (nützlich in der Entwicklung, laut in Produktion), und
`.without_targets()` blendet die Target-Spalte aus — tun Sie das nur, wenn Sie
das Filtern danach aufgegeben haben.

> Jeder Format-Wert erreicht seinen eigenen Formatter. `compact` wurde früher
> akzeptiert und dann wie der Default gerendert — das ist behoben (#1480).

## Logging aus den Settings konfigurieren

Alles oben hat ein TOML-Äquivalent, damit ein Deployment sein Logging ohne
Rebuild ändern kann. Der Abschnitt heißt `[logging]`:

```toml
# config/dev_settings.toml
[logging]
level             = "info,sqlx=warn"
format            = "pretty"
with_line_numbers = true
```

```toml
# config/prod_settings.toml
[logging]
level         = "info"
format        = "json"
file_dir      = "/var/log/myapp"
file_prefix   = "app"
file_rotation = "daily"
```

| Schlüssel | Typ | Default | Hinweise |
|---|---|---|---|
| `level` | String | `info,sqlx=warn` | `RUST_LOG`-Syntax. Nur genutzt, wenn `RUST_LOG` fehlt |
| `format` | String | `full` | `full` / `pretty` / `compact` / `json`. Unbekannte Werte fallen mit einem `warn` auf `full` zurück |
| `color` | String | `auto` | `auto` / `always` / `never`. `auto` färbt nur ein Terminal und respektiert `NO_COLOR` |
| `access_log` | bool | `true` | Eine Zeile pro Request, plus die Span, die `tenant` in Handler-Events trägt |
| `with_thread_ids` | bool | `false` | Thread-ID an jedem Event |
| `with_line_numbers` | bool | `false` | Quellzeile an jedem Event |
| `without_targets` | bool | `false` | Target-Spalte ausblenden |
| `file_dir` | String | nicht gesetzt | Setzen aktiviert die Datei-Senke |
| `file_prefix` | String | `app` | Dateinamens-Stamm |
| `file_rotation` | String | `daily` | `daily` / `hourly` / `minutely` / `never`. Unbekannte Werte fallen mit einem `warn` auf `daily` zurück |
| `file_only` | bool | `false` | stdout weglassen. No-op ohne `file_dir` |

Angewendet wird das mit einem Aufruf auf der `Cli`:

```rust,ignore
rustango::manage::Cli::new()
    .with_settings_from_env()
    .with_logging()               // installiert aus Settings.logging
    .api(urls::api())
    .run()
    .await
```

`with_logging()` ist Opt-in — standardmäßig aus, damit ein Projekt, das selbst
`logging::setup()` aufruft, keinen zweiten Installer bekommt. Die Reihenfolge in
der Kette spielt keine Rolle: installiert wird bei `run()`, gegen die
endgültigen Settings.

> **`#[rustango::main]` gewinnt dagegen.** Das Makro installiert einen
> Subscriber, noch bevor die Runtime gebaut ist, und jeder Installer nutzt
> `try_init` — der erste gewinnt, spätere werden stillschweigend verworfen. Ein
> `main`, das das Makro behält *und* `with_logging()` aufruft, bekommt also die
> Defaults des Makros: Ihr `[logging]`-Abschnitt wird gelesen und bleibt dann
> wirkungslos, ohne jeden Hinweis. Für Settings-gesteuertes Logging tauschen Sie
> `#[rustango::main]` gegen `#[tokio::main]` — das ist die eine Änderung, die
> ein generiertes Projekt braucht. Verfolgt in
> [#1465](https://github.com/ujeenet/rustango/issues/1465).

Jeder Schlüssel lässt sich pro Deployment per Umgebungsvariable überschreiben,
mit Abschnitt und Schlüssel als Pfadsegmenten:

```sh
RUSTANGO__LOGGING__LEVEL=debug
RUSTANGO__LOGGING__FORMAT=json
```

Wenn Sie den Subscriber selbst aus demselben Abschnitt bauen wollen — etwa weil
Sie den zurückgegebenen Guard brauchen, siehe unten — nehmen Sie
`Setup::from_settings`:

```rust,ignore
let settings = rustango::config::Settings::load_from_env()?;
let _guard = rustango::logging::Setup::from_settings(&settings.logging).install();
```

## In eine Datei schreiben

Logs gehen nach stdout, solange Sie nichts anderes verlangen — für einen
Container ist das richtig. Wenn Sie Dateien brauchen, schreibt `with_file`
zusätzlich in einen rollierenden Appender:

```rust,ignore
use rustango::logging::{Rotation, Setup};

let _guard = Setup::new()
    .json()
    .with_file("/var/log/myapp", "app", Rotation::Daily)
    .install();
```

Dateien landen bei `Daily` unter `{dir}/{prefix}.YYYY-MM-DD`, das Verzeichnis
wird beim ersten Schreiben angelegt. `Rotation` kennt `Daily`, `Hourly`,
`Minutely` und `Never`. `.file_only()` entfernt die stdout-Schicht — für einen
Headless-Worker oder einen daemonisierten Prozess, wo ohnehin niemand stdout
liest.

> **Halten Sie den Guard am Leben.** `install()` liefert
> `Option<tracing_appender::non_blocking::WorkerGuard>` — `Some`, wenn eine
> Datei-Senke konfiguriert ist. Der Datei-Writer ist nicht blockierend, eine
> hängende Platte kann die Request-Verarbeitung also nicht anhalten; der Preis
> ist, dass gepufferte Events erst beim Droppen des Guards geflusht werden.
> Binden Sie ihn für die Lebensdauer des Prozesses (ein `static`, ein
> `OnceLock`, oder ein `let` in `main`, das alles überlebt).
> `let _ = ...install();` droppt ihn sofort, und Sie verlieren Schreibvorgänge.
> `Cli::with_logging()` hält ihn für Sie.

## Das Access-Log

Ein Event pro abgeschlossenem Request, mit den Feldern, nach denen im Betrieb
gesucht wird:

```rust,ignore
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};

let app = router.access_log(AccessLogLayer::default());
```

```text
INFO rustango::access_log: http.request.method=GET url.path=/api/posts url.query=page=2 http.response.status_code=200 duration_ms=12 client.address=192.0.2.1 tenant=acme
```

Das Level trägt Bedeutung, Alarmierung kann also daran ansetzen:

| Bedingung | Level |
|---|---|
| Normale Antwort | `info` |
| Status >= 400 | `warn` |
| Langsamer als `slow_threshold_ms` (Default 1000) | `warn`, Meldung `slow request` |

Einstellen:

```rust,ignore
AccessLogLayer::default()
    .errors_only()               // 2xx/3xx ganz überspringen
    .slow_threshold_ms(250)      // was als langsam gilt
    .without_ip()                // Client-IP weglassen
    .trust_proxy_headers(true)   // X-Forwarded-For, nur hinter vertrauenswürdigem Proxy
```

Query-Parameter, die Zugangsdaten tragen, werden vor dem Schreiben mit
`[redacted]` maskiert — `password`, `passwd`, `token`, `secret`, `api_key`,
`apikey`, `access_token`, `refresh_token`, `signature`, `auth`. Erweitern mit
`.redact_additional("session_id")`, oder die ganze Liste ersetzen mit
`.redact(vec![...])`. Das betrifft **nur Query-Strings**; für den Rest siehe
[security.md](security.md#secrets-aus-deinen-logs-heraushalten).

## Welcher Mandant war das?

`tenant` benennt den Mandanten, zu dem der Request aufgelöst wurde, und ist `-`,
wenn keiner aufgelöst wurde — ein Request auf die Apex-Domain oder die
Operator-Konsole, oder eine Anwendung ohne Mandantenfähigkeit. Das Feld ist nie
leer, „kein Mandant“ liest sich also anders als ein Feld, das verloren ging.

Nichts zu verdrahten: `ChainResolver` veröffentlicht die Identität, sobald er
eine auflöst, und das Access-Log liest sie zurück. Der Mechanismus ist
[`rustango::tenant_log`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/src/tenant_log.rs),
ein Slot pro Request — nötig, weil die Mandantenidentität in einem Extractor
lebt, also innerhalb des Handlers und unterhalb der Middleware, die sie loggen
möchte.

Der Slug wird vom Betreiber vergeben und ist oft der Kundenname. Wenn Logs Ihre
Infrastruktur verlassen, labeln Sie stattdessen per ID:

```rust,ignore
use rustango::access_log::{AccessLogLayer, TenantField};

AccessLogLayer::default().tenant_field(TenantField::Id)     // tenant=42
AccessLogLayer::default().tenant_field(TenantField::Both)   // tenant=acme#42
AccessLogLayer::default().tenant_field(TenantField::Off)    // tenant=-
```

> **Nur der Request-Pfad.** Ein Hintergrund-Job läuft außerhalb des Requests,
> der ihn eingereiht hat, und hat keinen Mandanten zu loggen — der Slot gilt pro
> Task, und `tokio::spawn` erbt ihn nicht. Verfolgt in
> [#1229](https://github.com/ujeenet/rustango/issues/1229) /
> [#1223](https://github.com/ujeenet/rustango/issues/1223).

## Request-Spans und OpenTelemetry

`TracingLayer` legt um jeden Request einen Span nach den
[semantischen Konventionen von OpenTelemetry v1.30](https://opentelemetry.io/docs/specs/semconv/http/http-spans/),
sodass ein Collector keine Attribut-Umbenennungsregeln braucht:

```rust,ignore
use rustango::tracing_layer::TracingLayer;
use tower::ServiceBuilder;

let app = ServiceBuilder::new().layer(TracingLayer::new()).service(router);
```

Der Span trägt `http.request.method`, `url.path`, `url.query`,
`network.protocol.version`, `user_agent.original`,
`http.response.status_code`, `http.response.body.size`, `duration_ms` sowie
`tenant` / `org_id`, sobald ein Mandant aufgelöst ist. Kommt der Request mit
einem W3C-`traceparent`-Header, werden zusätzlich `trace_id`, `parent_span_id`
und `trace_flags` aufgezeichnet — genau das greift eine
`tracing-opentelemetry`-Schicht ab, um sich in den Trace einzuklinken.

Schon wegen des Mandantenfelds lohnt sich diese Schicht: weil die Felder am
*Span* hängen, trägt jedes während des Requests emittierte Event — auch die des
ORM — sie im Span-Kontext, ohne dass irgendein Subsystem wissen muss, was ein
Mandant ist. Sie ist **nicht** standardmäßig installiert; weder
`server::Builder` noch `Cli` noch der Scaffolder fügen sie hinzu.

## Logging in Tests

Installer nutzen `try_init`, ein `logging::setup()` aus einem Test ist also auch
dann unbedenklich, wenn ein anderer Test bereits installiert hat. Um Ausgabe zu
prüfen, nehmen Sie lieber einen begrenzten Subscriber als einen globalen:

```rust,ignore
let subscriber = tracing_subscriber::fmt()
    .with_writer(make_writer)
    .with_max_level(tracing::Level::INFO)
    .finish();
let _guard = tracing::subscriber::set_default(subscriber);
```

`set_default` ist thread-lokal und liefert einen Guard, der den vorherigen
Subscriber wiederherstellt — parallele Tests geraten sich so nicht in die Quere.
`#[tokio::test]` läuft auf einer Current-Thread-Runtime, was das gesamte Future
auf dem Thread hält, den der Guard abdeckt.

## Es kommt nichts heraus

- **`RUST_LOG` gesetzt, trotzdem still.** Der Filter wird einmal gelesen, bei
  der Installation. Die Variable nach dem `setup()` zu setzen ändert nichts.
- **Das eigene Crate ist bei `info` still.** `RUST_LOG=info` gilt für jedes
  Target; wer `RUST_LOG=rustango=info` setzt, hat den eigenen Code
  herausgefiltert. Beides benennen: `RUST_LOG=info,rustango=warn`.
- **Ein Filter auf `crate::irgendwas` passt auf nichts.** Targets sind Strings,
  siehe den Hinweis unter [Targets](#targets-das-subsystem-benennen).
- **Datei nach einem Absturz leer.** Der Guard aus `install()` wurde gedroppt,
  oder der Prozess starb vor dem Flush des Appenders. Siehe
  [In eine Datei schreiben](#in-eine-datei-schreiben).
- **Zwei Subscriber, der zweite ignoriert.** `try_init` heißt: der erste
  gewinnt, stillschweigend. Wer `logging::setup()` *und* `Cli::with_logging()`
  aufruft, verliert den aus den Settings.
