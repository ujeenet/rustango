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
peut aussi créer un compte pour un email inconnu ; c'est désactivé par défaut.)

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
rustango = { version = "0.61", features = ["admin-sso"] }
# SSO membre (utilisateur final) sans l'auto-admin :
rustango = { version = "0.61", features = ["tenancy", "sso"] }
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
  depuis le panneau *Shared SSO* de la console (`/sso-shared`, fonctionnalité
  `admin-sso` ; tout opérateur voit la liste, les changements exigent une
  console modifiable), où *Allow email linking*
  bascule `allow_email_link` sur place (l'id et ses liens restent). Le
  drapeau s'applique à **tous** les tenants. *Edit*
  (`/sso-shared/{id}/edit`) modifie un provider sur place (même id et slug ;
  un secret vide garde celui enregistré), et chaque ligne affiche l'URL de
  callback à enregistrer auprès de l'IdP. Le champ d'URL de l'issuer
  n'apparaît que pour `oidc`. Un ajout ou une modification refusé réaffiche le
  formulaire (statut 400) avec les valeurs saisies et l'erreur, jamais le
  secret.

Sur la page de connexion d'un tenant, les deux ensembles fusionnent, et
en cas de collision de slug, c'est le **propre provider du tenant qui
l'emporte** sur le provider partagé — un tenant peut ainsi remplacer un
provider partagé pour lui-même.

L'URL de callback est dérivée par requête à partir de l'hôte + du slug
(`https://<host><login>/sso/<slug>/callback`), c'est donc celle-là qu'il
faut enregistrer auprès de l'IdP. Le schéma est `https`, sauf si un proxy de
confiance envoie `X-Forwarded-Proto`. Un utilisateur est lié par la liaison
optionnelle par email (utilisateurs tenant non privilégiés), ou par un
superutilisateur qui ajoute une ligne `SsoLink` : `provider_source`
(`tenant`, `shared`, `admin`, ou `app` pour une connexion native), `provider_id` (l'id de la ligne du
provider), `issuer` (`kind`, ou `kind|issuer_url` sans barre oblique
finale), `subject` et `user_id`. L'admin calcule `key_sha256`. La ligne de journal du refus (`sso refused`) porte
`provider_id`, `issuer` et `subject`. Ajouter une ligne exige
l'authentification par session de l'admin (`Builder::with_session_auth`,
ou `with_session` de l'admin tenant) ; sans elle, personne ne peut ajouter
de liens.

Un lien créé plus d'une heure avant le compte vers lequel il pointe (un id
réutilisé après une restauration, par exemple) est supprimé, et la connexion
continue comme sans lien. Un lien dont l'utilisateur n'existe plus est supprimé
à la connexion suivante, et supprimer un utilisateur dans l'admin supprime ses
liens. Si les valeurs `created_at` des comptes sont importées ou remplies après
leurs liens, `rustango::sso::link::set_stale_links(StaleLinks::Warn)` se
contente de journaliser un avertissement et connecte quand même.

Avec la fonctionnalité `totp`, un utilisateur du bare admin qui a un appareil
TOTP confirmé doit encore saisir un code après le SSO. L'admin affiche le
formulaire de code et garde la connexion en attente 5 minutes dans un cookie
signé ; le formulaire poste vers `<login>/sso-totp`. L'admin tenant n'a pas
d'étape TOTP.

`manage check --deploy` avertit pour un provider activé qui refuserait tout
utilisateur existant : aucune ligne `SsoLink`, et liaison par email désactivée
ou seulement des utilisateurs privilégiés. Dans le bare admin, c'est tout
provider sans lien. Il avertit aussi pour la liaison par email sur un provider
`kind = "oidc"`, un issuer `http://` hors loopback, des règles de domaine email
avec `require_verified_email` désactivé, et un provider partagé qui inscrit des
utilisateurs.

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

Montez `member_sso_router` dans une pile `rustango::server::Builder` (il
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

Avec plusieurs backends compilés, `member_sso_router` sert le type de tenant
par défaut (Postgres) ; utilisez `member_sso_router_for::<sqlx::Sqlite>` (ou
`MySql`).

Il monte deux routes par slug sous `login_base` :

- `GET {login_base}/sso/{slug}` — démarrer la poignée de main, rediriger
  vers l'IdP.
- `GET {login_base}/sso/{slug}/callback` — la terminer, trouver ou
  provisionner le membre, émettre le cookie de session.

Un tenant à préfixe de chemin reçoit les deux mêmes routes sous son préfixe.

Différences avec le flux admin :

- **Schéma du callback.** Le `redirect_uri` membre prend son schéma dans le
  `X-Forwarded-Proto` d'un proxy de confiance (et son hôte dans
  `X-Forwarded-Host`), sinon `http` sur un hôte de
  loopback (`localhost`, `*.localhost`, une IP de loopback) et `https` partout
  ailleurs, donc `localhost.example.com` obtient `https`.
- **Provisionnement automatique.** Avec `auto_provision = true` (optionnel ; le
  défaut est `false`), un email IdP vérifié sans ligne `rustango_users`
  correspondante en **crée** une — nom d'utilisateur issu de la partie
  locale de l'email (dédupliqué en cas de collision), avec un hash de mot
  de passe aléatoire réel mais inutilisable (les utilisateurs SSO ne
  peuvent pas se connecter par mot de passe) — et la lie. Un email qui
  correspond à un compte existant suit la règle `allow_email_link`
  ci-dessus. Mettez-le à `false` pour refuser les emails inconnus. Une
  connexion native (une app qui a vérifié elle-même son ID token) appelle
  directement `find_or_provision_member`, avec `ProviderKey::app(issuer)`
  comme clé. Son résultat `MemberSignIn` vaut `Member(id)`, `NotLinked` (un
  compte a cet email mais ne peut pas être lié par lui), `NoAccount`,
  `Inactive`, `Unverified` ou `Refused`.
- **Son propre cookie de session.** Le cookie membre
  (`rustango_member_session`) est **séparé par domaine** des cookies de
  session tenant / admin : le message signé porte une étiquette
  par domaine et une revendication d'audience, si bien qu'un cookie
  membre ne peut jamais valider comme cookie tenant/admin (ni
  l'inverse), même si les deux sont signés avec
  `RUSTANGO_SESSION_SECRET`. Il est lié au slug (un cookie émis pour
  `acme` n'authentifie jamais sur `globex`) et invalidé par une rotation
  de mot de passe (parité avec la session admin).
- **Déconnexion.** `member_auth::logout(&pool, &user)` termine les sessions
  du membre sur tous les appareils et renvoie la valeur `Set-Cookie` qui
  efface le cookie. Pour un tenant à préfixe de chemin, utilisez
  `logout_at`, qui efface le cookie sur le chemin du préfixe.

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

## Qui peut se connecter

Chaque provider (et chaque provider partagé de la console opérateur) a des
règles de connexion. Des règles vides admettent toute personne que l'IdP
authentifie.

| Champ | Règle |
|---|---|
| `allowed_email_domains` | Seulement ces domaines : `client.org` ne correspond qu'à lui-même, `*.client.org` qu'à ses sous-domaines. Séparés par des virgules ou des retours à la ligne. |
| `blocked_email_domains` | Jamais ces domaines, même autorisés ci-dessus. Vérifié sur tout email, vérifié ou non. |
| `allowed_idp_tenants` | Identifiants d'annuaire Microsoft (`tid`). Lus uniquement depuis un id_token vérifié. |
| `allowed_hosted_domains` | Domaines Google Workspace (claim `hd`). |
| `required_claims` | JSON `{"claim": ["valeur", ...]}` : le claim (texte ou liste) doit contenir une des valeurs. `{"iss": [...]}` fixe l'émetteur. Un claim absent refuse. |
| `require_verified_email` | Activé par défaut : la liste autorisée n'accepte qu'un email vérifié par l'IdP. |

L'admin et la console opérateur enregistrent les listes sous une forme
canonique et refusent une valeur illisible. Une valeur invalide écrite par
un autre chemin refuse toute connexion via ce provider.

Une connexion refusée va dans le log serveur et dans le journal d'audit
(`source = sso:<surface>`, `changes.rule`, p. ex. `domain_not_allowed`).
Le navigateur voit un message générique ; la page de connexion du tenant
affiche aussi le `access_denied_message` du tenant (de `SsoTenantPolicy`), échappé.

La route membre (`member_sso_router`) ne résout que les providers propres
au tenant ; les providers partagés, et leur restriction par le tenant, ne
s'y appliquent pas encore.

### Confiance dans l'id_token

Les données de profil viennent de `/userinfo` via TLS. Les claims que seul
l'id_token porte (`tid`, `iss`, `xms_edov`) ne servent que si l'id_token
passe : signature contre le JWKS de l'émetteur (RS256 ou ES256 ; mis en
cache selon son `max-age`, rechargé au plus une fois par minute pour une clé
inconnue), `aud` contient le client id, `exp`/`nbf` avec une minute de
tolérance, l'émetteur (Microsoft : `https://login.microsoftonline.com/{tid}/v2.0`)
et le même `sub` que userinfo. Sinon ces claims sont absents, et une règle
qui en a besoin refuse. Un email Microsoft compte comme vérifié si
`xms_edov` est vrai, ou si le provider fixe `allowed_idp_tenants` et que le
`tid` du token y figure.

## Qui peut s'inscrire

Un provider peut aussi créer le compte à la première connexion. Cela
n'arrive que si aucun lien ni aucun compte ne correspond à l'e-mail vérifié,
et seulement après les règles de connexion ci-dessus.

| Champ | Sens |
|---|---|
| `signup_mode` | `closed` (liens seulement), `invite_only` (une `SsoInvite` pour cet e-mail), `allowlist` (une invitation pour l'e-mail ou son domaine) ou `open` (toute personne admise par les règles). Vide : fermé sur la connexion tenant ; le flux membre suit `MemberAuthConfig::auto_provision`. L'admin nu ne crée jamais de compte. |
| `default_roles` | Rôles du tenant donnés au nouvel utilisateur, par ex. `Viewer`. Une invitation avec des rôles les remplace. |
| `role_map` | JSON `{"<groupe ou rôle d'app>": "Editor"}` (ou une liste). Lu dans les claims de `role_claims` (vide : `groups,roles`) ; pour un provider avec JWKS, seulement depuis un id_token vérifié. |
| `sync_roles_on_login` | Réappliquer `role_map` à chaque connexion. Seuls les rôles nommés dans `role_map` sont ajoutés ou retirés ; les rôles donnés à la main restent. |
| `username_from` | `email_local` (défaut), `email` ou `preferred_username` (sinon la partie locale de l'e-mail). |
| `username_clash` | `suffix` (défaut : `ann-x7k2q9`) ou `refuse`. |
| `sync_profile` | Reprendre le nom affiché et l'e-mail de l'IdP à chaque connexion (jamais pour un superuser ; un e-mail déjà pris est ignoré). |
| `allow_email_link_staff` | Avec `allow_email_link`, lier aussi un utilisateur qui a des rôles. Un superuser n'est jamais lié par e-mail. |

Un nouvel utilisateur n'est jamais superuser. Les rôles passent par le
moteur de permissions (`UserRole`) ; un nom de rôle sans ligne `Role` est
ignoré et noté dans le journal d'audit (`sso_role_unknown`). L'utilisateur,
son lien (`provisioned = true`), ses rôles et l'usage de l'invitation sont
écrits dans une seule transaction.

**Invitations** (`SsoInvite`, réservées aux superusers dans l'admin du
tenant) : un e-mail (`ann@client.org`) ou un domaine (`@client.org`, ou
`*.client.org` pour ses sous-domaines). Elles peuvent viser un provider,
porter des rôles, expirer (`expires_at`) et limiter les usages (`max_uses`).
Un usage est compté de façon atomique : deux inscriptions qui se disputent
le dernier usage créent un seul compte. `.../rustango_sso_invites/bulk`
en ajoute plusieurs d'un coup, depuis une liste collée ou une colonne CSV.

**Délier et relier.** Sur les lignes `SsoLink`, les actions *Unlink* et
*Re-link on next sign-in* (superusers seulement). Re-link supprime le lien
et laisse une invitation à usage unique, valable 7 jours, qui relie le même
utilisateur à sa prochaine connexion par le même provider avec son e-mail
vérifié, même avec un nouveau subject. Un superuser n'est jamais relié ; un
détenteur de rôles seulement avec `allow_email_link_staff`.

`check --deploy` ne signale pas comme bloquant un provider dont
l'inscription n'est pas fermée, et avertit pour `signup_mode = open` sans
`allowed_email_domains`, `allowed_hosted_domains` ni `allowed_idp_tenants`.

## Stockage des secrets

`client_secret` est stocké **chiffré au repos** avec XChaCha20-Poly1305
(AEAD), la clé étant dérivée de la variable d'environnement
**`RUSTANGO_SECRET_KEY`**. Il n'est déchiffré en mémoire qu'au moment de
la connexion, pour s'authentifier auprès du point de terminaison de
jetons de l'IdP. Ainsi, un dump de base de données divulgué n'expose
jamais le secret, et chaque tenant conserve son propre secret sans
variable d'environnement par provider.

> Définissez `RUSTANGO_SECRET_KEY` dans le déploiement. Elle est hachée en
> SHA-256 vers une clé de 32 octets ; utilisez au moins 32 caractères
> aléatoires (avec le provisionnement des tenants monté, `check --deploy`
> avertit pour une clé plus courte). Sans elle, enregistrer ou utiliser un provider échoue
> immédiatement — la même posture qu'une URL de base de données manquante.

Pour faire tourner la clé, mettez la nouvelle valeur dans
`RUSTANGO_SECRET_KEY` et l'ancienne dans `RUSTANGO_SECRET_KEY_PREVIOUS`. La
lecture retombe sur l'ancienne clé. Dans un déploiement multi-tenant,
`manage reseal-tenant-secrets --apply` rescelle ensuite les secrets SSO
partagés et des tenants avec la nouvelle clé.

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
- Le modèle de confiance repose sur `/userinfo` via TLS ; les claims de
  l'id_token ne comptent qu'après la vérification JWKS (voir
  [confiance dans l'id_token](#confiance-dans-lid_token)). Placez l'admin derrière HTTPS.

## Voir aussi

- [Guide de sécurité](security.md) · [Authentification](auth-flows.md)
