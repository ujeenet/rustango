# Benchmarks : Rustango vs Go vs Python vs Laravel vs WordPress

À quel point **Rustango** est-il rapide ? Cette page compare le même blog sur dix
runtimes : **Go** (`net/http` de la bibliothèque standard + `pgx`), **Rustango**
sur axum et sur actix-web, **axum** et **actix-web** seuls avec `sqlx` brut, un
framework **Python** sur gunicorn et sur Hypercorn, **Laravel** sur php-fpm et
sur Octane, et **WordPress**. Chaque application rend le même HTML à partir des
mêmes données.

Tous les chiffres ont été mesurés le **13 octobre 2026** avec rustango 0.60.6,
dans un seul harnais, sur une seule machine, selon les mêmes règles.

> **En bref.** Avec exactement le SQL de Go, Rust seul (axum + sqlx) bat Go de
> **34 %** sur l'index non caché, fait jeu égal sur la page de tag et reste
> **18 %** derrière sur la page de détail non cachée. Sur les pages cachées en
> Redis, chaque application Rust sert **1,5 à 2,2×** les requêtes de Go avec un
> p99 proche de **0,45 ms** (Go : 2,0 à 2,4 ms). **Rustango** coûte **5 à 32 %**
> par rapport à sqlx brut sur le même serveur et les mêmes requêtes, ce qui le
> place au niveau de Go sur l'index et 22 % derrière Go sur le détail. Les
> runtimes interprétés sont **5 à 40×** derrière sur les pages non cachées ; les
> plus rapides d'entre eux sont l'application Python sur le détail (2 865 req/s)
> et Laravel sur Octane sur l'index (1 098). Toutes les applications Rust tournent
> avec **7 à 8 Mio** sous charge ; Go avec 17 à 21 Mio ; les runtimes interprétés
> avec 89 à 296 Mio.

---

## Le dispositif

Un blog avec **auteurs, articles, tags (plusieurs-à-plusieurs) et commentaires** :

| Route | Rend | Caché ? |
|---|---|---|
| `GET /` | les 20 derniers articles, chacun avec auteur, tags, nombre de commentaires | non |
| `GET /cached` | identique à `/` | Redis, 60 s |
| `GET /post/{slug}` | corps de l'article + auteur + tags + tous les commentaires | non |
| `GET /post/{slug}/cached` | identique au détail | Redis, 60 s |
| `GET /tag/{slug}` | les 20 derniers articles portant un tag | non |
| `GET /compute` | somme de tous les nombres premiers en dessous de 20 000 (CPU seul, sans BDD) | non |

Le seed est déterministe : 10 auteurs, 30 tags, **1 000 articles**, 2 600 liens
article-tag et **10 000 commentaires**. Les relations sont chargées de manière
avide (pas de N+1). `/compute` exécute la même boucle de division par essais dans
chaque langage.

### Les applications

| Application | Serveur | Accès aux données | Cache de page |
|---|---|---|---|
| **Go** | `net/http` stdlib | pool `pgx`, SQL brut | `go-redis` GET/SET |
| **rustango · axum** | axum 0.8 | ORM rustango | `CachePageLayer` |
| **rustango · actix** | actix-web 4 | ORM rustango | GET/SET Redis écrit à la main |
| **axum + sqlx** | axum 0.8 | `sqlx` brut, texte SQL de Go | GET/SET Redis écrit à la main |
| **actix + sqlx** | actix-web 4 | `sqlx` brut, texte SQL de Go | GET/SET Redis écrit à la main |
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI) | l'ORM du framework, pool psycopg intégré | cache Redis du framework |
| **Python · Hypercorn** | Hypercorn, 4 workers (ASGI, vues synchrones) | identique | identique |
| **Laravel · php-fpm** | nginx + php-fpm, 16 processus enfants statiques | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 workers | identique | identique |
| **WordPress** | nginx + php-fpm, 16 processus enfants statiques | `WP_Query`, `get_the_tags`, `get_comments` sur **MySQL 8.4** | drop-in `advanced-cache.php`, phpredis |

Templates : Tera pour les applications Rust, `html/template` pour Go, le moteur
propre du framework Python, Blade pour Laravel, un petit thème pour WordPress.

### Ce qui rend la comparaison équitable

- **Même budget CPU.** Chaque application reçoit 4 CPU (cœurs 0 à 3, `cpus: 4`,
  2 Go de RAM). Go tourne avec `GOMAXPROCS=4`, Tokio et actix-web avec 4 workers.
- **Même pool.** Chaque application compilée a 16 connexions PostgreSQL, 4 au
  minimum. L'application Python utilise le pool intégré de son framework, 4 par
  processus worker (16 par application).
- **Mêmes requêtes.** Les applications compilées suivent toutes le plan de Go :
  3 requêtes pour l'index, 3 pour le détail, 4 pour la page de tag. Les
  applications Rustango les construisent avec l'ORM ; les applications Rust seules
  reprennent le texte SQL de Go tel quel. Les applications interprétées utilisent
  le chargement avide habituel de leur ORM. WordPress exécute 11 à 13 requêtes par
  page, dont 3 sont ses propres requêtes de démarrage.
- **Même sortie.** Avant toute charge, chaque endpoint de chaque application est
  récupéré et comparé au HTML de Go ; il doit correspondre après normalisation des
  espaces et du style d'entités, sinon l'exécution s'arrête. Les réponses ne sont
  compressées nulle part (`Accept-Encoding: identity`, gzip désactivé dans nginx),
  donc chaque application envoie la même page d'index d'environ 6,4 Ko.
- **Mode production.** Debug désactivé partout. PHP : OPcache activé avec la
  vérification des horodatages désactivée, `php artisan optimize` (config, routes,
  événements et vues en cache). Python : connexions persistantes issues du pool.
- **Des cœurs séparés pour le reste.** PostgreSQL 16, MySQL 8.4 et Redis 7
  tournent sur les cœurs 4 à 7, le générateur de charge sur les cœurs 8 à 11.
- **Générateur de charge dans Docker.** [`oha`](https://github.com/hatoo/oha)
  1.16.0 (épinglé par digest) tourne sur le réseau compose, et non via la
  redirection de ports de Docker Desktop. 50 connexions, keep-alive activé.
- **Exécutions répétées.** Par endpoint : 5 s de chauffe, puis 3 × 20 s
  d'exécution ; la médiane est rapportée. L'ordre des applications change à chaque
  tour. Seules les réponses HTTP 200 comptent pour les req/s.
- **Builds épinglés.** Rust : rustc 1.99.0, `opt-level=3`, fat LTO,
  `codegen-units=1`, `panic=abort`. Go 1.25.14. PHP 8.3.35, Laravel 13.36.0,
  Swoole 6.2.3, WordPress 6.9.10. Images de base par digest, fichiers de
  verrouillage versionnés dans le harnais.

Hôte : Apple M3 Pro, VM Docker avec 12 CPU et 16 Go. Go a tourné dans chaque lot
comme référence et est resté à moins de 6 % de sa première exécution.

---

## Runtimes compilés

### Débit : requêtes par seconde (plus haut est meilleur)

| Endpoint | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| index, non caché | 9 360 | 8 485 | 6 341 | **12 505** | 9 963 |
| index, caché | 96 214 | 164 942 | 200 167 | **214 097** | 197 252 |
| détail, non caché | **22 675** | 17 634 | 13 883 | 18 578 | 14 697 |
| détail, caché | 143 795 | 194 939 | 214 662 | **233 669** | 211 293 |
| tag, non caché | **6 457** | 5 577 | 5 146 | 6 299 | 5 903 |
| compute | **16 588** | 14 733 | 15 137 | 15 085 | 14 741 |

### Latence : p50 / p99 en ms (plus bas est meilleur)

| Endpoint | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| index, non caché | 5,11 / 9,31 | 5,81 / 8,00 | 7,78 / 10,20 | **3,93 / 5,80** | 4,95 / 6,99 |
| index, caché | 0,36 / 2,40 | 0,30 / 0,53 | 0,24 / 0,47 | **0,23 / 0,42** | 0,24 / 0,46 |
| détail, non caché | **1,91** / 4,97 | 2,78 / 4,32 | 3,54 / 5,19 | 2,64 / **4,12** | 3,34 / 4,88 |
| détail, caché | 0,24 / 2,01 | 0,25 / 0,47 | 0,22 / 0,46 | **0,21 / 0,40** | 0,22 / 0,47 |
| tag, non caché | **7,53** / 11,73 | 8,83 / 11,99 | 9,59 / 12,65 | 7,82 / **10,42** | 8,36 / 11,28 |
| compute | **2,36** / 10,84 | 3,40 / 6,07 | 3,32 / **3,58** | 3,33 / 5,97 | 3,39 / 3,65 |

### Empreinte

| | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| Image (non compressée) | **18,5 MB** | 147 MB | 146 MB | 143 MB | 143 MB |
| RAM, au repos | 5,1 MiB | 1,4 MiB | 1,7 MiB | **1,3 MiB** | 1,6 MiB |
| RAM, index sous charge | 21,2 MiB | 7,5 MiB | 7,3 MiB | 7,3 MiB | **6,9 MiB** |
| CPU, index sous charge (sur 400 %) | 296 % | 393 % | 296 % | 328 % | 272 % |

Go est livré sur `scratch` ; les images Rust sont `debian:bookworm-slim` plus le
binaire, donc l'essentiel de leur taille vient de l'image de base.

### Lire les chiffres

- **Rust vs Go, même SQL.** axum + sqlx mène sur l'index non caché (+34 %), Go
  mène sur le détail non caché (+22 %), et la page de tag est à égalité. Go sert
  aussi 10 % de requêtes compute en plus, mais son p99 compute (10,8 ms) vaut 2 à
  3× celui des applications Rust.
- **Pages cachées.** Sans travail de base de données restant, chaque application
  Rust sert 1,5 à 2,2× les requêtes de Go, et le p99 reste sous 0,55 ms.
- **La page de tag mesure PostgreSQL.** La base de données a utilisé 3,65 de ses
  4 cœurs sur cette route, donc les cinq applications se heurtent au même mur.
- **Rustango vs sqlx brut** (même serveur axum, mêmes requêtes) : −32 % sur
  l'index, −5 % sur le détail, −12 % sur le tag. Rustango est la seule application
  à utiliser tout son budget CPU (393 %). L'essentiel de l'écart vient d'une seule
  fonction ; voir [Coûts connus](#coûts-connus-dans-rustango).
- **actix-web vs axum.** Avec un pool sqlx partagé, les applications actix restent
  à 272 à 296 % de CPU et sont derrière axum sur les pages non cachées. La cause
  n'a pas encore été identifiée
  ([#2660](https://github.com/ujeenet/rustango/issues/2660)).

---

## Runtimes interprétés

### Débit : requêtes par seconde (plus haut est meilleur)

| Endpoint | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| index, non caché | 683 | 601 | 518 | **1 098** | 599 |
| index, caché | 11 052 | 2 744 | 3 326 | 5 884 | **21 285** |
| détail, non caché | **2 865** | 1 314 | 572 | 1 712 | 687 |
| détail, caché | 11 473 | 2 644 | 3 013 | 5 836 | **21 591** |
| tag, non caché | **1 337** | 786 | 490 | 1 102 | 647 |
| compute | 410 | 353 | **1 611** | 1 521 | 732 |

### Latence : p50 / p99 en ms (plus bas est meilleur)

| Endpoint | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| index, non caché | 74,5 / 137,7 | 96,4 / 213,0 | 95,5 / 122,6 | **44,1** / 110,1 | 78,1 / **103,9** |
| index, caché | 4,4 / 12,5 | 18,7 / 40,4 | 14,6 / 22,4 | 7,4 / 57,7 | **2,3 / 3,5** |
| détail, non caché | **14,2 / 43,0** | 35,3 / 76,2 | 86,5 / 113,4 | 27,9 / 96,2 | 70,5 / 99,0 |
| détail, caché | 4,1 / 11,4 | 19,0 / 48,2 | 16,4 / 21,6 | 7,4 / 58,2 | **2,3 / 3,6** |
| tag, non caché | **35,6 / 77,0** | 66,0 / 131,1 | 100,8 / 130,1 | 44,0 / 110,9 | 75,9 / 97,0 |
| compute | 117,9 / 222,5 | 129,6 / 245,3 | **31,3 / 38,7** | 32,7 / 78,5 | 67,0 / 89,0 |

### Empreinte

| | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| Image (non compressée) | **294 MB** | **294 MB** | 844 MB | 895 MB | 1,01 GB |
| RAM, au repos | 128 MiB | 172 MiB | **54 MiB** | 219 MiB | 61 MiB |
| RAM, index sous charge | 224 MiB | 296 MiB | **89 MiB** | 241 MiB | 142 MiB |
| CPU, index sous charge (sur 400 %) | 260 % | 323 % | 380 % | 337 % | 405 % |

### Lire les chiffres

- **Pages non cachées.** Le meilleur résultat interprété sur chaque page est 6 à
  11× derrière Rust seul et Go sur les pages d'index et de détail, et environ 5×
  sur la page de tag.
- **Pages cachées.** Le `advanced-cache.php` de WordPress sert un hit avant que
  WordPress ne se charge, donc il bat tous les autres runtimes interprétés
  (21k req/s). Le cache de l'application Python sur gunicorn vient ensuite (11k) ;
  Laravel exécute sa pile de middlewares avant `Cache::remember` (3 à 6k).
- **gunicorn vs Hypercorn.** Les vues sont synchrones, donc l'ASGI ajoute
  seulement un saut vers un threadpool : Hypercorn est 2× plus lent sur le détail
  et 4× plus lent sur les pages cachées que gunicorn avec la même application.
- **Octane vs php-fpm.** Un worker Swoole résident fait 2 à 3× php-fpm sur les
  pages non cachées et environ 1,8× sur les pages cachées, et fait jeu égal sur
  compute.
- **Compute.** PHP 8.3 avec OPcache exécute la boucle d'entiers environ 4× plus
  vite que CPython.
- **WordPress** tourne sur MySQL, pas sur PostgreSQL, donc ses chiffres non cachés
  mesurent en partie une autre base de données. L'une de ses trois exécutions de
  l'index non caché a échoué avec 14 253 erreurs de « connexion à la base de
  données » en 7 s (WordPress ouvre une connexion MySQL par requête) ; la médiane,
  599, vient d'une exécution propre.

---

## Ce qui a changé depuis la version précédente de cette page

La version de juin 2026 donnait Go à 6 651 req/s et Rustango à 4 781 sur l'index,
et Laravel, l'application Python et Hypercorn avec « 100 % de succès ». Plusieurs
de ces chiffres étaient faux.

**Runtimes compilés.** Deux écarts, chacun mesuré seul avec la méthode ci-dessus :

| Endpoint | Go, 12 threads | Go, 4 threads | rustango, ancien plan | rustango, plan de Go |
|---|--:|--:|--:|--:|
| index, non caché | 6 814 | 9 322 (+37 %) | 7 385 | 8 462 (+15 %) |
| détail, non caché | 19 184 | 22 125 (+15 %) | 12 546 | 17 594 (+40 %) |
| tag, non caché | 5 450 | 6 250 (+15 %) | 5 965 | 5 515 (−8 %) |
| compute | 9 263 | 16 599 (+79 %) | 14 714 | 14 710 (0 %) |

- **Le nombre de threads de Go.** Le module Go déclarait `go 1.23`, donc Go 1.25
  n'appliquait pas sa valeur par défaut de `GOMAXPROCS` adaptée aux conteneurs et
  tournait avec 12 threads sur un quota de 4 CPU. Le bridage CPU qui en résultait
  explique aussi l'ancien p99 compute de 47 ms, que l'ancienne page attribuait au
  ramasse-miettes.
- **Les requêtes de Rustango.** L'ancienne application exécutait 5 requêtes sur
  l'index et le détail et 7 sur le tag, en chargeant chaque auteur, chaque tag et
  le corps complet de chaque article. Elle suit maintenant le plan de Go.
- **Aussi corrigé, sans mesure séparée :** la taille du pool (10 par défaut pour
  sqlx contre 16 pour Go), rustango 0.43 → 0.60.6, le générateur de charge déplacé
  de l'hôte macOS vers le réseau Docker, l'épinglage des cœurs, et une seule
  exécution de 10 s remplacée par la médiane de trois exécutions de 20 s.

**Runtimes interprétés.**

- **« 100 % de succès » signifiait succès de connexion, pas HTTP 200.**
  L'exécution Hypercorn sur le détail en juin comptait 5 457 HTTP 500 pour 3 655
  bonnes réponses. Seul HTTP 200 compte désormais.
- **Hypercorn laissait fuir des connexions à la base de données.** Les connexions
  persistantes sous ASGI ouvraient une nouvelle connexion PostgreSQL pour la
  plupart des requêtes et atteignaient la limite de 100 connexions en quelques
  secondes. Il ouvrait aussi une nouvelle connexion Redis par requête. Il utilise
  maintenant le pool PostgreSQL du framework et un pool de connexions Redis
  partagé par worker.
- **Laravel manquait de ports.** predis ouvrait une nouvelle connexion Redis par
  requête ; après 20 s de charge, le conteneur n'avait plus de ports locaux et
  renvoyait des 500. Il utilise maintenant phpredis avec des connexions
  persistantes.
- **Laravel envoyait des pages compressées.** nginx et Octane compressaient la
  réponse en gzip, donc Laravel envoyait environ 700 octets là où les autres
  applications envoyaient 6,4 Ko. La compression est maintenant désactivée
  partout.
- **Les caches de production de Laravel manquaient.** Seules les vues Blade
  étaient en cache ; `php artisan optimize` met maintenant aussi en cache la
  config, les routes et les événements.

---

## Coûts connus dans Rustango

Ils sont apparus en s'alignant sur les requêtes de Go. C'est du travail pour le
framework, pas des problèmes du harnais. Les parts de CPU viennent de profils `perf`
de l'application rustango sous la même charge. Chacun a un ticket :

| Coût | Ticket |
|---|---|
| Les lignes `values_list` sur PostgreSQL essaient une dizaine de types par cellule, en construisant une erreur à chaque échec. **46 % du CPU de l'application** sur l'index non caché ; décoder une seule fois selon le type de colonne fait passer le décodage de la requête des posts de 87 µs à 5 µs. | [#2655](https://github.com/ujeenet/rustango/issues/2655) |
| `CachePageLayer` stocke le corps en base64 dans du JSON, donc chaque hit analyse du JSON et décode du base64 : **environ 21 % du CPU par hit**, 165k hits/s contre 214k pour un simple `GET` Redis. | [#2656](https://github.com/ujeenet/rustango/issues/2656) |
| `values()` et `annotate()` construisent pour chaque ligne une `HashMap` avec des clés `String` possédées : environ 6 à 7 % du CPU sur l'index non caché. | [#2661](https://github.com/ujeenet/rustango/issues/2661) |
| Chaque requête reconstruit et recompile les mêmes querysets : environ 2 % du CPU. | [#2662](https://github.com/ujeenet/rustango/issues/2662) |
| `.only()` et `values_list` ne peuvent pas sélectionner une colonne liée comme `author__name` ; reproduire les colonnes de Go demande un `Join` manuel avec `project`. | [#2657](https://github.com/ujeenet/rustango/issues/2657) |
| Il n'y a pas de bind de tableau `= ANY($1)` ; une liste `IN` devient `$1..$n`, donc chaque longueur de liste est sa propre requête préparée. Pas de coût CPU dans ce benchmark (moins de 0,5 %). | [#2658](https://github.com/ujeenet/rustango/issues/2658) |
| Il n'y a pas de prefetch plusieurs-à-plusieurs par lot pour une liste de lignes ; les tags arrivent par une jointure sur la table intermédiaire. | [#2659](https://github.com/ujeenet/rustango/issues/2659) |
| Avec un pool sqlx partagé, les applications actix-web utilisent moins de 300 % de leurs 4 CPU. Visible aussi sans rustango. | [#2660](https://github.com/ujeenet/rustango/issues/2660) |

---

## Limites de ce benchmark

- C'est une charge de travail **à forte lecture, rendue côté serveur**. Elle ne
  mesure pas les écritures, les flux d'authentification, les websockets ni une
  logique métier lourde.
- Un seul hôte (un portable M3 Pro avec 6 cœurs performance et 6 cœurs
  efficacité), qui faisait aussi d'autres tâches pendant les exécutions (charge
  moyenne de 3 à 4). Le tableau relatif reste valable ; les chiffres absolus
  seront différents sur un autre matériel.
- Go et les applications Rust seules sont des services écrits à la main.
  Rustango, l'application Python, Laravel et WordPress sont des frameworks ; les
  lignes Rustango vs sqlx brut montrent ce que cette couche coûte aujourd'hui.
- WordPress tourne sur MySQL parce qu'il ne prend pas en charge PostgreSQL.

---

## Reproduire

Le harnais est un projet autonome (`rustango-bench`). Depuis son répertoire :

```sh
bench/vendor.sh                 # vendor rustango at origin/develop into the build
docker compose --profile app build
bench/run.sh                    # 5 compiled apps; APPS="..." to pick others
```

Ajustez l'exécution avec `REPS`, `DURATION`, `WARMUP` et `CONCURRENCY`, et la
répartition CPU avec `CAP_CPUS` et les valeurs `CAP_CPUSET_*` dans `.env`. Le JSON
brut de `oha`, les fichiers de parité HTML et un `meta.txt` avec chaque version
arrivent dans `bench/results/`.

---

## Voir aussi

- [Getting started](getting-started.md)
- [ORM cookbook](orm.md)
