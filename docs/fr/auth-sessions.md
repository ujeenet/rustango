# Sessions

Une session maintient un utilisateur connecté à travers les requêtes en
remettant au navigateur un **identifiant opaque** dans un cookie et en conservant
tout le reste côté serveur. Le `SessionStore` de **Rustango** place cet état dans
un cache (Redis en production, en mémoire pour les tests), de sorte que le cookie
ne transporte aucun secret et qu'une session peut être **révoquée
instantanément** — supprimez l'entrée et chaque réplique voit la déconnexion à la
requête suivante.

[![Les sessions dans Rustango : le cookie ne contient qu'un identifiant opaque, le SessionStore conserve les données dans Redis, et destroy() les révoque partout](img/auth-sessions.png)](img/auth-sessions.png)

> **Source :** `rustango::sessions` (`Session`, `SessionStore`) +
> `rustango::cache` (`BoxedCache`, `InMemoryCache`) — derrière la fonctionnalité
> `sessions` (activée par défaut ; tire `cache`). Pour un magasin adossé à Redis
> en production, ajoutez la fonctionnalité `cache-redis` (désactivée par défaut)
> pour obtenir `RedisCache`.
>
> **Version exécutable :** les extraits ci-dessous sont copiés depuis l'exemple
> testé [`auth_demo`](../crates/rustango/examples/auth_demo/tests/auth_sessions.rs)
> — `cargo test -p auth_demo --test auth_sessions`.

> **Un terme vous est inconnu ici ?** *session*, *identifiant opaque*, *cookie*,
> *cache* — voir le [glossaire](glossary.md).

> Compagnon d'approfondissement du [guide de sécurité](security.md). Le
> verrouillage des routes derrière une session connectée est traité dans
> [Décorateurs d'authentification](auth-decorators.md) ; pour des jetons d'API
> sans état à la place, voir [JWT](auth-jwt.md).

---

## Table des matières
- [Démarrage rapide](#quick-start) · [Sessions vs JWT](#sessions-vs-jwt)
- [Le sac de session](#the-session-bag) · [Le cookie](#the-cookie)
- [Choisir un backend](#picking-a-backend) · [Expiration et renouvellement glissant](#expiry-and-sliding-renewal)
- [Modification en place](#updating-a-session-in-place) · [Remarques et limites](#notes-and-limits)

---

## Démarrage rapide

```rust
use rustango::sessions::{Session, SessionStore};
use rustango::cache::{BoxedCache, RedisCache};
use std::sync::Arc;

let store = SessionStore::new(Arc::new(RedisCache::new("redis://localhost/0")?) as BoxedCache);

// After the password check (see auth-passwords.md): stash who the user is,
// save → an opaque id, and set that id as the cookie.
let mut session = Session::new();
session.set("user_id", user.id);
let sid = store.save(&session).await?;
// Set-Cookie: rustango_session={sid}; HttpOnly; SameSite=Lax; Secure; Path=/

// On later requests: read the id from the cookie, load the session back.
let session = store.load(&sid).await?.unwrap_or_default();
let user_id: Option<i64> = session.get("user_id");

// Logout: drop the server-side entry — the cookie is now meaningless.
store.destroy(&sid).await?;
```

L'identifiant représente 192 bits d'aléa OS-CSPRNG, encodés en base64url sur 32
caractères — bien au-dessus du plancher de 128 bits pour les jetons de session,
et impossible à deviner.

---

## Sessions vs JWT

Les deux répondent à « qui est cette requête ? », avec des compromis opposés :

| | Session | [JWT](auth-jwt.md) |
|---|---|---|
| État | côté serveur (recherche dans le cache par requête) | sans état (jeton auto-contenu) |
| Révocation | **instantanée** — `destroy()` l'entrée | difficile — valide jusqu'à expiration (nécessite une liste de blocage) |
| Idéal pour | applications navigateur, « déconnecter cet utilisateur MAINTENANT » | API, service à service, pas de magasin partagé |

Optez pour les sessions lorsque vous devez déconnecter de force quelqu'un
(changement de mot de passe, « déconnecter tous les appareils », un compte banni).
Optez pour le JWT lorsque vous voulez zéro recherche par requête et que vous
n'avez pas de cache partagé.

---

## Le sac de session

`Session` est un sac typé clé→valeur doté d'un bit d'altération (*dirty bit*), de
sorte que le magasin peut sauter une écriture lorsque rien n'a changé :

```rust
let mut s = Session::new();
s.set("user_id", 42_i64);            // serialize any Serialize value
s.set("role", "editor");
let uid: Option<i64> = s.get("user_id");   // None if absent or wrong type
s.remove("role");
s.clear();                            // wipe everything (e.g. on logout)
```

`get` est **tolérant aux erreurs** : une clé manquante *ou* une valeur qui ne se
désérialise pas dans le type demandé renvoie `None` plutôt que de paniquer — de
sorte qu'un changement de schéma ne provoque jamais d'erreur 500 sur une requête.

---

## Le cookie

Le cookie ne contient que `sid`. Configurez-le avec les attributs de sécurité
dont un cookie de session a besoin :

- **`HttpOnly`** — JavaScript ne peut pas le lire (émousse le vol de jeton par
  XSS).
- **`SameSite=Lax`** — non envoyé sur les sous-requêtes intersites (défense CSRF ;
  associez-le aux [jetons CSRF](security.md#protecting-against-csrf) pour les
  envois de formulaire).
- **`Secure`** — HTTPS uniquement (à retirer seulement pour le développement HTTP
  local).
- **`Path=/`** — visible pour toute l'application.

Rien de sensible n'est dans le cookie ; un cookie divulgué est donc exactement
aussi puissant que la session qu'il désigne — et vous pouvez révoquer celle-ci
côté serveur à tout moment.

---

## Choisir un backend

`SessionStore::new` accepte n'importe quel `BoxedCache` :

- **`RedisCache`** — production. Partagé entre les répliques, de sorte qu'une
  connexion sur une instance et une déconnexion sur une autre sont toutes deux
  visibles partout.
- **`InMemoryCache`** — processus unique / tests. Rapide, sans dépendances, mais
  les sessions ne survivent pas à un redémarrage et ne sont pas partagées entre
  les répliques.

```rust
use rustango::cache::{BoxedCache, InMemoryCache};
use std::sync::Arc;

// Tests / single-process:
let store = SessionStore::new(Arc::new(InMemoryCache::new()) as BoxedCache);
```

---

## Expiration et renouvellement glissant

Les sessions ont par défaut une TTL de **2 semaines**. Remplacez-la par magasin,
et appelez `touch` à chaque requête authentifiée pour une expiration glissante
(les utilisateurs actifs restent connectés, les inactifs finissent par expirer) :

```rust
use std::time::Duration;

let store = SessionStore::new(cache).ttl(Duration::from_secs(60 * 60)); // 1 hour

// On each request, after a successful load — extend without rewriting:
store.touch(&sid).await?;   // Ok(false) if the session is already gone
```

---

## Modification d'une session en place

`save` frappe toujours un nouvel identifiant (utilisez-le à la connexion). Pour
modifier une session existante pendant une requête, chargez → mutez →
`save_with_id` sous le même identifiant :

```rust
let mut s = store.load(&sid).await?.unwrap_or_default();
s.set("last_seen", chrono::Utc::now().to_rfc3339());
store.save_with_id(&sid, &s).await?;
```

---

## Remarques et limites

- **La révocation est la fonctionnalité phare** — `destroy()` (déconnexion) et
  l'expiration par TTL prennent toutes deux effet à la requête suivante, sur
  chaque réplique partageant le cache.
- **Les identifiants corrompus ou inconnus se chargent comme `None`**
  (*fail-open*) : un changement de schéma de cache ou un cookie falsifié produit
  une session vide, pas une erreur — la requête est simplement non authentifiée.
- **Le magasin ne pose pas le cookie à votre place** — il gère l'état côté
  serveur ; vous attachez/lisez le cookie `sid` dans votre handler (ou via une
  couche). Cela le rend utilisable depuis n'importe quel câblage de framework.
- **Frappez un nouvel identifiant de session lors d'un changement de privilège**
  (par ex. juste après la connexion) pour éviter la fixation de session — `save`
  le fait déjà puisqu'il génère toujours un nouvel identifiant.


---

## Voir aussi

- [Décorateurs d'authentification](auth-decorators.md)
- [JWT](auth-jwt.md)
- [Backends d'authentification](auth-backends.md)
- [Guide de sécurité](security.md)
