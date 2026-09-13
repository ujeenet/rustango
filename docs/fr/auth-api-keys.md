# Clés d'API

Une clé d'API est un **identifiant à longue durée de vie pour les machines** —
tâches CI, scripts, appels de serveur à serveur — qui ne peuvent pas présenter
de formulaire de connexion ni porter de cookie de session. Le client envoie la
clé à chaque requête ; le serveur la recherche et identifie l'appelant.
**Rustango** vous offre deux couches : un utilitaire autonome de
génération/vérification que vous pouvez brancher sur votre propre table, et un
backend clé en main qui stocke les clés et authentifie les requêtes
`Authorization: Bearer`.

[![Clés d'API dans Rustango : generate_key renvoie un token à usage unique prefix.secret, vous stockez le préfixe de 8 caractères plus un hash argon2id, et verify_key vérifie un secret entrant](../img/auth-api-keys.png)](../img/auth-api-keys.png)

> **Un terme vous est inconnu ?** *Token*, *hash*, *Bearer*, *argon2id* — le
> [glossaire](glossary.md) définit les briques de base.

> **Source :** `rustango::api_keys` (`generate_key`, `hash_secret`, `verify_key`,
> `split_token`, `ApiKeyError`) — l'utilitaire autonome, derrière la
> fonctionnalité `api_keys` (activée par défaut). Le backend avec stockage est
> `rustango::tenancy::auth_backends` (`create_api_key`, `ApiKeyBackend`,
> `ensure_api_keys_table_pool`) — derrière la fonctionnalité `tenancy`.
>
> **Version exécutable :** les extraits de l'utilitaire sont copiés depuis
> [`auth_api_keys_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_api_keys_doc.rs)
> (`cargo test -p rustango --test auth_api_keys_doc`) ; le flux du middleware
> `ApiKeyBackend` depuis
> [`auth_backends_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_backends_doc.rs)
> (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Table des matières

- [Comment fonctionne une clé d'API](#how-an-api-key-works)
- [L'utilitaire autonome](#the-standalone-helper)
- [Le backend avec stockage](#the-stored-backend)
- [Émettre une clé (CLI + code)](#issuing-a-key)
- [Authentifier les requêtes](#authenticating-requests)
- [Notes de sécurité](#security-notes)
- [Voir aussi](#see-also)

---

## Comment fonctionne une clé d'API

Une clé a deux parties jointes par un point : **`{prefix}.{secret}`**.

- Le **préfixe** fait 8 caractères — stocké en clair et utilisé comme index de
  recherche rapide et unique (« quelle clé est-ce ? »).
- Le **secret** est l'identifiant réel. Vous ne stockez qu'un **hash argon2id**
  de celui-ci, jamais le secret lui-même.

Le token complet est montré à l'utilisateur **exactement une fois**, à la
création. Perdez-le et vous le réémettez — il n'y a aucun moyen de le récupérer,
car seul le hash est conservé. C'est la même discipline « hasher, ne pas
stocker » que pour les [mots de passe](auth-passwords.md), appliquée aux
identifiants machine.

---

## L'utilitaire autonome

`rustango::api_keys` est une boîte à outils sans dépendance (pas de base de
données, pas de tables) — utilisez-le quand vous voulez stocker les clés dans
votre propre schéma.

```rust
use rustango::api_keys::{generate_key, split_token, verify_key};

// À la création : renvoie (full_token, prefix, hash).
let (token, prefix, hash) = generate_key()?;
// → token  = "a1b2c3d4.<secret>"   à montrer à l'utilisateur UNE FOIS
// → prefix = "a1b2c3d4"            à stocker comme clé de recherche
// → hash   = "$argon2id$v=19$..."  à stocker au lieu du secret

// Sur une requête entrante : extraire le token, trouver la ligne par préfixe, vérifier.
let (prefix, secret) = split_token(&token).expect("well-formed token");
let stored_hash = lookup_hash_by_prefix(prefix);     // votre requête
if verify_key(secret, &stored_hash)? {
    // authentifié
}
```

`split_token` est strict — il renvoie `None` sauf si le préfixe fait exactement
8 caractères et que le secret est non vide, de sorte qu'une entrée malformée est
rejetée avant que vous ne touchiez à la base de données :

```rust
assert!(split_token("no-dot-here").is_none());
assert!(split_token("short.secret").is_none()); // le préfixe doit faire 8 caractères
assert!(split_token("a1b2c3d4.").is_none());     // secret vide
```

`hash_secret` et `verify_key` utilisent argon2id avec un sel aléatoire par hash,
de sorte que hasher deux fois le même secret produit des chaînes différentes —
et les deux se vérifient. `verify_key` renvoie `Ok(false)` en cas de non-
correspondance et `Err(ApiKeyError)` uniquement quand la chaîne stockée n'est pas
un hash valide.

---

## Le backend avec stockage

Si vous êtes déjà sur la couche `tenancy`, vous n'avez pas besoin de votre propre
table. `rustango::tenancy::auth_backends` fournit un modèle `ApiKey` (table
`rustango_api_keys`), un créateur, et un backend d'authentification qui se
branche sur la [chaîne de backends](auth-backends.md).

Amorcez la table une fois (tri-dialecte, idempotent) :

```rust
use rustango::tenancy::auth_backends::ensure_api_keys_table_pool;

ensure_api_keys_table_pool(&pool).await?;   // CREATE TABLE IF NOT EXISTS
```

La ligne `ApiKey` stocke `user_id` (FK vers `rustango_users`), le `key_prefix`
de 8 caractères (unique), le `key_hash` argon2id, un `label`, et un `expires_at`
optionnel.

---

## Émettre une clé

`create_api_key` génère le token, hashe le secret, insère la ligne, et renvoie le
**token en clair une seule fois** :

```rust
use rustango::tenancy::auth_backends::create_api_key;

// Émettre une clé sans expiration pour l'utilisateur 42, étiquetée "ci-key".
let token = create_api_key(42, "ci-key", None, &pool).await?;
println!("Store this — it won't be shown again: {token}");

// Ou avec une expiration :
use chrono::{Duration, Utc};
let token = create_api_key(42, "tmp", Some(Utc::now() + Duration::days(30)), &pool).await?;
```

Depuis la ligne de commande, la CLI `manage` enveloppe le même appel :

```bash
cargo run -- create-api-key <tenant> <username> --label "ci-key" --expires-days 30
```

---

## Authentifier les requêtes

Enregistrez `ApiKeyBackend` dans votre [chaîne de backends
d'authentification](auth-backends.md) et le middleware authentifie toute requête
`Authorization: Bearer {prefix}.{secret}` :

```rust
use std::sync::Arc;
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};
use rustango::tenancy::RouterAuthExt;

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),   // HTTP Basic (humains)
    Arc::new(ApiKeyBackend),  // clé Bearer  (machines)
];

let app = Router::new()
    .route("/api/data", get(handler))
    .require_auth(backends, pool);
```

Un client appelle alors :

```bash
curl https://api.example.com/api/data \
  -H "Authorization: Bearer a1b2c3d4.the-secret-half"
```

Le backend trouve l'`ApiKey` par son préfixe de 8 caractères, vérifie
`expires_at`, vérifie le secret contre le hash stocké, charge l'utilisateur
propriétaire, et l'injecte pour que vos handlers le lisent via
[`CurrentUser`](auth-backends.md). Un mauvais secret ou un préfixe inconnu est un
`401` ; une clé expirée est rejetée ; un propriétaire désactivé est un `403`.

---

## Notes de sécurité

- **Le secret est montré une fois.** Seuls le préfixe + le hash argon2id sont
  persistés — il n'y a pas de récupération, seulement une réémission.
- **Le préfixe est stocké en clair à dessein** — c'est l'index de recherche en
  O(1). Une fuite de base de données révèle quels préfixes existent, jamais les
  secrets.
- **Le timing est égalisé.** Un préfixe inconnu exécute quand même une
  vérification factice, de sorte qu'une clé manquante prend à peu près le même
  temps qu'une vraie — pas d'énumération via le timing de réponse.
- **Cantonnez les clés à un utilisateur, définissez une expiration, et faites-les
  tourner.** Émettez-en une par intégration afin de pouvoir en révoquer une sans
  perturber les autres ; préférez des fenêtres `expires_at` courtes pour un accès
  temporaire.
- **Distinction des JWT :** le backend traite une valeur Bearer comme une clé
  d'API uniquement quand son premier segment séparé par un point fait exactement
  8 caractères — ainsi les clés d'API et les [JWT](auth-jwt.md) peuvent partager
  l'en-tête `Authorization: Bearer`.

---

## Voir aussi

- [Backends d'authentification](auth-backends.md) — la chaîne sur laquelle se
  branche `ApiKeyBackend`, et l'extracteur `CurrentUser` + le middleware
  `require_auth`/`require_perm`.
- [Signature de requêtes HMAC](auth-hmac.md) — pour les appelants machine qui ont
  besoin d'intégrité par requête, pas seulement d'un identifiant bearer.
- [Mots de passe](auth-passwords.md) — la même discipline « hasher, ne pas
  stocker » pour les connexions humaines.
- [JWT](auth-jwt.md) — tokens sans état à courte durée de vie, l'autre option
  machine.
