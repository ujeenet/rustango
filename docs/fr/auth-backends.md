# Backends d'authentification

Un **backend d'authentification** répond à une seule question : *étant donné une
requête entrante, qui est l'utilisateur ?* **Rustango** vous permet d'en empiler
plusieurs — HTTP Basic, clé d'API, JWT — dans une chaîne que le middleware
d'authentification essaie dans l'ordre, de sorte qu'une même application peut
accepter humains et machines sur les mêmes routes. C'est l'idée
`AUTHENTICATION_BACKENDS` de Django, câblée à axum. Associez-la à
`require_auth` / `require_perm` pour verrouiller les routes et à l'extracteur
`CurrentUser` pour lire le résultat.

[![Les backends d'authentification dans Rustango : une requête traverse une chaîne de backends (ModelBackend, ApiKeyBackend, JwtBackend) ; le premier à reconnaître l'identifiant l'emporte et injecte CurrentUser, puis require_perm vérifie un codename](../img/auth-backends.png)](../img/auth-backends.png)

> **Un terme vous est inconnu ici ?** *Backend*, *middleware*, *extracteur*,
> *codename de permission* — voir le [glossaire](glossary.md).

> **Source :** `rustango::tenancy::auth_backends` (`AuthBackend`, `ModelBackend`,
> `ApiKeyBackend`, `JwtBackend`, `AuthUser`, `AuthError`) et
> `rustango::tenancy::{RouterAuthExt, CurrentUser}` — derrière la fonctionnalité
> `tenancy`. Un registre portable et indépendant de la base de données vit aussi
> à `rustango::auth_backends` (toujours compilé).
>
> **Version exécutable :** chaque extrait est copié depuis
> [`auth_backends_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_backends_doc.rs)
> (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Table des matières

- [La chaîne](#the-chain) · [Les backends intégrés](#the-built-in-backends)
- [Verrouiller les routes : require_auth](#gating-routes-require_auth)
- [Lire l'utilisateur : CurrentUser](#reading-the-user-currentuser)
- [Permissions : require_perm](#permissions-require_perm)
- [Le registre portable](#the-portable-registry)
- [Voir aussi](#see-also)

---

## La chaîne

Vous passez à `require_auth` un `Vec<Arc<dyn AuthBackend>>`. À chaque requête, le
middleware les essaie **dans l'ordre** :

- le **premier** backend qui reconnaît l'identifiant l'emporte (renvoie
  l'utilisateur) ;
- un backend qui ne le reconnaît pas renvoie « aucun » et le suivant est essayé ;
- si un backend échoue durement (par ex. un compte inactif sur un jeton
  *valide*), la chaîne s'arrête avec cette erreur ;
- si aucun ne correspond, la requête reçoit un `401` (avec `require_auth`) ou
  poursuit de manière anonyme (avec `optional_auth`).

```rust
use std::sync::Arc;
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),    // HTTP Basic  → humans
    Arc::new(ApiKeyBackend),   // Bearer key  → machines
];
```

---

## Les backends intégrés

| Backend | Identifiant qu'il lit | Identifie un utilisateur par |
|---|---|---|
| `ModelBackend` | `Authorization: Basic <base64(user:pass)>` | nom d'utilisateur + vérification du mot de passe argon2id contre `rustango_users` |
| `ApiKeyBackend` | `Authorization: Bearer <prefix.secret>` | la table `rustango_api_keys` (voir [Clés d'API](auth-api-keys.md)) |
| `JwtBackend` | `Authorization: Bearer <jwt>` | un jeton HS256 signé (voir [JWT](auth-jwt.md)) |

`ApiKeyBackend` et `JwtBackend` lisent tous deux `Bearer` et lèvent l'ambiguïté
par la forme (le premier segment séparé par un point d'une clé d'API fait
exactement 8 caractères). Construisez `JwtBackend` avec un secret d'**au moins 32
octets** (`JwtBackend::new(secret)` panique sinon) :

```rust
use rustango::tenancy::auth_backends::JwtBackend;

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),
    Arc::new(JwtBackend::new(jwt_secret_at_least_32_bytes.to_vec())),
];
```

Écrivez un backend personnalisé en implémentant le trait (une seule méthode async
qui inspecte les `Parts` de la requête et renvoie `Option<AuthUser>`) :

```rust
use async_trait::async_trait;   // add `async-trait` to your Cargo.toml
use axum::http::request::Parts;
use rustango::sql::Pool;
use rustango::tenancy::auth_backends::{AuthBackend, AuthError, AuthUser};

struct HeaderBackend;

#[async_trait]
impl AuthBackend for HeaderBackend {
    async fn authenticate(&self, parts: &Parts, _pool: &Pool)
        -> Result<Option<AuthUser>, AuthError>
    {
        // ...inspect parts.headers, return Some(AuthUser{..}) or Ok(None)
        Ok(None)
    }
}
```

---

## Verrouiller les routes : require_auth

`RouterAuthExt` ajoute le middleware. `require_auth` rejette les requêtes
anonymes avec un `401` ; `optional_auth` les laisse passer (de sorte qu'un
handler peut se ramifier selon connecté ou non) :

```rust
use rustango::tenancy::RouterAuthExt;

let app = Router::new()
    .route("/profile", get(profile))
    .require_auth(backends, pool);     // 401 if no backend matches
```

Comportement vérifié :

```rust
// no credentials               → 401
// Basic alice:<correct>        → 200
// Basic alice:<wrong>          → 401   (no backend accepted; no enumeration)
// Bearer <valid api key>       → 200
```

---

## Lire l'utilisateur : CurrentUser

Les handlers lisent l'utilisateur authentifié avec l'extracteur `CurrentUser`.
Il est infaillible — `Some(user)` lorsqu'un backend en a résolu un, `None` sinon :

```rust
use rustango::tenancy::CurrentUser;

async fn profile(CurrentUser(user): CurrentUser) -> Response {
    match user {
        Some(u) => format!("hello {}", u.username).into_response(),
        None    => StatusCode::UNAUTHORIZED.into_response(),
    }
}
```

> **Piège :** parce que `CurrentUser` est infaillible, oublier `require_auth` ne
> provoque pas d'échec de compilation — chaque requête voit simplement `None`.
> Derrière `require_auth`, les requêtes anonymes reçoivent déjà un `401`, donc
> `user` y est toujours `Some`.

---

## Permissions : require_perm

`require_perm` verrouille une route sur un **codename** de permission
(`{table}.{action}`, par ex. `post.add`). Appliquez-le au sous-routeur interne et
`require_auth` à l'externe, afin que l'utilisateur soit résolu *avant* que la
permission ne soit vérifiée :

```rust
let admin = Router::new()
    .route("/admin", get(admin_only))
    .require_perm("post.add", pool.clone());   // inner: needs the codename

let app = Router::new()
    .route("/profile", get(profile))
    .merge(admin)
    .require_auth(backends, pool);             // outer: resolves the user first
```

```rust
// alice (granted post.add)   → /admin 200
// bob   (authed, no grant)   → /admin 403
// anonymous                  → /admin 401   (auth runs first)
```

Résolution : un **superutilisateur** (actif) passe tout ; un utilisateur
**désactivé** est refusé même avec des octrois ; une surcharge explicite par
utilisateur l'emporte sur les octrois de rôle ; sinon, tout rôle détenu par
l'utilisateur qui octroie le codename passe. Octroyez avec
`set_user_perm_pool` / les rôles via `create_role_pool` + `assign_role_pool` (les
tables de permissions sont créées par `ensure_tables_pool`).

---

## Le registre portable

Séparément, `rustango::auth_backends` (à noter : racine de crate, **et non**
`tenancy`) est un petit registre **indépendant du framework** — une chaîne
`Credentials` → `Principal` dotée de son propre trait `AuthBackend`. Il n'a aucune
glu HTTP/axum ; utilisez-le lorsque vous voulez une pluggabilité de backend à la
Django au sein de votre propre code d'authentification :

```rust
use rustango::auth_backends::{AuthBackendChain, Credentials, RemoteUserBackend};

let chain = AuthBackendChain::new().with(Arc::new(RemoteUserBackend::trust_username()));
let principal = chain.authenticate(&Credentials::remote("alice")).await?;
```

Mêmes sémantiques « le premier succès l'emporte / la première erreur arrête » que
la chaîne HTTP. Pour verrouiller de vraies routes, utilisez le middleware
`tenancy` ci-dessus.

---

## Voir aussi

- [Clés d'API](auth-api-keys.md) et [JWT](auth-jwt.md) — les identifiants que
  `ApiKeyBackend` / `JwtBackend` consomment.
- [Mots de passe](auth-passwords.md) — le hachage contre lequel `ModelBackend`
  vérifie.
- [Décorateurs d'accès](auth-decorators.md) — verrouillage par handler
  `login_required` / `permission_required`, l'alternative de style décorateur à
  `require_auth`/`require_perm`.
- [Sessions](auth-sessions.md) — authentification par cookie pour les
  navigateurs.
