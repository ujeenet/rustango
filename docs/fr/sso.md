# SSO admin (OpenID Connect / connexion sociale)

Connectez-vous à l'admin rustango avec un fournisseur d'identité externe —
Google, Microsoft / Azure AD, GitHub, GitLab, Discord, ou **n'importe quel
fournisseur OpenID Connect** (Okta, Auth0, Keycloak, …) — au lieu d'un
mot de passe local. Activez-le avec la fonctionnalité cargo `admin-sso`.

Vous pouvez configurer **plusieurs providers**, chacun géré depuis
l'interface d'admin comme une ligne (pas de fichier de configuration, pas
de recompilation). Les points de terminaison d'un provider sont
découverts automatiquement à partir de son URL d'émetteur (issuer) OIDC
lors de la connexion ; les providers sociaux utilisent des préréglages
intégrés.

Le SSO fonctionne en **liaison à un compte existant** : l'email vérifié
renvoyé par l'IdP doit correspondre à un utilisateur admin existant. Le
SSO authentifie la personne ; il ne crée jamais de comptes et n'accorde
jamais d'accès par lui-même. Un email inconnu ou non vérifié est refusé.

```toml
[dependencies]
rustango = { version = "0.48", features = ["admin-sso"] }
```

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
