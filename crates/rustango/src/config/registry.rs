//! Every settings key, who reads it, and which env vars win over it.
//!
//! A test walks the [`Settings`] type and fails when a field has no row
//! here, so a new key cannot ship without a named reader.

use super::sections::Settings;

/// Who reads a key.
#[derive(Clone, Copy)]
pub(crate) enum Reader {
    /// The framework applies it.
    Framework,
    /// Only the named constructor reads it, and the app calls that.
    App(&'static str),
    /// Nothing reads it. `instead` is where to configure it; `set` says
    /// whether a loaded [`Settings`] has it.
    Unread {
        instead: &'static str,
        // Read by the boot warning, which needs `manage`.
        #[cfg_attr(not(any(test, feature = "manage")), allow(dead_code))]
        set: fn(&Settings) -> bool,
    },
}

/// One settings key.
pub(crate) struct Key {
    /// `section.key`, as in `[section] key`.
    pub(crate) path: &'static str,
    pub(crate) reader: Reader,
    /// Its value is never printed.
    pub(crate) secret: bool,
    /// Bare env vars that win over the file and `RUSTANGO__*`.
    pub(crate) env: &'static [&'static str],
}

const fn key(path: &'static str, reader: Reader) -> Key {
    Key {
        path,
        reader,
        secret: false,
        env: &[],
    }
}

const fn secret(path: &'static str, reader: Reader) -> Key {
    Key {
        path,
        reader,
        secret: true,
        env: &[],
    }
}

const F: Reader = Reader::Framework;
const ADMIN: Reader = Reader::App("admin::Builder::from_settings");
const CACHE: Reader = Reader::App("cache::from_settings_async");
const JOBS: Reader = Reader::App("jobs::inmemory_from_settings");
const MAIL: Reader = Reader::App("email::from_settings");
const JWT: Reader = Reader::App("auth_routes::Config::with_jwt_settings");
const I18N: Reader = Reader::App("i18n::Translator::from_settings");
const MCP: Reader = Reader::App("mcp::secure_tenant_router_from_settings");
const STORAGE: Reader = Reader::App("storage::StorageRegistry::from_settings");

/// SSO providers live in the database, per tenant.
const SSO: &str = "add an SSO provider in the admin (per tenant in a tenancy app)";

const fn sso(path: &'static str, set: fn(&Settings) -> bool) -> Key {
    key(path, Reader::Unread { instead: SSO, set })
}

/// Every key of [`Settings`], in section order.
pub(crate) const KEYS: &[Key] = &[
    Key {
        path: "database.url",
        reader: F,
        secret: true,
        env: &[
            "DATABASE_URL",
            "DB_DRIVER",
            "DB_HOST",
            "DB_PORT",
            "DB_USER",
            "DB_PASSWORD",
            "DB_PASSWORD_FILE",
            "DB_NAME",
            "DB_PARAMS",
        ],
    },
    key("database.backend", F),
    key("database.pool_max_size", F),
    key("database.pool_min_size", F),
    key("database.pool_acquire_timeout_secs", F),
    key("database.pool_idle_timeout_secs", F),
    key("database.pool_max_lifetime_secs", F),
    key("database.pool_test_before_acquire", F),
    key("database.pool_idle_ping_secs", F),
    key("admin.allowed_tables", ADMIN),
    key("admin.read_only_tables", ADMIN),
    key("admin.title", ADMIN),
    key("admin.subtitle", ADMIN),
    key("admin.logo_url", ADMIN),
    key(
        "admin.primary_color",
        Reader::Unread {
            instead: "[brand] primary_color (operator console)",
            set: |s| s.admin.primary_color.is_some(),
        },
    ),
    key("admin.theme_mode", ADMIN),
    key("admin.url_prefix", ADMIN),
    key(
        "admin.csrf_cookie_secure",
        Reader::Unread {
            instead: "[security] secure_cookies",
            set: |s| s.admin.csrf_cookie_secure.is_some(),
        },
    ),
    key(
        "admin.session_timeout_minutes",
        Reader::Unread {
            instead: "nothing yet: the admin has no idle timeout",
            set: |s| s.admin.session_timeout_minutes.is_some(),
        },
    ),
    Key {
        path: "tenancy.apex_domain",
        reader: F,
        secret: false,
        env: &["RUSTANGO_APEX_DOMAIN"],
    },
    key("cache.backend", CACHE),
    secret("cache.redis_url", CACHE),
    key("cache.file_cache_dir", CACHE),
    key("jobs.backend", JOBS),
    key("jobs.concurrency", JOBS),
    key("mail.backend", MAIL),
    key("mail.smtp_host", MAIL),
    key("mail.smtp_port", MAIL),
    key("mail.smtp_username", MAIL),
    secret("mail.smtp_password", MAIL),
    key("mail.smtp_tls", MAIL),
    key("mail.smtp_timeout_secs", MAIL),
    key("mail.from_address", MAIL),
    key("mail.server_email", MAIL),
    key("mail.email_subject_prefix", MAIL),
    key("mail.admins", MAIL),
    key("mail.managers", MAIL),
    key("mail.file_email_dir", MAIL),
    Key {
        path: "server.bind",
        reader: F,
        secret: false,
        env: &["RUSTANGO_BIND"],
    },
    key("server.request_timeout_secs", F),
    key("server.max_body_bytes", F),
    key("server.shutdown_timeout_secs", F),
    key("server.migrate_on_boot", F),
    key("server.migrate_lock_timeout_secs", F),
    key("auth.jwt.access_ttl_secs", JWT),
    key("auth.jwt.refresh_ttl_secs", JWT),
    key(
        "auth.jwt.issuer",
        Reader::Unread {
            instead: "nothing yet: tokens carry no `iss`",
            set: |s| s.auth.jwt.issuer.is_some(),
        },
    ),
    key(
        "auth.jwt.audience",
        Reader::Unread {
            instead: "nothing yet: tokens carry no `aud`",
            set: |s| s.auth.jwt.audience.is_some(),
        },
    ),
    key("auth.argon2_memory_kib", F),
    key("auth.argon2_iterations", F),
    key("auth.argon2_parallelism", F),
    key("auth.lockout_threshold", F),
    key("auth.lockout_duration_secs", F),
    key("auth.login_ip_limit", F),
    key("auth.login_ip_window_secs", F),
    key("auth.login_global_limit", F),
    key("auth.login_global_window_secs", F),
    key("auth.hash_wait_ms", F),
    key("auth.login_field", F),
    key("brand.name", F),
    key("brand.tagline", F),
    key("brand.logo_url", F),
    key("brand.logo_path", F),
    key("brand.primary_color", F),
    key("brand.theme_mode", F),
    key("security.headers_preset", F),
    key("security.csp", F),
    key("security.hsts_max_age_secs", F),
    key("security.cors_allowed_origins", F),
    key("security.cors_allowed_headers", F),
    key("security.cors_expose_headers", F),
    key("security.cors_allow_credentials", F),
    key("security.allowed_hosts", F),
    key("security.csrf_trusted_origins", F),
    key("security.secure_ssl_redirect", F),
    key("security.secure_redirect_exempt", F),
    key("security.secure_proxy_ssl_header", F),
    key("security.secure_cookies", F),
    key("routes.legacy_preset", F),
    key("routes.login_url", F),
    key("routes.logout_url", F),
    key("routes.admin_url", F),
    key("routes.audit_url", F),
    key("routes.static_url", F),
    key("routes.brand_url", F),
    key("routes.change_password_url", F),
    key("routes.impersonation_handoff_url", F),
    key("audit.retention_days", F),
    key("audit.redact_query_params", F),
    key("logging.level", F),
    key("logging.format", F),
    key("logging.color", F),
    key("logging.with_thread_ids", F),
    key("logging.with_line_numbers", F),
    key("logging.without_targets", F),
    key("logging.file_dir", F),
    key("logging.file_prefix", F),
    key("logging.file_rotation", F),
    key("logging.file_only", F),
    key("logging.access_log", F),
    key("i18n.default_locale", I18N),
    key("i18n.languages", I18N),
    key("i18n.locale_paths", I18N),
    key("i18n.fallback_chain", I18N),
    key("mcp.prefix", MCP),
    key("mcp.token_ttl_secs", MCP),
    key("mcp.enable_sse", MCP),
    key("mcp.allowed_origins", MCP),
    key("mcp.rate_limit_per_minute", MCP),
    key("mcp.max_tools_listed", MCP),
    key("mcp.max_body_bytes", MCP),
    key("storage.default", STORAGE),
    // Disk entries hold account keys.
    secret("storage.disks", STORAGE),
    key("errors.enabled", F),
    key("errors.templates", F),
    key("errors.htmx_target", F),
    sso("sso.enabled", |s| s.sso.enabled.is_some()),
    sso("sso.provider", |s| s.sso.provider.is_some()),
    sso("sso.issuer_url", |s| s.sso.issuer_url.is_some()),
    sso("sso.client_id", |s| s.sso.client_id.is_some()),
    Key {
        path: "sso.client_secret",
        reader: Reader::Unread {
            instead: SSO,
            set: |s| s.sso.client_secret.is_some(),
        },
        secret: true,
        env: &[],
    },
    sso("sso.redirect_uri", |s| s.sso.redirect_uri.is_some()),
];

#[cfg(feature = "manage")]
impl Key {
    /// `[section] key`, as written in the settings file.
    pub(crate) fn toml_name(&self) -> String {
        match self.path.rsplit_once('.') {
            Some((section, key)) => format!("[{section}] {key}"),
            None => self.path.to_owned(),
        }
    }

    /// The boot warning for a set key nothing reads.
    pub(crate) fn unread_message(&self) -> String {
        let instead = match self.reader {
            Reader::Unread { instead, .. } => instead,
            _ => "",
        };
        format!(
            "`{}` is set but nothing reads it; {instead}",
            self.toml_name()
        )
    }
}

/// The row for `path`, or for the map it sits in (`storage.disks.x.y`).
pub(crate) fn find(path: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| {
        path == k.path
            || path
                .strip_prefix(k.path)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

impl Settings {
    /// Keys that are set but that nothing reads, so boot can say so.
    #[cfg(any(test, feature = "manage"))]
    pub(crate) fn inert_keys(&self) -> Vec<&'static Key> {
        KEYS.iter()
            .filter(|k| matches!(k.reader, Reader::Unread { set, .. } if set(self)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::{self, Deserialize, IntoDeserializer, Visitor};
    use std::cell::RefCell;

    /// A deserializer that walks [`Settings`]' own shape: it records each
    /// leaf path, and sets only `target` (every other `Option` is `None`).
    struct Probe<'a> {
        path: String,
        target: Option<&'a str>,
        seen: &'a RefCell<Vec<String>>,
    }

    type E = de::value::Error;

    impl Probe<'_> {
        fn leaf(&self) {
            self.seen.borrow_mut().push(self.path.clone());
        }
    }

    macro_rules! leaf {
        ($($name:ident => $visit:ident($($v:expr)?)),* $(,)?) => {$(
            fn $name<V: Visitor<'de>>(self, v: V) -> Result<V::Value, E> {
                self.leaf();
                v.$visit($($v)?)
            }
        )*};
    }

    impl<'de> de::Deserializer<'de> for Probe<'_> {
        type Error = E;

        leaf! {
            deserialize_any => visit_unit(),
            deserialize_bool => visit_bool(true),
            deserialize_i8 => visit_i64(1),
            deserialize_i16 => visit_i64(1),
            deserialize_i32 => visit_i64(1),
            deserialize_i64 => visit_i64(1),
            deserialize_u8 => visit_u64(1),
            deserialize_u16 => visit_u64(1),
            deserialize_u32 => visit_u64(1),
            deserialize_u64 => visit_u64(1),
            deserialize_f32 => visit_f64(1.0),
            deserialize_f64 => visit_f64(1.0),
            deserialize_char => visit_char('x'),
            deserialize_str => visit_str("x"),
            deserialize_string => visit_str("x"),
            deserialize_bytes => visit_bytes(b"x"),
            deserialize_byte_buf => visit_bytes(b"x"),
            deserialize_unit => visit_unit(),
            deserialize_identifier => visit_str("x"),
            deserialize_ignored_any => visit_unit(),
        }

        fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, E> {
            self.leaf();
            if self.target == Some(self.path.as_str()) {
                v.visit_some(self)
            } else {
                v.visit_none()
            }
        }

        fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value, E> {
            self.leaf();
            v.visit_seq(de::value::SeqDeserializer::<_, E>::new(
                std::iter::empty::<()>(),
            ))
        }

        fn deserialize_map<V: Visitor<'de>>(self, v: V) -> Result<V::Value, E> {
            self.leaf();
            v.visit_map(de::value::MapDeserializer::<_, E>::new(
                std::iter::empty::<((), ())>(),
            ))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _: &'static str,
            variants: &'static [&'static str],
            v: V,
        ) -> Result<V::Value, E> {
            self.leaf();
            v.visit_enum(variants[0].into_deserializer())
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            v: V,
        ) -> Result<V::Value, E> {
            v.visit_map(Fields {
                probe: self,
                fields,
                next: 0,
            })
        }

        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            v: V,
        ) -> Result<V::Value, E> {
            v.visit_newtype_struct(self)
        }

        serde::forward_to_deserialize_any! {
            i128 u128 unit_struct tuple tuple_struct
        }
    }

    struct Fields<'a> {
        probe: Probe<'a>,
        fields: &'static [&'static str],
        next: usize,
    }

    impl<'de> de::MapAccess<'de> for Fields<'_> {
        type Error = E;

        fn next_key_seed<K: de::DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, E> {
            let Some(name) = self.fields.get(self.next) else {
                return Ok(None);
            };
            seed.deserialize((*name).into_deserializer()).map(Some)
        }

        fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, E> {
            let name = self.fields[self.next];
            self.next += 1;
            let path = if self.probe.path.is_empty() {
                name.to_owned()
            } else {
                format!("{}.{name}", self.probe.path)
            };
            seed.deserialize(Probe {
                path,
                target: self.probe.target,
                seen: self.probe.seen,
            })
        }
    }

    fn probe(target: Option<&str>) -> (Settings, Vec<String>) {
        let seen = RefCell::new(Vec::new());
        let s = Settings::deserialize(Probe {
            path: String::new(),
            target,
            seen: &seen,
        })
        .expect("Settings deserializes from the probe");
        let mut paths = seen.into_inner();
        paths.dedup();
        (s, paths)
    }

    /// A new settings field must name its reader here.
    #[test]
    fn every_settings_field_has_a_registry_row() {
        let (_, fields) = probe(None);
        let rows: Vec<&str> = KEYS.iter().map(|k| k.path).collect();
        let missing: Vec<&String> = fields
            .iter()
            .filter(|f| !rows.contains(&f.as_str()))
            .collect();
        let stale: Vec<&&str> = rows
            .iter()
            .filter(|r| !fields.iter().any(|f| f == *r))
            .collect();
        assert!(
            missing.is_empty(),
            "settings fields with no row in config/registry.rs KEYS: {missing:?}"
        );
        assert!(stale.is_empty(), "registry rows with no field: {stale:?}");
        assert_eq!(rows.len(), fields.len(), "duplicate registry rows");
    }

    /// Each unread key's `set` reads its own field, and only that one.
    #[test]
    fn unread_keys_check_their_own_field() {
        let unread: Vec<&Key> = KEYS
            .iter()
            .filter(|k| matches!(k.reader, Reader::Unread { .. }))
            .collect();
        assert!(unread.len() >= 11, "{}", unread.len());
        for k in unread {
            let (s, _) = probe(Some(k.path));
            let inert: Vec<&str> = s.inert_keys().iter().map(|k| k.path).collect();
            assert_eq!(inert, [k.path], "{}", k.path);
        }
        assert!(Settings::default().inert_keys().is_empty());
    }

    #[test]
    fn find_maps_a_disk_key_to_its_map() {
        assert_eq!(
            find("storage.disks.uploads.account_key").map(|k| k.path),
            Some("storage.disks")
        );
        assert_eq!(find("database.url").map(|k| k.path), Some("database.url"));
        assert!(find("database.urls").is_none());
    }
}
