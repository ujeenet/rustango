# SSO (OpenID Connect / connexion sociale)

Connectez-vous avec un fournisseur d'identité externe — Google,
Microsoft / Azure AD, GitHub, GitLab, Discord, ou **n'importe quel
fournisseur OpenID Connect** (Okta, Auth0, Keycloak, …) — au lieu d'un
mot de passe local.

Vous pouvez configurer **plusieurs providers**, chacun géré depuis
l'interface d'admin comme une ligne (pas de fichier de configuration, pas
de recompilation). Les points de terminaison d'un provider sont
découverts automatiquement à partir de son URL d'émetteur (issuer) OIDC
lors de la connexion ; les providers sociaux utilisent des préréglages
intégrés.

Pour l'admin, le SSO fonctionne en **liaison à un compte existant** :
l'email vérifié renvoyé par l'IdP doit correspondre à un utilisateur
admin existant. Il authentifie la personne ; il ne crée jamais de comptes
et n'accorde jamais d'accès par lui-même. Un email inconnu ou non vérifié
est refusé. (Le flux membre, ci-dessous, peut opter pour le
provisionnement automatique.)

> **Source :** le cœur `rustango::sso`, indépendant de l'admin
> (`SsoProvider`, `build_provider`, `verified_email`, `ResolvedSso`,
> `SsoError`), le câblage bare-admin `rustango::admin::sso`, le SSO
> par tenant / console `rustango::tenancy::sso`
> (`SharedSsoProvider`), et le SSO membre
> `rustango::tenancy::member_auth`.

## Fonctionnalités et qui les utilise

Depuis la **0.49**, le cœur SSO est sa propre fonctionnalité, indépendante
de l'auto-admin, si bien qu'une connexion d'utilisateur final (membre)
peut se compiler sans tirer `crate::admin` :

| Fonctionnalité | Tire | Vous donne |
|---|---|---|
| `sso` | `oauth2`, `casts` | Le cœur indépendant de l'admin : `rustango::sso` — la poignée de main OIDC / OAuth social, le modèle `SsoProvider` adossé à la base (secret chiffré au repos via `casts`), et le flux membre (`tenancy::member_auth`, avec `tenancy`). |
| `admin-sso` | `admin`, `sso` | Ce qui précède **plus** le câblage de connexion bare-admin (`rustango::admin::sso`) — les boutons SSO sur la page de connexion de l'admin, qui émettent la session admin. |

```toml
[dependencies]
# Connexion admin avec SSO :
rustango = { version = "0.52", features = ["admin-sso"] }
# SSO membre (utilisateur final) sans l'auto-admin :
rustango = { version = "0.52", features = ["tenancy", "sso"] }
```

`admin::sso_provider` et les anciens chemins du cœur `admin::sso::*` sont
désormais des **shims de ré-export** au-dessus de `sso::provider` /
`sso::*`, donc les imports existants
`crate::admin::sso::{build_provider, ResolvedSso, …}` et
`crate::admin::sso_provider::SsoProvider` continuent de résoudre
inchangés (le nom de table `rustango_sso_providers` et tous ses champs
sont intacts — les migrations ne sont pas affectées).

L'email sur lequel un utilisateur est lié est la colonne `email`. Sur le
modèle `User` du tenant, elle est conditionnée à la fonctionnalité
**`sso`** (déplacée hors de `admin-sso` en 0.49, pour que les builds
membre-SSO-seul obtiennent quand même la colonne) ; le `AdminUser.email`
nu reste derrière `admin-sso`. Activer ou désactiver la fonctionnalité
émet une migration `AddColumn` / `DropColumn` pour cette colonne.

## Comment ça fonctionne

1. La page de connexion affiche un bouton **« Sign in with &lt;provider&gt; »**
   par provider activé.
2. Cliquer sur l'un d'eux (`GET <login>/sso/<slug>`) redirige vers l'IdP
   avec un cookie de flux signé et à courte durée de vie (PKCE + `state`
   CSRF).
3. L'IdP renvoie l'utilisateur vers `<login>/sso/<slug>/callback`.
4. rustango vérifie le flux, échange le code, lit `/userinfo`, et exige
   **`email_verified`**.
5. Il recherche un utilisateur admin par cet email. S'il en existe un et
   qu'il est actif, rustango émet la **même session à cookie signé**
   qu'une connexion par mot de passe produit, liée à cet utilisateur —
   ainsi chaque garde-fou existant (superutilisateur / permissions,
   invalidation en direct au changement de mot de passe) s'applique
   toujours.
6. Aucune correspondance → l'utilisateur est renvoyé vers la page de
   connexion avec une erreur générique (les détails vont dans le journal
   serveur, jamais dans le navigateur).

Le secret client est **chiffré au repos** — la colonne `client_secret`
est un cast [`EncryptedString`](#stockage-des-secrets), déchiffré en
mémoire uniquement au moment de la connexion.

## Les providers sont des lignes, gérées dans l'admin

Chaque provider est une ligne `SsoProvider`. Il apparaît comme un modèle
admin ordinaire — ajout/modification/activation depuis l'interface
d'admin, sans redéploiement. Champs :

| Champ | Signification |
|---|---|
| `slug` | Clé de route stable + id du bouton (`<login>/sso/<slug>`). Unique. |
| `label` | Texte du bouton, ex. « Sign in with Google ». |
| `kind` | Un préréglage — `google` / `microsoft` / `github` / `gitlab` / `discord` — ou `oidc` pour un provider OpenID Connect générique. |
| `issuer_url` | URL de base de découverte OIDC (pour `kind = "oidc"`) ; rustango récupère `{issuer}/.well-known/openid-configuration`. Inutilisé pour les préréglages. |
| `client_id` | L'identifiant client OAuth fourni par l'IdP. |
| `client_secret` | Le secret client OAuth, **chiffré au repos** (jamais en clair dans la base de données). |
| `enabled` | Indique si le bouton s'affiche sur la page de connexion. |
| `sort_order` | Ordre d'affichage des boutons (croissant). |
| `scopes` | Substitution optionnelle des scopes, séparés par des espaces (par défaut `openid email profile`). |

Pour ajouter un provider : saisissez le `client_id` + `client_secret`,
choisissez un `kind` (ou `oidc` + une `issuer_url`), et enregistrez. Les
points de terminaison sont découverts au moment de la connexion — aucun
câblage de points de terminaison par provider n'est nécessaire.

## Où chaque surface gère les providers

- **Admin autonome / mono-tenant** (`crate::admin`) : les lignes
  `SsoProvider` forment une table globale simple, gérée depuis l'admin
  nu. Nécessite `Builder::with_session_auth` (le SSO émet la même
  session).
- **Admin tenant** (multi-tenancy) : chaque tenant gère ses **propres**
  lignes `SsoProvider` depuis son admin — granulaire, en libre-service,
  isolé par tenant.
- **Console opérateur** (multi-tenancy) : un opérateur définit un
  **`SharedSsoProvider`** une seule fois, et il est proposé à **tous**
  les tenants (un Google à l'échelle de l'entreprise, par exemple). Géré
  depuis le panneau *Shared SSO* de la console.

Sur la page de connexion d'un tenant, les deux ensembles fusionnent, et
en cas de collision de slug, c'est le **propre provider du tenant qui
l'emporte** sur le provider partagé — un tenant peut ainsi remplacer un
provider partagé pour lui-même.

L'URL de callback est dérivée par requête à partir de l'hôte + du slug
(`https://<host><login>/sso/<slug>/callback`), c'est donc celle-là qu'il
faut enregistrer auprès de l'IdP. Liez un utilisateur en définissant la
colonne `email` sur sa ligne `rustango_users` (tenant) /
`rustango_admin_users` (nu) à l'adresse renvoyée par l'IdP.

## SSO membre (utilisateur final)

Les surfaces ci-dessus connectent des personnes à un **admin**.
`tenancy::member_auth` en est l'équivalent côté membre : il connecte un
utilisateur final au pool d'utilisateurs propre au tenant
(`rustango_users`) et émet une **session membre**, pour qu'un adhérent de
salle de sport / client SaaS puisse « Se connecter avec Google » sans
toucher à l'admin. Il réutilise exactement le même cœur `rustango::sso`
et les lignes `SsoProvider` du tenant — seule la session émise diffère,
d'où le fait qu'il vive derrière la fonctionnalité `sso` (et non
`admin-sso`) et n'ait besoin d'aucun auto-admin.

Montez `member_sso_router` dans une pile `tenancy::server::Builder` (il
lit l'`Arc<TenantContext>` résolu que le builder injecte) :

```rust
use rustango::tenancy::member_auth::{member_sso_router, MemberAuthConfig};

let members = member_sso_router(MemberAuthConfig {
    login_base:     "/auth".into(),   // buttons link to /auth/sso/<slug>
    landing_url:    "/".into(),       // post-login destination (honors a same-origin ?next)
    auto_provision: true,             // create a user from a verified email on first sign-in
    session_ttl:    7 * 24 * 60 * 60, // 7 days
    ..Default::default()
});
```

Il monte deux routes par slug sous `login_base` :

- `GET {login_base}/sso/{slug}` — démarrer la poignée de main, rediriger
  vers l'IdP.
- `GET {login_base}/sso/{slug}/callback` — la terminer, trouver ou
  provisionner le membre, émettre le cookie de session.

Différences avec le flux admin :

- **Provisionnement automatique.** Avec `auto_provision = true` (le
  défaut), un email IdP vérifié sans ligne `rustango_users`
  correspondante en **crée** une — nom d'utilisateur issu de la partie
  locale de l'email (dédupliqué en cas de collision), avec un hash de mot
  de passe aléatoire réel mais inutilisable (les utilisateurs SSO ne
  peuvent pas se connecter par mot de passe). Mettez-le à `false` pour
  une liaison à un compte existant à la manière de l'admin (email inconnu
  refusé).
- **Son propre cookie de session.** Le cookie membre
  (`rustango_member_session`) est **séparé par domaine** des cookies de
  session tenant / admin : le message signé porte une étiquette
  par domaine et une revendication d'audience, si bien qu'un cookie
  membre ne peut jamais valider comme cookie tenant/admin (ni
  l'inverse), même si les deux sont signés avec
  `RUSTANGO_SESSION_SECRET`. Il est lié au slug (un cookie émis pour
  `acme` n'authentifie jamais sur `globex`) et invalidé par une rotation
  de mot de passe (parité avec la session admin).

Lisez le membre courant dans un handler avec l'extracteur
**`CurrentMember`** — l'équivalent membre de `SessionUser`. Il est
infaillible (`None` pour les sessions anonymes / expirées / invalidées
par rotation / inter-tenants), donc il se compose avec les routes
publiques :

```rust
use rustango::tenancy::member_auth::CurrentMember;

async fn dashboard(CurrentMember(member): CurrentMember) -> impl axum::response::IntoResponse {
    match member {
        Some(user) => format!("Hi, {}", user.username),
        None => "Please sign in".to_owned(),
    }
}
```

> **Périmètre v1.** Le SSO membre résout les providers uniquement depuis
> les lignes `SsoProvider` du tenant — la fusion avec le
> `SharedSsoProvider` à l'échelle du registry et un hook `provision`
> personnalisé sont des suites.

## Stockage des secrets

`client_secret` est stocké **chiffré au repos** avec XChaCha20-Poly1305
(AEAD), la clé étant dérivée de la variable d'environnement
**`RUSTANGO_SECRET_KEY`**. Il n'est déchiffré en mémoire qu'au moment de
la connexion, pour s'authentifier auprès du point de terminaison de
jetons de l'IdP. Ainsi, un dump de base de données divulgué n'expose
jamais le secret, et chaque tenant conserve son propre secret sans
variable d'environnement par provider.

> Définissez `RUSTANGO_SECRET_KEY` dans le déploiement (n'importe quelle
> longueur ; elle est hachée en SHA-256 vers une clé de 32 octets). Sans
> elle, enregistrer ou utiliser un provider échoue immédiatement — la
> même posture qu'une URL de base de données manquante.

## Providers (préréglages)

Préréglages intégrés : `google`, `microsoft` (Azure AD), `github`,
`gitlab`, `discord`. Pour tout le reste, utilisez `kind = "oidc"` avec
une `issuer_url` — rustango exécute la découverte OpenID Connect pour
trouver les points de terminaison. (Sign in with Apple n'est pas un
préréglage ; il nécessite une vérification id_token/JWKS.)

## Notes de sécurité

- **Email vérifié uniquement** — les emails non vérifiés de l'IdP sont
  rejetés.
- **Pas de provisionnement automatique** — un email inconnu ne peut pas
  entrer ; créez d'abord l'utilisateur admin (et définissez son
  `email`).
- **Secrets chiffrés au repos** (`RUSTANGO_SECRET_KEY`), déchiffrés
  uniquement en mémoire au moment de la connexion ; les formulaires
  d'édition masquent le secret stocké.
- Le cookie de flux a une courte durée de vie (10 min), `HttpOnly`,
  `SameSite=Lax`, et `Secure` en HTTPS ; la poignée de main transporte
  PKCE + un `state` signé.
- Les sessions SSO sont la session admin ordinaire — faire tourner ou
  désactiver l'utilisateur lié les invalide via le garde-fou en direct
  existant.
- Le modèle de confiance repose sur `/userinfo` via TLS (l'id_token n'est
  pas vérifié indépendamment) ; placez l'admin derrière HTTPS.

## Voir aussi

- [Guide de sécurité](security.md) · [Authentification](auth-flows.md)
