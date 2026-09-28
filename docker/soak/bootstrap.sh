#!/bin/sh
# Bring one app instance's database to the state the soak expects, then
# exit 0. Every web and worker container waits on that exit via
# `service_completed_successfully`.
#
#   bootstrap.sh single    migrate only
#   bootstrap.sh saas      migrate, then operator + SOAK_TENANTS tenants
#
# Re-runnable on purpose: `up` after a crash must not need a volume
# wipe. `migrate` is idempotent, and tenants are created only when
# absent (`create-tenant` itself errors on an existing slug, so the
# check is done here).

set -eu

MODE="${1:?usage: bootstrap.sh single|saas}"
TENANTS="${SOAK_TENANTS:-20}"
TENANT_MODE="${SOAK_TENANT_MODE:-database}"
APEX="${RUSTANGO_APEX_DOMAIN:-soak.localhost}"

log() { echo "[bootstrap:$MODE] $*"; }

# ---------------------------------------------------------------- waiting
#
# `depends_on: service_healthy` covers the server being up, but a
# freshly-created MySQL database is not necessarily reachable the
# instant the server reports healthy. Retry rather than race.
wait_for_db() {
    i=0
    while [ "$i" -lt 60 ]; do
        if app db:info >/dev/null 2>&1; then
            return 0
        fi
        i=$((i + 1))
        sleep 2
    done
    log "database never became reachable at DATABASE_URL"
    return 1
}

# ------------------------------------------------------------- migrations

log "migrating"
wait_for_db
app migrate
log "migrate done"

if [ "$MODE" = "single" ]; then
    # The admin login the driver and the browser agent use, plus a
    # second account the login-lock checks may lock (#1609).
    app create-admin soakadmin --password "${SOAK_ADMIN_PASSWORD:-soak-admin-pw}" --superuser \
        2>/dev/null || log "admin soakadmin already exists"
    app create-admin lockprobe --password "${SOAK_LOCKPROBE_PASSWORD:-soak-lockprobe-pw}" \
        2>/dev/null || log "admin lockprobe already exists"
    log "single-tenant: done"
    exit 0
fi

# ------------------------------------------------------------ tenant prep
#
# Nothing to do here, deliberately.
#
# rustango does NOT create tenant databases — `provision.rs` says
# "database-mode tenants bring their own database", and preflight
# *creates and drops a table* to prove migrations can run, so a missing
# database is a hard provisioning failure rather than a warning.
#
# But this container is the app image: debian-slim with curl and the two
# app binaries. Shipping psql and the MySQL client into a
# production-shaped image so a test script can issue CREATE DATABASE is
# the wrong trade — the image is meant to be the thing you would deploy.
#
# So the databases are created by `initdb/{pg,my}.sql`, mounted into the
# database containers and run once when their volumes initialise:
#
#   postgres schema mode -> only the registry DB; the provisioning
#                           engine issues CREATE SCHEMA per tenant
#   mysql   database mode -> registry + all twenty tenant databases,
#                            plus one wildcard grant
#   sqlite  database mode -> nothing at all; `?mode=rwc` creates files
#
# The consequence to remember: changing SOAK_TENANTS above 20 on MySQL
# needs `initdb/my.sql` extended and the volume recreated, because an
# initdb script only runs on a fresh volume.

# --------------------------------------------------------------- operator

log "creating operator"
# Idempotent by check: `create-operator` errors on an existing username.
if ! app list-tenants >/dev/null 2>&1; then
    log "registry not ready after migrate — aborting"
    exit 1
fi
app create-operator soakops --password "${SOAK_OPERATOR_PASSWORD:-soak-operator-pw}" \
    2>/dev/null || log "operator already exists"
# Operators the checks may lock or re-password, so `soakops` stays usable.
for op in lockops pwops; do
    app create-operator "$op" --password "soak-${op}-pw" 2>/dev/null || log "operator $op already exists"
done

# ----------------------------------------------------------------- tenants
#
# Two of them get a non-subdomain host_pattern, so `SubdomainResolver`
# misses and `RegisteredHostResolver` — the middle link of the default
# resolver chain — is actually covered by the run.

existing=$(app list-tenants 2>/dev/null || true)

i=1
created=0
while [ "$i" -le "$TENANTS" ]; do
    slug=$(printf 't%02d' "$i")
    if echo "$existing" | grep -q "\\b${slug}\\b"; then
        i=$((i + 1))
        continue
    fi

    case "$TENANT_MODE" in
    schema)
        app create-tenant "$slug" --mode schema --backend postgres \
            --display-name "Tenant $slug" --host-pattern "${slug}.${APEX}"
        ;;
    database)
        case "${DATABASE_URL}" in
        mysql://*)
            turl="mysql://rustango:rustango@my:3306/commerce_saas_${slug}"
            backend=mysql
            ;;
        sqlite:*)
            turl="sqlite:///data/tenant_${slug}.db?mode=rwc"
            backend=sqlite
            ;;
        *)
            turl="postgres://rustango:rustango@pg:5432/commerce_saas_${slug}"
            backend=postgres
            ;;
        esac
        app create-tenant "$slug" --mode database --backend "$backend" \
            --display-name "Tenant $slug" --database-url "$turl" \
            --host-pattern "${slug}.${APEX}"
        ;;
    esac
    created=$((created + 1))
    i=$((i + 1))
done

log "tenants created: $created (of $TENANTS requested)"

# A tenant admin on t01 only: the driver logs in with it, and proves the
# same login is refused on t02 (GHSA-c4gg-mvfq-h268).
app create-user t01 soakadmin --password "${SOAK_TENANT_ADMIN_PASSWORD:-soak-admin-pw}" \
    --superuser 2>/dev/null || log "tenant admin already exists"

# One tenant per check family, so a lock or a password change on one
# never gets in the way of another (or of the browser agent on t01).
#   t03  SSO: a plain user, a staff user, a superuser, a spare
#   t04  login lock and limits      t05  password change (#1338)
#   t06  JWT                        t07  tenant admin CSRF (#1713)
# Superusers first: a tenant's first user is promoted to superuser
# whatever the flags say, and sso-user must stay a plain user.
for pair in "t03 sso-super" "t05 pwchange" "t07 csrfadmin"; do
    slug=${pair%% *}; user=${pair#* }
    app create-user "$slug" "$user" --password "soak-${user}-pw" --superuser 2>/dev/null \
        || log "user $user on $slug already exists"
done
for pair in "t03 sso-user" "t03 sso-staff" "t03 sso-other" "t04 lockprobe" "t06 jwtuser"; do
    slug=${pair%% *}; user=${pair#* }
    app create-user "$slug" "$user" --password "soak-${user}-pw" 2>/dev/null \
        || log "user $user on $slug already exists"
done

# The two custom hostnames. `add-host` is what writes
# `rustango_org_hosts`, which is what `RegisteredHostResolver` reads.
for pair in "t19 shop-t19.example.test" "t20 storefront-t20.example.test"; do
    slug=$(echo "$pair" | cut -d' ' -f1)
    host=$(echo "$pair" | cut -d' ' -f2)
    app add-host "$slug" "$host" 2>/dev/null \
        && log "registered custom host $host -> $slug" \
        || log "custom host $host already present (or tenant missing)"
done

log "done"
