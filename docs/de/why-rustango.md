# Warum Rustango: ein ehrlicher Vergleich

Rustango ist ein Web-Framework für Rust, das alles Nötige mitbringt: Du deklarierst ein Modell einmal und bekommst daraus ein ORM, Migrationen, ein Admin, eine REST-API und Mandantenfähigkeit. Diese Seite zeigt, was es gut kann, wie es im Vergleich zu den üblichen Frameworks dasteht, die Benchmark-Zahlen und wann du besser etwas anderes nimmst.

## Kurz gesagt

- **Nimm Rustango**, wenn du die Produktivität eines Frameworks mit allem Drum und Dran willst, dazu Geschwindigkeit, wenig Speicherverbrauch und Prüfungen beim Kompilieren durch Rust, und ein einziges Binary zum Ausliefern.
- **Nimm etwas anderes**, wenn dein Team kein Rust schreibt, wenn du heute schon ein riesiges Paket-Ökosystem brauchst oder ein Framework mit zehn Jahren Produktionsgeschichte willst.

## Was dabei ist

| Bereich | Rustango |
|---|---|
| ORM | Ein Modell, korrektes SQL für PostgreSQL, MySQL 8+ und SQLite; Relationen, Aggregate, Soft Delete |
| Migrationen | Erzeugt durch Vergleich deiner Modelle mit dem letzten Snapshot, dann angewendet |
| Admin | Ein automatisch erzeugtes Admin mit Suche, Filtern, Audit-Log und Berechtigungen |
| APIs | ViewSets und Serializer für REST, daraus erzeugtes OpenAPI |
| Anmeldung | Sessions, JWT, OAuth2 / OIDC, Single Sign-On, API-Keys, HMAC, TOTP, Passkeys |
| Mandanten | Ein Schema pro Mandant (PostgreSQL) oder eine Datenbank pro Mandant (alle drei Datenbanken) |
| Hintergrundarbeit | Jobs, geplante Aufgaben, Signale, E-Mail, Dateispeicher |
| Sicherheit | CSRF, Rate-Limits, Login-Drosselung, Security-Header, CORS, erlaubte Hosts |
| KI-Agenten | Ein eingebauter MCP-Server |
| Werkzeuge | Ein Projekt-Generator, Code-Generatoren und eine einzige `manage`-Kommandozeile |

Alles außer dem ORM ist ein Cargo-Feature, sodass eine reine JSON-API ganz ohne Admin und Templates kompiliert.

## Der Vergleich

| | Rustango | Django | Rails | Laravel | Loco | axum + sqlx |
|---|---|---|---|---|---|---|
| Sprache | Rust | Python | Ruby | PHP | Rust | Rust |
| ORM | Eingebaut | Eingebaut | Eingebaut | Eingebaut | SeaORM | Abfragen von Hand |
| Migrationen aus Modellen | Ja, per Vergleich | Ja, per Vergleich | Mit Generatoren geschrieben | Von Hand geschrieben | Mit Generatoren geschrieben | Von Hand |
| Admin | Eingebaut | Eingebaut | Gems | Pakete (Nova, Filament) | Nein | Nein |
| REST-API-Schicht | Eingebaut, mit OpenAPI | Paket (DRF) | Eingebaut (API-Modus) | Eingebaut | Eingebaut | Von Hand |
| Mandanten | Eingebaut | Pakete | Gems | Pakete | Nein | Von Hand |
| Hintergrund-Jobs | Eingebaut | Tasks-API; Worker über ein Paket | Eingebaut | Eingebaut | Eingebaut | Von Hand |
| Fehler in Modellen und Abfragen | Beim Kompilieren erkannt | Zur Laufzeit | Zur Laufzeit | Zur Laufzeit | Beim Kompilieren erkannt | Beim Kompilieren erkannt |
| Auslieferung | Ein Binary | App + Python-Laufzeit | App + Ruby-Laufzeit | App + PHP-Laufzeit | Ein Binary | Ein Binary |
| Ökosystem | Klein und neu | Riesig | Riesig | Riesig | Wachsend | Groß (Rust-Crates) |

Ehrlich gelesen: Bei den Funktionen spielt Rustango in derselben Klasse wie Django und Rails, nicht darüber. Es gewinnt bei Geschwindigkeit, Speicher, Sicherheit beim Kompilieren und Auslieferung, und gegenüber den anderen Rust-Optionen bei allem, was mitgeliefert wird.

## Benchmarks

Jede App lieferte denselben Blog aus: 1 000 Beiträge, 10 000 Kommentare, dasselbe HTML Byte für Byte geprüft, 4 CPUs, 16 Datenbankverbindungen, 50 gleichzeitige Verbindungen. Die vollständige Methode, die Grenzen und die Profile stehen im [Benchmark-Bericht](benchmarks.md). Die Runden wurden getrennt gemessen, deshalb bleibt jede Tabelle innerhalb einer Runde.

**Runde 2: kompilierte Apps** (Anfragen pro Sekunde, höher ist besser):

| App | Index | Detail | Tag | Index, gecacht | Speicher unter Last |
|---|--:|--:|--:|--:|--:|
| Rustango · actix | **17 401** | **27 292** | 7 056 | **231 975** | **6.3 MiB** |
| Rustango · axum | 14 686 | 25 788 | **7 090** | 174 501 | 6.9 MiB |
| axum + sqlx | 14 140 | 22 842 | 6 467 | 198 669 | 7.6 MiB |
| actix + sqlx | 14 408 | 21 630 | 6 628 | 226 887 | 7.2 MiB |
| Rocket | 13 316 | 24 561 | 6 836 | 138 224 | 8.7 MiB |
| Loco | 10 290 | 15 635 | 5 604 | 153 243 | 12.0 MiB |
| Go (net/http + pgx) | 8 741 | 22 607 | 6 386 | 91 839 | 22.8 MiB |

Rustango ist mit seinem ORM auf ungecachten Seiten der schnellste der acht, vor den von Hand geschriebenen Abfragen auf reinem sqlx.

**Runde 1: interpretierte Laufzeiten**, mit Rustango aus demselben Lauf (Stand 0.60.6, vor den Korrekturen aus Runde 2):

| App | Index ungecacht, req/s | Index gecacht, req/s | Speicher unter Last |
|---|--:|--:|--:|
| Rustango · actix | 6 341 | 200 167 | ~7 MiB |
| Laravel · Octane | 1 098 | 5 884 | 241 MiB |
| Python · gunicorn | 697 | 11 012 | 224 MiB |
| WordPress | 599 | 21 285 | 142 MiB |
| Laravel · php-fpm | 518 | 3 326 | 89 MiB |

## Die ehrlichen Einschränkungen

- **Go ist bei reiner Rechenarbeit schneller**, um 8–17 % im Rechentest, und reines axum mit sqlx liefert den gecachten Index schneller aus als Rustango auf axum (198 669 gegenüber 174 501 req/s).
- **Es ist jung.** Version 0.x, mit einer großen Oberfläche, die sich noch setzt. Die letzten Versionen haben eher ergänzt als entfernt, aber lies beim Aktualisieren das Changelog.
- **Das Ökosystem ist klein.** Django, Rails und Laravel haben für fast alles ein Paket; mit Rustango schreibst du es öfter selbst oder greifst zu einem allgemeinen Rust-Crate.
- **Rust hat eine Lernkurve**, und die Kompilierzeiten sind real: Minuten für den ersten Build, Sekunden für eine Änderung.
- **Ein Schema pro Mandant braucht PostgreSQL.** Auf MySQL und SQLite bekommt jeder Mandant eine eigene Datenbank.
- **Die Benchmarks haben Grenzen.** Ein Laptop, eine Docker-VM, eine leselastige, serverseitig gerenderte Last. Keine Schreibzugriffe, keine Anmeldeabläufe, keine schwere Geschäftslogik. Deine Zahlen werden anders sein.

## Für wen es passt

- Teams, die die Arbeitsweise von Django oder Rails mögen, aber die Geschwindigkeit, Sicherheit und Sparsamkeit von Rust wollen.
- Rust-Teams, die es leid sind, Anmeldung, Admin, Migrationen und API in jedem Projekt neu zusammenzubauen.
- Mandantenfähige SaaS-Produkte, die die Mandanten im Framework haben wollen statt nachträglich angeflanscht.

## Ausprobieren

```bash
cargo install cargo-rustango
cargo rustango new myapp
cd myapp && cargo run
```

Die [Einführung](getting-started.md) baut Schritt für Schritt einen Blog mit Admin, JSON-API und Anmeldung. Für Content-Websites baut [Rustango-CMS](https://cms.rustango.com) darauf auf.
