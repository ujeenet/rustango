# Serializers

Un serializer transforme une instance de modèle en une forme typée, prête pour le
JSON — et inversement à l'entrée. C'est la réponse de **Rustango** à un
`ModelSerializer` de Django REST Framework ou à une API Resource de Laravel :
déclarer une structure, annoter ses champs, et l'on obtient une sortie contrôlée
(renommage, masquage, calcul, imbrication), une validation au niveau du champ et
de l'objet, ainsi qu'un point d'ancrage propre vers les ViewSets.

Une chose à intégrer d'emblée, car elle diffère de DRF : un serializer Rustango
**façonne les données, il ne les persiste pas**. Il n'existe pas de
`serializer.save()` qui écrit dans la base de données — c'est l'ORM qui s'en
charge. Le serializer met en correspondance un modèle avec du JSON (`from_model` →
`to_value`), déclare quels champs sont accessibles en écriture, et valide. On le
compose avec l'ORM et les ViewSets plutôt que d'acheminer les écritures *à
travers* lui.

> **Un terme nouveau ici ?** — *serializer*, *model*, *ORM*, *DRF* ? Le
> [glossaire](glossary.md) définit chacun en langage clair.

[![Un serializer Rustango : read_only, renommage via source, un champ méthode calculé, une FK imbriquée et un champ write_only — déclarés sur une seule structure](img/serializers.png)](img/serializers.png)

> **Source :** `rustango::serializer` (`ModelSerializer`, `#[derive(Serializer)]`,
> les attributs de champ `#[serializer(...)]`) — derrière la feature `serializer`
> (activée par défaut).
>
> **Versions exécutables :** le serializer minimal est fourni dans l'exemple testé
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog/src/post_serializer.rs),
> et le comportement complet du derive est couvert par les tests unitaires du
> framework lui-même — `crates/rustango/tests/serializer_derive.rs` et
> `serializer_cross_validate.rs`. Si un extrait semble incorrect, comparer avec
> eux.

---

## Table des matières
- [Démarrage rapide](#quick-start) · [Le trait `ModelSerializer`](#the-modelserializer-trait)
- [Attributs de champ](#field-attributes) — la référence complète
- [Champs calculés](#computed-fields) · [Serializers imbriqués](#nested-serializers) · [Collections](#collections-many) · [Champs slug](#slug-related-fields)
- [Validation](#validation) · [Validation d'unicité combinée](#unique-together-validation)
- [Sortie hyperliée](#hyperlinked-output) · [Sérialiser des listes](#serializing-lists)
- [Utiliser un serializer avec un ViewSet](#using-a-serializer-with-a-viewset) · [Valider dans un handler personnalisé](#validating-in-a-custom-handler)
- [OpenAPI](#openapi-schemas) · [Scaffolding](#scaffolding) · [Ajustements et limites](#tweaks-and-current-limits)

---

## Démarrage rapide

Un serializer est une simple structure avec `#[derive(Serializer)]` et un
`#[serializer(model = …)]` pointant vers le modèle qu'elle met en correspondance.
Elle nécessite deux derives compagnons : `serde::Deserialize` (pour pouvoir aussi
parser le JSON entrant) et `Default` (pour que les champs exclus/optionnels
puissent être initialisés).

```rust
use rustango::Serializer;
use rustango::serializer::ModelSerializer;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,

    #[serializer(source = "body")]      // JSON key `content`, read from model.body
    pub content: String,

    #[serializer(read_only)]            // in output, never accepted on write
    pub published_at: Auto<DateTime<Utc>>,
}
```

Utilisation :

```rust
let post = Post::objects().find(42, &pool).await?.expect("post 42");

let one  = PostSerializer::from_model(&post).to_value();   // a JSON object
let many = PostSerializer::many_to_value(&posts);          // a JSON array
```

`from_model` clone les champs du modèle dans la structure (en respectant les
attributs ci-dessous) ; `to_value` la sérialise (en ignorant les champs
`write_only`). Voilà toute la boucle centrale.

---

## Le trait `ModelSerializer`

`#[derive(Serializer)]` implémente `ModelSerializer` (plus un `serde::Serialize`
qui respecte `write_only`, et une impl `OpenApiSchema` sous la feature `openapi`).
La surface du trait :

| Méthode | Signature | Notes |
|---|---|---|
| `from_model` | `fn(model: &Self::Model) -> Self` | Met en correspondance un modèle → serializer. Généré ; non redéfinissable. |
| `to_value` | `fn(&self) -> serde_json::Value` | Sérialise en JSON (ignore `write_only`). Redéfinissable. |
| `many` | `fn(&[Self::Model]) -> Vec<Self>` | `from_model` par lot. Redéfinissable. |
| `many_to_value` | `fn(&[Self::Model]) -> serde_json::Value` | Lot → tableau JSON. Redéfinissable. |
| `writable_fields` | `fn() -> &'static [&'static str]` | Noms des champs du serializer acceptés en écriture (exclut `read_only`, `skip`, `method`, `nested`, `many`, `slug`). |
| `writable_source_fields` | `fn() -> &'static [&'static str]` | Les **colonnes du modèle** des champs accessibles en écriture (résolues via `source`). Le chemin d'écriture du ViewSet ne persiste que celles-ci. Généré. |
| `from_writable_json` | `fn(&Value) -> Result<Self, FormErrors>` | Construit une instance à partir d'un corps de requête en n'utilisant que les champs accessibles en écriture (le reste prend sa valeur par défaut) ; les erreurs de parsing par champ → `FormErrors`. Généré. |
| `validate` | `fn(&self) -> Result<(), FormErrors>` | Exécute les validateurs déclarés par champ + inter-champs. Sans effet quand aucun n'est déclaré ; redéfinissable. |

Il n'y a délibérément **aucun** `create` / `update` / `save` sur le trait — les
écritures passent par l'ORM (`model.save(&pool)`). Quand un serializer est câblé
dans un [ViewSet](viewsets.md), le chemin create/update utilise
`from_writable_json()` + `validate()` + `writable_source_fields()` pour valider et
filtrer la requête avant l'enregistrement.

---

## Attributs de champ

Tout est contrôlé par `#[serializer(...)]` sur chaque champ. L'ensemble complet :

| Attribut | Ce que fait `from_model` | Dans la sortie JSON ? | Accessible en écriture ? |
|---|---|---|---|
| *(aucun)* | met en correspondance depuis le modèle | oui | oui |
| `read_only` | met en correspondance depuis le modèle | oui | **non** |
| `write_only` | `Default::default()` | **non** | oui |
| `source = "x"` | met en correspondance depuis `model.x` (renomme) | oui | oui |
| `skip` | `Default::default()` — à définir soi-même | oui | non |
| `method = "fn"` | appelle `Self::fn(&model)` | oui | non |
| `nested` | parcourt une FK → `Child::from_model(parent)` | oui | non |
| `nested(strict)` | idem, mais panique si la FK n'a pas été chargée | oui | non |
| `many = ChildSer` | initialise `Vec::new()` ; remplir via `set_<field>(&[Child])` | oui | non |
| `slug = "name"` | clone `model.<source>.value()?.name` | oui | non |
| `validate = "fn"` | validateur par champ exécuté par `validate(&self)` | s/o | s/o |

**Mutuellement exclusifs** (erreurs de compilation si combinés) : `read_only` +
`write_only` ; `method` + `source` ; `slug` + l'un de `method` / `nested` /
`many`.

**Validateurs déclaratifs.** `max_length = N`, `min_length = N`, `min = N` et
`max = N` ajoutent une validation à l'écriture sur un champ sans changer la forme
de sa sortie (et un champ dépourvu de ceux-ci hérite des bornes du modèle). Voir
[Validation](#validation).

`write_only` est destiné aux données entrantes uniquement (un mot de passe, un
jeton à usage unique) : présent dans `writable_fields()`, absent de la sortie.
`skip` est l'échappatoire inverse — le champ n'est pas lu depuis le modèle et
n'est pas accessible en écriture, on le renseigne donc à la main après
`from_model` (par exemple une liste d'ids de tags récupérée séparément).

> **`write_only` ne transforme pas la valeur.** Un champ `write_only` est accepté
> en écriture et persisté **tel quel** — le serializer ne le hache ni ne le
> chiffre jamais. Pour un mot de passe, le hacher soi-même (voir
> [Mots de passe](auth-passwords.md)) avant `save()` ; les champs `read_only`, à
> l'inverse, sont silencieusement ignorés à l'écriture plutôt que rejetés.

---

## Champs calculés

`method = "fn"` est l'équivalent du `SerializerMethodField` de DRF. Déclarer le
champ, puis écrire une fonction associée `fn(&Model) -> FieldType` ; elle est
appelée durant `from_model` :

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub title: String,
    #[serializer(method = "excerpt")]
    pub excerpt: String,
}

impl PostSerializer {
    fn excerpt(model: &Post) -> String {
        model.body.chars().take(80).collect::<String>() + "…"
    }
}
```

Les champs calculés sont en sortie seule (exclus de `writable_fields()`).

---

## Serializers imbriqués

`nested` intègre un autre serializer en parcourant une clé étrangère chargée. Le
type du champ est le serializer enfant :

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Comment)]
pub struct CommentSerializer {
    pub id: Auto<i64>,
    pub body: String,
    #[serializer(nested)]               // reads the loaded `author` FK
    pub author: AuthorBrief,
}
```

La FK doit déjà être chargée (via `select_related` / une récupération anticipée).
Si elle **ne l'était pas**, le champ retombe sur `Default::default()` plutôt que
de paniquer — la production se dégrade gracieusement en cas de prefetch manquant.
Dans les tests, utiliser `#[serializer(nested(strict))]` pour transformer ce repli
en panique afin qu'un prefetch oublié soit détecté. Pointer vers une FK au nom
différent avec `source` :

```rust
#[serializer(nested, source = "owner")]
pub author: AuthorBrief,
```

Les champs imbriqués sont en **lecture seule** dans la forme de sortie — les
objets imbriqués accessibles en écriture ne sont pas encore pris en charge (voir
[limites](#tweaks-and-current-limits)).

---

## Collections (`many`)

Pour des enfants un-à-plusieurs ou M2M, `many = ChildSerializer` déclare un champ
`Vec<…>`. Comme l'accesseur M2M/associé est asynchrone, la macro ne peut pas le
charger automatiquement ; elle initialise le vec vide et émet un assistant
`set_<field>(&[ChildModel])` à appeler après avoir récupéré les enfants :

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostWithTags {
    pub id: Auto<i64>,
    pub title: String,
    #[serializer(many = TagBrief)]
    pub tags: Vec<TagBrief>,
}

// usage
let tags = post.tags_m2m().all(&pool).await?;
let mut s = PostWithTags::from_model(&post);
s.set_tags(&tags);                       // generated setter, named set_<field>
let json = s.to_value();
```

---

## Champs slug

`slug = "name"` est l'équivalent du `SlugRelatedField` de DRF : au lieu d'un id de
FK ou d'un objet imbriqué complet, émettre un unique champ nommé extrait du parent
chargé.

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,
    #[serializer(slug = "name", source = "author")]   // author.name as a flat field
    pub author_name: String,
}
```

Comme nested, il lit depuis une FK chargée et retombe sur la valeur par défaut
lorsqu'elle n'est pas chargée ; il est en affichage seul (non accessible en
écriture).

---

## Validation

Trois couches, toutes remontant sous forme de `rustango::forms::FormErrors` (et,
lors d'une écriture via ViewSet, un `400` à la forme DRF). Elles s'exécutent dans
cet ordre : contraintes déclaratives, puis validateurs par champ, puis le point
d'ancrage inter-champs.

**Contraintes déclaratives (les `validators` de DRF, hérités automatiquement).**
`max_length`, `min_length`, `min` et `max` sont des attributs de champ — et
lorsqu'on les omet, un champ **hérite des** `max_length` / `min` / `max` /
`choices` du modèle. Ainsi, une colonne `#[rustango(max_length = 200)]` voit sa
longueur vérifiée sans aucun attribut de serializer (comportement du
`ModelSerializer` de DRF). Elles sont vérifiées sur chaque champ accessible en
écriture, transformant des `500` de contrainte de base de données potentiels en
aimables `400` :

```rust
#[serializer(model = Widget)]
struct WidgetSerializer {
    pub code: String,               // inherits the model's max_length
    #[serializer(max_length = 4)]   // overrides the model's bound
    pub note: String,
    pub priority: i64,              // inherits the model's min / max
    pub status: String,             // inherits the model's choices
}
```

Les messages correspondent à ceux de Django/DRF :
`"Ensure this value has at most N characters."`,
`"Ensure this value has at least N characters."`, `"Ensure this value is ≥ N."` /
`"≤ N"`, et `"Select a valid choice."`. (`min_length` est propre au serializer ;
`choices` est hérité du modèle — il n'existe pas d'attribut `choices`.)

**Par champ** (personnalisé) — déclarer `validate = "fn"` et écrire
`fn(value: &FieldType) -> Result<(), String>` :

```rust
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    #[serializer(validate = "title_min_3")]
    pub title: String,
    pub body: String,
}

impl PostSerializer {
    fn title_min_3(t: &String) -> Result<(), String> {
        if t.chars().count() < 3 { Err("title must be at least 3 chars".into()) } else { Ok(()) }
    }
}
```

Le derive génère un `validate(&self)` qui exécute chaque validateur par champ et
collecte les échecs dans un `FormErrors` indexé par nom de champ.

**Inter-champs** — déclarer un point d'ancrage au niveau de la structure et les
validateurs fusionnent. Soit ajouter `#[serializer(validate = "cross_validate")]`
sur la structure (retournant `Result<(), FormErrors>`), soit implémenter
simplement `validate(&self)` soi-même lorsqu'il n'y a aucun validateur par champ
pour le générer :

```rust
impl PostSerializer {
    pub fn validate(&self) -> Result<(), rustango::forms::FormErrors> {
        let mut errors = rustango::forms::FormErrors::default();
        if self.title.is_empty() {
            errors.add("title", "title cannot be empty");          // field error
        }
        if self.body.starts_with(&self.title) {
            errors.add_non_field("body must not repeat the title"); // object-level error
        }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}
```

`FormErrors` sépare les erreurs de **champ** (`add(field, msg)`, un
`HashMap<String, Vec<String>>`) des erreurs **hors-champ**
(`add_non_field(msg)`). L'inspecter avec `.fields()`, `.non_field()`,
`.get(field)`, `.is_empty()`, et le combiner avec `.merge(other)`. Au-delà des
contraintes déclaratives ci-dessus (`max_length` / `min_length` / `min` / `max` /
`choices` hérités), les règles personnalisées sont de simples fonctions — il n'y a
pas de magie `email`/regex, ce qui garde la validation personnalisée explicite et
testable. Hors d'un ViewSet, le framework ne rend pas automatiquement les
`FormErrors` dans un corps HTTP ; les mettre en correspondance avec votre réponse
400 (la séparation champ/hors-champ s'aligne sur le JSON d'erreur de DRF).

---

## Validation d'unicité combinée

Pour le `UniqueTogetherValidator` de Django — une vérification avant
enregistrement qu'une ligne candidate n'entrera pas en collision sur un index
d'unicité multi-colonnes — appeler `check_unique_together_pool` avant
l'enregistrement :

```rust
use std::collections::HashMap;
use rustango::core::SqlValue;
use rustango::serializer::check_unique_together_pool;

let mut values: HashMap<&'static str, SqlValue> = HashMap::new();
values.insert("org_id",  SqlValue::I64(self.org_id));
values.insert("user_id", SqlValue::I64(self.user_id));

// None on insert; Some(&pk) on update so the row doesn't clash with itself.
check_unique_together_pool(&pool, Membership::SCHEMA, &values, None).await?;
```

Il parcourt les index d'unicité multi-colonnes déclarés du modèle et retourne
`Err(FormErrors)` avec une erreur hors-champ par collision
(`"The fields a, b must be unique together."`). L'unicité mono-colonne `unique`
est laissée à la gestion des conflits de l'insertion ; les index partiels
(`unique_when`) sont ignorés.

---

## Sortie hyperliée

Pour une forme de type `HyperlinkedModelSerializer` (URLs de ressource au lieu
d'ids nus), deux assistants post-traitent le JSON :

```rust
use rustango::serializer::{hyperlink_url, hyperlinked_to_value};
use std::collections::HashMap;

let base = PostSerializer::from_model(&post).to_value();

let mut fk_templates = HashMap::new();
fk_templates.insert("author_id", "/api/users/{pk}");

let out = hyperlinked_to_value(base, "/api/posts/{pk}", "id", &fk_templates);
// → { "url": "/api/posts/42", "author_id_url": "/api/users/7", "id": 42, ... }
```

`hyperlink_url(template, &pk)` effectue une substitution ponctuelle de `{pk}` ;
`hyperlinked_to_value` ajoute un `url` de premier niveau plus un `<fk>_url` par
template (FK nulle → URL nulle). Les clés id/`<fk>_id` d'origine sont conservées
(les supprimer ensuite si l'on veut s'en débarrasser).

---

## Sérialiser des listes

`many_to_value(&models)` retourne un tableau JSON d'objets sérialisés. Les
ViewSets enveloppent une page d'entre eux dans l'enveloppe standard :

```json
{ "count": 100, "page": 1, "page_size": 20, "last_page": 5, "results": [ { … }, { … } ] }
```

(C'est l'enveloppe par défaut à numéro de page ; voir
[Pagination](viewsets.md#pagination) pour les formes cursor et limit/offset.)

---

## Utiliser un serializer avec un ViewSet

Câbler un serializer dans un [ViewSet](viewsets.md) et il pilote toute la
ressource REST — **sortie et entrée**, sur chaque backend (PostgreSQL, MySQL,
SQLite) :

```rust
#[derive(ViewSet)]
#[viewset(model = Post, serializer = crate::PostSerializer, ordering = "-published_at")]
pub struct PostViewSet;
// or, on the builder: ViewSet::for_model(Post::SCHEMA).serializer::<PostSerializer>()…
```

- **Sortie** — les réponses `list` / `retrieve` / `create` / `update` sont rendues
  via `from_model`, si bien que `source` / `method` / `read_only` / `write_only`
  façonnent le JSON.
- **Entrée** — `create` / `update` exécutent le `validate()` du serializer (un
  échec est un `400` à la forme DRF, `{field: [msgs]}`), et seuls les champs
  accessibles en écriture sont écrits — les champs `read_only` / calculés qu'un
  client poste sont ignorés, résolus via `source` vers la colonne du modèle.

Le ViewSet pilote cela via trois méthodes `ModelSerializer` que le derive
génère : `validate()`, `writable_source_fields()` et `from_writable_json()`. Voir
le [guide des ViewSets](viewsets.md#the-serializer-marriage-input--output) pour le
comportement complet et un exemple concret.

On peut aussi utiliser un serializer **de façon autonome** — mettre une ligne en
correspondance et émettre son JSON depuis n'importe quel handler :

```rust
let post = Post::objects().find(42, &pool).await?.expect("post 42");
let body = PostSerializer::from_model(&post).to_value();   // shaped JSON
```

---

## Valider dans un handler personnalisé

Hors d'un ViewSet, le serializer dérive `serde::Deserialize`, on peut donc parser
un corps de requête vers lui, exécuter `.validate()`, et — en cas de succès —
mettre les données en correspondance avec un modèle et `save(&pool)`.
`from_writable_json()` construit une instance à partir des seules clés accessibles
en écriture (les champs read-only / calculés prennent leur valeur par défaut), et
`writable_fields()` / `writable_source_fields()` indiquent quelles clés sont
acceptées — la même mécanique que le ViewSet utilise en interne.

---

## Schémas OpenAPI

Avec la feature `openapi` activée, le derive émet aussi une impl `OpenApiSchema` :
les types de champ correspondent à des types JSON-schema, `Option<T>` devient
nullable-et-non-requis, et les champs `write_only` sont exclus du schéma de
réponse. C'est ce qui alimente la documentation d'API générée — aucun schéma
distinct à maintenir.

> **Approfondissement :** [OpenAPI](openapi.md) — transformer ce schéma (plus les
> chemins CRUD de votre ViewSet) en une spécification OpenAPI 3.1 complète servie
> avec Swagger UI / Redoc.

---

## Scaffolding

Générer un squelette de serializer avec la CLI manage :

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Elle écrit un module de départ à compléter :

```rust
//! Auto-scaffolded by `manage make:serializer PostSerializer`.

use rustango::Serializer;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: i64,
    // pub title: String,
    // #[serializer(read_only)]
    // pub created_at: chrono::DateTime<chrono::Utc>,
}
```

Enregistrer ensuite le module (`mod post_serializer;`) aux côtés des autres.

---

## Ajustements et limites actuelles

Quelques angles saillants et échappatoires qu'il vaut la peine de connaître :

- **Champs conditionnels.** Il n'y a pas de sélection de champ à l'exécution (les
  champs sont fixés à la compilation). Pour « inclure uniquement lorsque
  présent », utiliser `Option<T>` plus
  `#[serde(skip_serializing_if = "Option::is_none")]` sur le champ — l'impl
  `Serialize` personnalisée respecte les attributs serde.
- **Forme de sortie personnalisée.** Redéfinir `to_value(&self)` sur votre
  structure pour un objet JSON entièrement sur mesure lorsque les attributs ne
  suffisent pas.
- **Les objets imbriqués accessibles en écriture** ne sont pas pris en charge —
  les champs `nested` / `many` / `slug` sont en sortie seule. Accepter les
  écritures sous forme d'ids scalaires et les résoudre soi-même.
- **Les validateurs intégrés se limitent à longueur/plage/choix** — `max_length` /
  `min_length` / `min` / `max` (et les `choices` hérités) sont déclaratifs ; les
  autres règles (`email`, regex, …) sont des fonctions que l'on écrit (voir
  [Validation](#validation)).
- **Un seul validateur par champ, par champ.** Pour plusieurs règles sur un champ,
  les combiner dans la fonction de ce champ, ou ajouter un `validate(&self)`
  inter-champs.
- **Le serializer ne persiste pas.** Mettre en correspondance → valider → confier
  les données à l'ORM ; il n'y a pas de `serializer.save()`.

---

## À essayer

Le serializer minimal est fourni dans l'exemple
[`getting_started_blog`](../crates/rustango/examples/getting_started_blog/src/post_serializer.rs)
(étape 13 du guide de démarrage). Le comportement complet du derive — les
attributs de champ, les champs computed/nested/many, et les deux couches de
validation — est couvert par les tests unitaires du framework lui-même (aucune
base de données requise) :

```bash
cd crates/rustango
cargo test --test serializer_derive          # field attrs, method, nested, many, slug, OpenAPI
cargo test --test serializer_cross_validate  # per-field + cross-field validation aggregation
```

---

## Voir aussi

- [ViewSets](viewsets.md) — câbler un serializer dans une API CRUD JSON.
- [Vues HTML](html-views.md) — l'alternative rendue côté serveur à une API JSON.
- [OpenAPI](openapi.md) — les champs d'un serializer deviennent un schéma de composant.
- [Recueil de recettes ORM](orm.md) — les modèles depuis lesquels les serializers mettent en correspondance.
