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

Le SSO connecte l'utilisateur **lié** à l'identité de l'IdP : une ligne
`rustango_sso_links` par `(provider, sub)`. L'email envoyé par l'IdP ne
suffit jamais à lui seul. Un utilisateur qui se connecte pour la première
fois n'est lié par son email vérifié que si le provider a
**`allow_email_link`** activé (désactivé par défaut), et jamais si le
compte est un superutilisateur ou du staff (tenant : détient une
permission quelconque ; bare admin : tout compte). Ces comptes sont liés
par un admin, qui ajoute la ligne `SsoLink` dans l'admin ; la ligne de
journal du refus indique le subject. (Le flux membre, ci-dessous,
provisionne automatiquement par défaut ; voir plus bas pour le désactiver.)

> **Source :** le cœur `rustango::sso`, indépendant de l'admin
> (`SsoProvider`, `build_provider`, `verified_email`, `ResolvedSso`,
> `SsoError`), la table de liens `rustango::sso::link` (`SsoLink`,
> `ProviderKey`, `sign_in`), le câblage bare-admin `rustango::admin::sso`, le SSO
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
rustango = { version = "0.60", features = ["admin-sso"] }
# SSO membre (utilisateur final) sans l'auto-admin :
rustango = { version = "0.60", features = ["tenancy", "sso"] }
```

`admin::sso_provider` et les anciens chemins du cœur `admin::sso::*` sont
désormais des **shims de ré-export** au-dessus de `sso::provider` /
`sso::*`, donc les imports existants
`crate::admin::sso::{build_provider, ResolvedSso, …}` et
`crate::admin::sso_provider::SsoProvider` continuent de résoudre
inchangés. Depuis la 0.58, `rustango_sso_providers` a une colonne
`allow_email_link` et il existe une table `rustango_sso_links` ;
`makemigrations` émet les deux.

L'email que compare la liaison optionnelle par email est la colonne `email`. Sur le
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
4. rustango vérifie le flux, échange le code et lit `/userinfo`.
5. Il cherche le lien pour ce provider et le `sub` de l'IdP. Sans lien, et
   si `allow_email_link` est activé, un email **vérifié** correspondant à
   un utilisateur non privilégié crée le lien.
6. Si l'utilisateur lié est actif, rustango émet la **même session à
   cookie signé** qu'une connexion par mot de passe — ainsi chaque
   garde-fou existant (superutilisateur / permissions, invalidation en
   direct au changement de mot de passe) s'applique toujours.
7. Sinon, l'utilisateur est renvoyé vers la page de connexion avec une
   erreur générique (les détails vont dans le journal serveur, jamais dans
   le navigateur).

La table de liens est un modèle migré normal : dans le stockage du tenant
pour les connexions tenant, et dans la base de l'admin pour le bare admin.
Un lien est comparé exactement (issuer et subject clés par un SHA-256 ;
l'email n'ignore que la casse ASCII), quelle que soit la collation de la base.

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
| `allow_email_link` | Lier un utilisateur qui se connecte pour la première fois par son email vérifié (désactivé par défaut). Ne lie jamais un compte superutilisateur ou staff ; ignoré par le bare admin. |

Seul un superutilisateur peut ajouter, modifier ou supprimer des lignes
`SsoProvider` et `SsoLink` dans l'admin ; le reste du staff peut seulement
les lister.

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
  depuis le panneau *Shared SSO* de la console, où *Allow email linking*
  bascule `allow_email_link` sur place (l'id et ses liens restent). Le
  drapeau s'applique à **tous** les tenants.

Sur la page de connexion d'un tenant, les deux ensembles fusionnent, et
en cas de collision de slug, c'est le **propre provider du tenant qui
l'emporte** sur le provider partagé — un tenant peut ainsi remplacer un
provider partagé pour lui-même.

L'URL de callback est dérivée par requête à partir de l'hôte + du slug
(`https://<host><login>/sso/<slug>/callback`), c'est donc celle-là qu'il
faut enregistrer auprès de l'IdP. Un utilisateur est lié par la liaison
optionnelle par email (utilisateurs tenant non privilégiés), ou par un
superutilisateur qui ajoute une ligne `SsoLink` : `provider_source`
(`tenant`, `shared` ou `admin`), `provider_id` (l'id de la ligne du
provider), `issuer` (`kind`, ou `kind|issuer_url` sans barre oblique
finale), `subject` et `user_id`. L'admin calcule `key_sha256`. La ligne de journal du refus (`sso refused`) porte
`provider_id`, `issuer` et `subject`. Ajouter une ligne exige
l'authentification par session de l'admin (`Builder::with_session_auth`,
ou `with_session` de l'admin tenant) ; sans elle, personne ne peut ajouter
de liens.

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

- **Provisionnement automatique.** Avec `auto_provision = true` (optionnel ; le
  défaut est `false`), un email IdP vérifié sans ligne `rustango_users`
  correspondante en **crée** une — nom d'utilisateur issu de la partie
  locale de l'email (dédupliqué en cas de collision), avec un hash de mot
  de passe aléatoire réel mais inutilisable (les utilisateurs SSO ne
  peuvent pas se connecter par mot de passe) — et la lie. Un email qui
  correspond à un compte existant suit la règle `allow_email_link`
  ci-dessus. Mettez-le à `false` pour refuser les emails inconnus. Une
  connexion native appelle directement `find_or_provision_member` ; son
  résultat `MemberSignIn` distingue `NotLinked` (un compte a cet email
  mais ne peut pas être lié par lui) de `NoAccount`.
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
trouver les points de terminaison (une fois par émetteur et par heure). (Sign in with Apple n'est pas un
préréglage ; il nécessite une vérification id_token/JWKS.)

## Notes de sécurité

- **Le lien d'abord** — le lien `(provider, sub)` décide ; l'email ne sert
  qu'à la liaison optionnelle par email, et seulement s'il est vérifié.
- **Pas de liens privilégiés par email** — seuls les superutilisateurs lient
  les superutilisateurs et le staff. Un lien créé par email continue de
  fonctionner après la promotion de l'utilisateur ; supprimez-le si ce
  n'est pas voulu.
- **Pas de provisionnement automatique** pour les admins — un email inconnu
  ne peut pas entrer.
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
