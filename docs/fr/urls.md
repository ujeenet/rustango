# Noms d'URL & reverse

Coder en dur des URL (`/posts/42`) partout dans les handlers et les templates est
fragile — changez une route et chaque littéral casse silencieusement.
**Rustango** vous donne la réponse de Django : **nommez un motif d'URL une fois,
puis construisez l'URL par son nom partout** — en Rust avec `reverse(...)`, dans
les templates avec `{{ url(...) }}`, et dans les redirections avec
`redirect_to_view(...)`. La surface de l'API reflète les
`reverse()` / `{% url %}` / `resolve_url()` / `redirect()` de Django.

[![URL reverse à la Django : register_url! nomme un motif, reverse() construit l'URL en Rust, et {{ url(...) }} construit l'URL dans un template](img/urls.png)](img/urls.png)

> **Source :** `rustango::urls` (`register_url!`, `reverse`, `reverse_owned`,
> `all_routes`, `duplicates`, `register_url_tag`) et `rustango::shortcuts`
> (`resolve_url`, `redirect_to_view`).

> **Nouveau ici ?** *route*, *reverse*, *namespacing* — voir le
> [glossaire](glossary.md).

---

## Table des matières
- [Enregistrer une URL nommée](#register-a-named-url)
- [Reverse en Rust](#reverse-in-rust) · [Reverse dans les templates](#reverse-in-templates)
- [Rediriger par nom](#redirect-by-name) · [Namespacing](#namespacing)
- [Inspecter la carte des URL](#inspect-the-url-map) · [Erreurs](#errors)
- [Motifs regex & chemins typés](#regex--typed-path-patterns) · [Notes & limites](#notes-and-limits)

---

## Enregistrer une URL nommée

`register_url!("name", "/pattern")` enregistre un mapping nom → motif. Il s'exécute
au chargement du module (via `inventory`), donc la route atterrit dans un registre
global dès que son module est lié — pas de `urls.py` central à éditer, et pas
d'`include()` à câbler.

```rust
use rustango::register_url;

register_url!("post-detail", "/posts/{id}");
register_url!("user-posts",  "/users/{user_id}/posts/{post_id}");
register_url!("home",        "/");
```

Les placeholders utilisent la syntaxe de chemin `{name}` d'axum. Le motif est la
même chaîne à laquelle vous montez le handler — gardez-les synchronisés
(enregistrez le nom là où vous construisez la route).

---

## Reverse en Rust

`reverse(name, &params)` substitue les `{placeholders}` du motif par les valeurs
données (en percent-encodant chacune) et retourne l'URL :

```rust
use std::collections::HashMap;
use rustango::urls::reverse;

let mut params = HashMap::new();
params.insert("id", "42".to_string());

let url = reverse("post-detail", &params)?;   // → "/posts/42"
```

Pour des clés dynamiques (p. ex. des valeurs assemblées à partir d'une requête),
`reverse_owned` prend `HashMap<String, String>` au lieu de
`HashMap<&str, String>` :

```rust
use rustango::urls::reverse_owned;
let url = reverse_owned("post-detail", &owned_params)?;
```

`reverse` est **strict** : un placeholder manquant, ou une clé `params`
supplémentaire que le motif n'a pas, est une erreur (pas un décalage silencieux) —
voir [Erreurs](#errors).

---

## Reverse dans les templates

Les templates reçoivent le `{% url %}` de Django comme fonction Tera.
Enregistrez-la une fois sur votre instance `Tera` à l'initialisation (elle est
derrière la feature `template_views`) :

```rust
rustango::urls::register_url_tag(&mut tera);
```

Puis appelez `url(name=..., <param>=...)` dans n'importe quel template — `name`
est requis, et tout autre argument nommé est un paramètre de chemin (chaînes,
nombres et booléens sont acceptés) :

```jinja
<a href="{{ url(name='post-detail', id=42) }}">View post</a>
<a href="{{ url(name='user-posts', user_id=7, post_id=42) }}">…</a>
```

C'est l'équivalent du `{% url 'post-detail' id=42 %}` de Django. Pour le motif de
capture `{% url 'x' as var %}`, utilisez le `{% set %}` de Tera :

```jinja
{% set post_url = url(name='post-detail', id=post.id) %}
<a href="{{ post_url }}">{{ post.title }}</a>
```

Un argument `null` (généralement une variable de template non définie) échoue
bruyamment plutôt que de produire silencieusement une URL cassée.

---

## Rediriger par nom

`rustango::shortcuts` reflète les helpers de redirection par nom de vue de Django,
de sorte que les handlers ne codent jamais en dur un `Location` :

```rust
use std::collections::HashMap;
use rustango::shortcuts::{redirect_to_view, resolve_url};

// redirect('post-detail', id=42) → 302 Location: /posts/42
let mut params = HashMap::new();
params.insert("id", "42".to_string());
let response = redirect_to_view("post-detail", &params)?;
```

`resolve_url(spec, &params)` est le `resolve_url` de Django : si `spec` ressemble
déjà à une URL (`/…`, `http://`, `https://`, `./`, `../`) elle est retournée
telle quelle ; sinon elle est traitée comme un nom de route et résolue par
reverse. Pratique pour un paramètre `?next=` ou un réglage pouvant contenir *soit*
un chemin, *soit* un nom :

```rust
let url = resolve_url("post-detail", &params)?;  // name  → "/posts/42"
let url = resolve_url("/dashboard", &params)?;   // path  → "/dashboard" (as-is)
```

(Pour les redirections brutes vers une URL connue, `rustango::shortcuts::redirect(url)`
retourne un simple `302`.)

---

## Namespacing

Il n'y a pas d'`include()` ni de namespace d'app auto-appliqué — chaque
`register_url!` atterrit dans un unique registre global. Le namespacing est une
**convention dans le nom lui-même** : préfixez avec `app:`, exactement comme vous
appelleriez le `reverse("app:detail")` de Django.

```rust
register_url!("blog:post-detail", "/blog/posts/{id}");
register_url!("shop:product",     "/shop/products/{slug}");
```

```rust
reverse("blog:post-detail", &params)?;   // "/blog/posts/42"
```

Le deux-points fait simplement partie de la chaîne enregistrée — choisissez un
préfixe cohérent par app pour éviter les collisions.

---

## Inspecter la carte des URL

Listez toutes les routes enregistrées depuis la CLI — utile pour un audit rapide
ou pour scripter :

```bash
cargo run -- showurls                  # plain table of name → pattern
cargo run -- showurls --format json    # machine-readable
```

En code, `all_routes()` retourne tout le registre, et `duplicates()` retourne tout
nom enregistré plus d'une fois (premier arrivé gagne sinon — à asserter au
démarrage) :

```rust
use rustango::urls::{all_routes, duplicates};

for route in all_routes() {
    println!("{} → {}", route.name, route.pattern);
}

let dups = duplicates();
assert!(dups.is_empty(), "duplicate URL names: {dups:?}");
```

---

## Erreurs

`reverse` / `reverse_owned` / `resolve_url` / `redirect_to_view` retournent
`Result<_, rustango::urls::ReverseError>` :

| Variante | Quand |
|---|---|
| `UnknownName(name)` | Aucun `register_url!` ne s'est exécuté pour ce nom (faute de frappe, ou son module n'était pas lié). |
| `MissingParam { name, param }` | Le motif a `{param}` mais `params` ne l'a pas fourni. |
| `UnexpectedParam { name, param }` | `params` portait une clé que le motif n'a pas (attrape les fautes de frappe). |
| `MalformedPattern { name, detail }` | Le motif enregistré est malformé (p. ex. un `{` non fermé). |

Dans les templates, elles remontent comme des erreurs de rendu Tera (un 500 via
`shortcuts::render` / `template_views`), donc un mauvais `{{ url(...) }}` échoue
visiblement plutôt que de rendre un lien cassé.

---

## Motifs regex & chemins typés

**Rustango n'a pas de `re_path`, et aucun convertisseur de chemin n'est jamais
appliqué.** Un segment de motif est soit un littéral (`/posts/new`) soit un
placeholder `{name}` qui capture exactement un segment ; `{*name}` capture le
reste du chemin. C'est tout le vocabulaire — il n'y a pas de
`r'(?P<year>[0-9]{4})'`, et `{int:id}` ne contraint **pas** `id` à un entier.

### Pourquoi — le matcher n'est pas un moteur de regex

Le routage *est* [axum](https://docs.rs/axum) 0.8, et axum matche les chemins avec
[`matchit`](https://docs.rs/matchit), un routeur à **arbre radix (radix-trie)**.
Il parcourt l'URL un segment à la fois le long d'un arbre de préfixes, donc un
match coûte O(longueur du chemin) et est indépendant du nombre de routes
enregistrées. Un routeur regex fait l'inverse : Django évalue `urlpatterns` de
haut en bas, exécutant la regex de chaque entrée contre le chemin jusqu'à ce
qu'une corresponde. Le trie achète un matching en temps constant et une préséance
non ambiguë « le littéral le plus spécifique gagne » — au prix de ne pas exprimer
les contraintes de classe de caractères *dans le chemin lui-même*.

Rustango hérite de ce matcher en bloc. Il n'y a **pas de second résolveur basé sur
regex** superposé, et `register_url!` enregistre délibérément les *mêmes* chaînes
`{name}` que le routeur comprend déjà — il ne compile jamais de regex. Donc les
chemins regex ne sont pas « désactivés » ; la couche de routage n'a simplement
jamais été un moteur de regex au départ.

La forme `{int:id}` n'est acceptée que comme **facilité de portage** pour
`reverse()` : le constructeur découpe le placeholder sur `:` et ne garde que le
nom, jetant le préfixe de type ([`urls.rs`](../crates/rustango/src/urls.rs)). Cela
permet à `reverse()` de fonctionner sur un motif copié verbatim d'un
`path("<int:id>/", …)` de Django — mais rien ne valide que la valeur fournie est
réellement un entier.

### Comment exprimer une route contrainte

Matchez le segment avec un simple `{placeholder}`, puis imposez sa forme là où la
valeur est utilisée. Le `re_path(r'^articles/(?P<year>[0-9]{4})/$', …)` de Django
devient :

```rust
register_url!("article-by-year", "/articles/{year}");
// router:
.route("/articles/{year}", get(article_by_year))

async fn article_by_year(Path(year): Path<String>) -> impl IntoResponse {
    // the router accepted any single segment; enforce [0-9]{4} here
    match year.parse::<u16>() {
        Ok(y) if (1000..=9999).contains(&y) => render_year(y).await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
```

Pour rejeter *avant* que le handler ne s'exécute (plus proche de la sémantique des
convertisseurs de Django), placez le contrôle dans un extracteur axum personnalisé
(`FromRequestParts`) et prenez ce type comme argument du handler au lieu de
`Path<String>` — le framework n'en fournit pas, mais le trait d'extracteur d'axum
est la couture prévue. Le crate `regex` est déjà une dépendance (l'ORM l'utilise
pour les lookups `__regex`), donc un extracteur validant peut compiler une `Regex`
une fois et la réutiliser à travers les requêtes.

---

## Notes et limites

- **L'enregistrement se fait au moment du link.** Un `register_url!` ne prend effet
  que si son module est compilé dans le binaire. Une erreur `UnknownName` signifie
  généralement que le nom est une faute de frappe *ou* que son module n'est
  référencé nulle part (donc le linker l'a écarté).
- **Les motifs ne sont pas validés contre vos vraies routes.** `register_url!`
  enregistre un mapping nom → chaîne ; il ne vérifie pas qu'un handler est
  réellement monté à ce motif. Enregistrez le nom là où vous montez la route pour
  qu'ils restent synchronisés.
- **Les valeurs sont percent-encodées** par `reverse`, donc elles sont sûres à
  déposer dans un header `Location` ou un `href`.
- **Pas de convertisseurs regex/typés** dans les motifs (le `<int:pk>` de Django) ;
  les placeholders sont de simples `{name}` et les valeurs sont substituées telles
  quelles (après encodage). Voir [Motifs regex & chemins typés](#regex--typed-path-patterns)
  pour le pourquoi, et comment contraindre une route à la place.


---

## Voir aussi

- [Vues HTML](html-views.md)
- [ViewSets](viewsets.md)
- [Middleware](middleware.md)
