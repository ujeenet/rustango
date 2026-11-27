//! `manage` verb arguments: one parser, so every verb refuses an unknown
//! flag the same way.
#![cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]

/// A refused `manage` argument; the message is the whole error.
#[derive(Debug)]
pub(crate) struct ArgError(pub(crate) String);

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ArgError> for crate::migrate::MigrateError {
    fn from(e: ArgError) -> Self {
        Self::Validation(e.0)
    }
}

#[cfg(feature = "tenancy")]
impl From<ArgError> for crate::tenancy::TenancyError {
    fn from(e: ArgError) -> Self {
        Self::Validation(e.0)
    }
}

/// What a verb accepts. Declared once so every verb refuses the same way:
/// an ignored flag runs the bare verb and exits 0 (#1909, #1910).
pub(crate) struct Spec<'a> {
    pub(crate) verb: &'a str,
    pub(crate) usage: &'a str,
    /// Flags without a value (`--dry-run`).
    pub(crate) switches: &'a [&'a str],
    /// Flags that consume the next argument (`--password <p>`).
    pub(crate) valued: &'a [&'a str],
    pub(crate) max_positionals: usize,
}

/// Arguments split by [`parse`]: flag values are never positionals.
#[derive(Debug)]
pub(crate) struct Parsed {
    positionals: Vec<String>,
    switches: Vec<String>,
    values: Vec<(String, String)>,
}

impl Parsed {
    pub(crate) fn positional(&self, i: usize) -> Option<&String> {
        self.positionals.get(i)
    }

    /// Positional `i`, or the usage as the error.
    pub(crate) fn required(&self, i: usize, usage: &str) -> Result<String, ArgError> {
        self.positional(i)
            .cloned()
            .ok_or_else(|| ArgError(format!("usage: {usage}")))
    }

    pub(crate) fn has(&self, flag: &str) -> bool {
        self.switches.iter().any(|s| s == flag)
    }

    /// `--on` / `--off`, refusing both rather than guessing (#1355).
    pub(crate) fn on_off(&self) -> Result<Option<bool>, ArgError> {
        match (self.has("--on"), self.has("--off")) {
            (true, true) => Err(ArgError(
                "--on and --off contradict each other — pass one".into(),
            )),
            (true, false) => Ok(Some(true)),
            (false, true) => Ok(Some(false)),
            (false, false) => Ok(None),
        }
    }

    /// Every value given for `flag`, in order.
    fn values(&self, flag: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(f, _)| f == flag)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The value for `flag`; refuses it given twice rather than guessing.
    pub(crate) fn value(&self, flag: &str) -> Result<Option<&str>, ArgError> {
        match self.values(flag).as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            _ => Err(ArgError(format!("{flag} given twice"))),
        }
    }
}

/// Split `args` per `spec`, refusing unknown flags and extra positionals.
/// `--help` / `-h` return the usage as a validation error, never run the verb.
pub(crate) fn parse(args: &[String], spec: &Spec<'_>) -> Result<Parsed, ArgError> {
    let mut out = Parsed {
        positionals: Vec::new(),
        switches: Vec::new(),
        values: Vec::new(),
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let a = arg.as_str();
        if a == "--help" || a == "-h" {
            return Err(ArgError(spec.usage.to_owned()));
        }
        if spec.switches.contains(&a) {
            out.switches.push(a.to_owned());
        } else if spec.valued.contains(&a) {
            // `--password --superuser` is a missing value, not a password.
            let v = iter
                .next()
                .cloned()
                .ok_or_else(|| ArgError(format!("{a} requires a value")))?;
            if v.starts_with("--") {
                return Err(ArgError(format!("`{a}` needs a value")));
            }
            out.values.push((a.to_owned(), v));
        } else if a.starts_with('-') {
            return Err(ArgError(format!(
                "{}: unknown flag `{a}` — usage: {}",
                spec.verb, spec.usage
            )));
        } else if out.positionals.len() == spec.max_positionals {
            return Err(ArgError(format!(
                "{} does not take `{a}` — usage: {}",
                spec.verb, spec.usage
            )));
        } else {
            out.positionals.push(a.to_owned());
        }
    }
    Ok(out)
}
