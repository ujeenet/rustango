# Servidor MCP

El **Model Context Protocol (MCP)** es el estándar abierto que permite a un agente
de IA — Claude, un asistente de IDE, tu propia app LLM — llamar de forma segura a
las **tools** de *tu* aplicación, leer sus **resources** y usar sus **prompts**.
**Rustango** incluye un servidor MCP listo para producción: registra una tool con
una sola macro, monta un router, y cualquier cliente MCP puede descubrirla y
llamarla sobre el transporte JSON-RPC estándar — con autorización **fail-closed**
por agente y OAuth 2.1 integrados.

[![Servidor MCP en Rustango: un agente LLM se conecta sobre JSON-RPC + SSE; el servidor autentica el JWT del agente, lista solo las tools que sus skills concedidos permiten, y ejecuta el handler de la tool contra el pool de tu app](img/mcp.png)](img/mcp.png)

> **¿Nuevo con algún término aquí?** *MCP*, *JSON-RPC*, *tool/resource/prompt*,
> *agente*, *JWT*, *OAuth* — consulta el [glosario](glossary.md).

> **Fuente:** `rustango::mcp` (`router`, `tenant_router`, `secure_tenant_router`,
> `secure_tenant_router_from_settings`, `register_mcp_tool!`,
> `register_mcp_resource!`, `McpContext`, `issue_agent_token`) y los helpers de
> agente/skill de `rustango::tenancy` — detrás del **feature `mcp`** (DESACTIVADO
> por defecto; arrastra `tenancy, sse, serializer, openapi, jwt`).
>
> **Versión ejecutable:** cada fragmento está copiado de
> [`mcp_doc.rs`](../crates/rustango/tests/mcp_doc.rs)
> (`cargo test -p rustango --features sqlite,mcp --test mcp_doc`); toda la
> superficie del protocolo se prueba en condiciones reales con la suite
> `crates/rustango/tests/mcp_*.rs`, y un servidor ejecutable vive en
> [`examples/mcp_demo`](../crates/rustango/examples/mcp_demo).

## Tabla de contenidos

- [Qué te aporta MCP](#what-mcp-gives-you)
- [Paso 1 — Activar el feature](#step-1--enable-the-feature)
- [Paso 2 — Definir una tool](#step-2--define-a-tool)
- [Paso 3 — Montar el servidor](#step-3--mount-the-server)
- [Paso 4 — Autorizar agentes](#step-4--authorize-agents)
- [El protocolo](#the-protocol)
- [Ajustes](#settings)
- [Cómo probar](#how-to-test) — [la suite](#a-the-test-suite) · [curl](#b-curl-the-json-rpc) · [el MCP Inspector visual](#c-test-it-visually-with-the-mcp-inspector) · [un cliente real](#d-connect-a-real-mcp-client)
- [Build opcional vs. por defecto](#optional-vs-default-build)
- [Véase también](#see-also)

---

## Qué te aporta MCP

Un servidor MCP de **Rustango** expone tres cosas que un agente puede usar, todas
**registradas a mano para un control explícito** (nada se auto-expone):

| Primitiva | Qué es | Cómo se declara |
|---|---|---|
| **Tool** | una función que el agente llama (con argumentos JSON tipados) | `register_mcp_tool!` |
| **Resource** | contenido legible que el agente obtiene por URI | `register_mcp_resource!` + adjunto a un skill |
| **Prompt** | una plantilla de instrucción reutilizable | derivada de un **skill** concedido |

Cada llamada está **autorizada por agente**: el JWT de un agente lleva los
**skills** (y las tools que desbloquean) que se le concedieron, y `tools/list` /
`tools/call` **fallan de forma cerrada** — un agente nunca ve ni ejecuta una tool
que no se le concedió.

---

## Paso 1 — Activar el feature

MCP es el feature opcional `mcp` (desactivado por defecto). Actívalo:

```toml
# Cargo.toml
rustango = { version = "0.44", features = ["mcp"] }
```

Arrastra `tenancy` (agentes/skills), `sse` (el flujo de notificaciones),
`serializer` + `openapi` (los esquemas de entrada de las tools) y `jwt` (los
tokens de agente). Un build **sin** el feature no compila nada del módulo MCP —
consulta [Build opcional vs. por defecto](#optional-vs-default-build).

---

## Paso 2 — Definir una tool

Una tool es una struct de entrada tipada + un handler async, registrado en tiempo
de compilación con `register_mcp_tool!`. El tipo de entrada deriva
`serde::Deserialize` e implementa `OpenApiSchema` (que se convierte en el JSON
Schema publicado de la tool):

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

El handler recibe un `McpContext { pool, agent, progress, cancel }` — el pool de
BD del tenant, el agente autenticado, un reportero de progreso y un token de
cancelación — de modo que una tool puede consultar tus modelos, reportar el
progreso de un trabajo largo y abortar al cancelar. Devuelve cualquier
`serde_json::Value` (se presenta como el `structuredContent` de la tool) o un
`McpError`.

Los **resources** son contenido estático registrado del mismo modo:

```rust
rustango::register_mcp_resource!(
    "rustango://about", "About", "text/plain",
    || "This server exposes the demo tools.".to_string(),
);
```

Los **prompts** provienen de los **skills** (siguiente paso) — las instrucciones
de un skill se convierten en un prompt que el agente puede obtener.

---

## Paso 3 — Montar el servidor

Elige un montaje acorde a tu despliegue; todos devuelven un `axum::Router` que
anidas bajo un prefijo (por convención `/mcp`):

| Montaje | Tenancy | Auth | Usar para |
|---|---|---|---|
| `mcp::router(pool)` | single-tenant | ninguna | solo transporte (`initialize`/`ping`) |
| `mcp::tenant_router()` | multi-tenant | ninguna | solo transporte (pool por petición) |
| `mcp::secure_tenant_router()` | multi-tenant | **JWT de agente** | lo de verdad |
| `mcp::secure_tenant_router_from_settings(&s)` | multi-tenant | JWT de agente | producción (CORS, rate-limit, SSE, tope de cuerpo desde `[mcp]`) |

Las tools requieren la ruta **autenticada** (un contexto de agente), así que los
servidores de producción usan `secure_tenant_router*`:

```rust
use rustango::mcp;

let api = axum::Router::new()
    .nest("/mcp", mcp::secure_tenant_router_from_settings(&settings.mcp));
// hand `api` to your tenancy Cli/Builder as usual
```

El router autenticado monta: `POST {prefix}` (JSON-RPC), `GET {prefix}`
(notificaciones SSE), `POST {prefix}/token` (credencial → JWT),
`POST {prefix}/oauth/token` (OAuth 2.1), y los dos documentos de descubrimiento
`.well-known/*`. Firma los tokens de agente con `RUSTANGO_SESSION_SECRET`.

El handshake `initialize` es un simple POST JSON-RPC y funciona en cualquier
montaje:

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

## Paso 4 — Autorizar agentes

La autorización es **basada en skills y fail-closed**. Aprovisionas un **agente**
(que obtiene un secreto de un solo uso), defines un **skill** que agrupa tools (y
resources/un prompt), y luego **concedes** el skill al agente en un tenant:

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

El cliente intercambia su credencial por un **JWT fijado al tenant y con alcance
limitado** en `POST /mcp/token` (o mediante el flujo OAuth `client_credentials` en
`/mcp/oauth/token`). El servidor resuelve la concesión en los claims `skills` +
`tools` del token; cada petición lo reverifica. El efecto, verificado de extremo
a extremo:

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

Los tokens están fijados al tenant: un token emitido para `acme` se rechaza contra
cualquier otro tenant (replay entre tenants → 401). Revoca un agente y su JTI
queda en la lista negra.

### Claves propiedad del usuario (capacidades dirigidas por permisos)

Los agentes anteriores son identidades de máquina independientes. Un miembro puede
en cambio generar una **clave personal** — un agente propiedad del usuario — para
que un LLM actúe *en su nombre*, con capacidades que siguen el **RBAC** existente
del tenant en lugar de una lista fijada en la clave.

Dos piezas lo cablean:

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

En la emisión del token, el servidor llama a
[`resolve_user_agent_grants_pool`](../crates/rustango/src/tenancy/agents.rs) —
los permisos efectivos del propietario (`user_permissions_pool`, es decir, roles +
concesiones directas − denegaciones) seleccionan los skills mapeados, cuyos
tools/prompts/resources se aplanan en los claims `skills`/`tools` del JWT. Así
`tools/list`, `tools/call`, `prompts/get` y `resources/read` están **todos**
protegidos por RBAC, sin cambio alguno en esos handlers. El usuario propietario
viaja en el claim `uid` del token; un handler de tool lo lee como
`ctx.agent.user_id` para acotar el trabajo a ese miembro. Revoca capacidades
recién otorgadas cambiando los permisos del usuario (se re-resuelven en el
siguiente token); revoca la clave misma con
`revoke_user_key_pool(&pool, user_id, agent_id)`. Lista las claves de un miembro
con `list_user_keys_pool(&pool, user_id)`.

**Alcance por clave vs. derechos por usuario.** Los skills alcanzan una clave por
dos ejes: los **derechos** del propietario (superusuario → todos los skills; de lo
contrario los skills mapeados a un permiso que posee) y el **alcance** de la clave
(los skills fijados en la creación). Una clave sin alcance (`skills = &[]`) obtiene
la totalidad de los derechos del propietario; una clave con alcance
(`skills = &["coach", …]`) se limita a esos. La resolución siempre reintersecta el
alcance con los derechos *actuales* — de modo que una clave nunca puede exceder
los permisos del propietario, y perder un permiso estrecha cada clave en la
siguiente emisión. Acotar a un skill al que el propietario no tiene derecho se
rechaza en la creación.

Los agentes independientes no se ven afectados — un agente de máquina
(`user_id = None`) sigue usando solo sus concesiones explícitas
`grant_skill_pool`.

### Desde la CLI (verbos `manage`)

Todo lo anterior también está disponible de fábrica a través del dispatcher
`manage` consciente de la multi-tenancy (estos verbos se compilan con el feature
`mcp`). Cada uno tiene alcance de tenant y toma un `<slug>`:

| Verbo | Qué hace |
|---|---|
| `create-agent <slug> <name>` | Aprovisiona un agente de máquina; imprime su `prefix.secret` **una sola vez**. |
| `rotate-agent-secret <slug> <name>` | Emite un secreto nuevo, invalidando el anterior. |
| `list-agents <slug>` | Lista los agentes de un tenant (id, name, status, prefix). |
| `create-skill <slug> <codename> [--name ..] [--description ..] [--tools a,b] [--instructions ..]` | Define un skill (un paquete de tools + prompt). |
| `grant-skill <slug> <agent> <skill>` | Concede un skill a un agente. |
| `revoke-skill <slug> <agent> <skill>` | Revoca un skill a un agente. |
| `list-skills <slug>` | Lista los skills de un tenant. |
| `create-user-key <slug> <username> [--label <l>] [--skill <codename>]…` | Emite una **clave propiedad del usuario**; imprime su token **una sola vez**. Repite `--skill` para acotar la clave a un solo skill o a un conjunto de skills; omítelo para una clave completa (label por defecto = username). |
| `list-user-keys <slug> <username>` | Lista las claves personales de un usuario (id, label, created-at). |
| `revoke-user-key <slug> <username> <key_id>` | Revoca una de las claves personales de un usuario por id (propiedad verificada). |
| `map-skill-permission <slug> <skill> <permission>` | Mapea un skill a un codename de permiso. Idempotente — cualquier clave de usuario cuyo propietario posea `<permission>` gana el skill. |
| `unmap-skill-permission <slug> <skill> <permission>` | Elimina un mapeo skill↔permission. |

El flujo **permiso → skill → tools** de extremo a extremo:

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

La clave de Alice ahora resuelve las tools del skill `coach` en cada emisión de
token porque ella posee `mcp.coach`. Cambia sus permisos y las capacidades se
re-resuelven en su siguiente token; revoca la clave misma con `revoke-user-key`.
El mismo id de clave se distingue de los agentes de máquina en el admin del tenant
(la lista `Agent` muestra `user_id`).

El auto-admin también los expone: `Agent`, `AgentSkill`, `AgentSkillPermission`
(y `AgentGrant`) renderizan cada uno una tabla auto-CRUD en el admin del tenant,
de modo que los mapeos skill↔permission pueden revisarse y editarse sin la CLI.

---

## El protocolo

JSON-RPC 2.0 (versión de protocolo `2025-06-18`) sobre HTTP POST, con un flujo SSE
opcional (`GET {prefix}`) para notificaciones servidor→cliente. Métodos:

| Método | Auth | Propósito |
|---|---|---|
| `initialize` · `ping` | no | handshake + liveness |
| `tools/list` · `tools/call` | sí | descubrir + invocar tools (solo las concedidas) |
| `prompts/list` · `prompts/get` | sí | prompts derivados de skills |
| `resources/list` · `resources/read` · `resources/templates/list` | sí | resources estáticos + de skill |
| `logging/setLevel` · `completion/complete` | sí | nivel de log + completado de prefijo |
| `notifications/progress` · `notifications/*/list_changed` | — | servidor→cliente sobre SSE |
| `notifications/cancelled` | — | el cliente cancela una llamada en curso |

Un *handler* de tool fallido devuelve un resultado normal con `isError: true` (el
agente puede reaccionar); los problemas a nivel de protocolo (tool
desconocida/prohibida, params inválidos) devuelven un `error` JSON-RPC con códigos
como `-32002` (`TOOL_NOT_FOUND`), `-32003` (`TOOL_FORBIDDEN`), `-32602`
(`INVALID_PARAMS`). Las tools largas reportan progreso y honran la cancelación a
través del `McpContext`.

---

## Ajustes

La sección `[mcp]` (leída por `secure_tenant_router_from_settings`):

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

## Cómo probar

### (a) La suite de tests

Todo el protocolo está cubierto por `crates/rustango/tests/mcp_*.rs` + el test que
respalda esta doc. Ejecútalos con el feature activado:

```bash
# The doc's headline flow (register → initialize → grant → list → call → fail-closed):
cargo test -p rustango --features sqlite,mcp --test mcp_doc

# Slices + end-to-end + OAuth + settings:
cargo test -p rustango --features sqlite,mcp,config --test 'mcp_*'
```

### (b) curl al JSON-RPC

Arranca la demo (siguiente sección) y háblale directamente. La demo protege
**cada** método detrás de un token de agente (una llamada sin autenticar devuelve
`401`), así que emite uno primero — la demo imprime el secreto del agente al
arrancar:

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

### (c) Pruébalo visualmente con el MCP Inspector

El [MCP Inspector](https://github.com/modelcontextprotocol/inspector) es el
cliente visual oficial — conéctalo a tu servidor y navega por tools, resources y
prompts. Ejecuta la demo, luego el Inspector:

```bash
# 1. Start the demo MCP server (seeds an `acme` tenant + `demo-bot` agent + the `add` tool):
cd crates/rustango/examples/mcp_demo && cargo run   # serves on http://localhost:8090/mcp

# 2. Launch the Inspector (opens a browser UI on http://localhost:6274):
npx @modelcontextprotocol/inspector
```

En el Inspector: pon el transporte en **Streamable HTTP** y la URL en
`http://localhost:8090/mcp`. Abre **Authentication → Custom Headers**, añade un
header `Authorization` con valor `Bearer <token>` (emite el token con la llamada
`/mcp/token` de arriba), activa la fila, luego **Connect**.

Cambia a la pestaña **Tools** y haz clic en **List Tools** — verás *solo* la tool
`add` que concede el skill del agente, con su JSON Schema. Selecciónala, introduce
`a = 2`, `b = 3`, y **Run Tool**:

[![El MCP Inspector conectado a la demo de Rustango sobre Streamable HTTP, mostrando la tool `add` concedida y su esquema de entrada a/b](img/mcp-inspector-tools.png)](img/mcp-inspector-tools.png)

La llamada devuelve un resultado estructurado — `{ "sum": 5 }` — y la petición
aparece en el panel History (`initialize` → `tools/list` → `tools/call`):

[![El mismo Inspector tras ejecutar la tool: Tool Result Success con contenido estructurado { sum: 5 }, y el historial de llamadas JSON-RPC](img/mcp-inspector-call.png)](img/mcp-inspector-call.png)

### (d) Conecta un cliente MCP real

Apunta Claude Code (o cualquier cliente MCP) al servidor en ejecución, pasando el
token de agente como header (emítelo con la llamada `/mcp/token` de arriba):

```bash
claude mcp add --transport http rustango-demo http://localhost:8090/mcp \
  --header "Authorization: Bearer $TOKEN"
```

Luego pídele al agente que sume dos números — descubre y llama a la tool `add`
sobre el mismo protocolo que usó el Inspector.

---

## Build opcional vs. por defecto

El feature está totalmente gated — todo el módulo `rustango::mcp` está detrás de
`#[cfg(feature = "mcp")]`, de modo que nunca afecta a apps que no se inscriben:

```bash
cargo build -p rustango                 # default — MCP module NOT compiled
cargo build -p rustango --features mcp  # MCP server compiled + linked
```

Una app por defecto no lleva nada de código, dependencias ni rutas de MCP;
activar el feature es lo único que lo enciende.

---

## Véase también

- [OpenAPI](openapi.md) — la maquinaria de JSON Schema que reutiliza la entrada de
  una tool.
- [API de auth JWT](auth-jwt-api.md) · [Backends de auth](auth-backends.md) — el
  ciclo de vida de tokens sobre el que se construye la auth de agente.
- [Guía de seguridad](security.md) — autorización fail-closed, secretos, límites
  de tasa.
- [Jobs en segundo plano](jobs.md) — ejecutar el trabajo de una tool larga fuera
  de la petición.
