# API d'authentification JWT

Le module [JWT autonome](auth-jwt.md) signe et vérifie un seul token. Une vraie
API a besoin de tout le **cycle de vie** : un token d'*accès* à courte durée, un
token de *rafraîchissement* à longue durée, la rotation au rafraîchissement, et
la **révocation** pour la déconnexion. **Rustango** fournit tout cela sous la
forme de `JwtLifecycle` — et un routeur clé en main qui monte pour vous
`POST /api/auth/login`, `/refresh`, `/logout`, et `GET /me`.

[![API d'authentification JWT : login émet une paire access+refresh, refresh effectue une rotation et met en liste noire l'ancien token, logout révoque via un magasin de JTI](img/auth-jwt-api.png)](img/auth-jwt-api.png)

> **Source :** `rustango::tenancy::jwt_lifecycle` (`JwtLifecycle`, `JwtTokenPair`,
> `JwtClaims`) et `rustango::tenancy::auth_routes` (`jwt_router`, `Config`) +
> `rustango::jti_store` (`JtiStore`, `InMemoryJtiStore`) — derrière `jwt` +
> `tenancy`.
>
> **Version exécutable :** le moteur de tokens est couvert par le test
> [`auth_demo`](../crates/rustango/examples/auth_demo/tests/auth_jwt_api.rs) —
> `cargo test -p auth_demo --test auth_jwt_api`. Les endpoints HTTP sont
> cloisonnés par tenant et éprouvés de bout en bout par le propre
> `crates/rustango/tests/tenant_auth_live.rs` du framework.

> **Un terme vous est inconnu ?** *token d'accès/de rafraîchissement*,
> *rotation*, *révocation* — voir le [glossaire](glossary.md).

> Complément approfondi de la section « Émettre et rafraîchir des JWT » du
> [Guide de sécurité](security.md). Pour un token unique géré manuellement, voir
> plutôt [JWT (autonome)](auth-jwt.md).

---

## Table des matières
- [Le routeur intégré](#the-built-in-router) · [Le câblage](#wiring-it-up)
- [Le moteur de tokens](#the-token-engine-jwtlifecycle) · [Rafraîchissement et rotation](#refresh-and-rotation)
- [Révocation et le magasin de JTI](#revocation-and-the-jti-store) · [Claims personnalisés](#custom-claims)
- [Notes et limites](#notes-and-limits)

---

## Le routeur intégré

`jwt_router` monte les quatre endpoints standard sur la table `rustango_users`
propre à chaque tenant — les ~50 lignes de boilerplate de login que tout projet
réécrit sinon :

| Méthode | Chemin | Corps / Auth | Renvoie |
|---|---|---|---|
| POST | `/api/auth/login` | `{username, password}` | `{access, refresh, user}` |
| POST | `/api/auth/refresh` | `{refresh}` | `{access, refresh}` |
| POST | `/api/auth/logout` | `Authorization: Bearer <access>` | `204` (révoque le JTI) |
| GET | `/api/auth/me` | `Authorization: Bearer <access>` | `{user_id, username, is_superuser}` |

Login vérifie le mot de passe avec [argon2id](auth-passwords.md), puis émet une
paire. Les chemins, TTL et la clé de signature sont configurables via `Config`.

## Le câblage

```rust
use rustango::tenancy::auth_routes::{jwt_router, Config};

rustango::manage::Cli::new()
    .tenancy()
    .api(my_app::urls::api()
        .merge(jwt_router(Config::default())))   // monte /api/auth/*
    .run()
    .await
```

`Config::default()` signe avec `RUSTANGO_SESSION_SECRET` (la même clé que le
cookie de session admin) et utilise des TTL de 15 min pour l'accès / 7 jours pour
le rafraîchissement. Redéfinissez `prefix`, `access_ttl_secs`,
`refresh_ttl_secs`, ou `session_secret` selon vos besoins. Les endpoints
s'exécutent dans le contexte du tenant, montez-les donc dans une application
tenancy.

```sh
# Login → access + refresh
curl -sX POST localhost:8080/api/auth/login \
  -H 'content-type: application/json' \
  -d '{"username":"alice","password":"hunter2hunter"}'

# Appeler un endpoint protégé
curl localhost:8080/api/auth/me -H "Authorization: Bearer $ACCESS"
```

---

## Le moteur de tokens (`JwtLifecycle`)

Sous le routeur se trouve `JwtLifecycle` — utilisable directement si vous voulez
le cycle de vie sans la forme HTTP intégrée :

```rust
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

let jwt = JwtLifecycle::new(secret_32_bytes);

// Login : émettre la paire.
let pair = jwt.issue_pair(user_id);
// → pair.access  (TTL court, à envoyer dans l'en-tête Authorization)
// → pair.refresh (TTL long, à stocker dans un cookie HttpOnly / stockage sécurisé)

// Requête authentifiée : vérifier le token d'accès.
match jwt.verify_access(&access) {
    Some(claims) => { /* claims.sub est l'id utilisateur */ }
    None => { /* 401 : invalide, expiré, révoqué, ou mauvais type */ }
}
```

Les tokens d'accès et de rafraîchissement ne sont **pas interchangeables** —
`verify_access` rejette un token de rafraîchissement et vice versa, de sorte
qu'un token d'accès à courte durée volé ne peut pas servir à en forger de
nouveaux :

```rust
let pair = jwt.issue_pair(42);
assert!(jwt.verify_refresh(&pair.access).is_none());
assert!(jwt.verify_access(&pair.refresh).is_none());
```

---

## Rafraîchissement et rotation

`refresh` échange un token de rafraîchissement valide contre une **nouvelle
paire** et met en liste noire le JTI de l'ancien token de rafraîchissement —
expiration glissante avec des tokens de rafraîchissement à usage unique (le
rejeu de l'ancien est refusé) :

```rust
let pair = jwt.issue_pair(7);
let rotated = jwt.refresh(&pair.refresh).expect("refresh ok");
assert_ne!(pair.access, rotated.access);
assert!(jwt.refresh(&pair.refresh).is_none());   // l'ancien refresh est maintenant mort
```

Par défaut, `refresh` **préserve** les claims personnalisés du token. Si les
permissions ont pu changer (rôle révoqué, portée réduite), utilisez
`refresh_with(token, new_claims)` pour substituer un payload frais tout en
mettant quand même en liste noire l'ancien JTI de rafraîchissement.

---

## Révocation et le magasin de JTI

Chaque token porte un `jti` unique. `revoke` l'ajoute à une liste noire de sorte
que les appels `verify_*` suivants échouent jusqu'à ce que le token ait de toute
façon expiré — c'est ce qu'appelle `POST /api/auth/logout` :

```rust
let pair = jwt.issue_pair(1);
assert!(jwt.revoke(&pair.access));
assert!(jwt.verify_access(&pair.access).is_none());
```

La liste noire réside dans un `JtiStore` interchangeable. Le `InMemoryJtiStore`
par défaut est **mono-processus et perd les révocations au redémarrage** —
convient pour une seule instance. Tout déploiement multi-réplica DOIT installer
un magasin partagé et durable (Redis / BD) pour qu'une déconnexion sur une
réplica soit honorée par toutes :

```rust
use rustango::jti_store::{InMemoryJtiStore, JtiStore};
use std::sync::Arc;

let shared: Arc<dyn JtiStore> = Arc::new(InMemoryJtiStore::new()); // à remplacer par Redis en prod
let a = JwtLifecycle::new(secret.clone()).with_jti_store(Arc::clone(&shared));
let b = JwtLifecycle::new(secret).with_jti_store(Arc::clone(&shared));

let pair = a.issue_pair(5);
a.revoke(&pair.access);
assert!(b.verify_access(&pair.access).is_none());   // B voit la révocation de A
```

> Sans magasin partagé, `/logout` est au mieux « best-effort » : un token révoqué
> peut encore être accepté sur une autre réplica jusqu'à son expiration
> naturelle. C'est le paramètre de production le plus important pour
> l'authentification JWT.

---

## Claims personnalisés

Intégrez `roles` / `tenant` / `scope` directement dans le token pour que la
vérification ne nécessite aucune consultation de BD. Les noms réservés (`sub`,
`exp`, `jti`, `typ`) sont rejetés :

```rust
let custom = serde_json::json!({ "roles": ["admin"], "tenant": "acme" })
    .as_object().unwrap().clone();
let pair = jwt.issue_pair_with(99, custom)?;

let claims = jwt.verify_access(&pair.access).unwrap();
let roles: Vec<String> = claims.get_custom("roles").unwrap();   // ["admin"]
```

Les claims personnalisés survivent à `refresh` (reportés sur la nouvelle paire)
sauf si vous utilisez `refresh_with`.

---

## Notes et limites

- **Sessions vs JWT vs ceci :** un [JWT](auth-jwt.md) simple ne peut pas être
  révoqué ; une [Session](auth-sessions.md) est révocable mais nécessite une
  consultation du magasin à chaque requête ; `JwtLifecycle` est la voie
  intermédiaire — vérification sans état, plus une liste de blocage JTI pour les
  révocations dont vous avez réellement besoin (déconnexion, rotation).
- **Les endpoints HTTP sont cloisonnés par tenant.** `jwt_router` résout les
  utilisateurs via le contexte du tenant + `rustango_users` ; montez-le dans une
  application `.tenancy()`. Le moteur de tokens (`JwtLifecycle`) lui-même n'a pas
  cette exigence.
- **Associez ceci** au `JwtBackend` de la [chaîne de backends
  d'authentification](auth-backends.md) pour authentifier des routes arbitraires
  à partir de l'en-tête `Authorization: Bearer`.
- **Signature HS256**, plancher de clé de 32 octets — même algorithme et mêmes
  contraintes que le [JWT autonome](auth-jwt.md#security-model).


---

## Voir aussi

- [JWT (autonome)](auth-jwt.md)
- [Backends d'authentification](auth-backends.md)
- [Sessions](auth-sessions.md)
- [Guide de sécurité](security.md)
