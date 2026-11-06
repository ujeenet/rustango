//! `run-server` verb through [`crate::tenancy::manage::run`]: refused.
//! It would serve without the `[security]` / `[server]` layers and bind
//! that `manage::Cli` applies, so it points there instead.

use crate::tenancy::error::TenancyError;

/// Why `run-server` is not served here.
pub(super) const REFUSAL: &str =
    "run-server is not served by `tenancy::manage::run`: it would skip \
     the [security] and [server] settings (headers, allowed hosts, body limit, timeout, bind). \
     Serve with `rustango::manage::Cli::new().tenancy().with_settings_from_env().run()`, \
     which handles the other verbs too.";

pub(super) fn run_server_cmd() -> Result<(), TenancyError> {
    Err(TenancyError::Validation(REFUSAL.into()))
}

#[cfg(test)]
mod tests {
    /// The bare tenancy dispatcher refuses to serve and names `Cli`.
    #[test]
    fn run_server_points_at_cli() {
        let err = super::run_server_cmd().unwrap_err().to_string();
        assert!(err.contains("manage::Cli::new().tenancy()"), "{err}");
    }
}
