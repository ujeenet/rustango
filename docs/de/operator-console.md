# Die Operator-Konsole

Ein Multi-Tenant-Projekt hat zwei Arten von Administratoren, und die vermischen sich nie:

- **Operatoren** betreiben das *Deployment*. Sie leben in der Registry, melden sich auf der Apex-Domain an und erreichen jeden Tenant.
- **Tenant-Benutzer** betreiben *einen* Tenant. Sie leben in der Datenbank dieses Tenants und sehen die Konsole nie.

Die Operator-Konsole ist die Weboberfläche für die erste Art — Tenants provisionieren, Hostnamen binden, Operatoren verwalten, den Audit-Trail lesen und einen Tenant außer Betrieb nehmen. Alles davon ist auch ein [`manage`-Verb](manage.md#tenancy-befehle), denn eine Aktion, die es nur auf einer Oberfläche gibt, lässt sich nicht automatisieren — und eine, die es nur in der Shell gibt, nicht delegieren.

[![Die Tenant-Liste der Operator-Konsole — jeder Tenant der Registry mit Speichermodus, Host-Muster und Aktiv-Status, dazu Aktionen zum Provisionieren, Migrieren und Vorwärmen](../img/operator-console.png)](../img/operator-console.png)

## Inhaltsverzeichnis

- [Einbinden](#einbinden)
- [Was jede Seite tut](#was-jede-seite-tut)
- [Hostnamen](#hostnamen)
- [Operatoren](#operatoren)
- [Das Audit-Log](#das-audit-log)
- [Provisioning-Läufe](#provisioning-läufe)
- [Die drei Fähigkeitsstufen](#die-drei-fähigkeitsstufen)

---

## Einbinden

Die Konsole ist ein Router, den du einhängst; wie viel sie kann, hängt davon ab, was du ihr gibst.

```rust
use rustango::tenancy::operator_console::{router, router_with_pools, router_with_provisioning, SessionSecret};

// Nur lesend: Tenants, Operatoren und das Audit-Log durchsehen.
let app = router(registry.clone(), SessionSecret::from_env_or_random());

// …plus Tenants bearbeiten, Operatoren verwalten, Hostnamen binden, Pools vorwärmen.
let app = router_with_pools(registry.clone(), pools.clone(), secret);

// …plus neue Tenants provisionieren und Migrationen ausführen.
let app = router_with_provisioning(registry.clone(), pools.clone(), provisioner, secret);
```

In einem generierten `tenant`-Projekt ist das bereits verdrahtet — `Cli::new().tenancy().with_tenant_provisioning("migrations")` hängt die volle Variante ein. Siehe [Scaffolding](scaffolding.md).

Die Konsole antwortet auf der **Apex**-Domain (`RUSTANGO_APEX_DOMAIN`), nicht auf einer Tenant-Subdomain: `http://localhost:8080/login` bei Apex `localhost`. Eine Anfrage an `127.0.0.1` ist nicht der Apex und trifft nicht zu.

---

## Was jede Seite tut

| Seite | Wofür sie da ist |
|---|---|
| **Organizations** | Jeder Tenant, mit Speichermodus, Host-Muster und Aktiv-Status |
| **Organizations → Edit** | Anzeigename, Host-Muster, Pfad-Präfix, Port, Datenbank-URL, Branding |
| **Hostnames** | Die zusätzlichen Domains, unter denen ein Tenant antwortet |
| **Operators** | Wer sich an dieser Konsole anmelden darf |
| **Audit log** | Jede Änderung, die über diese Konsole lief |
| **Runs** | Provisioning- und Migrationsläufe, live gestreamt |

---

## Hostnamen

Ein Tenant ist unter seiner Subdomain erreichbar und optional unter weiteren Hostnamen, die du an ihn bindest. Ein Hostname führt zu genau einem Tenant.

[![Die Hostnamen-Seite eines Tenants — der Basis-Host als solcher markiert und nicht löschbar, weitere Hosts mit Park- und Entfernen-Aktionen, dazu ein Formular zum Hinzufügen](../img/operator-console-hosts.png)](../img/operator-console-hosts.png)

Zwei Dinge, die man wissen sollte:

**Der Basis-Host hat keinen Löschen-Button.** Er stammt aus der Spalte `host_pattern` des Tenants, nicht aus der Hostnamen-Tabelle, es gibt also keine Zeile zum Entfernen — ändere ihn stattdessen auf der Bearbeitungsseite des Tenants. Das ist strukturell, keine UI-Regel: die Engine lehnt es ebenfalls ab, sodass jemand, der den `POST` errät, dieselbe Antwort bekommt wie jemand, der die Seite liest.

**Stilllegen behält die Zeile.** *Park* nimmt einen Host aus dem Betrieb, ohne den Eintrag zu verlieren — nützlich, während DNS propagiert, oder beim Ausmustern einer Domain, die du vielleicht zurückhaben willst. *Serve* nimmt ihn wieder in Betrieb.

Hostnamen werden beim Eingang normalisiert: kleingeschrieben, ohne Schema, ohne Port, ohne Pfad. Der gespeicherte Wert wird Byte für Byte gegen den `Host`-Header verglichen, ein Wert, der nie passen könnte, wird also abgelehnt statt gespeichert.

Über die CLI: [`list-hosts` / `add-host` / `remove-host` / `set-host-enabled`](manage.md#hostnamen).

---

## Operatoren

[![Die Operatoren-Seite — der angemeldete Operator als „das bist du" markiert, mit Formular zum Anlegen eines weiteren und der Option, ein Passwort zu generieren](../img/operator-console-operators.png)](../img/operator-console-operators.png)

Operatoren werden deaktiviert, nie gelöscht: die Zeile bleibt, sodass sich ein späteres „wer war das?" noch auflösen lässt. Die Konsole liest sie bei **jeder** Anfrage neu, sodass eine Deaktivierung beim nächsten Klick greift und nicht erst, wenn das Cookie abläuft.

Zwei Dinge lässt die Seite aus demselben Grund nicht zu:

- **Dich selbst deaktivieren.** Deine nächste Anfrage würde abgewiesen.
- **Den letzten aktiven Operator deaktivieren.** Das sperrt alle aus, und nur eine Shell auf der Registry könnte es rückgängig machen.

Ein generiertes Passwort wird **einmal** angezeigt, im Antwortkörper — nie über einen Redirect, der es in die Adressleiste, die Historie, den Referrer und jedes Access-Log dazwischen tragen würde.

Über die CLI: [`list-operators` / `set-operator-active`](manage.md#list-operators).

---

## Das Audit-Log

[![Das Audit-Log der Konsole — wer wann was an welchem Datensatz geändert hat, filterbar nach Entität, Id und Operation](../img/operator-console-audit.png)](../img/operator-console-audit.png)

Jede Änderung über die Konsole wird festgehalten: Tenant-Bearbeitungen, Hostnamen-Änderungen, Operator-Verwaltung, Impersonation, Purges, Vorwärmen. Die Spalte `source` trägt `operator:<id>:<verb>`, sodass sich Operator-Aktivität im Nachhinein von Tenant-Benutzer-Aktivität trennen lässt.

Das ist das Log der **Registry**. Die eigene Historie eines Tenants liegt im Admin dieses Tenants — beides zu mischen hieße, für eine Seite eine Abfrage über jeden Tenant-Pool zu fächern.

Es wächst mit der Nutzung der Konsole, also kürze es regelmäßig mit [`audit-cleanup`](manage.md#audit-cleanup), das das Log der Registry und das jedes aktiven Tenants aufräumt.

Über die CLI: [`audit-log`](manage.md#nachsehen-was-passiert-ist).

---

## Provisioning-Läufe

[![Die Seite der Provisioning-Läufe — jeder Lauf mit Art, Tenant, Status und Zeiten, verlinkt auf die aufgezeichneten Schritte](../img/operator-console-runs.png)](../img/operator-console-runs.png)

Einen Tenant zu provisionieren sind mehrere Schritte gegen eine Datenbank, die langsam oder nicht erreichbar sein kann — deshalb wird es als **Lauf** aufgezeichnet und nicht als Anfrage, die entweder zurückkommt oder nicht. Jeder Lauf streamt seine Schritte live und übersteht einen Reload, eine neue Verbindung oder das Zuschauen von einem zweiten Pod aus.

Über die CLI angelegte Tenants werden ebenfalls aufgezeichnet, markiert mit `requested_by = cli`, sodass die Historie beide Oberflächen abdeckt.

Über die CLI: [`list-runs` / `show-run`](manage.md#nachsehen-was-passiert-ist).

---

## Die drei Fähigkeitsstufen

Welche Routen existieren, hängt davon ab, welchen Konstruktor du eingehängt hast:

| | `router` | `router_with_pools` | `router_with_provisioning` |
|---|---|---|---|
| Tenants, Operatoren, Audit-Log durchsehen | ✓ | ✓ | ✓ |
| Tenant bearbeiten, Hostnamen verwalten | | ✓ | ✓ |
| Operatoren anlegen / deaktivieren | | ✓ | ✓ |
| Pools vorwärmen | | ✓ | ✓ |
| Tenant außer Betrieb nehmen | | ✓ | ✓ |
| Neuen Tenant provisionieren, Migrationen ausführen | | | ✓ |
| Provisioning-Läufe ansehen | | | ✓ |

Nicht eingehängte Routen liefern 404 statt 403 — eine Nur-Lese-Konsole bewirbt nicht, was sie nicht kann.

**Jeder Operator kann alles.** Es gibt keine Rechte-Gates pro Operator: ein Operator erreicht jeden Tenant und jede Aktion, die die Konsole anbietet. Die Zugriffsgrenze ist die Operatorenliste selbst — genau deshalb greift eine Deaktivierung bei der nächsten Anfrage.
