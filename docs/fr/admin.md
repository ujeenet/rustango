# L'admin

**Rustango** génère une interface d'administration complète à partir de vos
modèles — la même idée que l'admin de Django ou un panneau Laravel
Nova/Filament, mais avec **zéro boilerplate par modèle**. Ajoutez
`#[derive(Model)]`, montez l'admin une fois, et chaque modèle obtient une vue
liste avec recherche, filtres, tri, pagination et actions groupées ; un
formulaire de création/édition regroupé en fieldsets ; l'édition inline des
enfants ; une piste d'audit par ligne ; et une référence de modèle en direct.
Tout ce qui suit est configuré de manière déclarative dans un bloc
`admin(...)` sur le derive, ou avec une poignée de méthodes `Builder` et de
macros d'enregistrement au niveau du module.

[![L'admin auto-généré : une liste de posts avec des facettes de filtre, la recherche, des actions groupées et la pagination — tout à partir d'un seul bloc `admin(...)`](../img/admin.png)](../img/admin.png)

> **Source :** `rustango::admin` (les options du derive `admin(...)`, l'API
> `Builder` et les macros d'enregistrement) — derrière la feature `admin`
> (activée par défaut).
>
> **Version exécutable :** chaque fonctionnalité de cette page est illustrée
> dans un exemple testé et compilable à
> [`crates/rustango/examples/admin_demo`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/admin_demo).
> Les captures d'écran de cette page proviennent de cet exemple. Si un extrait
> semble incorrect, comparez-le à celui-ci.

> **Un terme nouveau ici ?** *modèle*, *fieldset*, *piste d'audit* — voir le
> [glossaire](glossary.md).

---

## Table des matières
- [Le monter](#mount-it) · [La page d'accueil](#the-home-page)
- [Configurer un modèle : le bloc `admin(...)`](#configure-a-model-the-admin-block)
- [La vue liste](#the-list-view) — colonnes, recherche, filtres, hiérarchie de dates, tri, pagination
- [Le formulaire de modification](#the-change-form) — fieldsets, widgets, édition des FK, champs prépeuplés et en lecture seule
- [Inlines](#inlines) · [Actions groupées](#bulk-actions) · [Piste d'audit](#audit-trail)
- [Colonnes calculées et filtres personnalisés](#computed-columns-and-custom-filters)
- [Vues personnalisées, querysets et permissions](#custom-views-querysets-and-permissions)
- [Authentification](#authentication) · [Thème et image de marque](#theming-and-branding)
- [Référence `Builder`](#builder-reference) · [Référence des routes](#routes-reference)
- [La référence de modèle (`__docs`)](#the-model-reference) · [Essayer l'exemple](#try-the-example)

---

## Le monter

> **L'admin est ouvert par défaut.** Il découvre et sert automatiquement
> *chaque* modèle — liste, création, édition, suppression — sans
> authentification jusqu'à ce que vous l'ajoutiez. Ne l'exposez pas
> publiquement avant d'avoir câblé la connexion : voir
> [Authentification](#authentication) ci-dessous.

L'admin est un `axum::Router` que vous construisez à partir d'un pool de base
de données et que vous nichez (« nest ») sous un chemin :

```rust
use rustango::admin;

let admin_router = admin::Builder::new(pool.clone())
    .title("Admin Demo")
    .subtitle("rustango auto-admin showcase")
    .admin_prefix("/admin")          // MUST match the nest path below
    .build();

let api = axum::Router::new().nest("/admin", admin_router);
```

L'auto-admin découvre **automatiquement** chaque `#[derive(Model)]` de votre
binaire via le registre `inventory` — vous n'enregistrez pas les modèles un
par un. Ouvrez `http://localhost:8080/admin` et ils apparaissent regroupés
dans la barre latérale.

> **`admin_prefix` doit être égal au chemin de nesting.** L'admin construit
> ses liens et les actions de formulaire à partir de `admin_prefix` (par
> défaut `/__admin`). Si vous nichez sous `/admin` mais laissez le préfixe par
> défaut, chaque lien renvoie une 404. Réglez-les de façon identique.

> **Lier le registre.** L'enregistrement inventory n'est lié dans le binaire
> final que si les types de modèles sont référencés quelque part. Un crate
> bibliothèque dont les modèles ne sont pas utilisés ailleurs peut avoir
> besoin d'un petit coup de pouce `let _ = std::any::type_name::<Post>();`
> dans `main` (c'est ce que fait l'exemple) — sinon le linker les élimine et
> ils n'apparaissent jamais.

### La page d'accueil

La racine de l'admin (`GET /<prefix>`) liste chaque modèle enregistré,
regroupé par app, avec le nom de table et le nombre de champs de chaque
modèle — plus un flux **Actions récentes** des derniers changements audités.

[![La page d'accueil de l'admin : chaque modèle enregistré regroupé par app avec le nombre de tables et de champs, et un flux d'activité des actions récentes](../img/admin-home.png)](../img/admin-home.png)

---

## Configurer un modèle : le bloc `admin(...)`

Tout ce qui concerne l'apparence d'un modèle est réglé dans un bloc
`admin(...)` sur le derive. Voici le `Post` de démonstration de l'exemple, qui
exerce presque tous les réglages :

```rust
#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display       = "id, title, author_id, status, view_count, published_at",
        list_display_links = "id, title",
        list_filter        = "status, author_id",
        search_fields      = "title, body",
        search_help_text   = "Search posts by title or body",
        ordering           = "-published_at",
        list_per_page      = 10,
        date_hierarchy     = "published_at",
        fieldsets          = "Content: title, body, status | Publishing: author_id, published_at, view_count",
        actions            = "publish, archive",
    ),
    audit(track = "title, body, status"),
)]
pub struct Post { /* … */ }
```

`display = "title"` (sur le modèle, en dehors de `admin(...)`) définit le
libellé humain utilisé partout où une ligne est référencée — les colonnes FK
dans les listes d'autres modèles, le fil d'Ariane, le titre de la page de
détail.

### Toutes les options `admin(...)`

| Clé | Exemple | Ce que ça fait |
|---|---|---|
| `list_display` | `"id, title, status"` | Colonnes affichées dans la liste, dans l'ordre. Les colonnes FK affichent la valeur `display` de la cible. Les colonnes calculées (voir plus bas) peuvent être nommées ici. Vide = chaque champ scalaire. |
| `list_display_links` | `"id, title"` | Quelles cellules de `list_display` renvoient vers la page de détail. Doit être un sous-ensemble de `list_display`. |
| `list_filter` | `"status, author_id"` | Cartes de facettes dans la colonne de droite — valeurs distinctes + compteurs, cliquez pour filtrer. Fonctionne sur les colonnes scalaires et FK. |
| `search_fields` | `"title, body"` | Champs que la boîte de recherche `?q=` compare (`ILIKE`/`LIKE` insensible à la casse). |
| `search_help_text` | `"Search by title"` | Légende affichée à côté de la boîte de recherche. |
| `ordering` | `"-published_at, id"` | Tri par défaut. Préfixe `-` = DESC ; nu = ASC. Plusieurs clés séparées par des virgules. |
| `list_per_page` | `10` | Taille de page (par défaut 50). |
| `date_hierarchy` | `"published_at"` | Bandeau de forage année → mois → jour au-dessus de la liste, sur une colonne Date/DateTime. |
| `fieldsets` | `"Content: title, body \| Meta: status"` | Regroupe le formulaire de modification en sections titrées. La barre `\|` sépare les sections, la virgule sépare les champs ; la légende `Title:` est optionnelle. |
| `actions` | `"publish, archive"` | Actions groupées proposées dans le sélecteur d'actions de la liste (chacune nécessite un gestionnaire enregistré — voir [Actions groupées](#bulk-actions)). |
| `readonly_fields` | `"created_at"` | Champs affichés en texte (sans saisie) sur le formulaire de modification. |
| `raw_id_fields` | `"author_id"` | Champs FK édités via une saisie d'id brut + un lien de recherche (bien adapté aux grandes tables cibles). |
| `autocomplete_fields` | `"author_id"` | Champs FK édités via un typeahead Ajax adossé à l'endpoint `__autocomplete` de la cible. |
| `prepopulated_fields` | `"slug:title"` | Remplit automatiquement un champ en slugifiant un autre au fur et à mesure de la saisie (`cible:source` ; combinez les sources avec `+`). |
| `list_select_related` | `"all"` / `"none"` / `"author_id"` | Contrôle la JOIN automatique des colonnes FK dans la requête de liste. `"all"` (par défaut) joint chaque FK ; `"none"` désactive ; une liste CSV restreint aux FK nommées. |
| `formfield_overrides` | `"status:textarea"` | Remplace le widget de formulaire d'un champ (`champ:widget`) — voir le [tableau des widgets](#form-widgets). |
| `actions_on_top` | `true` | Affiche la barre d'actions groupées au-dessus de la liste (par défaut `true`). |
| `actions_on_bottom` | `false` | Affiche une seconde barre d'actions sous la liste (par défaut `false`). |

---

## La vue liste

`GET /<prefix>/<table>` affiche la liste. À partir du seul bloc `admin(...)`
ci-dessus, vous obtenez des colonnes triables, une boîte de recherche avec son
texte d'aide, les cartes de facettes statut/auteur avec des compteurs en
direct, le forage par date, la pagination à 10 par page, et le sélecteur
d'actions publier/archiver.

**Filtrage.** Cliquez sur une valeur dans une carte de facette `list_filter`
pour restreindre la liste ; le filtre actif s'affiche sous forme de puce avec
un lien **effacer**, et le nombre de lignes ainsi que les compteurs de
facettes se mettent à jour. Les filtres, la recherche, le tri et la hiérarchie
de dates se combinent tous dans la chaîne de requête et peuvent être associés.

[![La liste des posts filtrée par status=published : une puce de filtre actif, la facette correspondante mise en évidence, la boîte de recherche et le sélecteur d'actions groupées](../img/admin-list-filtered.png)](../img/admin-list-filtered.png)

**Tri.** Cliquez sur l'en-tête d'une colonne pour trier ; cliquez à nouveau
pour inverser le sens (`?sort=col&order=asc|desc`). La valeur par défaut vient
de `ordering`.

**Pagination.** `list_per_page` fixe la taille de page ; naviguez avec
`?page=N`. Pour les très grandes tables, enregistrez-les avec
`Builder::skip_count_for([...])` pour éviter le `SELECT COUNT(*)` (le
pagineur affiche alors « Page N » sans total global) ; un `?count=skip` par
requête fait la même chose ponctuellement.

**Recherche.** Quand `search_fields` est défini, une boîte de recherche
apparaît et fait correspondre ces champs avec `ILIKE` (PostgreSQL) / `LIKE`
(MySQL, SQLite). `search_help_text` s'affiche comme légende.

**Hiérarchie de dates.** Avec `date_hierarchy` défini, un fil d'Ariane
année → mois → jour se place au-dessus du tableau ; forer dans cette
hiérarchie ajoute des filtres de plage semi-ouverte sur cette colonne en
utilisant l'extraction de date tri-dialecte (PostgreSQL `EXTRACT`, MySQL,
SQLite `strftime`).

---

## Le formulaire de modification

`GET /<prefix>/<table>/new` (création) et `GET /<prefix>/<table>/<pk>/edit`
(édition) affichent le formulaire. `fieldsets` regroupe les saisies en
sections titrées ; sans lui, tous les champs modifiables apparaissent en un
seul bloc.

[![Le formulaire de modification de Post regroupé en fieldsets Content et Publishing, chaque champ avec le widget de saisie adapté à son type](../img/admin-fieldsets.png)](../img/admin-fieldsets.png)

Soumettre un formulaire valide la saisie, écrit la ligne, enregistre une
entrée d'audit, et redirige vers la vue **détail** en lecture seule
(`GET /<prefix>/<table>/<pk>`), qui affiche chaque champ ainsi que les
inlines et la carte d'audit (ci-dessous). Les boutons **Éditer** et
**Supprimer** de la page de détail mènent au formulaire et à la confirmation
de suppression.

### Widgets de formulaire

Chaque champ affiche une saisie correspondant à son type par défaut —
`<input type="number">` pour les entiers, `type="date"`/`datetime-local` pour
les dates, `type="checkbox"` pour les booléens, un `<textarea>` pour les
chaînes longues, un `<select>` pour les colonnes FK, et ainsi de suite.
Remplacez-le par champ avec `formfield_overrides = "champ:widget"` :

| Widget | S'applique à | Affiche |
|---|---|---|
| `textarea` | String | `<textarea>` multi-lignes |
| `password` | String | `<input type="password">` |
| `email` | String | `<input type="email">` |
| `url` | String | `<input type="url">` |
| `color` | String | `<input type="color">` |
| `slug` | String | saisie texte avec un motif de slug |
| `ipaddress` | String | saisie texte avec un motif d'IP |
| `json` | Json | `<textarea>` monospace |
| `hidden` | any | `<input type="hidden">` |

### Éditer les clés étrangères

Les colonnes FK ont trois modes d'édition :

- **Par défaut** — un `<select>` peuplé à partir de la table cible, affichant
  la valeur `display` de chaque ligne.
- **`raw_id_fields`** — une simple saisie d'id plus un lien de recherche ;
  préférable quand la table cible est trop grande pour être énumérée dans une
  liste déroulante.
- **`autocomplete_fields`** — un typeahead Ajax qui interroge les
  `search_fields` du modèle cible via
  `GET /<prefix>/<target>/__autocomplete?q=…`.

### Champs prépeuplés et en lecture seule

`prepopulated_fields = "slug:title"` émet du JS côté client qui slugifie le
champ source vers la cible au fur et à mesure de la saisie (combinez
plusieurs sources avec `+`, p. ex. `"slug:section+title"`).
`readonly_fields` affiche les champs nommés en texte échappé sur le
formulaire au lieu de saisies.

---

## Inlines

Les inlines affichent les lignes d'un modèle enfant sur la page du parent
(inlines Django). Enregistrez-en un au niveau du module :

```rust
rustango::register_admin_inline!(
    parent = "posts",
    child  = "comments",
    fk     = "post_id",                                     // child column → parent PK
    kind   = rustango::admin::inlines::InlineKind::Tabular, // or Stacked
    label  = "Comments",
    fields = &["author_name", "body", "created_at"],
);
```

Sur la page **détail** du parent, les enfants s'affichent sous forme de
tableau en lecture seule ; sur la page **édition**, ils deviennent un
FormSet modifiable (ajouter / modifier / supprimer des lignes sur place).
Options : `kind` (`Tabular` — une ligne de tableau par enfant, ou `Stacked` —
un fieldset par enfant), `label`, `fields` (par défaut : chaque scalaire sauf
la FK), `extra` (lignes vierges proposées pour l'ajout), `max_num`, et
`readonly_fields`.

[![La page de détail d'un post : champs en lecture seule, le tableau inline Comments, et la carte de piste d'audit montrant l'entrée de création sous forme de diff JSON](../img/admin-detail.png)](../img/admin-detail.png)

Pour les lignes enfants rattachées par une clé étrangère générique (paire
type-de-contenu + pk-d'objet) plutôt que par une seule colonne FK, utilisez
`register_admin_inline_generic!(parent, child, ct = "content_type_id", pk =
"object_pk", …)` — les mêmes options par ailleurs.

---

## Actions groupées

Nommez les actions dans `admin(actions = "...")`, puis enregistrez un
gestionnaire par action sur le `Builder`. Le gestionnaire reçoit le pool et
les clés primaires des lignes sélectionnées :

```rust
use rustango::core::SqlValue;

let admin_router = admin::Builder::new(pool)
    .register_action("posts", "publish", |pool, pks| {
        Box::pin(async move {
            let ids: Vec<String> = pks.iter().filter_map(|v| match v {
                SqlValue::I64(n) => Some(n.to_string()),
                SqlValue::I32(n) => Some(n.to_string()),
                _ => None,
            }).collect();
            if !ids.is_empty() {
                let sql = format!("UPDATE posts SET status='published' WHERE id IN ({})", ids.join(","));
                rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
            }
            Ok(())
        })
    })
    .register_action("posts", "archive", /* … */)
    .build();
```

Choisissez les lignes avec les cases à cocher, sélectionnez l'action dans le
sélecteur, et soumettez (`POST /<prefix>/<table>/__action`).
`delete_selected` est intégré — vous ne l'enregistrez pas. Un nom d'action
listé dans `admin(actions = ...)` sans gestionnaire enregistré n'apparaît
simplement pas.

---

## Piste d'audit

Ajoutez `audit(track = "field1, field2")` à un modèle et chaque création,
mise à jour et suppression est enregistrée dans la table
`rustango_audit_log` (créée pour vous lors de l'exécution de `migrate`).
Seuls les modèles portant un attribut `audit(...)` sont journalisés ; `track`
sélectionne les champs capturés dans le diff (omettez-le pour suivre tous les
scalaires).

```rust
#[rustango(table = "posts", audit(track = "title, body, status"))]
```

Chaque entrée stocke la table, la clé primaire, l'opération, la source, un
diff par champ (`{before, after}`) en JSON, l'acteur, un horodatage, et une
empreinte inviolable. Deux endroits l'exposent :

- La **page de détail** du modèle gagne une carte **Piste d'audit** listant
  les changements récents (qui, quand, et le diff), avec un lien **Voir
  l'historique complet** (visible dans la [capture de détail](#inlines)
  ci-dessus).
- La vue **Activité** de la barre latérale (`GET /<prefix>/__audit`) est un
  flux transversal, du plus récent au plus ancien, avec des cartes de
  facettes par entité / opération / source et un formulaire de nettoyage pour
  purger les entrées plus anciennes que N jours (lui-même enregistré comme
  une entrée d'audit).

[![Le flux Activité : chaque changement audité à travers les modèles avec des diffs JSON, des cartes de facettes par table/opération/source, et un formulaire de nettoyage](../img/admin-audit.png)](../img/admin-audit.png)

---

## Colonnes calculées et filtres personnalisés

Quand le bloc déclaratif ne suffit pas, deux macros au niveau du module
étendent la vue liste :

**Colonnes calculées** — une colonne dérivée, hors base de données :

```rust
rustango::register_admin_computed!(
    "posts", "word_count", "Words",
    |row| row.get("body").and_then(|v| v.as_str())
             .unwrap_or_default().split_whitespace().count().to_string(),
);
// then add `word_count` to admin(list_display = "...").
```

La closure reçoit la ligne comme `serde_json::Value` et renvoie du HTML
pré-échappé. Une forme à 5 arguments ajoute `link = |row| Option<String>`
pour envelopper la cellule dans un `<a>`.

**Filtres de liste personnalisés** — une logique de filtrage que les
facettes automatiques ne peuvent pas exprimer :

```rust
fn by_status(value: &str) -> Vec<rustango::core::Filter> { /* map value → predicates */ }

rustango::register_admin_list_filter!(
    "posts", "status", "Status",
    &[("draft", "Drafts"), ("published", "Published")],   // (value, label) choices
    by_status,                                            // fn(&str) -> Vec<Filter>
);
```

---

## Vues personnalisées, querysets et permissions

Trois macros d'enregistrement supplémentaires reflètent les hooks
`ModelAdmin` de Django :

- **Pages d'admin personnalisées** —
  `register_admin_view!("posts", "duplicate", Method::POST, "Duplicate", handler)`
  monte une page/action supplémentaire à `/<prefix>/posts/duplicate`. Le
  gestionnaire est une `fn(Pool, Request) -> Response` asynchrone. (Les
  suffixes réservés comme `new`, `__action`, `__autocomplete`, `{pk}`,
  `{pk}/edit`, `{pk}/delete` sont ignorés avec un avertissement.)
- **Restriction de queryset** —
  `register_admin_queryset!("posts", hook)` où
  `hook: fn(&Parts) -> Vec<Filter>` restreint ce qu'une requête peut voir
  (p. ex. seulement les lignes de l'utilisateur courant). Plusieurs hooks sur
  une table se combinent.
- **Permissions au niveau de la ligne** —
  `register_admin_object_permission!("posts", "change", check)` où
  `check: fn(&Parts, Option<&Value>) -> bool` autorise ou refuse par ligne.
  Les gestionnaires intégrés consultent les actions `add`, `change`,
  `delete` et `view` ; plusieurs hooks se combinent avec un ET logique.

Pour un contrôle d'accès plus grossier basé sur des codenames,
`Builder::with_user_perms([...])` conditionne chaque table sur
`{table}.view` / `.add` / `.change` / `.delete` : l'absence de `view` masque
le modèle et renvoie une 404 sur les accès directs, l'absence de `change` le
rend en lecture seule, et l'absence de `add` / `delete` retire ces boutons.

---

## Authentification

Par défaut, l'admin est **ouvert** — quiconque peut y accéder peut l'utiliser.
Verrouillez-le de l'une des deux façons suivantes :

- **Authentification par session (intégrée).** `Builder::with_session_auth(secret)`
  monte `/login` + `/logout` (et une page optionnelle de changement de mot de
  passe `/account/password`) et enveloppe chaque autre route dans un
  middleware qui redirige les requêtes anonymes vers le formulaire de
  connexion. Les identifiants vivent dans la table
  `rustango_admin_users` (`username`, `password_hash` argon2,
  `is_superuser`, `active`, `created_at`) ; changer un mot de passe révoque
  les autres sessions de cet utilisateur. Une double authentification TOTP
  optionnelle est disponible derrière la feature `totp`, avec l'inscription
  sous `/account/totp`.

  ```rust
  let admin = admin::Builder::new(pool)
      .with_session_auth(session_secret)
      .secure_cookies(true)              // HTTPS-only cookie in production
      .build();
  ```

- **Le protéger avec votre propre authentification.** Laissez l'admin ouvert
  et placez une authentification HTTP Basic, OAuth2, ou un SSO d'entreprise
  devant le chemin de nesting avec votre propre middleware.

Lorsque l'authentification par session est active, le pied de la barre latérale
affiche une ligne **« Connecté en tant que _username_ »** et un bouton
**Déconnexion** (un formulaire `POST`). Les admins autonomes postent vers
`{admin_prefix}/logout` par défaut ; un admin de tenant se trouve derrière la
route de déconnexion propre à la couche de multi-tenancy, alors pointez le
bouton dessus avec `Builder::logout_url` :

```rust
let admin = admin::Builder::new(pool)
    .with_session_auth(session_secret)
    .logout_url("/staff-logout")       // POST target for the sidebar Logout button
    .build();
```

Le builder d'admin de tenant câble cela automatiquement sur son
`RouteConfig::logout_url`, de sorte que le bouton atteint toujours une route qui
existe.

---

## Thème et image de marque

| Méthode | Effet |
|---|---|
| `.theme_mode("light" \| "dark" \| "auto")` | Thème de couleur par défaut (définit `data-theme` sur `<html>`). |
| `.title(s)` / `.subtitle(s)` | Texte d'en-tête de la barre latérale. |
| `.brand_logo_url(url)` | Logo affiché au-dessus du titre. |
| `.brand_name(s)` / `.brand_tagline(s)` | Surcharges par tenant du titre/sous-titre. |
| `.tenant_brand_css(css)` | Un bloc CSS `:root{…}` de variables déjà construit, inliné pour des palettes par tenant. |
| `.from_settings(pool, &settings)` | Construit l'image de marque + la visibilité à partir des sections `[admin]` / `[brand]` de votre fichier de configuration. |

`from_settings` lit `admin.title`, `admin.subtitle`, `admin.logo_url`,
`admin.theme_mode`, `admin.url_prefix`, `admin.allowed_tables`,
`admin.read_only_tables`, en se repliant sur la section `[brand]`, et met
`secure_cookies` à `true` par défaut. Les appels impératifs au `Builder`
après cela l'emportent toujours.

---

## Référence `Builder`

Chaque méthode de `admin::Builder` (chacune renvoie `Self` pour le chaînage
sauf indication contraire) :

| Méthode | Objet |
|---|---|
| `new(pool)` | Construit à partir de n'importe quel pool (PostgreSQL / MySQL / SQLite). Par défaut : préfixe `/__admin`, cookies de développement. |
| `from_settings(pool, &settings)` | Construit à partir d'une configuration analysée (feature `config`). |
| `title(s)` / `subtitle(s)` | En-tête / sous-en-tête de la barre latérale. |
| `admin_prefix(p)` | Préfixe d'URL — **doit correspondre au chemin de nesting**. Par défaut `/__admin`. |
| `audit_url(u)` | Chemin de la vue activité/audit. Par défaut `/__audit`. |
| `static_url(u)` | Préfixe pour les ressources embarquées (favicon, logo). Par défaut `/__static__`. |
| `change_password_url(u)` | Chemin de la page libre-service de changement de mot de passe (ajoute le lien dans la barre latérale). |
| `show_only([tables])` | Liste blanche des tables qui apparaissent ; les autres renvoient une 404 et sont masquées. |
| `read_only([tables])` | Affiche ces tables mais interdit la création/édition/suppression. |
| `read_only_all()` | Marque **toutes** les tables en lecture seule. |
| `skip_count_for([tables])` | Évite `COUNT(*)` sur les tables volumineuses (le pagineur affiche « Page N »). |
| `with_user_perms([codenames])` | Conditionne les tables sur `{table}.view/add/change/delete`. |
| `register_action(table, name, handler)` | Enregistre un gestionnaire d'action groupée. |
| `with_session_auth(secret)` | Exige une connexion par cookie (`/login` + `/logout`). |
| `secure_cookies(bool)` | Définit le drapeau `Secure` (HTTPS uniquement) sur le cookie de session. |
| `theme_mode(m)` | `"light"` / `"dark"` / `"auto"`. |
| `brand_logo_url(url)` | Logo au-dessus du titre. |
| `brand_name(s)` / `brand_tagline(s)` | Surcharges de marque par tenant. |
| `tenant_brand_css(css)` | Bloc CSS de variables par tenant. |
| `impersonated_by(operator_id)` | Affiche une bannière d'emprunt d'identité (console opérateur). |
| `tenant_mode()` | Masque les modèles à portée registre (défini automatiquement pour les admins de tenant). |
| `build()` | Finalise et renvoie le `axum::Router`. |

---

## Référence des routes

Tous les chemins sont relatifs à `admin_prefix` :

| Chemin | Méthode | Quoi |
|---|---|---|
| `/` | GET | Accueil — index des modèles + actions récentes. |
| `/<table>` | GET | Vue liste (recherche, filtres, tri, pagination). |
| `/<table>` | POST | Soumission de création. |
| `/<table>/new` | GET | Formulaire de création. |
| `/<table>/<pk>` | GET | Vue détail (lecture seule), avec inlines + carte d'audit. |
| `/<table>/<pk>` | POST | Soumission de mise à jour. |
| `/<table>/<pk>/edit` | GET | Formulaire de modification. |
| `/<table>/<pk>/delete` | POST | Suppression (après confirmation). |
| `/<table>/__action` | POST | Exécute une action groupée sur les PK sélectionnées. |
| `/<table>/__autocomplete` | GET | JSON du typeahead FK (`?q=`). |
| `/__docs` | GET | Référence de modèle. |
| `/__audit` (ou `audit_url`) | GET | Flux d'activité + nettoyage. |
| `/login`, `/logout` | GET/POST | Authentification par session (si activée). |
| `/account/password`, `/account/totp` | GET/POST | Changement de mot de passe libre-service / inscription TOTP. |

Les routes personnalisées enregistrées avec `register_admin_view!` se
montent à `/<table>/<suffix>`.

---

## La référence de modèle

Chaque admin embarque une référence de modèle en direct (l'admindocs de
Django) à `<prefix>/__docs` — un catalogue en lecture seule de chaque modèle
enregistré avec ses champs, colonnes, types, drapeaux (PK, unique, …) et
relations. Rien à configurer ; elle est générée à partir de vos modèles, donc
elle ne dérive jamais du schéma.

[![La référence de modèle : les champs de chaque modèle avec le nom de colonne, le type Rust, les drapeaux et les relations — générée à partir des modèles](../img/admin-model-reference.png)](../img/admin-model-reference.png)

---

## Essayer l'exemple

```bash
cd crates/rustango/examples/admin_demo
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/admin_demo
cargo run -- migrate     # tables + the audit-log table
cargo run                # seeds demo data, serves the admin at /admin
```

Puis ouvrez <http://localhost:8080/admin> et cliquez sur **Posts** pour voir
les filtres, la recherche, la hiérarchie de dates, les actions, les
fieldsets, les commentaires en inline, la piste d'audit, et la référence de
modèle — chaque capture d'écran de cette page — au même endroit.


---

## Voir aussi

- [Le cookbook de l'ORM](orm.md) — les modèles à partir desquels l'admin est généré (y compris la piste d'audit partagée).
- [Vues HTML](html-views.md) — les vues génériques basées sur des classes sur lesquelles l'admin est construit.
- [Backends d'authentification](auth-backends.md) · [Sessions](auth-sessions.md) — sécuriser l'admin derrière une connexion.
- [Guide de sécurité](security.md) — durcir avant de l'exposer.
