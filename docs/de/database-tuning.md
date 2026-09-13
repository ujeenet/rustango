# Datenbank-Tuning

Jede Datenbankverbindung Ihrer Anwendung stammt aus einem **Pool** —
einer Menge offen gehaltener Verbindungen, die an Requests ausgegeben
werden. Die Voreinstellungen des Pools kommen vom Treiber, nicht von
Ihrer Last, und sie sind für die meisten Produktivsysteme in mindestens
einem Punkt falsch.

Diese Seite erklärt, was jeder Parameter bewirkt, was schiefgeht, wenn
man ihn unangetastet lässt, und wo man ihn setzt.

## Warum überhaupt tunen

Vier Fehlerbilder, jedes durch eine andere Voreinstellung verursacht:

**Ihr zehnter gleichzeitiger Request wartet.** Der Standard-Pool des
Treibers hält **10** Verbindungen. Request elf schlägt nicht fehl — er
*wartet in der Schlange*, und Ihre p99-Latenz steigt, während die CPU
untätig ist. Nichts im Log sagt „Pool erschöpft"; Sie sehen langsame
Requests.

**Eine nicht erreichbare Datenbank legt den ganzen Server lahm.** Kann
keine Verbindung geholt werden, wartet der Aufrufer. Die Voreinstellung
des Treibers sind 30 Sekunden — eine Zahl für Batch-Werkzeuge: auf dem
Request-Pfad bedeutet sie, dass ein Worker pro Request eine halbe Minute
blockiert ist. Der Ausfall *einer* Datenbank sättigt so alle Worker und
reißt Oberflächen mit, die diese Datenbank nie anfassen. Rustango setzt
deshalb **5 Sekunden** als Voreinstellung.

**Verbindungen sterben unbemerkt, und der nächste Request zahlt dafür.**
Ein Load Balancer, eine Firewall oder PostgreSQLs eigenes
`idle_in_transaction_session_timeout` schließt eine untätige Verbindung.
Ihr Pool weiß davon nichts. Der nächste Request, der sie ausleiht,
bekommt einen Broken Pipe — sporadisch und bei geringer Last, also genau
die Sorte Fehler, die sich am schlechtesten reproduzieren lässt.

**Ein Failover oder eine Passwortrotation greift nicht.** Gepoolte
Verbindungen sind bewusst langlebig. Nach einem Failover oder einem
rotierten Passwort kann ein Pool weiter Verbindungen zum alten Server
oder mit den alten Zugangsdaten benutzen, bis etwas sie schließt.

## Wo Sie es einstellen

Drei Orte. **Weiter oben gewinnt**, damit ein Notfall-Override zur
Deploy-Zeit niemals einen Config-Push und einen Neustart Ihrer
Config-Pipeline braucht.

| | wo | wofür |
|---|---|---|
| 1 | Umgebungsvariablen | Overrides pro Deployment, Secrets-Manager, Nachjustieren im Notfall |
| 2 | `[database]` in Ihrer Settings-Stufe | die Werte, mit denen Ihr Projekt normalerweise läuft, in der Versionsverwaltung |
| 3 | Framework-Voreinstellungen | was Sie bekommen, wenn Sie nichts setzen |

### In einer Settings-Stufe

Settings liegen in `config/*_settings.toml`, eine Datei pro Umgebung.
Tragen Sie die Werte in die Stufe ein, zu der sie gehören —
Entwicklungswerte in `dev_settings.toml`, Produktionswerte in
`prod_settings.toml`:

```toml
[database]
pool_max_size             = 50
pool_min_size             = 5
pool_acquire_timeout_secs = 5
pool_idle_timeout_secs    = 600
pool_max_lifetime_secs    = 1800
```

### Als Umgebungsvariablen

Jeder Parameter hat ein Umgebungs-Override, das Vorrang vor dem TOML hat:

```bash
RUSTANGO_DB_MAX_CONNECTIONS=50
RUSTANGO_DB_MIN_CONNECTIONS=5
RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS=5
RUSTANGO_DB_IDLE_TIMEOUT_SECS=600
RUSTANGO_DB_MAX_LIFETIME_SECS=1800
```

Ein Wert, der keine positive ganze Zahl ist, wird **mit einer Warnung
ignoriert** statt befolgt — ein Tippfehler soll weder stillschweigend
eine Grenze auf null setzen noch den Start abbrechen.

### Eine Reihenfolgeregel

Die Settings werden von `Cli::with_settings(...)` auf die Pools
angewendet. Rufen Sie das auf, **bevor** irgendetwas eine
Datenbankverbindung öffnet. Ein früher gebauter Pool läuft mit den
Umgebungs-Voreinstellungen, und das Framework protokolliert eine Warnung
mit der Anzahl betroffener Pools, statt Sie das unter Last entdecken zu
lassen.

## Die Parameter

| Setting | Umgebungsvariable | Voreinstellung | Wirkung |
|---|---|---|---|
| `pool_max_size` | `RUSTANGO_DB_MAX_CONNECTIONS` | 10 (Treiber) | Höchstzahl geöffneter Verbindungen. Weitere Requests warten. |
| `pool_min_size` | `RUSTANGO_DB_MIN_CONNECTIONS` | 0 (Treiber) | Verbindungen, die auch im Leerlauf offen bleiben. |
| `pool_acquire_timeout_secs` | `RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS` | 5 | Wie lange ein Aufrufer auf eine Verbindung wartet, bevor er scheitert. |
| `pool_idle_timeout_secs` | `RUSTANGO_DB_IDLE_TIMEOUT_SECS` | Treiber-Voreinstellung | Schließt Verbindungen, die so lange untätig waren. |
| `pool_max_lifetime_secs` | `RUSTANGO_DB_MAX_LIFETIME_SECS` | Treiber-Voreinstellung | Schließt Verbindungen ab diesem Alter, unabhängig von der Nutzung. |

Einen Parameter nicht zu setzen ist nicht dasselbe, wie ihn auf null zu
setzen. Nicht gesetzt heißt: *die Voreinstellung des Treibers gilt* —
also das Verhalten, das Sie hatten, bevor es den Parameter gab.

### `pool_max_size`

Die Obergrenze für gleichzeitige Datenbankarbeit. Erhöhen Sie sie, wenn
Requests auf Verbindungen warten; das Symptom ist Latenz, die mit dem
Verkehr wächst, während die Datenbank selbst nicht ausgelastet ist.

**Es ist eine Obergrenze, kein Zielwert** — der Pool öffnet Verbindungen
nach Bedarf und nur bis zu dieser Zahl.

Die wichtige Nebenbedingung liegt auf der *anderen* Seite: Ihr
Datenbankserver hat sein eigenes Verbindungslimit (PostgreSQLs
`max_connections`, typischerweise 100). Die Summe aller Pools über alle
Repliken muss darunter bleiben, sonst werden neue Verbindungen
abgewiesen. Zehn Pods mit `pool_max_size = 50` sind 500 Verbindungen
gegen einen Server, der 100 erlaubt.

### `pool_min_size`

Verbindungen, die offen bleiben, auch wenn sie niemand benutzt. Es geht
um Latenz: bei `0` zahlt der erste Request nach einer ruhigen Phase den
vollen TCP-, TLS- und Authentifizierungs-Roundtrip, bevor irgendeine
Query läuft.

Bemessen Sie den Wert nach Ihrer Grundlast, nicht nach der Spitze.
Offen gehaltene Verbindungen kosten auch auf dem Server Ressourcen.

### `pool_acquire_timeout_secs`

Wie lange ein Aufrufer auf eine Verbindung wartet, bevor er aufgibt —
das umfasst **sowohl** den Aufbau einer neuen Verbindung **als auch** das
Anstehen für eine freie.

Dieser Parameter entscheidet, wie sich Ihre Anwendung verhält, wenn die
Datenbank nicht erreichbar ist. Zu hoch, und Worker blockieren an einer
Datenbank, die nie antworten wird; zu niedrig, und eine legitime
Lastspitze — bei der Anstehen echte, produktive Arbeit ist — wird zu
Fehlern.

Die Voreinstellung von 5 Sekunden ist bewusst enger als die 30 des
Treibers. Erhöhen Sie sie bei langlaufenden Queries und einem
ausgelasteten, aber gesunden Pool; senken Sie sie, wenn Sie Last lieber
schnell abweisen.

### `pool_idle_timeout_secs`

Schließt Verbindungen, die ungenutzt herumlagen. Setzen Sie den Wert
**unter** das kürzeste Idle-Timeout auf dem Weg zwischen Anwendung und
Datenbank — Load Balancer, Proxy, Connection-Tabelle einer Firewall oder
die Idle-Timeouts des Datenbankservers selbst. Schließt etwas anderes die
Verbindung zuerst, gibt Ihr Pool eine tote Verbindung aus.

10 Minuten sind ein gängiger Startwert.

### `pool_max_lifetime_secs`

Schließt Verbindungen ab einem festen Alter, ob gesund oder nicht. Das
ist der Parameter, der Failover und Passwortrotation überhaupt erst
wirksam macht: ohne ihn kann ein Pool weiter mit dem Server sprechen, zu
dem er beim Start verbunden hat.

30 Minuten sind ein gängiger Startwert. Kürzer, wenn Sie Zugangsdaten mit
kurzer Gültigkeit aus einem Secrets-Manager beziehen.

## Backends unterscheiden sich

**SQLite ist kein Server**, und Pool-Größen bedeuten dort etwas anderes.
SQLite serialisiert Schreiber global — immer nur ein Schreiber,
unabhängig von der Pool-Größe — ein höheres `pool_max_size` bringt also
Lese-, aber nie Schreibparallelität. Bei einer In-Memory-Datenbank sind
zusätzliche Verbindungen schlimmer als nutzlos: jede ist eine *eigene
leere Datenbank*, sofern die URL nicht `cache=shared` setzt.

**PostgreSQL und MySQL** erzwingen beide ein serverseitiges
Verbindungslimit. Bemessen Sie Ihre Pools an diesem Budget über alle
Repliken hinweg, und denken Sie daran, dass Connection-Pooler wie
PgBouncer die Rechnung verändern.

## Verbindungsoptionen sind etwas anderes

Die obigen Parameter betreffen den *Pool*. Optionen für eine einzelne
*Verbindung* — TLS, Verbindungs-Timeouts, der Anwendungsname, den der
Server sieht — reisen in der Verbindungs-URL mit:

```
postgres://user:pw@host:5432/db?sslmode=require&connect_timeout=10&application_name=myapp
mysql://user:pw@host:3306/db?ssl-mode=REQUIRED
sqlite://./dev.db?mode=rwc
```

Diese werden an den Treiber durchgereicht; alles, was dessen URL-Parser
akzeptiert, funktioniert.

## Tenant-Pools

In einer mandantenfähigen Anwendung bekommt jeder Tenant im
Datenbank-Modus seinen eigenen Pool, und diese werden separat über
`TenantPoolsConfig` konfiguriert — siehe
[Tenant-Pool-Tuning](manage.md) in der `manage`-Anleitung. Ihre
Voreinstellungen weichen von denen des primären Pools ab; insbesondere
ist das Acquire-Timeout der Tenants großzügiger, was einen Blick wert
ist, wenn Sie viele Tenants gegen Datenbanken betreiben, die unabhängig
voneinander ausfallen können.
