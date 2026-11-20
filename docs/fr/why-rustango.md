# Pourquoi Rustango : une comparaison honnête

Rustango est un framework web pour Rust, livré avec tout le nécessaire : déclarez un modèle une fois et vous obtenez un ORM, des migrations, une interface d'administration, une API REST et le multi-tenant. Cette page présente ses points forts, sa place face aux frameworks qu'on choisit habituellement, les chiffres des benchmarks, et les cas où il vaut mieux choisir autre chose.

## En bref

- **Choisissez Rustango** si vous voulez la productivité d'un framework complet avec la vitesse, la sobriété en mémoire et les vérifications à la compilation de Rust, et un seul binaire à déployer.
- **Choisissez autre chose** si votre équipe n'écrit pas en Rust, si vous avez besoin dès aujourd'hui d'un immense écosystème de paquets, ou si vous voulez un framework avec dix ans d'utilisation en production.

## Ce qui est inclus

| Domaine | Rustango |
|---|---|
| ORM | Un modèle, un SQL correct pour PostgreSQL, MySQL 8+ et SQLite ; relations, agrégats, suppression logique |
| Migrations | Générées en comparant vos modèles au dernier instantané, puis appliquées |
| Administration | Une administration générée automatiquement, avec recherche, filtres, journal d'audit et permissions |
| API | ViewSets et serializers pour REST, OpenAPI généré à partir d'eux |
| Authentification | Sessions, JWT, OAuth2 / OIDC, authentification unique, clés d'API, HMAC, TOTP, passkeys |
| Multi-tenant | Un schéma par tenant (PostgreSQL) ou une base par tenant (les trois bases) |
| Tâches de fond | Jobs, tâches planifiées, signaux, e-mail, stockage de fichiers |
| Sécurité | CSRF, limites de débit, limitation des connexions, en-têtes de sécurité, CORS, hôtes autorisés |
| Agents d'IA | Un serveur MCP intégré |
| Outils | Un générateur de projet, des générateurs de code et une seule ligne de commande `manage` |

Tout ce qui dépasse l'ORM est une feature cargo : une API purement JSON se compile entièrement sans l'administration ni les templates.

## Comparaison

| | Rustango | Django | Rails | Laravel | Loco | axum + sqlx |
|---|---|---|---|---|---|---|
| Langage | Rust | Python | Ruby | PHP | Rust | Rust |
| ORM | Intégré | Intégré | Intégré | Intégré | SeaORM | Requêtes à la main |
| Migrations depuis les modèles | Oui, par différence | Oui, par différence | Écrites avec des générateurs | Écrites à la main | Écrites avec des générateurs | À la main |
| Administration | Intégrée | Intégrée | Gems | Paquets (Nova, Filament) | Non | Non |
| Couche d'API REST | Intégrée, avec OpenAPI | Paquet (DRF) | Intégrée (mode API) | Intégrée | Intégrée | À la main |
| Multi-tenant | Intégré | Paquets | Gems | Paquets | Non | À la main |
| Tâches de fond | Intégrées | API Tasks ; worker via un paquet | Intégrées | Intégrées | Intégrées | À la main |
| Erreurs dans les modèles et requêtes | Détectées à la compilation | À l'exécution | À l'exécution | À l'exécution | Détectées à la compilation | Détectées à la compilation |
| Livrable | Un binaire | App + environnement Python | App + environnement Ruby | App + environnement PHP | Un binaire | Un binaire |
| Écosystème | Petit et récent | Immense | Immense | Immense | En croissance | Large (crates Rust) |

Lecture honnête : en fonctionnalités, Rustango joue dans la même catégorie que Django et Rails, sans les dépasser. Il l'emporte sur la vitesse, la mémoire, la sûreté à la compilation et le déploiement ; face aux autres options en Rust, il l'emporte sur tout ce qui est fourni d'office.

## Benchmarks

Chaque application servait le même blog : 1 000 articles, 10 000 commentaires, le même HTML vérifié octet par octet, 4 CPU, 16 connexions à la base, 50 connexions simultanées. La méthode complète, les limites et les profils sont dans le [compte rendu des benchmarks](benchmarks.md). Les manches ont été mesurées séparément : chaque tableau reste donc à l'intérieur d'une seule manche.

**Manche 2 : applications compilées** (requêtes par seconde, plus c'est haut, mieux c'est) :

| Application | Index | Détail | Tag | Index, en cache | Mémoire sous charge |
|---|--:|--:|--:|--:|--:|
| Rustango · actix | **17 401** | **27 292** | 7 056 | **231 975** | **6.3 MiB** |
| Rustango · axum | 14 686 | 25 788 | **7 090** | 174 501 | 6.9 MiB |
| axum + sqlx | 14 140 | 22 842 | 6 467 | 198 669 | 7.6 MiB |
| actix + sqlx | 14 408 | 21 630 | 6 628 | 226 887 | 7.2 MiB |
| Rocket | 13 316 | 24 561 | 6 836 | 138 224 | 8.7 MiB |
| Loco | 10 290 | 15 635 | 5 604 | 153 243 | 12.0 MiB |
| Go (net/http + pgx) | 8 741 | 22 607 | 6 386 | 91 839 | 22.8 MiB |

Rustango, avec son ORM, est le plus rapide des huit sur les pages non mises en cache, devant les requêtes écrites à la main sur sqlx.

**Manche 1 : environnements interprétés**, avec Rustango dans la même série de mesures (la 0.60.6, avant les corrections de la manche 2) :

| Application | Index sans cache, req/s | Index en cache, req/s | Mémoire sous charge |
|---|--:|--:|--:|
| Rustango · actix | 6 341 | 200 167 | ~7 MiB |
| Laravel · Octane | 1 098 | 5 884 | 241 MiB |
| Python · gunicorn | 697 | 11 012 | 224 MiB |
| WordPress | 599 | 21 285 | 142 MiB |
| Laravel · php-fpm | 518 | 3 326 | 89 MiB |

## Les limites, honnêtement

- **Go est plus rapide en calcul pur**, de 8 à 17 % sur le test de calcul, et axum avec sqlx seul sert l'index en cache plus vite que Rustango sur axum (198 669 contre 174 501 req/s).
- **Il est jeune.** Version 0.x, avec une large surface encore en train de se stabiliser. Les dernières versions ont ajouté plutôt que retiré, mais lisez le changelog à chaque mise à jour.
- **L'écosystème est petit.** Django, Rails et Laravel ont un paquet pour presque tout ; avec Rustango, vous écrirez plus souvent le code vous-même, ou vous utiliserez une crate Rust générique.
- **Rust demande un apprentissage**, et les temps de compilation sont réels : quelques minutes pour une première compilation, quelques secondes pour une modification.
- **Un schéma par tenant demande PostgreSQL.** Sur MySQL et SQLite, chaque tenant a sa propre base.
- **Les benchmarks ont leurs limites.** Un ordinateur portable, une VM Docker, une charge en lecture rendue côté serveur. Pas d'écritures, pas de connexion, pas de logique métier lourde. Vos chiffres seront différents.

## Pour qui

- Les équipes qui aiment la façon de travailler de Django ou de Rails, mais veulent la vitesse, la sûreté et la sobriété de Rust.
- Les équipes Rust lassées de réassembler l'authentification, l'administration, les migrations et l'API à chaque projet.
- Les produits SaaS multi-tenant qui veulent le multi-tenant dans le framework plutôt que greffé après coup.

## Essayer

```bash
cargo install cargo-rustango
cargo rustango new myapp
cd myapp && cargo run
```

Le [guide de démarrage](getting-started.md) construit pas à pas un blog avec une administration, une API JSON et la connexion. Pour les sites de contenu, [Rustango-CMS](https://cms.rustango.com) est construit par-dessus.
