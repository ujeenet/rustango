# Benchmarks: Rustango vs Django vs Laravel vs Go

Wie schnell ist **Rustango** wirklich? Diese Seite berichtet über einen direkten Benchmark
gegen die beiden Frameworks, denen **Rustango** nachempfunden ist — Django und Laravel — und
gegen eine **Go**-Basislinie (das `net/http` der Standardbibliothek), die verankert, was eine
zweite kompilierte, native Laufzeit bei derselben Arbeitslast erreicht. Alle verwenden
*funktional identische* Blog-Sites: dieselben Daten, dasselbe Schema, dieselben Endpunkte,
dasselbe Hardware-Budget — die einzige Variable ist die Laufzeit. Django und Laravel werden
jeweils in **beiden** Varianten gemessen: ihrem konventionellen Produktions-Deployment *und* einer
robusteren Laufzeit: **Django** auf **gunicorn** (WSGI) und auf **Hypercorn**
(ASGI); **Laravel** auf **php-fpm + nginx** und auf **Octane** (Swoole). Rustango
und Go sind jeweils ein einziges residentes Binary, also gibt es von jedem eines.

Jede Zahl unten ist **gemessen und reproduzierbar**, aus einem konsistenten Durchlauf eines
Ein-Kommando-Harness (siehe [Reproduce](#reproduce)). Nichts hier ist mit Handbewegungen weggeredet.

> **Kurzfassung.** Auf identischer Hardware, die identisch gerenderte HTML-Seiten ausliefert, lassen die beiden
> **kompilierten, nativen** Laufzeiten — **Rustango** und **Go** — die interpretierten
> Frameworks **um das 5- bis 30-Fache hinter sich** und wechseln sich untereinander in der Führung ab. Beim
> nicht gecachten Index führte **Go** mit **6.651 Req/s** und **Rustango** folgte mit
> **4.781** — **5,6×** Django (gunicorn) und **11,7×** Laravel (php-fpm). Gos
> Vorsprung ist auf der nicht gecachten Detailseite am größten (**13.921 vs 6.538**, 2,1×).
> **Rustango** erobert die Führung dort zurück, wo es für ausgelieferten Traffic zählt: den
> **Redis-gecachten** Pfaden (**25.546** vs Gos 20.929 beim Index; 35.781 vs
> 29.470 beim Detail) und **reiner Berechnung** (14.341 vs 11.573). Es hielt außerdem den
> **geringsten RAM unter Last** (18,5 MiB, kein GC) und — anders als die Go-stdlib-Anwendung —
> liefert ein vollständiges, batteries-included Framework. Selbst das schnellste nicht kompilierte
> Ergebnis, **Laravel auf Octane**, liegt um das 4- bis 7-Fache hinter beiden Binaries.

[![Requests/Sek. auf dem nicht gecachten Blog-Index über alle sechs Laufzeiten — Go 6.651, Rustango 4.781, Laravel+Octane 1.238, Django+Hypercorn 910, Django+gunicorn 850, Laravel+php-fpm 408](../img/benchmarks.png)](../img/benchmarks.png)

---

## Der Aufbau

Vier Blog-Anwendungen — **Autoren, Beiträge, Tags (Viele-zu-Viele) und Kommentare** —
die HTML-Seiten rendern:

| Route | Rendert | Gecacht? |
|---|---|---|
| `GET /` | die neuesten 20 Beiträge, jeder mit Autor, Tags, Kommentaranzahl | nein |
| `GET /cached` | dasselbe wie `/` | Redis, 60 s |
| `GET /post/{slug}` | Beitragsrumpf + Autor + Tags + jeder Kommentar | nein |
| `GET /post/{slug}/cached` | dasselbe wie Detail | Redis, 60 s |
| `GET /tag/{slug}` | Beiträge, die einen Tag tragen | nein |
| `GET /compute` | Summe aller Primzahlen unter 20.000 (CPU-gebunden; keine DB, kein Cache) | nein |

Die ersten fünf sind I/O- + Render-gebunden; `/compute` ist eine reine CPU-Arbeitslast — der
identische Algorithmus der Probedivision in jeder Sprache — um die rohe Laufzeit-Geschwindigkeit
zu isolieren. Jede Anwendung lädt ihre Relationen **eifrig** (kein N+1): **Rustango** bündelt die
Abfragen explizit, Django verwendet `select_related` / `prefetch_related` /
`annotate(Count)`, Laravel verwendet `with()` + `withCount()`, **Go** lädt im Batch
mit `= ANY($1)`-Abfragen. Die Templates sind bewusst winzig und äquivalent (Tera,
Django-Templates, Blade, Gos `html/template`), sodass wir das *Framework* messen, nicht den
Template-Aufwand.

### Was es zu einem fairen Kampf macht

- **Identische Daten.** Ein PostgreSQL-Schema und ein deterministischer Seed, geteilt von allen
  vier Anwendungen — sie lesen dieselben *Tabellen*: 10 Autoren, 30 Tags, **1.000 Beiträge**,
  2.600 Beitrag-Tag-Verknüpfungen, **10.000 Kommentare**. Der Index zeigt dieselben 20 Beiträge in
  derselben Reihenfolge auf jedem Framework, und das gerenderte HTML ist byte-identisch
  (abgesehen vom Entity-Escaping-Stil jeder Engine).
- **Identisches Hardware-Budget.** Jede Anwendung läuft in einem Container, gedeckelt auf
  **4 CPUs / 2 GB RAM**. PostgreSQL und Redis werden geteilt und sind identisch.
- **Eine nach der anderen.** Die Anwendungen werden sequenziell lastgetestet, sodass sie nie um den
  Host konkurrieren (eine 12-Kern- / 18-GB-Maschine). Lastgenerator:
  [`oha`](https://github.com/hatoo/oha), 50 gleichzeitige Verbindungen, 10 s pro
  Endpunkt, HTTP-Keep-Alive an. Jeder Durchlauf meldete **100 % Erfolg**.
- **Alles im Produktionsmodus.** Das zählt sehr (siehe unten).

### Produktionskonfiguration

| | Laufzeit | Produktionshärtung |
|---|---|---|
| **Rustango** | ein `--release`-Binary (axum + Tokio, async, alle Kerne) | `opt-level=3` + LTO; Redis-Seiten-Cache via `CachePageLayer` |
| **Go** | ein statisches Binary (stdlib `net/http`, Goroutinen, alle Kerne) | `pgx`-Pool + `go-redis`-Seiten-Cache; Templates eingebettet; ausgeliefert auf `scratch` |
| **Django 5.2** · gunicorn | gthread-Worker + Keep-Alive (WSGI) | `DEBUG=False`; integrierter Redis-Cache + `@cache_page`; persistente DB-Verbindungen |
| **Django 5.2** · Hypercorn | ASGI-Server, 4 Worker, Keep-Alive | dieselbe Anwendung, über ASGI ausgeliefert; die synchronen Views laufen in einem Threadpool |
| **Laravel 13** · php-fpm | php-fpm + nginx, OPcache **an**, 16 Worker | `APP_ENV=production`; `composer install --no-dev --optimize-autoloader`; Blade gecacht; `Cache::remember` auf Redis |
| **Laravel 13** · Octane | Octane + **Swoole**, persistente Worker | dieselbe Anwendung auf einer speicherresidenten Laufzeit — kein Framework-Bootstrap pro Anfrage |

> Der Produktionsmodus ist keine Fußnote. Laravels erster (ungetunter) Durchlauf schaffte nur
> ~53 Req/s; das Aktivieren von OPcache, dem optimierten Autoloader und einem ordentlichen Worker-
> Pool brachte ihn auf ~408 Req/s — ein **7,7-facher Ausschlag** allein durch Konfiguration. Die
> Zahlen unten stammen alle aus der Produktionskonfiguration.

---

## Ergebnisse

### Durchsatz — Requests pro Sekunde (höher ist besser)

Die beiden kompilierten Binaries, dann jedes interpretierte Framework in seiner konventionellen
Laufzeit **und** seiner robusten Laufzeit:

| Endpunkt | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Index, nicht gecacht | 4 781 | **6 651** | 850 | 910 | 408 | 1 238 |
| Index, **gecacht** | **25 546** | 20 929 | 4 841 | 1 537 | 1 224 | 5 777 |
| Detail, nicht gecacht | 6 538 | **13 921** | 1 983 | 916 | 464 | 1 790 |
| Detail, **gecacht** | **35 781** | 29 470 | 4 843 | 1 320 | 793 | 5 811 |
| Tag, nicht gecacht | 3 926 | **4 353** | 1 129 | 1 033 | 398 | 1 179 |
| **compute** (CPU-gebunden) | **14 341** | 11 573 | 452 | 400 | 716 | 1 504 |

Drei Geschichten stechen hervor. Erstens **gruppieren sich Go und Rustango weit über allen
anderen** — die Kluft zwischen den beiden nativen Binaries (zehn Prozent, mit Go vorn
bei nicht gecachtem I/O und Rustango vorn bei gecachten Treffern + Berechnung) ist klein neben der
5- bis 30-fachen Kluft zu den interpretierten Laufzeiten. Zweitens ist **Laravel + Octane** (Swoole)
ein **3- bis 7-facher Sprung** gegenüber php-fpm — ein residenter Worker, der Laravels Framework-
Bootstrap pro Anfrage überspringt — und das schnellste nicht kompilierte Ergebnis auf jeder Seite.
Drittens ist **Django + Hypercorn** (ASGI) grob **flach und langsamer auf den gecachten
Pfaden**: die Views des Blogs sind *synchron*, sodass ASGI nur einen Threadpool-Sprung hinzufügt
ohne einen der Nebenläufigkeitsgewinne, die *async*-Views bringen würden. Selbst das Beste
dieses Felds (Octane, 1.238 Req/s nicht gecachter Index; 5.811 bei gecachtem Detail) liegt
um das 4- bis 7-Fache hinter beiden Binaries.

### Latenz — p50 in Millisekunden (niedriger ist besser)

| Endpunkt | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Index, nicht gecacht | 10.2 | **7.2** | 56.9 | 43.2 | 114.7 | 39.9 |
| Index, gecacht | **1.8** | 2.1 | 9.3 | 5.0 | 20.2 | 7.6 |
| Detail, gecacht | **1.3** | 1.5 | 5.8 | 32.1 | 81.8 | 7.6 |
| compute (CPU-gebunden) | 3.5 | **2.5** | 70.8 | 130.0 | 87.4 | 32.9 |

(Mediane gezeigt; die vollständigen p50 / p95 / p99 für jeden Endpunkt und jede Laufzeit stehen in
`bench/results/summary.tsv`.) Die Mediane von **Rustango** und **Go** auf dem
nicht gecachten Index (10,2 / 7,2 ms) liegen unter denen jedes interpretierten Konkurrenten, und
ihre gecachten Mediane (1,3–2,1 ms) sind eine Größenordnung unter selbst der
schnellsten Framework-Laufzeit. Ein Vorbehalt zu Gos Gunsten *und* zu seinen Ungunsten: sein
Compute-p50 (2,5 ms) schlägt Rustangos, aber sein Compute-p99 springt auf ~47 ms bei einer GC-
Pause — ein Schwanz, den das GC-lose Rust-Binary nicht hat.

### Fußabdruck — Image-Größe, Speicher, CPU

| | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Container-Image (unkomprimiert) | 164 MB | **18.5 MB** | 293 MB | 293 MB | 959 MB | 1.01 GB |
| RAM, im Leerlauf | 12.1 MiB | **5.2 MiB** | 128 MiB | 173 MiB | 92 MiB | 248 MiB |
| RAM, unter Last | **18.5 MiB** | 34.7 MiB | 218 MiB | 277 MiB | 133 MiB | 267 MiB |
| CPU unter Last (von 400 %-Deckel) | 295 % | 366 % | 356 % | 408 % | 406 % | 335 % |

**Go** liefert das kleinste Image — ein statisches Binary auf `scratch`, **18,5 MB** — und
läuft im Leerlauf bei nur **5,2 MiB**. Aber unter Last wächst sein GC-Heap auf **34,7 MiB**,
~1,9× die konstanten **18,5 MiB** von **Rustango**: ohne Garbage Collector und ohne
Allokation pro Anfrage passt das Rust-Binary unter Volllast in weniger RAM, als Go
benutzt, und unter das, was jede interpretierte Laufzeit *im Leerlauf* verbraucht. Die robusten
Laufzeiten kosten *mehr* Speicher, nicht weniger: Octane hält ein residentes Laravel in jedem
Worker; Hypercorn legt den ASGI-Stack obendrauf auf Django.

### Effizienz — geleistete Arbeit pro Ressource (die eigentliche Geschichte)

Roher Durchsatz ist eine Sache; **Durchsatz pro Ressourceneinheit** ist das, was Ihre
Cloud-Rechnung tatsächlich verfolgt (nicht gecachter Index):

| Metrik | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Requests/Sek. **pro MiB RAM** | **258** | 192 | 3.9 | 3.3 | 3.1 | 4.6 |
| Requests/Sek. **pro CPU-%** | 16.2 | **18.2** | 2.4 | 2.2 | 1.0 | 3.7 |

Pro Megabyte Speicher leistet **Rustango** die meiste Arbeit aller Laufzeiten hier —
~35× das beste interpretierte Ergebnis und ~1,3× Gos, weil sein Fußabdruck unter Last flach
bleibt. Pro CPU-Prozent zieht **Go** knapp vorbei (es wandelt die zusätzlichen Kerne, die es
hochfährt, in etwas mehr Durchsatz um). So oder so: um den Index-Durchsatz eines einzigen kompilierten
Binaries zu erreichen, müssten Sie ~4 Octane-Laravels oder ~6 gunicorn-Djangos betreiben — jedes
mit seinem eigenen mehrhundert-MB-Fußabdruck.

---

## Was der Cache verändert

Jede Laufzeit ist am schnellsten, wenn ein Redis-Seiten-Cache es ihr erlaubt, die Datenbank und
das Rendern gänzlich zu überspringen — nicht gecachte → **gecachte** Index-Req/s:

- **Rustango**: 4 781 → **25 546** (5,3× durch Caching) — erobert den ersten Platz zurück.
- **Go**: 6 651 → **20 929** (3,1×) — führt ungecacht, zweiter gecacht.
- **Laravel · Octane**: 1 238 → **5 777** (4,7×) — das Beste des interpretierten Feldes.
- **Django · gunicorn**: 850 → **4 841** (5,7×).
- **Django · Hypercorn**: 910 → **1 537** (1,7×) — der Overhead des ASGI-Threadpools
  deckelt den Gewinn selbst bei Cache-Treffern.
- **Laravel · php-fpm**: 408 → **1 224** (3,0×).

Caching hilft allen, aber es tilgt die Kluft nicht — und hier trennen sich die beiden
kompilierten Binaries: ohne laufenden Anwendungscode ist der
HTTP-Accept → Cache-Read → Response-Pfad alles, was übrig bleibt, und der
allokationsfreie `CachePageLayer` von **Rustango** (25.546 Req/s) zieht am `go-redis`-
Pfad von Go (20.929) vorbei, beide bei ~4× der besten Framework-Laufzeit.

---

## Rohe Berechnung: kompiliert vs interpretiert (und kompiliert vs kompiliert)

Die fünf Seitenrouten werden von der Datenbank und der Template-Engine dominiert. Die
`/compute`-Route entfernt diese — sie summiert jede Primzahl unter 20.000 per Probedivision,
der *identische* Algorithmus in Rust, Go, Python und PHP. Alle vier geben
dieselbe Antwort zurück (`21171191`); nur die Geschwindigkeit unterscheidet sich:

| | **Rustango** | **Go** | Django | Laravel |
|---|--:|--:|--:|--:|
| Durchsatz | **14 341 req/s** | 11 573 | 452 | 716 |
| p50-Latenz | 3.5 ms | **2.5 ms** | 70.8 ms | 87.4 ms |

Die beiden nativen Binaries durchlaufen die Schleife **~26- bis 32-mal** schneller als Django und
**~16- bis 20-mal** schneller als Laravel — die Kluft zwischen kompiliertem Maschinencode und einem
Bytecode-Interpreter. Zwischen Rust und Go holt sich Rusts LTO-optimierte `--release`-Schleife die
Durchsatz-Krone, während Gos Median-Latenz tatsächlich niedriger ist; Gos GC zeigt sich
dann als Schwanz-Latenz (p99 ~47 ms), die das Rust-Binary nie zahlt. Interessanterweise übertrifft PHP 8.3
(mit OPcache) CPython bei dieser engen Integer-Schleife rechnerisch, sodass Laravel Django hier
*rechnerisch übertrifft*, obwohl es auf jeder I/O-gebundenen Seite verliert. Dies ist
die Arbeitslast, bei der die Sprache, nicht das Framework, dominiert — und wo das Verlagern
heißer Logik nach **Rustango** sich am meisten auszahlt.

---

## Ehrliche Vorbehalte

Ein Benchmark, in den man keine Löcher bohren kann, ist die Veröffentlichung nicht wert. Also:

- Die Zahlen stammen von **einem** 12-Kern- / 18-GB-Host (macOS + Docker). Absolute
  Werte verschieben sich auf anderer Hardware; das **relative** Bild ist es, was Bestand hat.
- Dies ist eine **lese-lastige, serverseitig gerenderte** Arbeitslast — die häufigste Blog-
  Form. Sie misst keine Schreibvorgänge, Auth-Flows, Websockets oder schwere
  Geschäftslogik.
- **Go ist hier die Standardbibliothek, kein ebenbürtiges Framework.** Rustango, Django
  und Laravel sind batteries-included Frameworks (ORM, Admin, Migrationen, Routing,
  Templating, Multi-Tenancy); die Go-Anwendung ist handgeschriebenes `net/http` + rohes SQL —
  die schlankeste, schnellste Basislinie, die ein Go-Dienst realistisch erreicht, und
  die fairste Repräsentation der Sprache. Dass sie Rustango beim rohen ungecachten
  Durchsatz erreicht oder schlägt, ist genau der Punkt: **Rustango liefert Go-Klasse-Leistung
  mit einer Entwicklererfahrung der Django/Laravel-Klasse.** Gos Vorsprung bei den ungecachten
  Endpunkten wird damit erkauft, SQL, Mapping und Verdrahtung selbst zu schreiben.
- Die Laufzeiten sind grundlegend verschieden: Rustango und Go sind jeweils ein Binary,
  das alle Kerne mit billigen Tasks nutzt; Django und Laravel verwenden feste Pools von Worker-
  Prozessen/Threads. Dieser Unterschied *ist* Teil des Ergebnisses, und die
  Worker-Anzahlen wurden auf sinnvolle Pro-CPU-Werte gesetzt, nicht getunt, um irgendjemanden zu bevorzugen.
- Django und Laravel werden jeweils in **beiden** Laufzeiten gezeigt — konventionell
  (gunicorn, php-fpm) und robust (Hypercorn, Octane). **Laravel auf Octane ist
  3- bis 7-mal schneller** als php-fpm; **Django auf Hypercorn** (synchrone Views) ist grob
  flach. Beide verengen die Kluft zu den kompilierten Binaries; keines schließt sie.

Der Punkt ist nicht, dass Django oder Laravel langsam sind — sie treiben einen riesigen Teil des
Webs an. Der Punkt ist, dass **Rustango** Ihnen dieselbe batteries-included Entwicklererfahrung mit
der Leistung und dem Fußabdruck von kompiliertem Rust gibt — auf Augenhöhe mit einem handgetunten
Go-Dienst, während es Ihnen das Framework in die Hand gibt, das Go Sie bauen lässt.

---

## Reproduzieren

Das vollständige Harness — die vier Anwendungen, das geteilte PostgreSQL-Schema + der deterministische
Seed, das Docker-Compose-Setup und der Runner — ist ein eigenständiges Projekt
(`rustango-bench`). Aus seinem Verzeichnis:

```sh
bench/vendor.sh                       # vendor the framework into the build
docker compose build                  # build all six images
DURATION=10s CONCURRENCY=50 bench/run.sh
```

Voraussetzungen: Docker + Compose und Rust (für `cargo install oha`). Passen Sie den
Hardware-Deckel in `.env` (`CAP_CPUS`, `CAP_MEM`) und die Last mit `DURATION` /
`CONCURRENCY` an. Die rohe Ausgabe pro Durchlauf landet in `bench/results/`.


---

## Siehe auch

- [Getting started](getting-started.md)
- [ORM cookbook](orm.md)
