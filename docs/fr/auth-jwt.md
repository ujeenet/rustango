# JWT (autonome)

Un JSON Web Token est un identifiant **sans état** : une chaîne signée et
autonome que le client envoie à chaque requête, et que votre serveur vérifie
avec un secret — sans consultation de base de données ni de cache à chaque
requête. Le module `rustango::jwt` de **Rustango** est la brique minimale :
`encode` pour signer des claims, `decode` pour les vérifier et les relire,
HS256 en interne.

[![JWT autonome dans Rustango : les Claims portent des champs sub/exp/personnalisés, encode() signe avec un secret partagé, decode() vérifie la signature + l'expiration](img/auth-jwt.png)](img/auth-jwt.png)

> **Source :** `rustango::jwt` (`Claims`, `encode`, `decode`, `decode_at`,
> `decode_unverified`, `JwtError`) — derrière la fonctionnalité `jwt` (activée
> par défaut). Pour une **API** access+refresh clé en main avec révocation, voir
> [API d'authentification JWT](auth-jwt-api.md).
>
> **Version exécutable :** les extraits sont copiés depuis le test
> [`auth_demo`](../crates/rustango/examples/auth_demo/tests/auth_jwt.rs) —
> `cargo test -p auth_demo --test auth_jwt`.

> **Un terme vous est inconnu ?** *JWT*, *claims*, *sans état*, *secret* — voir le
> [glossaire](glossary.md).

> Complément approfondi de la section « Émettre et rafraîchir des JWT » du
> [Guide de sécurité](security.md).

---

## Table des matières
- [Démarrage rapide](#quick-start) · [Quand l'utiliser](#when-to-use-standalone-jwt)
- [Construire des claims](#building-claims) · [Vérifier](#verifying-a-token)
- [Modèle de sécurité](#security-model) — à lire · [Inspecter sans faire confiance](#inspecting-without-verifying)
- [Notes et limites](#notes-and-limits)

---

## Démarrage rapide

```rust
use rustango::jwt::{Claims, encode, decode};
use std::time::Duration;

// HS256 est symétrique — le même secret signe et vérifie. Doit faire >= 32 octets.
let secret = b"a-shared-signing-secret-at-least-32-bytes!!";

let mut claims = Claims::new("user-42").ttl(Duration::from_secs(900));
claims.set("roles", vec!["editor", "author"]);

let token = encode(&claims, secret)?;        // header.payload.signature

let verified = decode(&token, secret)?;       // vérifie la signature + exp/nbf
assert_eq!(verified.subject(), Some("user-42"));
let roles: Vec<String> = verified.get("roles").unwrap();
```

---

## Quand utiliser un JWT autonome

Optez pour `rustango::jwt` quand vous voulez un simple token signé et que vous
gérerez le cycle de vie vous-même :

- **Liens magiques / tokens à usage unique** — quelques claims (id utilisateur,
  finalité, `exp` court).
  Voir [Liens magiques et flux d'authentification](auth-flows.md).
- **Tokens bearer de service à service** (le pendant JWT de la [signature de
  requêtes HMAC](auth-hmac.md) — HMAC pour les requêtes canoniques façon AWS,
  JWT pour un bearer sans état).
- **Tokens SSO** que vous remettez à un tiers.

Si vous voulez une API clé en main **login → access + refresh → refresh →
logout** avec révocation de tokens, ne la construisez pas là-dessus — utilisez
l'[API d'authentification JWT](auth-jwt-api.md), qui enveloppe ce module avec
rotation + un magasin de révocation. Et si vous devez déconnecter un utilisateur
de force *maintenant*, préférez une [Session](auth-sessions.md) révocable : un
JWT simple reste valide jusqu'à son expiration.

---

## Construire des claims

`Claims` enveloppe un objet JSON, de sorte que les claims standard et vos propres
champs d'extension coexistent :

```rust
let mut claims = Claims::new("user-42")     // définit `sub` + `iat=now`
    .ttl(Duration::from_secs(3600))         // définit `iat`=now et `exp`=now+ttl
    .issuer("api.example.com")              // `iss`
    .audience("web-client")                 // `aud`
    .jti("unique-token-id");                // `jti` (pour votre propre liste de blocage)
claims.set("role", "admin");                // toute valeur Serialize
claims.set("org_id", 7_i64);
```

| Builder / setter | Claim |
|---|---|
| `Claims::new(sub)` | `sub` + `iat` |
| `Claims::empty()` | aucune (contrôle total) |
| `.ttl(Duration)` | `iat` (now) + `exp` (now+ttl) |
| `.expires_at(secs)` / `.not_before(secs)` | `exp` / `nbf` absolus |
| `.issuer(s)` / `.audience(s)` / `.jti(s)` | `iss` / `aud` / `jti` |
| `.set(name, value)` | tout claim personnalisé |

Relisez-les avec `.subject()` et `.get::<T>(name)` (renvoie `None` pour un claim
absent ou du mauvais type).

---

## Vérifier un token

```rust
use rustango::jwt::{decode, JwtError};

match decode(&token, secret) {
    Ok(claims) => { /* faire confiance à claims.subject() etc. */ }
    Err(JwtError::Expired(_))      => { /* 401 — token périmé */ }
    Err(JwtError::BadSignature)    => { /* 401 — falsifié ou mauvaise clé */ }
    Err(JwtError::NotYetValid(_))  => { /* nbf dans le futur */ }
    Err(_)                         => { /* malformé / alg non supporté */ }
}
```

`decode` vérifie la **signature**, puis `exp` et `nbf`. Pour tester le
comportement de la fenêtre temporelle (ou ajouter une tolérance de dérive),
`decode_at(token, secret, now)` vous permet de fixer la seconde « courante » :

```rust
let token = encode(&Claims::new("x").expires_at(1000), secret)?;
assert!(decode_at(&token, secret, 500).is_ok());                     // avant exp
assert!(matches!(decode_at(&token, secret, 2000), Err(JwtError::Expired(_)))); // après
```

---

## Modèle de sécurité

C'est du code de frontière d'authentification — trois choses à savoir
impérativement :

1. **`decode` ne valide PAS `iss` / `aud`.** Une signature valide prouve que le
   token a été forgé avec votre secret, pas qu'il a été forgé *pour votre
   service*. Si vous définissez `iss`/`aud` à l'émission, **vérifiez-les
   vous-même** sur les claims décodés :

   ```rust
   let c = decode(&token, secret)?;
   if c.get::<String>("aud").as_deref() != Some("web-client") {
       return Err("wrong audience");
   }
   ```

2. **Le secret doit faire ≥ 32 octets** — `encode` refuse de signer avec une clé
   plus courte (une clé courte est devinable, et une clé HMAC devinable signifie
   des tokens falsifiables). HS256 est symétrique : quiconque possède le secret
   de vérification peut aussi *forger* des tokens, il reste donc à l'intérieur de
   votre frontière de confiance (service unique / backend partagé). L'émission de
   tokens inter-organisations demande du RS256/ES256 asymétrique, que ce module
   ne fournit délibérément pas.

3. **`alg=none` et la falsification sont rejetés.** `decode` fige HS256 (la
   falsification classique « alg: none » est refusée), et toute modification de
   l'en-tête ou du payload casse la signature — vérifiée par une comparaison à
   temps constant.

Il n'y a **aucune tolérance de dérive d'horloge** : `exp`/`nbf` se comparent à la
seconde courante exacte. Si les horloges de l'émetteur et du vérificateur
dérivent, soustrayez quelques secondes via `decode_at`.

---

## Inspecter sans vérifier

`decode_unverified` lit le payload **sans** vérifier la signature ni
l'expiration — utile uniquement pour jeter un œil à un claim (p. ex. un id de
clé) afin de choisir le bon secret, puis d'appeler `decode` pour de vrai.

```rust
let peek = rustango::jwt::decode_unverified(&token)?;   // PAS de confiance
let kid = peek.get::<String>("kid");
// ... rechercher le secret pour `kid`, puis vérifier correctement :
let claims = decode(&token, &resolved_secret)?;
```

**N'autorisez jamais sur la sortie de `decode_unverified`** — elle ne porte
aucune garantie d'intégrité.

---

## Notes et limites

- **HS256 uniquement** — symétrique, un seul secret partagé. Pas de RS256/ES256
  (garde l'arbre de dépendances toujours actif réduit ; la plupart des
  applications mono-service utilisent HS256 de toute façon).
- **Sans état = non révocable.** Un JWT simple est valide jusqu'à `exp`. Si vous
  avez besoin de « déconnexion immédiate » / de révocation par token, utilisez
  l'[API d'authentification JWT](auth-jwt-api.md) (liste de blocage JTI) ou une
  [Session](auth-sessions.md) (supprimez l'entrée côté serveur).
- **Gardez `exp` court** pour les access tokens (quelques minutes). Les JWT
  simples à longue durée de vie sont un risque précisément parce qu'ils ne
  peuvent pas être révoqués.
- Associez l'émission aux [Mots de passe](auth-passwords.md) (vérifier, puis
  émettre) et protégez les routes d'API via le `JwtBackend` de la [chaîne de
  backends d'authentification](auth-backends.md).


---

## Voir aussi

- [API d'authentification JWT](auth-jwt-api.md)
- [Backends d'authentification](auth-backends.md)
- [Clés d'API](auth-api-keys.md)
- [Sessions](auth-sessions.md)
