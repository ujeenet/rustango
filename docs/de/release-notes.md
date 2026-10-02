# Versionshinweise

Jede für Nutzer sichtbare Änderung kommt mit einem Changelog-Eintrag. Diese Seite ist der Weg dorthin, denn bis jetzt hatte die Doku-Website keinen — wer hier las, sah versionsbeschriftete Überschriften innerhalb der Anleitungen und sonst nichts, und so kam es, dass eine auf 0.42 festgenagelte Überschrift sich las wie „die Doku hört bei 0.42 auf“ ([#1304](https://github.com/ujeenet/rustango/issues/1304)).

## Wo die Hinweise leben

| Was | Wo | Am besten für |
|---|---|---|
| Vollständige Historie, jede Version | [`CHANGELOG.md`](https://github.com/ujeenet/rustango/blob/main/CHANGELOG.md) | „wann hat sich das geändert, und was hat sich noch mitbewegt“ |
| Eine Seite pro Version | [GitHub Releases](https://github.com/ujeenet/rustango/releases) | „was steckt in der Version, auf die ich gerade aktualisiert habe“ |
| Was gerade in der API steht | [docs.rs/rustango](https://docs.rs/rustango) | Signaturen, Doku pro Element |

Das Changelog ist die Quelle; eine Release-Seite ist derselbe Inhalt für eine einzelne Version. Beide gibt es nur auf Englisch.

## Wie du einen Eintrag liest

Einträge sind unter `Added` (Hinzugefügt), `Changed` (Geändert), `Fixed` (Behoben), `Removed` (Entfernt) und `Security` (Sicherheit) gruppiert, die neueste Version zuerst, gemäß [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Zwei Konventionen, die du kennen solltest:

- **Einträge sagen warum, nicht nur was.** Ein Eintrag, der Verhalten ändert, erklärt, was vorher passiert ist, denn genau das sagt dir, ob es dich betrifft.
- **Issue-Nummern sind tragend.** `(#1229)` verlinkt die Diskussion, aus der die Änderung hervorging, und dort steht meist mehr Detail, als im Eintrag Platz hat.

## Versionierung

Das Projekt folgt [SemVer](https://semver.org/) locker, mit einem Vorbehalt, der vor 1.0 zählt: **nichts vor 1.0 trägt eine Stabilitätsgarantie.** Eine Minor-Version kann eine API ändern.

In der Praxis vermeidet das Projekt das, und eine brechende Änderung bekommt einen `Changed`-Eintrag, der sagt, was zu tun ist. Aber die Garantie gibt es noch nicht — pinne also eine Version, die du getestet hast, statt eines Bereichs.

Zwei Versionen wurden zurückgezogen. `0.51.0` und `0.51.1` versprachen einen Migrations-Abgleich, der gegen eine echte Datenbank nie ausgelöst wurde; aktualisiere direkt auf `0.51.2` oder neuer, was beide behebt. Unter [Migrationen](migrations.md) steht, was zu tun ist, wenn du auf einer davon bist.

## Aktualisieren

Hebe den Pin an, lass deine Tests laufen und lies die `Changed`- und `Removed`-Einträge für jede Version, die du übersprungen hast — nicht nur für die, auf der du landest. Die meisten Upgrades sind eine Versionsanhebung und sonst nichts.

Wenn sich ein Modell geändert hat, zeigt dir `cargo run -- makemigrations`, was sich bewegt hat, bevor irgendetwas die Datenbank berührt.
