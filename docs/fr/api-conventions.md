# Conventions d'API

> **À qui s'adresse cette page.** Il s'agit d'une **référence avancée destinée aux développeurs Rust** qui travaillent *avec* ou *sur* le code du framework — elle explique les conventions de nommage, de types de retour et de modules derrière l'API Rust de Rustango. Ce **n'est pas** un guide pour *appeler* l'API REST d'une application Rustango via HTTP. Si c'est ce que vous cherchez, commencez par les [ViewSets](viewsets.md) (construire une API REST) et le [glossaire](glossary.md) (termes en langage clair) ; revenez ici une fois que vous écrirez du Rust contre le framework.

Cette page explique les patterns que suit l'API de **Rustango**, afin que vous puissiez prédire le comportement de n'importe quelle méthode avant même d'en lire la documentation. Si vous contribuez ou auditez une fonctionnalité, ce sont les règles à connaître.

[![Convention de nommage de Rustango : le suffixe de la méthode indique ce qu'elle attend — `*_on` pour un pool typé, forme nue pour le pool multi-backend, et signaux sans pool](../img/api-conventions.png)](../img/api-conventions.png)

## Table des matières

- [Naming](#naming)
- [Constructors](#constructors)
- [Return types](#return-types)
- [Async vs sync](#async-vs-sync)
- [The pool argument](#the-pool-argument)
- [Filtering](#filtering)
- [Errors](#errors)
- [Module naming](#module-naming)
- [Builders vs config structs](#builders-vs-config-structs)
- [Feature flags](#feature-flags)
- [Macros vs runtime](#macros-vs-runtime)
- [Contributing](#contributing)

---

## Nommage

Le nom d'une méthode indique ce qu'elle fait. Une fois ces suffixes assimilés, vous pouvez deviner la majeure partie de l'API.

### Fonctions

- **`save_on(executor)`, `delete_on(executor)`** — les méthodes d'écriture prennent un *executor* (un pool, une connexion ou une transaction — la chose qui dialogue avec la base de données). Le suffixe `_on` signifie « exécute ceci contre l'executor que je te fournis ».
- **`fetch_on(executor)`, `count_on(executor)`** — même suffixe `_on`, pour les lectures.
- **`save()`, `fetch()`, `count()`** sans `_on` — raccourci qui appelle la version `_on` avec un `&pool` par défaut. Ne fonctionne que là où le queryset ou le modèle détient déjà une référence de pool (rare dans le code applicatif).
- **`from_X(value)`** — convertit DEPUIS une autre valeur (par ex. `from_model(post)`, `from_base32(s)`).
- **`with_X(value)`** — une méthode de builder qui définit une option et retourne l'objet, ce qui permet d'enchaîner les appels (par ex. `with_default_ttl(d)`, `with_access_ttl(secs)`).
- **`new()`** — le constructeur minimal. Les arguments qu'il prend sont des dépendances obligatoires (par ex. `RedisCache::new(url)` — vous ne pouvez pas construire le cache sans une URL).

### Types

Ils suivent la casse standard de Rust, la même que la distinction de PEP 8 en Python entre classes et fonctions :

- **`PascalCase`** — types, traits et variantes d'enum (comme les classes Python).
- **`snake_case`** — modules, fonctions, champs et variables locales.
- **`SCREAMING_SNAKE_CASE`** — constantes, plus la constante `Model::SCHEMA` que la macro derive génère pour chaque modèle.
- **`Boxed*`** — un alias pour `Arc<dyn Trait>`, un pointeur partagé thread-safe vers un objet-trait (la façon Rust de conserver « n'importe quelle implémentation de cette interface »). Par exemple `BoxedCache = Arc<dyn Cache>`. C'est le type standard pour un backend interchangeable que vous pouvez remplacer.

### Modules

- **Singulier** lorsque le module contient UN seul type ou concept principal : `cache`, `email`, `storage`, `signed_url`, `request_id`.
- **Pluriel** lorsque le module contient une COLLECTION d'éléments : `bulk_actions`, `api_keys`, `passwords`, `forms`, `signals`.

---

## Constructeurs

La façon de construire un objet dépend de ce dont il a besoin. Il existe quelques formes standard :

| Pattern | Quand | Exemple |
|---|---|---|
| `T::new()` | Minimal — aucune dépendance obligatoire | `InMemoryCache::new()`, `Validator::new()` |
| `T::new(arg)` | Une dépendance obligatoire | `EnvSecrets::with_prefix(s)`, `RedisCache::new(url)` |
| `T::with_X(arg)` | Surcharge de style builder après `new()` | `InMemoryCache::with_default_ttl(d)`, `JwtLifecycle::new(s).with_access_ttl(60)` |
| `T::from_X(arg)` | Convertir DEPUIS Y | `TotpSecret::from_base32(s)`, `Locale::new(s)` (parfois `from_str`) |
| `T::for_Y(arg)` | Construire un T restreint à un Y spécifique | `ViewSet::for_model(schema)` |

**À éviter :** `T::with_X_and_Y_and_Z(a, b, c)` — un seul constructeur qui prend tout. Découpez-le plutôt en `new(...)` plus des appels chaînés `.with_*()`.

---

## Types de retour

Le type de retour d'une méthode indique comment elle peut échouer. Rust n'a pas d'exceptions, donc l'échec fait partie de la valeur de retour. Il existe trois formes.

**`Result<T, E>`** — comme une fonction qui soit retourne une valeur, soit lève une exception. Vous obtenez soit la valeur `T`, soit une erreur `E` avec des détails. À utiliser pour les opérations qui peuvent échouer et où le *pourquoi* importe :
- E/S : `pool.fetch(...).await -> Result<_, sqlx::Error>`
- Validation : `Form::parse(data) -> Result<Self, FormErrors>`
- Émission : `JwtLifecycle::issue_pair_with(uid, claims) -> Result<_, JwtIssueError>`

**`Option<T>`** — soit une valeur (`Some`), soit rien (`None`), comme un champ nullable. À utiliser quand « rien trouvé » est un résultat normal et que vous n'avez pas besoin d'un message d'erreur expliquant pourquoi :
- Recherches : `cache.get(k) -> Result<Option<String>, _>` (le `Result` couvre l'échec d'E/S ; l'`Option` couvre « clé absente »)
- Vérification : `async JwtLifecycle::verify_access(token) -> Option<Claims>` (« expiré ou invalide » est un résultat attendu, donc `None` suffit)
- Lectures de config optionnelles : `env::optional("FOO") -> Result<Option<T>, _>`

**`bool`** — un simple oui/non quand aucun détail supplémentaire n'est nécessaire :
- `cache.exists(k) -> Result<bool, _>` (le `Result` couvre l'E/S ; le `bool` est la réponse)
- `JwtLifecycle::revoke(token) -> bool` (true = ajouté à la liste noire)
- `disconnect_pre_save(id) -> bool` (true = une entrée a été retirée)

**`Result<Option<T>>` ou `Result<T>` avec une erreur `NotFound` ?** Les deux peuvent exprimer « la recherche a échoué », alors choisissez selon le caractère exceptionnel de « non trouvé » :
- Utilisez `Result<Option<T>>` quand « non trouvé » est courant — votre code se ramifie de toute façon presque toujours sur `Some`/`None`.
- Utilisez `Result<T>` avec une variante d'erreur `NotFound` quand « non trouvé » est exceptionnel — quelque chose que vous consigneriez en avertissement ou transformeriez en 404.

---

## Async vs sync

La règle générale : si une méthode attend quelque chose (la base de données, le réseau ou le disque), elle est `async` et vous devez la `.await`. Si elle ne fait que calculer, c'est un appel sync normal. Ce tableau le détaille.

| Opération | Sync ou async ? |
|---|---|
| Méthode de trait qui touche l'E/S (BDD, réseau, fichier) | **async** |
| Méthode de trait purement calculatoire (`hash`, `verify`, `encode`) | **sync** |
| Méthodes de builder (`with_X`, setters chaînables) | **sync** |
| Macros (`derive(Model)`, `derive(Serializer)`) | **N/A** (à la compilation) |
| Signal `connect_*` (enregistre un récepteur) | **sync** |
| Signal `send_*` (dispatche vers des récepteurs async) | **async** |

**Exception :** `Cache::set` est `async` même si la version en mémoire (`InMemoryCache::set`) n'attend jamais réellement. Le trait est modelé pour le cas Redis, qui, lui, attend. C'est intentionnel : une méthode de trait devrait être `async` si *une seule* implémentation raisonnable a besoin d'attendre, afin que tous les backends partagent une même signature.

---

## L'argument pool

Chaque appel ORM prend un pool ou un executor (le handle de base de données) comme **dernier** argument. Vous passez la connexion à chaque fois, plutôt que de vous appuyer sur un état global caché :

```rust
post.save_on(&pool).await?
Post::objects().filter(...).fetch_on(&pool).await?
send_post_save(&post, ctx).await                  // ⚠️ no pool — signals are pool-free
```

**Une seule exception :** les signaux ne prennent pas de pool, car ils ne touchent jamais la base de données. La règle tient : tout ce qui atteint la BDD prend le pool ; tout ce qui ne l'atteint pas, non.

**Pourquoi le passer à chaque fois ?** Rust préfère les dépendances visibles à un état global caché. Django conserve la connexion dans un stockage thread-local, mais cela s'effondre dans le monde async de Rust, où une tâche peut sauter d'un thread à l'autre en plein milieu d'une requête. L'inconvénient, c'est plus de saisie ; l'avantage, c'est que vous pouvez faire un grep sur chaque endroit qui touche la base de données.

Si vous vous retrouvez à faire transiter `&pool` à travers dix couches d'appels de fonctions, acceptez `impl Executor` une seule fois au point d'entrée public et laissez les helpers internes partager cette unique connexion.

---

## Filtrage

Il y a trois façons de filtrer un queryset, et elles se combinent toutes dans une même requête. Choisissez selon la provenance du filtre.

```rust
// 1. HTTP query string (set via ViewSet filter_fields, parsed at request time)
//    GET /api/posts?author_id=42&status__ne=archived

// 2. String-keyed (lookup at compile of the queryset; runtime field name resolution)
Post::objects().filter("author_id", Op::Eq, SqlValue::I64(42));

// 3. Typed columns (compile-time field check)
Post::objects().where_(Post::author_id.eq(42));
```

| Syntaxe | À utiliser quand |
|---|---|
| Requête HTTP | Endpoints d'API publics — le ViewSet les analyse pour vous, comme les backends de filtre de DRF |
| `.filter` par clé-chaîne | Code CRUD générique ou d'admin, où les noms de champs viennent de la config et ne sont pas connus à la compilation |
| `.where_` typé | Le code de votre application — le choix par défaut recommandé. Le compilateur vérifie que le champ existe et que les types correspondent |

Vous pouvez **mélanger les trois** dans un même queryset.

---

## Erreurs

**Rustango** compte **plus de 20 types d'erreur** — un par module — au lieu d'une unique classe d'exception fourre-tout. Ils forment une hiérarchie souple, et un type de plus haut niveau les relie de sorte que vous les manipulez rarement individuellement.

| Couche | Module | Type d'erreur |
|---|---|---|
| E/S ORM | `sql::*` | `ExecError` |
| Écrivain SQL de l'ORM | `sql::*` | `SqlError` (variante de `ExecError::Sql`) |
| Migrations | `migrate::*` | `MigrateError` |
| Formulaires | `forms::*` | `FormError` (simple) + `FormErrors` (multiple) + `ModelFormError` |
| Cache | `cache::*` | `CacheError` |
| Email | `email::*` | `MailError` |
| Stockage | `storage::*` | `StorageError` |
| Backends d'authentification | `tenancy::auth_backends` | `AuthError` |
| JWT | `tenancy::jwt_lifecycle` | `JwtIssueError` |
| Clés d'API | `api_keys::*` | `ApiKeyError` |
| Mots de passe | `passwords::*` | `PasswordError` |
| Webhooks | `webhook::*` | (retourne un bool, pas d'erreur dédiée) |
| URLs signées | `signed_url::*` | `SignedUrlError` |
| Actions en masse | `bulk_actions::*` | `BulkActionError` |
| Fixtures | `fixtures::*` | `FixtureError` |
| Filtre IP | `ip_filter::*` | `IpFilterError` |
| i18n | `i18n::*` | `I18nError` |
| Env | `env::*` | `EnvError` |
| Secrets | `secrets::*` | `SecretsError` |
| Réponses d'API | `api_errors::*` | `ApiError` (forme HTTP, pas interne) |

**Celle à utiliser dans les handlers :** il existe une enum `RustangoError` de plus haut niveau (exportée depuis `lib.rs`, avec l'alias `RustangoResult<T> = Result<T, RustangoError>`). Elle enveloppe chacune des erreurs ci-dessus via des conversions `From`, si bien que l'opérateur `?` promeut automatiquement toute erreur de module vers elle. Elle implémente aussi `IntoResponse`, ce qui signifie que chaque variante est mappée vers un statut HTTP sensé lorsqu'elle est retournée depuis un handler. La répartition est simple : utilisez les erreurs spécifiques par module au plus profond de votre code, et `RustangoError` / `RustangoResult` à la frontière du handler. Pour les erreurs provenant de crates tierces, `RustangoError::other(msg)` / `RustangoError::other_from(e)` enveloppent n'importe quel `std::error::Error + Send + Sync + 'static`.

**Un exemple de handler :**

```rust
use rustango::api_errors::ApiError;

async fn handler() -> Result<Json<X>, ApiError> {
    let post = Post::objects().get(&pool, 1).await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(post))
}
```

`ApiError` implémente `IntoResponse`, si bien que le retourner produit automatiquement la forme JSON d'erreur standard.

---

## Nommage des modules

Le nom d'un module devrait vous permettre de **deviner les noms des types qu'il contient** sans ouvrir le fichier.

| Module | Héberge | Confiance de recherche |
|---|---|---|
| `cache` | le trait `Cache`, les impls `*Cache` | élevée |
| `email` | le trait `Mailer`, `Email`, les impls `*Mailer` | élevée |
| `storage` | le trait `Storage`, les impls `*Storage` | élevée |
| `signed_url` | les fonctions libres `sign`, `verify` | moyenne |
| `text` | les fonctions libres `slugify`, `html_escape`, `truncate` | moyenne |
| `bulk_actions` | `BulkActionRegistry`, `BulkAction`, les impls `Bulk*Action` | élevée |
| `api_keys` | les fonctions libres `generate_key`, `verify_key`, `split_token` | moyenne |

**À éviter :** un module qui contient un fourre-tout hétéroclite (`utils`, `helpers`, `common`). Si vous ne pouvez pas nommer le concept unique qu'il couvre, il ne devrait pas être un module.

---

## Builders vs structs de config

Il y a deux façons de fournir un objet configuré. Choisissez selon la manière dont les utilisateurs le paramétreront.

### Builder : setters chaînés, pas de `Default`

```rust
let l = SecurityHeadersLayer::strict()
    .csp(...)
    .header("x-extra", "v");
```

À utiliser quand :
- La plupart des utilisateurs partent d'un préréglage et l'ajustent
- Les setters expriment une intention (par ex. `.errors_only()` se lit mieux que `.log_success(false)`)
- Le struct a de nombreux champs optionnels (10+)

### Struct de config : définir les champs directement, se rabattre sur `Default`

```rust
let l = AccessLogLayer {
    log_success: false,
    include_ip: true,
    slow_threshold_ms: 500,
    ..Default::default()
};
```

À utiliser quand :
- Les utilisateurs veulent être explicites sur chaque champ
- La réflexion / sérialisation importe
- La mise à jour sur place est courante (`config.field = ...`)

**En règle générale, **Rustango** utilise des builders** pour les middlewares HTTP (`security_headers`, `cors`, `rate_limit`, et ainsi de suite) et des structs de config pour les simples porteurs de données (`Email`, `AccessLogLayer`, l'état interne de `RateLimitLayer`).

---

## Feature flags

Une *feature* est un flag de build Cargo (le `[features]` de `Cargo.toml`) qui active ou désactive un pan du crate — comparable à la découverte de paquets de Laravel ou aux `INSTALLED_APPS` de Django, mais résolu à la compilation. Chaque module qui tire une dépendance supplémentaire se trouve derrière l'une d'elles. L'ensemble par défaut, c'est « vous en voulez presque certainement » :

```toml
default = [
    "postgres", "manage", "admin", "config", "forms", "serializer",
    "cache", "signals", "email", "storage", "scheduler", "secrets", "totp",
    "webhook", "webhook-delivery", "api_keys", "passwords", "signed_url",
    "notifications", "casts", "jobs", "jobs-postgres", "auth_flows", "sse",
    "websocket", "oauth2", "http-client", "compression", "openapi",
    "csp-nonce", "sessions", "hmac-auth", "jwt", "uploads", "storage-s3",
    "media", "runserver", "template_views",
]
```

**Désactivées par défaut :** les features qui tirent de lourdes dépendances ou des services externes :
- `tenancy` — ajoute `argon2`, `hmac`, `sha2`, `cookie`, `tower` (la plupart des applications n'en ont pas besoin)
- `cache-redis` — ajoute la crate `redis` (la plupart des applications se contentent du cache en mémoire)
- `csrf` — activée automatiquement par `admin`, mais aussi disponible seule

Pour réduire un binaire qui n'a pas besoin de tout, désactivez les valeurs par défaut et ne listez que ce que vous utilisez :

```toml
rustango = { version = "0.44", default-features = false, features = ["postgres", "admin"] }
```

---

## Macros vs runtime

Une *macro* est du code qui génère du code à la compilation (`#[derive(Model)]` et consorts) — à peu près ce que fait un générateur Rails, sauf qu'elle s'exécute à chaque build et que le compilateur vérifie le résultat. La répartition ci-dessous décide de ce qui est fait par une macro versus du simple code au runtime.

| Préoccupation | Macro ou runtime ? |
|---|---|
| Métadonnées de schéma pour `inventory` | macro (`#[derive(Model)]`) |
| Construction de requêtes pilotée par le schéma | runtime (utilise le `&'static ModelSchema` issu de la macro) |
| Parsing de formulaire | macro pour le struct (`#[derive(Form)]`) ; runtime pour la logique de parsing |
| Sélection des champs du serializer | macro (`#[derive(Serializer)]`) — émet un `from_model` + une impl `Serialize` personnalisée |
| Opérations de migration | runtime (diff de `SchemaSnapshot`) |
| Dispatch de signal | runtime (registre indexé par `TypeId`, pas de macro par modèle) |
| Filtrage par pattern des backends d'authentification | runtime (`#[async_trait]` sur `AuthBackend`) |

**Règle :** utilisez une macro pour tout ce que le compilateur peut vérifier d'emblée (les noms de champs doivent exister, les types doivent correspondre). Utilisez du code au runtime pour tout ce qui varie par requête ou par déploiement.

---

## Contribuer

Lorsque vous ajoutez une nouvelle fonctionnalité, suivez ces étapes :

1. **Un module par concept**, dans `crates/rustango/src/<name>.rs` ou `<name>/mod.rs`.
2. **Ajoutez de la rustdoc au niveau du module** avec un exemple « Quick start » dans un bloc `// ignore`.
3. **Ajoutez un feature flag si vous tirez une nouvelle dépendance** — nommez-le d'après le module (`feature = "<name>"`).
4. **Ré-exportez le module depuis `lib.rs`** avec une rustdoc d'une ligne.
5. **Placez les tests unitaires dans le même fichier**, derrière `#[cfg(test)] mod tests` — pas de base de données à moins d'en avoir vraiment besoin.
6. **Placez les tests d'intégration dans `crates/rustango/tests/<name>.rs`** pour le scénario de bout en bout.
7. **N'ajoutez pas de nouveau type d'erreur à moins que les existants ne conviennent pas** — étendez d'abord une enum existante.
8. **Suivez le [guide des types de retour](#return-types)** au moment de choisir entre `Result`, `Option` ou `bool`.
9. **Vous ajoutez une sous-commande `manage` ?** Câblez-la dans le dispatcher `match cmd` et `print_help`, ajoutez un test dans `crates/rustango/tests/migrate_manage.rs`, et documentez une ligne dans `docs/manage.md`.
10. **Mettez à jour `CHANGELOG.md`** avec une entrée `Added` sous la prochaine version.

Lorsque vous cassez l'API :
- Marquez l'ancien élément `#[deprecated(since = "...", note = "use X instead")]` et conservez-le pendant une version mineure complète avant de le retirer.
- Consignez-le dans `CHANGELOG.md` sous `Breaking changes`.
- Faites le lien vers le chemin de migration depuis les notes de version.
