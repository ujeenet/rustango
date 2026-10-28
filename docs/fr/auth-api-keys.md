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
> `ensure_api_keys_table_pool`, `set_api_key_secret`) — derrière la
> fonctionnalité `tenancy`.
>
> **Version exécutable :** les extraits de l'utilitaire sont copiés depuis
> [`auth_api_keys_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_api_keys_doc.rs)
> (`cargo test -p rustango --test auth_api_keys_doc`) ; le flux du middleware
> `ApiKeyBackend` depuis
> [`auth_backends_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_backends_doc.rs)
> (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Table des matières

- [Comment fonctionne une clé d'API](#comment-fonctionne-une-clé-dapi)
- [L'utilitaire autonome](#lutilitaire-autonome)
- [Le backend avec stockage](#le-backend-avec-stockage)
- [Émettre une clé (CLI + code)](#émettre-une-clé)
- [Authentifier les requêtes](#authentifier-les-requêtes)
- [Notes de sécurité](#notes-de-sécurité)
- [Voir aussi](#voir-aussi)

---

## Comment fonctionne une clé d'API

Une clé a deux parties jointes par un point : **`{prefix}.{secret}`**.

- Le **préfixe** fait 8 caractères hexadécimaux — stocké en clair et utilisé
  comme index de recherche rapide (« quelle clé est-ce ? »).
- Le **secret** fait 32 caractères hexadécimaux et est l'identifiant réel. Vous
  ne stockez qu'un **hash** de celui-ci, jamais le secret lui-même.

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

Depuis du code async, appelez `generate_key_async`, `hash_secret_async` et
`verify_key_async` ; ils calculent argon2 hors du runtime. Ils peuvent aussi
renvoyer `ApiKeyError::Busy` quand aucun créneau de hachage ne se libère à
temps.

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
de 8 caractères (indexé, non unique), le `key_hash`, un `label`, un
`expires_at` optionnel et `created_at`.

Le backend avec stockage n'utilise pas argon2id. `key_hash` est un HMAC-SHA256
du secret, sous une clé dérivée du secret serveur. La vérification reste donc
peu coûteuse à chaque requête. Le secret serveur est `RUSTANGO_SESSION_SECRET`
(en dev, le fichier de clé de session tenant). Appelez
`set_api_key_secret(secret)` une fois au démarrage pour utiliser une autre clé ;
le premier appel gagne. **Changer cette clé, ou faire tourner
`RUSTANGO_SESSION_SECRET`, rend inutilisable chaque clé émise.** Les lignes
stockées en argon2id par d'anciennes versions sont encore vérifiées et sont
réécrites en HMAC à la première utilisation.

---

## Émettre une clé

`create_api_key` génère le token, hashe le secret, insère la ligne, et renvoie le
**token en clair une seule fois**. Sur le tier prod, il échoue quand
`RUSTANGO_SESSION_SECRET` n'est pas défini :

```rust
use rustango::tenancy::auth_backends::create_api_key;

// Émettre une clé sans expiration pour l'utilisateur 42, étiquetée "ci-key".
let token = create_api_key(42, "ci-key", None, &pool).await?;
println!("Store this — it won't be shown again: {token}");

// Ou avec une expiration :
use chrono::{Duration, Utc};
let token = create_api_key(42, "tmp", Some(Utc::now() + Duration::days(30)), &pool).await?;
```

Depuis la ligne de commande, la CLI `manage` enveloppe le même appel. Elle crée
la table si besoin, et `--label` est vide par défaut :

```bash
cargo run -- create-api-key <tenant> <username> --label "ci-key" --expires-days 30
```

La CLI doit utiliser le même secret serveur que le serveur, sinon la clé ne
correspond à rien. Quand elle se rabat sur le fichier de clé de dev, elle
affiche une note.

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
    .require_auth(backends);
```

Un client appelle alors :

```bash
curl https://api.example.com/api/data \
  -H "Authorization: Bearer a1b2c3d4.the-secret-half"
```

Le backend trouve les lignes `ApiKey` par le préfixe de 8 caractères, vérifie
le secret contre chaque hash stocké, puis vérifie `expires_at`, charge
l'utilisateur propriétaire, et l'injecte pour que vos handlers le lisent via
[`CurrentUser`](auth-backends.md).

- Un mauvais secret ou un préfixe inconnu n'est pas une erreur : le backend
  suivant de la chaîne s'exécute. Si aucun n'authentifie l'appelant,
  `require_auth` répond `401`.
- Une clé expirée est un `401` immédiat.
- Un propriétaire désactivé est un `403`.
- Les échecs sont limités par tenant et par IP client. Au-delà de la limite, la
  réponse est `429`.

La clé vit dans la base d'un seul tenant, elle ne marche donc que sur ce
tenant.

---

## Notes de sécurité

- **Le secret est montré une fois.** Seuls le préfixe + le hash à clé sont
  persistés — il n'y a pas de récupération, seulement une réémission.
- **Le préfixe est stocké en clair à dessein** — c'est l'index de recherche en
  O(1). Une fuite de base de données révèle quels préfixes existent, jamais les
  secrets.
- **Le timing est égalisé.** Le hash est comparé en temps constant, et le
  secret est vérifié avant l'expiration, donc une clé expirée coûte autant
  qu'une clé inconnue.
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
