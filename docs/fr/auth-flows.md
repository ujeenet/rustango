# Flux de compte (réinitialisation, vérification, lien magique)

Les flux dont chaque application a besoin en périphérie de la connexion : **réinitialisation
du mot de passe**, **vérification de l'e-mail** et **connexion par lien magique (sans mot de passe)**.
Tous les trois ont la même forme — envoyer par e-mail à l'utilisateur un lien inviolable et à durée
limitée, puis agir lorsqu'il clique dessus — et **Rustango** les construit sur un même socle :
les **URL signées**. Une URL signée est une URL normale à laquelle est ajoutée une signature HMAC,
de sorte que le serveur peut faire confiance à ses paramètres sans rien stocker.

[![Flux de compte dans Rustango : signed_url::sign ajoute une signature HMAC + une expiration ; les trois flux (réinitialisation du mot de passe, vérification de l'e-mail, lien magique) émettent un lien, l'envoient par e-mail et le vérifient au clic](img/auth-flows.png)](img/auth-flows.png)

> **Un terme vous est inconnu ?** *HMAC*, *token*, *expiration* — voir le [glossaire](glossary.md).

> **Source :** `rustango::signed_url` (`sign`, `verify`, `SignedUrlError`) et
> `rustango::auth_flows` (`PasswordReset`, `EmailVerification`, `MagicLink`,
> `confirm_password_reset_pool_into`) — derrière les fonctionnalités `signed_url` / `auth_flows`
> (activées par défaut ; la confirmation de réinitialisation nécessite en plus `passwords` + un backend BD).
>
> **Version exécutable :** chaque extrait est copié depuis
> [`auth_flows_doc.rs`](../crates/rustango/tests/auth_flows_doc.rs)
> (`cargo test -p rustango --features sqlite --test auth_flows_doc`).

## Table des matières

- [Les URL signées : le socle](#signed-urls-the-substrate)
- [Réinitialisation du mot de passe](#password-reset)
- [Vérification de l'e-mail](#email-verification)
- [Connexion par lien magique](#magic-link-login)
- [Tokens à usage unique](#single-use-tokens)
- [Ce que vous fournissez](#what-you-provide)
- [Voir aussi](#see-also)

---

## Les URL signées : le socle

`sign` ajoute une signature HMAC-SHA256 (et une expiration facultative) sur le chemin + la
requête de l'URL. `verify` la recalcule : altérez un paramètre quelconque, utilisez le mauvais
secret ou laissez-la expirer, et elle échoue.

```rust
use rustango::signed_url::{sign, verify, SignedUrlError};

let url = "https://app.example.com/files/42?user_id=7";
let signed = sign(url, secret, None);     // None = never expires
assert!(verify(&signed, secret).is_ok());

// Flip any signed byte → InvalidSignature.
let tampered = signed.replace("user_id=7", "user_id=8");
assert_eq!(verify(&tampered, secret), Err(SignedUrlError::InvalidSignature));
```

Ajoutez une TTL et un lien expiré est rejeté (`sign_at` / `verify_at` prennent des secondes unix
explicites pour des tests déterministes) :

```rust
use rustango::signed_url::{sign_at, verify_at, SignedUrlError};

let signed = sign_at(url, secret, Some(100));         // expires at t=100
assert!(verify_at(&signed, secret, 50).is_ok());      // before → ok
assert_eq!(verify_at(&signed, secret, 1000), Err(SignedUrlError::Expired));
```

La requête est triée avant la signature, l'ordre des paramètres n'a donc pas d'importance. Les
erreurs sont `MissingSignature`, `MalformedSignature`, `InvalidSignature`, `Expired`.

---

## Réinitialisation du mot de passe

Les assistants `auth_flows` enveloppent les URL signées avec une **étiquette de finalité** (pour
qu'un token de réinitialisation ne puisse pas être rejoué comme un lien magique) et encodent
l'identifiant de l'utilisateur. `PasswordReset` fournit aussi un assistant de confirmation qui
vérifie le token et **fait tourner le hash stocké** en un seul appel.

```rust
use std::time::Duration;
use rustango::auth_flows::{PasswordReset, confirm_password_reset_pool_into};

// 1. User asks to reset → look them up → issue a link → email it.
let url = PasswordReset::issue(
    "https://app.example.com/auth/reset",   // your callback route
    user_id,                                // encoded in the token
    secret,
    Duration::from_secs(3600),              // 1-hour TTL
);
mailer.send(&Email::new().to(addr).subject("Reset your password").body(&url)).await?;

// 2. User clicks + submits a new password → verify + rotate the hash.
let user_id = confirm_password_reset_pool_into(
    &pool, &url, "a-brand-new-strong-password", secret,
    "rustango_users", "id", "password_hash",  // table, pk col, password col
).await?;
```

L'assistant de confirmation impose une longueur minimale, hache le nouveau mot de passe avec
argon2id et l'écrit — en rejetant les entrées faibles, expirées, altérées ou au mauvais secret sans
toucher à la ligne :

```rust
// valid token + strong pw → hash rotated (starts "$argon2…")
// "short"                  → Err(WeakPassword), nothing written
// user_id tampered         → Err(InvalidSignature), nothing written
```

> `confirm_password_reset_pool` est la forme pratique qui suppose les valeurs par défaut
> `rustango_users` / `id` / `password_hash` ; utilisez `_into` pour pointer vers votre propre
> table/colonnes.

---

## Vérification de l'e-mail

`EmailVerification` encode à la fois l'identifiant de l'utilisateur **et** l'e-mail, de sorte qu'à
la vérification vous récupérez les deux et pouvez confirmer que l'adresse correspond toujours
(pour attraper les liens envoyés avant un changement d'e-mail). Il n'y a pas d'écriture BD intégrée
ici — vous définissez votre propre colonne « vérifié » :

```rust
use rustango::auth_flows::EmailVerification;

// On signup:
let url = EmailVerification::issue(callback, user_id, &email, secret, Duration::from_secs(86_400));
mailer.send(&Email::new().to(&email).subject("Confirm your email").body(&url)).await?;

// On click:
let (user_id, email) = EmailVerification::verify(&url, secret)?;
// → if email still matches the user's current address, mark them verified
```

---

## Connexion par lien magique

`MagicLink` encode uniquement l'e-mail — l'utilisateur clique, vous recherchez le compte et
émettez une [session](auth-sessions.md). Gardez une TTL courte (10–30 min) et rendez-le
**à usage unique** (section suivante), car le lien *est* l'identifiant :

```rust
use rustango::auth_flows::MagicLink;

let url = MagicLink::issue(callback, &email, secret, Duration::from_secs(900));
mailer.send(&Email::new().to(&email).subject("Your sign-in link").body(&url)).await?;

// On click:
let email = MagicLink::verify_single_use(&url, secret, &cache).await?;
// → look up the user by email, create a session
```

---

## Tokens à usage unique

`verify` seul ne vérifie que la signature + l'expiration, donc un lien divulgué est rejouable
jusqu'à son expiration. Pour la connexion et la réinitialisation, préférez `verify_single_use(url, secret,
&cache)` — il enregistre la signature du token dans un `Cache` et refuse une deuxième
utilisation :

```rust
// first click  → Ok(email)
// same link reused → Err(AuthFlowError::AlreadyUsed)
```

Adossez-le à un cache **partagé** (Redis) en production, pour qu'un token ne puisse pas être rejoué
contre un réplica différent. La vérification échoue en mode fermé (une erreur de cache refuse plutôt
que de risquer un rejeu).

---

## Ce que vous fournissez

Le framework émet/vérifie les tokens et (pour la réinitialisation) écrit le hash ; votre application
fournit le reste :

- Un **secret** (une clé d'application stable ; 32 octets par convention).
- Un **mailer** pour envoyer les liens — `rustango::email` fournit `ConsoleMailer`,
  `SmtpMailer` et `InMemoryMailer` (pratique dans les tests).
- Une **table utilisateur** avec les colonnes dont chaque flux a besoin (e-mail pour la recherche
  vérification/lien magique ; une colonne de hash de mot de passe pour la réinitialisation ; une
  colonne « vérifié » qui vous appartient).
- Les **routes de rappel** qui reçoivent le clic et l'émission de session pour la connexion par
  lien magique.

---

## Voir aussi

- [Mots de passe](auth-passwords.md) — le hachage que la réinitialisation fait tourner.
- [Sessions](auth-sessions.md) — ce que la connexion par lien magique crée en cas de succès.
- [Signature de requête HMAC](auth-hmac.md) — la même primitive HMAC, appliquée aux requêtes API
  plutôt qu'aux URL.
- [Guide de sécurité](security.md) — la liste de durcissement plus large.
