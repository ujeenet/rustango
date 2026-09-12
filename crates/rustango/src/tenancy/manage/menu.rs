//! `manage menu` — numbered choices over the tenancy verbs.
//!
//! There are 42 verbs. Reading `--help` tells you they exist; it does not
//! help you run one you have never run before. The menu lists them by group,
//! asks only for what the verb will not ask for itself, echoes the command it
//! is about to run, and comes back for the next action.
//!
//! ## Why the catalog is small
//!
//! Most verbs already prompt for their own required values on a terminal —
//! `create-tenant` asks for a slug, `create-operator` asks for a username and
//! reads the password without echoing it (`manage_interactive`). Re-asking
//! here would duplicate that and, for passwords, do it worse. So [`Action`]
//! carries only the options a verb will *not* prompt for: `--days`,
//! `--mode`, `--generate`. Everything else is left to the verb.
//!
//! Each pick re-enters [`super::dispatch`], so the menu cannot drift from
//! what the flags do — same code path, different front end.

use std::io::{BufRead, Write};
use std::path::Path;

use crate::tenancy::error::TenancyError;
use crate::tenancy::pools::TenantPools;

use super::wizard::{prompt_value, prompt_yes_no};
use super::InitTenancyFn;

/// An option the menu asks about, appended to the argv when answered.
enum Ask {
    /// `--name <value>`; skipped when the answer is empty.
    Value {
        flag: &'static str,
        label: &'static str,
        default: Option<&'static str>,
    },
    /// `--name`; appended only when the answer is yes.
    Toggle {
        flag: &'static str,
        question: &'static str,
    },
    /// One of two flags, always appended — for a verb that refuses to guess
    /// a direction, where "no answer" is not a valid argv.
    Either {
        question: &'static str,
        yes: &'static str,
        no: &'static str,
    },
}

struct Action {
    verb: &'static str,
    about: &'static str,
    asks: &'static [Ask],
}

struct Group {
    title: &'static str,
    actions: &'static [Action],
}

const GROUPS: &[Group] = &[
    Group {
        title: "TENANTS",
        actions: &[
            Action {
                verb: "list-tenants",
                about: "every tenant in the registry",
                asks: &[],
            },
            Action {
                verb: "create-tenant",
                about: "provision a new tenant",
                asks: &[
                    Ask::Value {
                        flag: "--mode",
                        label: "storage mode (schema|database)",
                        default: Some("schema"),
                    },
                    Ask::Value {
                        flag: "--display-name",
                        label: "display name (blank to skip)",
                        default: None,
                    },
                    Ask::Value {
                        flag: "--host-pattern",
                        label: "host pattern (blank to skip)",
                        default: None,
                    },
                ],
            },
            Action {
                verb: "drop-tenant",
                about: "soft-delete a tenant; data preserved",
                asks: &[],
            },
            Action {
                verb: "purge-tenant",
                about: "HARD-delete a tenant and its schema",
                asks: &[Ask::Toggle {
                    flag: "--purge-database",
                    question: "also drop the tenant's database?",
                }],
            },
            Action {
                verb: "test-tenant-connection",
                about: "check a database URL before using it",
                asks: &[],
            },
        ],
    },
    Group {
        title: "HOSTNAMES",
        actions: &[
            Action {
                verb: "list-hosts",
                about: "every hostname a tenant answers on",
                asks: &[],
            },
            Action {
                verb: "add-host",
                about: "bind an extra hostname to a tenant",
                asks: &[],
            },
            Action {
                verb: "remove-host",
                about: "unbind one",
                asks: &[],
            },
            Action {
                verb: "set-host-enabled",
                about: "park or serve a bound hostname",
                asks: &[Ask::Either {
                    question: "serve it? (no parks it)",
                    yes: "--on",
                    no: "--off",
                }],
            },
        ],
    },
    Group {
        title: "OPERATORS AND USERS",
        actions: &[
            Action {
                verb: "list-operators",
                about: "every operator and whether they are active",
                asks: &[],
            },
            Action {
                verb: "create-operator",
                about: "an apex-level account for the operator console",
                asks: &[Ask::Toggle {
                    flag: "--generate",
                    question: "generate a password instead of choosing one?",
                }],
            },
            Action {
                verb: "set-operator-active",
                about: "turn an operator's access off or back on",
                asks: &[Ask::Either {
                    question: "activate them? (no deactivates)",
                    yes: "--on",
                    no: "--off",
                }],
            },
            Action {
                verb: "reset-operator-password",
                about: "reset an operator's password",
                asks: &[Ask::Toggle {
                    flag: "--generate",
                    question: "generate the new password?",
                }],
            },
            Action {
                verb: "create-user",
                about: "a tenant-scoped user",
                asks: &[
                    Ask::Toggle {
                        flag: "--superuser",
                        question: "make them a superuser?",
                    },
                    Ask::Toggle {
                        flag: "--generate",
                        question: "generate a password instead of choosing one?",
                    },
                ],
            },
            Action {
                verb: "set-superuser",
                about: "promote or demote a tenant user",
                asks: &[Ask::Toggle {
                    flag: "--off",
                    question: "demote instead of promote?",
                }],
            },
            Action {
                verb: "reset-password",
                about: "reset a tenant user's password",
                asks: &[Ask::Toggle {
                    flag: "--generate",
                    question: "generate the new password?",
                }],
            },
        ],
    },
    Group {
        title: "ROLES AND PERMISSIONS",
        actions: &[
            Action {
                verb: "list-roles",
                about: "roles defined for a tenant",
                asks: &[],
            },
            Action {
                verb: "create-role",
                about: "define a new role",
                asks: &[],
            },
            Action {
                verb: "assign-role",
                about: "give a user a role",
                asks: &[],
            },
            Action {
                verb: "revoke-role",
                about: "take a role away",
                asks: &[],
            },
            Action {
                verb: "seed-permissions",
                about: "create the built-in permission rows",
                asks: &[],
            },
        ],
    },
    Group {
        title: "MIGRATIONS AND MAINTENANCE",
        actions: &[
            Action {
                verb: "migrate",
                about: "apply migrations: registry first, then every tenant",
                asks: &[],
            },
            Action {
                verb: "migrate-registry",
                about: "the registry database only",
                asks: &[],
            },
            Action {
                verb: "migrate-tenants",
                about: "every active tenant",
                asks: &[],
            },
            Action {
                verb: "audit-cleanup",
                about: "trim old audit-log entries",
                asks: &[Ask::Value {
                    flag: "--days",
                    label: "delete entries older than (days)",
                    default: Some("90"),
                }],
            },
            Action {
                verb: "prewarm-pools",
                about: "open a connection per tenant up front",
                asks: &[],
            },
            Action {
                verb: "wizard",
                about: "guided first-time setup",
                asks: &[],
            },
        ],
    },
];

/// Loop: show the menu, run one verb, come back. Ends on `q` or EOF.
pub(super) async fn menu_cmd<R: BufRead, W: Write + Send, DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    init_fn: InitTenancyFn,
    reader: &mut R,
    writer: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let flat = flatten();
    loop {
        write_menu(writer, &flat)?;
        let Some(action) = pick(reader, writer, &flat)? else {
            writeln!(writer)?;
            return Ok(());
        };

        let argv = build_argv(action, reader, writer)?;
        writeln!(writer)?;
        writeln!(writer, "  Same thing without the menu:")?;
        writeln!(writer, "    cargo run -- {}", argv.join(" "))?;
        writeln!(writer)?;

        // Same match the flags go through — boxed because this is the one
        // place dispatch is re-entered from inside itself.
        let outcome = Box::pin(super::dispatch(
            pools,
            registry_url,
            dir,
            argv,
            writer,
            init_fn,
        ))
        .await;

        // A failed verb ends that action, not the session: the operator is
        // sitting right there and the usual answer is to pick again.
        if let Err(e) = outcome {
            writeln!(writer, "  {} failed: {e}", action.verb)?;
        }

        if !prompt_yes_no(reader, writer, "\n  Another action?", true)? {
            return Ok(());
        }
    }
}

/// Every action in menu order, so a typed number maps to one entry.
fn flatten() -> Vec<&'static Action> {
    GROUPS.iter().flat_map(|g| g.actions.iter()).collect()
}

fn write_menu<W: Write>(w: &mut W, flat: &[&Action]) -> Result<(), TenancyError> {
    let width = flat.iter().map(|a| a.verb.len()).max().unwrap_or(0);
    let iw = flat.len().to_string().len();
    writeln!(w)?;
    writeln!(w, "  rustango manage — pick an action")?;
    let mut n = 0;
    for group in GROUPS {
        writeln!(w)?;
        writeln!(w, "  {}", group.title)?;
        for action in group.actions {
            n += 1;
            writeln!(w, "    {n:>iw$}) {:<width$}  {}", action.verb, action.about)?;
        }
    }
    writeln!(w)?;
    writeln!(w, "    {:>iw$}  quit", "q")?;
    writeln!(w, "    {:>iw$}  every other verb: cargo run -- --help", "?")?;
    Ok(())
}

/// Read a choice. `None` means quit (typed `q`, or EOF).
fn pick<'a, R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    flat: &[&'a Action],
) -> Result<Option<&'a Action>, TenancyError> {
    loop {
        write!(writer, "\n  > ")?;
        writer.flush()?;
        let mut buf = String::new();
        if reader.read_line(&mut buf)? == 0 {
            return Ok(None); // EOF — piped input ran out.
        }
        let raw = buf.trim();
        match raw {
            "q" | "quit" | "exit" => return Ok(None),
            "?" | "help" => {
                super::write_help(writer)?;
                continue;
            }
            "" => continue,
            _ => {}
        }
        if let Ok(n) = raw.parse::<usize>() {
            if n >= 1 && n <= flat.len() {
                return Ok(Some(flat[n - 1]));
            }
        }
        // Typing the verb works too — it is on screen right above.
        if let Some(a) = flat.iter().find(|a| a.verb == raw) {
            return Ok(Some(a));
        }
        writeln!(writer, "  pick 1-{}, or q to quit", flat.len())?;
    }
}

/// The argv for `action`, asking only what the verb will not ask itself.
fn build_argv<R: BufRead, W: Write>(
    action: &Action,
    reader: &mut R,
    writer: &mut W,
) -> Result<Vec<String>, TenancyError> {
    let mut argv = vec![action.verb.to_owned()];
    if !action.asks.is_empty() {
        writeln!(writer)?;
    }
    for ask in action.asks {
        match ask {
            Ask::Value {
                flag,
                label,
                default,
            } => {
                let v = prompt_value(reader, writer, &format!("  {label}"), *default)?;
                if !v.is_empty() {
                    argv.push((*flag).to_owned());
                    argv.push(v);
                }
            }
            Ask::Toggle { flag, question } => {
                if prompt_yes_no(reader, writer, &format!("  {question}"), false)? {
                    argv.push((*flag).to_owned());
                }
            }
            Ask::Either { question, yes, no } => {
                let pick = prompt_yes_no(reader, writer, &format!("  {question}"), true)?;
                argv.push((*if pick { yes } else { no }).to_owned());
            }
        }
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Every menu entry must be a verb the dispatcher handles.
    ///
    /// The catalog is a second list of verb names, so it can rot: rename a
    /// verb and the menu keeps offering the old spelling, which falls
    /// through to the single-tenant runner and fails with something
    /// unrelated. Reading the match arms is crude but it is the only way to
    /// compare against the real table without running a database.
    #[test]
    fn every_menu_verb_is_dispatchable() {
        let src = include_str!("mod.rs");
        for action in flatten() {
            let arm = format!("\"{}\"", action.verb);
            assert!(
                src.contains(&arm),
                "the menu offers `{}`, which no arm of `dispatch` matches — \
                 it would fall through to the single-tenant runner (#1345)",
                action.verb
            );
        }
    }

    /// A verb listed twice would make one of the two numbers unreachable.
    #[test]
    fn no_verb_appears_twice() {
        let mut seen: Vec<&str> = Vec::new();
        for action in flatten() {
            assert!(
                !seen.contains(&action.verb),
                "`{}` is in the menu twice",
                action.verb
            );
            seen.push(action.verb);
        }
    }

    /// Numbering is positional, so the flat order must match what is printed.
    #[test]
    fn numbering_matches_the_printed_order() {
        let flat = flatten();
        let mut out = Vec::new();
        write_menu(&mut out, &flat).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        for (i, action) in flat.iter().enumerate() {
            assert!(
                text.contains(&format!("{}) {}", i + 1, action.verb)),
                "entry {} should be `{}`:\n{text}",
                i + 1,
                action.verb
            );
        }
    }

    #[test]
    fn a_number_and_the_verb_itself_pick_the_same_action() {
        let flat = flatten();
        for (i, action) in flat.iter().enumerate() {
            let by_number = format!("{}\n", i + 1);
            let mut sink = Vec::new();
            let picked = pick(&mut Cursor::new(by_number), &mut sink, &flat)
                .expect("pick")
                .expect("some");
            assert_eq!(picked.verb, action.verb);

            let mut sink = Vec::new();
            let by_name = pick(
                &mut Cursor::new(format!("{}\n", action.verb)),
                &mut sink,
                &flat,
            )
            .expect("pick")
            .expect("some");
            assert_eq!(by_name.verb, action.verb);
        }
    }

    #[test]
    fn quit_and_eof_both_end_the_menu() {
        let flat = flatten();
        for input in ["q\n", "quit\n", ""] {
            let mut sink = Vec::new();
            let got = pick(&mut Cursor::new(input), &mut sink, &flat).expect("pick");
            assert!(got.is_none(), "`{input:?}` should end the menu");
        }
    }

    /// An out-of-range number re-asks rather than running something else.
    #[test]
    fn a_bad_choice_reprompts() {
        let flat = flatten();
        let input = format!("{}\n2\n", flat.len() + 1);
        let mut sink = Vec::new();
        let picked = pick(&mut Cursor::new(input), &mut sink, &flat)
            .expect("pick")
            .expect("some");
        assert_eq!(picked.verb, flat[1].verb);
        let text = String::from_utf8(sink).expect("utf8");
        assert!(text.contains("pick 1-"), "should have complained: {text}");
    }

    /// A pick must actually reach the verb — the whole point of re-entering
    /// `dispatch` rather than describing what the verb would have done.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn picking_an_action_runs_it() {
        use crate::sql::sqlx;

        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
        let pools = TenantPools::<sqlx::Sqlite>::new(pool);
        let migrations = tempfile::tempdir().expect("migrations");

        let mut buf: Vec<u8> = Vec::new();
        super::super::run_with_writer(
            &pools,
            &url,
            migrations.path(),
            vec!["migrate-registry".to_owned()],
            &mut buf,
        )
        .await
        .expect("migrate-registry");

        // Pick `list-tenants` by name, then decline another action.
        let mut out: Vec<u8> = Vec::new();
        menu_cmd(
            &pools,
            &url,
            migrations.path(),
            crate::tenancy::bootstrap::init_tenancy,
            &mut Cursor::new("list-tenants\nn\n"),
            &mut out,
        )
        .await
        .expect("menu");

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("cargo run -- list-tenants"),
            "the menu should echo the command it runs:\n{text}"
        );
        // `list-tenants` on an empty registry still prints its own header, so
        // reaching the verb is observable without seeding anything.
        assert!(
            !text.contains("list-tenants failed"),
            "the verb should have run cleanly:\n{text}"
        );
    }

    /// Declining "another action?" ends the loop rather than re-showing the
    /// menu forever — a menu that cannot be left is worse than no menu.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn the_loop_ends_when_asked() {
        use crate::sql::sqlx;

        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
        let pools = TenantPools::<sqlx::Sqlite>::new(pool);
        let migrations = tempfile::tempdir().expect("migrations");

        let mut out: Vec<u8> = Vec::new();
        // `q` at the first prompt: no verb runs at all.
        menu_cmd(
            &pools,
            &url,
            migrations.path(),
            crate::tenancy::bootstrap::init_tenancy,
            &mut Cursor::new("q\n"),
            &mut out,
        )
        .await
        .expect("menu");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("Same thing without the menu"),
            "quitting should not have run anything:\n{text}"
        );
    }

    /// Values become `--flag value`; an empty answer drops the flag entirely
    /// so the verb sees its own default rather than an empty string.
    #[test]
    fn answers_become_flags_and_blanks_are_dropped() {
        let action = Action {
            verb: "create-tenant",
            about: "",
            asks: &[
                Ask::Value {
                    flag: "--mode",
                    label: "mode",
                    default: Some("schema"),
                },
                Ask::Value {
                    flag: "--display-name",
                    label: "display name",
                    default: None,
                },
                Ask::Toggle {
                    flag: "--no-migrate",
                    question: "skip migrations?",
                },
            ],
        };
        // mode: enter (default) · display-name: blank · toggle: no
        let mut sink = Vec::new();
        let argv = build_argv(&action, &mut Cursor::new("\n\nn\n"), &mut sink).expect("argv");
        assert_eq!(argv, vec!["create-tenant", "--mode", "schema"]);

        // mode: database · display-name: Acme · toggle: yes
        let mut sink = Vec::new();
        let argv =
            build_argv(&action, &mut Cursor::new("database\nAcme\ny\n"), &mut sink).expect("argv");
        assert_eq!(
            argv,
            vec![
                "create-tenant",
                "--mode",
                "database",
                "--display-name",
                "Acme",
                "--no-migrate"
            ]
        );
    }
}
