# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

Wie schnell ist **Rustango**? Diese Seite vergleicht denselben Blog auf zehn
Laufzeiten: **Go** (`net/http` der Standardbibliothek + `pgx`), **Rustango** auf
axum und auf actix-web, reines **axum** und **actix-web** mit rohem `sqlx`, ein
**Python**-Framework auf gunicorn und auf Hypercorn, **Laravel** auf php-fpm und
auf Octane, sowie **WordPress**. Jede Anwendung rendert dasselbe HTML aus
denselben Daten.

Alle Zahlen wurden am **13. Oktober 2026** mit rustango 0.60.6 gemessen, in einem
Harness, auf einer Maschine, nach denselben Regeln.

> **Kurzfassung.** Mit exakt dem SQL von Go schlägt reines Rust (axum + sqlx) Go
> beim nicht gecachten Index um **34 %**, liegt auf der Tag-Seite gleichauf und
> bleibt auf der nicht gecachten Detailseite **18 %** zurück. Auf Redis-gecachten
> Seiten liefert jede Rust-Anwendung das **1,5- bis 2,2-Fache** der Requests von Go,
> mit einem p99 von etwa **0,45 ms** (Go: 2,0–2,4 ms). In 0.60.6 kostete
> **Rustango** **5–32 %** gegenüber rohem sqlx auf demselben Server mit denselben
> Abfragen; **0.60.7 schließt diese Lücke**: Rustango liefert jetzt **93–104 %**
> von rohem axum + sqlx beim nicht gecachten Index und 94–100 % auf den anderen
> Seiten (siehe [0.60.7](#0607-die-orm-lücke-schließen)). Die interpretierten Laufzeiten liegen bei nicht gecachten Seiten **5- bis
> 40-fach** zurück; am schnellsten sind dort die Python-Anwendung beim Detail
> (2.865 Req/s) und Laravel auf Octane beim Index (1.098). Alle Rust-Anwendungen
> laufen unter Last mit **7–8 MiB**; Go mit 17–21 MiB; die interpretierten
> Laufzeiten mit 89–296 MiB.

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
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI) | ORM des Frameworks, eingebauter psycopg-Pool | Redis-Cache des Frameworks |
| **Python · Hypercorn** | Hypercorn, 4 Worker (ASGI, synchrone Views) | dasselbe | dasselbe |
| **Laravel · php-fpm** | nginx + php-fpm, 16 statische Kindprozesse | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 Worker | dasselbe | dasselbe |
| **WordPress** | nginx + php-fpm, 16 statische Kindprozesse | `WP_Query`, `get_the_tags`, `get_comments` auf **MySQL 8.4** | `advanced-cache.php`-Drop-in, phpredis |

Templates: Tera für die Rust-Anwendungen, `html/template` für Go, die eigene
Engine des Python-Frameworks, Blade für Laravel, ein kleines Theme für WordPress.

### Was den Vergleich fair hält

- **Gleiches CPU-Budget.** Jede Anwendung bekommt 4 CPUs (Kerne 0–3, `cpus: 4`,
  2 GB RAM). Go läuft mit `GOMAXPROCS=4`, Tokio und actix-web mit 4 Workern.
- **Gleicher Pool.** Jede kompilierte Anwendung hat 16 PostgreSQL-Verbindungen,
  mindestens 4. Die Python-Anwendung nutzt den eingebauten Pool ihres Frameworks,
  4 pro Worker-Prozess (16 pro Anwendung).
- **Gleiche Abfragen.** Alle kompilierten Anwendungen nutzen den Plan von Go:
  3 Abfragen für den Index, 3 für Detail, 4 für die Tag-Seite. Die
  Rustango-Anwendungen bauen sie mit dem ORM; die reinen Rust-Anwendungen nutzen
  den SQL-Text von Go wörtlich. Die interpretierten Anwendungen nutzen das
  übliche eifrige Laden ihres ORM. WordPress führt 11–13 Abfragen pro Seite aus,
  3 davon seine eigenen Start-Abfragen.
- **Gleiche Ausgabe.** Vor jeder Last wird jeder Endpunkt jeder Anwendung
  abgerufen und mit dem HTML von Go verglichen; es muss nach Normalisierung von
  Leerraum und Entity-Stil übereinstimmen, sonst bricht der Durchlauf ab. Die
  Antworten sind überall unkomprimiert (`Accept-Encoding: identity`, gzip in nginx
  aus), daher sendet jede Anwendung dieselbe Indexseite von ~6,4 KB.
- **Produktionsmodus.** Debug überall aus. PHP: OPcache an, Zeitstempelprüfung
  aus, `php artisan optimize` (Config, Routen, Events, Views gecacht). Python:
  persistente Verbindungen aus dem Pool.
- **Eigene Kerne für alles andere.** PostgreSQL 16, MySQL 8.4 und Redis 7 laufen
  auf den Kernen 4–7, der Lastgenerator auf den Kernen 8–11.
- **Lastgenerator in Docker.** [`oha`](https://github.com/hatoo/oha) 1.16.0
  (per Digest fixiert) läuft im Compose-Netzwerk, nicht über die
  Port-Weiterleitung von Docker Desktop. 50 Verbindungen, Keep-Alive an.
- **Wiederholte Durchläufe.** Pro Endpunkt: 5 s Aufwärmen, dann 3 × 20 s
  Durchläufe; angegeben ist der Median. Die Reihenfolge der Anwendungen rotiert
  in jeder Runde. Nur HTTP-200-Antworten zählen für Req/s.
- **Fixierte Builds.** Rust: rustc 1.99.0, `opt-level=3`, Fat LTO,
  `codegen-units=1`, `panic=abort`. Go 1.25.14. PHP 8.3.35, Laravel 13.36.0,
  Swoole 6.2.3, WordPress 6.9.10. Basis-Images per Digest, Lock-Dateien im
  Harness eingecheckt.

Host: Apple M3 Pro, Docker-VM mit 12 CPUs und 16 GB. Go lief in jedem Batch als
Anker mit und blieb innerhalb von 6 % seines ersten Durchlaufs.

---

## Kompilierte Laufzeiten

### Durchsatz: Requests pro Sekunde (höher ist besser)

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
plus das Binary, also stammt der Großteil ihrer Größe vom Basis-Image.

### Die Zahlen lesen

- **Rust vs Go, gleiches SQL.** axum + sqlx führt beim nicht gecachten Index
  (+34 %), Go führt beim nicht gecachten Detail (+22 %), und die Tag-Seite ist
  ein Unentschieden. Go liefert außerdem 10 % mehr compute-Requests, aber sein
  compute-p99 (10,8 ms) ist das 2- bis 3-Fache der Rust-Anwendungen.
- **Gecachte Seiten.** Ohne verbleibende Datenbankarbeit liefert jede
  Rust-Anwendung das 1,5- bis 2,2-Fache der Requests von Go, und das p99 bleibt
  unter 0,55 ms.
- **Die Tag-Seite misst PostgreSQL.** Die Datenbank nutzte auf dieser Route 3,65
  ihrer 4 Kerne, also stoßen alle fünf Anwendungen an dieselbe Wand.
- **Rustango vs rohes sqlx** (gleicher axum-Server, gleiche Abfragen): −32 % beim
  Index, −5 % beim Detail, −12 % beim Tag. Rustango ist die einzige Anwendung, die
  ihr volles CPU-Budget ausschöpft (393 %). Der Großteil der Lücke ist eine
  Funktion; siehe [Bekannte Kosten](#bekannte-kosten-in-rustango). 0.60.7
  schließt sie; siehe unten.
- **actix-web vs axum.** Mit einem gemeinsamen sqlx-Pool bleiben die
  actix-Anwendungen bei 272–296 % CPU und liegen bei nicht gecachten Seiten
  hinter axum. Die Ursache ist noch nicht geklärt
  ([#2660](https://github.com/ujeenet/rustango/issues/2660)).

### 0.60.7: die ORM-Lücke schließen

Die Tabelle oben zeigt rustango 0.60.6. 0.60.7 dekodiert jede Spalte einmal nach
ihrem Typ, statt pro Zelle zehn Typen zu probieren
([#2655](https://github.com/ujeenet/rustango/issues/2655)), liest die
Spaltennamen einmal pro Ergebnismenge
([#2661](https://github.com/ujeenet/rustango/issues/2661)) und quotiert
Bezeichner mit einer einzigen Allokation
([#2662](https://github.com/ujeenet/rustango/issues/2662)). Gemessen in einem
Lauf, gleiche Methode, alter und neuer Rustango nebeneinander (Median Req/s,
keine anderen Builds liefen):

| Endpunkt | axum + sqlx | rustango 0.60.6 | rustango 0.60.7 | 0.60.7 vs axum |
|---|--:|--:|--:|--:|
| Index, nicht gecacht | 10.750 | 8.596 | **11.143** | 104 % |
| Detail, nicht gecacht | 17.907 | 17.024 | 16.875 | 94 % |
| Tag, nicht gecacht | 6.242 | 5.596 | 6.023 | 96 % |
| compute | 15.091 | 14.736 | 15.084 | 100 % |

Das Dekodieren sank von 46 % der CPU der Anwendung auf 4,9 %, so viel wie die
typisierten Lesezugriffe von rohem sqlx (5,0 %). Der nicht gecachte Index
schwankte zwischen den Läufen; vergleicht man den besten Lauf jeder Anwendung,
erreicht Rustango 93 % von axum + sqlx. Der Rest ist beiden Anwendungen gemeinsam:
Tera-Rendering (etwa 26 %) und die Verbindungsprüfung des Pools beim Ausleihen
(etwa 18 %).

---

## Interpretierte Laufzeiten

### Durchsatz: Requests pro Sekunde (höher ist besser)

| Endpunkt | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| Index, nicht gecacht | 683 | 601 | 518 | **1.098** | 599 |
| Index, gecacht | 11.052 | 2.744 | 3.326 | 5.884 | **21.285** |
| Detail, nicht gecacht | **2.865** | 1.314 | 572 | 1.712 | 687 |
| Detail, gecacht | 11.473 | 2.644 | 3.013 | 5.836 | **21.591** |
| Tag, nicht gecacht | **1.337** | 786 | 490 | 1.102 | 647 |
| compute | 410 | 353 | **1.611** | 1.521 | 732 |

### Latenz: p50 / p99 in ms (niedriger ist besser)

| Endpunkt | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| Index, nicht gecacht | 74,5 / 137,7 | 96,4 / 213,0 | 95,5 / 122,6 | **44,1** / 110,1 | 78,1 / **103,9** |
| Index, gecacht | 4,4 / 12,5 | 18,7 / 40,4 | 14,6 / 22,4 | 7,4 / 57,7 | **2,3 / 3,5** |
| Detail, nicht gecacht | **14,2 / 43,0** | 35,3 / 76,2 | 86,5 / 113,4 | 27,9 / 96,2 | 70,5 / 99,0 |
| Detail, gecacht | 4,1 / 11,4 | 19,0 / 48,2 | 16,4 / 21,6 | 7,4 / 58,2 | **2,3 / 3,6** |
| Tag, nicht gecacht | **35,6 / 77,0** | 66,0 / 131,1 | 100,8 / 130,1 | 44,0 / 110,9 | 75,9 / 97,0 |
| compute | 117,9 / 222,5 | 129,6 / 245,3 | **31,3 / 38,7** | 32,7 / 78,5 | 67,0 / 89,0 |

### Fußabdruck

| | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| Image (unkomprimiert) | **294 MB** | **294 MB** | 844 MB | 895 MB | 1,01 GB |
| RAM, im Leerlauf | 128 MiB | 172 MiB | **54 MiB** | 219 MiB | 61 MiB |
| RAM, Index unter Last | 224 MiB | 296 MiB | **89 MiB** | 241 MiB | 142 MiB |
| CPU, Index unter Last (von 400 %) | 260 % | 323 % | 380 % | 337 % | 405 % |

### Die Zahlen lesen

- **Nicht gecachte Seiten.** Das beste interpretierte Ergebnis auf jeder Seite
  liegt beim Index und beim Detail 6- bis 11-fach hinter reinem Rust und Go, auf
  der Tag-Seite etwa 5-fach.
- **Gecachte Seiten.** Das `advanced-cache.php` von WordPress liefert einen
  Treffer, bevor WordPress lädt, und schlägt damit jede andere interpretierte
  Laufzeit (21k Req/s). Danach folgt der Cache der Python-Anwendung auf gunicorn
  (11k); Laravel durchläuft seinen Middleware-Stack vor `Cache::remember` (3–6k).
- **gunicorn vs Hypercorn.** Die Views sind synchron, also fügt ASGI nur einen
  Threadpool-Sprung hinzu: Hypercorn ist beim Detail 2-fach und bei gecachten
  Seiten 4-fach langsamer als gunicorn mit derselben Anwendung.
- **Octane vs php-fpm.** Ein residenter Swoole-Worker erreicht bei nicht
  gecachten Seiten das 2- bis 3-Fache von php-fpm, bei gecachten Seiten etwa das
  1,8-Fache, und bei compute dasselbe.
- **Compute.** PHP 8.3 mit OPcache führt die Integer-Schleife etwa 4-mal
  schneller aus als CPython.
- **WordPress** läuft auf MySQL, nicht auf PostgreSQL, daher messen seine nicht
  gecachten Zahlen teilweise eine andere Datenbank. Einer seiner drei Durchläufe
  beim nicht gecachten Index scheiterte mit 14.253 „Datenbankverbindung"-Fehlern
  in 7 s (WordPress öffnet pro Request eine MySQL-Verbindung); der Median, 599,
  stammt aus einem sauberen Durchlauf.

---

## Was sich seit der vorigen Version dieser Seite geändert hat

Die Version vom Juni 2026 meldete beim Index Go mit 6.651 Req/s und Rustango mit
4.781, und Laravel, die Python-Anwendung und Hypercorn mit „100 % Erfolg". Mehrere
dieser Zahlen waren falsch.

**Kompilierte Laufzeiten.** Zwei Lücken, jede einzeln mit der obigen Methode
gemessen:

| Endpunkt | Go, 12 Threads | Go, 4 Threads | rustango, alter Plan | rustango, Plan von Go |
|---|--:|--:|--:|--:|
| Index, nicht gecacht | 6.814 | 9.322 (+37 %) | 7.385 | 8.462 (+15 %) |
| Detail, nicht gecacht | 19.184 | 22.125 (+15 %) | 12.546 | 17.594 (+40 %) |
| Tag, nicht gecacht | 5.450 | 6.250 (+15 %) | 5.965 | 5.515 (−8 %) |
| compute | 9.263 | 16.599 (+79 %) | 14.714 | 14.710 (0 %) |

- **Thread-Anzahl von Go.** Das Go-Modul deklarierte `go 1.23`, daher wandte
  Go 1.25 seinen container-bewussten `GOMAXPROCS`-Standard nicht an und lief mit
  12 Threads auf einer Quote von 4 CPUs. Die dadurch verursachte CPU-Drosselung
  erklärt auch das alte compute-p99 von 47 ms, das die alte Seite der Garbage
  Collection zuschrieb.
- **Die Abfragen von Rustango.** Die alte Anwendung führte 5 Abfragen bei Index
  und Detail und 7 beim Tag aus und lud jeden Autor, jeden Tag und den vollen
  Rumpf jedes Beitrags. Jetzt nutzt sie den Plan von Go.
- **Ebenfalls behoben, nicht einzeln gemessen:** Pool-Größe (Standard von sqlx 10
  gegenüber 16 bei Go), rustango 0.43 → 0.60.6, der Lastgenerator zog vom
  macOS-Host ins Docker-Netzwerk, Kern-Pinning, und ein einzelner 10-s-Durchlauf
  wurde durch den Median von drei 20-s-Durchläufen ersetzt.

**Interpretierte Laufzeiten.**

- **„100 % Erfolg" war Verbindungserfolg, nicht HTTP 200.** Der Hypercorn-Durchlauf
  für Detail vom Juni hatte 5.457 HTTP-500-Antworten gegenüber 3.655 guten
  Antworten. Jetzt zählt nur HTTP 200.
- **Hypercorn verlor Datenbankverbindungen.** Persistente Verbindungen unter ASGI
  öffneten für die meisten Requests eine neue PostgreSQL-Verbindung und erreichten
  innerhalb von Sekunden das Limit von 100 Verbindungen. Außerdem öffnete es pro
  Request eine neue Redis-Verbindung. Jetzt nutzt es den PostgreSQL-Pool des
  Frameworks und einen gemeinsamen Redis-Verbindungspool pro Worker.
- **Laravel gingen die Ports aus.** predis öffnete pro Request eine neue
  Redis-Verbindung; unter 20 s Last gingen dem Container die lokalen Ports aus,
  und er lieferte 500er. Jetzt nutzt es phpredis mit persistenten Verbindungen.
- **Laravel sendete komprimierte Seiten.** nginx und Octane komprimierten die
  Antwort mit gzip, daher sendete Laravel ~700 Bytes, wo die anderen Anwendungen
  6,4 KB sendeten. Die Kompression ist jetzt überall aus.
- **Laravels Produktions-Caches fehlten.** Nur die Blade-Views waren gecacht;
  `php artisan optimize` cacht jetzt auch Config, Routen und Events.

---

## Bekannte Kosten in Rustango

Diese zeigten sich beim Angleichen an die Abfragen von Go. Sie sind Arbeit für das
Framework, keine Probleme des Harness. Die CPU-Anteile stammen aus `perf`-Profilen
der rustango-Anwendung unter derselben Last (0.60.6). Jeder Punkt hat ein Ticket:

| Kosten | Ticket | Status |
|---|---|---|
| `values_list`-Zeilen auf PostgreSQL probieren pro Zelle etwa zehn Typen und bauen für jeden Fehlversuch einen Fehler. **46 % der CPU der Anwendung** beim nicht gecachten Index. | [#2655](https://github.com/ujeenet/rustango/issues/2655) | **Behoben in 0.60.7** (jetzt 4,9 %) |
| `CachePageLayer` speichert den Rumpf als base64 in JSON, also parst jeder Treffer JSON und dekodiert base64: **etwa 21 % der CPU pro Treffer**, 165k Treffer/s gegenüber 214k für ein einfaches Redis-`GET`. | [#2656](https://github.com/ujeenet/rustango/issues/2656) | Offen |
| `values()` und `annotate()` bauen für jede Zeile eine `HashMap` mit eigenen String-Schlüsseln: etwa 6–7 % der CPU beim nicht gecachten Index. | [#2661](https://github.com/ujeenet/rustango/issues/2661) | **Behoben in 0.60.7** (Namen werden einmal gelesen; der eigene Schlüssel bleibt, er ist der öffentliche Typ) |
| Jede Anfrage baut und kompiliert dieselben Querysets neu: etwa 2 % der CPU. | [#2662](https://github.com/ujeenet/rustango/issues/2662) | Teilweise behoben in 0.60.7 (Quotierung); Cache für kompiliertes SQL offen |
| `.only()` und `values_list` können keine verknüpfte Spalte wie `author__name` auswählen; um die Spalten von Go zu treffen, braucht es einen manuellen `Join` mit `project`. | [#2657](https://github.com/ujeenet/rustango/issues/2657) | Offen |
| Es gibt keinen `= ANY($1)`-Array-Bind; eine `IN`-Liste wird zu `$1..$n`, also ist jede Listenlänge ein eigenes Prepared Statement. In diesem Benchmark keine CPU-Kosten (unter 0,5 %). | [#2658](https://github.com/ujeenet/rustango/issues/2658) | Offen |
| Es gibt kein Batch-Prefetch für Viele-zu-Viele über eine Liste von Zeilen; Tags kommen über einen Join auf die Zwischentabelle. | [#2659](https://github.com/ujeenet/rustango/issues/2659) | Offen |
| actix-web-Anwendungen nutzen mit einem gemeinsamen sqlx-Pool weniger als 300 % ihrer 4 CPUs. Auch ohne rustango zu sehen. | [#2660](https://github.com/ujeenet/rustango/issues/2660) | Offen |

---

## Grenzen dieses Benchmarks

- Es ist eine **lese-lastige, serverseitig gerenderte** Arbeitslast. Sie misst
  keine Schreibvorgänge, Auth-Flows, Websockets oder schwere Geschäftslogik.
- Ein Host (ein M3-Pro-Laptop mit 6 Performance- und 6 Effizienz-Kernen), der
  während der Durchläufe auch andere Arbeit erledigte (Load Average 3–4). Das
  relative Bild ist übertragbar; absolute Zahlen weichen auf anderer Hardware ab.
- Go und die reinen Rust-Anwendungen sind handgeschriebene Dienste. Rustango, die
  Python-Anwendung, Laravel und WordPress sind Frameworks; die Zeilen Rustango vs
  rohes sqlx zeigen, was diese Schicht heute kostet.
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
