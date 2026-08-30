# Décorateurs d'accès

Une fois qu'un utilisateur est authentifié, vous verrouillez les routes.
**Rustango** livre la famille `@login_required` de Django sous forme de **couches**
axum composables : attachez-en une à un routeur et les requêtes anonymes sont
refoulées — redirigées par 302 vers votre page de connexion (flux navigateur) ou
répondues par 401/403 (flux API) — avant même d'atteindre le handler.

[![Décorateurs d'accès : login_required redirige par 302 les navigateurs anonymes vers /login?next=, la famille _or_403 renvoie 401/403 pour les API, superuser_required verrouille par rôle](../img/auth-decorators.png)](../img/auth-decorators.png)

> **Source :** `rustango::auth_decorators` (`login_required`, `login_required_or_401`,
> `user_passes_test`, `superuser_required`, `active_required`,
> `permission_required` + les variantes `_or_403` ; `safe_next`, `extract_next`) —
> derrière la fonctionnalité `tenancy` (les verrous lisent l'extracteur
> `SessionUser`).
>
> **Version exécutable :** le comportement de verrouillage est couvert par le
> [`auth_demo`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/auth_demo/tests/auth_decorators.rs)
> testé — `cargo test -p auth_demo --test auth_decorators`.

> **Un terme vous est inconnu ici ?** *middleware/couche*, *extracteur*,
> *401/403* — voir le [glossaire](glossary.md).

> Compagnon d'approfondissement du [guide de sécurité](security.md). Les verrous
> lisent la session posée à la connexion — voir [Sessions](auth-sessions.md).

---

## Table des matières
- [Démarrage rapide](#quick-start) · [Verrous navigateur vs API](#browser-vs-api-gates)
- [La famille de verrous](#the-gate-family) · [Verrous par prédicat et par rôle](#predicate-and-role-gates)
- [Verrous de permission](#permission-gates) · [L'aller-retour `?next=`](#the-next-round-trip)
- [Remarques et limites](#notes-and-limits)

---

## Démarrage rapide

```rust
use rustango::auth_decorators::login_required;

// Scope the gate to a sub-router (the idiomatic shape):
let private = Router::new()
    .route("/profile", get(profile))
    .route("/settings", get(settings))
    .layer(login_required("/login"));      // anonymous → 302 /login?next=...

let app = Router::new()
    .route("/", get(home))                 // public
    .merge(private);
```

Les requêtes anonymes vers `/profile` sont redirigées vers
`/login?next=%2Fprofile` ; une requête authentifiée passe jusqu'au handler.

---

## Verrous navigateur vs API

Le même verrou se présente sous deux formes de réponse. Choisissez selon ce que
l'appelant peut faire de la réponse :

- **Navigateur / HTML** → les verrous de base **redirigent par 302** vers votre
  page de connexion (un humain peut la suivre et se connecter).
- **API JSON** → la famille `_or_403` renvoie des **codes de statut** :
  `401 Unauthorized` pour l'anonyme, `403 Forbidden` pour l'authentifié-mais-non-autorisé
  (un client ne peut pas afficher une page de connexion HTML, et la distinction
  401/403 lui permet de différencier « connectez-vous » de « vous ne pouvez pas
  faire cela »).

```rust
// Browser: redirect to /login
let app = Router::new().route("/dashboard", get(dash)).layer(login_required("/login"));

// API: 401 for anonymous, never a redirect
let api = Router::new().route("/api/me", get(me)).layer(login_required_or_401());
```

---

## La famille de verrous

| Verrou (navigateur, 302) | Variante API (401/403) | Laisse passer |
|---|---|---|
| `login_required(url)` | `login_required_or_401()` | tout utilisateur connecté |
| `active_required(url)` | `active_required_or_403()` | connecté **et** `active` |
| `superuser_required(url)` | `superuser_required_or_403()` | `is_superuser && active` |
| `user_passes_test(url, pred)` | `user_passes_test_or_403(pred)` | prédicat sur le modèle `User` |
| `permission_required(url, codename)` | `permission_required_or_403(codename)` | détient le codename de permission |

Ce sont toutes des couches tower — `.layer(...)`-les sur un routeur ou un
sous-routeur.

---

## Verrous par prédicat et par rôle

`user_passes_test` exécute votre closure contre le modèle `User` résolu, de sorte
que vous pouvez verrouiller sur n'importe quel champ :

```rust
use rustango::auth_decorators::{user_passes_test, superuser_required_or_403};

// Staff-only sub-router (browser):
let staff = Router::new()
    .route("/admin/dashboard", get(dashboard))
    .layer(user_passes_test("/login", |u| u.is_superuser));

// Superuser-only JSON API → 401 anonymous / 403 non-superuser:
let api = Router::new()
    .route("/api/admin/stats", get(stats))
    .layer(superuser_required_or_403());
```

`superuser_required` / `active_required` sont des raccourcis figés pour les
prédicats courants `is_superuser && active` / `active`, afin que les sites
d'appel ne divergent pas silencieusement sur la question de savoir si les comptes
désactivés comptent encore.

---

## Verrous de permission

`permission_required` vérifie un codename de permission contre le moteur de
permissions du locataire (les superutilisateurs le contournent automatiquement).
Il résout en plus l'extracteur `Tenant`, si bien que les routes qui l'utilisent
doivent être montées sous le contexte du locataire :

```rust
use rustango::auth_decorators::permission_required;
use rustango::tenancy::permissions::ACCESS_ADMIN_CODENAME;

let admin = Router::new()
    .route("/admin", get(dashboard))
    .layer(permission_required("/login", ACCESS_ADMIN_CODENAME));
```

---

## L'aller-retour `?next=`

`login_required` préserve l'URL initialement demandée dans `?next=` afin que votre
handler de connexion puisse renvoyer l'utilisateur après authentification. **Vous
devez assainir cette valeur** — la réinjecter dans une redirection sans contrôle
est un trou de redirection ouverte (hameçonnage) classique. `safe_next` est le
garde-fou :

```rust
use rustango::auth_decorators::{extract_next, safe_next};

async fn login_post(Query(q): Query<HashMap<String, String>>, /* … */) -> Response {
    // … verify credentials, set the session …
    let dest = extract_next(&q)
        .and_then(|n| safe_next(&n))          // rejects open redirects
        .unwrap_or_else(|| "/".to_owned());
    Redirect::to(&dest).into_response()
}
```

`safe_next` n'accepte que les chemins de même origine, relatifs à la racine — il
rejette les URL absolues, les `//host` relatifs au schéma, les variantes à
barre oblique inverse, et leurs formes encodées en pourcentage :

```rust
assert_eq!(safe_next("/dashboard"),            Some("/dashboard".to_owned()));
assert_eq!(safe_next("https://evil.example/x"), None);
assert_eq!(safe_next("//evil.example/x"),       None);   // scheme-relative
assert_eq!(safe_next("%2F%2Fevil.example/x"),   None);   // decodes to //evil
```

---

## Remarques et limites

- **Ces verrous lisent la session.** « Connecté » signifie que l'extracteur
  [`SessionUser`](auth-sessions.md) a résolu un utilisateur depuis le cookie de
  session — ils sont donc destinés à l'authentification par session/cookie.
  L'authentification par jeton d'API ([JWT](auth-jwt-api.md), [clés
  d'API](auth-api-keys.md)) se verrouille plutôt au niveau de la [chaîne de
  backends](auth-backends.md), en lisant l'en-tête `Authorization`.
- **L'ordre des couches compte.** `.layer(gate)` protège chaque route ajoutée au
  routeur *avant* elle ; les routes ajoutées après sont publiques. Cantonner le
  verrou à un sous-routeur dédié (la forme du démarrage rapide) évite ce piège.
- **`permission_required` a besoin du contexte du locataire** (il interroge le
  moteur de permissions du locataire) — montez-le sous le locataire ; une route
  sans locataire renvoie une erreur 500.
- Le `?next=` de la redirection est toujours encodé en pourcentage, de sorte que
  le CRLF / fractionnement de réponse ne peut pas fuiter dans l'en-tête
  `Location`.


---

## Voir aussi

- [Backends d'authentification](auth-backends.md)
- [Sessions](auth-sessions.md)
- [Guide de sécurité](security.md)
