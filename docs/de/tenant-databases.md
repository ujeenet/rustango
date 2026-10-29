# Tenant-Datenbanken auf einem gespeicherten Server

Ein Tenant im Datenbankmodus kann eine eigene Datenbank-URL mitbringen, oder
rustango legt die Datenbank für ihn an. Diese Seite zeigt den zweiten Weg von
Anfang bis Ende: Server speichern, testen, Tenants darauf anlegen, ihre
Passwörter rotieren, sie löschen.

rustango legt für jeden Tenant eine Datenbank mit dem Slug als Namen an und
einen eigenen Login mit Zufallspasswort, der nur auf diese Datenbank Rechte
hat. Beides wird in `rustango_tenant_databases` festgehalten. Nur eine
festgehaltene Datenbank wird von rustango je gelöscht oder bekommt ein neues
Passwort.

## Inhaltsverzeichnis

- [Bevor du anfängst](#bevor-du-anfängst)
- [Rechte](#rechte)
- [1. Den geheimen Schlüssel setzen](#1-den-geheimen-schlüssel-setzen)
- [2. Den Server speichern](#2-den-server-speichern)
- [3. Ihn testen](#3-ihn-testen)
- [4. Einen Tenant anlegen](#4-einen-tenant-anlegen)
- [5. Das Passwort eines Tenants rotieren](#5-das-passwort-eines-tenants-rotieren)
- [6. Einen Tenant löschen](#6-einen-tenant-löschen)
- [Der geheime Schlüssel: Backup und Rotation](#der-geheime-schlüssel-backup-und-rotation)
- [Backup und Wiederherstellung](#backup-und-wiederherstellung)
- [Hinweise zu PostgreSQL](#hinweise-zu-postgresql)
- [Hinweise zu MySQL](#hinweise-zu-mysql)
- [Lokale Entwicklung mit Docker](#lokale-entwicklung-mit-docker)

## Bevor du anfängst

- Der Server läuft mit dem Backend der Registry. Eine Postgres-Registry legt
  Postgres-Tenants an, und so weiter.
- **PostgreSQL 15 oder neuer.** Vor 15 darf jeder Login im Schema `public`
  jeder Datenbank Tabellen anlegen, auch in der der Registry.
  `test-db-server` lehnt ältere Server ab.
- Der Admin-Login braucht diese Rechte:

| Backend | Rechte |
|---|---|
| PostgreSQL | `CREATEDB` und `CREATEROLE`, oder Superuser |
| MySQL / MariaDB | globales `CREATE`, `DROP`, `CREATE USER` und jedes Datenbankrecht `WITH GRANT OPTION` |
| SQLite | ein beschreibbares Verzeichnis; Tenants sind `<dir>/<slug>.db` |

Rechte aus MySQL-Rollen zählen, wenn die Rolle standardmäßig aktiv ist
(`SET DEFAULT ROLE`). Das vollständige Bild steht unter [Rechte](#rechte).

## Rechte

Jeder Weg, einen Tenant anzulegen, nutzt einen anderen Login und braucht
andere Rechte. **Test connection**, `test-db-server` und das Anlegen selbst
prüfen dieselbe Liste, nennen, was fehlt, und geben die Anweisungen aus, die
es beheben. Ein Anlegen wird abgelehnt, bevor irgendetwas entsteht.

| Option | Wo die Daten liegen | Genutzter Login | Eigener Login des Tenants | Nötige Rechte |
|---|---|---|---|---|
| Datenbank, neu auf einem gespeicherten Server | eine neue Datenbank mit dem Slug als Namen | der Admin-Login des Servers, einmalig | ja | Datenbanken anlegen, Logins anlegen, Rechte vergeben |
| Datenbank, vorhandene URL | die Datenbank in der URL | der Login in der URL | der der URL | keine: nichts wird angelegt; der Login muss Tabellen anlegen dürfen |
| Datenbank auf dem Server der Registry | eine vorhandene Datenbank dort | der der Registry (`DATABASE_URL`), geteilt | nein, nicht isoliert | keine: nichts wird angelegt |
| Schema (nur PostgreSQL) | ein Schema in der Registry-Datenbank | der der Registry (`DATABASE_URL`), geteilt | nein | `CREATE` auf der Registry-Datenbank |

Um einem Login die Rechte zu geben, führe als Admin aus. MySQL / MariaDB, als
`root`:

```sql
GRANT CREATE, DROP, CREATE USER ON *.* TO 'rustango'@'%';
GRANT ALL PRIVILEGES ON *.* TO 'rustango'@'%' WITH GRANT OPTION;
```

PostgreSQL, als Superuser:

```sql
ALTER ROLE rustango CREATEDB CREATEROLE;
-- schema mode: the registry's login
GRANT CREATE ON DATABASE app TO rustango;
```

Der MySQL-Login `rustango` aus der generierten docker-compose hat nur Rechte
auf seine eigene Datenbank: Speichere den Server stattdessen mit `root` als
Admin-Login. Sagt der Server nicht, was ein Login darf, meldet die Prüfung
„unknown" und lässt das Anlegen trotzdem versuchen. Oder überspringe all das:
Nutze eine vorhandene Datenbank-URL.

## 1. Den geheimen Schlüssel setzen

Server-Passwörter und neue Tenant-URLs werden versiegelt gespeichert
(`enc:v1:`), mit einem Schlüssel, der aus `RUSTANGO_SECRET_KEY` abgeleitet
wird. Nimm 32 oder mehr Zufallsbytes:

```bash
export RUSTANGO_SECRET_KEY="$(openssl rand -base64 48)"
```

Ohne Schlüssel oder mit einem leeren wird ein Server-Passwort abgelehnt und
eine Tenant-URL unversiegelt mit einer Warnung gespeichert (abgelehnt unter
`RUSTANGO_REQUIRE_SEALED_SECRETS=1`). Die Datenbankserver-Seiten der Konsole
und ihre Seite **New tenant** zeigen ein Banner, solange der Schlüssel fehlt
oder zu kurz ist.

## 2. Den Server speichern

Konsole: **Organizations → Database servers**. CLI:

```bash
cargo run -- add-db-server main --host db.internal --user admin --tls require
```

Das Passwort wird verdeckt abgefragt und versiegelt gespeichert. TLS ist
standardmäßig `require`. Alle Flags stehen in [manage.md](manage.md).

## 3. Ihn testen

```bash
cargo run -- test-db-server main
```

Der Befehl meldet sich an und gibt die Server-Version aus und, je eine Zeile,
ob der Admin Datenbanken anlegen, Logins anlegen und Rechte vergeben darf.
Fehlt eines, nennt er es und gibt die Anweisungen aus, die es beheben (siehe
[Rechte](#rechte)). Auf der Konsolenseite **New tenant** führt **Test
connection** mit einem Slug außerdem jede Prüfung aus, die ein Anlegen vor
jeder DDL ausführt: Slug frei, Routing frei, und keine Datenbank und kein
Login mit diesem Namen.

## 4. Einen Tenant anlegen

Konsole: **Organizations → New tenant**, Datenbankmodus, **Create a new
database on a saved server**. CLI:

```bash
cargo run -- create-tenant acme --server main
```

Der Lauf legt Datenbank, Login und Rechtevergabe an, meldet sich als neuer
Login an, registriert den Tenant, führt seine Migrationen aus und aktiviert
ihn. Ein Fehler, nachdem die Datenbank existiert, löscht, was der Lauf
angelegt hat. Die URL des Tenants enthält das Passwort des neuen Logins und
wird versiegelt gespeichert.

Um dem Tenant seinen ersten Admin zu geben, trag im Formular die Admin-E-Mail
ein (Benutzername optional; Standard ist die E-Mail), oder per CLI:

```bash
cargo run -- create-tenant acme --server main --admin-email admin@example.com
```

Der Lauf legt einen Superuser ohne nutzbares Passwort an, bevor der Tenant
live geht. Der Admin setzt sein Passwort selbst über einen Link, der einmal
funktioniert, 72 Stunden lang, nur für diesen Tenant. Mit
`Builder::first_admin_mailer` wird er ihm gemailt; sonst zeigt die Konsole
ihn nach dem Absenden **einmal** an, und die CLI gibt ihn aus. Kopiere ihn
dann. Ein verlorener oder abgelaufener Link:
`cargo run -- set-password-link acme admin@example.com`. Details:
[Erster Admin](operator-console.md#erster-admin).

## 5. Das Passwort eines Tenants rotieren

Konsole: die Seite des Tenants, **Rotate database password**. CLI:

```bash
cargo run -- rotate-tenant-password acme
```

Beide laufen als festgehaltener Lauf mit diesen Schritten:

1. `check`: Der Tenant hat einen Eintrag, seine URL nennt noch diese
   Datenbank und diesen Login, die URL ist im Klartext oder versiegelt
   gespeichert (eine `env://`- oder Vault-Referenz wird abgelehnt: Ändere das
   Passwort in diesem Speicher), und der Admin darf den Login ändern.
2. `alter_password`: ein neues Zufallspasswort auf dem Server.
3. `store_password`: Die neue URL wird versiegelt nur geschrieben, solange die
   gespeicherte URL noch die gelesene ist (Compare-and-Set). Scheitert das,
   setzt `revert` das alte Passwort zurück, aber nur, solange die gespeicherte
   URL es noch enthält; sonst führe `rotate-tenant-password` erneut aus.
4. `invalidate_pools`: Dieser Prozess verwirft seine Pools für den Tenant.
5. `discard_old_password`: siehe das Zeitfenster unten.

**Das Zeitfenster.** Andere Pods bemerken die neue URL beim nächsten
Fingerprint-Poll von `rustango_orgs` (`RUSTANGO_HOST_GEN_CHECK_SECS`,
Standard 5 s). Bis dahin nutzen ihre *neuen* Verbindungen das alte Passwort.
Offene Verbindungen werden nie getrennt.

- Auf MySQL 8.0.14 und neuer bleibt das alte Passwort mit
  `RETAIN CURRENT PASSWORD` für zwei Poll-Intervalle gültig und wird dann mit
  `DISCARD OLD PASSWORD` verworfen. Keine Verbindung schlägt fehl. Das
  globale `CREATE USER` des Admins reicht für beides. Stoppt der Prozess
  während des Wartens, bleibt das alte Passwort bis zur nächsten Rotation
  oder Löschung gültig.
- Auf PostgreSQL und MariaDB gibt es keine doppelten Passwörter: Das alte
  hört sofort auf zu gelten, sodass andere Pods bis zu ein Poll-Intervall lang
  keine neuen Verbindungen öffnen können.

Ein SQLite-Tenant hat kein Passwort. Eine Datenbank, die rustango nicht
angelegt hat, wird abgelehnt.

## 6. Einen Tenant löschen

Konsole: die Seite des Tenants, **Purge** (Slug eintippen, **drop the
database** ankreuzen). CLI:

```bash
cargo run -- purge-tenant acme --confirm acme --purge-database
```

Mit einer Provisioning-Konsole ist das Löschen ein festgehaltener Lauf, den
du live auf seiner Lauf-Seite verfolgst; die CLI gibt dieselben Schritte aus.
Jede Ablehnung kommt zuerst, in `check`, sodass ein abgelehntes Löschen
nichts ändert:

- Die URL des Tenants nennt die festgehaltene Datenbank oder den Login nicht
  mehr, etwa nach `edit-tenant --database-url` oder
  `migrate-tenant-storage`. Nichts wird gelöscht: Setz die URL zurück, oder
  lösche diese Datenbank von Hand und entferne ihre Zeile in
  `rustango_tenant_databases`;
- die URL eines anderen Tenants nennt dieselbe Datenbank oder lässt sich zum
  Ausschließen nicht lesen (auf MySQL und SQLite wird jede gegen den laufenden
  Server und die Datei geprüft, ein Alias zählt also), oder es ist die der
  Registry;
- ein anderes Löschen oder eine Passwortänderung läuft für den Tenant;
- der Admin besitzt die Datenbank nicht (Postgres: ein anderer Admin hat sie
  angelegt) oder darf den Login nicht löschen (siehe
  [Hinweise zu PostgreSQL](#hinweise-zu-postgresql)); auf MySQL braucht er
  globales `DROP` und `CREATE USER`;
- eine Sitzung, die nicht von den Pools dieses Tenants geöffnet wurde, ist in
  der Datenbank offen (ein Operator, ein Backup).

Dann: `deactivate`, `purge_media`, `drop_database` (vorher werden die eigenen
Sitzungen dieses Tenants auf jedem Pod beendet), `drop_login`, `remove_org`,
`delete_record`. Ein erneuter Versuch nach einem gescheiterten Schritt
überspringt, was schon weg ist. SQLite löscht die Datei des Tenants und ihre
Dateien `-wal`, `-shm` und `-journal`.

Eine Datenbank, die der Operator mitgebracht hat, hat keinen Eintrag und
behält das alte Verhalten: Auf Postgres wird sie über ihre eigene URL
gelöscht; anderswo bleibt sie stehen, und das Löschen sagt das.

## Der geheime Schlüssel: Backup und Rotation

**Sichere `RUSTANGO_SECRET_KEY` zusammen mit dem Registry-Backup.** Ohne ihn
öffnet sich keine versiegelte Tenant-URL und kein Server-Passwort, und jeder
Tenant auf einem gespeicherten Server ist unerreichbar, bis jede URL neu
eingegeben wurde.

Um den Schlüssel zu wechseln:

1. Setze `RUSTANGO_SECRET_KEY_PREVIOUS` auf den alten Schlüssel und
   `RUSTANGO_SECRET_KEY` auf den neuen, in jedem Prozess, und starte neu. Der
   alte Schlüssel öffnet weiterhin, was er versiegelt hat.
2. `cargo run -- reseal-tenant-secrets` listet, was sich ändern würde.
3. `cargo run -- reseal-tenant-secrets --apply` versiegelt Tenant-URLs,
   Server-Passwörter, Operator-TOTP-Secrets, SSO-Client-Secrets und jede
   `Cast<EncryptedString>`-Spalte der Modelle (Registry und Tenants) neu mit
   dem neuen Schlüssel.
4. Entferne `RUSTANGO_SECRET_KEY_PREVIOUS` und starte neu.

Der Befehl erkennt eine versiegelte Spalte am Feldtyp, den das Model-Derive
sieht: ausgeschrieben `Cast<EncryptedString>` oder
`Option<Cast<EncryptedString>>`. Jede andere Schreibweise, etwa ein Typalias,
lehnt das Derive ab, also wird keine Spalte übersehen. Jedes Model braucht einen
einspaltigen Primärschlüssel; eines ohne wird als Fehler gemeldet. Ein Wert, den
kein Schlüssel öffnet, wird gemeldet und bleibt unverändert.

Werte, die mit einem leeren Schlüssel versiegelt wurden, bevor leere
Schlüssel abgelehnt wurden, öffnen sich weiterhin, wenn
`RUSTANGO_SECRET_KEY_PREVIOUS` gesetzt und leer ist; versiegle sie auf
dieselbe Weise neu.

## Backup und Wiederherstellung

Ein Dump einer Tenant-Datenbank enthält ihren Login nicht: Postgres-Rollen
und MySQL-Benutzer liegen außerhalb jeder einzelnen Datenbank. Um einen
Tenant auf einem anderen Server wiederherzustellen:

- Postgres: Stelle zuerst die Rollen wieder her (`pg_dumpall --roles-only`),
  oder lege den Login des Tenants an, stelle die Datenbank wieder her, vergib
  die Rechte und führe `rotate-tenant-password` aus, damit gespeicherte URL
  und Login übereinstimmen.
- MySQL: Lege den Benutzer und seine Rechte an, stelle die Datenbank wieder
  her, dann `rotate-tenant-password`.

Stelle die Registry zusammen mit ihren Zeilen in `rustango_tenant_databases`
wieder her und richte den gespeicherten Server mit `edit-db-server` auf den
neuen Host.

## Hinweise zu PostgreSQL

- **Neue Datenbanken kopieren `template1`.** Dessen Encoding, Locale und alle
  Objekte oder Extensions darin erreichen jeden Tenant. Halte `template1` in
  `UTF8`.
- **Extensions.** Der Tenant-Login hat alle Rechte auf seiner Datenbank, ist
  aber kein Superuser. Er kann `CREATE EXTENSION` für *vertrauenswürdige*
  Extensions ausführen (`pgcrypto`, `citext`, `hstore`, `pg_trgm`,
  `uuid-ossp`, `unaccent`, `ltree` und andere als trusted markierte). Eine
  nicht vertrauenswürdige, etwa PostGIS, muss ein Superuser installieren: in
  `template1`, bevor der Tenant angelegt wird, oder danach in der Datenbank
  des Tenants.
- **Den Admin-Login wechseln (PostgreSQL 16+).** Ein `CREATEROLE`-Login darf
  nur Rollen ändern oder löschen, auf die er `ADMIN OPTION` hat, und nur der
  Admin, der einen Tenant-Login angelegt hat, hat sie. Nachdem du einen
  Server auf einen anderen Admin umgestellt hast, werden Rotation und Löschen
  älterer Tenants abgelehnt, mit dem auszuführenden `GRANT … WITH ADMIN
  OPTION`. Ein Löschen braucht außerdem, dass der Admin die Datenbank besitzt
  (oder Superuser ist).
- **Sitzungen beenden.** Tenant-Pools verbinden sich mit
  `application_name=rustango-tenant:<id>`. Ein Löschen beendet diese
  Sitzungen, wofür es einen Superuser, die Mitgliedschaft in der Rolle des
  Tenants oder `pg_signal_backend` braucht; ohne das wird das Löschen
  abgelehnt, solange solche Sitzungen offen sind.

## Hinweise zu MySQL

- Der Tenant-Login ist `'<login>'@'%'`: Er darf sich von jedem Host verbinden,
  den das Netzwerk des Servers zulässt. Begrenze ihn über das Netzwerk oder
  die `bind-address` des Servers; Logins pro Host werden noch nicht angeboten.
- Tenant-Datenbanken sind `utf8mb4` mit der Collation der Registry-Datenbank.
- Doppelte Passwörter brauchen MySQL 8.0.14+; MariaDB rotiert ohne sie.

## Lokale Entwicklung mit Docker

Das Standard-Image `postgres` hat kein TLS, speichere es also mit
`--tls disable`. `mysql:8` liefert TLS mit einem selbstsignierten Zertifikat:
`--tls require` funktioniert, `verify-ca` braucht dieses Zertifikat. Auf
MySQL nimm `root`: Dem Login `rustango` aus der generierten docker-compose
fehlen die Rechte (siehe [Rechte](#rechte)).

```bash
docker run -d --name pg -e POSTGRES_PASSWORD=pw -p 5432:5432 postgres:16
cargo run -- add-db-server local --host 127.0.0.1 --port 5432 --user postgres --tls disable

docker run -d --name my -e MYSQL_ROOT_PASSWORD=pw -p 3306:3306 mysql:8
cargo run -- add-db-server local --host 127.0.0.1 --port 3306 --user root --tls require
```

Nutze `--tls disable` nie für einen Server, den du über ein Netzwerk
erreichst, das du nicht kontrollierst: Der Admin-Login kann jede Datenbank
darauf löschen.
