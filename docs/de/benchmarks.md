# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

Derselbe Blog auf zwölf Laufzeiten: **Go** (`net/http` der Standardbibliothek +
`pgx`), **Rustango** auf axum und auf actix-web, reines **axum** und
**actix-web** mit rohem `sqlx`, ein **Python**-Framework auf gunicorn,
Hypercorn, Uvicorn und Granian, **Laravel** auf php-fpm und auf Octane, sowie
**WordPress**. Jede Anwendung rendert dasselbe HTML aus denselben Daten.

Gemessen am **13. Oktober 2026**: rustango release 0.60.6 in den Haupttabellen, rustango
0.60.7 in [einer eigenen Tabelle](#rustango-0607). Die vier Python-Server wurden
am 14. Oktober 2026 gemessen. Ein Harness, eine Maschine, dieselben Regeln für
jede Anwendung.

## Zusammenfassung

- **Nicht gecachte Seiten, kompilierte Anwendungen.** Reines Rust (axum + sqlx)
  ist beim Index 34 % schneller als Go, Go ist beim Detail 22 % schneller, die
  Tag-Seite ist ein Unentschieden.
- **Rustango 0.60.7** liefert 93–104 % von reinem axum + sqlx beim nicht
  gecachten Index und 94–100 % bei Detail, Tag und compute.
- **Gecachte Seiten.** Jede Rust-Anwendung liefert das 1,5- bis 2,2-Fache der
  Requests von Go, mit einem p99 um 0,45 ms (Go: 2,0–2,4 ms).
- **Interpretierte Laufzeiten** liegen bei nicht gecachten Seiten 4- bis 40-fach
  hinter den kompilierten Anwendungen. Bei gecachten Seiten ist WordPress unter
  ihnen am schnellsten (21k Req/s).
- **Python:** gunicorn (synchron) ist am schnellsten; asynchrone Views auf
  ASGI-Servern schlagen es nicht. Granian ist der schnellste ASGI-Server.
- **Speicher unter Last:** Rust-Anwendungen 7–8 MiB, Go 17–21 MiB,
  interpretierte Laufzeiten 89–293 MiB.

---

## Der Aufbau

Ein Blog mit **Autoren, Beiträgen, Tags (Viele-zu-Viele) und Kommentaren**:

| Route | Rendert | Gecacht? |
|---|---|---|
| `GET /` | die neuesten 20 Beiträge, jeder mit Autor, Tags, Kommentaranzahl | nein |
| `GET /cached` | dasselbe wie `/` | Redis, 60 s |
| `GET /post/{slug}` | Beitragsrumpf + Autor + Tags + jeder Kommentar | nein |
| `GET /post/{slug}/cached` | dasselbe wie Detail | Redis, 60 s |
| `GET /tag/{slug}` | die neuesten 20 Beiträge mit einem Tag | nein |
| `GET /compute` | Summe aller Primzahlen unter 20.000 (nur CPU, keine DB) | nein |

Der Seed ist deterministisch: 10 Autoren, 30 Tags, **1.000 Beiträge**, 2.600
Beitrag-Tag-Verknüpfungen und **10.000 Kommentare**. Relationen werden eifrig
geladen (kein N+1). `/compute` führt in jeder Sprache dieselbe
Probedivisions-Schleife aus.

### Die Anwendungen

| Anwendung | Server | Datenzugriff | Seiten-Cache |
|---|---|---|---|
| **Go** | stdlib `net/http` | `pgx`-Pool, rohes SQL | `go-redis` GET/SET |
| **rustango · axum** | axum 0.8 | rustango-ORM | `CachePageLayer` |
| **rustango · actix** | actix-web 4 | rustango-ORM | handgeschriebenes Redis GET/SET |
| **axum + sqlx** | axum 0.8 | rohes `sqlx`, SQL-Text von Go | handgeschriebenes Redis GET/SET |
| **actix + sqlx** | actix-web 4 | rohes `sqlx`, SQL-Text von Go | handgeschriebenes Redis GET/SET |
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI, synchrone Views) | ORM des Frameworks, eingebauter psycopg-Pool | Redis-Cache des Frameworks |
| **Python · Hypercorn** | Hypercorn, 4 Worker, uvloop (ASGI, asynchrone Views) | asynchrone ORM-API des Frameworks, derselbe Pool | `redis.asyncio`, ein GET pro Treffer |
| **Python · Uvicorn** | Uvicorn, 4 Worker, uvloop + httptools (ASGI, asynchrone Views) | dasselbe | dasselbe |
| **Python · Granian** | Granian, 4 Worker, uvloop (ASGI, asynchrone Views) | dasselbe | dasselbe |
| **Laravel · php-fpm** | nginx + php-fpm, 16 statische Kindprozesse | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 Worker | dasselbe | dasselbe |
| **WordPress** | nginx + php-fpm, 16 statische Kindprozesse | `WP_Query`, `get_the_tags`, `get_comments` auf **MySQL 8.4** | `advanced-cache.php`-Drop-in, phpredis |

Templates: Tera für die Rust-Anwendungen, `html/template` für Go, die eigene
Engine des Python-Frameworks, Blade für Laravel, ein kleines Theme für WordPress.

### Testregeln

- **CPU und Speicher.** Jede Anwendung bekommt 4 CPUs (Kerne 0–3) und 2 GB RAM.
  Go läuft mit `GOMAXPROCS=4`, Tokio und actix-web mit 4 Workern.
- **Datenbank-Pool.** 16 PostgreSQL-Verbindungen pro Anwendung (die
  Python-Anwendung: 4 pro Worker-Prozess, 16 insgesamt).
- **Abfragen.** Die kompilierten Anwendungen führen denselben Plan aus:
  3 Abfragen für den Index, 3 für Detail, 4 für die Tag-Seite. Rustango baut sie
  mit dem ORM; die reinen Rust-Anwendungen nutzen den SQL-Text von Go. Die
  interpretierten Anwendungen nutzen das eifrige Laden ihres ORM. WordPress führt
  11–13 Abfragen pro Seite aus.
- **Ausgabe.** Vor jeder Last wird jede Seite jeder Anwendung mit dem HTML von Go
  verglichen; bei einer Abweichung bricht der Durchlauf ab. Keine Kompression,
  daher sendet jede Anwendung dieselbe Indexseite von ~6,4 KB.
- **Produktionseinstellungen.** Debug aus. PHP: OPcache an, Zeitstempelprüfung
  aus, `php artisan optimize`. Python: Verbindungen aus dem Pool, Access-Logs
  aus.
- **Asynchrone Python-Anwendungen.** `async def`-Views auf der asynchronen
  ORM-API des Frameworks, ein asynchroner Middleware-Wrapper und ein
  gemeinsamer `redis.asyncio`-Pool mit 16 Verbindungen pro Prozess. Ein
  gecachter Treffer ist ein Redis-GET mit rohem HTML.
- **Andere Dienste.** PostgreSQL 16, MySQL 8.4 und Redis 7 auf den Kernen 4–7;
  der Lastgenerator [`oha`](https://github.com/hatoo/oha) 1.16.0 auf den Kernen
  8–11, im Docker-Netzwerk. 50 Verbindungen, Keep-Alive an.
- **Durchläufe.** Pro Endpunkt: 5 s Aufwärmen, dann 3 × 20 s; der Median zählt.
  Nur HTTP-200-Antworten zählen.
- **Builds.** rustc 1.99.0 (`opt-level=3`, Fat LTO, `codegen-units=1`,
  `panic=abort`), Go 1.25.14, PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3,
  WordPress 6.9.10, Uvicorn 0.54.0, Granian 2.8.3, uvloop 0.22.1,
  httptools 0.8.0.

Host: Apple M3 Pro, Docker-VM mit 12 CPUs und 16 GB.

---

## Kompilierte Laufzeiten

### Durchsatz: Requests pro Sekunde (höher ist besser)

Rustango-Spalten: 0.60.6.

| Endpunkt | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| Index, nicht gecacht | 9.360 | 8.485 | 6.341 | **12.505** | 9.963 |
| Index, gecacht | 96.214 | 164.942 | 200.167 | **214.097** | 197.252 |
| Detail, nicht gecacht | **22.675** | 17.634 | 13.883 | 18.578 | 14.697 |
| Detail, gecacht | 143.795 | 194.939 | 214.662 | **233.669** | 211.293 |
| Tag, nicht gecacht | **6.457** | 5.577 | 5.146 | 6.299 | 5.903 |
| compute | **16.588** | 14.733 | 15.137 | 15.085 | 14.741 |

### Latenz: p50 / p99 in ms (niedriger ist besser)

| Endpunkt | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| Index, nicht gecacht | 5,11 / 9,31 | 5,81 / 8,00 | 7,78 / 10,20 | **3,93 / 5,80** | 4,95 / 6,99 |
| Index, gecacht | 0,36 / 2,40 | 0,30 / 0,53 | 0,24 / 0,47 | **0,23 / 0,42** | 0,24 / 0,46 |
| Detail, nicht gecacht | **1,91** / 4,97 | 2,78 / 4,32 | 3,54 / 5,19 | 2,64 / **4,12** | 3,34 / 4,88 |
| Detail, gecacht | 0,24 / 2,01 | 0,25 / 0,47 | 0,22 / 0,46 | **0,21 / 0,40** | 0,22 / 0,47 |
| Tag, nicht gecacht | **7,53** / 11,73 | 8,83 / 11,99 | 9,59 / 12,65 | 7,82 / **10,42** | 8,36 / 11,28 |
| compute | **2,36** / 10,84 | 3,40 / 6,07 | 3,32 / **3,58** | 3,33 / 5,97 | 3,39 / 3,65 |

### Fußabdruck

| | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| Image (unkomprimiert) | **18,5 MB** | 147 MB | 146 MB | 143 MB | 143 MB |
| RAM, im Leerlauf | 5,1 MiB | 1,4 MiB | 1,7 MiB | **1,3 MiB** | 1,6 MiB |
| RAM, Index unter Last | 21,2 MiB | 7,5 MiB | 7,3 MiB | 7,3 MiB | **6,9 MiB** |
| CPU, Index unter Last (von 400 %) | 296 % | 393 % | 296 % | 328 % | 272 % |

Go wird auf `scratch` ausgeliefert; die Rust-Images sind `debian:bookworm-slim`
plus das Binary.

### Rustango 0.60.7

Gemessen in einem separaten Lauf, mit axum + sqlx und rustango release 0.60.6
daneben neu gemessen (Median Req/s):

| Endpunkt | axum + sqlx | rustango release 0.60.6 | rustango 0.60.7 | 0.60.7 vs axum |
|---|--:|--:|--:|--:|
| Index, nicht gecacht | 10.750 | 8.596 | **11.143** | 104 % |
| Detail, nicht gecacht | 17.907 | 17.024 | 16.875 | 94 % |
| Tag, nicht gecacht | 6.242 | 5.596 | 6.023 | 96 % |
| compute | 15.091 | 14.736 | 15.084 | 100 % |

Der nicht gecachte Index schwankte zwischen den Läufen; vergleicht man den
besten Lauf jeder Anwendung, erreicht Rustango 0.60.7 93 % von axum + sqlx.

---

## Interpretierte Laufzeiten

Die vier Python-Server stammen aus einem Lauf (14. Oktober 2026); Laravel und
WordPress vom 13. Oktober 2026.

### Python-Server

#### Durchsatz: Requests pro Sekunde (höher ist besser)

| Endpunkt | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| Index, nicht gecacht | **697** | 663 | 675 | 692 |
| Index, gecacht | **11.012** | 5.151 | 6.233 | 7.706 |
| Detail, nicht gecacht | **2.778** | 1.550 | 1.654 | 1.854 |
| Detail, gecacht | **11.848** | 5.085 | 6.215 | 7.810 |
| Tag, nicht gecacht | **1.411** | 906 | 948 | 1.020 |
| compute | **422** | 388 | 385 | 404 |

#### Latenz: p50 / p99 in ms (niedriger ist besser)

| Endpunkt | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| Index, nicht gecacht | **66,9** / 155,8 | 71,2 / 128,1 | 72,8 / **113,6** | 69,9 / 119,9 |
| Index, gecacht | **4,9 / 10,1** | 9,3 / 23,5 | 7,1 / 19,6 | 6,4 / 15,4 |
| Detail, nicht gecacht | **18,9 / 34,9** | 30,5 / 57,0 | 29,2 / 52,5 | 26,0 / 47,6 |
| Detail, gecacht | **3,8 / 8,5** | 9,7 / 24,5 | 7,7 / 21,1 | 6,2 / 15,1 |
| Tag, nicht gecacht | **27,9** / 89,0 | 52,3 / 95,0 | 52,4 / 84,4 | 49,0 / **80,3** |
| compute | **113,0** / 215,8 | 128,6 / 160,6 | 131,7 / **154,5** | 117,9 / 187,1 |

#### Fußabdruck

| | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| Image (unkomprimiert, gemeinsam) | 345 MB | 345 MB | 345 MB | 345 MB |
| RAM, Index unter Last | **224 MiB** | 255 MiB | 249 MiB | 293 MiB |
| CPU, Index unter Last (von 400 %) | 244 % | 323 % | 314 % | 303 % |

### PHP

#### Durchsatz: Requests pro Sekunde (höher ist besser)

| Endpunkt | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| Index, nicht gecacht | 518 | **1.098** | 599 |
| Index, gecacht | 3.326 | 5.884 | **21.285** |
| Detail, nicht gecacht | 572 | **1.712** | 687 |
| Detail, gecacht | 3.013 | 5.836 | **21.591** |
| Tag, nicht gecacht | 490 | **1.102** | 647 |
| compute | **1.611** | 1.521 | 732 |

#### Latenz: p50 / p99 in ms (niedriger ist besser)

| Endpunkt | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| Index, nicht gecacht | 95,5 / 122,6 | **44,1** / 110,1 | 78,1 / **103,9** |
| Index, gecacht | 14,6 / 22,4 | 7,4 / 57,7 | **2,3 / 3,5** |
| Detail, nicht gecacht | 86,5 / 113,4 | **27,9** / 96,2 | 70,5 / **99,0** |
| Detail, gecacht | 16,4 / 21,6 | 7,4 / 58,2 | **2,3 / 3,6** |
| Tag, nicht gecacht | 100,8 / 130,1 | **44,0** / 110,9 | 75,9 / **97,0** |
| compute | **31,3 / 38,7** | 32,7 / 78,5 | 67,0 / 89,0 |

#### Fußabdruck

| | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| Image (unkomprimiert) | **844 MB** | 895 MB | 1,01 GB |
| RAM, im Leerlauf | **54 MiB** | 219 MiB | 61 MiB |
| RAM, Index unter Last | **89 MiB** | 241 MiB | 142 MiB |
| CPU, Index unter Last (von 400 %) | 380 % | 337 % | 405 % |

Einer der drei Durchläufe von WordPress beim nicht gecachten Index lieferte
14.253 „Datenbankverbindung"-Fehler in 7 s; der Median stammt aus einem sauberen
Durchlauf.

---

## Warum: was schneller ist, und wo

Nur gemessene Fakten; wo eine Ursache nicht profiliert wurde, steht das dabei.

- **Tag-Seite: die Datenbank entscheidet.** PostgreSQL nutzte auf dieser Route
  3,65 seiner 4 Kerne, daher liegen alle kompilierten Anwendungen innerhalb von
  20 % voneinander.
- **Nicht gecachter Index: reines Rust führt vor Go um 34 %.** Dieselben drei
  Abfragen, dieselben Zeilen. Auf der Go-Seite nicht profiliert.
- **Nicht gecachtes Detail: Go führt vor reinem Rust um 22 %.** Die Seite rendert
  einen Beitrag mit allen seinen Kommentaren. Nicht profiliert.
- **Gecachte Seiten: Rust 1,5- bis 2,2-fach Go, p99 ~0,45 ms vs 2,0–2,4 ms.**
  Ein Treffer ist ein Redis-Lesezugriff und eine Antwort; keine Datenbank, kein
  Template. Die Ursache für den breiteren Ausläufer von Go wurde nicht
  profiliert.
- **Compute: Go liefert 10 % mehr Requests, Rust hat einen kürzeren Ausläufer**
  (p99 3,6–6,1 ms vs 10,8 ms) bei derselben Probedivisions-Schleife.
- **Speicher: Rust 7–8 MiB, Go 17–21 MiB unter Last.** Die Laufzeit von Go hält
  einen Heap mit Garbage Collection; die Rust-Binaries geben Speicher sofort
  wieder frei.
- **Rustango vs reines axum + sqlx.** In 0.60.6 gingen 46 % der CPU von Rustango
  in das Dekodieren der Abfrageergebnisse, indem pro Wert etwa zehn Typen
  probiert wurden. 0.60.7 dekodiert jede Spalte einmal nach ihrem Typ (4,9 % der
  CPU, rohes sqlx: 5,0 %), und erreicht damit das Tempo von reinem sqlx. Der Rest
  der CPU ist in beiden Anwendungen gleich: Tera-Rendering (~26 %) und die
  Verbindungsprüfung des Pools (~18 %).
- **actix-web vs axum.** Mit einem gemeinsamen sqlx-Pool nutzen die
  actix-Anwendungen nur 272–296 % von 4 CPUs und liegen bei nicht gecachten
  Seiten hinter axum. Ursache nicht gefunden
  ([#2660](https://github.com/ujeenet/rustango/issues/2660)).
- **Gecachte Seiten, interpretiert.** WordPress antwortet aus Redis in
  `advanced-cache.php`, bevor WordPress selbst lädt (21k Req/s). Die
  Python-Anwendungen und Laravel durchlaufen zuerst ihr Framework und ihre
  Middleware und lesen dann den Cache (Python 5–12k, Laravel 3–6k).
- **Laravel: Octane 2- bis 3-fach php-fpm bei nicht gecachten Seiten.** php-fpm
  startet das Framework bei jedem Request; Octane hält es im Speicher.
- **Python: asynchrone Views schlagen gunicorn nicht.** gunicorn liefert bei
  gecachten Seiten das 1,4- bis 2,3-Fache der ASGI-Server und bei nicht
  gecachtem Detail und Tag das 1,4- bis 1,8-Fache; beim nicht gecachten Index
  sind alle vier gleich (663–697). Aus dem Code des Frameworks: das asynchrone
  ORM führt das synchrone ORM in einem Thread aus (`sync_to_async`), es gibt
  keinen asynchronen Datenbank-Pool, und der ASGI-Handler erzeugt pro Request
  einen Thread-Executor plus etwa 3 Thread-Wechsel (Request-Signale, Schließen
  der Antwort). Der asynchrone Cache-Pfad macht weniger Cache-Arbeit als der von
  gunicorn (ein Redis-GET mit rohem HTML gegenüber zwei GETs und einem
  Unpickle) und ist trotzdem langsamer. Der Anteil jeder Ursache wurde nicht
  profiliert.
- **ASGI-Server: Granian > Uvicorn > Hypercorn.** Gecachter Index 7.706 /
  6.233 / 5.151 Req/s, nicht gecachtes Detail 1.854 / 1.654 / 1.550. Nicht
  profiliert.
- **Compute, interpretiert.** PHP 8.3 mit OPcache führt die Integer-Schleife etwa
  4-mal schneller aus als CPython.
- **WordPress nicht gecacht** führt 11–13 Abfragen pro Seite auf MySQL aus,
  gegenüber 3–4 auf PostgreSQL bei den anderen Anwendungen.

---

## CPU-Profil von Rustango

`perf`-Profil von rustango release 0.60.6 unter Last beim nicht gecachten Index, mit dem
Status in 0.60.7:

| Kosten | CPU-Anteil | Ticket | Status |
|---|---|---|---|
| Jeden Wert dekodieren, indem etwa zehn Typen probiert werden | 46 % | [#2655](https://github.com/ujeenet/rustango/issues/2655) | **Behoben in 0.60.7** (jetzt 4,9 %) |
| `CachePageLayer` speichert Seiten als JSON + base64 | ~21 % eines gecachten Treffers | [#2656](https://github.com/ujeenet/rustango/issues/2656) | Offen |
| Eine `HashMap` mit eigenen Schlüsseln für jede `values()`-Zeile | 6–7 % | [#2661](https://github.com/ujeenet/rustango/issues/2661) | **Behoben in 0.60.7** (Namen einmal gelesen) |
| Querysets werden bei jedem Request neu gebaut und kompiliert | ~2 % | [#2662](https://github.com/ujeenet/rustango/issues/2662) | Teilweise behoben in 0.60.7 |
| `.only()` / `values_list` können keine verknüpfte Spalte auswählen (`author__name`) | API-Lücke | [#2657](https://github.com/ujeenet/rustango/issues/2657) | Offen |
| Kein `= ANY($1)`-Array-Bind für `IN`-Listen | unter 0,5 % | [#2658](https://github.com/ujeenet/rustango/issues/2658) | Offen |
| Kein Batch-Prefetch für Viele-zu-Viele | API-Lücke | [#2659](https://github.com/ujeenet/rustango/issues/2659) | Offen |

---

## Grenzen

- Eine **lese-lastige, serverseitig gerenderte** Arbeitslast. Keine
  Schreibvorgänge, Auth-Flows, Websockets oder schwere Geschäftslogik.
- Ein Host (M3-Pro-Laptop, 6 Performance- + 6 Effizienz-Kerne). Absolute Zahlen
  weichen auf anderer Hardware ab.
- Go und die reinen Rust-Anwendungen sind handgeschriebene Dienste; Rustango, die
  Python-Anwendung, Laravel und WordPress sind Frameworks.
- WordPress läuft auf MySQL, weil es PostgreSQL nicht unterstützt.

---

## Reproduzieren

Das Harness ist ein eigenständiges Projekt (`rustango-bench`). Aus seinem
Verzeichnis:

```sh
bench/vendor.sh                 # vendor rustango at origin/develop into the build
docker compose --profile app build
bench/run.sh                    # 5 compiled apps; APPS="..." to pick others
```

Passen Sie den Durchlauf mit `REPS`, `DURATION`, `WARMUP` und `CONCURRENCY` an,
und die CPU-Aufteilung mit `CAP_CPUS` und den `CAP_CPUSET_*`-Werten in `.env`.
Rohes `oha`-JSON, die HTML-Paritätsdateien und eine `meta.txt` mit jeder Version
landen in `bench/results/`.

---

## Siehe auch

- [Getting started](getting-started.md)
- [ORM cookbook](orm.md)
