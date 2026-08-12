# Glossaire

Une référence en langage simple des mots utilisés dans cette documentation. Si un
terme d'un guide vous est inconnu, cherchez-le ici d'abord. Les définitions sont
volontairement informelles — les guides approfondis donnent les détails précis.

Si vous n'avez jamais construit d'API web auparavant, lisez [Les bases des API web](#web-api-basics)
de haut en bas ; c'est une introduction de cinq minutes. Tout le reste est fait pour
être consulté au fur et à mesure.

## Table des matières

- [Les bases des API web](#web-api-basics) — ce qu'est une API, en termes de tous les jours
- [Les briques de Rustango](#rustango-building-blocks) — les pièces que vous assemblez
- [Les données et la base de données](#data-and-the-database)
- [Quelques mots de Rust](#a-few-rust-words) — pour que les blocs de code ne fassent pas peur
- [Frameworks auxquels nous comparons](#frameworks-we-compare-to)

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
| `401` / `403` | Non connecté / non autorisé |
| `404` | Not found (introuvable) |
| `429` | Too many requests — ralentissez |
| `500` | Le serveur a rencontré une erreur |

**CRUD** — *Create, Read, Update, Delete* (créer, lire, mettre à jour, supprimer). Les quatre choses de base que vous faites aux données.
Une « API CRUD » signifie simplement une API qui vous permet de faire les quatre. Voir
[ViewSets](viewsets.md), qui construisent une API CRUD complète à partir d'une seule déclaration.

**Chaîne de requête / paramètre de requête (query string / query parameter)** — la partie `?key=value` à la fin d'une URL,
utilisée pour filtrer, rechercher, trier ou paginer les résultats — p. ex.
`/api/posts?status=published&page=2`. Chaque `key=value` est un paramètre.

**Pagination** — découper une longue liste de résultats en pages pour qu'une réponse ne soit pas
énorme. L'**enveloppe** est l'emballage autour de la page qui vous indique aussi les
totaux — p. ex. `{"count": 137, "page": 2, "results": [ … ]}`. Voir
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

**Template (gabarit)** — un fichier avec des espaces réservés (Rustango utilise [Tera](https://keats.github.io/tera/),
très semblable aux templates Django ou à Jinja) que le serveur remplit de données pour produire
une page HTML. `{{ post.title }}` insère une valeur ; `{% for … %}` fait une boucle.

**Router / montage (mount)** — le router associe les URL entrantes au code qui les
traite. *Monter* un ViewSet signifie « attacher ses endpoints à votre application à un chemin
donné », p. ex. monter l'API des posts sur `/api/posts`. Voir [URLs et routage](urls.md).

**Middleware (une « couche » / layer)** — du code qui s'exécute *autour* de chaque requête — avant votre
handler et après lui — pour des préoccupations transversales comme la journalisation, la limitation de débit, les
en-têtes de sécurité ou le CSRF. « Layer » est le mot de Rustango pour une pièce de
middleware. Voir le [guide du Middleware](middleware.md).

**Pool (ou executor)** — la connexion à la base de données que votre code utilise pour lire et
écrire. Rustango vous demande de passer le pool explicitement à chaque appel à la base de données
(plutôt que de le cacher dans un global), pour qu'il soit toujours clair ce qui touche à la
base de données. Vous verrez `&pool` comme dernier argument des appels de l'ORM.

**QuerySet** — une requête de base de données que vous construisez étape par étape en Rust
(`Post::objects().filter(...).order_by(...)`) avant de l'exécuter. Elle est paresseuse (lazy) :
rien ne touche la base de données tant que vous ne la `fetch`ez pas.

**Feature flag (drapeau de fonctionnalité)** — un interrupteur marche/arrêt, défini dans `Cargo.toml`, qui inclut ou
exclut un morceau du framework à la compilation. Il vous permet de garder votre application petite
en ne compilant que ce que vous utilisez. La plupart des fonctionnalités sont activées par défaut.

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

**Tri-dialecte (tri-dialect)** — « fonctionne de la même façon sur les trois bases de données prises en charge » —
PostgreSQL, MySQL et SQLite. Quand une fonctionnalité est tri-dialecte, vous pouvez changer de
base de données sans changer votre code.

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

---

## Frameworks auxquels nous comparons

Cette documentation dit parfois « comme X » pour aider les lecteurs venant d'autres
écosystèmes. Les comparaisons sont un bonus — vous n'en avez jamais besoin pour suivre un guide.

**Django** — un framework web Python populaire. Rustango emprunte beaucoup de sa forme
(modèles, migrations, une interface d'administration, les commandes `manage`).

**DRF (Django REST Framework)** — l'extension de Django pour construire des API REST.
Les serializers et ViewSets de Rustango en sont inspirés, donc « à la manière de DRF » signifie
« disposé de la façon dont DRF le fait » — p. ex. des erreurs de validation renvoyées sous forme d'objet JSON
indexé par nom de champ.

**Laravel / Rails** — des frameworks web PHP et Ruby populaires, mentionnés pour la même
raison « si vous avez utilisé ceci, cela vous semblera familier ».
