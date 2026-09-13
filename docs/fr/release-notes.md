# Notes de version

Chaque changement visible par l'utilisateur est livré avec une entrée de changelog. Cette page en est le point d'accès, car jusqu'ici le site de documentation n'en avait aucun — un lecteur n'y voyait que des titres étiquetés d'une version à l'intérieur des guides et rien d'autre, ce qui a fait qu'un titre figé sur 0.42 s'est lu comme « la documentation s'est arrêtée en 0.42 » ([#1304](https://github.com/ujeenet/rustango/issues/1304)).

## Où vivent les notes

| Quoi | Où | Idéal pour |
|---|---|---|
| Historique complet, chaque version | [`CHANGELOG.md`](https://github.com/ujeenet/rustango/blob/main/CHANGELOG.md) | « quand cela a-t-il changé, et qu'est-ce qui a bougé avec » |
| Une page par version | [GitHub Releases](https://github.com/ujeenet/rustango/releases) | « qu'y a-t-il dans la version vers laquelle je viens de migrer » |
| Ce que l'API expose en ce moment | [docs.rs/rustango](https://docs.rs/rustango) | signatures, documentation par élément |

Le changelog est la source ; une page de release reprend le même contenu pour une seule version. Les deux ne sont disponibles qu'en anglais.

## Comment lire une entrée

Les entrées sont regroupées sous `Added` (ajouté), `Changed` (modifié), `Fixed` (corrigé), `Removed` (supprimé) et `Security` (sécurité), la version la plus récente en premier, en suivant [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Deux conventions à connaître :

- **Les entrées disent pourquoi, pas seulement quoi.** Une entrée qui change un comportement explique ce qui se passait avant, car c'est cela qui vous dit si vous êtes concerné.
- **Les numéros d'issue sont porteurs.** `(#1229)` renvoie à la discussion qui a produit le changement, laquelle contient en général plus de détails que l'entrée ne peut en accueillir.

## Versionnage

Le projet suit [SemVer](https://semver.org/) de façon souple, avec une réserve qui compte avant la 1.0 : **rien avant la 1.0 ne porte de garantie de stabilité.** Une version mineure peut changer une API.

En pratique le projet l'évite, et un changement cassant reçoit une entrée `Changed` qui indique quoi faire. Mais la garantie n'est pas encore là : épinglez une version que vous avez testée plutôt qu'une plage.

Deux versions ont été retirées. `0.51.0` et `0.51.1` promettaient une réconciliation de migrations qui ne s'est jamais déclenchée face à une vraie base de données ; passez directement à `0.51.2` ou ultérieure, qui corrige les deux. Voyez [Migrations](migrations.md) pour savoir quoi faire si vous êtes sur l'une d'elles.

## Mise à niveau

Relevez l'épinglage, lancez vos tests et lisez les entrées `Changed` et `Removed` de chaque version que vous avez sautée — pas seulement celle sur laquelle vous arrivez. La plupart des mises à niveau se résument à un changement de version, et rien d'autre.

Si un modèle a changé, `cargo run -- makemigrations` vous montre ce qui a bougé avant que quoi que ce soit ne touche la base de données.
