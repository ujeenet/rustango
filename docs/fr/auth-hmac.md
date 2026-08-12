# Signature de requêtes HMAC

La signature HMAC prouve **à la fois qui a envoyé une requête et qu'elle n'a pas
été altérée en transit**. Le client signe chaque requête avec un secret partagé ;
le serveur recalcule la signature et compare. Contrairement à une [clé
d'API](auth-api-keys.md) bearer — rejouable si elle est interceptée — une
signature HMAC couvre la méthode, le chemin, la requête, l'horodatage et le
corps, de sorte qu'une requête altérée ou périmée est rejetée. C'est le schéma
utilisé par AWS SigV4 et les signatures de webhooks, et **Rustango** le fournit
sous la forme d'une seule couche tower.

[![Signature HMAC dans Rustango : le client signe méthode+chemin+requête+date+hash-du-corps avec un secret partagé ; HmacAuthLayer recalcule et compare en temps constant, rejetant les requêtes altérées ou périmées](img/auth-hmac.png)](img/auth-hmac.png)

> **Un terme vous est inconnu ?** *HMAC*, *secret partagé*, *rejeu*, *comparaison en temps constant* —
> voir le [glossaire](glossary.md).

> **Source :** `rustango::hmac_auth` (`HmacAuthLayer`, `KeyResolver`, `sign_now`,
> `sign_request`) — derrière la fonctionnalité `hmac-auth` (activée par défaut ;
> la protection contre le rejeu nécessite en plus `cache`).
>
> **Version exécutable :** chaque extrait est copié depuis
> [`auth_hmac_doc.rs`](../crates/rustango/tests/auth_hmac_doc.rs)
> (`cargo test -p rustango --test auth_hmac_doc`).

## Table des matières

- [Quand l'utiliser](#when-to-use-it)
- [Ce qui est signé](#what-gets-signed)
- [Serveur : vérifier avec la couche](#server-verify-with-the-layer)
- [Client : signer une requête](#client-sign-a-request)
- [Dérive d'horloge et rejeu](#clock-skew-and-replay)
- [Limites](#limits)
- [Voir aussi](#see-also)

---

## Quand l'utiliser

| Utilisez… | Quand |
|---|---|
| [Clé d'API](auth-api-keys.md) (Bearer) | Authentification machine simple ; le risque de capture est acceptable (TLS, rotation courte). |
| **Signature HMAC** | Vous avez besoin d'**intégrité par requête + résistance au rejeu** — webhooks, API partenaires, tout ce où une requête capturée ne doit pas être réutilisable ou modifiable. |
| [JWT](auth-jwt.md) | Tokens utilisateur sans état et auto-descriptifs avec claims. |

HMAC nécessite que les deux côtés détiennent le même secret hors bande (vous le
provisionnez), et des horloges raisonnablement synchronisées.

---

## Ce qui est signé

Le client construit une chaîne canonique et lui applique HMAC-SHA256 avec le
secret partagé :

```text
<UPPERCASE-METHOD>\n
<PATH>\n
<SORTED-QUERY>\n
<X-DATE>\n
<HEX-SHA256(BODY)>
```

Deux en-têtes de requête portent le résultat :

- `X-Date` — un horodatage RFC 3339 (également partie de la chaîne signée).
- `Authorization: HMAC-SHA256 keyId=<id>,signature=<base64>`

Parce que la requête est **triée** des deux côtés, `?b=2&a=1` et `?a=1&b=2`
produisent la même signature. Parce que le corps est haché dans la chaîne,
changer un seul octet l'invalide.

---

## Serveur : vérifier avec la couche

`HmacAuthLayer::new` prend un **`KeyResolver`** — une closure qui mappe un
`keyId` à son secret (`None` ⇒ clé inconnue ⇒ 401). Attachez-le comme une couche
tower normale devant les routes que vous voulez protéger :

```rust
use std::sync::Arc;
use rustango::hmac_auth::{HmacAuthLayer, KeyResolver};
use tower::Layer;

// Résoudre les identifiants de clé en secrets — adossez ceci à votre BD / magasin de secrets.
let resolver: KeyResolver = Arc::new(|key_id: &str| {
    (key_id == "k_demo").then(|| b"shared-secret-at-least-32-bytes-long!!".to_vec())
});

let layer = HmacAuthLayer::new(resolver)
    .tolerance_secs(300);                 // fenêtre de dérive d'horloge ±5 min (défaut)

let app = protected_router.layer(layer);
```

Une requête correctement signée passe ; altérez le corps, supprimez `X-Date`, ou
signez avec une clé inconnue et c'est un `401` :

```rust
// correctement signée         → 200
// corps modifié après signature → 401  (non-correspondance de signature)
// en-tête X-Date manquant      → 401
// keyId rejeté par le resolver → 401
```

> **Pas d'extracteur d'identité.** La couche vérifie la signature mais n'**injecte
> pas** quel `keyId` a signé dans la requête — il n'y a pas d'extracteur
> `HmacUser`. Si un handler a besoin de l'identité de l'appelant, enveloppez la
> couche ou portez-la vous-même. Les rejets sont de simples réponses `401`/`413`,
> pas une erreur typée que vous filtrez.

---

## Client : signer une requête

`sign_now` signe avec l'heure actuelle et renvoie les deux valeurs d'en-tête à
attacher (`sign_request` est la variante qui prend une date RFC 3339 explicite) :

```rust
use rustango::hmac_auth::sign_now;

let body = br#"{"amount": 100}"#;
let (x_date, authorization) =
    sign_now("k_demo", b"shared-secret-at-least-32-bytes-long!!",
             "POST", "/api/charge", "", body);

// Attachez les deux en-têtes et envoyez le corps EXACT que vous avez signé :
let req = http::Request::post("/api/charge")
    .header("x-date", x_date)
    .header("authorization", authorization)
    .body(body.to_vec())?;
```

La signature est en base64 ; le hash du corps à l'intérieur de la chaîne
canonique est en hexadécimal. Envoyez le corps octet pour octet tel qu'il a été
signé — tout proxy qui le réécrit (recompression, re-sérialisation JSON) casse la
vérification.

---

## Dérive d'horloge et rejeu

L'horodatage `X-Date` borne le rejeu : une requête dont la date est en dehors de
`tolerance_secs` (défaut ±300 s) est rejetée, de sorte qu'une requête capturée
n'est réutilisable que dans cette courte fenêtre. Pour la fermer entièrement,
attachez un **magasin de nonces** (n'importe quel `cache::Cache`) et chaque
signature ne peut être dépensée qu'une seule fois dans la fenêtre :

```rust
use rustango::cache::InMemoryCache;

let layer = HmacAuthLayer::new(resolver)
    .tolerance_secs(120)
    .nonce_store(Arc::new(InMemoryCache::new()));  // rejeter les rejeux
```

En production, utilisez un magasin **partagé** (Redis) pour que la protection
tienne à travers les réplicas — un cache en processus ne garde qu'une seule
instance. La vérification de rejeu échoue en mode ouvert sur une erreur de cache
(disponibilité plutôt que le risque étroit dans la fenêtre).

---

## Limites

- **±dérive symétrique, dates RFC 3339.** Les deux horloges doivent être à peu
  près synchronisées ; le client doit envoyer le même horodatage qu'il a signé
  (`sign_now` vous le renvoie).
- **Bufférisation complète du corps.** Le corps est lu en mémoire pour le hacher
  (plafond par défaut 10 MiB → `413` ; augmentez avec `.body_limit(n)` mais
  attention à la mémoire). Les corps en streaming ne sont pas supportés.
- **La signature est en base64 sur le fil, le hash du corps est en hexadécimal**
  — facile à confondre lors de l'écriture d'un client dans un autre langage.
- **Gardez la couche la plus externe** par rapport à tout ce qui mute le corps.

---

## Voir aussi

- [Clés d'API](auth-api-keys.md) — identifiant bearer plus simple quand
  l'intégrité/le rejeu ne sont pas une préoccupation.
- [Backends d'authentification](auth-backends.md) — pour identifier un
  *utilisateur* par requête (HMAC prouve l'intégrité du message, pas une identité
  de session).
- [Webhooks](security.md) — le pendant entrant : vérifier les signatures sur les
  événements que vous recevez.
- [Middleware](middleware.md) — comment les couches tower s'attachent et
  s'ordonnent.
