# MCP-Server

Das **Model Context Protocol (MCP)** ist der offene Standard, der es einem
KI-Agenten — Claude, einem IDE-Assistenten, Ihrer eigenen LLM-App — erlaubt,
sicher die **Tools** *Ihrer* Anwendung aufzurufen, ihre **Resources** zu lesen
und ihre **Prompts** zu nutzen. **Rustango** liefert einen produktionsreifen
MCP-Server: Registrieren Sie ein Tool mit einem einzigen Makro, mounten Sie einen
Router, und jeder MCP-Client kann es über den standardmäßigen JSON-RPC-Transport
entdecken und aufrufen — mit pro-Agent, **fail-closed** Autorisierung und
eingebautem OAuth 2.1.

[![MCP-Server in Rustango: Ein LLM-Agent verbindet sich über JSON-RPC + SSE; der Server authentifiziert das JWT des Agenten, listet nur die Tools auf, die seine gewährten Skills erlauben, und führt den Tool-Handler gegen den Pool Ihrer App aus](../img/mcp.png)](../img/mcp.png)

> **Ein Begriff hier neu?** *MCP*, *JSON-RPC*, *Tool/Resource/Prompt*, *Agent*,
> *JWT*, *OAuth* — siehe das [Glossar](glossary.md).

> **Quelle:** `rustango::mcp` (`router`, `tenant_router`, `secure_tenant_router`,
> `secure_tenant_router_from_settings`, `register_mcp_tool!`,
> `register_mcp_resource!`, `McpContext`, `issue_agent_token`) und die
> Agent/Skill-Helfer aus `rustango::tenancy` — hinter dem **`mcp`-Feature**
> (standardmäßig AUS; zieht `tenancy, sse, serializer, openapi, jwt` mit).
>
> **Lauffähige Version:** Jeder Ausschnitt ist aus
> [`mcp_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/mcp_doc.rs) kopiert
> (`cargo test -p rustango --features sqlite,mcp --test mcp_doc`); die gesamte
> Protokolloberfläche wird von der Suite `crates/rustango/tests/mcp_*.rs` im
> Realbetrieb erprobt, und ein lauffähiger Server liegt in
> [`examples/mcp_demo`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/mcp_demo).

## Inhaltsverzeichnis

- [Was MCP Ihnen bietet](#what-mcp-gives-you)
- [Schritt 1 — Das Feature aktivieren](#step-1--enable-the-feature)
- [Schritt 2 — Ein Tool definieren](#step-2--define-a-tool)
- [Schritt 3 — Den Server mounten](#step-3--mount-the-server)
- [Schritt 4 — Agenten autorisieren](#step-4--authorize-agents)
- [Das Protokoll](#the-protocol)
- [Einstellungen](#settings)
- [Wie man testet](#how-to-test) — [die Suite](#a-the-test-suite) · [curl](#b-curl-the-json-rpc) · [der visuelle MCP Inspector](#c-test-it-visually-with-the-mcp-inspector) · [ein echter Client](#d-connect-a-real-mcp-client)
- [Optionaler vs. Standard-Build](#optional-vs-default-build)
- [Siehe auch](#see-also)

---

## Was MCP Ihnen bietet

Ein **Rustango**-MCP-Server stellt drei Dinge bereit, die ein Agent nutzen kann,
alle **von Hand registriert für explizite Kontrolle** (nichts wird automatisch
freigegeben):

| Primitiv | Was es ist | Wie es deklariert wird |
|---|---|---|
| **Tool** | eine Funktion, die der Agent aufruft (mit typisierten JSON-Argumenten) | `register_mcp_tool!` |
| **Resource** | lesbarer Inhalt, den der Agent per URI abruft | `register_mcp_resource!` + skill-gebunden |
| **Prompt** | eine wiederverwendbare Instruktionsvorlage | abgeleitet aus einem gewährten **Skill** |

Jeder Aufruf ist **pro Agent autorisiert**: Das JWT eines Agenten trägt die
**Skills** (und die Tools, die sie freischalten), die ihm gewährt wurden, und
`tools/list` / `tools/call` **schlagen fail-closed fehl** — ein Agent sieht oder
führt niemals ein Tool aus, das ihm nicht gewährt wurde.

---

## Schritt 1 — Das Feature aktivieren

MCP ist das optionale `mcp`-Feature (standardmäßig aus). Schalten Sie es ein:

```toml
# Cargo.toml
rustango = { version = "0.44", features = ["mcp"] }
```

Es zieht `tenancy` (Agenten/Skills), `sse` (den Benachrichtigungsstrom),
`serializer` + `openapi` (die Tool-Eingabeschemata) und `jwt` (Agenten-Tokens)
mit. Ein Build **ohne** das Feature kompiliert nichts vom MCP-Modul — siehe
[Optionaler vs. Standard-Build](#optional-vs-default-build).

---

## Schritt 2 — Ein Tool definieren

Ein Tool ist eine typisierte Eingabe-Struct + ein async Handler, zur
Kompilierzeit mit `register_mcp_tool!` registriert. Der Eingabetyp leitet
`serde::Deserialize` ab und implementiert `OpenApiSchema` (was zum veröffentlichten
JSON Schema des Tools wird):

```rust
use rustango::mcp::{McpContext, McpError};
use serde_json::json;

rustango::register_mcp_tool!(
    "add",
    "Add two integers",
    AddInput,
    |_ctx: McpContext, input: AddInput| async move {
        Ok::<_, McpError>(json!({ "sum": input.a + input.b }))
    },
);

#[derive(serde::Deserialize)]
struct AddInput { a: i64, b: i64 }

impl rustango::openapi::OpenApiSchema for AddInput {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::object()
            .property("a", rustango::openapi::Schema::integer())
            .property("b", rustango::openapi::Schema::integer())
            .required(["a", "b"])
    }
}
```

Der Handler erhält einen `McpContext { pool, agent, progress, cancel }` — den
Tenant-DB-Pool, den authentifizierten Agenten, einen Fortschrittsmelder und ein
Cancellation-Token — sodass ein Tool Ihre Modelle abfragen, den Fortschritt
langer Arbeit melden und bei Abbruch aussteigen kann. Geben Sie einen beliebigen
`serde_json::Value` zurück (er wird als `structuredContent` des Tools
präsentiert) oder einen `McpError`.

**Resources** sind statischer Inhalt, auf dieselbe Weise registriert:

```rust
rustango::register_mcp_resource!(
    "rustango://about", "About", "text/plain",
    || "This server exposes the demo tools.".to_string(),
);
```

**Prompts** stammen aus **Skills** (nächster Schritt) — die Instruktionen eines
Skills werden zu einem Prompt, den der Agent abrufen kann.

---

## Schritt 3 — Den Server mounten

Wählen Sie ein Mount passend zu Ihrem Deployment; alle geben einen `axum::Router`
zurück, den Sie unter einem Präfix verschachteln (konventionell `/mcp`):

| Mount | Tenancy | Auth | Verwenden für |
|---|---|---|---|
| `mcp::router(pool)` | single-tenant | keine | nur Transport (`initialize`/`ping`) |
| `mcp::tenant_router()` | multi-tenant | keine | nur Transport (Pool pro Anfrage) |
| `mcp::secure_tenant_router()` | multi-tenant | **Agenten-JWT** | die echte Sache |
| `mcp::secure_tenant_router_from_settings(&s)` | multi-tenant | Agenten-JWT | Produktion (CORS, Rate-Limit, SSE, Body-Cap aus `[mcp]`) |

Tools benötigen den **authentifizierten** Pfad (einen Agenten-Kontext), daher
verwenden Produktionsserver `secure_tenant_router*`:

```rust
use rustango::mcp;

let api = axum::Router::new()
    .nest("/mcp", mcp::secure_tenant_router_from_settings(&settings.mcp));
// hand `api` to your tenancy Cli/Builder as usual
```

Der authentifizierte Router mountet: `POST {prefix}` (JSON-RPC), `GET {prefix}`
(SSE-Benachrichtigungen), `POST {prefix}/token` (Credential → JWT),
`POST {prefix}/oauth/token` (OAuth 2.1) und die beiden `.well-known/*`
Discovery-Dokumente. Er signiert Agenten-Tokens mit `RUSTANGO_SESSION_SECRET`.

Der `initialize`-Handshake ist ein einfacher JSON-RPC-POST und funktioniert auf
jedem Mount:

```json
// → POST /mcp
{ "jsonrpc": "2.0", "id": 1, "method": "initialize",
  "params": { "protocolVersion": "2025-06-18", "capabilities": {},
              "clientInfo": { "name": "my-client", "version": "0" } } }

// ← 200
{ "jsonrpc": "2.0", "id": 1, "result": {
    "protocolVersion": "2025-06-18",
    "serverInfo": { "name": "rustango", "version": "0.44.0" },
    "capabilities": { "tools": { "listChanged": true }, "prompts": {}, "resources": {} } } }
```

---

## Schritt 4 — Agenten autorisieren

Die Autorisierung ist **skill-basiert und fail-closed**. Sie provisionieren einen
**Agenten** (der ein einmaliges Secret erhält), definieren einen **Skill**, der
Tools (und Resources/einen Prompt) bündelt, und **gewähren** dann den Skill dem
Agenten in einem Tenant:

```rust
use rustango::tenancy::{create_agent_pool, create_skill_pool, grant_skill_pool};

// 1. Provision an agent — returns a one-time `name`.`secret` credential.
let issued = create_agent_pool(&pool, "calc-bot").await?;

// 2. A skill bundles tools (here, the `add` tool) + a prompt body.
create_skill_pool(&pool, "calculator", "Calculator", "does arithmetic",
                  "You are a precise calculator.", &["add".into()]).await?;

// 3. Grant it to the agent in tenant "acme".
grant_skill_pool(&pool, "acme", "calc-bot", "calculator").await?;
```

Der Client tauscht sein Credential gegen ein **tenant-gepinntes, scoped JWT** an
`POST /mcp/token` (oder über den OAuth-`client_credentials`-Flow an
`/mcp/oauth/token`). Der Server löst die Gewährung in die `skills` + `tools`
Claims des Tokens auf; jede Anfrage verifiziert es erneut. Der Effekt, von Ende
zu Ende verifiziert:

```rust
// tools/list returns ONLY the granted tool, with its JSON Schema:
let listed = list_tools(&agent);                       // → { "tools": [ { "name": "add", … } ] }

// tools/call runs the handler and returns a structured result:
let out = call_tool(ctx, json!({ "name": "add", "arguments": { "a": 2, "b": 3 } })).await?;
assert_eq!(out["structuredContent"]["sum"], 5);

// An agent WITHOUT the grant sees an empty list and is refused:
//   list_tools(&ungranted) → { "tools": [] }
//   call_tool(ungranted, "add") → Err(code = TOOL_FORBIDDEN)
```

Tokens sind tenant-gepinnt: Ein für `acme` ausgestelltes Token wird gegen jeden
anderen Tenant abgelehnt (Cross-Tenant-Replay → 401). Widerrufen Sie einen
Agenten, und seine JTI wird auf die Blacklist gesetzt.

### Nutzereigene Keys (berechtigungsgesteuerte Capabilities)

Die obigen Agenten sind eigenständige Maschinenidentitäten. Ein Mitglied kann
stattdessen einen **persönlichen Key** generieren — einen nutzereigenen Agenten —
sodass ein LLM *in seinem Namen* handelt, mit Capabilities, die dem bestehenden
**RBAC** des Tenants folgen, statt einer auf den Key gepinnten Liste.

Zwei Teile verdrahten es:

```rust
use rustango::tenancy::{create_user_key_pool, create_skill_pool, map_skill_to_permission_pool};

// 1. Map a skill to a permission codename. Any user-owned key whose owner
//    holds `mcp.coach` is then granted this skill's tools + prompt + resources.
create_skill_pool(&pool, "coach", "Coach", "logs workouts",
                  "You are the member's coach.", &["log_set".into()]).await?;
map_skill_to_permission_pool(&pool, "coach", "mcp.coach").await?;

// 2. The member generates a key — a one-time `name`.`secret`, shown once.
//    `&[]` = a FULL key: everything the owner is entitled to. Pass skill
//    codenames to SCOPE the key to a single skill or a skillset instead —
//    always bounded by the owner's entitlement (you can't exceed your perms):
let issued = create_user_key_pool(&pool, user_id, "Alice's phone", &[]).await?;
// scoped: create_user_key_pool(&pool, user_id, "coach bot", &["coach".into()]).await?;
println!("copy once: {}", issued.token);
```

Bei der Token-Ausstellung ruft der Server
[`resolve_user_agent_grants_pool`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/src/tenancy/agents.rs) auf —
die effektiven Berechtigungen des Eigentümers (`user_permissions_pool`, d. h.
Rollen + direkte Gewährungen − Verweigerungen) wählen die gemappten Skills, deren
Tools/Prompts/Resources in die `skills`/`tools` Claims des JWT eingeebnet werden.
So sind `tools/list`, `tools/call`, `prompts/get` und `resources/read` **alle**
durch RBAC abgesichert, ohne Änderung an diesen Handlern. Der besitzende Nutzer
reist im `uid`-Claim des Tokens mit; ein Tool-Handler liest ihn als
`ctx.agent.user_id`, um die Arbeit auf dieses Mitglied zu beschränken. Widerrufen
Sie frische Capabilities, indem Sie die Berechtigungen des Nutzers ändern (sie
werden beim nächsten Token neu aufgelöst); widerrufen Sie den Key selbst mit
`revoke_user_key_pool(&pool, user_id, agent_id)`. Listen Sie die Keys eines
Mitglieds mit `list_user_keys_pool(&pool, user_id)` auf.

**Scope pro Key vs. Berechtigung pro Nutzer.** Skills erreichen einen Key entlang
zweier Achsen: die **Berechtigung** des Eigentümers (Superuser → jeder Skill;
sonst die einer von ihm gehaltenen Permission zugeordneten Skills) und der
**Scope** des Keys (bei der Erstellung gepinnte Skills). Ein Key ohne Scope
(`skills = &[]`) erhält die volle Berechtigung des Eigentümers; ein gescopeter Key
(`skills = &["coach", …]`) ist auf diese beschränkt. Die Auflösung schneidet den
Scope stets erneut mit der *aktuellen* Berechtigung — sodass ein Key niemals die
Berechtigungen des Eigentümers überschreiten kann, und der Verlust einer
Permission jeden Key bei der nächsten Ausstellung einengt. Ein Scope auf einen
Skill, zu dem der Eigentümer nicht berechtigt ist, wird bei der Erstellung
abgelehnt.

Eigenständige Agenten sind unberührt — ein Maschinenagent (`user_id = None`)
verwendet weiterhin nur seine expliziten `grant_skill_pool`-Gewährungen.

### Über die CLI (`manage`-Verben)

Alles Obige ist auch out of the box über den tenancy-bewussten `manage`-Dispatcher
verfügbar (diese Verben kompilieren mit dem `mcp`-Feature). Jedes ist
tenant-scoped und nimmt ein `<slug>`:

| Verb | Was es tut |
|---|---|
| `create-agent <slug> <name>` | Provisioniert einen Maschinenagenten; druckt sein `prefix.secret` **einmalig**. |
| `rotate-agent-secret <slug> <name>` | Stellt ein frisches Secret aus und macht das alte ungültig. |
| `list-agents <slug>` | Listet die Agenten eines Tenants (id, name, status, prefix). |
| `create-skill <slug> <codename> [--name ..] [--description ..] [--tools a,b] [--instructions ..]` | Definiert einen Skill (ein Bündel aus Tools + Prompt). |
| `grant-skill <slug> <agent> <skill>` | Gewährt einem Agenten einen Skill. |
| `revoke-skill <slug> <agent> <skill>` | Widerruft einem Agenten einen Skill. |
| `list-skills <slug>` | Listet die Skills eines Tenants. |
| `create-user-key <slug> <username> [--label <l>] [--skill <codename>]…` | Stellt einen **nutzereigenen Key** aus; druckt sein Token **einmalig**. Wiederholen Sie `--skill`, um den Key auf einen einzelnen Skill oder ein Skillset zu scopen; weglassen für einen vollständigen Key (Standard-Label = username). |
| `list-user-keys <slug> <username>` | Listet die persönlichen Keys eines Nutzers (id, label, created-at). |
| `revoke-user-key <slug> <username> <key_id>` | Widerruft einen der persönlichen Keys eines Nutzers per id (eigentumsgeprüft). |
| `map-skill-permission <slug> <skill> <permission>` | Mappt einen Skill auf einen Permission-Codename. Idempotent — jeder Nutzer-Key, dessen Eigentümer `<permission>` hält, gewinnt den Skill. |
| `unmap-skill-permission <slug> <skill> <permission>` | Entfernt ein Skill↔Permission-Mapping. |

Der **Permission → Skill → Tools** Fluss von Ende zu Ende:

```console
# 1. Define the skill and map it to a permission codename.
$ cargo run -- create-skill acme coach --tools log_set --instructions "You are the member's coach."
$ cargo run -- map-skill-permission acme coach mcp.coach

# 2. Grant the permission to the member (roles or direct — see grant-perm),
#    then issue their personal key.
$ cargo run -- grant-perm acme alice mcp.coach
$ cargo run -- create-user-key acme alice --label "Alice's phone"
created key #7 for user `alice` in tenant `acme` (label `Alice's phone`, scope full (owner's permissions))
  token: 3f9c1a2b.7d…            # copy once — never shown again
  store this safely — it won't be shown again

# …or scope the key to a single skill / skillset (repeat --skill):
$ cargo run -- create-user-key acme alice --label "coach bot" --skill coach
```

Alices Key löst nun bei jeder Token-Ausstellung die Tools des `coach`-Skills auf,
weil sie `mcp.coach` hält. Ändern Sie ihre Berechtigungen und die Capabilities
werden bei ihrem nächsten Token neu aufgelöst; widerrufen Sie den Key selbst mit
`revoke-user-key`. Dieselbe Key-id ist von Maschinenagenten im Tenant-Admin
unterscheidbar (die `Agent`-Liste zeigt `user_id`).

Der Auto-Admin stellt diese ebenfalls dar: `Agent`, `AgentSkill`,
`AgentSkillPermission` (und `AgentGrant`) rendern jeweils eine Auto-CRUD-Tabelle
im Tenant-Admin, sodass die Skill↔Permission-Mappings ohne die CLI überprüft und
bearbeitet werden können.

---

## Das Protokoll

JSON-RPC 2.0 (Protokollversion `2025-06-18`) über HTTP POST, mit einem optionalen
SSE-Strom (`GET {prefix}`) für Server→Client-Benachrichtigungen. Methoden:

| Methode | Auth | Zweck |
|---|---|---|
| `initialize` · `ping` | nein | Handshake + Liveness |
| `tools/list` · `tools/call` | ja | Tools entdecken + aufrufen (nur gewährte) |
| `prompts/list` · `prompts/get` | ja | skill-abgeleitete Prompts |
| `resources/list` · `resources/read` · `resources/templates/list` | ja | statische + Skill-Resources |
| `logging/setLevel` · `completion/complete` | ja | Log-Level + Präfix-Vervollständigung |
| `notifications/progress` · `notifications/*/list_changed` | — | Server→Client über SSE |
| `notifications/cancelled` | — | Client bricht einen laufenden Aufruf ab |

Ein fehlgeschlagener Tool-*Handler* gibt ein normales Ergebnis mit `isError: true`
zurück (der Agent kann reagieren); Probleme auf Protokollebene (unbekanntes/
verbotenes Tool, ungültige Params) geben einen JSON-RPC-`error` mit Codes wie
`-32002` (`TOOL_NOT_FOUND`), `-32003` (`TOOL_FORBIDDEN`), `-32602`
(`INVALID_PARAMS`) zurück. Lange Tools melden Fortschritt und honorieren
Abbrüche über den `McpContext`.

---

## Einstellungen

Der Abschnitt `[mcp]` (gelesen von `secure_tenant_router_from_settings`):

```toml
[mcp]
prefix                = "/mcp"   # URL prefix the router mounts under
token_ttl_secs        = 900      # agent access-token lifetime (15 min)
enable_sse            = true     # serve the GET {prefix} SSE stream
allowed_origins       = []       # CORS allow-list (empty = same-origin only)
rate_limit_per_minute = 0        # per-IP cap (0/unset = unlimited)
max_tools_listed      = 0        # tools/list page size (0/unset = unlimited)
```

---

## Wie man testet

### (a) Die Test-Suite

Das gesamte Protokoll wird von `crates/rustango/tests/mcp_*.rs` + dem
untermauernden Test der Doku abgedeckt. Führen Sie sie mit aktiviertem Feature
aus:

```bash
# The doc's headline flow (register → initialize → grant → list → call → fail-closed):
cargo test -p rustango --features sqlite,mcp --test mcp_doc

# Slices + end-to-end + OAuth + settings:
cargo test -p rustango --features sqlite,mcp,config --test 'mcp_*'
```

### (b) curl das JSON-RPC

Booten Sie die Demo (nächster Abschnitt) und sprechen Sie direkt mit ihr. Die
Demo sichert **jede** Methode hinter einem Agenten-Token (ein nicht
authentifizierter Aufruf gibt `401` zurück), also prägen Sie zuerst eines — die
Demo druckt das Agenten-Secret beim Booten:

```bash
TOKEN=$(curl -sX POST http://localhost:8090/mcp/token \
  -H 'content-type: application/json' -d '{"name":"demo-bot","secret":"<printed-secret>"}' \
  | jq -r .access_token)

# initialize:
curl -sX POST http://localhost:8090/mcp -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'

# tools/call — only the granted `add` tool is callable:
curl -sX POST http://localhost:8090/mcp -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":3}}}'
# → { ... "result": { "structuredContent": { "sum": 5 }, "isError": false } }
```

### (c) Testen Sie es visuell mit dem MCP Inspector

Der [MCP Inspector](https://github.com/modelcontextprotocol/inspector) ist der
offizielle visuelle Client — verbinden Sie ihn mit Ihrem Server und klicken Sie
sich durch Tools, Resources und Prompts. Starten Sie die Demo, dann den
Inspector:

```bash
# 1. Start the demo MCP server (seeds an `acme` tenant + `demo-bot` agent + the `add` tool):
cd crates/rustango/examples/mcp_demo && cargo run   # serves on http://localhost:8090/mcp

# 2. Launch the Inspector (opens a browser UI on http://localhost:6274):
npx @modelcontextprotocol/inspector
```

Im Inspector: Stellen Sie den Transport auf **Streamable HTTP** und die URL auf
`http://localhost:8090/mcp`. Öffnen Sie **Authentication → Custom Headers**, fügen
Sie einen Header `Authorization` mit dem Wert `Bearer <token>` hinzu (prägen Sie
das Token mit dem obigen `/mcp/token`-Aufruf), schalten Sie die Zeile ein, dann
**Connect**.

Wechseln Sie zum **Tools**-Tab und klicken Sie **List Tools** — Sie sehen *nur*
das `add`-Tool, das der Skill des Agenten gewährt, mit seinem JSON Schema. Wählen
Sie es aus, geben Sie `a = 2`, `b = 3` ein und **Run Tool**:

[![Der MCP Inspector über Streamable HTTP mit der Rustango-Demo verbunden, zeigt das gewährte `add`-Tool und sein a/b-Eingabeschema](../img/mcp-inspector-tools.png)](../img/mcp-inspector-tools.png)

Der Aufruf gibt ein strukturiertes Ergebnis zurück — `{ "sum": 5 }` — und die
Anfrage erscheint im History-Panel (`initialize` → `tools/list` → `tools/call`):

[![Derselbe Inspector nach der Tool-Ausführung: Tool Result Success mit strukturiertem Inhalt { sum: 5 } und die JSON-RPC-Aufrufhistorie](../img/mcp-inspector-call.png)](../img/mcp-inspector-call.png)

### (d) Einen echten MCP-Client verbinden

Richten Sie Claude Code (oder einen beliebigen MCP-Client) auf den laufenden
Server, indem Sie das Agenten-Token als Header übergeben (prägen Sie es mit dem
obigen `/mcp/token`-Aufruf):

```bash
claude mcp add --transport http rustango-demo http://localhost:8090/mcp \
  --header "Authorization: Bearer $TOKEN"
```

Bitten Sie dann den Agenten, zwei Zahlen zu addieren — er entdeckt und ruft das
`add`-Tool über dasselbe Protokoll auf, das der Inspector verwendet hat.

---

## Optionaler vs. Standard-Build

Das Feature ist vollständig gated — das gesamte `rustango::mcp`-Modul liegt
hinter `#[cfg(feature = "mcp")]`, sodass es niemals Apps betrifft, die sich nicht
einschreiben:

```bash
cargo build -p rustango                 # default — MCP module NOT compiled
cargo build -p rustango --features mcp  # MCP server compiled + linked
```

Eine Standard-App trägt keinerlei MCP-Code, -Abhängigkeiten oder -Routen; das
Aktivieren des Features ist das Einzige, was es einschaltet.

---

## Siehe auch

- [OpenAPI](openapi.md) — die JSON-Schema-Maschinerie, die die Eingabe eines
  Tools wiederverwendet.
- [JWT-Auth-API](auth-jwt-api.md) · [Auth-Backends](auth-backends.md) — der
  Token-Lebenszyklus, auf dem die Agenten-Auth aufgebaut ist.
- [Sicherheitsleitfaden](security.md) — fail-closed Autorisierung, Secrets,
  Rate-Limits.
- [Hintergrund-Jobs](jobs.md) — die Arbeit eines langen Tools außerhalb der
  Anfrage ausführen.
