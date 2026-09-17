# Fichiers, téléversements et médias

Presque toutes les applications stockent des fichiers utilisateur — avatars, pièces jointes,
rapports exportés, images. **Rustango** vous offre un trait `Storage` avec des backends
interchangeables (disque local, stockage objet compatible S3, en mémoire pour les tests), un helper
de **téléversement** multipart sûr avec des garde-fous de taille/type, et — lorsque vous avez besoin
d'une médiathèque suivie — un `MediaManager` adossé à une base de données avec des URL présignées.
Écrivez votre code une seule fois contre le trait ; passez du disque local à S3 avec un changement
d'une ligne.

[![Les fichiers dans Rustango : un téléversement multipart est vérifié en taille et en extension puis écrit à travers le trait Storage ; le même trait alimente le disque local, S3 et en mémoire, et url() renvoie une adresse publique](../img/files.png)](../img/files.png)

> **Un terme vous est inconnu ?** *backend de stockage*, *multipart*, *stockage objet*,
> *URL présignée* — voir le [glossaire](glossary.md).

> **Source :** `rustango::storage` (`Storage`, `LocalStorage`, `InMemoryStorage`,
> `s3::S3Storage`, `BoxedStorage`), `rustango::uploads` (`save_uploads`,
> `UploadConfig`, `sanitize_filename`), et `rustango::media`
> (`Media`, `MediaManager`) — derrière les fonctionnalités `storage` / `uploads` / `storage-s3` /
> `media` (toutes activées par défaut).
>
> **Version exécutable :** les snippets Storage + garde-fous de téléversement sont copiés depuis
> [`files_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/files_doc.rs)
> (`cargo test -p rustango --test files_doc`) ; le flux multipart `save_uploads` de bout
> en bout est mis à l'épreuve par les tests intégrés dans
> `crates/rustango/src/uploads.rs`, et la médiathèque par
> [`media_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/media_sqlite_live.rs).

## Table des matières

- [Étape 1 — Choisir un backend de stockage](#étape-1--choisir-un-backend-de-stockage)
- [Étape 2 — Enregistrer, charger et servir des fichiers](#étape-2--enregistrer-charger-et-servir-des-fichiers)
- [Étape 3 — Accepter un téléversement](#étape-3--accepter-un-téléversement)
- [Noms de fichiers sûrs](#noms-de-fichiers-sûrs)
- [Production : stockage compatible S3](#production--stockage-compatible-s3)
- [La médiathèque](#la-médiathèque)
- [Référence](#référence)
- [Voir aussi](#voir-aussi)

---

## Étape 1 — Choisir un backend de stockage

Chaque backend implémente le même trait `Storage`, si bien que votre code ne nomme jamais le type
concret — il tient un **`BoxedStorage`** (`Arc<dyn Storage>`) :

```rust
use rustango::storage::{BoxedStorage, LocalStorage};
use std::path::PathBuf;
use std::sync::Arc;

let storage: BoxedStorage = Arc::new(LocalStorage::new(PathBuf::from("./uploads")));
```

| Backend | Fonctionnalité | À utiliser pour |
|---|---|---|
| `LocalStorage` | `storage` | déploiements mono-serveur — fichiers sur disque local |
| `S3Storage` | `storage-s3` | production — stockage objet S3 / R2 / B2 / MinIO |
| `InMemoryStorage` | `storage` | tests — un `HashMap`, ne touche jamais au disque |

---

## Étape 2 — Enregistrer, charger et servir des fichiers

Le trait tient en quatre méthodes async, indexées par un chemin sous forme de chaîne. `save` écrit
des octets, `load` les relit, plus `exists` / `delete` :

```rust
use rustango::storage::{Storage, InMemoryStorage};

let store = InMemoryStorage::new();
store.save("avatars/7.png", &png_bytes).await?;
assert!(store.exists("avatars/7.png").await?);
let bytes = store.load("avatars/7.png").await?;
store.delete("avatars/7.png").await?;
```

**Servir le fichier.** Attachez une URL de base (votre CDN ou hôte statique) et `url(key)`
construit l'adresse publique que vous stockez sur le modèle et remettez au navigateur :

```rust
let store = LocalStorage::new("./uploads".into())
    .with_base_url("https://cdn.example.com/uploads");

store.url("docs/report.pdf");   // Some("https://cdn.example.com/uploads/docs/report.pdf")
```

Sans URL de base, `url()` renvoie `None` — vous streameriez plutôt les octets à travers un
handler. `LocalStorage` protège aussi contre la traversée de chemin dans les clés.

---

## Étape 3 — Accepter un téléversement

`save_uploads` consomme un corps `Multipart` axum, valide chaque fichier contre un
`UploadConfig`, et écrit les survivants dans votre `Storage` — en streaming, si bien qu'un fichier
surdimensionné est rejeté en cours de transfert au lieu d'être mis en tampon en mémoire d'abord.

```rust
use rustango::uploads::{save_uploads, UploadConfig};
use axum::extract::Multipart;

async fn upload(mp: Multipart) -> Result<impl IntoResponse, UploadError> {
    let cfg = UploadConfig::new("avatars/")          // key prefix
        .max_bytes(2 * 1024 * 1024)                  // reject files over 2 MiB
        .allowed_extensions(&["png", "jpg", "jpeg", "webp"])
        .randomize_filename(true);                   // avoid collisions

    let saved = save_uploads(mp, &cfg, &storage).await?;   // Vec<SavedUpload>
    Ok(Json(saved))
}
```

Les garde-fous sont appliqués (et vérifiés) : `allowed_extensions` est **insensible à la casse**
(`"PNG"` et `"png"` sont identiques), et `max_bytes` interrompt le stream dès que la taille est
dépassée. Les tests `uploads` intégrés pilotent de vrais corps multipart et affirment que les
fichiers atterrissent dans le stockage, que les fichiers surdimensionnés sont rejetés, et que les
extensions non autorisées sont refusées.

```rust
let cfg = UploadConfig::new("avatars/").allowed_extensions(&["PNG", "Jpg"]);
assert!(cfg.allowed_extensions.contains("png"));   // normalized to lowercase
assert!(cfg.allowed_extensions.contains("jpg"));
```

---

## Noms de fichiers sûrs

Ne faites jamais confiance à un nom de fichier fourni par le client. `sanitize_filename` le réduit
à un basename sûr — en retirant les composantes de répertoire (traversée de chemin) et en
remplaçant les caractères dangereux :

```rust
use rustango::uploads::sanitize_filename;

sanitize_filename("../../etc/passwd");   // "passwd"   — no traversal
sanitize_filename("my photo!.png");      // "my_photo_.png"
sanitize_filename("");                    // "upload"   — never empty
```

`save_uploads` l'applique pour vous ; ne l'appelez directement que si vous construisez des clés à la
main.

---

## Production : stockage compatible S3

Pour les déploiements multi-serveurs, remplacez `LocalStorage` par `S3Storage` (derrière la
fonctionnalité `storage-s3`). Il parle l'API S3 avec un signeur SigV4 fait maison, si bien qu'il
fonctionne avec **AWS S3, Cloudflare R2, Backblaze B2 et MinIO**. Le trait est
identique — seul le constructeur change :

```rust
use rustango::storage::s3::S3Storage;   // needs the `storage-s3` feature

let storage: BoxedStorage = Arc::new(
    S3Storage::new(/* bucket, region, endpoint, credentials */)
);
// save / load / delete / url — exactly the same calls as LocalStorage
```

Vos handlers et vos modèles ne changent pas ; seul le câblage au démarrage change.

---

## La médiathèque

Lorsque les fichiers sont des enregistrements de première classe — suivis en base de données,
parcourables dans l'admin, avec des miniatures et une livraison CDN/présignée — tournez-vous vers
`rustango::media` plutôt que vers `Storage` brut. `MediaManager` persiste une ligne `Media` par
fichier et prend en charge deux flux de téléversement :

- **Côté serveur :** `manager.save_bytes(...)` stocke les octets et la ligne en un seul
  appel.
- **Direct vers le stockage :** `manager.begin_upload(...)` renvoie une URL de **PUT présigné**
  vers laquelle le navigateur téléverse directement (votre serveur ne relaie jamais les octets),
  puis vous confirmez la ligne.

```rust
use rustango::media::{Media, MediaManager};

let manager = MediaManager::new_pool(pool.clone(), registry);
// Hand the browser a short-lived download link:
// Renvoie Option<String> — None sur les backends qui ne savent pas signer (p. ex. disque local).
let Some(url) = manager.presigned_get(&media, Duration::from_secs(3600)).await else {
    return Err(/* pas d'URL signée pour ce backend */);
};
```

Il gère aussi la suppression douce et la purge des orphelins. Le flux complet est mis à l'épreuve
dans `media_sqlite_live.rs` ; les méthodes présignées/de téléversement direct du manager sont
orientées PostgreSQL.

### Le routeur REST exige une politique d'autorisation

`media::router` monte 16 routes JSON au-dessus du manager. **Toutes sont
protégées, et il n'existe aucun défaut permissif.** Construisez-le avec
`media_router_with` et fournissez une politique :

```rust
use rustango::media::router::{media_router_with, MediaPerms};

let app = axum::Router::new()
    .nest("/media", media_router_with(manager, MediaPerms::new(pool)));
```

`media_router(manager)` — l'ancien constructeur — est déprécié et répond
désormais `403` sur chaque route. C'est un changement de comportement
délibéré en 0.57.7 : auparavant ces routes ne prenaient aucun extracteur
d'authentification, d'autorisation ni de tenant, si bien que
`GET /media/{id}` renvoyait la ligne **et une URL de téléchargement S3
présignée** à quiconque savait deviner un entier, et `POST /uploads/begin`
émettait un `PUT` présigné pour un préfixe de clé choisi par l'appelant.

`MediaPerms` (nécessite la feature `tenancy`) vérifie les codenames de
permission `{table}.{action}` que l'admin utilise déjà —
`rustango_media.view` pour lire, `rustango_media_collections.add` pour créer
un dossier, etc. Montez-le **à l'intérieur** de `require_auth`, qui injecte
l'identité qu'il lit ; sans cela toute requête est un `401`. Les
superutilisateurs contournent la vérification.

Trois choses qu'il ne fait pas :

- **Les décisions au niveau de la ligne.** Un `rustango_media.view` lit
  *n'importe quelle* ligne de média par id. `MediaManager` détient un seul
  pool, un déploiement multi-tenant délimite donc les lignes lui-même —
  c'est le rôle de `MediaAuthorizer`. `MediaTarget` nomme la ligne
  (`Media(id)`, `Collection(id)`, `CollectionSubtree(id)`, …) exactement
  pour cela.
- **Délimiter le stockage d'objets.** `disk` est fourni par l'appelant sur
  `POST /uploads/begin` et le `StorageRegistry` est à l'échelle du processus :
  un pool par locataire isole la base de données et non le bucket, donc un
  simple `rustango_media.add` écrit dans n'importe quel disque que le processus
  connaît. Précisez lesquels avec
  `MediaPerms::new(pool).allow_disks(["user-uploads"])`. Les préfixes au sein
  d'un disque exigent toujours `MediaAuthorizer`, qui reçoit `key_prefix`.
- **Deviner ce que signifie une nouvelle route.** `MediaAction` et
  `MediaTarget` sont `#[non_exhaustive]` : terminez votre politique par
  `_ => false` et une route ajoutée plus tard arrivera refusée plutôt
  qu'autorisée.

À retenir si vous écrivez la vôtre : `DELETE /collections/{id}` supprime tout
le **sous-arbre** et réaffecte les médias à chaque niveau, il arrive donc
comme `Delete(CollectionSubtree(id))` et non `Delete(Collection(id))` — et
sous `MediaPerms` il exige `rustango_media.change` en plus de
`rustango_media_collections.delete`, parce qu'il écrit dans la table des
médias.
[UPGRADING.md](https://github.com/ujeenet/rustango/blob/main/UPGRADING.md)
contient les notes de migration.

### Comment les tables de médias sont créées

Les tables de médias (`rustango_media`, `rustango_media_collections`,
`rustango_media_tags`, `rustango_media_tag_links`) sont des modèles gérés. Leur
schéma est livré sous forme de **migration système** et est créé — par locataire, quand la
fonctionnalité `media` est activée — de la même façon que le reste des propres tables du framework,
chaque fois que vous exécutez `migrate` / provisionnez un locataire. Il n'y a pas d'étape paresseuse
de « création à la première utilisation » ; si la fonctionnalité est désactivée, les tables ne sont
jamais créées.

> **Mise à niveau depuis une version antérieure à 0.51.** Les versions plus anciennes créaient les
> tables de médias de façon paresseuse via un appel DDL `ensure_table` plutôt que via une migration.
> Au premier `migrate` (ou provisionnement de locataire) après la mise à niveau, le framework
> **réconcilie automatiquement** :
> parce que les tables existent déjà, la migration de médias générée est enregistrée dans
> le registre des migrations système *sans* réexécuter son `CREATE TABLE`, et vos
> lignes existantes sont laissées intactes. **Aucune étape manuelle n'est requise** — la mise à
> niveau qui échouerait autrement avec `relation already exists` / `table already exists`
> fonctionne désormais tout simplement. (Introduit en 0.51.1 ; voir le CHANGELOG.)

---

## Référence

**Trait `Storage` :** `save(key, &bytes)` · `load(key)` · `delete(key)` ·
`exists(key)` · `url(key) -> Option<String>`.

**`UploadConfig` :** `new(prefix)` · `.max_bytes(n)` · `.allowed_extensions(&[..])`
(insensible à la casse) · `.randomize_filename(bool)`. Utilisé par
`save_uploads(multipart, &cfg, &storage)`.

**Backends :** `LocalStorage` (disque) · `S3Storage` (stockage objet, `storage-s3`)
· `InMemoryStorage` (tests). Tous renvoient un `BoxedStorage`.

---

## Voir aussi

- [L'admin](admin.md) — les médias et les widgets de FK font apparaître les fichiers téléversés dans l'UI.
- [Tâches d'arrière-plan](jobs.md) — redimensionnez/transcodez un téléversement en dehors de la requête.
- [Mise en cache](caching.md) — le même motif de trait « change-le-backend ».
- [Guide de sécurité](security.md) — valider une entrée de téléversement non fiable.
