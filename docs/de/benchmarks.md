# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

Derselbe Blog in jeder Anwendung, mit denselben Daten und demselben HTML.
Bisher zwei Runden:

- **Runde 2, 14. Oktober 2026: kompilierte Anwendungen, profiliert.** **Go**
  (stdlib `net/http` + `pgx`) und sieben Rust-Anwendungen: reines **axum** und
  **actix-web** mit rohem `sqlx`, **Rocket**, **Loco**, **Cot** sowie
  **Rustango** auf axum und auf actix-web. Nur die getestete Anwendung läuft.
  CPU-, Syscall- und Postgres-Wire-Profile zeigen, wo jede Anwendung ihre Zeit
  verbringt.
- **Runde 1, 13. Oktober 2026: alle Laufzeiten.** Go, Rustango und reines
  Rust, ein **Python**-Framework auf gunicorn, Hypercorn, Uvicorn und Granian
  (Python-Server am 14. Oktober gemessen), **Laravel** auf php-fpm und auf
  Octane, sowie **WordPress**. Ihre kompilierten Ergebnisse ersetzt Runde 2;
  ihre interpretierten Ergebnisse stehen unten.

Artikel mit Diagrammen (Englisch):
[Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled)
und [Runde 1](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress).

## Zusammenfassung

- **Nicht gecachte Seiten.** Rustango auf actix-web und auf axum sind die zwei
  schnellsten der acht kompilierten Anwendungen auf Index, Detail und
  Tag-Seite (Index: 17.401 und 14.686 Req/s; axum + sqlx 14.140, Go 8.741).
- **Gecachte Seiten.** Jede Rust-Anwendung liefert mehr als Go: 138.224–231.975
  Req/s beim gecachten Index gegenüber 91.839 bei Go. Hauptursache ist der
  Garbage Collector von Go.
- **Compute.** Go ist 8–17 % schneller als die Rust-Anwendungen. Rust mit der
  Befehlsreihenfolge von Go läuft in der Zeit von Go.
- **Speicher unter Last:** 6,3–12,0 MiB bei sechs der Rust-Anwendungen, Cot
  22,6 MiB, Go 22,8 MiB.
- **Interpretierte Laufzeiten** (Runde 1) liegen bei nicht gecachten Seiten
  4- bis 40-fach hinter den kompilierten Anwendungen. Bei gecachten Seiten ist
  WordPress unter ihnen am schnellsten (21k Req/s). Bei Python schlägt
  gunicorn (synchron) die drei ASGI-Server; Granian ist der schnellste
  ASGI-Server. Speicher unter Last: 89–293 MiB.
- **Rustango-Korrekturen.** Die Profile zeigten, wo Rustango Zeit verlor;
  0.60.7 und 0.60.8 beheben die großen Kosten, 0.61.2 ergänzt zwei fehlende
  Abfrage-Funktionen (siehe [die Tabelle](#rustango-profil-befunde-und-status)).

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

### Testregeln

- **CPU und Speicher.** Jede Anwendung bekommt 4 CPUs (Kerne 0–3) und 2 GB RAM.
  Go läuft mit `GOMAXPROCS=4`, Tokio und actix-web mit 4 Workern.
- **Datenbank-Pool.** 16 PostgreSQL-Verbindungen pro Anwendung (Runde 2: 16
  maximal, 4 minimal; die Python-Anwendung: 4 pro Worker-Prozess, 16
  insgesamt).
- **Abfragen.** Die kompilierten Anwendungen führen denselben Plan aus:
  3 Abfragen für den Index, 3 für Detail, 4 für die Tag-Seite (Cot: 5, 5 und
  7). Rustango und Loco bauen sie mit ihrem ORM; die reinen Rust-Anwendungen
  nutzen den SQL-Text von Go. Die interpretierten Anwendungen nutzen das
  eifrige Laden ihres ORM. WordPress führt 11–13 Abfragen pro Seite aus.
- **Ausgabe.** Vor jeder Last wird jede Seite jeder Anwendung mit dem HTML von Go
  verglichen; bei einer Abweichung bricht der Durchlauf ab. Keine Kompression,
  daher sendet jede Anwendung dieselbe Indexseite von ~6,4 KB.
- **Produktionseinstellungen.** Debug aus. PHP: OPcache an, Zeitstempelprüfung
  aus, `php artisan optimize`. Python: Verbindungen aus dem Pool, Access-Logs
  aus.
- **Andere Dienste.** PostgreSQL 16, MySQL 8.4 (nur WordPress) und Redis 7 auf
  den Kernen 4–7; der Lastgenerator [`oha`](https://github.com/hatoo/oha)
  1.16.0 auf den Kernen 8–11, im Docker-Netzwerk. 50 Verbindungen, Keep-Alive
  an.
- **Isolation (Runde 2).** Nur die getestete Anwendung läuft; die anderen sind
  gestoppt. Zwischen den Anwendungen kehrt die Zahl der Postgres-Verbindungen
  auf ihren Ausgangswert zurück.
- **Durchläufe.** Pro Endpunkt: 5 s Aufwärmen, dann 3 × 20 s; der Median zählt.
  Nur HTTP-200-Antworten zählen. Runde 2 hatte 0 Fehler oder Nicht-200-Antworten
  in 144 Durchläufen.

Host: Apple M3 Pro, Docker-VM mit 12 CPUs und 16 GB.

---

## Runde 2: kompilierte Laufzeiten

Gemessen am **14. Oktober 2026**.

### Die Anwendungen

| Anwendung | Server | Datenzugriff | Templates | Seiten-Cache |
|---|---|---|---|---|
| **Go** | stdlib `net/http` | `pgx`-v5-Pool, rohes SQL | `html/template` | `go-redis` |
| **axum + sqlx** | axum | rohes `sqlx`, SQL-Text von Go | Tera | handgeschriebenes Redis |
| **actix + sqlx** | actix-web | rohes `sqlx`, SQL-Text von Go | Tera | handgeschriebenes Redis |
| **Rocket** | Rocket | `rocket_db_pools` (sqlx) | `rocket_dyn_templates` (Tera) | Redis |
| **Rustango · axum** | axum | rustango-ORM | Tera | `CachePageLayer` |
| **Rustango · actix** | actix-web | rustango-ORM | Tera | Seiten-Cache von rustango |
| **Loco** | Loco | SeaORM | Tera | Redis |
| **Cot** | Cot | ORM von Cot | askama | Redis |

### Versionen

- Go 1.25.14, pgx v5.7.2, go-redis v9.7.0
- rustc 1.99.0 (`opt-level=3`, Fat LTO, `codegen-units=1`, `panic=abort`);
  axum 0.8.9, actix-web 4.16.0, tokio 1.53, sqlx 0.8.6, Tera 1.20.1
- Rocket 0.5.1 (sqlx 0.7.4, hyper 0.14)
- Loco 1.2.0 (SeaORM 2.0.4, sqlx 0.9.0)
- Cot 0.7.0 (sqlx 0.9.0, askama 0.16.1)
- Rustango: ein Entwicklungs-Build ohne Acquire-Ping, mit einem Seiten-Cache,
  der rohe Bytes speichert, und einer Pool-Verbindung pro Request. Diese drei
  Änderungen kamen mit 0.60.8.

### Einstellungen, die nicht angeglichen werden konnten

| Einstellung | Anwendungen | Warum |
|---|---|---|
| Der Pool pingt Postgres bei jedem Checkout (sqlx `test_before_acquire`) | Rocket, Loco, Cot | Keine Einstellung zum Abschalten. In axum + sqlx, actix + sqlx und Rustango ist er aus. |
| 10 Pool-Verbindungen statt 16 | Cot | Cot hat keine Pool-Einstellungen. |
| Ein Pool-Anteil pro Worker (4 der 16 Verbindungen) und eine Redis-Verbindung pro Worker | actix + sqlx, Rustango · actix | Ein gemeinsamer Pool ist auf actix-web langsamer ([Befund](#actix-web-ein-gemeinsamer-pool-ist-langsam)). |
| sqlx 0.7.4 und hyper 0.14 | Rocket | Von Rocket 0.5.1 festgelegt. |
| 5, 5 und 7 Abfragen pro Index-, Detail- und Tag-Seite statt 3, 3 und 4 | Cot | Seine Abfrage-API kann die Abfragen von Go nicht ausdrücken. |
| askama statt Tera | Cot | Die Template-Engine von Cot. |

### Durchsatz: Requests pro Sekunde (höher ist besser)

| Anwendung | Index | Detail | Tag | Index, gecacht | Detail, gecacht | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 8.741 | 22.607 | 6.386 | 91.839 | 141.574 | **16.580** |
| axum + sqlx | 14.140 | 22.842 | 6.467 | 198.669 | 214.871 | 14.963 |
| actix + sqlx | 14.408 | 21.630 | 6.628 | 226.887 | 240.161 | 14.578 |
| Rocket | 13.316 | 24.561 | 6.836 | 138.224 | 155.691 | 14.696 |
| Rustango · axum | 14.686 | 25.788 | **7.090** | 174.501 | 192.897 | 14.799 |
| Rustango · actix | **17.401** | **27.292** | 7.056 | **231.975** | **242.609** | 14.710 |
| Loco | 10.290 | 15.635 | 5.604 | 153.243 | 170.158 | 14.184 |
| Cot | 2.018 | 9.282 | 3.065 | 190.245 | 203.119 | 15.330 |

Index, Detail und Tag sind nicht gecacht.

### Latenz: p50 / p99 in ms (niedriger ist besser)

| Anwendung | Index | Detail | Tag | Index, gecacht | Detail, gecacht | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 5,50 / 9,96 | 1,91 / 5,02 | 7,61 / 11,72 | 0,35 / 2,73 | 0,24 / 2,05 | **2,37** / 10,75 |
| axum + sqlx | 3,49 / 4,71 | 2,14 / 3,60 | 7,42 / 13,89 | 0,24 / 0,53 | 0,22 / 0,51 | 3,35 / 6,07 |
| actix + sqlx | 3,32 / 6,38 | 2,21 / 4,30 | 7,23 / 13,23 | **0,20** / 0,50 | **0,19 / 0,45** | 3,41 / 4,97 |
| Rocket | 3,66 / 5,83 | 1,97 / 3,58 | 7,14 / 11,55 | 0,34 / 0,70 | 0,30 / 0,67 | 3,40 / 6,14 |
| Rustango · axum | 3,28 / 5,65 | 1,83 / 3,55 | **6,90 / 11,18** | 0,27 / 0,59 | 0,25 / 0,52 | 3,38 / 6,09 |
| Rustango · actix | **2,80 / 4,51** | **1,77 / 3,37** | 6,91 / 11,53 | **0,20 / 0,48** | **0,19 / 0,45** | 3,41 / **3,96** |
| Loco | 4,62 / 10,84 | 2,99 / 6,34 | 8,67 / 14,52 | 0,30 / 0,79 | 0,28 / 0,62 | 3,48 / 6,90 |
| Cot | 23,35 / 50,36 | 5,28 / 8,02 | 15,57 / 27,06 | 0,25 / 0,50 | 0,23 / 0,52 | 3,27 / 5,85 |

### Speicher unter Last

Residenter Speicher des Anwendungs-Containers beim nicht gecachten Index
(`docker stats`, die Probe mit der höchsten CPU von sechs, genommen während des
ersten Index-Durchlaufs):

| | Go | axum + sqlx | actix + sqlx | Rocket | Rustango · axum | Rustango · actix | Loco | Cot |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| RAM, MiB | 22,8 | 7,6 | 7,2 | 8,7 | 6,9 | **6,3** | 12,0 | 22,6 |

Image-Größen aus Runde 1 (unkomprimiert): Go 18,5 MB auf `scratch`, die
Rust-Anwendungen 143–147 MB auf `debian:bookworm-slim`.

### Rustango von Runde 1 zu Runde 2

Rustango · actix, nicht gecachter Index, Req/s:

| Durchlauf | Req/s |
|---|--:|
| Runde 1 (0.60.6) | 6.341 |
| Korrekturen, gemeinsamer Pool (Profiling-Lauf) | 12.052 |
| Korrekturen, ein Pool-Anteil pro Worker (Runde 2) | **17.401** |

Rustango · axum vor → nach den Korrekturen, Profiling-Läufe:

| | Vorher | Nachher |
|---|--:|--:|
| Index, Req/s | 11.215 | **15.849** |
| Detail, Req/s | 17.267 | **28.097** |
| Index, Anwendungs-CPU pro Request | 286,6 µs | **220,8 µs** |
| Detail, Anwendungs-CPU pro Request | 164,7 µs | **110,4 µs** |

---

## Warum: was schneller ist, und wo

Nur gemessene Fakten; wo eine Ursache nicht belegt ist, steht das dabei. Die
Profile sind einzelne 25-s-Läufe, eine Anwendung nach der anderen, daher
weichen ihre Req/s leicht von den Tabellen oben ab.

### Compute: Go ist 8–17 % schneller

Beide kompilierten Schleifen haben 8 Befehle, ein 64-Bit-`sdiv` und eine
Prüfung auf Division durch null; LLVM wählt eine andere Befehlsreihenfolge.
Allein auf einem Kern, ohne HTTP, dauert ein Aufruf 194 µs in Go, 224 µs in
Rust und 194 µs in Rust mit exakt der Befehlsreihenfolge von Go. Welches
CPU-Detail diese Reihenfolge langsamer macht, ist nicht belegt: die Docker-VM
hat keine Hardware-Zähler. Der Rest jedes Requests ist in Rust billiger (8 µs
gegenüber 14 µs bei Go).

Go hat den niedrigeren p50, aber den höheren p99: es bedient Requests außer der
Reihe (p10 0,56 ms, p99 10,75 ms), die Rust-Anwendungen nahezu in
Eingangsreihenfolge (axum + sqlx: p10 1,85 ms, p99 6,07 ms). Warum der p99 von
Go bei etwa 10 ms liegt, ist nicht belegt.

### Gecachte Seiten: der Garbage Collector von Go

Go führt beim gecachten Index 441 Garbage Collections pro Sekunde aus; die
Stop-the-World-Pausen summieren sich auf etwa 10 % der Zeit. Ein höheres
`GOGC` schließt den größten Teil der Lücke (Profiling-Läufe, Standard
`GOGC=100`: 97.289 Req/s):

| Go, gecachter Index | Req/s | p99 |
|---|--:|--:|
| `GOGC=100` (Standard) | 97.289 | 2,48 ms |
| `GOGC=400` | 144.083 | |
| GC fast aus | 159.734 | 0,90 ms |

Der Rest ist CPU pro Request:

| Gecachter Index, pro Request | Go | axum + sqlx |
|---|--:|--:|
| Anwendungs-CPU | 27,2 µs | **14,6 µs** |
| Schreibpfad des Kernels | 10,6 µs | **6,9 µs** |
| Syscalls | 10,7 (6,6 read, 2,9 write) | **4,8** (2,0 recvfrom, 1,4 writev) |
| Redis-CPU | 6,8 µs | **2,5 µs** |
| Redis-Verkehr | go-redis: ein GET pro Request | eine Verbindung, GETs vieler Requests in einer Pipeline gebündelt |

### Detailseite: Postgres-Roundtrips

Alle Anwendungen senden dieselben drei Abfragen, aber die Zahl der Roundtrips
unterscheidet sich. pgx sendet einen pro Abfrage. sqlx fügt einen hinzu, wenn
eine Verbindung aus dem Pool genommen wird (der Acquire-Ping), und einen, wenn
sie zurückgeht. axum + sqlx nimmt eine Verbindung pro Abfrage und zahlt daher
pro Abfrage; Rocket und Rustango nehmen eine Verbindung pro Seite und zahlen
einmal. Wire-Mitschnitt unter Last, ReadyForQuery-Nachrichten pro Detailseite
gezählt (der Index zeigt dieselben Zahlen):

| Anwendung | Roundtrips |
|---|--:|
| Go · pgx | **3** |
| Rustango | 4 |
| Rocket | 5 |
| axum + sqlx, Ping aus | 6 |
| axum + sqlx, Ping an | 9 |

Acquire-Ping in axum + sqlx abgeschaltet (Profiling-Läufe): Detail
18.222 → 23.472 Req/s (Postgres-CPU 166,6 → 134,3 µs pro Request), Index
12.140 → 14.419 Req/s (237,9 → 208,5 µs). Drei Roundtrips weniger sparen
29–32 µs Postgres-CPU pro Seite: etwa 10 µs pro Roundtrip. Go hat die
wenigsten Roundtrips, aber sein Garbage Collector hält es auf dieser Route etwa
9 % der Zeit an.

### Indexseite: GC und Template-Rendering

Wieder der Garbage Collector von Go (Pausen etwa 8 % der Zeit; `GOGC=400`
bringt den Index von 9.359 auf 12.848 Req/s, GC fast aus auf 13.967) und das
Template-Rendering: `html/template` von Go nutzt Reflection und kostet 167 µs
pro Request, serde + Tera 72 µs. In axum + sqlx kostet sqlx etwa 112 µs pro
Request (davon etwa 25 µs der Ping, wenn eine Verbindung in den Pool
zurückgeht), glibc malloc etwa 56 µs und Kernel-`sendto` 38 µs; die Anteile
überlappen.

### actix-web: ein gemeinsamer Pool ist langsam

In Runde 1 lagen die actix-web-Anwendungen bei nicht gecachten Seiten hinter
axum und nutzten weniger als 300 % ihrer 400 % CPU. axum betreibt eine
gemeinsame, mehrfädige Tokio-Laufzeit; actix-web eine einfädige Laufzeit pro
Worker. Mit einem Pool für alle Worker weckt jede Abfrage einen Thread einer
anderen Laufzeit. Ein Pool-Anteil (4 der 16 Verbindungen) und eine
Redis-Verbindung pro Worker beheben das (Profiling-Läufe, Req/s):

| | Gemeinsamer Pool | Pool pro Worker |
|---|--:|--:|
| actix + sqlx, Index | 11.094 | **15.108** |
| actix + sqlx, Detail | 18.080 | **23.330** |
| actix + sqlx, Index gecacht | 192.604 | **238.884** |
| Rustango · actix, Index | 12.052 | **17.454** |
| Rustango · actix, Detail | 23.032 | **28.693** |
| Rustango · actix, Index gecacht | 190.889 | **237.962** |

Die Kontextwechsel pro Request sinken etwa 2,5-fach (actix + sqlx Index
4,04 → 1,45, Detail 3,57 → 1,64; Rustango · actix Index 2,70 → 0,81, Detail
2,34 → 1,03). Mehr Worker helfen nicht: 8 Worker mit gemeinsamem Pool
erreichen 11.904 Req/s beim Index von actix + sqlx. Der Effekt ist gemessen;
der Mechanismus (ein Socket wird von der Laufzeit bedient, die ihn geöffnet
hat) ist die beste Erklärung, aber nicht direkt belegt. **Auf actix-web den
Datenbank-Pool in jedem Worker anlegen, nicht einmal für alle Worker.**

### Rocket, Loco und Cot

- **Rocket** ist stark bei Datenbankseiten (eine Verbindung pro Seite,
  5 Roundtrips) und die langsamste Rust-Anwendung bei gecachten Seiten:
  24,1 µs CPU pro Request gegenüber 14,6 µs bei axum + sqlx, 8,1 Syscalls
  gegenüber 4,8, ein zusätzliches `writev` (2,24 pro Request gegenüber 1,36)
  und Konkurrenz in malloc (futex 2,88 pro Request gegenüber 0,91).
- **Loco**: SeaORM sendet so viele Abfragen wie Go. Bei gecachten Seiten
  braucht Loco 21,0 µs pro Request: 2,4 µs mehr in malloc und 1,2 µs mehr in
  der Middleware als axum + sqlx. Bei Datenbankseiten lässt sich der
  Acquire-Ping nicht abschalten; diese Seiten wurden nicht profiliert, daher
  ist offen, wie viel der Lücke er erklärt.
- **Cot**: die Abfrage-API kennt nur filter, limit und offset (kein ORDER BY,
  JOIN, IN oder COUNT), daher lädt der Index bei jedem Request alle 1.000
  Beiträge mit ihrem Text: 5, 5 und 7 Abfragen statt 3, 3 und 4, und 22,6 MiB
  unter Last. Gecachte Seiten umgehen das ORM und sind schnell (16,4 µs CPU pro
  Request).

### Gemeinsame Kosten

Zwei Kosten zeigen sich in jeder Rust-Anwendung: Tera-Rendering (etwa 18–20 %
der CPU bei den nicht gecachten Seiten) und glibc malloc (etwa 12 %). Ein
schnellerer Allokator wie mimalloc oder jemalloc könnte das Zweite senken; das
ist nicht getestet.

### Interpretierte Laufzeiten (Runde 1)

- **Gecachte Seiten.** WordPress antwortet aus Redis in `advanced-cache.php`,
  bevor WordPress selbst lädt (21k Req/s). Die Python-Anwendungen und Laravel
  durchlaufen zuerst ihr Framework und ihre Middleware und lesen dann den
  Cache (Python 5–12k, Laravel 3–6k).
- **Laravel: Octane 2- bis 3-fach php-fpm bei nicht gecachten Seiten.** php-fpm
  startet das Framework bei jedem Request; Octane hält es im Speicher.
- **Python: asynchrone Views schlagen gunicorn nicht.** gunicorn liefert bei
  gecachten Seiten das 1,4- bis 2,3-Fache der ASGI-Server und bei nicht
  gecachtem Detail und Tag das 1,4- bis 1,8-Fache; beim nicht gecachten Index
  sind alle vier gleichauf (663–697). Laut Code des Frameworks führt das
  asynchrone ORM das synchrone ORM in einem Thread aus (`sync_to_async`), es
  gibt keinen asynchronen Datenbank-Pool, und der ASGI-Handler legt pro Request
  einen Thread-Executor an, plus etwa 3 Thread-Wechsel (Request-Signale,
  Schließen der Antwort). Der asynchrone Cache-Pfad macht weniger Cache-Arbeit
  als der von gunicorn (ein Redis-GET von rohem HTML gegenüber zwei GETs und
  einem Unpickle) und ist trotzdem langsamer. Der Anteil jeder Ursache wurde
  nicht profiliert.
- **ASGI-Server: Granian > Uvicorn > Hypercorn.** Gecachter Index 7.706 /
  6.233 / 5.151 Req/s, nicht gecachtes Detail 1.854 / 1.654 / 1.550. Nicht
  profiliert.
- **Compute.** PHP 8.3 mit OPcache führt die Integer-Schleife etwa 4-mal
  schneller aus als CPython.
- **WordPress nicht gecacht** führt 11–13 Abfragen pro Seite auf MySQL aus,
  gegenüber 3–4 auf PostgreSQL bei den anderen Anwendungen.

---

## Rustango: Profil-Befunde und Status

Aus `perf`-Profilen von Rustango unter Last: Runde 1 (0.60.6) und die
Profiling-Läufe von Runde 2.

| Kosten | CPU-Anteil | Status |
|---|---|---|
| Jeden Wert dekodieren, indem etwa zehn Typen probiert werden | 46 % des Index | **Behoben in 0.60.7**: jede Spalte einmal nach ihrem Typ dekodiert (jetzt 4,9 %) |
| Ein Pool-Checkout für jede Abfrage | 19–35 % | **Behoben in 0.60.8**: `pool.acquire()` führt die Abfragen eines Requests auf einer Verbindung aus (Detail +26 %) |
| Ping an Postgres bei jedem Checkout | 8–16 % | **Behoben in 0.60.8**: standardmäßig aus; `pool_test_before_acquire` schaltet ihn ein |
| `CachePageLayer` speichert Seiten als JSON + base64 | ~12–20 % pro Cache-Treffer | **Behoben in 0.60.8**: Seiten als rohe Bytes gespeichert |
| Eine `HashMap` mit eigenen Schlüsseln für jede `values()`-Zeile | 6–7 % | Verringert in 0.60.7: Namen einmal pro Ergebnismenge gelesen; der eigene Schlüssel bleibt (öffentlicher Typ) |
| Querysets werden bei jedem Request neu gebaut und kompiliert | ~2 % | Offen; 0.60.7 quotet Bezeichner mit einer Allokation |
| `.only()` / `values_list` können keine verknüpfte Spalte auswählen (`author__name`) | API-Lücke | **Hinzugefügt in 0.61.2** |
| Kein `= ANY($1)`-Array-Bind für `IN`-Listen | unter 0,5 % | **Hinzugefügt in 0.61.2** (PostgreSQL, Listen eines Typs) |
| Kein Batch-Prefetch für Viele-zu-Viele | API-Lücke | Offen |
| Gecachter Index: Rustango · axum 174.501 Req/s gegenüber 198.669 bei axum + sqlx | nicht profiliert | Offen |

---

## Runde 1: interpretierte Laufzeiten

Die vier Python-Server stammen aus einem Lauf (14. Oktober 2026); Laravel und
WordPress vom 13. Oktober 2026. Dieselben Regeln wie oben, mit allen
Anwendungen gleichzeitig laufend.

### Die Anwendungen

| Anwendung | Server | Datenzugriff | Seiten-Cache |
|---|---|---|---|
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI, synchrone Views) | ORM des Frameworks, eingebauter psycopg-Pool | Redis-Cache des Frameworks |
| **Python · Hypercorn** | Hypercorn, 4 Worker, uvloop (ASGI, asynchrone Views) | asynchrone ORM-API des Frameworks, derselbe Pool | `redis.asyncio`, ein GET pro Treffer |
| **Python · Uvicorn** | Uvicorn, 4 Worker, uvloop + httptools (ASGI, asynchrone Views) | dasselbe | dasselbe |
| **Python · Granian** | Granian, 4 Worker, uvloop (ASGI, asynchrone Views) | dasselbe | dasselbe |
| **Laravel · php-fpm** | nginx + php-fpm, 16 statische Kindprozesse | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 Worker | dasselbe | dasselbe |
| **WordPress** | nginx + php-fpm, 16 statische Kindprozesse | `WP_Query`, `get_the_tags`, `get_comments` auf **MySQL 8.4** | `advanced-cache.php`-Drop-in, phpredis |

Templates: die eigene Engine des Python-Frameworks, Blade für Laravel, ein
kleines Theme für WordPress. Die asynchronen Python-Anwendungen nutzen
`async def`-Views auf der asynchronen ORM-API des Frameworks, einen
asynchronen Middleware-Wrapper und einen gemeinsamen `redis.asyncio`-Pool von
16 pro Prozess; ein gecachter Treffer ist ein Redis-GET von rohem HTML.

Versionen: PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3, WordPress 6.9.10,
Uvicorn 0.54.0, Granian 2.8.3, uvloop 0.22.1, httptools 0.8.0.

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
14.253 „Datenbankverbindung"-Fehler in 7 s; der Median stammt aus einem
sauberen Durchlauf.

---

## Grenzen

- Eine **lese-lastige, serverseitig gerenderte** Arbeitslast. Keine
  Schreibvorgänge, Transaktionen, Auth-Flows, Websockets oder schwere
  Geschäftslogik.
- Ein Host: ein Apple-M3-Pro-Laptop (6 Performance- + 6 Effizienz-Kerne).
  Andere CPUs können die Ergebnisse anders ordnen, vor allem bei compute.
- Die Anwendungen laufen in der Docker-VM auf macOS, nicht auf Linux ohne VM.
  Die VM hat keine Hardware-Leistungszähler.
- Go und die reinen Rust-Anwendungen sind handgeschriebene Dienste; Rustango,
  Rocket, Loco, Cot, die Python-Anwendung, Laravel und WordPress sind
  Frameworks.
- Runde 1 und 2 sind getrennte Läufe; Zahlen nur innerhalb einer Runde
  vergleichen.
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

- [Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled) (Englisch)
- [Runde 1: der faire Benchmark](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress) (Englisch)
- [Getting started](getting-started.md)
- [ORM cookbook](orm.md)
