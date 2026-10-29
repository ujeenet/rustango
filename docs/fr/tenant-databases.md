# Bases de tenants sur un serveur enregistré

Un tenant en mode base de données peut apporter sa propre URL de base, ou
rustango peut créer la base pour lui. Cette page décrit la seconde voie de bout
en bout : enregistrer un serveur, le tester, y créer des tenants, faire tourner
leurs mots de passe, les purger.

rustango crée pour chaque tenant une base portant le nom du slug et un login
propre avec un mot de passe aléatoire, autorisé sur cette base seulement. Il
enregistre les deux dans `rustango_tenant_databases`. rustango ne supprime ou
ne change le mot de passe que d'une base enregistrée.

## Table des matières

- [Avant de commencer](#avant-de-commencer)
- [Droits](#droits)
- [1. Définir la clé secrète](#1-définir-la-clé-secrète)
- [2. Enregistrer le serveur](#2-enregistrer-le-serveur)
- [3. Le tester](#3-le-tester)
- [4. Créer un tenant](#4-créer-un-tenant)
- [5. Faire tourner le mot de passe d'un tenant](#5-faire-tourner-le-mot-de-passe-dun-tenant)
- [6. Purger un tenant](#6-purger-un-tenant)
- [La clé secrète : sauvegarde et rotation](#la-clé-secrète--sauvegarde-et-rotation)
- [Sauvegarde et restauration](#sauvegarde-et-restauration)
- [Notes PostgreSQL](#notes-postgresql)
- [Notes MySQL](#notes-mysql)
- [Développement local avec Docker](#développement-local-avec-docker)

## Avant de commencer

- Le serveur utilise le backend du registre. Un registre Postgres crée des
  tenants Postgres, et ainsi de suite.
- **PostgreSQL 15 ou plus récent.** Avant la 15, chaque login peut créer des
  tables dans le schéma `public` de chaque base, y compris celle du registre.
  `test-db-server` refuse les serveurs plus anciens.
- Le login d'administration a besoin de ces droits :

| Backend | Droits |
|---|---|
| PostgreSQL | `CREATEDB` et `CREATEROLE`, ou superutilisateur |
| MySQL / MariaDB | `CREATE`, `DROP`, `CREATE USER` globaux, et chaque privilège de base `WITH GRANT OPTION` |
| SQLite | un répertoire accessible en écriture ; les tenants sont `<dir>/<slug>.db` |

Les droits issus de rôles MySQL comptent quand le rôle est actif par défaut
(`SET DEFAULT ROLE`). Voir [Droits](#droits) pour la vue complète.

## Droits

Chaque façon de créer un tenant utilise un login différent et demande des
droits différents. **Test connection**, `test-db-server` et la création
elle-même vérifient la même liste, nomment ce qui manque et affichent les
instructions qui le corrigent. Une création est refusée avant que quoi que ce
soit ne soit créé.

| Option | Où vivent les données | Login utilisé | Login propre au tenant | Droits nécessaires |
|---|---|---|---|---|
| Base, nouvelle sur un serveur enregistré | une nouvelle base nommée comme le slug | le login d'administration du serveur, une fois | oui | créer des bases, créer des logins, accorder |
| Base, URL existante | la base de l'URL | le login de l'URL | celui de l'URL | aucun : rien n'est créé ; le login doit pouvoir créer des tables |
| Base sur le serveur du registre | une base existante sur ce serveur | celui du registre (`DATABASE_URL`), partagé | non, pas isolé | aucun : rien n'est créé |
| Schéma (PostgreSQL uniquement) | un schéma dans la base du registre | celui du registre (`DATABASE_URL`), partagé | non | `CREATE` sur la base du registre |

Pour donner ces droits à un login, exécutez en tant qu'administrateur. MySQL /
MariaDB, en tant que `root` :

```sql
GRANT CREATE, DROP, CREATE USER ON *.* TO 'rustango'@'%';
GRANT ALL PRIVILEGES ON *.* TO 'rustango'@'%' WITH GRANT OPTION;
```

PostgreSQL, en tant que superutilisateur :

```sql
ALTER ROLE rustango CREATEDB CREATEROLE;
-- schema mode: the registry's login
GRANT CREATE ON DATABASE app TO rustango;
```

Le login MySQL `rustango` du docker-compose généré n'a de droits que sur sa
propre base : enregistrez plutôt le serveur avec `root` comme login
d'administration. Quand le serveur ne dit pas ce qu'un login peut faire, la
vérification indique « unknown » et laisse la création essayer. Ou passez tout
cela : utilisez une URL de base existante.

## 1. Définir la clé secrète

Les mots de passe des serveurs et les nouvelles URL de tenant sont stockés
scellés (`enc:v1:`), avec une clé dérivée de `RUSTANGO_SECRET_KEY`. Utilisez 32
octets aléatoires ou plus :

```bash
export RUSTANGO_SECRET_KEY="$(openssl rand -base64 48)"
```

Sans clé, ou avec une clé vide, un mot de passe de serveur est refusé et une
URL de tenant est stockée non scellée avec un avertissement (refusée sous
`RUSTANGO_REQUIRE_SEALED_SECRETS=1`). Les pages serveurs de bases de données de
la console et sa page **New tenant** affichent une bannière tant que la clé
manque ou est trop courte.

## 2. Enregistrer le serveur

Console : **Organizations → Database servers**. CLI :

```bash
cargo run -- add-db-server main --host db.internal --user admin --tls require
```

Le mot de passe est demandé par une invite masquée et stocké scellé. TLS vaut
`require` par défaut. Voir [manage.md](manage.md) pour chaque option.

## 3. Le tester

```bash
cargo run -- test-db-server main
```

Il se connecte et indique la version du serveur et, une ligne chacun, si
l'administrateur peut créer des bases, créer des logins et accorder des droits.
Quand l'un manque, il le nomme et affiche les instructions qui le corrigent
(voir [Droits](#droits)). Sur la page **New tenant** de la console, **Test
connection** avec un slug lance aussi chaque vérification qu'une création
effectue avant tout DDL : slug libre, routage libre, et aucune base ni aucun
login de ce nom.

## 4. Créer un tenant

Console : **Organizations → New tenant**, mode base de données, **Create a new
database on a saved server**. CLI :

```bash
cargo run -- create-tenant acme --server main
```

L'exécution crée la base, le login et l'autorisation, se connecte avec le
nouveau login, enregistre le tenant, lance ses migrations et l'active. Un échec
après la création de la base supprime ce que l'exécution a créé. L'URL du
tenant contient le mot de passe du nouveau login et est stockée scellée.

Pour donner au tenant son premier administrateur, remplissez l'e-mail
d'administrateur dans le formulaire (nom d'utilisateur facultatif ; par défaut
l'e-mail), ou en CLI :

```bash
cargo run -- create-tenant acme --server main --admin-email admin@example.com
```

L'exécution crée un superutilisateur sans mot de passe utilisable, avant la
mise en service du tenant. L'administrateur définit son propre mot de passe
via un lien qui ne sert qu'une fois, pendant 72 heures, sur ce tenant
uniquement. Avec `Builder::first_admin_mailer`, il lui est envoyé par e-mail ;
sinon la console l'affiche **une fois** après la soumission, et la CLI
l'imprime. Copiez-le à ce moment-là. Lien perdu ou expiré :
`cargo run -- set-password-link acme admin@example.com`. Détails :
[Premier administrateur](operator-console.md#premier-administrateur).

## 5. Faire tourner le mot de passe d'un tenant

Console : la page du tenant, **Rotate database password**. CLI :

```bash
cargo run -- rotate-tenant-password acme
```

Les deux s'exécutent comme une exécution enregistrée avec ces étapes :

1. `check` : le tenant a un enregistrement, son URL nomme toujours cette base
   et ce login, l'URL est stockée en clair ou scellée (une référence `env://`
   ou vers un coffre est refusée : changez le mot de passe dans ce coffre), et
   l'administrateur peut modifier le login.
2. `alter_password` : un nouveau mot de passe aléatoire sur le serveur.
3. `store_password` : la nouvelle URL, scellée, n'est écrite que tant que l'URL
   stockée est toujours celle qui a été lue (compare-and-set). Si cela échoue,
   `revert` remet l'ancien mot de passe, mais seulement tant que l'URL stockée
   le contient encore ; sinon relancez `rotate-tenant-password`.
4. `invalidate_pools` : ce processus abandonne ses pools pour le tenant.
5. `discard_old_password` : voir la fenêtre ci-dessous.

**La fenêtre.** Les autres pods remarquent la nouvelle URL à leur prochain
sondage de l'empreinte de `rustango_orgs` (`RUSTANGO_HOST_GEN_CHECK_SECS`, 5 s
par défaut). D'ici là, leurs *nouvelles* connexions utilisent l'ancien mot de
passe. Les connexions ouvertes ne sont jamais coupées.

- Sur MySQL 8.0.14 et plus récent, l'ancien mot de passe reste valide avec
  `RETAIN CURRENT PASSWORD` pendant deux intervalles de sondage, puis est
  supprimé avec `DISCARD OLD PASSWORD`. Aucune connexion n'échoue. Le
  `CREATE USER` global de l'administrateur suffit pour les deux. Si le
  processus s'arrête pendant l'attente, l'ancien mot de passe reste valide
  jusqu'à la prochaine rotation ou purge.
- Sur PostgreSQL et MariaDB, il n'y a pas de double mot de passe : l'ancien
  cesse immédiatement, donc les autres pods peuvent échouer à ouvrir de
  nouvelles connexions pendant au plus un intervalle de sondage.

Un tenant SQLite n'a pas de mot de passe. Une base que rustango n'a pas créée
est refusée.

## 6. Purger un tenant

Console : la page du tenant, **Purge** (tapez le slug, cochez **drop the
database**). CLI :

```bash
cargo run -- purge-tenant acme --confirm acme --purge-database
```

Avec une console de provisioning, la purge est une exécution enregistrée,
suivie en direct sur sa page ; la CLI affiche les mêmes étapes. Chaque refus
vient en premier, dans `check`, donc une purge refusée ne change rien :

- l'URL du tenant ne nomme plus la base ou le login enregistrés, par exemple
  après `edit-tenant --database-url` ou `migrate-tenant-storage`. Rien n'est
  supprimé : remettez l'URL, ou supprimez cette base à la main et effacez sa
  ligne `rustango_tenant_databases` ;
- l'URL d'un autre tenant nomme la même base, ou ne peut pas être lue pour
  l'exclure (sur MySQL et SQLite, chacune est vérifiée par le serveur et le
  fichier réels, donc un alias compte), ou c'est celle du registre ;
- une autre purge ou un autre changement de mot de passe est en cours pour le
  tenant ;
- l'administrateur ne possède pas la base (Postgres : elle a été créée par un
  autre administrateur), ou ne peut pas supprimer le login (voir
  [Notes PostgreSQL](#notes-postgresql)) ; sur MySQL il lui faut `DROP` et
  `CREATE USER` globaux ;
- une session qui n'a pas été ouverte par les pools de ce tenant est ouverte
  dans la base (un opérateur, une sauvegarde).

Ensuite : `deactivate`, `purge_media`, `drop_database` (en terminant d'abord
les sessions propres à ce tenant sur chaque pod), `drop_login`, `remove_org`,
`delete_record`. Une nouvelle tentative après une étape échouée saute ce qui a
déjà disparu. SQLite supprime le fichier du tenant et ses fichiers `-wal`,
`-shm` et `-journal`.

Une base apportée par l'opérateur n'a pas d'enregistrement et garde l'ancien
comportement : sur Postgres, elle est supprimée via sa propre URL ; ailleurs,
elle est laissée en place et la purge le signale.

## La clé secrète : sauvegarde et rotation

**Sauvegardez `RUSTANGO_SECRET_KEY` avec la sauvegarde du registre.** Sans
elle, aucune URL de tenant ni aucun mot de passe de serveur scellé ne s'ouvre,
et chaque tenant d'un serveur enregistré est injoignable jusqu'à ce que chaque
URL soit saisie à nouveau.

Pour changer la clé :

1. Mettez l'ancienne clé dans `RUSTANGO_SECRET_KEY_PREVIOUS` et la nouvelle
   dans `RUSTANGO_SECRET_KEY`, sur chaque processus, puis redémarrez.
   L'ancienne clé ouvre encore ce qu'elle a scellé.
2. `cargo run -- reseal-tenant-secrets` liste ce qui changerait.
3. `cargo run -- reseal-tenant-secrets --apply` rescelle les URL de tenant, les
   mots de passe des serveurs, les secrets TOTP des opérateurs, les secrets
   client SSO et chaque colonne `Cast<EncryptedString>` des modèles (registre
   et tenants) avec la nouvelle clé.
4. Retirez `RUSTANGO_SECRET_KEY_PREVIOUS` et redémarrez.

La commande repère une colonne scellée par le type de champ que voit le derive
du modèle : `Cast<EncryptedString>` ou `Option<Cast<EncryptedString>>`, écrit en
entier. Le derive refuse toute autre écriture, comme un alias de type, donc
aucune colonne n'est oubliée. Chaque modèle doit avoir une clé primaire sur une
seule colonne ; un modèle sans elle est signalé en échec. Une valeur qu'aucune
clé n'ouvre est signalée et laissée telle quelle.

Les valeurs scellées sous une clé vide, avant que les clés vides ne soient
refusées, s'ouvrent encore quand `RUSTANGO_SECRET_KEY_PREVIOUS` est défini et
vide ; rescellez-les de la même façon.

## Sauvegarde et restauration

Un dump d'une base de tenant ne contient pas son login : les rôles Postgres et
les utilisateurs MySQL vivent en dehors de toute base. Pour restaurer un tenant
sur un autre serveur :

- Postgres : restaurez d'abord les rôles (`pg_dumpall --roles-only`), ou créez
  le login du tenant, restaurez la base, accordez-lui les droits et lancez
  `rotate-tenant-password` pour que l'URL stockée et le login concordent.
- MySQL : créez l'utilisateur et son autorisation, restaurez la base, puis
  `rotate-tenant-password`.

Restaurez le registre avec ses lignes `rustango_tenant_databases`, et pointez
le serveur enregistré vers le nouvel hôte avec `edit-db-server`.

## Notes PostgreSQL

- **Les nouvelles bases copient `template1`.** Son encodage, sa locale et tous
  les objets ou extensions qu'il contient atteignent chaque tenant. Gardez
  `template1` en `UTF8`.
- **Extensions.** Le login du tenant a tous les droits sur sa base mais n'est
  pas superutilisateur. Il peut faire `CREATE EXTENSION` pour les extensions
  *trusted* (`pgcrypto`, `citext`, `hstore`, `pg_trgm`, `uuid-ossp`,
  `unaccent`, `ltree` et les autres marquées trusted). Une extension non
  trusted, comme PostGIS, doit être installée par un superutilisateur : dans
  `template1` avant la création du tenant, ou dans la base du tenant après.
- **Changer de login d'administration (PostgreSQL 16+).** Un login
  `CREATEROLE` ne peut modifier ou supprimer que les rôles sur lesquels il
  détient `ADMIN OPTION`, et seul l'administrateur qui a créé un login de
  tenant le détient. Après avoir basculé un serveur vers un autre
  administrateur, la rotation et la purge des tenants plus anciens sont
  refusées avec le `GRANT … WITH ADMIN OPTION` à exécuter. Une purge exige
  aussi que l'administrateur possède la base (ou soit superutilisateur).
- **Terminer les sessions.** Les pools de tenant se connectent avec
  `application_name=rustango-tenant:<id>`. Une purge termine ces sessions, ce
  qui exige un superutilisateur, l'appartenance au rôle du tenant ou
  `pg_signal_backend` ; sans cela, la purge est refusée tant que de telles
  sessions sont ouvertes.

## Notes MySQL

- Le login du tenant est `'<login>'@'%'` : il peut se connecter depuis tout
  hôte que le réseau du serveur autorise. Limitez-le par le réseau ou par le
  `bind-address` du serveur ; les logins par hôte ne sont pas encore proposés.
- Les bases de tenant sont en `utf8mb4` avec la collation de la base du registre.
- Les doubles mots de passe demandent MySQL 8.0.14+ ; MariaDB fait la rotation
  sans eux.

## Développement local avec Docker

L'image `postgres` standard n'a pas de TLS : enregistrez-la donc avec
`--tls disable`. `mysql:8` sert du TLS avec un certificat auto-signé :
`--tls require` fonctionne, `verify-ca` demande ce certificat. Sur MySQL,
utilisez `root` : le login `rustango` du docker-compose généré n'a pas les
droits (voir [Droits](#droits)).

```bash
docker run -d --name pg -e POSTGRES_PASSWORD=pw -p 5432:5432 postgres:16
cargo run -- add-db-server local --host 127.0.0.1 --port 5432 --user postgres --tls disable

docker run -d --name my -e MYSQL_ROOT_PASSWORD=pw -p 3306:3306 mysql:8
cargo run -- add-db-server local --host 127.0.0.1 --port 3306 --user root --tls require
```

N'utilisez jamais `--tls disable` pour un serveur joint via un réseau que vous
ne contrôlez pas : le login d'administration peut supprimer chaque base qu'il
héberge.
