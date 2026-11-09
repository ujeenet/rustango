# Glossaire

Une référence en langage simple des mots utilisés dans cette documentation. Si un
terme d'un guide vous est inconnu, cherchez-le ici d'abord. Les définitions sont
volontairement informelles — les guides approfondis donnent les détails précis.

Si vous n'avez jamais construit d'API web auparavant, lisez [Les bases des API web](#les-bases-des-api-web)
de haut en bas ; c'est une introduction de cinq minutes. Tout le reste est fait pour
être consulté au fur et à mesure.

## Table des matières

- [Les bases des API web](#les-bases-des-api-web) — ce qu'est une API, en termes de tous les jours
- [Les briques de Rustango](#les-briques-de-rustango) — les pièces que vous assemblez
- [Les données et la base de données](#les-données-et-la-base-de-données)
- [Connexion et sécurité](#connexion-et-sécurité) — sessions, tokens, mots de passe, signatures
- [Cache, fichiers, e-mail et jobs](#cache-fichiers-e-mail-et-jobs)
- [Langues](#langues) — traduire votre application
- [Temps réel et agents IA](#temps-réel-et-agents-ia) — mises à jour en direct, MCP
- [Multi-tenance (multi-tenancy)](#multi-tenance-multi-tenancy) — seulement si vous servez plusieurs clients depuis un seul déploiement
- [Quelques mots de Rust](#quelques-mots-de-rust) — pour que les blocs de code ne fassent pas peur

---

## Les bases des API web

**API** — *Application Programming Interface* (interface de programmation d'application). Un moyen pour un programme de parler à
un autre. Une **API web** le fait par internet : votre application envoie un message, le
serveur en renvoie un. Voyez-la comme un serveur au restaurant — vous commandez à partir d'un menu, la
cuisine renvoie un plat.

**API REST** — le style d'API web le plus courant. « REST » n'est qu'un ensemble de
conventions : vous agissez sur des **ressources** (comme « posts » ou « users ») en utilisant des verbes
web standard. Vous n'avez pas besoin de connaître la théorie — en pratique cela signifie *des
URL prévisibles et une poignée de verbes*, décrits ci-après.

**Endpoint (point de terminaison)** — une URL précise à laquelle votre API répond, comme `/api/posts` (tous les posts)
ou `/api/posts/42` (le post d'id 42). Une API est un ensemble d'endpoints.

**Verbe HTTP (ou méthode)** — *ce que* vous voulez faire à un endpoint. Il y en a cinq
que vous verrez constamment :

| Verbe | Signifie | Exemple |
|---|---|---|
| `GET` | lire / récupérer | « donne-moi tous les posts » |
| `POST` | créer | « ajoute un nouveau post » |
| `PUT` | remplacer | « écrase entièrement le post 42 » |
| `PATCH` | mettre à jour partiellement | « change juste le titre du post 42 » |
| `DELETE` | supprimer | « supprime le post 42 » |

**Requête / Réponse** — une requête est le message que vous envoyez (un verbe + un endpoint
+ éventuellement un corps de données). La réponse est ce qui revient (un code de statut +
généralement un corps de données).

**JSON** — le format texte que les API utilisent pour transporter les données. Il ressemble à
`{"title": "Hello", "published": true}` — des valeurs étiquetées, lisibles par un humain. Les
requêtes comme les réponses sont généralement en JSON.

**Code de statut** — un nombre à trois chiffres dans chaque réponse qui indique comment cela s'est passé :

| Code | Signification |
|---|---|
| `200` | OK — voici vos données |
| `201` | Created — votre nouvelle chose a été enregistrée |
| `204` | Done — rien à renvoyer (p. ex. après une suppression) |
| `400` | Bad request — vous avez envoyé quelque chose d'invalide (le corps dit quoi) |
| `422` | Non traitable — les données n'ont pas passé la validation (le corps nomme le champ) |
| `401` / `403` | Non connecté / non autorisé |
| `404` | Not found (introuvable) |
| `429` | Too many requests — ralentissez |
| `500` | Le serveur a rencontré une erreur |

**401 / 403** — `401` veut dire « nous ne savons pas qui vous êtes » : connectez-vous
ou envoyez un token. `403` veut dire « nous savons qui vous êtes, mais vous n'avez
pas le droit de faire cela ».

**Route** — un motif d'URL plus le verbe auquel il répond, comme
`GET /api/posts/{id}`, relié au code qui le traite. Voir
[URL et routage](urls.md).

**Handler** — la fonction qui s'exécute pour une route. Elle reçoit la requête et
renvoie la réponse.

**Cookie** — un petit morceau de texte que le serveur demande au navigateur de
garder. Le navigateur le renvoie à chaque requête suivante vers le même site.
C'est ainsi qu'un navigateur reste connecté.

**Multipart (envoi de formulaire)** — le format de requête qu'un navigateur
utilise pour envoyer un formulaire qui contient des fichiers. Chaque champ et
chaque fichier voyage comme une partie à part. Voir
[Fichiers](files.md#étape-3--accepter-un-téléversement).

**Rendu côté serveur (server-rendered)** — une page HTML que le serveur construit
et envoie complète ; le navigateur n'a plus qu'à l'afficher. Voir
[Vues HTML](html-views.md).

**CRUD** — *Create, Read, Update, Delete* (créer, lire, mettre à jour, supprimer). Les quatre choses de base que vous faites aux données.
Une « API CRUD » signifie simplement une API qui vous permet de faire les quatre. Voir
[ViewSets](viewsets.md), qui construisent une API CRUD complète à partir d'une seule déclaration.

**Chaîne de requête / paramètre de requête (query string / query parameter)** — la partie `?key=value` à la fin d'une URL,
utilisée pour filtrer, rechercher, trier ou paginer les résultats — p. ex.
`/api/posts?status=published&page=2`. Chaque `key=value` est un paramètre.

**Pagination** — découper une longue liste de résultats en pages pour qu'une réponse ne soit pas
énorme. L'**enveloppe** est l'emballage autour de la page qui vous indique aussi les
totaux — p. ex. `{"count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ]}`. Voir
[Pagination](viewsets.md#pagination).

**`curl`** — un outil en ligne de commande pour envoyer des requêtes d'API à la main. Les
exemples `curl ...` dans cette documentation vous permettent d'essayer un endpoint depuis un terminal
sans écrire de code.

---

## Les briques de Rustango

Ce sont les pièces que vous assemblez pour construire une application. Chacune renvoie vers son guide complet.

**Modèle (Model)** — une description d'un type de chose que votre application stocke, comme un `Post` ou
un `User`. Vous l'écrivez comme un `struct` Rust ; Rustango le transforme en table de base de
données. Voir le [guide de l'ORM](orm.md).

**Migration** — un changement enregistré de la forme de votre base de données (ajouter une table,
une colonne…). Vous en générez une avec `makemigrations` et l'appliquez avec `migrate`,
pour que chaque environnement se retrouve avec la même structure de base de données.

**Serializer (sérialiseur)** — le traducteur entre les lignes de votre base de données et le JSON que votre API
envoie et reçoit. Il décide quels champs sont visibles, renomme ou calcule des
champs pour la sortie, et valide les données entrantes. Il *met en forme* les données ; il ne les
enregistre pas (c'est le modèle qui le fait). Voir le [guide des Serializers](serializers.md).

**ViewSet** — prend un modèle et un serializer et produit une **API JSON** CRUD
complète (les cinq verbes ci-dessus) automatiquement, pour que vous n'écriviez pas chaque
endpoint à la main. La *vue API*. Voir le [guide des ViewSets](viewsets.md).

**Vue HTML (template view, class-based view)** — la contrepartie rendue côté serveur
d'un ViewSet : transforme un modèle en **pages** HTML — une page de liste, une
page de détail, et des formulaires de création/édition/suppression — rendues via des templates Tera,
au lieu de JSON. La *vue HTML*. Voir [Vues HTML](html-views.md).

**Template (gabarit)** — un fichier avec des espaces réservés (Rustango utilise [Tera](https://keats.github.io/tera/))
que le serveur remplit de données pour produire
une page HTML. `{{ post.title }}` insère une valeur ; `{% for … %}` fait une boucle.

**Router / routeur / montage (mount)** — le router associe les URL entrantes au code qui les
traite. *Monter* un ViewSet signifie « attacher ses endpoints à votre application à un chemin
donné », p. ex. monter l'API des posts sur `/api/posts`. Voir [URLs et routage](urls.md).

**Middleware (une « couche » / layer)** — du code qui s'exécute *autour* de chaque requête — avant votre
handler et après lui — pour des préoccupations transversales comme la journalisation, la limitation de débit, les
en-têtes de sécurité ou le CSRF. « Layer » est le mot de Rustango pour une pièce de
middleware. Voir le [guide du Middleware](middleware.md).

**Extracteur (extractor)** — un argument du handler qui sort une chose de la
requête pour vous, comme l'utilisateur connecté, le corps JSON ou le tenant
courant. S'il manque, l'extracteur rejette la requête avant que votre code ne
s'exécute.

**Pool** — le handle de base de données que votre code utilise pour lire et écrire.
Rustango vous demande de le passer explicitement à chaque appel à la base de données
(plutôt que de le cacher dans un global), pour qu'il soit toujours clair ce qui touche à
la base de données. Vous verrez `&pool` comme dernier argument des appels de l'ORM.
`rustango::sql::Pool` est un enum sur les trois backends, et c'est ce que prennent les
méthodes courantes.

**Executor** — *pas* un synonyme de pool. Un executor est une seule connexion ou une
transaction ouverte : ce sur quoi une instruction s'exécute réellement. Les méthodes
qui finissent par `_on` en prennent un, et c'est ainsi que vous placez plusieurs
instructions dans une même transaction. `fetch_on`, `first_on`, `count_on` et
`exists_on` acceptent n'importe quel executor sur les trois backends ; le reste de la
famille `_on` reste réservé à Postgres. La distinction décide des méthodes qui existent
dans votre build, donc [api-conventions](api-conventions.md#fonctions) détaille la règle.

**QuerySet** — une requête de base de données que vous construisez étape par étape en Rust
(`Post::objects().filter(...).order_by(...)`) avant de l'exécuter. Elle est paresseuse (lazy) :
rien ne touche la base de données tant que vous ne la `fetch`ez pas.

**Feature flag (drapeau de fonctionnalité)** — un interrupteur marche/arrêt, défini dans `Cargo.toml`, qui inclut ou
exclut un morceau du framework à la compilation. Il vous permet de garder votre application petite
en ne compilant que ce que vous utilisez. La plupart des fonctionnalités sont activées par défaut.

**Reverse (construire une URL par son nom)** — produire une URL à partir de son
nom au lieu de taper le chemin, pour qu'un chemin renommé ne casse aucun lien.
Voir [Noms d'URL et reverse](urls.md).

**Namespace / namespacing** — un préfixe dans le nom d'une URL, comme
`blog:post-detail`, qui sépare les noms des différentes parties de
l'application. Voir [Namespacing](urls.md#namespacing).

**Fieldset (groupe de champs)** — un groupe de champs avec un titre sur un
formulaire d'édition de l'admin, comme « Content » et « Publishing ». Voir
[Admin](admin.md#le-formulaire-de-modification).

**Piste d'audit (audit trail)** — un journal de qui a modifié quelle ligne,
quand, et quelles étaient les anciennes et les nouvelles valeurs. Voir
[Admin](admin.md#piste-daudit).

**OpenAPI** — un format de fichier standard qui décrit chaque endpoint d'une API.
Des outils le lisent pour afficher une documentation interactive et générer du
code client. Voir [OpenAPI](openapi.md).

**Schema (schéma)** — une description de la forme des données. Dans OpenAPI, il
décrit un corps JSON. Dans une base de données, c'est l'ensemble des tables, et
dans Postgres c'est aussi un groupe nommé de tables dans une même base.

**Fixture** — des données de test que vous préparez avant un test, pour que
chaque test parte d'un état connu. Voir [Tests](testing.md).

**Scaffolding (génération de squelette)** — des commandes de génération (`startapp`, `make:serializer`,
`make:viewset`…) qui écrivent des fichiers de départ pour vous, pour que vous ne partiez pas d'une
page blanche. Voir [Scaffolding](scaffolding.md).

---

## Les données et la base de données

**Champ / colonne (field / column)** — un morceau de données sur un modèle, comme le `title` ou le
`published_at` d'un post. « Champ » est le côté Rust ; « colonne » est le côté base de données ; ils
se correspondent un-à-un.

**Clé primaire (primary key)** — l'id unique qui identifie une ligne, généralement un
nombre auto-incrémenté appelé `id`.

**Clé étrangère (foreign key, FK)** — un champ sur un modèle qui pointe vers la ligne d'un autre modèle,
modélisant une relation — p. ex. un `Post` a une clé étrangère `author_id` pointant
vers un `Author`. C'est ainsi que les lignes se référencent les unes les autres.

**NULL / nullable** — `NULL` est le mot de la base de données pour « aucune valeur / vide ». Un
champ **nullable** a le droit d'être vide ; un champ non-nullable est obligatoire.

**ORM** — *Object-Relational Mapper.* La partie du framework qui transforme vos
structs Rust en lignes de base de données et inversement, pour que vous
écriviez vos requêtes en Rust plutôt qu'en SQL à la main. Voir le
[guide de l'ORM](orm.md).

**Transaction** — un groupe de changements en base qui réussissent ou échouent
ensemble. Soit tous sont enregistrés, soit aucun.

**Rollback (annulation)** — annuler tous les changements faits dans une
transaction, comme s'ils n'avaient jamais eu lieu. Les tests s'en servent pour
laisser la base propre. Voir
[Tests de base de données avec rollback](testing.md#tests-de-base-de-données-avec-rollback).

**Tri-dialecte (tri-dialect)** — « fonctionne de la même façon sur les trois bases de données prises en charge » —
PostgreSQL, MySQL et SQLite. Quand une fonctionnalité est tri-dialecte, vous pouvez changer de
base de données sans changer votre code.

---

## Connexion et sécurité

**Session** — la mémoire que garde le serveur d'un utilisateur connecté entre
deux requêtes. Le navigateur ne garde qu'un identifiant dans un cookie ; les
données restent sur le serveur. Voir [Sessions](auth-sessions.md).

**Identifiant opaque** — un identifiant qui ne veut rien dire seul, juste une
longue chaîne aléatoire. Seul le serveur peut retrouver ce qu'il désigne.

**Token (jeton)** — une chaîne qu'un client envoie pour prouver qui il est,
généralement dans l'en-tête `Authorization`. Traitez-le comme un mot de passe.

**Bearer token** — un token envoyé sous la forme `Authorization: Bearer <token>`.
Celui qui le détient (le « porteur ») entre, donc gardez-le secret. Voir
[Clés d'API](auth-api-keys.md).

**JWT** — *JSON Web Token.* Un token qui transporte des données signées (qui est
l'utilisateur, quand le token expire). Le serveur peut le vérifier sans
interroger la base. Voir [JWT](auth-jwt.md).

**Claims** — les données contenues dans un JWT, comme l'identifiant de
l'utilisateur et l'heure d'expiration.

**Sans état (stateless)** — le serveur ne garde rien entre les requêtes. Tout ce
dont il a besoin arrive dans la requête elle-même, comme avec un JWT.

**Token d'accès / token de rafraîchissement** — un token de courte durée utilisé à
chaque requête, plus un token de longue durée qui sert seulement à obtenir un
nouveau token d'accès quand l'ancien expire. Voir [API JWT](auth-jwt-api.md).

**Rotation des tokens** — à chaque utilisation d'un token de rafraîchissement, il
est remplacé par un nouveau et l'ancien cesse de fonctionner. Un token de
rafraîchissement volé ne sert alors qu'une fois.

**Révocation** — annuler un token ou une session avant son expiration, par
exemple à la déconnexion. Voir
[Révocation](auth-jwt-api.md#révocation-et-le-magasin-de-jti).

**Expiration** — le moment où un token, un lien ou une session cesse de
fonctionner.

**Secret / secret partagé** — une valeur privée qui signe ou vérifie des données.
Un secret partagé est connu des deux côtés, comme le client et le serveur. Ne le
mettez jamais dans le code ni dans les logs.

**Hash** — une empreinte à sens unique de données. Vous pouvez vérifier un mot de
passe par rapport à son hash, mais pas retrouver le mot de passe à partir du
hash. Voir [Mots de passe](auth-passwords.md).

**Salt (sel)** — des données aléatoires mélangées à chaque mot de passe avant le
hachage, pour que deux mots de passe identiques donnent des hashs différents.

**Argon2id** — la méthode de hachage de mots de passe qu'utilise Rustango. Elle
est lente exprès, ce qui rend coûteux de deviner des mots de passe.

**Chaîne PHC** — la forme texte standard d'un hash de mot de passe stocké. Elle
contient la méthode, ses réglages, le sel et le hash dans une seule chaîne qui
commence par `$argon2id$`.

**HMAC** — une signature faite à partir de données et d'une clé secrète. Toute
personne qui a la clé peut vérifier que les données n'ont pas été modifiées.
Voir [Signature de requêtes HMAC](auth-hmac.md) et
[Parcours de compte](auth-flows.md).

**Comparaison en temps constant** — comparer deux secrets de façon que cela prenne
toujours le même temps, pour qu'un attaquant n'apprenne rien de la durée de la
vérification.

**Rejeu (replay)** — un attaquant renvoie une requête qu'il a capturée plus tôt.
Une limite de temps sur les requêtes signées le bloque. Voir
[Dérive d'horloge et rejeu](auth-hmac.md#dérive-dhorloge-et-rejeu).

**URL présignée (presigned URL)** — un lien vers un fichier avec une signature et
une expiration intégrées : toute personne qui a le lien peut télécharger (ou
envoyer) ce fichier pendant peu de temps. Voir [Fichiers](files.md).

**CSRF** — *Cross-Site Request Forgery.* Un autre site pousse un navigateur
connecté à envoyer une requête à votre application. Un token CSRF dans chaque
formulaire le bloque. Voir [Protection CSRF](middleware.md#protection-csrf).

**Codename de permission** — le nom d'une permission, écrit `{table}.{action}`,
par exemple `post.add`. Les routes le vérifient pour décider qui peut faire
quoi. Voir [Backends d'authentification](auth-backends.md#permissions--require_perm).

**OAuth** — une façon standard pour une application d'obtenir un accès limité au
nom d'un utilisateur sans voir son mot de passe. C'est la base des boutons
« Se connecter avec … ». Voir [SSO](sso.md).

---

## Cache, fichiers, e-mail et jobs

**Backend (backend de stockage, backend de messagerie, backend de cache)** — le
lieu ou le service réel derrière une interface commune : disque local ou S3 pour
les fichiers, SMTP ou la console pour l'e-mail, mémoire ou Redis pour un cache.
Vous pouvez le changer sans toucher au code appelant. Les backends
d'authentification suivent la même idée : chacun est une façon de reconnaître un
utilisateur. Voir [Backends d'authentification](auth-backends.md).

**Cache** — un stockage rapide qui garde des copies de données lues souvent, pour
ne pas les recalculer ou les rechercher à chaque fois. Voir [Cache](caching.md).

**Clé de cache** — le nom sous lequel une valeur est rangée dans le cache, comme
`post:42:summary`.

**TTL** — *time to live.* Combien de temps une valeur reste dans le cache avant
d'expirer. Voir [TTL et expiration](caching.md#ttl-et-expiration).

**Stockage objet (object storage)** — un service qui stocke des fichiers
(« objets ») via HTTP, comme Amazon S3 ou Azure Blob Storage. Voir
[Fichiers](files.md).

**Mailer** — la partie de votre application qui envoie les e-mails. En
développement, il affiche l'e-mail dans le terminal ; en production, il l'envoie
vraiment. Voir [E-mail](email.md).

**SMTP** — le protocole standard pour envoyer des e-mails à un serveur de
messagerie.

**E-mail transactionnel** — un e-mail que votre application envoie suite à une
action d'un utilisateur, comme une réinitialisation de mot de passe ou un reçu.

**File de jobs (queue)** — une liste de tâches qui attendent d'être exécutées en
arrière-plan, pour qu'une requête puisse répondre tout de suite. Voir
[Jobs en arrière-plan](jobs.md).

**Worker** — une tâche d'arrière-plan qui prend les jobs dans la file et les
exécute.

**Nouvelle tentative et backoff** — relancer un job qui a échoué, en attendant un
peu plus après chaque échec. Voir
[Nouvelles tentatives et backoff](jobs.md#nouvelles-tentatives-et-backoff).

**Dead-letter** — l'endroit où va un job qui a échoué pour de bon, pour que vous
puissiez l'examiner plus tard. Voir
[Le handler dead-letter](jobs.md#le-handler-dead-letter).

---

## Langues

**Locale** — une langue, parfois avec une région, comme `en`, `fr` ou `pt-BR`.
Voir [i18n](i18n.md).

**Catalogue de messages** — le fichier des textes traduits pour une locale.

**Pluralisation** — choisir la bonne forme d'un mot selon un nombre, comme
« 1 fichier » et « 2 fichiers ». Certaines langues ont plus de deux formes. Voir
[Pluralisation](i18n.md#pluralisation).

**Accept-Language** — un en-tête que le navigateur envoie avec les langues que
l'utilisateur préfère. Voir [Négocier la langue](i18n.md#négocier-la-langue).

**RTL** — *right-to-left.* Des langues comme l'arabe et l'hébreu, qui s'écrivent
de droite à gauche. Voir
[Langues de droite à gauche](i18n.md#langues-de-droite-à-gauche).

---

## Temps réel et agents IA

**WebSocket** — une connexion qui reste ouverte pour que le navigateur et le
serveur puissent envoyer des messages à tout moment. Voir
[WebSockets et SSE](websockets.md).

**SSE** — *Server-Sent Events.* Une connexion qui reste ouverte pour que le
serveur pousse des mises à jour vers le navigateur. Les messages vont dans un
seul sens, du serveur vers le navigateur.

**Broadcast (diffusion)** — envoyer un message à tous les auditeurs connectés en
même temps.

**Keep-alive / ping** — un petit message envoyé de temps en temps pour qu'un proxy
ne ferme pas une connexion inactive, et pour repérer une connexion morte.

**Back-pressure (contre-pression)** — ce qui se passe quand un auditeur lit plus
lentement que les messages n'arrivent. L'émetteur doit attendre, mettre en
tampon ou abandonner des messages.

**MCP** — *Model Context Protocol.* Une façon standard pour un agent IA de
découvrir et d'appeler les outils que propose votre application. Voir
[Serveur MCP](mcp.md).

**JSON-RPC** — un protocole simple pour appeler une fonction nommée sur un
serveur, avec la requête et la réponse écrites en JSON. MCP l'utilise.

**Agent** — un programme d'IA, généralement piloté par un grand modèle de
langage, qui appelle des outils pour accomplir un travail.

**Tool, resource, prompt (MCP)** — ce qu'un serveur MCP offre à un agent : un tool
est une action qu'il peut appeler, une resource des données qu'il peut lire, et
un prompt une instruction toute prête.

---

## Multi-tenance (multi-tenancy)

Derrière la fonctionnalité `tenancy`. Passez cette section si vous construisez
une application ordinaire pour un seul client — rien de tout cela ne s'applique.

**Multi-tenance (multi-tenancy)** — faire tourner un seul déploiement qui sert
plusieurs clients, chacun ne voyant que ses propres données. Rustango détermine
à quel client appartient une requête à partir de son nom d'hôte, puis l'aiguille
vers les données de ce client pour le reste de la requête.

**Tenant (ou org)** — un client dans un tel déploiement. Un tenant a un slug
(`acme`), un motif d'hôte (`acme.example.com`) et ses propres utilisateurs.
`Org` est la ligne du registre ; « tenant » est la chose qu'elle décrit.

**Registre (registry)** — la petite base de données qui liste les tenants : qui
ils sont, où vivent leurs données, s'ils sont actifs. Distincte des données de
chaque tenant, et la seule base dont le framework a toujours besoin.

**Mode de stockage (storage mode)** — la façon dont les données d'un tenant sont
séparées de celles de ses voisins : une base de données distincte, ou un schéma
distinct dans une base partagée. Choisi par tenant au provisionnement.

**Opérateur (operator)** — un administrateur du *déploiement* plutôt que d'un
tenant. Les opérateurs créent les tenants et lient les noms d'hôte ; ils ne sont
utilisateurs d'aucun tenant et vivent dans le registre. Voir
[la console opérateur](operator-console.md).

**Console opérateur (operator console)** — l'interface web qu'utilisent les
opérateurs : provisionner des tenants, lier des noms d'hôte, gérer d'autres
opérateurs, lire la piste d'audit. Presque chaque action y est aussi un verbe
`manage`, donc scriptable.

**Provisionnement (provisioning)** — créer un tenant : fabriquer sa base de
données ou son schéma, y appliquer les migrations, et l'inscrire au registre.

---

## Quelques mots de Rust

Vous n'avez pas besoin de connaître Rust pour *lire* la plupart des exemples, mais ces quatre mots
apparaissent partout.

**`struct`** — un ensemble nommé de champs, comme un enregistrement ou une classe qui n'a que
des données. Les modèles et les serializers sont des structs.

**Macro derive (`#[derive(Model)]`, `#[derive(Serializer)]`…)** — une annotation d'une
ligne au-dessus d'un struct qui dit au compilateur d'auto-générer une pile de
code pour vous (le mapping de la base de données, la conversion JSON, …). C'est la magie qui
transforme un simple struct en modèle ou serializer fonctionnel.

**`async` / `.await`** — la façon dont Rust gère le travail qui implique d'attendre (une
requête de base de données, un appel réseau). Une fonction marquée `async` est « awaitable » ; le
`.await` après un appel signifie « attends ici le résultat ». Tout ce qui touche à la
base de données est `async`.

**`Result` / `Option`** — comment Rust rapporte les résultats au lieu de lever des
exceptions. Un `Result` est « un succès *ou* une erreur » ; une `Option` est « une valeur *ou*
rien ». Le `?` que vous voyez après certains appels signifie « si cela a échoué, arrête-toi et
renvoie l'erreur ».
