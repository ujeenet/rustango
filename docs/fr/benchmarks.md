# Benchmarks : Rustango vs Go vs Python vs Laravel vs WordPress

Le même blog servi par chaque application, avec les mêmes données et le même
HTML. Deux campagnes à ce jour :

- **Campagne 2, 14 octobre 2026 : applications compilées, profilées.** **Go**
  (`net/http` stdlib + `pgx`) et sept applications Rust : **axum** et
  **actix-web** seuls avec `sqlx` brut, **Rocket**, **Loco**, **Cot**, et
  **Rustango** sur axum et sur actix-web. Seule l'application testée tourne.
  Des profils CPU, syscalls et protocole Postgres montrent où chaque
  application passe son temps.
- **Campagne 1, 13 octobre 2026 : tous les runtimes.** Go, Rustango et Rust
  seul, un framework **Python** sur gunicorn, Hypercorn, Uvicorn et Granian
  (serveurs Python mesurés le 14 octobre), **Laravel** sur php-fpm et sur
  Octane, et **WordPress**. Ses résultats compilés sont remplacés par la
  campagne 2 ; ses résultats interprétés sont plus bas.

Articles avec graphiques (en anglais) :
[Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled)
et [campagne 1](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress).

## Résumé

- **Pages non cachées.** Rustango sur actix-web et sur axum sont les deux
  plus rapides des huit applications compilées sur l'index, le détail et la
  page de tag (index : 17 401 et 14 686 req/s ; axum + sqlx 14 140, Go 8 741).
- **Pages cachées.** Chaque application Rust sert plus que Go : 138 224 à
  231 975 req/s sur l'index caché contre 91 839 pour Go. Le ramasse-miettes de
  Go en est la cause principale.
- **Compute.** Go est 8 à 17 % plus rapide que les applications Rust. Rust
  compilé avec l'ordre d'instructions de Go tourne dans le temps de Go.
- **Mémoire sous charge :** 6,3 à 12,0 Mio pour six des applications Rust,
  Cot 22,6 Mio, Go 22,8 Mio.
- **Les runtimes interprétés** (campagne 1) sont 4 à 40× derrière les
  applications compilées sur les pages non cachées. Sur les pages cachées,
  WordPress est le plus rapide d'entre eux (21k req/s). Côté Python, gunicorn
  (synchrone) bat les trois serveurs ASGI ; Granian est le serveur ASGI le
  plus rapide. Mémoire sous charge : 89 à 293 Mio.
- **Corrections de Rustango.** Les profils ont montré où Rustango perdait du
  temps ; 0.60.7 et 0.60.8 corrigent les gros coûts et 0.61.2 ajoute deux
  fonctions de requête manquantes (voir
  [le tableau](#rustango--constats-du-profil-et-état)).

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

### Règles du test

- **CPU et mémoire.** Chaque application reçoit 4 CPU (cœurs 0 à 3) et 2 Go de
  RAM. Go tourne avec `GOMAXPROCS=4`, Tokio et actix-web avec 4 workers.
- **Pool de base de données.** 16 connexions PostgreSQL par application
  (campagne 2 : 16 au maximum, 4 au minimum ; l'application Python : 4 par
  processus worker, 16 au total).
- **Requêtes.** Les applications compilées exécutent le même plan : 3 requêtes
  pour l'index, 3 pour le détail, 4 pour la page de tag (Cot : 5, 5 et 7).
  Rustango et Loco les construisent avec leur ORM ; les applications Rust
  seules utilisent le texte SQL de Go. Les applications interprétées utilisent
  le chargement avide de leur ORM. WordPress exécute 11 à 13 requêtes par page.
- **Sortie.** Avant toute charge, chaque page de chaque application est comparée
  au HTML de Go ; une différence arrête l'exécution. Pas de compression, donc
  chaque application envoie la même page d'index d'environ 6,4 Ko.
- **Réglages de production.** Debug désactivé. PHP : OPcache activé,
  vérification des horodatages désactivée, `php artisan optimize`. Python :
  connexions issues du pool, logs d'accès désactivés.
- **Autres services.** PostgreSQL 16, MySQL 8.4 (WordPress seulement) et
  Redis 7 sur les cœurs 4 à 7 ; le générateur de charge
  [`oha`](https://github.com/hatoo/oha) 1.16.0 sur les cœurs 8 à 11, dans le
  réseau Docker. 50 connexions, keep-alive activé.
- **Isolation (campagne 2).** Seule l'application testée tourne ; les autres
  sont arrêtées. Entre deux applications, le nombre de connexions Postgres
  revient à sa valeur de base.
- **Exécutions.** Par endpoint : 5 s de chauffe, puis 3 × 20 s ; la médiane
  compte. Seules les réponses HTTP 200 comptent. La campagne 2 a eu 0 erreur
  ou réponse non-200 sur 144 exécutions.

Hôte : Apple M3 Pro, VM Docker avec 12 CPU et 16 Go.

---

## Campagne 2 : runtimes compilés

Mesuré le **14 octobre 2026**.

### Les applications

| Application | Serveur | Accès aux données | Templates | Cache de page |
|---|---|---|---|---|
| **Go** | `net/http` stdlib | pool `pgx` v5, SQL brut | `html/template` | `go-redis` |
| **axum + sqlx** | axum | `sqlx` brut, texte SQL de Go | Tera | Redis écrit à la main |
| **actix + sqlx** | actix-web | `sqlx` brut, texte SQL de Go | Tera | Redis écrit à la main |
| **Rocket** | Rocket | `rocket_db_pools` (sqlx) | `rocket_dyn_templates` (Tera) | Redis |
| **Rustango · axum** | axum | ORM rustango | Tera | `CachePageLayer` |
| **Rustango · actix** | actix-web | ORM rustango | Tera | cache de page de rustango |
| **Loco** | Loco | SeaORM | Tera | Redis |
| **Cot** | Cot | ORM de Cot | askama | Redis |

### Versions

- Go 1.25.14, pgx v5.7.2, go-redis v9.7.0
- rustc 1.99.0 (`opt-level=3`, fat LTO, `codegen-units=1`, `panic=abort`) ;
  axum 0.8.9, actix-web 4.16.0, tokio 1.53, sqlx 0.8.6, Tera 1.20.1
- Rocket 0.5.1 (sqlx 0.7.4, hyper 0.14)
- Loco 1.2.0 (SeaORM 2.0.4, sqlx 0.9.0)
- Cot 0.7.0 (sqlx 0.9.0, askama 0.16.1)
- Rustango : un build de développement sans ping à l'acquisition, avec un
  cache de page qui stocke des octets bruts et une connexion du pool par
  requête. Ces trois changements sont livrés dans 0.60.8.

### Réglages qui n'ont pas pu être égalisés

| Réglage | Applications | Pourquoi |
|---|---|---|
| Le pool pingue Postgres à chaque checkout (sqlx `test_before_acquire`) | Rocket, Loco, Cot | Aucun réglage pour le désactiver. Il est désactivé dans axum + sqlx, actix + sqlx et Rustango. |
| 10 connexions de pool au lieu de 16 | Cot | Cot n'a pas de réglages de pool. |
| Une tranche de pool par worker (4 des 16 connexions) et une connexion Redis par worker | actix + sqlx, Rustango · actix | Un pool partagé est plus lent sur actix-web ([constat](#actix-web--un-pool-partagé-est-lent)). |
| sqlx 0.7.4 et hyper 0.14 | Rocket | Imposés par Rocket 0.5.1. |
| 5, 5 et 7 requêtes par page d'index, de détail et de tag au lieu de 3, 3 et 4 | Cot | Son API de requêtes ne peut pas exprimer les requêtes de Go. |
| askama au lieu de Tera | Cot | Le moteur de templates de Cot. |

### Débit : requêtes par seconde (plus haut est meilleur)

| Application | Index | Détail | Tag | Index, caché | Détail, caché | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 8 741 | 22 607 | 6 386 | 91 839 | 141 574 | **16 580** |
| axum + sqlx | 14 140 | 22 842 | 6 467 | 198 669 | 214 871 | 14 963 |
| actix + sqlx | 14 408 | 21 630 | 6 628 | 226 887 | 240 161 | 14 578 |
| Rocket | 13 316 | 24 561 | 6 836 | 138 224 | 155 691 | 14 696 |
| Rustango · axum | 14 686 | 25 788 | **7 090** | 174 501 | 192 897 | 14 799 |
| Rustango · actix | **17 401** | **27 292** | 7 056 | **231 975** | **242 609** | 14 710 |
| Loco | 10 290 | 15 635 | 5 604 | 153 243 | 170 158 | 14 184 |
| Cot | 2 018 | 9 282 | 3 065 | 190 245 | 203 119 | 15 330 |

Index, détail et tag ne sont pas cachés.

### Latence : p50 / p99 en ms (plus bas est meilleur)

| Application | Index | Détail | Tag | Index, caché | Détail, caché | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 5,50 / 9,96 | 1,91 / 5,02 | 7,61 / 11,72 | 0,35 / 2,73 | 0,24 / 2,05 | **2,37** / 10,75 |
| axum + sqlx | 3,49 / 4,71 | 2,14 / 3,60 | 7,42 / 13,89 | 0,24 / 0,53 | 0,22 / 0,51 | 3,35 / 6,07 |
| actix + sqlx | 3,32 / 6,38 | 2,21 / 4,30 | 7,23 / 13,23 | **0,20** / 0,50 | **0,19 / 0,45** | 3,41 / 4,97 |
| Rocket | 3,66 / 5,83 | 1,97 / 3,58 | 7,14 / 11,55 | 0,34 / 0,70 | 0,30 / 0,67 | 3,40 / 6,14 |
| Rustango · axum | 3,28 / 5,65 | 1,83 / 3,55 | **6,90 / 11,18** | 0,27 / 0,59 | 0,25 / 0,52 | 3,38 / 6,09 |
| Rustango · actix | **2,80 / 4,51** | **1,77 / 3,37** | 6,91 / 11,53 | **0,20 / 0,48** | **0,19 / 0,45** | 3,41 / **3,96** |
| Loco | 4,62 / 10,84 | 2,99 / 6,34 | 8,67 / 14,52 | 0,30 / 0,79 | 0,28 / 0,62 | 3,48 / 6,90 |
| Cot | 23,35 / 50,36 | 5,28 / 8,02 | 15,57 / 27,06 | 0,25 / 0,50 | 0,23 / 0,52 | 3,27 / 5,85 |

### Mémoire sous charge

Mémoire résidente du conteneur de l'application sur l'index non caché
(`docker stats`, l'échantillon au CPU le plus élevé parmi six pris pendant la
première exécution de l'index) :

| | Go | axum + sqlx | actix + sqlx | Rocket | Rustango · axum | Rustango · actix | Loco | Cot |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| RAM, Mio | 22,8 | 7,6 | 7,2 | 8,7 | 6,9 | **6,3** | 12,0 | 22,6 |

Tailles d'image de la campagne 1 (non compressées) : Go 18,5 Mo sur
`scratch`, les applications Rust 143 à 147 Mo sur `debian:bookworm-slim`.

### Rustango de la campagne 1 à la campagne 2

Rustango · actix, index non caché, req/s :

| Exécution | req/s |
|---|--:|
| Campagne 1 (0.60.6) | 6 341 |
| Corrections, pool partagé (exécution de profilage) | 12 052 |
| Corrections, une tranche de pool par worker (campagne 2) | **17 401** |

Rustango · axum avant → après les corrections, exécutions de profilage :

| | Avant | Après |
|---|--:|--:|
| Index, req/s | 11 215 | **15 849** |
| Détail, req/s | 17 267 | **28 097** |
| Index, CPU de l'application par requête | 286,6 µs | **220,8 µs** |
| Détail, CPU de l'application par requête | 164,7 µs | **110,4 µs** |

---

## Pourquoi : ce qui est plus rapide, et où

Uniquement des faits mesurés ; quand une cause n'est pas prouvée, c'est
indiqué. Les profils sont des exécutions uniques de 25 s, une application à
la fois, donc leurs req/s diffèrent un peu des tableaux ci-dessus.

### Compute : Go est 8 à 17 % plus rapide

Les deux boucles compilées ont 8 instructions, un `sdiv` 64 bits et un test
de division par zéro ; LLVM choisit un autre ordre d'instructions. Seule sur
un cœur, hors HTTP, un appel prend 194 µs en Go, 224 µs en Rust et 194 µs en
Rust avec exactement l'ordre d'instructions de Go. Quel détail du CPU rend cet
ordre plus lent n'est pas prouvé : la VM Docker n'a pas de compteurs
matériels. Le reste de chaque requête coûte moins en Rust (8 µs contre 14 µs
pour Go).

Go a le p50 le plus bas mais le p99 le plus haut : il sert les requêtes dans
le désordre (p10 0,56 ms, p99 10,75 ms), les applications Rust presque dans
l'ordre d'arrivée (axum + sqlx : p10 1,85 ms, p99 6,07 ms). Pourquoi le p99 de
Go se situe vers 10 ms n'est pas prouvé.

### Pages cachées : le ramasse-miettes de Go

Go effectue 441 collectes par seconde sur l'index caché ; les pauses
stop-the-world totalisent environ 10 % du temps. Augmenter `GOGC` comble
l'essentiel de l'écart (exécutions de profilage, `GOGC=100` par défaut :
97 289 req/s) :

| Go, index caché | req/s | p99 |
|---|--:|--:|
| `GOGC=100` (défaut) | 97 289 | 2,48 ms |
| `GOGC=400` | 144 083 | |
| GC presque désactivé | 159 734 | 0,90 ms |

Le reste est du CPU par requête :

| Index caché, par requête | Go | axum + sqlx |
|---|--:|--:|
| CPU de l'application | 27,2 µs | **14,6 µs** |
| Chemin d'écriture du noyau | 10,6 µs | **6,9 µs** |
| Syscalls | 10,7 (6,6 read, 2,9 write) | **4,8** (2,0 recvfrom, 1,4 writev) |
| CPU de Redis | 6,8 µs | **2,5 µs** |
| Trafic Redis | go-redis : un GET par requête | une connexion, les GET de nombreuses requêtes regroupés dans un pipeline |

### Page de détail : allers-retours Postgres

Toutes les applications envoient les trois mêmes requêtes, mais le nombre
d'allers-retours diffère. pgx en envoie un par requête. sqlx en ajoute un
quand une connexion est prise dans le pool (le ping à l'acquisition) et un
quand elle y retourne. axum + sqlx prend une connexion par requête, donc paie
par requête ; Rocket et Rustango prennent une connexion par page, donc paient
une fois. Capture du protocole sous charge, messages ReadyForQuery comptés
par page de détail (l'index montre les mêmes chiffres) :

| Application | Allers-retours |
|---|--:|
| Go · pgx | **3** |
| Rustango | 4 |
| Rocket | 5 |
| axum + sqlx, ping désactivé | 6 |
| axum + sqlx, ping activé | 9 |

Ping à l'acquisition désactivé dans axum + sqlx (exécutions de profilage) :
détail 18 222 → 23 472 req/s (CPU Postgres 166,6 → 134,3 µs par requête),
index 12 140 → 14 419 req/s (237,9 → 208,5 µs). Trois allers-retours de moins
économisent 29 à 32 µs de CPU Postgres par page : environ 10 µs chacun. Go a
le moins d'allers-retours mais son ramasse-miettes l'arrête environ 9 % du
temps sur cette route.

### Page d'index : GC et rendu des templates

De nouveau le ramasse-miettes de Go (pauses environ 8 % du temps ;
`GOGC=400` porte l'index de 9 359 à 12 848 req/s, GC presque désactivé à
13 967), et le rendu des templates : `html/template` de Go utilise la
réflexion et coûte 167 µs par requête, serde + Tera 72 µs. Dans axum + sqlx,
sqlx coûte environ 112 µs par requête (dont environ 25 µs pour le ping quand
une connexion retourne au pool), malloc de la glibc environ 56 µs et `sendto`
du noyau 38 µs ; les parts se recouvrent.

### actix-web : un pool partagé est lent

Dans la campagne 1, les applications actix-web étaient derrière axum sur les
pages non cachées et utilisaient moins de 300 % de leurs 400 % de CPU. axum
fait tourner un runtime Tokio partagé et multi-thread ; actix-web un runtime
mono-thread par worker. Avec un pool partagé par tous les workers, chaque
requête réveille un thread d'un autre runtime. Une tranche de pool (4 des 16
connexions) et une connexion Redis par worker règlent le problème
(exécutions de profilage, req/s) :

| | Pool partagé | Pool par worker |
|---|--:|--:|
| actix + sqlx, index | 11 094 | **15 108** |
| actix + sqlx, détail | 18 080 | **23 330** |
| actix + sqlx, index caché | 192 604 | **238 884** |
| Rustango · actix, index | 12 052 | **17 454** |
| Rustango · actix, détail | 23 032 | **28 693** |
| Rustango · actix, index caché | 190 889 | **237 962** |

Les changements de contexte par requête baissent d'environ 2,5× (actix + sqlx
index 4,04 → 1,45, détail 3,57 → 1,64 ; Rustango · actix index 2,70 → 0,81,
détail 2,34 → 1,03). Plus de workers n'aident pas : 8 workers avec un pool
partagé atteignent 11 904 req/s sur l'index d'actix + sqlx. L'effet est
mesuré ; le mécanisme (un socket est piloté par le runtime qui l'a ouvert) est
la meilleure explication, sans preuve directe. **Sur actix-web, créez le pool
de base de données dans chaque worker, pas une fois pour tous les workers.**

### Rocket, Loco et Cot

- **Rocket** est fort sur les pages avec base de données (une connexion par
  page, 5 allers-retours) et l'application Rust la plus lente sur les pages
  cachées : 24,1 µs de CPU par requête contre 14,6 µs pour axum + sqlx, 8,1
  syscalls contre 4,8, un `writev` en plus (2,24 par requête contre 1,36) et
  de la contention dans malloc (futex 2,88 par requête contre 0,91).
- **Loco** : SeaORM envoie autant de requêtes que Go. Sur les pages cachées,
  Loco dépense 21,0 µs par requête : 2,4 µs de plus dans malloc et 1,2 µs de
  plus dans les middlewares qu'axum + sqlx. Sur les pages avec base de
  données, le ping à l'acquisition ne peut pas être désactivé ; ces pages
  n'ont pas été profilées, donc la part de l'écart qu'il explique n'est pas
  prouvée.
- **Cot** : l'API de requêtes n'a que filter, limit et offset (pas d'ORDER BY,
  JOIN, IN ni COUNT), donc l'index charge les 1 000 articles avec leur corps à
  chaque requête : 5, 5 et 7 requêtes au lieu de 3, 3 et 4, et 22,6 Mio sous
  charge. Les pages cachées évitent l'ORM et sont rapides (16,4 µs de CPU par
  requête).

### Coûts communs

Deux coûts apparaissent dans chaque application Rust : le rendu Tera
(environ 18 à 20 % du CPU sur les pages non cachées) et malloc de la glibc
(environ 12 %). Un allocateur plus rapide comme mimalloc ou jemalloc pourrait
réduire le second ; ce n'est pas testé.

### Runtimes interprétés (campagne 1)

- **Pages cachées.** WordPress répond depuis Redis dans `advanced-cache.php`,
  avant que WordPress lui-même ne se charge (21k req/s). Les applications
  Python et Laravel exécutent d'abord leur framework et leurs middlewares,
  puis lisent le cache (Python 5 à 12k, Laravel 3 à 6k).
- **Laravel : Octane 2 à 3× php-fpm sur les pages non cachées.** php-fpm démarre
  le framework à chaque requête ; Octane le garde en mémoire.
- **Python : les vues asynchrones ne battent pas gunicorn.** gunicorn sert
  1,4 à 2,3× les serveurs ASGI sur les pages cachées et 1,4 à 1,8× sur le
  détail et le tag non cachés ; sur l'index non caché, les quatre sont égaux
  (663 à 697). D'après le code du framework : l'ORM asynchrone exécute l'ORM
  synchrone dans un thread (`sync_to_async`), il n'y a pas de pool de base de
  données asynchrone, et le handler ASGI crée un exécuteur de threads par
  requête plus environ 3 sauts de thread (signaux de requête, fermeture de la
  réponse). Le chemin asynchrone du cache fait moins de travail que celui de
  gunicorn (un GET Redis de HTML brut contre deux GET et un unpickle) et reste
  plus lent. La part de chaque coût n'a pas été profilée.
- **Serveurs ASGI : Granian > Uvicorn > Hypercorn.** Index caché 7 706 /
  6 233 / 5 151 req/s, détail non caché 1 854 / 1 654 / 1 550. Non profilé.
- **Compute.** PHP 8.3 avec OPcache exécute la boucle d'entiers environ 4×
  plus vite que CPython.
- **WordPress non caché** exécute 11 à 13 requêtes par page sur MySQL, contre 3
  à 4 sur PostgreSQL pour les autres applications.

---

## Rustango : constats du profil et état

D'après les profils `perf` de Rustango sous charge : campagne 1 (0.60.6) et
les exécutions de profilage de la campagne 2.

| Coût | Part du CPU | État |
|---|---|---|
| Décoder chaque valeur en essayant une dizaine de types | 46 % de l'index | **Corrigé dans 0.60.7** : chaque colonne décodée une fois selon son type (maintenant 4,9 %) |
| Un checkout du pool pour chaque requête SQL | 19 à 35 % | **Corrigé dans 0.60.8** : `pool.acquire()` exécute les requêtes d'une requête HTTP sur une seule connexion (détail +26 %) |
| Ping vers Postgres à chaque checkout | 8 à 16 % | **Corrigé dans 0.60.8** : désactivé par défaut ; `pool_test_before_acquire` le réactive |
| `CachePageLayer` stocke les pages en JSON + base64 | ~12 à 20 % par hit en cache | **Corrigé dans 0.60.8** : pages stockées en octets bruts |
| Une `HashMap` avec des clés possédées pour chaque ligne `values()` | 6 à 7 % | Réduit dans 0.60.7 : noms lus une fois par jeu de résultats ; la clé possédée reste (type public) |
| Querysets reconstruits et compilés à chaque requête | ~2 % | Ouvert ; 0.60.7 quote les identifiants avec une seule allocation |
| `.only()` / `values_list` ne peuvent pas sélectionner une colonne liée (`author__name`) | Manque d'API | **Ajouté dans 0.61.2** |
| Pas de bind de tableau `= ANY($1)` pour les listes `IN` | moins de 0,5 % | **Ajouté dans 0.61.2** (PostgreSQL, listes d'un seul type) |
| Pas de prefetch plusieurs-à-plusieurs par lot | Manque d'API | Ouvert |
| Index caché : Rustango · axum 174 501 req/s contre 198 669 pour axum + sqlx | non profilé | Ouvert |

---

## Campagne 1 : runtimes interprétés

Les quatre serveurs Python viennent d'une même exécution (14 octobre 2026) ;
Laravel et WordPress du 13 octobre 2026. Mêmes règles que ci-dessus, avec
toutes les applications en marche.

### Les applications

| Application | Serveur | Accès aux données | Cache de page |
|---|---|---|---|
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI, vues synchrones) | l'ORM du framework, pool psycopg intégré | cache Redis du framework |
| **Python · Hypercorn** | Hypercorn, 4 workers, uvloop (ASGI, vues asynchrones) | l'API ORM asynchrone du framework, même pool | `redis.asyncio`, un GET par hit |
| **Python · Uvicorn** | Uvicorn, 4 workers, uvloop + httptools (ASGI, vues asynchrones) | identique | identique |
| **Python · Granian** | Granian, 4 workers, uvloop (ASGI, vues asynchrones) | identique | identique |
| **Laravel · php-fpm** | nginx + php-fpm, 16 processus enfants statiques | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 workers | identique | identique |
| **WordPress** | nginx + php-fpm, 16 processus enfants statiques | `WP_Query`, `get_the_tags`, `get_comments` sur **MySQL 8.4** | drop-in `advanced-cache.php`, phpredis |

Templates : le moteur propre du framework Python, Blade pour Laravel, un petit
thème pour WordPress. Les applications Python asynchrones utilisent des vues
`async def` sur l'API ORM asynchrone du framework, un wrapper de middleware
asynchrone et un pool `redis.asyncio` partagé de 16 par processus ; un hit en
cache est un GET Redis de HTML brut.

Versions : PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3, WordPress 6.9.10,
Uvicorn 0.54.0, Granian 2.8.3, uvloop 0.22.1, httptools 0.8.0.

### Serveurs Python

#### Débit : requêtes par seconde (plus haut est meilleur)

| Endpoint | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| index, non caché | **697** | 663 | 675 | 692 |
| index, caché | **11 012** | 5 151 | 6 233 | 7 706 |
| détail, non caché | **2 778** | 1 550 | 1 654 | 1 854 |
| détail, caché | **11 848** | 5 085 | 6 215 | 7 810 |
| tag, non caché | **1 411** | 906 | 948 | 1 020 |
| compute | **422** | 388 | 385 | 404 |

#### Latence : p50 / p99 en ms (plus bas est meilleur)

| Endpoint | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| index, non caché | **66,9** / 155,8 | 71,2 / 128,1 | 72,8 / **113,6** | 69,9 / 119,9 |
| index, caché | **4,9 / 10,1** | 9,3 / 23,5 | 7,1 / 19,6 | 6,4 / 15,4 |
| détail, non caché | **18,9 / 34,9** | 30,5 / 57,0 | 29,2 / 52,5 | 26,0 / 47,6 |
| détail, caché | **3,8 / 8,5** | 9,7 / 24,5 | 7,7 / 21,1 | 6,2 / 15,1 |
| tag, non caché | **27,9** / 89,0 | 52,3 / 95,0 | 52,4 / 84,4 | 49,0 / **80,3** |
| compute | **113,0** / 215,8 | 128,6 / 160,6 | 131,7 / **154,5** | 117,9 / 187,1 |

#### Empreinte

| | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| Image (non compressée, partagée) | 345 Mo | 345 Mo | 345 Mo | 345 Mo |
| RAM, index sous charge | **224 Mio** | 255 Mio | 249 Mio | 293 Mio |
| CPU, index sous charge (sur 400 %) | 244 % | 323 % | 314 % | 303 % |

### PHP

#### Débit : requêtes par seconde (plus haut est meilleur)

| Endpoint | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| index, non caché | 518 | **1 098** | 599 |
| index, caché | 3 326 | 5 884 | **21 285** |
| détail, non caché | 572 | **1 712** | 687 |
| détail, caché | 3 013 | 5 836 | **21 591** |
| tag, non caché | 490 | **1 102** | 647 |
| compute | **1 611** | 1 521 | 732 |

#### Latence : p50 / p99 en ms (plus bas est meilleur)

| Endpoint | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| index, non caché | 95,5 / 122,6 | **44,1** / 110,1 | 78,1 / **103,9** |
| index, caché | 14,6 / 22,4 | 7,4 / 57,7 | **2,3 / 3,5** |
| détail, non caché | 86,5 / 113,4 | **27,9** / 96,2 | 70,5 / **99,0** |
| détail, caché | 16,4 / 21,6 | 7,4 / 58,2 | **2,3 / 3,6** |
| tag, non caché | 100,8 / 130,1 | **44,0** / 110,9 | 75,9 / **97,0** |
| compute | **31,3 / 38,7** | 32,7 / 78,5 | 67,0 / 89,0 |

#### Empreinte

| | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| Image (non compressée) | **844 Mo** | 895 Mo | 1,01 Go |
| RAM, au repos | **54 Mio** | 219 Mio | 61 Mio |
| RAM, index sous charge | **89 Mio** | 241 Mio | 142 Mio |
| CPU, index sous charge (sur 400 %) | 380 % | 337 % | 405 % |

L'une des trois exécutions de WordPress sur l'index non caché a renvoyé 14 253
erreurs de « connexion à la base de données » en 7 s ; la médiane vient d'une
exécution propre.

---

## Limites

- Une charge de travail **à forte lecture, rendue côté serveur**. Pas
  d'écritures, de transactions, de flux d'authentification, de websockets ni
  de logique métier lourde.
- Un seul hôte : un portable Apple M3 Pro (6 cœurs performance + 6 cœurs
  efficacité). D'autres CPU peuvent ordonner les résultats autrement, surtout
  compute.
- Les applications tournent dans la VM Docker sur macOS, pas sur Linux sans
  VM. La VM n'a pas de compteurs de performance matériels.
- Go et les applications Rust seules sont des services écrits à la main ;
  Rustango, Rocket, Loco, Cot, l'application Python, Laravel et WordPress sont
  des frameworks.
- Les campagnes 1 et 2 sont des exécutions séparées ; comparez les chiffres au
  sein d'une même campagne.
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

- [Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled) (en anglais)
- [Campagne 1 : le benchmark équitable](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress) (en anglais)
- [Getting started](getting-started.md)
- [ORM cookbook](orm.md)
