//! Commands a crate or the app adds to `manage`, next to the built-in verbs.
//!
//! ```ignore
//! rustango::register_command!(
//!     "reset-link",
//!     "Print a password-reset link for a user",
//!     |mut ctx: rustango::manage::CommandContext, args: Vec<String>| async move {
//!         let pool = ctx.tenant_pool().ok_or("pass --tenant <slug>")?.clone();
//!         writeln!(ctx.stdout(), "user {}", args.first().ok_or("usage: reset-link <user>")?)?;
//!         Ok(())
//!     },
//! );
//! ```

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;

use crate::sql::Pool;

/// What a command returns on failure; any error converts with `?`.
pub type CommandError = Box<dyn std::error::Error + Send + Sync>;

/// The future a command handler returns.
pub type CommandFuture = Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'static>>;

/// One [`register_command!`](crate::register_command) entry. Built by
/// the macro; not meant to be written by hand.
#[doc(hidden)]
pub struct CommandEntry {
    pub name: &'static str,
    pub about: &'static str,
    /// The registering crate, so `--help` groups by it.
    pub group: &'static str,
    pub handler: fn(CommandContext, Vec<String>) -> CommandFuture,
}

inventory::collect!(CommandEntry);

/// Register a `manage` command from any crate, without the host app
/// changing `main`. The handler gets a [`CommandContext`] and the
/// arguments after the verb (`--tenant <slug>` already taken out).
/// A name that clashes with a built-in verb or another command makes
/// [`Cli::run`](crate::manage::Cli::run) fail at startup.
#[macro_export]
macro_rules! register_command {
    ($name:expr, $about:expr, $handler:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::manage::CommandEntry {
                name: $name,
                about: $about,
                group: ::core::env!("CARGO_PKG_NAME"),
                handler: {
                    fn __rustango_command_handler(
                        ctx: $crate::manage::CommandContext,
                        args: ::std::vec::Vec<::std::string::String>,
                    ) -> $crate::manage::CommandFuture {
                        $crate::manage::__command_future(($handler)(ctx, args))
                    }
                    __rustango_command_handler
                },
            }
        }
    };
}

/// Boxes a handler's future; the bound fixes its error type, so a bare
/// `Ok(())` needs no annotation.
#[doc(hidden)]
pub fn __command_future<F>(fut: F) -> CommandFuture
where
    F: Future<Output = Result<(), CommandError>> + Send + 'static,
{
    Box::pin(fut)
}

type Handler = Arc<dyn Fn(CommandContext, Vec<String>) -> CommandFuture + Send + Sync>;

/// A command from the macro or from [`Cli::command`](crate::manage::Cli::command).
#[derive(Clone)]
pub(crate) struct Command {
    name: String,
    about: String,
    group: String,
    handler: Handler,
}

impl Command {
    pub(crate) fn new<F, Fut>(name: &str, about: &str, group: &str, handler: F) -> Self
    where
        F: Fn(CommandContext, Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), CommandError>> + Send + 'static,
    {
        Self {
            name: name.to_owned(),
            about: about.to_owned(),
            group: group.to_owned(),
            handler: Arc::new(move |ctx, args| -> CommandFuture { Box::pin(handler(ctx, args)) }),
        }
    }

    pub(crate) async fn call(
        &self,
        ctx: CommandContext,
        args: Vec<String>,
    ) -> Result<(), CommandError> {
        (self.handler)(ctx, args).await
    }
}

/// Every verb the framework dispatches itself, in either mode. A
/// command may not take one, even a verb this build's features drop.
const BUILTIN_VERBS: &[&str] = &[
    // `Cli` itself.
    "runserver",
    "run-server",
    "dbshell",
    "help",
    "--help",
    "-h",
    // `migrate::manage`.
    "makemigrations",
    "migrate",
    "downgrade",
    "showmigrations",
    "status",
    "sqlmigrate",
    "forget-pending",
    "startapp",
    "add-data-op",
    "about",
    "check",
    "docs",
    "version",
    "--version",
    "db:dump",
    "db:restore",
    "db:info",
    "dumpdata",
    "loaddata",
    "showurls",
    "showmodels",
    "create-admin",
    "flush",
    "prune",
    "clear-cache",
    "clearsessions",
    "createcachetable",
    "create-cache-table",
    "sendtestemail",
    "inspectdb",
    "make:viewset",
    "make:api_routes",
    "make:serializer",
    "make:form",
    "make:job",
    "make:scheduled",
    "make:worker",
    "make:notification",
    "make:middleware",
    "make:test",
    // `tenancy::manage`.
    "create-tenant",
    "test-tenant-connection",
    "drop-tenant",
    "purge-tenant",
    "rotate-tenant-password",
    "list-tenants",
    "edit-tenant",
    "reseal-tenant-secrets",
    "list-hosts",
    "add-host",
    "remove-host",
    "set-host-enabled",
    "add-db-server",
    "edit-db-server",
    "list-db-servers",
    "test-db-server",
    "remove-db-server",
    "prewarm-pools",
    "migrate-tenants",
    "migrate-registry",
    "migrate-tenant-storage",
    "create-operator",
    "list-operators",
    "set-operator-active",
    "list-runs",
    "show-run",
    "audit-log",
    "create-user",
    "create-superuser",
    "set-superuser",
    "revoke-passkey",
    "reset-password",
    "set-password-link",
    "reset-operator-password",
    "change-password",
    "change-operator-password",
    "init-tenancy",
    "wizard",
    "init",
    "menu",
    "actions",
    "audit-cleanup",
    "create-role",
    "list-roles",
    "assign-role",
    "revoke-role",
    "grant-perm",
    "revoke-perm",
    "create-api-key",
    "create-agent",
    "rotate-agent-secret",
    "list-agents",
    "create-skill",
    "grant-skill",
    "revoke-skill",
    "list-skills",
    "create-user-key",
    "list-user-keys",
    "revoke-user-key",
    "map-skill-permission",
    "unmap-skill-permission",
    "seed-permissions",
];

/// The validated commands of one run: unique, and none a built-in verb.
#[derive(Clone, Default)]
pub(crate) struct CommandSet {
    commands: Vec<Command>,
}

impl CommandSet {
    /// Registered commands plus `extra` from the builder.
    ///
    /// # Errors
    /// A bad name, a built-in verb, or one name registered twice.
    pub(crate) fn build(extra: &[Command]) -> Result<Self, String> {
        let registered = crate::core::inventory::iter::<CommandEntry>
            .into_iter()
            .map(|e| {
                let handler = e.handler;
                Command {
                    name: e.name.to_owned(),
                    about: e.about.to_owned(),
                    group: e.group.to_owned(),
                    handler: Arc::new(handler),
                }
            });
        let mut commands: Vec<Command> = Vec::new();
        for c in registered.chain(extra.iter().cloned()) {
            if c.name.is_empty() || c.name.starts_with('-') || c.name.contains(char::is_whitespace)
            {
                return Err(format!(
                    "manage command `{}` ({}): not a valid verb",
                    c.name, c.group
                ));
            }
            if BUILTIN_VERBS.contains(&c.name.as_str()) {
                return Err(format!(
                    "manage command `{}` ({}) clashes with a built-in verb; rename it",
                    c.name, c.group
                ));
            }
            if let Some(prev) = commands.iter().find(|p| p.name == c.name) {
                return Err(format!(
                    "manage command `{}` is registered twice ({} and {})",
                    c.name, prev.group, c.group
                ));
            }
            commands.push(c);
        }
        commands.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
        Ok(Self { commands })
    }

    pub(crate) fn get(&self, verb: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.name == verb)
    }

    /// The `--help` section, grouped by crate; nothing when empty.
    pub(crate) fn write_help<W: Write>(&self, w: &mut W) -> std::io::Result<()> {
        let mut group: Option<&str> = None;
        for c in &self.commands {
            if group != Some(c.group.as_str()) {
                writeln!(w, "\nCOMMANDS FROM {}:", c.group)?;
                group = Some(c.group.as_str());
            }
            writeln!(w, "  {:<24} {}", c.name, c.about)?;
        }
        Ok(())
    }
}

/// What a command runs with.
pub struct CommandContext {
    pool: Pool,
    #[cfg(feature = "tenancy")]
    tenant: Option<(crate::tenancy::Org, Pool)>,
    #[cfg(feature = "tenancy")]
    tenants: Option<Arc<dyn TenantLookup>>,
    #[cfg(feature = "config")]
    settings: Option<crate::config::Settings>,
    stdout: Box<dyn Write + Send>,
}

impl CommandContext {
    pub(crate) fn new(pool: Pool) -> Self {
        Self {
            pool,
            #[cfg(feature = "tenancy")]
            tenant: None,
            #[cfg(feature = "tenancy")]
            tenants: None,
            #[cfg(feature = "config")]
            settings: None,
            stdout: Box::new(std::io::stdout()),
        }
    }

    #[cfg(feature = "config")]
    pub(crate) fn with_settings(mut self, settings: Option<crate::config::Settings>) -> Self {
        self.settings = settings;
        self
    }

    /// The app's database; in a tenancy project, the registry.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// The `--tenant <slug>` org, when one was passed.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant(&self) -> Option<&crate::tenancy::Org> {
        self.tenant.as_ref().map(|(org, _)| org)
    }

    /// The `--tenant <slug>` org's pool: its schema or its own database.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_pool(&self) -> Option<&Pool> {
        self.tenant.as_ref().map(|(_, pool)| pool)
    }

    /// Any tenant's org and pool, by slug.
    ///
    /// # Errors
    /// Not a tenancy project, no such tenant, or its pool fails to open.
    #[cfg(feature = "tenancy")]
    pub async fn tenant_by_slug(
        &self,
        slug: &str,
    ) -> Result<(crate::tenancy::Org, Pool), CommandError> {
        let lookup = self
            .tenants
            .as_ref()
            .ok_or("tenants need `Cli::tenancy()`")?;
        Ok(lookup.resolve(slug).await?)
    }

    /// The loaded settings, when the `Cli` was given them.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn settings(&self) -> Option<&crate::config::Settings> {
        self.settings.as_ref()
    }

    /// Where the command's output goes.
    pub fn stdout(&mut self) -> &mut (dyn Write + Send) {
        &mut *self.stdout
    }
}

/// `TenantPools<DB>` with the backend erased, so the context is not generic.
#[cfg(feature = "tenancy")]
pub(crate) trait TenantLookup: Send + Sync {
    fn resolve<'a>(
        &'a self,
        slug: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(crate::tenancy::Org, Pool), crate::tenancy::TenancyError>>
                + Send
                + 'a,
        >,
    >;
}

#[cfg(feature = "tenancy")]
impl<DB: sqlx::Database> TenantLookup for crate::tenancy::TenantPools<DB>
where
    Pool: From<sqlx::Pool<DB>>,
{
    fn resolve<'a>(
        &'a self,
        slug: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(crate::tenancy::Org, Pool), crate::tenancy::TenancyError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let org = crate::tenancy::manage::api::find_org(self, slug)
                .await?
                .ok_or_else(|| {
                    crate::tenancy::TenancyError::Validation(format!("tenant `{slug}` not found"))
                })?;
            let pool = self.scoped_pool_dyn(&org).await?;
            Ok((org, pool))
        })
    }
}

/// Run `cmd` in a tenancy project: `--tenant <slug>` comes out of `args`
/// and resolves to that tenant's pool before the handler starts.
#[cfg(feature = "tenancy")]
pub(crate) async fn run_tenancy(
    cmd: &Command,
    mut ctx: CommandContext,
    tenants: Arc<dyn TenantLookup>,
    args: Vec<String>,
) -> Result<(), CommandError> {
    let (slug, rest) = take_tenant_flag(args)?;
    if let Some(slug) = slug {
        ctx.tenant = Some(tenants.resolve(&slug).await?);
    }
    ctx.tenants = Some(tenants);
    cmd.call(ctx, rest).await
}

/// `--tenant <slug>` / `--tenant=<slug>` out of `args`, the rest kept in order.
#[cfg(feature = "tenancy")]
fn take_tenant_flag(args: Vec<String>) -> Result<(Option<String>, Vec<String>), CommandError> {
    let mut slug = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let value = if arg == "--tenant" {
            Some(it.next().ok_or("--tenant needs a slug")?)
        } else {
            arg.strip_prefix("--tenant=").map(str::to_owned)
        };
        match value {
            Some(v) if slug.is_some() => return Err(format!("--tenant given twice (`{v}`)").into()),
            Some(v) => slug = Some(v),
            None => rest.push(arg),
        }
    }
    Ok((slug, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop(name: &str) -> Command {
        Command::new(name, "about", "app", |_, _| async { Ok(()) })
    }

    #[test]
    fn a_builtin_verb_or_a_duplicate_is_refused() {
        for verb in ["migrate", "create-tenant", "runserver", "make:job"] {
            let err = CommandSet::build(&[noop(verb)]).err().expect(verb);
            assert!(err.contains("built-in"), "{err}");
        }
        let err = CommandSet::build(&[noop("x-dup"), noop("x-dup")])
            .err()
            .unwrap();
        assert!(err.contains("twice"), "{err}");
        assert!(CommandSet::build(&[noop("--flag")]).is_err());
        assert!(CommandSet::build(&[noop("x-ok")])
            .unwrap()
            .get("x-ok")
            .is_some());
    }

    #[test]
    fn help_lists_commands_by_crate() {
        let set = CommandSet::build(&[noop("x-zeta"), noop("x-alpha")]).unwrap();
        let mut out = Vec::new();
        set.write_help(&mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let (a, z) = (out.find("x-alpha").unwrap(), out.find("x-zeta").unwrap());
        assert!(out.contains("COMMANDS FROM app:") && a < z, "{out}");
    }

    #[cfg(feature = "tenancy")]
    #[test]
    fn the_tenant_flag_comes_out_of_the_args() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let (slug, rest) = take_tenant_flag(args(&["a", "--tenant", "acme", "b"])).unwrap();
        assert_eq!((slug.as_deref(), rest), (Some("acme"), args(&["a", "b"])));
        let (slug, _) = take_tenant_flag(args(&["--tenant=acme"])).unwrap();
        assert_eq!(slug.as_deref(), Some("acme"));
        assert!(take_tenant_flag(args(&["--tenant"])).is_err());
        assert!(take_tenant_flag(args(&["--tenant", "a", "--tenant", "b"])).is_err());
    }
}
