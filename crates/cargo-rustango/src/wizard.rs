//! Interactive `cargo rustango new` — numbered menus instead of flags.
//!
//! The wizard decides nothing on its own: every answer sets a field the
//! flags already set, and it prints the equivalent one-line command before
//! creating anything. So there is one code path deciding what a project
//! contains, and using the wizard once teaches the flags for next time.

use std::io::{self, IsTerminal, Write};

use crate::{Backend, NewArgs, Template, OPTIONAL_FEATURES};

const TEMPLATES: &[(&str, Template, &str)] = &[
    (
        "fullstack",
        Template::Fullstack,
        "ORM + auto-admin + forms — the usual starting point",
    ),
    (
        "api",
        Template::Api,
        "bare ORM + axum, no admin UI — for JSON-only services",
    ),
    (
        "tenant",
        Template::Tenant,
        "multi-tenancy: tenant registry + operator console",
    ),
];

const BACKENDS: &[(&str, Backend, &str)] = &[
    (
        "postgres",
        Backend::Postgres,
        "every feature, including schema-mode tenancy",
    ),
    (
        "sqlite",
        Backend::Sqlite,
        "a file beside the project — no server to run",
    ),
    ("mysql", Backend::Mysql, "MySQL 8.0+ / MariaDB"),
];

/// Ask for anything the flags did not already answer, then confirm.
///
/// `given` carries what was parsed from the command line; the wizard only
/// asks about the rest, so `new myapp -i` skips straight to the menus.
pub fn run(given: NewArgs, name_was_given: bool) -> Result<NewArgs, String> {
    if !io::stdin().is_terminal() {
        return Err(
            "--interactive needs a terminal — pass --template / --backend / --features instead"
                .to_owned(),
        );
    }

    println!();
    println!("  rustango — new project");
    println!("  (enter accepts the default; ctrl-c aborts)");

    let name = if name_was_given {
        given.name
    } else {
        ask_name()?
    };

    let template = choose("Template", TEMPLATES.iter().map(|(n, _, a)| (*n, *a)), 0)
        .map(|ix| TEMPLATES[ix].1)?;

    let backend = choose("Database", BACKENDS.iter().map(|(n, _, a)| (*n, *a)), 0)
        .map(|ix| BACKENDS[ix].1)?;

    let features = ask_features(template, &given.features)?;

    let args = NewArgs {
        name,
        template,
        backend,
        features,
        rustango_path: given.rustango_path,
        interactive: false,
    };

    println!();
    println!("  Same thing without the wizard:");
    println!("    {}", equivalent_command(&args));
    println!();
    if !confirm("Create it?")? {
        return Err("cancelled".to_owned());
    }
    Ok(args)
}

/// The non-interactive command line that produces `args`.
fn equivalent_command(args: &NewArgs) -> String {
    let mut cmd = format!(
        "cargo rustango new {} --template {} --backend {}",
        args.name,
        args.template.name(),
        args.backend.feature()
    );
    if !args.features.is_empty() {
        cmd.push_str(&format!(" --features {}", args.features.join(",")));
    }
    cmd
}

fn ask_name() -> Result<String, String> {
    loop {
        let raw = prompt("\n  Project name: ")?;
        let name = raw.trim();
        if name.is_empty() {
            println!("  a name is required");
            continue;
        }
        match crate::validate_name(name) {
            Ok(()) => return Ok(name.to_owned()),
            Err(e) => println!("  {e}"),
        }
    }
}

/// One-of-N. Returns the index into `options`.
fn choose<'a>(
    title: &str,
    options: impl Iterator<Item = (&'a str, &'a str)>,
    default_ix: usize,
) -> Result<usize, String> {
    let options: Vec<(&str, &str)> = options.collect();
    let width = options.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let iw = options.len().to_string().len();
    println!();
    println!("  {title}");
    for (i, (name, about)) in options.iter().enumerate() {
        let marker = if i == default_ix { "  (default)" } else { "" };
        println!("    {:>iw$}) {name:<width$}  {about}{marker}", i + 1);
    }
    loop {
        let raw = prompt("  > ")?;
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(default_ix);
        }
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= options.len() => return Ok(n - 1),
            // Typing the name works too — it is right there on screen.
            _ => match options.iter().position(|(n, _)| *n == raw) {
                Some(ix) => return Ok(ix),
                None => println!("  pick 1-{}", options.len()),
            },
        }
    }
}

/// Any-of-N, by number. Features the template already turns on are left out.
fn ask_features(template: Template, preselected: &[String]) -> Result<Vec<String>, String> {
    let base = template.base_features();
    let menu: Vec<(&str, &str)> = OPTIONAL_FEATURES
        .iter()
        .filter(|(n, _)| !base.contains(n))
        .map(|(n, a)| (*n, *a))
        .collect();
    if menu.is_empty() {
        return Ok(Vec::new());
    }

    let width = menu.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let iw = menu.len().to_string().len();
    println!();
    println!("  Extra features  (numbers, e.g. `1 3 4` — enter for none)");
    for (i, (name, about)) in menu.iter().enumerate() {
        let on = if preselected.iter().any(|p| p == name) {
            "*"
        } else {
            " "
        };
        println!("  {on} {:>iw$}) {name:<width$}  {about}", i + 1);
    }
    if !preselected.is_empty() {
        println!("    (* already set by --features; enter keeps them)");
    }

    loop {
        let raw = prompt("  > ")?;
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(preselected.to_vec());
        }
        let mut picked = Vec::new();
        let mut bad = None;
        for tok in raw.split([',', ' ']).filter(|t| !t.is_empty()) {
            match tok.parse::<usize>() {
                Ok(n) if n >= 1 && n <= menu.len() => {
                    let name = menu[n - 1].0.to_owned();
                    if !picked.contains(&name) {
                        picked.push(name);
                    }
                }
                _ => {
                    bad = Some(tok.to_owned());
                    break;
                }
            }
        }
        match bad {
            Some(t) => println!("  `{t}` is not one of 1-{}", menu.len()),
            None => return Ok(picked),
        }
    }
}

fn confirm(question: &str) -> Result<bool, String> {
    loop {
        let raw = prompt(&format!("  {question} [Y/n] "))?;
        return Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => true,
            "n" | "no" => false,
            _ => continue,
        });
    }
}

/// Print without a newline, then read one line. EOF (ctrl-d) aborts rather
/// than looping forever on an empty read.
fn prompt(label: &str) -> Result<String, String> {
    print!("{label}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut buf = String::new();
    match io::stdin().read_line(&mut buf) {
        Ok(0) => Err("cancelled".to_owned()),
        Ok(_) => Ok(buf),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line the wizard prints must re-create the same project.
    ///
    /// This is what keeps the wizard a front-end over the flags rather than a
    /// second code path: if it could produce a selection the flags cannot
    /// express, the two would drift.
    #[test]
    fn the_echoed_command_round_trips() {
        let cases = [
            (Template::Tenant, Backend::Sqlite, vec!["sso", "cache-page"]),
            (Template::Api, Backend::Mysql, vec![]),
            (Template::Fullstack, Backend::Postgres, vec!["tenancy"]),
        ];
        for (template, backend, features) in cases {
            let features: Vec<String> = features.iter().map(|s| (*s).to_owned()).collect();
            let args = NewArgs {
                name: "probe".to_owned(),
                template,
                backend,
                features: features.clone(),
                rustango_path: None,
                interactive: false,
            };
            let cmd = equivalent_command(&args);
            // Re-parse it exactly as a user retyping the line would.
            let argv: Vec<String> = cmd
                .split_whitespace()
                .skip(3) // `cargo rustango new`
                .map(str::to_owned)
                .collect();
            let back = crate::parse_new_args(&argv, false)
                .unwrap_or_else(|e| panic!("`{cmd}` does not parse: {e}"));
            assert_eq!(back.name, "probe", "{cmd}");
            assert_eq!(back.template.name(), template.name(), "{cmd}");
            assert_eq!(back.backend, backend, "{cmd}");
            assert_eq!(back.features, features, "{cmd}");
        }
    }

    /// Every menu entry must map onto a value the flags accept, so a wizard
    /// answer is never something `--template` / `--backend` cannot say.
    #[test]
    fn every_menu_label_is_a_valid_flag_value() {
        for (label, template, _) in TEMPLATES {
            assert_eq!(Template::parse(label).expect(label).name(), template.name());
        }
        for (label, backend, _) in BACKENDS {
            assert_eq!(&Backend::parse(label).expect(label), backend);
        }
    }
}
