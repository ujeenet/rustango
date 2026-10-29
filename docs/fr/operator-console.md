# La console opérateur

Un projet multi-tenant a deux sortes d'administrateurs, et elles ne se mélangent jamais :

- Les **opérateurs** exploitent le *déploiement*. Ils vivent dans le registry, se connectent sur le domaine apex et atteignent chaque tenant.
- Les **utilisateurs de tenant** exploitent *un* tenant. Ils vivent dans la base de ce tenant et ne voient jamais la console.

La console opérateur est l'interface web de la première sorte — provisionner des tenants, attacher des noms d'hôte, gérer les opérateurs, lire la piste d'audit et mettre un tenant hors service. Presque tout ici est aussi un [verbe `manage`](manage.md#commandes-de-tenancy), car une action qui n'existe que sur une seule surface ne peut pas être automatisée, et une qui n'existe que dans un shell ne peut pas être déléguée.

L'écart à connaître : la console peut migrer **un seul** tenant
(`POST /orgs/{slug}/migrate`), alors que `migrate-tenants` en CLI ne prend pas de slug
et traite tout le registry. Il n'y a pas de migration par tenant en ligne de commande.

[![La liste des tenants de la console opérateur — chaque tenant du registry avec son mode de stockage, son motif d'hôte et son état actif, plus les actions pour provisionner, migrer et préchauffer](../img/operator-console.png)](../img/operator-console.png)

## Table des matières

- [Le monter](#le-monter)
- [Ce que fait chaque page](#ce-que-fait-chaque-page)
- [Noms d'hôte](#noms-dhôte)
- [Hôtes inconnus](#hôtes-inconnus)
- [Opérateurs](#opérateurs)
- [Connexion à deux facteurs](#connexion-à-deux-facteurs)
- [Le journal d'audit](#le-journal-daudit)
- [Exécutions de provisioning](#exécutions-de-provisioning)
- [Serveurs de bases de données](#serveurs-de-bases-de-données)
- [Identité visuelle](#identité-visuelle)
- [Les trois niveaux de capacité](#les-trois-niveaux-de-capacité)

---

## Le monter

La console est un routeur que vous montez ; ce qu'elle peut faire dépend de ce que vous lui confiez.

```rust
use rustango::tenancy::operator_console::{router, router_with_pools, router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;

// Ici, `pools` est un `Arc<TenantPools<_>>`.

// Lecture seule : parcourir les tenants, les opérateurs et le journal d'audit.
let app = router(registry.clone(), SessionSecret::from_env_or_random());

// …plus l'édition des tenants, la gestion des opérateurs et des noms d'hôte, le préchauffage des pools.
let app = router_with_pools(registry.clone(), pools.clone().into_invalidator(), secret);

// …plus le provisioning de nouveaux tenants et l'exécution des migrations.
let provisioner = Provisioner::new(pools.clone(), registry_url.clone(), "migrations").erased();
let app = router_with_provisioning(
    registry.clone(),
    pools.clone().into_invalidator(),
    provisioner,
    secret,
);
```

Deux conversions font le travail ici, et aucune n'est facultative.
`pools` entre en tant qu'`Arc<dyn TenantPoolInvalidator>`, d'où
`.into_invalidator()` — la console ne fait qu'invalider des pools, et
c'est le trait étroit qui l'empêche d'atteindre le reste.
`Provisioner::new` capture les pools, l'URL du registre et le répertoire
de migrations, et `.erased()` le place derrière
`Arc<dyn TenantProvisioner>`.

Dans un projet `tenant` généré, tout est déjà câblé — `Cli::new().tenancy().with_tenant_provisioning("migrations")` monte la version complète. Voir [Échafaudage](scaffolding.md).

La console répond sur le domaine **apex** (`RUSTANGO_APEX_DOMAIN`), pas sur un sous-domaine de tenant : `http://localhost:8080/login` avec l'apex `localhost`. Une requête vers `127.0.0.1` n'est pas l'apex et ne correspondra pas.

---

## Ce que fait chaque page

| Page | À quoi elle sert |
|---|---|
| **Organizations** | Chaque tenant, avec mode de stockage, motif d'hôte et état actif |
| **Organizations → Edit** | Nom affiché, motif d'hôte, préfixe de chemin, port, URL de base de données, identité visuelle |
| **Hostnames** | Les domaines supplémentaires auxquels un tenant répond |
| **Operators** | Qui peut se connecter à cette console |
| **Audit log** | Chaque changement effectué via la console |
| **Runs** | Exécutions de provisioning et de migration, diffusées en direct |
| **Database servers** | Serveurs où les bases des tenants sont créées, avec leur login d'administration |
| **Change password** | Votre propre mot de passe ; vos autres sessions se terminent, celle-ci reste connectée |
| **Two-factor sign-in** | Votre application d'authentification (feature `totp`) |
| **Shared SSO** | Fournisseurs de connexion proposés à chaque tenant, avec l'URL de callback à enregistrer (feature `admin-sso` ; voir [SSO](sso.md)) |

---

## Noms d'hôte

Un tenant est joignable à son sous-domaine et, en option, aux noms d'hôte supplémentaires que vous lui attachez. Un nom d'hôte mène à exactement un tenant.

[![La page des noms d'hôte d'un tenant — l'hôte de base signalé comme tel et non supprimable, les hôtes supplémentaires avec les actions Mettre en attente et Retirer, et un formulaire d'ajout](../img/operator-console-hosts.png)](../img/operator-console-hosts.png)

Deux choses à savoir :

**L'hôte de base n'a pas de bouton de suppression.** Il provient de la colonne `host_pattern` du tenant et non de la table des noms d'hôte : il n'y a donc pas de ligne à retirer — changez-le sur la page d'édition du tenant. C'est structurel, pas une règle d'interface : le moteur le refuse aussi, si bien qu'un opérateur qui devine le `POST` obtient la même réponse que celui qui lit la page.

**La mise en attente conserve la ligne.** *Park* retire un hôte du service sans perdre l'enregistrement — utile pendant la propagation DNS, ou pour retirer un domaine que vous voudrez peut-être récupérer. *Serve* le remet en service.

Les noms d'hôte sont normalisés à l'entrée : minuscules, sans schéma, sans port, sans chemin. La valeur stockée est comparée octet par octet à l'en-tête `Host`, donc une valeur qui ne pourrait jamais correspondre est refusée plutôt qu'enregistrée.

Depuis la CLI : [`list-hosts` / `add-host` / `remove-host` / `set-host-enabled`](manage.md#noms-dhôte).

---

## Hôtes inconnus

Par défaut, une requête dont l'hôte ne résout aucun tenant va aux routes de
votre application, où l'extracteur `Tenant` et l'admin du tenant répondent 404.
Pour servir autre chose à la place — une page de configuration au premier
démarrage, une page « ce site n'est pas encore configuré » — passez un routeur
à `unresolved_tenant`, par exemple une route `/` qui répond « Créez votre premier site » tant que `TenantRegistry::has_tenants()` vaut false. `Builder::unresolved_tenant` a un exemple compilé.

Il s'exécute pour un hôte inconnu et pour un chemin de l'apex que la console ne
sert pas. Il ne s'exécute jamais pour un hôte qui résout un tenant, et la
console garde ses propres routes. Un tenant inactif ne résout pas : son hôte
atteint donc aussi ce routeur. Le routeur reçoit le registre sous la forme
`Extension<TenantRegistry>`, jamais un pool de tenant ; `has_tenants()` compte
aussi les tenants inactifs. `/health` et `/ready` restent servis à côté.

**Sécurité :** toute personne qui atteint le serveur atteint ce routeur.
Vérifiez l'état vous-même (par exemple, ne proposez la configuration que tant
que `has_tenants()` vaut false) et protégez tout ce qui écrit.

Le même setter existe sur `rustango::server::Builder`.

---

## Opérateurs

[![La page des opérateurs — l'opérateur connecté signalé « c'est vous », avec un formulaire pour en ajouter un autre et l'option de générer un mot de passe](../img/operator-console-operators.png)](../img/operator-console-operators.png)

Les opérateurs sont désactivés, jamais supprimés : la ligne reste, de sorte qu'un « qui a fait ça ? » se résout encore plus tard. La console la relit à **chaque** requête, si bien qu'une désactivation prend effet au clic suivant plutôt qu'à l'expiration du cookie.

Deux choses que la page refuse, pour la même raison :

- **Vous désactiver vous-même.** Votre requête suivante serait rejetée.
- **Désactiver le dernier opérateur actif.** Cela enferme tout le monde dehors, et seul un shell sur le registry pourrait le défaire.

Un mot de passe généré s'affiche **une fois**, dans le corps de la réponse — jamais via une redirection, qui le mettrait dans la barre d'adresse, l'historique, le référent et chaque journal d'accès intermédiaire.

Depuis la CLI : [`list-operators` / `set-operator-active`](manage.md#list-operators).

---

## Connexion à deux facteurs

Avec la feature `totp`, un opérateur peut ajouter une application d'authentification sous **Two-factor sign-in** (`/account/totp`). La page affiche une clé de configuration et un lien `otpauth://` jusqu'à ce qu'un code la confirme. La désactiver demande un code actuel.

Ensuite, le mot de passe seul n'ouvre que l'étape du code (`/login/totp`) ; aucune page de la console ne répond tant que le code n'est pas bon. Chaque code ne sert qu'une fois, et les codes faux comptent dans les mêmes limites et le même verrou que les mots de passe faux. L'activation, la désactivation et chaque code faux sont écrits dans le [journal d'audit](#le-journal-daudit) sous `totp_enroll`, `totp_disable` et `totp_failed`, jamais avec le code.

C'est facultatif par opérateur. `RUSTANGO_OPERATOR_REQUIRE_TOTP=1` le rend obligatoire : un opérateur sans appareil en configure un à l'étape du code avant sa première session. Lu au démarrage.

Les secrets sont dans `rustango_operator_totp` dans le registre, scellés sous `RUSTANGO_SECRET_KEY`, qui doit être défini pour s'inscrire. `migrate` crée la table, sinon la console le fait à la première connexion. Si la table manque (un rôle de registre sans CREATE, avant `migrate`), le mot de passe suffit sauf si TOTP est obligatoire ; si elle existe mais est illisible, la connexion est refusée. Il n'y a pas encore de codes de secours : si un opérateur perd son appareil, lancez `cargo run -- reset-operator-totp <username>`.

---

## Le journal d'audit

[![Le journal d'audit de la console — qui a changé quoi, sur quel enregistrement et quand, filtrable par entité, id et opération](../img/operator-console-audit.png)](../img/operator-console-audit.png)

Chaque modification passée par la console est enregistrée : éditions de tenant, changements de noms d'hôte, gestion des opérateurs, usurpation d'identité, purges, préchauffages. La colonne `source` porte `operator:<id>:<verb>`, ce qui permet de séparer après coup l'activité des opérateurs de celle des utilisateurs de tenant.

C'est le journal du **registry**. L'historique propre à un tenant vit dans l'admin de ce tenant — mélanger les deux imposerait d'éventer une requête sur chaque pool de tenant pour afficher une seule page.

Il grossit avec l'usage de la console : élaguez-le régulièrement avec [`audit-cleanup`](manage.md#audit-cleanup), qui balaie le journal du registry et celui de chaque tenant actif.

Depuis la CLI : [`audit-log`](manage.md#voir-ce-qui-sest-passé).

---

## Exécutions de provisioning

[![La page des exécutions de provisioning — chaque exécution avec son type, son tenant, son état et ses horaires, avec un lien vers les étapes enregistrées](../img/operator-console-runs.png)](../img/operator-console-runs.png)

Provisionner un tenant, ce sont plusieurs étapes contre une base qui peut être lente ou injoignable : c'est donc enregistré comme une **exécution** plutôt que comme une requête qui aboutit ou non. Chaque exécution diffuse ses étapes en direct et survit à un rechargement, à une reconnexion, ou au fait d'être suivie depuis un second pod.

Les tenants créés depuis la CLI sont enregistrés eux aussi, marqués `requested_by = cli`, si bien que l'historique couvre les deux surfaces.

### Créer un tenant

**Organizations → New tenant**. Chaque option indique sur le formulaire où
vivent les données, quel login est utilisé et de quels droits il a besoin :

- **Schema** (registry PostgreSQL uniquement) : un schéma dans la base du
  registry. Il utilise le login du registry (`DATABASE_URL`) ; les tenants le
  partagent et n'ont pas de login propre. Demande `CREATE` sur la base du registry.

En mode base de données, choisissez une option :

- **Create a new database on a saved server** : choisissez le serveur. Le
  login d'administration du serveur est utilisé une fois ; le tenant reçoit son
  propre login. La base porte exactement le nom du slug et reçoit un login
  propre avec un mot de passe aléatoire, autorisé sur cette base seulement.
  L'exécution affiche `create_database`, `create_login`, `grant` et
  `check_login` (le nouveau login se connecte). Une base existante de ce nom
  est refusée. Si une étape ultérieure échoue, l'étape `rollback` de
  l'exécution supprime la nouvelle base, le login et la ligne du tenant, et le
  formulaire revient avec la raison. C'est l'option par défaut dès qu'un
  serveur est enregistré.
- **Use an existing database URL** : rien n'est créé ; la base doit exister.
  Masquée, et jamais renvoyée à la page. Aucun droit d'administration requis.
- **A database on the registry's server** : un nom de base ; il utilise le
  login du registry (`DATABASE_URL`), donc les tenants ne sont pas isolés les
  uns des autres. Rien n'est créé.

**Test connection** vérifie ce dont l'option choisie a besoin et ne crée rien.
Sur un serveur enregistré, il lance chaque vérification qu'une soumission
effectue avant de créer quoi que ce soit (slug libre, routage, droits
d'administration, aucune base ni aucun login de ce nom) et liste chaque droit
avec oui, non ou inconnu. En mode schéma, il vérifie le `CREATE` du login du
registry sur la base du registry. Un droit manquant est accompagné des
instructions qui le corrigent, et la création est refusée avec le même texte
avant que quoi que ce soit ne soit créé. Voir
[Droits](tenant-databases.md#droits). Sinon, il se connecte à la base. Le
backend est toujours celui du registry.

### Premier administrateur

Indiquez un e-mail d'administrateur (et, en option, un nom d'utilisateur ; par
défaut l'e-mail) et l'exécution ajoute une étape `create_admin` après les
migrations, avant que le tenant ne soit mis en service. Elle crée un
superutilisateur sans mot de passe utilisable. Si l'étape échoue, l'exécution
revient en arrière comme pour toute autre étape.

L'administrateur choisit son propre mot de passe via un lien de définition de
mot de passe. Le lien ne sert qu'une fois, expire au bout de 72 heures et
n'ouvre que la page de changement de mot de passe de ce tenant. Avec
`Builder::first_admin_mailer(mailer, from)` (feature `email`), il est envoyé par e-mail à
l'administrateur. Sans mailer, la page de l'exécution vous l'affiche **une
fois**, juste après la soumission ; copiez-le et transmettez-le à
l'administrateur. Il n'est jamais stocké, audité ni journalisé ; le journal
d'audit n'enregistre que la création de l'utilisateur.

Les champs apparaissent quand le provisioner dispose d'une remise
(`Provisioner::with_first_admin_handover` ; le `Builder` standard en définit une).
Lien perdu ou expiré : lancez `manage set-password-link <slug> <username>`.

Depuis la CLI : [`list-runs` / `show-run`](manage.md#voir-ce-qui-sest-passé).

---

## Serveurs de bases de données

**Organizations → Database servers** enregistre un serveur une seule fois :
hôte, port, utilisateur et mot de passe d'administration, mode TLS et, sous
Postgres, une base de maintenance facultative à la place de `postgres`. Avec un
registre SQLite, un « serveur » est un répertoire pour les fichiers des
tenants. Le backend est toujours celui du registre.

Le mot de passe est stocké scellé (`enc:v1:`), donc `RUSTANGO_SECRET_KEY` doit
être défini ; sinon l'enregistrement est refusé et la page affiche un avertissement. Il n'est jamais réaffiché : le
formulaire d'édition laisse le champ vide, et un champ vide conserve celui
enregistré. Les lignes d'audit nomment les champs modifiés, jamais leurs valeurs.

**Test** se connecte et liste la version du serveur et, une ligne chacun, si le
login d'administration peut créer des bases, créer des logins et accorder des
droits. Quand il ne peut pas provisionner de tenants, il dit pourquoi (un droit
manquant, ou PostgreSQL avant 15) et affiche les instructions `GRANT` /
`ALTER ROLE` qui corrigent cela. Les droits issus de rôles MySQL actifs comptent.

TLS vaut `require` par défaut pour un nouveau serveur. Une modification faite
depuis une page que quelqu'un d'autre a changée entre-temps est refusée ;
rechargez et modifiez à nouveau.

Dans la stack de dev `docker compose` générée, Postgres n'a pas de TLS :
enregistrez-le avec TLS `disable` ; MySQL 8 sert du TLS, donc `require`
fonctionne. L'utilisateur MySQL `rustango` du compose ne peut ni créer
d'utilisateurs ni accorder de droits ; enregistrez ce serveur en tant que `root`.

La suppression est refusée tant que l'URL d'un tenant en mode base de données
pointe vers l'hôte et le port du serveur (SQLite : un fichier dans son
répertoire) ou ne peut pas être résolue, tant qu'une base créée là par rustango
est encore enregistrée, et sur une console construite sans pools de tenants.

Un tenant dont la base a été créée sur un serveur enregistré obtient **Rotate
database password** sur sa page. Cette action et **Purge** s'exécutent comme des
exécutions enregistrées, suivies en direct comme une création. Le déroulement
complet est dans [Bases de tenants sur un serveur enregistré](tenant-databases.md).

Depuis la CLI : [`add-db-server` et compagnie](manage.md#serveurs-de-bases-de-données).

---

## Identité visuelle

Le nom, le slogan, le logo, la couleur d'accent et le thème de la console
viennent de `[brand]` dans `config/default.toml` ou `config/<env>_settings.toml` :

```toml
[brand]
name          = "Acme Operator"
tagline       = "Production"
logo_url      = "https://cdn.example.com/acme-logo.png"
primary_color = "#2c6fb0"
theme_mode    = "auto"
```

| Clé | Défaut | Rôle |
|---|---|---|
| `name` | `Rustango` | Nom dans l'en-tête et sur la page de connexion |
| `tagline` | aucun ; la page de connexion affiche « Operator console » | Ligne sous le nom |
| `logo_url` | `/__static__/rustango.png` | Image du logo ; par défaut le logo fourni, servi par la console |
| `logo_path` | aucun | Fichier de logo local que la console sert elle-même ; voir [Votre propre logo](#votre-propre-logo) |
| `primary_color` | la couleur du thème | Teinte les boutons principaux et les liens. Hex : `#rrggbb` ou `#rgb` |
| `theme_mode` | `auto` | Thème de départ : `auto`, `light` ou `dark` |

Une valeur vide compte comme non définie. Un `primary_color` ou un
`theme_mode` qui ne se parse pas est ignoré, et la valeur par défaut reste.

Les variables d'environnement l'emportent sur le fichier : vous pouvez donc
changer l'identité d'un déploiement sans modifier la configuration :

| Variable | Remplace |
|---|---|
| `RUSTANGO_OPERATOR_BRAND_NAME` | `name` |
| `RUSTANGO_OPERATOR_TAGLINE` | `tagline` |
| `RUSTANGO_OPERATOR_LOGO_URL` | `logo_url` |
| `RUSTANGO_OPERATOR_LOGO_PATH` | `logo_path` |
| `RUSTANGO_OPERATOR_PRIMARY_COLOR` | `primary_color` |
| `RUSTANGO_OPERATOR_THEME_MODE` | `theme_mode` |

L'ordre est : variable d'environnement, puis `[brand]` (le fichier du niveau
au-dessus de `default.toml`), puis la valeur par défaut. Une variable vide est
ignorée. La console lit tout cela une seule fois au démarrage : redémarrez-la
après un changement.

### Votre propre logo

Le domaine apex ne sert que la console : un montage `Cli::with_static` est
servi sur les hôtes des tenants, pas sur l'apex. Un logo est donc soit un
fichier que sert la console, soit une URL complète.

`logo_path = "assets/acme-logo.png"` désigne un fichier local, relatif au
répertoire de travail. La console le lit une fois au démarrage, avec les mêmes
contrôles qu'un envoi de logo de tenant : PNG, JPEG, WebP ou ICO (pas de SVG),
au plus `RUSTANGO_BRAND_MAX_BYTES` (1 Mio). Elle sert le fichier à
`/__static__/brand-logo.<ext>?v=<hash>` avec un cache d'un an ; le hash change
avec le fichier. Un fichier manquant ou refusé journalise un avertissement et
le logo fourni reste.

`logo_url` va directement dans `<img src>` : le navigateur doit donc pouvoir le
charger — un CDN, un bucket de stockage objet ou votre site web. Si vous
définissez `[security] csp`, autorisez cet hôte dans `img-src`.

Si les deux sont définis, les variables d'environnement l'emportent sur le
fichier et, au sein de chacun, `logo_url` l'emporte sur `logo_path`.

Dans un projet sans tenancy, l'admin partage un hôte avec vos routes, donc un
montage statique y fonctionne ; voir [la section image de marque de l'admin](admin.md#configuration--admin-et-brand).

La console ne définit pas de favicon, et il n'existe pas de clé pour cela.

### Les slogans `dev` et `staging`

`cargo rustango new` écrit `tagline = "dev"` sous `[brand]` dans
`config/dev_settings.toml` et `tagline = "staging"` dans
`staging_settings.toml`, pour distinguer les niveaux d'un coup d'œil ; le
slogan s'affiche comme un badge. `prod_settings.toml` n'en définit aucun. Le
fichier du niveau l'emporte sur `default.toml`, donc cela remplace un slogan que
vous y auriez défini.

Pour le changer, modifiez la ligne, par exemple `tagline = "Acme dev"`. Pour le
retirer, supprimez la ligne ou mettez `tagline = ""`. Dans un projet avec
l'admin, le même slogan sert de sous-titre à l'admin.

### Identité visuelle par tenant

Chaque tenant a sa propre identité sur sa ligne `Org`, définie dans
**Organizations → Edit** : `brand_name`, `brand_tagline`, `primary_color` et
`theme_mode`. Le formulaire **Branding assets** de la même page envoie un logo
(PNG, JPEG ou WebP) et un favicon (PNG ou ICO), jusqu'à 1 Mo chacun
(`RUSTANGO_BRAND_MAX_BYTES`). Les fichiers sont stockés sous
`RUSTANGO_BRAND_STORAGE_DIR`, par défaut `./var/brand`. Ce dossier est local
à chaque pod et perdu au redéploiement ; avec plusieurs réplicas, passez un
disque partagé via `Cli::brand_storage` ou `server::Builder::brand_storage`
(par exemple un disque `StorageRegistry`).

L'admin du tenant et sa page de connexion les utilisent ; `[brand]` ne les
atteint pas. Sans `brand_name`, le nom affiché du tenant est utilisé, et sans
logo envoyé, le logo du framework. Le favicon s'affiche sur les pages de
connexion et de changement de mot de passe du tenant. Voir [la section image de marque de l'admin](admin.md#ladmin-du-tenant).

---

## Les trois niveaux de capacité

Les routes qui existent dépendent du constructeur que vous avez monté :

| | `router` | `router_with_pools` | `router_with_provisioning` |
|---|---|---|---|
| Parcourir tenants, opérateurs, journal d'audit | ✓ | ✓ | ✓ |
| Éditer un tenant, gérer les noms d'hôte | | ✓ | ✓ |
| Ajouter / désactiver des opérateurs | | ✓ | ✓ |
| Préchauffer les pools | | ✓ | ✓ |
| Mettre un tenant hors service | | ✓ | ✓ |
| Provisionner un tenant, exécuter les migrations | | | ✓ |
| Consulter les exécutions de provisioning | | | ✓ |
| Enregistrer et tester des serveurs de bases de données | | | ✓ |

Les routes non montées renvoient 404 plutôt que 403 — une console en lecture seule n'annonce pas ce qu'elle ne sait pas faire.

### Trois autres constructeurs, pour le câblage et non pour la capacité

Les trois ci-dessus sont les niveaux de capacité. Les autres reprennent
ces mêmes niveaux en ajoutant du câblage : ce ne sont donc pas un
quatrième et un cinquième échelon.

| Constructeur | Niveau | Ce qu'il prend en plus |
|---|---|---|
| `router_with_brand_storage` | lecture seule, ou édition si vous passez `Some(pools)` | un `BoxedStorage` pour les envois de marque, quand le `LocalStorage` par défaut n'est pas là où vous les voulez |
| `router_with_impersonation` | édition | le stockage de marque, le secret de session du tenant et une URL de reprise, pour qu'un opérateur puisse entrer dans un tenant |
| `router_full` | celui que vous passez | tout ce qui précède en `Option` — `None` ne monte simplement pas ces routes |

`rustango::server::Builder::serve` monte l'impersonation pour vous ; un point de
montage personnalisé y souscrit en utilisant `router_with_impersonation`
à la place de `router_with_pools`. Recourez à `router_full` quand vous
avez besoin d'une combinaison que les raccourcis ne nomment pas — il
existe pour qu'un nouvel appariement n'exige pas un nouveau
constructeur positionnel.

**Chaque opérateur peut tout faire.** Il n'y a pas de garde-fous de permission par opérateur : un opérateur atteint chaque tenant et chaque action offerte par la console. La frontière de contrôle d'accès est la liste des opérateurs elle-même — c'est précisément pour cela qu'une désactivation prend effet à la requête suivante.
