# Journalisation

Les journaux sont la façon dont une application en cours d'exécution raconte ce
qu'elle a fait. **Rustango** s'appuie sur [`tracing`](https://docs.rs/tracing) —
la même forme que le réglage `LOGGING` de Django ou les canaux de Laravel, mais
structurée : un événement porte des champs nommés (`status=500`, `tenant=acme`)
plutôt qu'une phrase déjà formatée, si bien qu'un agrégateur de logs peut
filtrer dessus.

Un projet généré par le scaffolder journalise déjà. Cette page traite de ce que
vous réglez : quel niveau, quels sous-systèmes, quel format, où va la sortie, et
comment distinguer le trafic d'un locataire de celui d'un autre.

> **Nouveau sur **Rustango** ?** Le [glossaire](glossary.md) couvre les briques
> du framework. Le vocabulaire de la journalisation — *niveau*, *target*, *span*
> — est défini sur cette page au fil de son apparition.

> **Source :** `rustango::logging` (`setup`, `setup_for_env`, `Setup`,
> `Rotation`, `DEFAULT_FILTER`) — le module n'est pas derrière une feature, mais
> chaque installateur requiert la feature `runtime`. `Setup::from_settings`
> requiert en plus `config`. `rustango::access_log` et `rustango::tenant_log`
> requièrent `admin` **ou** `tenancy` ; `rustango::tracing_layer` requiert
> `admin`.
>
> **Version exécutable :** les réglages et valeurs par défaut présentés ici sont
> figés par
> [`logging_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_doc.rs)
> (`cargo test -p rustango --test logging_doc`), le puits fichier par
> [`logging_file_appender_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_file_appender_live.rs),
> et le champ locataire du journal d'accès par
> [`access_log_tenant_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/access_log_tenant_sqlite_live.rs).

## Table des matières

- [Ce que vous avez déjà](#ce-que-vous-avez-déjà)
- [Les niveaux, et le filtre qui les choisit](#les-niveaux-et-le-filtre-qui-les-choisit)
- [Targets : nommer le sous-système](#targets--nommer-le-sous-système)
- [Choisir un format](#choisir-un-format)
- [Configurer la journalisation depuis les réglages](#configurer-la-journalisation-depuis-les-réglages)
- [Écrire dans un fichier](#écrire-dans-un-fichier)
- [Le journal d'accès](#le-journal-daccès)
- [C'était quel locataire ?](#cétait-quel-locataire-)
- [Spans de requête et OpenTelemetry](#spans-de-requête-et-opentelemetry)
- [Journalisation dans les tests](#journalisation-dans-les-tests)
- [Rien ne sort](#rien-ne-sort)

---

## Ce que vous avez déjà

`#[rustango::main]` installe un subscriber avant que votre code ne tourne. Un
projet généré l'obtient gratuitement, et c'est pourquoi `cargo run` affiche des
logs sans le moindre appel de configuration :

```rust,ignore
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Un subscriber `tracing_subscriber::fmt` est déjà installé ici.
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

Il utilise `RUST_LOG` quand elle est définie, et `info,sqlx=warn` sinon — la
valeur de `rustango::logging::DEFAULT_FILTER`. Ce défaut est délibéré : sqlx
journalise chaque requête en `info`, donc un `info` non filtré enterre vos
propres événements sous le SQL.

Pour configurer autre chose que le niveau, installez le subscriber vous-même.
Chaque installateur est idempotent (`try_init` en dessous), un appel
supplémentaire est donc sans effet plutôt qu'une panique :

```rust,ignore
fn main() {
    rustango::logging::setup();   // pretty, filtre d'environnement, "info,sqlx=warn"
    // ...
}
```

## Les niveaux, et le filtre qui les choisit

Cinq niveaux, du plus bruyant au plus discret : `error`, `warn`, `info`,
`debug`, `trace`. Un filtre nomme le niveau de détail maximal souhaité,
globalement ou par module :

```sh
RUST_LOG=info                                  # tout à partir de info
RUST_LOG=debug,sqlx=warn,hyper=warn            # debug pour vous, dépendances silencieuses
RUST_LOG=warn,rustango::tenancy=debug          # un sous-système, à fond
RUST_LOG=rustango=info                         # seulement le framework
```

Définissez la valeur de repli pour les cas où `RUST_LOG` est absente — c'est la
plupart des déploiements en production, où le filtre relève de la configuration
et non de l'environnement :

```rust,ignore
rustango::logging::Setup::new()
    .with_default_env_filter("info,sqlx=warn,hyper=warn")
    .install();
```

`RUST_LOG` l'emporte toujours sur ce repli. Il n'existe volontairement aucun
moyen de configurer un filtre que l'environnement ne puisse pas écraser — qui
débogue un incident en cours ne devrait pas avoir à livrer un build.

**Quel niveau attendre et où.** Les événements `warn` du framework partagent
souvent une forme qui mérite une alerte : *rustango a fait autre chose que ce
que vous avez demandé*. Un `format` inconnu dans vos réglages, une clause
d'index abandonnée parce que le backend ne sait pas l'exprimer, une route admin
en collision et donc ignorée, un appel `update` avec une liste de champs vide —
chacun continue, et le `warn` est le seul signe que c'est arrivé.

## Targets : nommer le sous-système

Chaque événement porte un **target**, et c'est là-dessus que
`RUST_LOG=<target>=<niveau>` s'applique. Les événements du framework vivent sous
la racine `rustango::`, donc `RUST_LOG=rustango=warn` les atteint tous :

| Target | Ce que ça couvre |
|---|---|
| `rustango::admin` | Routage et enregistrement de l'admin |
| `rustango::admin::audit` | Écritures du journal d'audit |
| `rustango::admin::sso` | SSO de l'admin |
| `rustango::cache` | Backends de cache |
| `rustango::cache_page` | Middleware de cache de page |
| `rustango::cors` | Décisions de politique CORS |
| `rustango::email` | Envoi de courrier |
| `rustango::email::smtp` | Transport SMTP |
| `rustango::humanize` | Filtres humanize |
| `rustango::jobs` | Files de tâches de fond |
| `rustango::logging` | Avertissements de ce sous-système lui-même |
| `rustango::manage` | Verbes `manage` |
| `rustango::media::auth` | Refus d'autorisation du routeur média |
| `rustango::messages` | Messages flash |
| `rustango::migrate` | Exécuteur de migrations |
| `rustango::rate_limit` | Limitation de débit |
| `rustango::request_timeout` | Délai par requête |
| `rustango::scheduler` | Cron / tâches planifiées |
| `rustango::server` | Démarrage et arrêt du serveur |
| `rustango::shutdown` | Gestion des signaux et hooks d'arrêt |
| `rustango::sql` | Exécution des requêtes |
| `rustango::sql::lock` | Clauses de verrou de ligne |
| `rustango::template_views` | Vues adossées à des templates |
| `rustango::tenancy` | Multi-tenancy, général |
| `rustango::tenancy::admin` | Admin de locataire |
| `rustango::tenancy::migrate_run` | Migrations par locataire |
| `rustango::tenancy::operator_console` | Console opérateur |
| `rustango::tenancy::pools` | Cycle de vie des pools de locataire |
| `rustango::tenancy::provision` | Provisionnement de locataires |
| `rustango::tenancy::provision_webhook` | Webhooks de provisionnement |
| `rustango::tenancy::resolver` | Résolution de locataire |
| `rustango::tenancy::sso` | SSO de locataire |
| `rustango::tenancy::sweep` | Balayages de rétention |

Ce sont les targets que le framework nomme explicitement. Les événements qui
n'en nomment aucun héritent de leur chemin de module, ce qui donne la même forme
— `rustango::access_log` et `rustango::tracing_layer` s'atteignent exactement
comme les lignes ci-dessus.

> **Un target est une chaîne, pas un chemin.** `target: "crate::cache"` compile
> sans broncher puis se retrouve dans un espace de noms qu'aucun filtre ne
> touche. Quarante-huit sites d'appel avaient dérivé ainsi avant que
> [`tracing_targets.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/tracing_targets.rs)
> ne commence à faire échouer le build là-dessus. Si vous ajoutez des targets
> dans votre propre code, utilisez le vrai nom de votre crate.

## Choisir un format

| Format | Pour quoi | Comment |
|---|---|---|
| `pretty` | Développement — couleur, multiligne, lisible | par défaut |
| `json` | Production — un objet par événement, pour Loki / CloudWatch / Datadog | `.json()` |

```rust,ignore
rustango::logging::Setup::new()
    .json()
    .with_default_env_filter("info")
    .install();
```

Ou laissez le palier décider. `setup_for_env()` lit `RUSTANGO_ENV` et choisit
JSON quand la valeur est `prod` ou `production`, pretty sinon :

```rust,ignore
rustango::logging::setup_for_env();
```

Deux réglages d'affichage méritent d'être connus : `.with_line_numbers()` ajoute
la position dans le code (utile en développement, bruyant en production), et
`.without_targets()` masque la colonne target — ne le faites que si vous avez
renoncé à filtrer dessus.

> Chaque valeur de format atteint son propre formateur. `compact` était accepté
> puis rendu comme la valeur par défaut — corrigé (#1480).
> pas.

## Configurer la journalisation depuis les réglages

Tout ce qui précède a un équivalent TOML, pour qu'un déploiement change sa
journalisation sans recompiler. La section s'appelle `[logging]` :

```toml
# config/dev_settings.toml
[logging]
level             = "info,sqlx=warn"
format            = "pretty"
with_line_numbers = true
```

```toml
# config/prod_settings.toml
[logging]
level         = "info"
format        = "json"
file_dir      = "/var/log/myapp"
file_prefix   = "app"
file_rotation = "daily"
```

| Clé | Type | Défaut | Notes |
|---|---|---|---|
| `level` | chaîne | `info,sqlx=warn` | Syntaxe `RUST_LOG`. Utilisée seulement si `RUST_LOG` est absente |
| `format` | chaîne | `full` | `full` / `pretty` / `compact` / `json`. Une valeur inconnue retombe sur `full` avec un `warn` |
| `color` | chaîne | `auto` | `auto` / `always` / `never`. `auto` ne colore qu'un terminal et respecte `NO_COLOR` |
| `access_log` | bool | `true` | Une ligne par requête, plus le span qui porte `tenant` dans les événements du handler |
| `with_thread_ids` | bool | `false` | Identifiant de thread sur chaque événement |
| `with_line_numbers` | bool | `false` | Ligne source sur chaque événement |
| `without_targets` | bool | `false` | Masquer la colonne target |
| `file_dir` | chaîne | non défini | La définir active le puits fichier |
| `file_prefix` | chaîne | `app` | Racine du nom de fichier |
| `file_rotation` | chaîne | `daily` | `daily` / `hourly` / `minutely` / `never`. Une valeur inconnue retombe sur `daily` avec un `warn` |
| `file_only` | bool | `false` | Supprimer stdout. Sans effet si `file_dir` n'est pas défini |

Appliquez-le d'un seul appel sur la `Cli` :

```rust,ignore
rustango::manage::Cli::new()
    .with_settings_from_env()
    .with_logging()               // installe depuis Settings.logging
    .api(urls::api())
    .run()
    .await
```

`with_logging()` est optionnel — désactivé par défaut, pour qu'un projet qui
appelle lui-même `logging::setup()` n'obtienne pas un second installateur.
L'ordre dans la chaîne n'a pas d'importance : l'installation a lieu à `run()`,
contre les réglages finaux.

> **`#[rustango::main]` l'emporte dessus.** La macro installe un subscriber
> avant même que le runtime ne soit construit, et chaque installateur utilise
> `try_init` : le premier gagne, les suivants sont écartés en silence. Un `main`
> qui garde la macro *et* appelle `with_logging()` obtient donc les valeurs par
> défaut de la macro : votre section `[logging]` est lue puis reste sans effet,
> sans rien pour le signaler. Pour une journalisation pilotée par les réglages,
> remplacez `#[rustango::main]` par `#[tokio::main]` — c'est la seule chose
> qu'un projet généré doit changer. Suivi dans
> [#1465](https://github.com/ujeenet/rustango/issues/1465).

N'importe quelle clé peut être écrasée par déploiement via une variable
d'environnement, la section et la clé servant de segments de chemin :

```sh
RUSTANGO__LOGGING__LEVEL=debug
RUSTANGO__LOGGING__FORMAT=json
```

Pour construire le subscriber vous-même depuis la même section — quand vous avez
besoin du guard renvoyé, voir plus bas — utilisez `Setup::from_settings` :

```rust,ignore
let settings = rustango::config::Settings::load_from_env()?;
let _guard = rustango::logging::Setup::from_settings(&settings.logging).install();
```

## Écrire dans un fichier

Les logs vont sur stdout sauf demande contraire, ce qui est le bon choix dans un
conteneur. Quand il vous faut des fichiers, `with_file` écrit en plus dans un
appender rotatif :

```rust,ignore
use rustango::logging::{Rotation, Setup};

let _guard = Setup::new()
    .json()
    .with_file("/var/log/myapp", "app", Rotation::Daily)
    .install();
```

Avec `Daily`, les fichiers atterrissent dans `{dir}/{prefix}.YYYY-MM-DD`, et le
répertoire est créé à la première écriture. `Rotation` vaut `Daily`, `Hourly`,
`Minutely` ou `Never`. Ajoutez `.file_only()` pour supprimer la couche stdout —
pour un worker sans terminal ou un processus démonisé, où personne ne lit stdout
de toute façon.

> **Gardez le guard en vie.** `install()` renvoie
> `Option<tracing_appender::non_blocking::WorkerGuard>` — `Some` lorsqu'un puits
> fichier est configuré. L'écrivain fichier est non bloquant, donc un disque
> bloqué ne peut pas suspendre le traitement des requêtes ; le prix est que les
> événements en tampon ne sont vidés qu'à la destruction du guard. Liez-le à la
> durée de vie du processus (un `static`, un `OnceLock`, ou un `let` dans `main`
> qui survit à tout). `let _ = ...install();` le détruit immédiatement et vous
> perdez des écritures. `Cli::with_logging()` le garde pour vous.

## Le journal d'accès

Un événement par requête terminée, avec les champs qu'un exploitant recherche :

```rust,ignore
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};

let app = router.access_log(AccessLogLayer::default());
```

```text
INFO rustango::access_log: http.request.method=GET url.path=/api/posts url.query=page=2 http.response.status_code=200 duration_ms=12 client.address=192.0.2.1 tenant=acme
```

Le niveau porte du sens, l'alerting peut donc s'y accrocher :

| Condition | Niveau |
|---|---|
| Réponse normale | `info` |
| Statut >= 400 | `warn` |
| Plus lent que `slow_threshold_ms` (1000 par défaut) | `warn`, message `slow request` |

Réglages :

```rust,ignore
AccessLogLayer::default()
    .errors_only()               // ignorer complètement les 2xx/3xx
    .slow_threshold_ms(250)      // ce qui compte comme lent
    .without_ip()                // omettre l'IP du client
    .trust_proxy_headers(true)   // X-Forwarded-For, derrière un proxy de confiance seulement
```

Les paramètres de requête porteurs d'identifiants sont masqués par `[redacted]`
avant l'écriture de la ligne — `password`, `passwd`, `token`, `secret`,
`api_key`, `apikey`, `access_token`, `refresh_token`, `signature`, `auth`.
Étendez avec `.redact_additional("session_id")`, ou remplacez toute la liste
avec `.redact(vec![...])`. Cela ne couvre **que les chaînes de requête** ; pour
le reste, voir
[security.md](security.md#garder-les-secrets-hors-de-vos-journaux).

## C'était quel locataire ?

`tenant` nomme le locataire auquel la requête a été résolue, et vaut `-`
lorsqu'aucun ne l'a été — une requête sur le domaine apex ou sur la console
opérateur, ou une application mono-locataire. Le champ n'est jamais vide, si
bien que « pas de locataire » se lit différemment d'un champ qui s'est perdu.

Rien à câbler : `ChainResolver` publie l'identité dès qu'il en résout une, et le
journal d'accès la relit. Le mécanisme est
[`rustango::tenant_log`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/src/tenant_log.rs),
un emplacement par requête, nécessaire parce que l'identité du locataire vit
dans un extracteur — à l'intérieur du handler, sous le middleware qui a besoin
de la journaliser.

Le slug est choisi par l'exploitant et c'est souvent le nom du client. Quand les
logs quittent votre infrastructure, étiquetez plutôt par identifiant :

```rust,ignore
use rustango::access_log::{AccessLogLayer, TenantField};

AccessLogLayer::default().tenant_field(TenantField::Id)     // tenant=42
AccessLogLayer::default().tenant_field(TenantField::Both)   // tenant=acme#42
AccessLogLayer::default().tenant_field(TenantField::Off)    // tenant=-
```

> **Le chemin de la requête uniquement.** Une tâche de fond tourne en dehors de
> la requête qui l'a mise en file et n'a pas de locataire à journaliser —
> l'emplacement est par tâche, et `tokio::spawn` n'en hérite pas. Suivi dans
> [#1229](https://github.com/ujeenet/rustango/issues/1229) /
> [#1223](https://github.com/ujeenet/rustango/issues/1223).

## Spans de requête et OpenTelemetry

`TracingLayer` enveloppe chaque requête dans un span suivant les
[conventions sémantiques OpenTelemetry v1.30](https://opentelemetry.io/docs/specs/semconv/http/http-spans/),
si bien qu'un collecteur n'a besoin d'aucune règle de renommage d'attributs :

```rust,ignore
use rustango::tracing_layer::TracingLayer;
use tower::ServiceBuilder;

let app = ServiceBuilder::new().layer(TracingLayer::new()).service(router);
```

Le span porte `http.request.method`, `url.path`, `url.query`,
`network.protocol.version`, `user_agent.original`,
`http.response.status_code`, `http.response.body.size`, `duration_ms`, ainsi que
`tenant` / `org_id` dès qu'un locataire est résolu. Quand la requête arrive avec
un en-tête `traceparent` du W3C, `trace_id`, `parent_span_id` et `trace_flags`
sont également enregistrés — c'est ce qu'une couche `tracing-opentelemetry`
récupère pour rejoindre la trace.

Cette couche vaut d'être installée ne serait-ce que pour le champ locataire :
comme les champs sont portés par le *span*, chaque événement émis pendant la
requête — y compris ceux de l'ORM — les porte dans son contexte de span, sans
qu'aucun sous-système ait à savoir ce qu'est un locataire. Elle n'est **pas**
installée par défaut ; ni `server::Builder`, ni `Cli`, ni le scaffolder ne
l'ajoutent.

## Journalisation dans les tests

Les installateurs utilisent `try_init`, appeler `logging::setup()` depuis un
test est donc sans danger même si un autre test en a déjà installé un. Pour
faire des assertions sur la sortie, préférez un subscriber circonscrit à un
subscriber global :

```rust,ignore
let subscriber = tracing_subscriber::fmt()
    .with_writer(make_writer)
    .with_max_level(tracing::Level::INFO)
    .finish();
let _guard = tracing::subscriber::set_default(subscriber);
```

`set_default` est local au thread et renvoie un guard qui restaure le subscriber
précédent, si bien que des tests parallèles ne se gênent pas. `#[tokio::test]`
lance un runtime mono-thread, ce qui garde tout le future sur le thread couvert
par le guard.

## Rien ne sort

- **`RUST_LOG` est définie et c'est toujours silencieux.** Le filtre est lu une
  seule fois, à l'installation. Définir la variable après `setup()` ne change
  rien.
- **Votre propre crate est muet en `info`.** `RUST_LOG=info` s'applique à tous
  les targets ; si vous avez mis `RUST_LOG=rustango=info`, vous avez filtré
  votre propre code. Nommez les deux : `RUST_LOG=info,rustango=warn`.
- **Un filtre sur `crate::quelque_chose` ne correspond à rien.** Les targets
  sont des chaînes ; voir la note sous
  [Targets](#targets--nommer-le-sous-système).
- **Fichier vide après un crash.** Le guard renvoyé par `install()` a été
  détruit, ou le processus est mort avant que l'appender ne vide son tampon.
  Voir [Écrire dans un fichier](#écrire-dans-un-fichier).
- **Deux subscribers, le second ignoré.** `try_init` signifie que le premier
  gagne, en silence. Si vous appelez `logging::setup()` *et*
  `Cli::with_logging()`, c'est celui des réglages qui perd.
