# Serveur MCP

Le **Model Context Protocol (MCP)** est le standard ouvert qui permet à un agent
IA — Claude, un assistant d'IDE, votre propre application LLM — d'appeler en toute
sécurité les **tools** de *votre* application, de lire ses **resources** et
d'utiliser ses **prompts**. **Rustango** fournit un serveur MCP prêt pour la
production : enregistrez un tool avec une seule macro, montez un routeur, et
n'importe quel client MCP peut le découvrir et l'appeler via le transport
JSON-RPC standard — avec une autorisation **fail-closed** par agent et OAuth 2.1
intégrés.

[![Serveur MCP dans Rustango : un agent LLM se connecte via JSON-RPC + SSE ; le serveur authentifie le JWT de l'agent, ne liste que les tools que ses skills accordés autorisent, et exécute le handler du tool contre le pool de votre application](img/mcp.png)](img/mcp.png)

> **Nouveau ici ?** *MCP*, *JSON-RPC*, *tool/resource/prompt*, *agent*,
> *JWT*, *OAuth* — voir le [glossaire](glossary.md).

> **Source :** `rustango::mcp` (`router`, `tenant_router`, `secure_tenant_router`,
> `secure_tenant_router_from_settings`, `register_mcp_tool!`,
> `register_mcp_resource!`, `McpContext`, `issue_agent_token`) et les helpers
> agent/skill de `rustango::tenancy` — derrière la **feature `mcp`** (désactivée
> par défaut ; tire `tenancy, sse, serializer, openapi, jwt`).
>
> **Version exécutable :** chaque extrait est copié depuis
> [`mcp_doc.rs`](../crates/rustango/tests/mcp_doc.rs)
> (`cargo test -p rustango --features sqlite,mcp --test mcp_doc`) ; toute la
> surface protocolaire est testée en conditions réelles par la suite
> `crates/rustango/tests/mcp_*.rs`, et un serveur exécutable se trouve dans
> [`examples/mcp_demo`](../crates/rustango/examples/mcp_demo).

## Table des matières

- [Ce que MCP vous apporte](#what-mcp-gives-you)
- [Étape 1 — Activer la feature](#step-1--enable-the-feature)
- [Étape 2 — Définir un tool](#step-2--define-a-tool)
- [Étape 3 — Monter le serveur](#step-3--mount-the-server)
- [Étape 4 — Autoriser les agents](#step-4--authorize-agents)
- [Le protocole](#the-protocol)
- [Réglages](#settings)
- [Comment tester](#how-to-test) — [la suite](#a-the-test-suite) · [curl](#b-curl-the-json-rpc) · [le MCP Inspector visuel](#c-test-it-visually-with-the-mcp-inspector) · [un vrai client](#d-connect-a-real-mcp-client)
- [Build optionnel vs. par défaut](#optional-vs-default-build)
- [Voir aussi](#see-also)

---

## Ce que MCP vous apporte

Un serveur MCP **Rustango** expose trois choses qu'un agent peut utiliser, toutes
**enregistrées à la main pour un contrôle explicite** (rien n'est auto-exposé) :

| Primitive | Ce que c'est | Comment c'est déclaré |
|---|---|---|
| **Tool** | une fonction que l'agent appelle (avec des arguments JSON typés) | `register_mcp_tool!` |
| **Resource** | du contenu lisible que l'agent récupère par URI | `register_mcp_resource!` + attaché à un skill |
| **Prompt** | un modèle d'instruction réutilisable | dérivé d'un **skill** accordé |

Chaque appel est **autorisé par agent** : le JWT d'un agent porte les **skills**
(et les tools qu'ils débloquent) qui lui ont été accordés, et `tools/list` /
`tools/call` **échouent de manière fermée** — un agent ne voit jamais et
n'exécute jamais un tool qui ne lui a pas été accordé.

---

## Étape 1 — Activer la feature

MCP est la feature optionnelle `mcp` (désactivée par défaut). Activez-la :

```toml
# Cargo.toml
rustango = { version = "0.44", features = ["mcp"] }
```

Elle tire `tenancy` (agents/skills), `sse` (le flux de notifications),
`serializer` + `openapi` (les schémas d'entrée des tools) et `jwt` (les tokens
d'agent). Un build **sans** la feature ne compile aucune partie du module MCP —
voir [Build optionnel vs. par défaut](#optional-vs-default-build).

---

## Étape 2 — Définir un tool

Un tool est une struct d'entrée typée + un handler async, enregistré à la
compilation avec `register_mcp_tool!`. Le type d'entrée dérive
`serde::Deserialize` et implémente `OpenApiSchema` (qui devient le JSON Schema
publié du tool) :

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

Le handler reçoit un `McpContext { pool, agent, progress, cancel }` — le pool DB
du tenant, l'agent authentifié, un rapporteur de progression et un token
d'annulation — de sorte qu'un tool peut interroger vos modèles, rapporter la
progression d'un travail long et abandonner sur annulation. Retournez n'importe
quel `serde_json::Value` (il est exposé comme le `structuredContent` du tool) ou
un `McpError`.

Les **resources** sont du contenu statique enregistré de la même façon :

```rust
rustango::register_mcp_resource!(
    "rustango://about", "About", "text/plain",
    || "This server exposes the demo tools.".to_string(),
);
```

Les **prompts** proviennent des **skills** (étape suivante) — les instructions
d'un skill deviennent un prompt que l'agent peut récupérer.

---

## Étape 3 — Monter le serveur

Choisissez un montage adapté à votre déploiement ; tous retournent un
`axum::Router` que vous imbriquez sous un préfixe (conventionnellement `/mcp`) :

| Montage | Multi-tenancy | Auth | À utiliser pour |
|---|---|---|---|
| `mcp::router(pool)` | mono-tenant | aucune | transport seul (`initialize`/`ping`) |
| `mcp::tenant_router()` | multi-tenant | aucune | transport seul (pool par requête) |
| `mcp::secure_tenant_router()` | multi-tenant | **JWT d'agent** | la vraie affaire |
| `mcp::secure_tenant_router_from_settings(&s)` | multi-tenant | JWT d'agent | production (CORS, rate-limit, SSE, plafond de corps depuis `[mcp]`) |

Les tools requièrent le chemin **authentifié** (un contexte d'agent), donc les
serveurs de production utilisent `secure_tenant_router*` :

```rust
use rustango::mcp;

let api = axum::Router::new()
    .nest("/mcp", mcp::secure_tenant_router_from_settings(&settings.mcp));
// hand `api` to your tenancy Cli/Builder as usual
```

Le routeur authentifié monte : `POST {prefix}` (JSON-RPC), `GET {prefix}` (SSE
notifications), `POST {prefix}/token` (credential → JWT), `POST {prefix}/oauth/token`
(OAuth 2.1), et les deux documents de découverte `.well-known/*`. Il signe les
tokens d'agent avec `RUSTANGO_SESSION_SECRET`.

Le handshake `initialize` est un simple POST JSON-RPC et fonctionne sur
n'importe quel montage :

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

## Étape 4 — Autoriser les agents

L'autorisation est **basée sur les skills et fail-closed**. Vous provisionnez un
**agent** (qui reçoit un secret à usage unique), définissez un **skill** qui
regroupe des tools (et des resources/un prompt), puis **accordez** le skill à
l'agent dans un tenant :

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

Le client échange son credential contre un **JWT épinglé au tenant et à portée
limitée** à `POST /mcp/token` (ou via le flux OAuth `client_credentials` à
`/mcp/oauth/token`). Le serveur résout le grant en claims `skills` + `tools` du
token ; chaque requête le revérifie. L'effet, vérifié de bout en bout :

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

Les tokens sont épinglés au tenant : un token émis pour `acme` est rejeté contre
tout autre tenant (replay cross-tenant → 401). Révoquez un agent et son JTI est
mis sur liste noire.

### Clés détenues par l'utilisateur (capacités pilotées par permissions)

Les agents ci-dessus sont des identités machine autonomes. Un membre peut plutôt
générer une **clé personnelle** — un agent détenu par l'utilisateur — pour qu'un
LLM agisse *en son nom*, avec des capacités qui suivent le **RBAC** existant du
tenant plutôt qu'une liste épinglée sur la clé.

Deux pièces le câblent :

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

À l'émission du token, le serveur appelle
[`resolve_user_agent_grants_pool`](../crates/rustango/src/tenancy/agents.rs) —
les permissions effectives du propriétaire (`user_permissions_pool`, c.-à-d.
rôles + grants directs − refus) sélectionnent les skills mappés, dont les
tools/prompts/resources sont aplatis dans les claims `skills`/`tools` du JWT.
Ainsi `tools/list`, `tools/call`, `prompts/get` et `resources/read` sont **tous**
gardés par le RBAC, sans aucun changement à ces handlers. L'utilisateur
propriétaire voyage dans le claim `uid` du token ; un handler de tool le lit
comme `ctx.agent.user_id` pour restreindre le travail à ce membre. Révoquez de
nouvelles capacités en changeant les permissions de l'utilisateur (elles se
re-résolvent au prochain token) ; révoquez la clé elle-même avec
`revoke_user_key_pool(&pool, user_id, agent_id)`. Listez les clés d'un membre
avec `list_user_keys_pool(&pool, user_id)`.

**Portée par clé vs. droits par utilisateur.** Les skills atteignent une clé
selon deux axes : les **droits** du propriétaire (superuser → tous les skills ;
sinon les skills mappés à une permission qu'il détient) et la **portée** de la
clé (les skills épinglés à la création). Une clé sans portée (`skills = &[]`)
reçoit l'intégralité des droits du propriétaire ; une clé à portée limitée
(`skills = &["coach", …]`) est restreinte à ceux-ci. La résolution re-intersecte
toujours la portée avec les droits *courants* — de sorte qu'une clé ne peut
jamais dépasser les permissions du propriétaire, et perdre une permission
rétrécit chaque clé à la prochaine émission. Restreindre la portée à un skill
auquel le propriétaire n'a pas droit est refusé à la création.

Les agents autonomes ne sont pas affectés — un agent machine (`user_id = None`)
n'utilise toujours que ses grants explicites `grant_skill_pool`.

### Depuis la CLI (verbes `manage`)

Tout ce qui précède est aussi disponible directement via le dispatcher `manage`
conscient de la multi-tenancy (ces verbes se compilent avec la feature `mcp`).
Chacun est à portée de tenant et prend un `<slug>` :

| Verbe | Ce qu'il fait |
|---|---|
| `create-agent <slug> <name>` | Provisionne un agent machine ; imprime son `prefix.secret` **une seule fois**. |
| `rotate-agent-secret <slug> <name>` | Émet un secret frais, invalidant l'ancien. |
| `list-agents <slug>` | Liste les agents d'un tenant (id, name, status, prefix). |
| `create-skill <slug> <codename> [--name ..] [--description ..] [--tools a,b] [--instructions ..]` | Définit un skill (un regroupement de tools + prompt). |
| `grant-skill <slug> <agent> <skill>` | Accorde un skill à un agent. |
| `revoke-skill <slug> <agent> <skill>` | Révoque un skill d'un agent. |
| `list-skills <slug>` | Liste les skills d'un tenant. |
| `create-user-key <slug> <username> [--label <l>] [--skill <codename>]…` | Émet une **clé détenue par l'utilisateur** ; imprime son token **une seule fois**. Répétez `--skill` pour restreindre la portée de la clé à un seul skill ou à un ensemble de skills ; omettez pour une clé complète (label par défaut = username). |
| `list-user-keys <slug> <username>` | Liste les clés personnelles d'un utilisateur (id, label, created-at). |
| `revoke-user-key <slug> <username> <key_id>` | Révoque l'une des clés personnelles d'un utilisateur par id (propriété vérifiée). |
| `map-skill-permission <slug> <skill> <permission>` | Mappe un skill à un codename de permission. Idempotent — toute clé utilisateur dont le propriétaire détient `<permission>` gagne le skill. |
| `unmap-skill-permission <slug> <skill> <permission>` | Supprime un mapping skill↔permission. |

Le flux **permission → skill → tools** de bout en bout :

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

La clé d'Alice résout désormais les tools du skill `coach` à chaque émission de
token parce qu'elle détient `mcp.coach`. Changez ses permissions et les
capacités se re-résolvent à son prochain token ; révoquez la clé elle-même avec
`revoke-user-key`. Le même id de clé se distingue des agents machine dans l'admin
du tenant (la liste `Agent` affiche `user_id`).

L'auto-admin les expose aussi : `Agent`, `AgentSkill`, `AgentSkillPermission`
(et `AgentGrant`) rendent chacun une table auto-CRUD dans l'admin du tenant, de
sorte que les mappings skill↔permission peuvent être revus et édités sans la CLI.

---

## Le protocole

JSON-RPC 2.0 (version de protocole `2025-06-18`) sur HTTP POST, avec un flux SSE
optionnel (`GET {prefix}`) pour les notifications serveur→client. Méthodes :

| Méthode | Auth | But |
|---|---|---|
| `initialize` · `ping` | non | handshake + liveness |
| `tools/list` · `tools/call` | oui | découvrir + invoquer les tools (accordés seulement) |
| `prompts/list` · `prompts/get` | oui | prompts dérivés des skills |
| `resources/list` · `resources/read` · `resources/templates/list` | oui | resources statiques + de skill |
| `logging/setLevel` · `completion/complete` | oui | niveau de log + complétion de préfixe |
| `notifications/progress` · `notifications/*/list_changed` | — | serveur→client via SSE |
| `notifications/cancelled` | — | le client annule un appel en cours |

Un *handler* de tool en échec retourne un résultat normal avec `isError: true`
(l'agent peut réagir) ; les problèmes au niveau du protocole (tool
inconnu/interdit, mauvais params) retournent une `error` JSON-RPC avec des codes
comme `-32002` (`TOOL_NOT_FOUND`), `-32003` (`TOOL_FORBIDDEN`), `-32602`
(`INVALID_PARAMS`). Les tools longs rapportent la progression et honorent
l'annulation via le `McpContext`.

---

## Réglages

La section `[mcp]` (lue par `secure_tenant_router_from_settings`) :

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

## Comment tester

### (a) La suite de tests

Tout le protocole est couvert par `crates/rustango/tests/mcp_*.rs` + le test qui
adosse cette doc. Exécutez-les avec la feature activée :

```bash
# The doc's headline flow (register → initialize → grant → list → call → fail-closed):
cargo test -p rustango --features sqlite,mcp --test mcp_doc

# Slices + end-to-end + OAuth + settings:
cargo test -p rustango --features sqlite,mcp,config --test 'mcp_*'
```

### (b) curl le JSON-RPC

Démarrez la démo (section suivante) et parlez-lui directement. La démo garde
**chaque** méthode derrière un token d'agent (un appel non authentifié retourne
`401`), donc émettez-en un d'abord — la démo imprime le secret de l'agent au
démarrage :

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

### (c) Testez-le visuellement avec le MCP Inspector

Le [MCP Inspector](https://github.com/modelcontextprotocol/inspector) est le
client visuel officiel — connectez-le à votre serveur et parcourez les tools,
resources et prompts. Lancez la démo, puis l'Inspector :

```bash
# 1. Start the demo MCP server (seeds an `acme` tenant + `demo-bot` agent + the `add` tool):
cd crates/rustango/examples/mcp_demo && cargo run   # serves on http://localhost:8090/mcp

# 2. Launch the Inspector (opens a browser UI on http://localhost:6274):
npx @modelcontextprotocol/inspector
```

Dans l'Inspector : réglez le transport sur **Streamable HTTP** et l'URL sur
`http://localhost:8090/mcp`. Ouvrez **Authentication → Custom Headers**, ajoutez
un header `Authorization` avec la valeur `Bearer <token>` (émettez le token avec
l'appel `/mcp/token` ci-dessus), activez la ligne, puis **Connect**.

Passez à l'onglet **Tools** et cliquez **List Tools** — vous verrez *seulement*
le tool `add` que le skill de l'agent accorde, avec son JSON Schema.
Sélectionnez-le, entrez `a = 2`, `b = 3`, et **Run Tool** :

[![Le MCP Inspector connecté à la démo Rustango via Streamable HTTP, montrant le tool `add` accordé et son schéma d'entrée a/b](img/mcp-inspector-tools.png)](img/mcp-inspector-tools.png)

L'appel retourne un résultat structuré — `{ "sum": 5 }` — et la requête apparaît
dans le panneau History (`initialize` → `tools/list` → `tools/call`) :

[![Le même Inspector après l'exécution du tool : Tool Result Success avec le contenu structuré { sum: 5 }, et l'historique des appels JSON-RPC](img/mcp-inspector-call.png)](img/mcp-inspector-call.png)

### (d) Connectez un vrai client MCP

Pointez Claude Code (ou n'importe quel client MCP) vers le serveur en cours
d'exécution, en passant le token d'agent comme header (émettez-le avec l'appel
`/mcp/token` ci-dessus) :

```bash
claude mcp add --transport http rustango-demo http://localhost:8090/mcp \
  --header "Authorization: Bearer $TOKEN"
```

Puis demandez à l'agent d'additionner deux nombres — il découvre et appelle le
tool `add` via le même protocole que celui utilisé par l'Inspector.

---

## Build optionnel vs. par défaut

La feature est entièrement gardée — tout le module `rustango::mcp` est derrière
`#[cfg(feature = "mcp")]`, de sorte qu'il n'affecte jamais les apps qui ne s'y
inscrivent pas :

```bash
cargo build -p rustango                 # default — MCP module NOT compiled
cargo build -p rustango --features mcp  # MCP server compiled + linked
```

Une app par défaut ne porte aucun code, dépendance ou route MCP ; activer la
feature est la seule chose qui l'active.

---

## Voir aussi

- [OpenAPI](openapi.md) — la machinerie JSON Schema que l'entrée d'un tool
  réutilise.
- [API d'auth JWT](auth-jwt-api.md) · [Backends d'auth](auth-backends.md) — le
  cycle de vie des tokens sur lequel l'auth d'agent est construite.
- [Guide de sécurité](security.md) — autorisation fail-closed, secrets, limites
  de débit.
- [Jobs en arrière-plan](jobs.md) — exécuter le travail d'un tool long hors de la
  requête.
