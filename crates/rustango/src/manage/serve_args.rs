//! The flags `run-server` takes through [`super::Cli`]. A flag beats the
//! env var and the settings file; anything else is refused, not dropped.

/// What the arguments after the server verb ask for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ServeArgs {
    Serve {
        bind: Option<String>,
        apex: Option<String>,
    },
    Help(String),
}

/// Parse `args` (after the verb). `--apex` only means something with tenancy.
pub(super) fn parse(args: &[String], tenancy: bool) -> Result<ServeArgs, String> {
    let mut bind = None;
    let mut apex = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (flag.as_str(), None),
        };
        match name {
            "--help" | "-h" if inline.is_none() => return Ok(ServeArgs::Help(usage(tenancy))),
            "--bind" => bind = Some(value(name, inline, &mut iter)?),
            "--apex" | "--apex-domain" if tenancy => apex = Some(value(name, inline, &mut iter)?),
            "--apex" | "--apex-domain" => {
                return Err(format!(
                    "run-server: `{name}` needs a tenancy app (`Cli::tenancy()`)"
                ))
            }
            _ => {
                return Err(format!(
                    "run-server: unknown argument `{flag}`\n{}",
                    usage(tenancy)
                ))
            }
        }
    }
    Ok(ServeArgs::Serve { bind, apex })
}

fn value<'a>(
    name: &str,
    inline: Option<String>,
    iter: &mut impl Iterator<Item = &'a String>,
) -> Result<String, String> {
    match inline.or_else(|| iter.next().cloned()) {
        Some(v) if !v.is_empty() && !v.starts_with("--") => Ok(v),
        _ => Err(format!("run-server: `{name}` needs a value")),
    }
}

fn usage(tenancy: bool) -> String {
    if tenancy {
        "usage: run-server [--bind <addr>] [--apex <domain>]\n  \
         --bind  listen address; beats RUSTANGO_BIND and [server] bind\n  \
         --apex  operator console host; beats RUSTANGO_APEX_DOMAIN and [tenancy] apex_domain"
            .to_owned()
    } else {
        "usage: run-server [--bind <addr>]\n  \
         --bind  listen address; beats RUSTANGO_BIND and [server] bind"
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn bind_and_apex_are_read() {
        let got = parse(
            &args(&["--bind", "127.0.0.1:8099", "--apex=shop.test"]),
            true,
        );
        assert_eq!(
            got,
            Ok(ServeArgs::Serve {
                bind: Some("127.0.0.1:8099".into()),
                apex: Some("shop.test".into()),
            })
        );
        assert_eq!(
            parse(&args(&["--bind=0.0.0.0:1"]), false),
            Ok(ServeArgs::Serve {
                bind: Some("0.0.0.0:1".into()),
                apex: None
            })
        );
    }

    #[test]
    fn unknown_missing_and_misplaced_flags_are_refused() {
        for (a, tenancy) in [
            (&["--bogus"][..], true),
            (&["--bogus"][..], false),
            (&["--bind"][..], false),
            (&["--bind", "--apex", "x"][..], true),
            (&["--apex", "x"][..], false),
            (&["extra"][..], true),
        ] {
            assert!(parse(&args(a), tenancy).is_err(), "{a:?} tenancy={tenancy}");
        }
    }

    #[test]
    fn help_names_only_the_flags_that_apply() {
        let Ok(ServeArgs::Help(t)) = parse(&args(&["--help"]), true) else {
            panic!("help");
        };
        assert!(t.contains("--apex"), "{t}");
        let Ok(ServeArgs::Help(s)) = parse(&args(&["-h"]), false) else {
            panic!("help");
        };
        assert!(s.contains("--bind") && !s.contains("--apex"), "{s}");
    }
}
