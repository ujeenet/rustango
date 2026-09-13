# Réglage de la base de données

Chaque connexion à la base de données que fait votre application vient
d'un **pool** : un ensemble de connexions maintenues ouvertes et
distribuées aux requêtes. Les valeurs par défaut du pool sont choisies
par le driver, pas par votre charge, et elles sont mauvaises pour la
plupart des déploiements en production sur au moins un point.

Cette page explique ce que fait chaque paramètre, ce qui casse quand on
n'y touche pas, et où le régler.

## Pourquoi régler quoi que ce soit

Quatre pannes, chacune causée par une valeur par défaut différente :

**Votre dixième requête simultanée attend.** Le pool par défaut du driver
garde **10** connexions. La onzième requête n'échoue pas — elle *fait la
queue*, et votre latence p99 monte pendant que le CPU ne fait rien. Rien
dans les logs ne dit « pool épuisé » ; vous voyez des requêtes lentes.

**Une base de données injoignable met à terre le serveur entier.**
Lorsqu'une connexion ne peut pas être obtenue, l'appelant attend. La
valeur par défaut du driver est de 30 secondes, un chiffre d'outil de
traitement par lots : sur le chemin d'une requête, cela veut dire qu'un
worker est bloqué une demi-minute par requête, donc la panne d'*une* base
sature tous les workers et emporte des surfaces qui n'y touchent jamais.
C'est pourquoi rustango met **5 secondes** par défaut.

**Les connexions meurent en silence et la requête suivante paie.** Un
répartiteur de charge, un pare-feu ou le propre
`idle_in_transaction_session_timeout` de PostgreSQL ferme une connexion
restée inactive. Votre pool ne le sait pas. La requête suivante qui
l'emprunte reçoit un *broken pipe* — par intermittence, à faible trafic,
c'est-à-dire le type de bug le plus difficile à reproduire.

**Un basculement ou une rotation d'identifiants ne prend pas effet.** Les
connexions d'un pool sont durables par conception. Après un basculement,
ou après une rotation de mot de passe, un pool peut continuer à utiliser
des connexions ouvertes vers l'ancien serveur ou avec les anciens
identifiants jusqu'à ce que quelque chose les ferme.

## Où le régler

Trois endroits. **Le plus haut l'emporte**, pour qu'une surcharge
d'urgence au déploiement n'exige jamais un *push* de configuration ni un
redémarrage de votre chaîne de configuration.

| | où | à quoi ça sert |
|---|---|---|
| 1 | variables d'environnement | surcharges par déploiement, gestionnaires de secrets, réglage d'urgence |
| 2 | `[database]` dans votre palier de settings | les valeurs avec lesquelles votre projet tourne normalement, versionnées |
| 3 | valeurs par défaut du framework | ce que vous obtenez si vous ne réglez rien |

### Dans un palier de settings

Les settings vivent dans `config/*_settings.toml`, un fichier par
environnement. Mettez les valeurs dans le palier auquel elles
appartiennent — celles de développement dans `dev_settings.toml`, celles
de production dans `prod_settings.toml` :

```toml
[database]
pool_max_size             = 50
pool_min_size             = 5
pool_acquire_timeout_secs = 5
pool_idle_timeout_secs    = 600
pool_max_lifetime_secs    = 1800
```

### En variables d'environnement

Chaque paramètre a une surcharge par environnement, qui prime sur le
TOML :

```bash
RUSTANGO_DB_MAX_CONNECTIONS=50
RUSTANGO_DB_MIN_CONNECTIONS=5
RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS=5
RUSTANGO_DB_IDLE_TIMEOUT_SECS=600
RUSTANGO_DB_MAX_LIFETIME_SECS=1800
```

Une valeur qui n'est pas un entier positif est **ignorée avec un
avertissement** plutôt qu'obéie : une faute de frappe ne doit ni mettre
silencieusement une borne à zéro, ni faire échouer un démarrage.

### Une règle d'ordre

Les settings sont appliqués aux pools par `Cli::with_settings(...)`.
Appelez-le **avant** que quoi que ce soit n'ouvre une connexion. Un pool
construit plus tôt tourne avec les valeurs de l'environnement, et le
framework journalise un avertissement indiquant combien de pools sont
concernés, plutôt que de vous laisser le découvrir sous charge.

## Les paramètres

| setting | variable d'environnement | par défaut | effet |
|---|---|---|---|
| `pool_max_size` | `RUSTANGO_DB_MAX_CONNECTIONS` | 10 (driver) | Nombre maximal de connexions ouvertes. Les requêtes en trop font la queue. |
| `pool_min_size` | `RUSTANGO_DB_MIN_CONNECTIONS` | 0 (driver) | Connexions maintenues ouvertes même au repos. |
| `pool_acquire_timeout_secs` | `RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS` | 5 | Combien de temps un appelant attend une connexion avant d'échouer. |
| `pool_idle_timeout_secs` | `RUSTANGO_DB_IDLE_TIMEOUT_SECS` | défaut du driver | Ferme les connexions inactives depuis cette durée. |
| `pool_max_lifetime_secs` | `RUSTANGO_DB_MAX_LIFETIME_SECS` | défaut du driver | Ferme les connexions de cet âge, utilisées ou non. |

Ne pas régler un paramètre n'est pas la même chose que le mettre à zéro.
Non réglé signifie *la valeur par défaut du driver s'applique* — le
comportement que vous aviez avant que le paramètre existe.

### `pool_max_size`

Le plafond du travail simultané sur la base. Augmentez-le quand les
requêtes font la queue pour une connexion ; le symptôme est une latence
qui croît avec le trafic alors que la base elle-même n'est pas chargée.

**C'est un plafond, pas une cible** — le pool ouvre des connexions à la
demande et seulement jusqu'à ce nombre.

La contrainte importante est de l'*autre* côté : votre serveur de base de
données a sa propre limite de connexions (le `max_connections` de
PostgreSQL, typiquement 100). La somme de tous les pools sur toutes les
répliques doit rester en dessous, sinon les nouvelles connexions sont
refusées. Dix pods avec `pool_max_size = 50`, cela fait 500 connexions
contre un serveur qui en autorise 100.

### `pool_min_size`

Des connexions maintenues ouvertes même quand personne ne s'en sert.
L'enjeu est la latence : à `0`, la première requête après une période
calme paie l'aller-retour complet TCP, TLS et authentification avant la
moindre requête SQL.

Dimensionnez-le sur votre charge de fond, pas sur votre pic. Les
connexions maintenues coûtent aussi des ressources côté serveur.

### `pool_acquire_timeout_secs`

Combien de temps un appelant attend une connexion avant d'abandonner —
ce qui couvre **à la fois** l'ouverture d'une nouvelle connexion **et**
l'attente d'une connexion libre.

C'est ce paramètre qui décide du comportement de votre application quand
la base est injoignable. Trop élevé, et les workers se bloquent sur une
base qui ne répondra jamais ; trop bas, et un pic de trafic légitime — où
faire la queue est un travail réel et productif — se transforme en
erreurs.

Les 5 secondes par défaut sont délibérément plus serrées que les 30 du
driver. Augmentez-les si vous avez des requêtes longues et un pool
saturé mais sain ; baissez-les si vous préférez rejeter la charge vite.

### `pool_idle_timeout_secs`

Ferme les connexions restées inutilisées. Réglez-le **en dessous** du
plus court délai d'inactivité présent sur le chemin entre votre
application et la base : un répartiteur de charge, un proxy, la table de
connexions d'un pare-feu, ou les délais du serveur lui-même. Si autre
chose ferme la connexion en premier, votre pool distribue une connexion
morte.

10 minutes est un point de départ courant.

### `pool_max_lifetime_secs`

Ferme les connexions au-delà d'un âge fixe, saines ou non. C'est le
paramètre qui fait que les basculements et les rotations d'identifiants
prennent réellement effet : sans lui, un pool peut continuer à parler au
serveur auquel il s'est connecté au démarrage.

30 minutes est un point de départ courant. Moins si vous obtenez des
identifiants à durée de vie courte depuis un gestionnaire de secrets.

## Les backends diffèrent

**SQLite n'est pas un serveur**, et dimensionner le pool n'y veut pas
dire la même chose. SQLite sérialise les écrivains globalement — un seul
à la fois, quelle que soit la taille du pool — donc augmenter
`pool_max_size` ajoute de la concurrence en lecture mais jamais en
écriture. Avec une base en mémoire, les connexions supplémentaires sont
pires qu'inutiles : chacune est une *base vide distincte*, sauf si l'URL
précise `cache=shared`.

**PostgreSQL et MySQL** imposent tous deux une limite de connexions côté
serveur. Dimensionnez vos pools dans ce budget, toutes répliques
confondues, et rappelez-vous que les *poolers* comme PgBouncer changent
le calcul.

## Les options de connexion sont autre chose

Les paramètres ci-dessus concernent le *pool*. Les options d'une
*connexion* individuelle — TLS, délais de connexion, le nom
d'application que voit le serveur — voyagent dans l'URL de connexion :

```
postgres://user:pw@host:5432/db?sslmode=require&connect_timeout=10&application_name=myapp
mysql://user:pw@host:3306/db?ssl-mode=REQUIRED
sqlite://./dev.db?mode=rwc
```

Elles sont transmises telles quelles au driver : tout ce qu'accepte son
analyseur d'URL fonctionne.

## Pools de tenants

Dans une application multi-tenant, chaque tenant en mode base de données
a son propre pool, et ceux-ci se configurent séparément via
`TenantPoolsConfig` — voir [Réglage des pools de tenants](manage.md) dans
le guide `manage`. Leurs valeurs par défaut diffèrent de celles du pool
principal ; en particulier le délai d'acquisition des tenants est plus
généreux, ce qui mérite un examen si vous exploitez de nombreux tenants
sur des bases pouvant devenir injoignables indépendamment.
