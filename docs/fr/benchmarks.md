# Benchmarks : Rustango vs Go vs Python vs Laravel vs WordPress

Le même blog servi par dix runtimes : **Go** (`net/http` de la bibliothèque
standard + `pgx`), **Rustango** sur axum et sur actix-web, **axum** et
**actix-web** seuls avec `sqlx` brut, un framework **Python** sur gunicorn et sur
Hypercorn, **Laravel** sur php-fpm et sur Octane, et **WordPress**. Chaque
application rend le même HTML à partir des mêmes données.

Mesuré le **13 octobre 2026** : rustango 0.60.6 dans les tableaux principaux,
rustango 0.60.7 dans [son propre tableau](#rustango-0607). Un seul harnais, une
seule machine, les mêmes règles pour chaque application.

## Résumé

- **Pages non cachées, applications compilées.** Rust seul (axum + sqlx) est
  34 % plus rapide que Go sur l'index, Go est 22 % plus rapide sur le détail, la
  page de tag est à égalité.
- **Rustango 0.60.7** sert 93 à 104 % de axum + sqlx seul sur l'index non caché
  et 94 à 100 % sur le détail, le tag et compute.
- **Pages cachées.** Chaque application Rust sert 1,5 à 2,2× les requêtes de Go,
  avec un p99 proche de 0,45 ms (Go : 2,0 à 2,4 ms).
- **Les runtimes interprétés** sont 5 à 40× derrière les applications compilées
  sur les pages non cachées. Sur les pages cachées, WordPress est le plus rapide
  d'entre eux (21k req/s).
- **Mémoire sous charge :** applications Rust 7 à 8 Mio, Go 17 à 21 Mio,
  runtimes interprétés 89 à 296 Mio.

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

### Règles du test

- **CPU et mémoire.** Chaque application reçoit 4 CPU (cœurs 0 à 3) et 2 Go de
  RAM. Go tourne avec `GOMAXPROCS=4`, Tokio et actix-web avec 4 workers.
- **Pool de base de données.** 16 connexions PostgreSQL par application
  (l'application Python : 4 par processus worker, 16 au total).
- **Requêtes.** Les applications compilées exécutent le même plan : 3 requêtes
  pour l'index, 3 pour le détail, 4 pour la page de tag. Rustango les construit
  avec l'ORM ; les applications Rust seules utilisent le texte SQL de Go. Les
  applications interprétées utilisent le chargement avide de leur ORM.
  WordPress exécute 11 à 13 requêtes par page.
- **Sortie.** Avant toute charge, chaque page de chaque application est comparée
  au HTML de Go ; une différence arrête l'exécution. Pas de compression, donc
  chaque application envoie la même page d'index d'environ 6,4 Ko.
- **Réglages de production.** Debug désactivé. PHP : OPcache activé,
  vérification des horodatages désactivée, `php artisan optimize`. Python :
  connexions issues du pool.
- **Autres services.** PostgreSQL 16, MySQL 8.4 et Redis 7 sur les cœurs 4 à 7 ;
  le générateur de charge [`oha`](https://github.com/hatoo/oha) 1.16.0 sur les
  cœurs 8 à 11, dans le réseau Docker. 50 connexions, keep-alive activé.
- **Exécutions.** Par endpoint : 5 s de chauffe, puis 3 × 20 s ; la médiane
  compte. Seules les réponses HTTP 200 comptent.
- **Builds.** rustc 1.99.0 (`opt-level=3`, fat LTO, `codegen-units=1`,
  `panic=abort`), Go 1.25.14, PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3,
  WordPress 6.9.10.

Hôte : Apple M3 Pro, VM Docker avec 12 CPU et 16 Go.

---

## Runtimes compilés

### Débit : requêtes par seconde (plus haut est meilleur)

Colonnes Rustango : 0.60.6.

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
binaire.

### Rustango 0.60.7

Mesuré dans une exécution séparée, avec axum + sqlx et rustango 0.60.6 mesurés
à nouveau à côté (req/s médianes) :

| Endpoint | axum + sqlx | rustango 0.60.6 | rustango 0.60.7 | 0.60.7 vs axum |
|---|--:|--:|--:|--:|
| index, non caché | 10 750 | 8 596 | **11 143** | 104 % |
| détail, non caché | 17 907 | 17 024 | 16 875 | 94 % |
| tag, non caché | 6 242 | 5 596 | 6 023 | 96 % |
| compute | 15 091 | 14 736 | 15 084 | 100 % |

L'index non caché a varié d'une exécution à l'autre ; en comparant la meilleure
exécution de chaque application, Rustango 0.60.7 atteint 93 % de axum + sqlx.

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

L'une des trois exécutions de WordPress sur l'index non caché a renvoyé 14 253
erreurs de « connexion à la base de données » en 7 s ; la médiane vient d'une
exécution propre.

---

## Pourquoi : ce qui est plus rapide, et où

Uniquement des faits mesurés ; quand une cause n'a pas été profilée, c'est
indiqué.

- **Page de tag : la base de données décide.** PostgreSQL a utilisé 3,65 de ses
  4 cœurs sur cette route, donc toutes les applications compilées restent à
  moins de 20 % les unes des autres.
- **Index non caché : Rust seul devance Go de 34 %.** Les trois mêmes requêtes,
  les mêmes lignes. Non profilé côté Go.
- **Détail non caché : Go devance Rust seul de 22 %.** La page rend un article
  avec tous ses commentaires. Non profilé.
- **Pages cachées : Rust 1,5 à 2,2× Go, p99 ~0,45 ms contre 2,0 à 2,4 ms.** Un
  hit, c'est une lecture Redis et une réponse ; pas de base de données, pas de
  template. La cause de la traîne plus large de Go n'a pas été profilée.
- **Compute : Go sert 10 % de requêtes en plus, Rust a une traîne plus courte**
  (p99 3,6 à 6,1 ms contre 10,8 ms) sur la même boucle de division par essais.
- **Mémoire : Rust 7 à 8 Mio, Go 17 à 21 Mio sous charge.** Le runtime de Go
  garde un tas géré par ramasse-miettes ; les binaires Rust libèrent la mémoire
  au fur et à mesure.
- **Rustango vs axum + sqlx seul.** En 0.60.6, 46 % du CPU de Rustango allait au
  décodage des résultats de requête, en essayant une dizaine de types par
  valeur. 0.60.7 décode chaque colonne une seule fois selon son type (4,9 % du
  CPU, sqlx brut : 5,0 %), ce qui l'amène à la vitesse de sqlx seul. Le reste du
  CPU est le même dans les deux applications : le rendu Tera (~26 %) et la
  vérification de connexion du pool (~18 %).
- **actix-web vs axum.** Avec un pool sqlx partagé, les applications actix
  n'utilisent que 272 à 296 % de 4 CPU et sont derrière axum sur les pages non
  cachées. Cause non trouvée
  ([#2660](https://github.com/ujeenet/rustango/issues/2660)).
- **Pages cachées, interprétées.** WordPress répond depuis Redis dans
  `advanced-cache.php`, avant que WordPress lui-même ne se charge (21k req/s).
  L'application Python et Laravel exécutent d'abord leur framework et leurs
  middlewares, puis lisent le cache (11k et 3 à 6k).
- **Laravel : Octane 2 à 3× php-fpm sur les pages non cachées.** php-fpm démarre
  le framework à chaque requête ; Octane le garde en mémoire.
- **Python : gunicorn 2 à 4× Hypercorn.** Les vues sont synchrones, donc sous
  ASGI chaque requête passe par un threadpool.
- **Compute, interprété.** PHP 8.3 avec OPcache exécute la boucle d'entiers
  environ 4× plus vite que CPython.
- **WordPress non caché** exécute 11 à 13 requêtes par page sur MySQL, contre 3
  à 4 sur PostgreSQL pour les autres applications.

---

## Profil CPU de Rustango

Profil `perf` de rustango 0.60.6 sous charge sur l'index non caché, avec le
statut dans 0.60.7 :

| Coût | Part du CPU | Ticket | Statut |
|---|---|---|---|
| Décoder chaque valeur en essayant une dizaine de types | 46 % | [#2655](https://github.com/ujeenet/rustango/issues/2655) | **Corrigé dans 0.60.7** (maintenant 4,9 %) |
| `CachePageLayer` stocke les pages en JSON + base64 | ~21 % d'un hit en cache | [#2656](https://github.com/ujeenet/rustango/issues/2656) | Ouvert |
| Une `HashMap` avec des clés possédées pour chaque ligne `values()` | 6 à 7 % | [#2661](https://github.com/ujeenet/rustango/issues/2661) | **Corrigé dans 0.60.7** (noms lus une seule fois) |
| Querysets reconstruits et compilés à chaque requête | ~2 % | [#2662](https://github.com/ujeenet/rustango/issues/2662) | Partiellement corrigé dans 0.60.7 |
| `.only()` / `values_list` ne peuvent pas sélectionner une colonne liée (`author__name`) | Manque d'API | [#2657](https://github.com/ujeenet/rustango/issues/2657) | Ouvert |
| Pas de bind de tableau `= ANY($1)` pour les listes `IN` | moins de 0,5 % | [#2658](https://github.com/ujeenet/rustango/issues/2658) | Ouvert |
| Pas de prefetch plusieurs-à-plusieurs par lot | Manque d'API | [#2659](https://github.com/ujeenet/rustango/issues/2659) | Ouvert |

---

## Limites

- Une charge de travail **à forte lecture, rendue côté serveur**. Pas
  d'écritures, de flux d'authentification, de websockets ni de logique métier
  lourde.
- Un seul hôte (portable M3 Pro, 6 cœurs performance + 6 cœurs efficacité). Les
  chiffres absolus seront différents sur un autre matériel.
- Go et les applications Rust seules sont des services écrits à la main ;
  Rustango, l'application Python, Laravel et WordPress sont des frameworks.
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
