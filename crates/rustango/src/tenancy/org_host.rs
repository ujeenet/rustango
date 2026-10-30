//! Additional hostnames a tenant answers on.
//!
//! [`Org::host_pattern`](super::Org) holds the tenant's base host, the
//! one `create-tenant` writes. This table holds the extra hosts added
//! later, so one tenant can serve several domains.
//!
//! The base host is deliberately not a row here. If it were, only a
//! runtime check would stop a `DELETE ... WHERE org_id = ?` from
//! leaving a tenant with no host at all. Keeping it on `Org` means
//! there is no row to delete. Listing joins the two and marks which is
//! which, so callers still see one list.

use serde::Serialize;

/// One extra hostname bound to a tenant.
#[derive(crate::Model, Debug, Clone, Serialize)]
#[rustango(
    table = "rustango_org_hosts",
    scope = "registry",
    display = "hostname",
    admin(
        list_display = "hostname, org_id, enabled, created_at",
        ordering = "hostname",
    )
)]
pub struct OrgHost {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,

    #[rustango(fk = "rustango_orgs", on = "id", index)]
    pub org_id: i64,

    /// The `Host` header to match, lowercased, no port and no scheme.
    ///
    /// Unique across the whole registry, not per-org: a hostname resolves
    /// to exactly one tenant, and letting two orgs claim one host would
    /// make routing depend on row order.
    #[rustango(max_length = 255, unique, index)]
    pub hostname: String,

    /// Lets an operator park a host without serving it — useful while DNS
    /// propagates, or to retire a domain without losing the record.
    pub enabled: bool,

    #[rustango(auto_now_add)]
    pub created_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
}

/// A hostname a tenant answers on, from either source.
#[derive(Debug, Clone, Serialize)]
pub struct TenantHost {
    pub hostname: String,
    /// True for `Org.host_pattern`. The caller uses this to refuse a
    /// remove, and the UI to explain why the row has no delete button.
    pub is_base: bool,
    pub enabled: bool,
    /// `None` for the base host — it has no row of its own.
    pub id: Option<i64>,
}

/// Why a host could not be registered.
#[derive(Debug)]
#[non_exhaustive]
pub enum HostError {
    /// Empty, or carrying a scheme, port, path or uppercase.
    Invalid(String),
    /// Already claimed — by this tenant or another one.
    Taken(String),
    /// Removing the base host, which this table cannot represent.
    IsBaseHost(String),
    /// The apex: the operator console and path-prefix tenants live there.
    IsApex(String),
    /// No tenant by that slug. Separate from [`Self::NotFound`], which
    /// is about a hostname, so a mistyped slug says so.
    NoSuchOrg(String),
    /// No such hostname on a tenant that does exist.
    NotFound,
    Driver(crate::sql::ExecError),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(h) => write!(f, "`{h}` is not a bare hostname"),
            Self::Taken(h) => write!(f, "`{h}` is already registered"),
            Self::IsBaseHost(h) => {
                write!(f, "`{h}` is this tenant's base host and cannot be removed")
            }
            Self::IsApex(h) => write!(f, "`{h}` is the apex and cannot be a tenant host"),
            Self::NoSuchOrg(s) => write!(f, "no tenant named `{s}`"),
            Self::NotFound => write!(f, "no such host"),
            Self::Driver(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<crate::sql::ExecError> for HostError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::Driver(e)
    }
}

/// Normalize and validate a hostname.
///
/// Anything that is not a bare host is rejected, not repaired. The
/// value is compared byte for byte with the `Host` header, so a stored
/// `https://x.com/` would never match and would look like a routing
/// bug instead of bad input.
///
/// # Errors
/// [`HostError::Invalid`] when the value is empty, over-long, or carries a
/// scheme / port / path / whitespace.
pub fn normalize_hostname(raw: &str) -> Result<String, HostError> {
    let h = raw.trim().to_ascii_lowercase();
    let bad = h.is_empty()
        || h.len() > 255
        || h.contains("://")
        || h.contains('/')
        || h.contains(':')
        || h.contains(char::is_whitespace)
        || h.starts_with('.')
        || h.ends_with('.')
        || !h
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if bad {
        return Err(HostError::Invalid(raw.to_owned()));
    }
    Ok(h)
}

use crate::core::Column as _;
use crate::sql::{FetcherPool as _, Pool};

/// Every hostname `org_slug` answers on — base first, then extras.
///
/// # Errors
/// Driver / query failures.
pub async fn list_for_org(registry: &Pool, org_slug: &str) -> Result<Vec<TenantHost>, HostError> {
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NoSuchOrg(org_slug.to_owned()));
    };
    let org_id = org.id.get().copied().unwrap_or_default();
    let mut out = Vec::new();
    if let Some(base) = org.host_pattern.as_deref().filter(|h| !h.trim().is_empty()) {
        out.push(TenantHost {
            hostname: base.to_owned(),
            is_base: true,
            enabled: true,
            id: None,
        });
    }
    let extras: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::org_id.eq(org_id))
        .order_by(&[("hostname", false)])
        .fetch(registry)
        .await?;
    out.extend(extras.into_iter().map(|h| TenantHost {
        hostname: h.hostname,
        is_base: false,
        enabled: h.enabled,
        id: h.id.get().copied(),
    }));
    Ok(out)
}

/// Bind an extra `hostname` to `org_slug`. `apex` is the server's apex host.
///
/// # Errors
/// [`HostError::Invalid`] for a malformed host, [`HostError::IsApex`] for
/// the apex, [`HostError::Taken`] when any tenant already claims it
/// (including as its base host), or [`HostError::NoSuchOrg`] for an unknown org.
pub async fn add_host(
    registry: &Pool,
    org_slug: &str,
    hostname: &str,
    apex: &str,
) -> Result<OrgHost, HostError> {
    let host = normalize_hostname(hostname)?;
    if host.eq_ignore_ascii_case(apex.strip_suffix('.').unwrap_or(apex)) {
        return Err(HostError::IsApex(host));
    }
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NoSuchOrg(org_slug.to_owned()));
    };
    let org_id = org.id.get().copied().unwrap_or_default();
    let Claimed { tx, row } = claim_extra_host(registry, &host, org_id).await?;
    tx.commit().await.map_err(crate::sql::ExecError::from)?;
    super::invalidate_host_cache();
    Ok(row)
}

/// Is `host` some tenant's base host or extra host, other than
/// `except_org`'s own? Every path that writes a host asks this, since
/// a host two tenants claim routes by row order (#1931).
///
/// # Errors
/// Driver / query failures.
pub(crate) async fn host_claimed(
    registry: &Pool,
    host: &str,
    except_org: Option<i64>,
) -> Result<bool, crate::sql::ExecError> {
    // The unique index on `rustango_org_hosts` cannot see
    // `rustango_orgs.host_pattern`, so both tables are checked.
    // `iexact`: a row stored before hosts were lowercased still clashes,
    // and only MySQL's collation would catch it with `=`.
    let base: Vec<super::Org> = super::Org::objects()
        .where_(super::Org::host_pattern.iexact(host))
        .fetch(registry)
        .await?;
    if other_org(&base, except_org) {
        return Ok(true);
    }
    let extra: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::hostname.iexact(host))
        .fetch(registry)
        .await?;
    Ok(extra
        .iter()
        .any(|h| except_org.is_none() || Some(h.org_id) != except_org))
}

/// Whose base routing values a [`claim_routing`] takes: they clash only
/// with another tenant's.
pub(crate) enum Claimant<'a> {
    /// An existing tenant.
    Base(i64),
    /// A tenant inserted in the claim's transaction.
    NewOrg(&'a mut super::Org),
}

/// A held extra-host claim: commit `tx`; `row` is the new host.
struct Claimed {
    tx: crate::sql::PoolTx<'static>,
    row: OrgHost,
}

/// Claim `host` as an extra host of `org_id` in a transaction this opens,
/// so nothing reads before it.
///
/// The claim is the hostname's `rustango_org_hosts` row: its unique index
/// makes a concurrent claim wait for the transaction, then fail. MySQL
/// breaks two claims of a free host with a deadlock; the loser retries
/// once and then sees the winner's row.
///
/// # Errors
/// [`HostError::Taken`] when any tenant has it, or a driver failure.
async fn claim_extra_host(registry: &Pool, host: &str, org_id: i64) -> Result<Claimed, HostError> {
    match claim_extra_host_once(registry, host, org_id).await {
        Err(HostError::Driver(e)) if e.is_deadlock() => {
            claim_extra_host_once(registry, host, org_id).await
        }
        r => r,
    }
}

async fn claim_extra_host_once(
    registry: &Pool,
    host: &str,
    org_id: i64,
) -> Result<Claimed, HostError> {
    let key = ClaimKey::Host(host.to_owned());
    let mut tx = crate::sql::write_transaction(registry).await?;
    let (row, _) = lock_key(&mut tx, &key, org_id, false)
        .await
        .map_err(ClaimError::into_host)?;
    if held_elsewhere(&mut tx, &key, row.id.get().copied(), None).await? {
        return Err(HostError::Taken(host.to_owned()));
    }
    Ok(Claimed { tx, row })
}

/// A routing value only one tenant may hold.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ClaimKey {
    /// A normalized hostname.
    Host(String),
    /// A validated path prefix, `/segment`.
    PathPrefix(String),
    Port(i32),
}

impl ClaimKey {
    /// Its lock row's `hostname`: no hostname holds `/` or `:`, so a
    /// prefix or port lock never meets a real host.
    fn lock_name(&self) -> String {
        match self {
            Self::Host(h) => h.clone(),
            Self::PathPrefix(p) => p.to_ascii_lowercase(),
            Self::Port(n) => format!(":{n}"),
        }
    }
}

impl std::fmt::Display for ClaimKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host(h) => write!(f, "host `{h}`"),
            Self::PathPrefix(p) => write!(f, "path prefix `{p}`"),
            Self::Port(n) => write!(f, "port {n}"),
        }
    }
}

/// Why a routing claim failed.
#[derive(Debug)]
pub(crate) enum ClaimError {
    Taken(ClaimKey),
    Driver(crate::sql::ExecError),
}

impl From<crate::sql::ExecError> for ClaimError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::Driver(e)
    }
}

impl ClaimError {
    /// MySQL broke a tie between two claims; the caller may retry once.
    pub(crate) fn is_deadlock(&self) -> bool {
        matches!(self, Self::Driver(e) if e.is_deadlock())
    }

    fn into_host(self) -> HostError {
        match self {
            Self::Taken(k) => HostError::Taken(k.lock_name()),
            Self::Driver(e) => HostError::Driver(e),
        }
    }

    /// As the tenant-edit and provisioning paths report it.
    pub(crate) fn into_tenancy(self) -> super::TenancyError {
        match self {
            Self::Taken(k) => {
                super::TenancyError::Validation(format!("{k} is already used by another tenant"))
            }
            Self::Driver(e) => super::TenancyError::Exec(e),
        }
    }
}

impl Claimant<'_> {
    /// The tenant's id, inserting a new tenant first.
    async fn begin(
        &mut self,
        tx: &mut crate::sql::PoolTx<'_>,
    ) -> Result<i64, crate::sql::ExecError> {
        Ok(match self {
            Self::Base(id) => *id,
            Self::NewOrg(org) => {
                // A retry re-inserts: the deadlock rolled the first INSERT back.
                org.id = crate::sql::Auto::Unset;
                org.insert_tx(tx).await?;
                org.id.get().copied().unwrap_or_default()
            }
        })
    }
}

/// Claim a tenant's base host, path prefix and port at once, in a
/// transaction this opens; commit it after the write it guards. Retried
/// once on a MySQL deadlock, as [`claim_extra_host`].
///
/// # Errors
/// [`ClaimError::Taken`] for the first value another tenant holds.
pub(crate) async fn claim_routing(
    registry: &Pool,
    keys: &[ClaimKey],
    mut claimant: Claimant<'_>,
) -> Result<crate::sql::PoolTx<'static>, ClaimError> {
    match claim_routing_once(registry, keys, &mut claimant).await {
        Err(ClaimError::Driver(e)) if e.is_deadlock() => {
            claim_routing_once(registry, keys, &mut claimant).await
        }
        r => r,
    }
}

async fn claim_routing_once(
    registry: &Pool,
    keys: &[ClaimKey],
    claimant: &mut Claimant<'_>,
) -> Result<crate::sql::PoolTx<'static>, ClaimError> {
    let mut tx = crate::sql::write_transaction(registry).await?;
    let org_id = claimant.begin(&mut tx).await?;
    claim_routing_tx(&mut tx, keys, org_id).await?;
    Ok(tx)
}

/// [`claim_routing`] for `org_id`'s base values, inside `tx`. No plain
/// read may run in `tx` before it: MySQL's snapshot must start after the locks.
///
/// # Errors
/// As [`claim_routing`]; not retried.
pub(crate) async fn claim_routing_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    keys: &[ClaimKey],
    org_id: i64,
) -> Result<(), ClaimError> {
    let mut keys = keys.to_vec();
    // One order for every claimant, so two cannot wait on each other.
    keys.sort();
    keys.dedup();
    let mut locked = Vec::with_capacity(keys.len());
    for key in &keys {
        locked.push(lock_key(tx, key, org_id, true).await?);
    }
    for (key, (row, _)) in keys.iter().zip(&locked) {
        if held_elsewhere(tx, key, row.id.get().copied(), Some(org_id)).await? {
            return Err(ClaimError::Taken(key.clone()));
        }
    }
    for (row, inserted) in locked {
        if inserted {
            row.delete_tx(tx).await?;
        }
    }
    Ok(())
}

/// Take `key`'s lock row in `tx`: a waiting writer of the same row
/// blocks here until it commits. `(row, inserted)`.
async fn lock_key(
    tx: &mut crate::sql::PoolTx<'_>,
    key: &ClaimKey,
    org_id: i64,
    base: bool,
) -> Result<(OrgHost, bool), ClaimError> {
    use crate::sql::FetcherTx as _;
    let name = key.lock_name();
    // A locking read: waits for a writer holding the row, takes no snapshot.
    let held: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::hostname.eq(name.clone()))
        .select_for_update()
        .silent_on_sqlite()
        .fetch_tx(tx)
        .await?;
    match held.into_iter().next() {
        // Promoting the tenant's own extra host: that row is the claim.
        Some(own) if base && own.org_id == org_id && matches!(key, ClaimKey::Host(_)) => {
            Ok((own, false))
        }
        Some(_) => Err(ClaimError::Taken(key.clone())),
        None => {
            let mut row = OrgHost {
                id: crate::sql::Auto::Unset,
                org_id,
                hostname: name,
                enabled: true,
                created_at: crate::sql::Auto::Unset,
            };
            match row.insert_tx(tx).await {
                Err(e) if e.is_unique_violation() => return Err(ClaimError::Taken(key.clone())),
                r => r?,
            }
            Ok((row, true))
        }
    }
}

/// Does a tenant other than `except` hold `key`? Plain reads, so MySQL's
/// snapshot starts after the lock. `iexact`, as [`host_claimed`]: a row
/// stored before lowercasing clashes too. `own_row` is the claim's lock row.
async fn held_elsewhere(
    tx: &mut crate::sql::PoolTx<'_>,
    key: &ClaimKey,
    own_row: Option<i64>,
    except: Option<i64>,
) -> Result<bool, crate::sql::ExecError> {
    use crate::sql::FetcherTx as _;
    let filter = match key {
        ClaimKey::Host(h) => super::Org::host_pattern.iexact(h.clone()),
        ClaimKey::PathPrefix(p) => super::Org::path_prefix.iexact(p.clone()),
        ClaimKey::Port(n) => super::Org::port.eq(Some(*n)),
    };
    let orgs: Vec<super::Org> = super::Org::objects().where_(filter).fetch_tx(tx).await?;
    if other_org(&orgs, except) {
        return Ok(true);
    }
    let ClaimKey::Host(h) = key else {
        return Ok(false);
    };
    let extra: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::hostname.iexact(h.clone()))
        .fetch_tx(tx)
        .await?;
    Ok(extra
        .iter()
        .any(|r| r.id.get().copied() != own_row && except != Some(r.org_id)))
}

/// Insert a new `org`, claiming its host, path prefix and port in the same
/// transaction.
///
/// # Errors
/// The first value another tenant holds, or the INSERT's driver failure.
pub(crate) async fn insert_org(
    registry: &Pool,
    org: &mut super::Org,
) -> Result<(), super::TenancyError> {
    let keys = routing_keys(org);
    if keys.is_empty() {
        org.insert_pool(registry).await?;
        return Ok(());
    }
    let tx = claim_routing(registry, &keys, Claimant::NewOrg(org))
        .await
        .map_err(ClaimError::into_tenancy)?;
    tx.commit().await?;
    Ok(())
}

/// The routing values `org` holds alone.
pub(crate) fn routing_keys(org: &super::Org) -> Vec<ClaimKey> {
    let host = org.host_pattern.clone().map(ClaimKey::Host);
    let prefix = org.path_prefix.clone().map(ClaimKey::PathPrefix);
    let port = org.port.map(ClaimKey::Port);
    [host, prefix, port].into_iter().flatten().collect()
}

/// Is `prefix` another tenant's path prefix? Two would route by row order.
///
/// # Errors
/// Driver / query failures.
pub(crate) async fn prefix_claimed(
    registry: &Pool,
    prefix: &str,
    except_org: Option<i64>,
) -> Result<bool, crate::sql::ExecError> {
    let rows: Vec<super::Org> = super::Org::objects()
        .where_(super::Org::path_prefix.iexact(prefix))
        .fetch(registry)
        .await?;
    Ok(other_org(&rows, except_org))
}

/// Is `port` another tenant's listener port?
///
/// # Errors
/// Driver / query failures.
pub(crate) async fn port_claimed(
    registry: &Pool,
    port: i32,
    except_org: Option<i64>,
) -> Result<bool, crate::sql::ExecError> {
    let rows: Vec<super::Org> = super::Org::objects()
        .where_(super::Org::port.eq(Some(port)))
        .fetch(registry)
        .await?;
    Ok(other_org(&rows, except_org))
}

/// Does another schema-mode tenant live in `schema`, by name or slug default?
/// A purge drops the schema, so two tenants must never share one (#2290).
///
/// # Errors
/// Driver / query failures.
pub(crate) async fn schema_claimed(
    registry: &Pool,
    schema: &str,
    except_org: Option<i64>,
) -> Result<bool, crate::sql::ExecError> {
    let mut rows: Vec<super::Org> = super::Org::objects()
        .where_(
            super::Org::schema_name
                .eq(Some(schema.to_owned()))
                .or(super::Org::slug.eq(schema.to_owned())),
        )
        .fetch(registry)
        .await?;
    // Exact, and through the one rule: MySQL's `=` ignores case.
    rows.retain(|o| {
        o.storage_mode == super::StorageMode::Schema.as_str() && o.effective_schema() == schema
    });
    Ok(other_org(&rows, except_org))
}

fn other_org(rows: &[super::Org], except_org: Option<i64>) -> bool {
    rows.iter()
        .any(|o| except_org.is_none() || o.id.get().copied() != except_org)
}

/// Unbind an extra hostname.
///
/// Refuses the base host: it lives on the org, not here, so there is
/// nothing to delete — and a tenant with no host is unreachable.
///
/// # Errors
/// [`HostError::IsBaseHost`] for the base host, [`HostError::NotFound`]
/// when no extra row matches.
pub async fn remove_host(registry: &Pool, org_slug: &str, hostname: &str) -> Result<(), HostError> {
    let host = normalize_hostname(hostname)?;
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NoSuchOrg(org_slug.to_owned()));
    };
    if org.host_pattern.as_deref() == Some(host.as_str()) {
        return Err(HostError::IsBaseHost(host));
    }
    let org_id = org.id.get().copied().unwrap_or_default();
    // Scoped to the org on purpose: one tenant must not be able to unbind
    // another's host by guessing the name.
    let Some(row) = OrgHost::objects()
        .where_(OrgHost::hostname.eq(host))
        .where_(OrgHost::org_id.eq(org_id))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    row.delete_pool(registry).await?;
    super::invalidate_host_cache();
    Ok(())
}

/// Enable or disable an extra host without unbinding it.
///
/// # Errors
/// [`HostError::IsBaseHost`], [`HostError::NotFound`], or a driver failure.
pub async fn set_host_enabled(
    registry: &Pool,
    org_slug: &str,
    hostname: &str,
    enabled: bool,
) -> Result<(), HostError> {
    let host = normalize_hostname(hostname)?;
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NoSuchOrg(org_slug.to_owned()));
    };
    if org.host_pattern.as_deref() == Some(host.as_str()) {
        return Err(HostError::IsBaseHost(host));
    }
    let org_id = org.id.get().copied().unwrap_or_default();
    let Some(mut row) = OrgHost::objects()
        .where_(OrgHost::hostname.eq(host))
        .where_(OrgHost::org_id.eq(org_id))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    row.enabled = enabled;
    row.save_pool(registry).await?;
    super::invalidate_host_cache();
    Ok(())
}

/// A cheap fingerprint of the host table, used to spot a change made by
/// **another process**.
///
/// `invalidate_host_cache` clears only the pod that called it. Behind a
/// load balancer the other pods would serve a stale answer until their
/// TTL ran out. Each pod re-reads this fingerprint and drops its cache
/// when the value moves.
///
/// The terms are `(count, enabled, enabled_id_sum, max_id)`, which need
/// no clock and no new column. They cover every mutation here:
///
/// | mutation             | what moves                  |
/// |----------------------|-----------------------------|
/// | [`add_host`]         | `count`, `max_id`           |
/// | [`remove_host`]      | `count`                     |
/// | [`set_host_enabled`] | `enabled`, `enabled_id_sum` |
///
/// `max(updated_at)` would be the obvious choice and is wrong twice
/// over. It is not monotonic across pods, because `auto_now` takes the
/// database clock on INSERT but the writer's clock on UPDATE, so a pod
/// with a slow clock can write a timestamp below the current max. It
/// also needs a new `NOT NULL` column, which SQLite refuses to add to a
/// populated table.
///
/// `enabled_id_sum` is there because a plain enabled count cancels out:
/// disable one host and enable another in the same interval and the
/// count returns to where it started. Summing the ids of enabled rows
/// says *which* rows are on. `SUM` is cast to `bigint`, so it decodes
/// as `i64` on all three backends.
///
/// Two id sets with equal sums, swapped in one interval, still collide;
/// the 30s `HOST_CACHE` TTL catches that, so the cost is slower
/// convergence, never permanent staleness.
///
/// **If you add a mutation** that edits a row in place without changing
/// the row count or the enabled set — renaming a hostname, say — add a
/// term here too, or other pods will miss the change.
///
/// # Errors
/// Driver / query failures.
pub async fn generation(registry: &Pool) -> Result<Generation, HostError> {
    use crate::core::aggregates::{count_all, max, sum};
    use crate::core::{AggregateQuery, Model as _, WhereExpr};
    use crate::sql::fetch_aggregate_pool;

    // One round trip, four scalars, no rows decoded.
    let q = AggregateQuery {
        model: OrgHost::SCHEMA,
        joins: Vec::new(),
        where_clause: WhereExpr::And(Vec::new()),
        group_by: Vec::new(),
        aggregates: vec![
            ("n".into(), count_all().into()),
            (
                "n_on".into(),
                count_all().filter(OrgHost::enabled.eq(true)).into(),
            ),
            ("max_id".into(), max("id").into()),
            (
                "on_id_sum".into(),
                sum("id").filter(OrgHost::enabled.eq(true)).into(),
            ),
        ],
        aliases: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source: None,
    };
    // `MAX` and `SUM` are NULL on an empty table, so decode as `Option`
    // and fold to 0. An empty registry gets a stable fingerprint.
    let rows: Vec<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)> =
        fetch_aggregate_pool(registry, &q).await?;
    let (n, n_on, max_id, on_id_sum) = rows.into_iter().next().unwrap_or((None, None, None, None));
    Ok(Generation {
        count: n.unwrap_or(0),
        enabled: n_on.unwrap_or(0),
        max_id: max_id.unwrap_or(0),
        enabled_id_sum: on_id_sum.unwrap_or(0),
    })
}

/// The fingerprint [`generation`] returns. A named struct, not a tuple,
/// because four `i64`s in a row are easy to transpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    /// Total rows. Moves on add and remove.
    pub count: i64,
    /// Rows with `enabled = true`. Moves on a toggle.
    pub enabled: i64,
    /// Largest row id. Catches an add plus a remove in one interval,
    /// which leaves `count` unchanged.
    pub max_id: i64,
    /// Sum of the enabled rows' ids. Says which rows are on, so
    /// swapping one for another still moves the fingerprint.
    pub enabled_id_sum: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_bare_hostname_and_lowercases_it() {
        assert_eq!(normalize_hostname("Example.COM").unwrap(), "example.com");
        assert_eq!(
            normalize_hostname("  a.b.example.com  ").unwrap(),
            "a.b.example.com"
        );
        assert_eq!(
            normalize_hostname("my-site.co.uk").unwrap(),
            "my-site.co.uk"
        );
        // A bare label is legal — `localhost`, or an internal name.
        assert_eq!(normalize_hostname("localhost").unwrap(), "localhost");
    }

    /// Each of these would insert happily and then never match the `Host`
    /// header, which reads as "the feature is broken" rather than "the
    /// input was wrong". Reject at the door instead.
    #[test]
    fn rejects_anything_that_is_not_a_bare_host() {
        for bad in [
            "",
            "   ",
            "https://example.com", // scheme
            "example.com/",        // path
            "example.com/blog",    // path
            "example.com:8080",    // port
            "exa mple.com",        // whitespace
            ".example.com",        // leading dot
            "example.com.",        // trailing dot
            "exam_ple.com",        // underscore is not a hostname char
            "exam?ple.com",
        ] {
            assert!(
                normalize_hostname(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_an_over_long_hostname() {
        assert!(normalize_hostname(&"a".repeat(256)).is_err());
        assert!(normalize_hostname(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn base_host_is_flagged_so_callers_can_refuse_a_remove() {
        let base = TenantHost {
            hostname: "acme.example.com".into(),
            is_base: true,
            enabled: true,
            id: None,
        };
        // The base has no row id precisely because it has no row — that is
        // what makes it undeletable through this table.
        assert!(base.is_base);
        assert!(base.id.is_none());
    }
}

/// Drop the resolver caches after a generic admin or REST write to
/// `table`, so the next request sees the row the write committed.
pub(crate) fn routing_written(table: &str) {
    use crate::core::Model as _;
    if table == super::Org::SCHEMA.table {
        super::invalidate_org_cache();
    } else if table == OrgHost::SCHEMA.table {
        super::invalidate_host_cache();
    }
}

/// What the generic admin's write of a `rustango_orgs` or
/// `rustango_org_hosts` row claims, so it cannot take another tenant's
/// host, path prefix or port.
#[cfg(feature = "admin")]
pub(crate) enum AdminClaims {
    /// Another table, or no routing value changed.
    Nothing,
    /// An edited tenant's new values, claimed before its UPDATE.
    OrgBefore(Vec<ClaimKey>),
    /// A new tenant's values, claimed once its INSERT gave it an id.
    OrgAfter(Vec<ClaimKey>),
    /// An extra host: its written row is the lock; checked after the write.
    HostAfter(String),
}

#[cfg(feature = "admin")]
impl AdminClaims {
    /// Validate and normalize `values` as the console does, and say what
    /// to claim. `before` is the stored row on an edit.
    ///
    /// # Errors
    /// The validator's message, for the form.
    pub(crate) fn of(
        table: &str,
        values: &mut [(&'static str, crate::core::SqlValue)],
        before: Option<&serde_json::Value>,
    ) -> Result<Self, String> {
        use crate::core::SqlValue;
        use crate::tenancy::provision::{
            validate_host_pattern, validate_new_path_prefix, validate_port,
        };
        let changed = |column: &str, now: &serde_json::Value| {
            before.is_none_or(|row| row.get(column).unwrap_or(&serde_json::Value::Null) != now)
        };
        let text = |v: &SqlValue| match v {
            SqlValue::String(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        };
        if table == <super::Org as crate::core::Model>::SCHEMA.table {
            let mut keys = Vec::new();
            for (column, value) in values.iter_mut() {
                let key = match *column {
                    "host_pattern" => text(value)
                        .map(|h| validate_host_pattern(&h).map(ClaimKey::Host))
                        .transpose()?,
                    // A kept prefix passes: one set before a console route took it stays editable.
                    "path_prefix" => text(value)
                        .map(|p| {
                            let p = p.trim().to_owned();
                            if changed(column, &serde_json::json!(p)) {
                                validate_new_path_prefix(&p)?;
                            }
                            Ok::<_, String>(ClaimKey::PathPrefix(p))
                        })
                        .transpose()?,
                    "port" => match value {
                        SqlValue::I32(n) => {
                            validate_port(*n)?;
                            Some(ClaimKey::Port(*n))
                        }
                        _ => None,
                    },
                    _ => continue,
                };
                // Stored as claimed: a host matches the `Host` header byte for byte.
                *value = match &key {
                    Some(ClaimKey::Host(h) | ClaimKey::PathPrefix(h)) => {
                        SqlValue::String(h.clone())
                    }
                    Some(ClaimKey::Port(n)) => SqlValue::I32(*n),
                    None => SqlValue::Null,
                };
                let now = match &key {
                    Some(ClaimKey::Host(h) | ClaimKey::PathPrefix(h)) => serde_json::json!(h),
                    Some(ClaimKey::Port(n)) => serde_json::json!(n),
                    None => serde_json::Value::Null,
                };
                if let Some(key) = key.filter(|_| changed(column, &now)) {
                    keys.push(key);
                }
            }
            return Ok(match (keys.is_empty(), before.is_some()) {
                (true, _) => Self::Nothing,
                (false, true) => Self::OrgBefore(keys),
                (false, false) => Self::OrgAfter(keys),
            });
        }
        if table == <OrgHost as crate::core::Model>::SCHEMA.table {
            let mut host = None;
            let mut moved = false;
            for (column, value) in values.iter_mut() {
                match (*column, &*value) {
                    ("hostname", SqlValue::String(h)) => {
                        let h = normalize_hostname(h).map_err(|e| e.to_string())?;
                        moved |= changed(column, &serde_json::json!(h));
                        *value = SqlValue::String(h.clone());
                        host = Some(h);
                    }
                    ("org_id", SqlValue::I64(id)) => {
                        moved |= changed(column, &serde_json::json!(id))
                    }
                    _ => {}
                }
            }
            return Ok(match host {
                Some(h) if moved => Self::HostAfter(h),
                _ => Self::Nothing,
            });
        }
        Ok(Self::Nothing)
    }

    /// [`Self::of`] for an edit without its stored row: every routing value
    /// sent is claimed, before the UPDATE.
    ///
    /// # Errors
    /// As [`Self::of`].
    pub(crate) fn of_edit(
        table: &str,
        values: &mut [(&'static str, crate::core::SqlValue)],
    ) -> Result<Self, String> {
        Ok(match Self::of(table, values, None)? {
            Self::OrgAfter(keys) => Self::OrgBefore(keys),
            other => other,
        })
    }

    /// Claim all of it in `tx`, already open and written: no retry, and on
    /// MySQL only as fresh as `tx`'s first plain read.
    ///
    /// # Errors
    /// As [`claim_routing_tx`].
    pub(crate) async fn claim_in(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        pk: &crate::core::SqlValue,
    ) -> Result<(), ClaimError> {
        match (self, pk_id(pk)) {
            (Self::OrgBefore(keys), Some(id)) => claim_routing_tx(tx, keys, id).await,
            _ => self.after_write(tx, pk).await,
        }
    }

    /// Whether a write deadlocked on this claim is worth one retry.
    pub(crate) fn retries(&self) -> bool {
        !matches!(self, Self::Nothing)
    }

    /// The write's transaction, an edited tenant's values already claimed.
    ///
    /// # Errors
    /// As [`claim_routing`].
    pub(crate) async fn begin(
        &self,
        registry: &Pool,
        pk: &crate::core::SqlValue,
    ) -> Result<crate::sql::PoolTx<'static>, ClaimError> {
        match (self, pk_id(pk)) {
            (Self::OrgBefore(keys), Some(id)) => {
                claim_routing(registry, keys, Claimant::Base(id)).await
            }
            _ => Ok(crate::sql::write_transaction(registry).await?),
        }
    }

    /// Claim or check what the write in `tx` could only claim once written.
    ///
    /// # Errors
    /// As [`claim_routing_tx`].
    pub(crate) async fn after_write(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        pk: &crate::core::SqlValue,
    ) -> Result<(), ClaimError> {
        let Some(id) = pk_id(pk) else {
            return Ok(());
        };
        match self {
            Self::OrgAfter(keys) => claim_routing_tx(tx, keys, id).await,
            Self::HostAfter(h) => {
                let key = ClaimKey::Host(h.clone());
                if held_elsewhere(tx, &key, Some(id), None).await? {
                    return Err(ClaimError::Taken(key));
                }
                Ok(())
            }
            Self::Nothing | Self::OrgBefore(_) => Ok(()),
        }
    }
}

#[cfg(feature = "admin")]
fn pk_id(pk: &crate::core::SqlValue) -> Option<i64> {
    match pk {
        crate::core::SqlValue::I64(n) => Some(*n),
        crate::core::SqlValue::I32(n) => Some(i64::from(*n)),
        _ => None,
    }
}
