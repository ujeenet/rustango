use super::*;

/// `start` reads its first quoted argument as a title: the URL must not be it.
#[test]
fn the_browser_opener_passes_the_url_as_the_target() {
    let url = "http://127.0.0.1:8000/__docs";
    assert_eq!(
        browser_opener("windows", url),
        Some(("cmd", vec!["/C", "start", "", url]))
    );
    assert_eq!(browser_opener("macos", url), Some(("open", vec![url])));
    assert_eq!(browser_opener("linux", url), Some(("xdg-open", vec![url])));
    assert_eq!(browser_opener("plan9", url), None);
}

/// The registry-scoped `Translation` must not flip a single-database
/// build into two-scope `makemigrations` (#2360).
#[cfg(not(feature = "tenancy"))]
#[test]
fn unmanaged_registry_model_is_not_tenancy() {
    use crate::core::Model as _;
    assert_eq!(
        crate::i18n::db::Translation::SCHEMA.scope,
        crate::core::ModelScope::Registry
    );
    assert!(!has_registry_models());
}

/// #1216 — the verbs that need no database must be dispatchable without
/// one. `Cli` builds a pool before dispatch otherwise, and `connect_lazy`
/// validates the URL scheme against the enabled backends, so a SQLite-only
/// build with a stale `postgres://` in the environment could not print its
/// own help.
#[test]
fn pool_free_verbs_need_no_database() {
    for verb in [
        "",
        "--help",
        "-h",
        "help",
        "version",
        "--version",
        "docs",
        "db:info",
    ] {
        let args = vec![verb.to_owned()];
        let mut out = Vec::new();
        let res = run_pool_free(&args, &mut out);
        assert!(
            res.is_some(),
            "`{verb}` must be dispatchable without a pool"
        );
        assert!(res.unwrap().is_ok(), "`{verb}` failed");
        assert!(!out.is_empty(), "`{verb}` produced no output");
    }
}

/// The generators write files from the model registry — no database either.
#[test]
fn generators_need_no_database() {
    for verb in [
        "make:viewset",
        "make:serializer",
        "make:form",
        "make:job",
        "make:scheduled",
        "make:worker",
        "make:notification",
        "make:middleware",
        "make:test",
        "startapp",
    ] {
        let args = vec![verb.to_owned()];
        assert!(
            run_pool_free(&args, &mut Vec::new()).is_some(),
            "`{verb}` must be dispatchable without a pool"
        );
    }
}

/// The docs show what `make:serializer` writes; the code lines must match (#1952).
#[test]
fn serializer_docs_snippet_matches_the_template() {
    let docs = include_str!("../../../../../docs/serializers.md");
    let start = docs
        .find("//! Auto-scaffolded by `manage make:serializer PostSerializer`.")
        .expect("snippet");
    let snippet = &docs[start..start + docs[start..].find("```").expect("fence")];
    let rendered = serializer_template("PostSerializer", &ModelRef::bare("Post"), "rustango");
    // Comments are abridged in the docs, so compare code lines only.
    for line in snippet.lines().filter(|l| !l.trim_start().starts_with('/')) {
        assert!(
            rendered.contains(line),
            "docs line not in template: `{line}`"
        );
    }
}

/// The complement matters as much: a verb that genuinely reads or writes
/// the database must NOT be short-circuited here, or it would silently do
/// nothing instead of connecting.
#[test]
fn database_verbs_are_not_short_circuited() {
    for verb in [
        "migrate",
        "makemigrations",
        "downgrade",
        "showmigrations",
        "about",
        "check",
        "dumpdata",
        "loaddata",
    ] {
        let args = vec![verb.to_owned()];
        assert!(
            run_pool_free(&args, &mut Vec::new()).is_none(),
            "`{verb}` touches the database — it must fall through to `run`"
        );
    }
}

#[test]
fn pascal_to_snake_cases() {
    assert_eq!(pascal_to_snake("Post"), "post");
    assert_eq!(pascal_to_snake("PostViewSet"), "post_view_set");
    assert_eq!(pascal_to_snake("API"), "a_p_i"); // simple impl — acceptable
    assert_eq!(pascal_to_snake("UserNotification"), "user_notification");
}

/// The regression from #1214: `docs/manage.md` documents
/// `make:test post_smoke`, and that exact invocation was rejected.
#[test]
fn make_test_accepts_the_documented_snake_case_name() {
    let args = vec!["post_smoke".to_owned()];
    let (name, _, _) = parse_name_and_model_as(&args, NameShape::Identifier)
        .expect("the documented example must be accepted");
    assert_eq!(name, "post_smoke");
    // Already snake — the filename conversion must leave it alone.
    assert_eq!(pascal_to_snake(&name), "post_smoke");
}

/// PascalCase keeps working, and lands on the same file as the snake form,
/// so neither spelling is a second way to name a different test.
#[test]
fn make_test_still_accepts_pascal_case() {
    let args = vec!["PostSmoke".to_owned()];
    let (name, _, _) = parse_name_and_model_as(&args, NameShape::Identifier).unwrap();
    assert_eq!(pascal_to_snake(&name), "post_smoke");
}

/// The type-name generators are unchanged — snake_case is still refused
/// there, because their output really is a `struct`.
#[test]
fn type_generators_still_reject_snake_case() {
    let args = vec!["post_view_set".to_owned()];
    assert!(parse_name_and_model_as(&args, NameShape::Type).is_err());
    assert!(parse_name_and_model(&args).is_err());
}

#[test]
fn identifier_rules() {
    assert!(is_valid_identifier("post_smoke"));
    assert!(is_valid_identifier("PostSmoke"));
    assert!(is_valid_identifier("_leading"));
    assert!(is_valid_identifier("a1"));
    assert!(!is_valid_identifier("1leading")); // digit first
    assert!(!is_valid_identifier("has-dash"));
    assert!(!is_valid_identifier(""));
}

#[test]
fn is_valid_type_name_accepts_pascal() {
    assert!(is_valid_type_name("Post"));
    assert!(is_valid_type_name("PostViewSet"));
    assert!(is_valid_type_name("Foo_Bar"));
}

#[test]
fn is_valid_type_name_rejects_invalid() {
    assert!(!is_valid_type_name(""));
    assert!(!is_valid_type_name("post")); // lowercase
    assert!(!is_valid_type_name("123Foo")); // starts with digit
    assert!(!is_valid_type_name("Post!")); // bad char
}

#[test]
fn parse_name_and_model_basic() {
    let (n, m, c) = parse_name_and_model(&["PostViewSet".into()]).unwrap();
    assert_eq!(n, "PostViewSet");
    assert_eq!(m, None);
    // Phase 2b of #145 — `--crate` defaults to "rustango" so today's
    // invocations stay bit-identical.
    assert_eq!(c, "rustango");
}

#[test]
fn parse_name_and_model_with_model_flag() {
    let args: Vec<String> = vec!["PostViewSet".into(), "--model".into(), "Post".into()];
    let (n, m, c) = parse_name_and_model(&args).unwrap();
    assert_eq!(n, "PostViewSet");
    assert_eq!(m, Some("Post".into()));
    assert_eq!(c, "rustango");
}

#[test]
fn parse_name_and_model_with_crate_flag() {
    // Phase 2b of #145 — passing `--crate rustango_orm` lets a
    // bare-ORM consumer (post-#144) emit source compiled against
    // the renamed dep. Whitespace trimmed; empty rejected at the
    // `parse_name_and_model_rejects_empty_crate` test below.
    let args: Vec<String> = vec!["PostSerializer".into(), "--crate".into(), "orm".into()];
    let (n, m, c) = parse_name_and_model(&args).unwrap();
    assert_eq!(n, "PostSerializer");
    assert_eq!(m, None);
    assert_eq!(c, "orm");
}

#[test]
fn parse_name_and_model_rejects_empty_crate() {
    let args: Vec<String> = vec!["Post".into(), "--crate".into(), "".into()];
    let err = parse_name_and_model(&args)
        .err()
        .expect("empty crate must fail");
    assert!(format!("{err}").contains("must not be empty"));
}

#[test]
fn parse_name_and_model_rejects_missing_crate_value() {
    let args: Vec<String> = vec!["Post".into(), "--crate".into()];
    let err = parse_name_and_model(&args)
        .err()
        .expect("missing crate value must fail");
    assert!(format!("{err}").contains("--crate requires a value"));
}

#[test]
fn parse_name_and_model_rejects_missing_name() {
    let r = parse_name_and_model(&[]);
    assert!(r.is_err());
}

#[test]
fn parse_name_and_model_rejects_lowercase_name() {
    let r = parse_name_and_model(&["postviewset".into()]);
    assert!(r.is_err());
}

/// `pub mod type;` / `pub mod std;` do not build (#1913).
#[test]
fn names_that_make_a_reserved_module_are_refused() {
    for n in ["Type", "Match", "Std", "Core", "Crate", "Self", "Super"] {
        assert!(parse_name_and_model(&[n.into()]).is_err(), "{n}");
    }
    for n in ["type", "self", "std"] {
        assert!(
            parse_name_and_model_as(&[n.into()], NameShape::Identifier).is_err(),
            "{n}"
        );
    }
    assert!(parse_name_and_model(&["Typed".into()]).is_ok());
}

/// The model must be in scope or the derive can't name it (#1913).
#[test]
fn pool_templates_import_their_model() {
    let post = ModelRef::bare("Post");
    let vs = viewset_template_pool(
        "PostViewSet",
        &post,
        "post_view_set",
        "/posts",
        "rustango",
        "app",
        true,
    );
    let ser = serializer_template("PostSerializer", &post, "rustango");
    for body in [&vs, &ser] {
        assert!(body.contains("use crate::models::Post;"), "{body}");
    }
    let tenant =
        viewset_template_tenant("PostViewSet", &post, "post_view_set", "/posts", "rustango");
    assert!(
        tenant.contains(".merge(crate::post_view_set::router())"),
        "{tenant}"
    );
}

/// `startapp blog` puts `Post` in `src/blog/models.rs`, so the import
/// must follow it there.
#[test]
fn model_ref_finds_an_app_model() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("blog")).unwrap();
    std::fs::write(src.join("models.rs"), "pub struct Author;\n").unwrap();
    std::fs::write(
        src.join("blog/models.rs"),
        "#[derive(Model)]\npub struct Post {\n    pub id: i64,\n}\n",
    )
    .unwrap();
    let path = |m: &str| ModelRef::resolve(m, &src).path;
    assert_eq!(path("Post"), "crate::blog::models::Post");
    assert_eq!(path("Author"), "crate::models::Author");
    assert_eq!(path("Missing"), "crate::models::Missing");
    assert_eq!(path("shop::models::Item"), "crate::shop::models::Item");
    let explicit = ModelRef::resolve("crate::shop::models::Item", &src);
    assert_eq!(
        (explicit.path.as_str(), explicit.name.as_str()),
        ("crate::shop::models::Item", "Item")
    );
    let post = ModelRef::resolve("Post", &src);
    let vs = viewset_template_pool(
        "PostViewSet",
        &post,
        "post_view_set",
        "/posts",
        "rustango",
        "my_app",
        true,
    );
    assert!(vs.contains("use crate::blog::models::Post;"), "{vs}");
    assert!(vs.contains("model        = Post,"), "{vs}");
    // `urls::api()` takes no pool; main.rs's `nest_with` hands one over.
    assert!(
            vs.contains(r#".nest_with("/api", |pool| my_app::post_view_set::PostViewSet::router("/posts", pool))"#),
            "{vs}"
        );
}

/// The mount hint serves the model's table, not the viewset's module:
/// `/api/posts`, as the guide curls, not `/api/post_view_set`.
#[test]
fn viewset_route_is_the_model_table() {
    let post = ModelRef {
        path: "crate::blog::models::Post".into(),
        name: "Post".into(),
    };
    let entries = [
        ("rustango::core", "Post", "rustango_posts"),
        ("my_app::shop::models", "Post", "shop_posts"),
        ("my_app::blog::models", "Post", "posts"),
        ("my_app::blog::models", "Comment", "comments"),
    ];
    assert_eq!(registered_table(&post, "my_app", entries), Some("posts"));
    // A guessed path still finds the one project model of that name.
    let bare = ModelRef::bare("Post");
    assert_eq!(
        registered_table(&bare, "my_app", [entries[0], entries[2]]),
        Some("posts")
    );
    // Ambiguous or framework-only: no table, the caller guesses.
    assert_eq!(registered_table(&bare, "my_app", entries), None);
    assert_eq!(
        registered_table(&post, "my_app", entries[..1].iter().copied()),
        None
    );
    let tenant =
        viewset_template_tenant("PostViewSet", &post, "post_view_set", "/posts", "rustango");
    assert!(
        tenant.contains(r#".tenant_router("/api/posts")"#),
        "{tenant}"
    );
}

/// Without `tenancy` codenames refuse to mount, so the pool template
/// does not suggest them there.
#[test]
fn viewset_write_guard_names_what_the_build_has() {
    let render = |codenames| {
        let post = ModelRef::bare("Post");
        viewset_template_pool(
            "PostViewSet",
            &post,
            "post_view_set",
            "/posts",
            "rustango",
            "app",
            codenames,
        )
    };
    let single = render(false);
    assert!(!single.contains("permissions("), "{single}");
    assert!(single.contains("`allow_anonymous`"), "{single}");
    assert!(single.contains("\n    // `read_only` for"), "{single}");
    let tenancy = render(true);
    assert!(tenancy.contains("add `permissions(create"), "{tenancy}");
}

/// `make:api_routes` routes each app model at its table, as `make:viewset`
/// does: `/posts`, not the module name `/post_view_set`.
#[test]
fn api_routes_mounts_follow_the_model_table() {
    let entries = [
        ("my_app::blog::models", "Post", "posts"),
        ("my_app::blog::models", "Comment", "blog_comments"),
        ("my_app::shop::models", "Order", "orders"),
        ("rustango::core", "Post", "rustango_posts"),
    ];
    let mounts = app_viewset_mounts("blog", "my_app", &entries);
    assert_eq!(
        mounts,
        [
            ("Post".to_owned(), "/posts".to_owned()),
            ("Comment".to_owned(), "/blog_comments".to_owned()),
        ]
    );
    let body = api_routes_template_pool("blog", "rustango", &mounts);
    assert!(
        body.contains(
            r#"// .merge(crate::post_view_set::PostViewSet::router("/posts", _pool.clone()))"#
        ),
        "{body}"
    );
    assert!(
        body.contains(r#"crate::comment_view_set::CommentViewSet::router("/blog_comments", "#),
        "{body}"
    );
    assert!(!body.contains(r#""/post_view_set""#), "{body}");
}

/// The notification template points at the shipped multi-channel trait.
#[test]
fn notification_template_names_the_notification_trait() {
    let (_, files) = run_make(
        "make:notification",
        &["WelcomeNote"],
        &["src"],
        &["src/welcome_note.rs"],
    );
    let body = &files[0];
    assert!(
        body.contains("rustango::notifications::Notification"),
        "{body}"
    );
    assert!(!body.contains("ships"), "{body}");
}

/// A `make:*` verb whose output needs a feature the build lacks says so.
#[test]
fn make_verbs_name_the_feature_their_output_needs() {
    let none = |_: &str| false;
    for (verb, feature) in [
        ("make:serializer", "serializer"),
        ("make:form", "forms"),
        ("make:job", "jobs"),
        ("make:worker", "jobs-postgres"),
        ("make:scheduled", "scheduler"),
        ("make:notification", "notifications"),
        ("make:test", "admin"),
    ] {
        assert_eq!(missing_make_feature(verb, none), Some(feature), "{verb}");
        assert_eq!(missing_make_feature(verb, |_| true), None, "{verb}");
    }
    for verb in ["make:middleware", "make:api_routes", "startapp"] {
        assert_eq!(missing_make_feature(verb, none), None, "{verb}");
    }
    let err = make_cmd("make:test", &[], &mut Vec::new(), none).unwrap_err();
    assert!(err.to_string().contains("`admin`"), "{err}");
}

/// In a project that renamed the dependency to `rgo`, `make:*` writes `rgo::`.
#[test]
fn make_verbs_follow_a_renamed_dependency() {
    let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"short\"\n\n[dependencies]\n\
             rgo = { package = \"rustango\", version = \"0.61\" }\n",
    )
    .unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let run = make_cmd(
        "make:job",
        &["SendMail".to_owned()],
        &mut Vec::new(),
        |_| true,
    );
    let body = std::fs::read_to_string("src/send_mail.rs");
    let _ = std::env::set_current_dir(&prev);
    run.unwrap();
    let body = body.unwrap();
    assert!(
        body.contains("rgo::") && !body.contains("rustango::"),
        "{body}"
    );
}

/// The pool viewset needs `admin`; the tenant one only `tenancy`.
#[test]
fn make_viewset_needs_the_feature_of_its_template() {
    // `--crate` with no value fails after the feature check, so no file is written.
    let run = |flag: &str, missing: &'static str| {
        let args = [flag.to_owned(), "--crate".to_owned()];
        make_cmd("make:viewset", &args, &mut Vec::new(), move |f| {
            f != missing
        })
        .unwrap_err()
        .to_string()
    };
    assert!(run("--tenant", "tenancy").contains("`tenancy`"));
    assert!(run("--no-tenant", "admin").contains("`admin`"));
    let err = run("--tenant", "admin");
    assert!(err.contains("--crate requires a value"), "{err}");
}

#[test]
fn compiled_feature_matches_the_build() {
    for (feature, on) in [
        ("admin", cfg!(feature = "admin")),
        ("tenancy", cfg!(feature = "tenancy")),
        ("serializer", cfg!(feature = "serializer")),
        ("forms", cfg!(feature = "forms")),
        ("jobs", cfg!(feature = "jobs")),
        ("jobs-postgres", cfg!(feature = "jobs-postgres")),
        ("scheduler", cfg!(feature = "scheduler")),
        ("notifications", cfg!(feature = "notifications")),
        ("no_such_feature", false),
    ] {
        assert_eq!(compiled_feature(feature), on, "{feature}");
    }
}

/// The worker resolves the database URL, so it must load `.env` like main.rs.
#[test]
fn worker_loads_dotenv_before_reading_the_url() {
    let body = worker_template("Mailer", "mailer", "rustango", "my_app");
    let dotenv = body.find("dotenvy::dotenv()").expect("loads .env");
    assert!(
        dotenv < body.find("database_url_from_env").unwrap(),
        "{body}"
    );
}

/// `#[rustango::main]` already installs logging; a second setup only warns.
#[test]
fn worker_leaves_logging_to_the_main_macro() {
    let body = worker_template("Mailer", "mailer", "rustango", "my_app");
    assert!(!body.contains("logging::setup"), "{body}");
}

/// Run `make:*` in a fresh project named `shop`; returns (output, files).
fn run_make(cmd: &str, args: &[&str], dirs: &[&str], read: &[&str]) -> (String, Vec<String>) {
    let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"shop\"\n\n[dependencies]\nrustango = \"0.61\"\n",
    )
    .unwrap();
    for d in dirs {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    std::fs::write(dir.path().join("src/lib.rs"), "pub mod urls;\n").unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let mut out = Vec::new();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let run = make_cmd(cmd, &args, &mut out, |_| true);
    let files: Vec<String> = read
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap_or_default())
        .collect();
    let _ = std::env::set_current_dir(&prev);
    run.unwrap();
    (String::from_utf8(out).unwrap(), files)
}

/// The pool `api()` takes the server's pool, so only `main.rs` can mount it.
#[test]
fn api_routes_hints_name_a_mount_that_compiles() {
    let (out, files) = run_make(
        "make:api_routes",
        &["blog"],
        &["src/blog"],
        &["src/blog/api_routes.rs"],
    );
    assert!(
        out.contains(r#".nest_with("/api", |pool| shop::blog::api_routes::api(pool))"#),
        "{out}"
    );
    assert!(!out.contains(".merge("), "{out}");
    let body = &files[0];
    assert!(!body.contains("viewsets::"), "{body}");
    assert!(
        body.contains(r#"crate::<snake>::<Name>::router("/<table>", "#),
        "{body}"
    );

    let (out, files) = run_make(
        "make:api_routes",
        &["blog", "--tenant"],
        &["src/blog"],
        &["src/blog/api_routes.rs"],
    );
    assert!(
        out.contains(".merge(crate::blog::api_routes::api())"),
        "{out}"
    );
    assert!(!files[0].contains("viewsets::"), "{}", files[0]);
    assert!(
        files[0].contains("crate::<snake>::router()"),
        "{}",
        files[0]
    );
}

/// The derive needs no trait import; an unused one warns on first build.
#[test]
fn make_form_imports_only_what_it_uses() {
    let (_, files) = run_make("make:form", &["PostForm"], &["src"], &["src/post_form.rs"]);
    assert!(
        !files[0].contains("use rustango::forms::Form;"),
        "{}",
        files[0]
    );
}

/// `main.rs` has no `pool`; a worker binary has one and runs no CLI verb.
#[test]
fn make_scheduled_wires_into_a_worker() {
    let (_, files) = run_make("make:scheduled", &["Tick"], &["src"], &["src/tick.rs"]);
    let body = &files[0];
    assert!(body.contains("make:worker"), "{body}");
    assert!(!body.contains("in main.rs"), "{body}");
    assert!(
        body.contains("Arc::new(shop::tick::Tick { pool: pool.clone() })"),
        "{body}"
    );
}

/// The tenant viewset checks permissions, so it needs a user: mount auth.
#[test]
fn tenant_viewset_mounts_an_auth_layer() {
    let body = viewset_template_tenant(
        "PostViewSet",
        &ModelRef::bare("Post"),
        "post_view_set",
        "/posts",
        "rustango",
    );
    assert!(
        body.contains(".require_auth(vec![Arc::new(ModelBackend)])"),
        "{body}"
    );
    assert!(body.contains("use rustango::tenancy::{ModelBackend, RouterAuthExt as _};"));
}

/// `startapp` already registered the module; the next step is migrations.
#[test]
fn startapp_report_does_not_ask_for_the_mod_it_added() {
    let report = super::super::scaffold::StartAppReport {
        written: vec!["src/shop/mod.rs".into()],
        patched: vec!["src/lib.rs".into(), "src/urls.rs".into()],
        ..Default::default()
    };
    let mut out = Vec::new();
    write_startapp_report(&mut out, "shop", &report).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("add `mod"), "{out}");
    assert!(out.contains("makemigrations"), "{out}");
}

// -------- dumpdata --------

#[test]
fn parse_dumpdata_args_defaults() {
    let p = parse_dumpdata_args(&[]).unwrap();
    assert!(p.model_filters.is_empty());
    assert_eq!(p.indent, 2);
    assert!(!p.help);
}

#[test]
fn parse_dumpdata_args_help_flag() {
    let p = parse_dumpdata_args(&["--help".into()]).unwrap();
    assert!(p.help);
}

#[test]
fn parse_dumpdata_args_collects_multiple_models() {
    let args: Vec<String> = vec![
        "--model".into(),
        "blog.Article".into(),
        "--model".into(),
        "Author".into(),
    ];
    let p = parse_dumpdata_args(&args).unwrap();
    assert_eq!(p.model_filters, vec!["blog.Article", "Author"]);
}

#[test]
fn parse_dumpdata_args_indent_accepts_zero() {
    let p = parse_dumpdata_args(&["--indent".into(), "0".into()]).unwrap();
    assert_eq!(p.indent, 0);
}

#[test]
fn parse_dumpdata_args_rejects_unknown_flag() {
    let r = parse_dumpdata_args(&["--unknown".into()]);
    assert!(r.is_err());
    assert!(format!("{}", r.unwrap_err()).contains("unknown flag"));
}

#[test]
fn parse_dumpdata_args_rejects_missing_model_value() {
    let r = parse_dumpdata_args(&["--model".into()]);
    assert!(r.is_err());
}

#[test]
fn parse_dumpdata_args_rejects_non_integer_indent() {
    let r = parse_dumpdata_args(&["--indent".into(), "abc".into()]);
    assert!(r.is_err());
}

#[test]
fn parse_dumpdata_args_rejects_positional() {
    let r = parse_dumpdata_args(&["unexpected".into()]);
    assert!(r.is_err());
    assert!(format!("{}", r.unwrap_err()).contains("positional"));
}

// -------- loaddata --------

#[test]
fn parse_loaddata_args_requires_path() {
    let r = parse_loaddata_args(&[]);
    assert!(r.is_err());
    assert!(format!("{}", r.unwrap_err()).contains("missing"));
}

#[test]
fn parse_loaddata_args_path_then_flag() {
    let p = parse_loaddata_args(&["fixtures.json".into(), "--fail-fast".into()]).unwrap();
    assert_eq!(p.file, "fixtures.json");
    assert_eq!(p.on_error, OnRowError::Abort);
}

#[test]
fn parse_loaddata_args_flag_then_path() {
    let p = parse_loaddata_args(&["--fail-fast".into(), "fixtures.json".into()]).unwrap();
    assert_eq!(p.file, "fixtures.json");
    assert_eq!(p.on_error, OnRowError::Abort);
}

#[test]
fn parse_loaddata_args_help_flag_short_circuits() {
    let p = parse_loaddata_args(&["--help".into()]).unwrap();
    assert!(p.help);
}

#[test]
fn parse_loaddata_args_rejects_two_positionals() {
    let r = parse_loaddata_args(&["a.json".into(), "b.json".into()]);
    assert!(r.is_err());
}

#[test]
fn parse_loaddata_args_rejects_unknown_flag() {
    let r = parse_loaddata_args(&["--unknown".into(), "a.json".into()]);
    assert!(r.is_err());
}

// -------- json_to_sql_value --------

fn field(name: &'static str, ty: crate::core::FieldType) -> crate::core::FieldSchema {
    crate::core::FieldSchema {
        name,
        column: name,
        ty,
        nullable: true,
        primary_key: false,
        relation: None,
        max_length: None,
        min: None,
        max: None,
        default: None,
        auto: false,
        auto_now: false,
        unique: false,
        generated_as: None,
        help_text: None,
        choices: None,
        db_comment: None,
        verbose_name: None,
        editable: true,
        blank: false,
        case_insensitive: false,
        fk_on_delete: None,
        validators: &[],
        cast: None,
        secret: None,
    }
}

/// The streamed fixture is byte for byte what `serde_json` writes whole.
#[test]
fn fixture_writer_matches_serde_json() {
    let entries = vec![
        serde_json::json!({"model": "a.B", "pk": 1, "fields": {"s": "x\ny", "l": [1, 2]}}),
        serde_json::json!({"model": "a.B", "pk": 2, "fields": {}}),
    ];
    for n in 0..=entries.len() {
        let some = &entries[..n];
        for indent in [0, 2] {
            let mut buf = Vec::new();
            let mut w = FixtureWriter::new(&mut buf, indent);
            for e in some {
                w.entry(e).unwrap();
            }
            w.finish().unwrap();
            let whole = if indent == 0 {
                serde_json::to_string(some)
            } else {
                serde_json::to_string_pretty(some)
            }
            .unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), format!("{whole}\n"));
        }
    }
}

/// Only `loaddata` reads the dumpdata tag; a seed fixture's literal
/// `{"$json": 1}` and a `null` stay as written.
#[test]
fn only_loaddata_reads_the_json_tag() {
    use crate::core::{FieldType, SqlValue};
    use serde_json::{json, Value};
    let mut f = field("j", FieldType::Json);
    f.nullable = false;
    let tagged = json!({"$json": 1});
    assert!(
        matches!(json_to_sql_value(&tagged, &f), Ok(SqlValue::Json(ref v)) if *v == tagged),
        "a seed fixture keeps the literal object"
    );
    assert!(matches!(
        json_to_sql_value(&Value::Null, &f),
        Ok(SqlValue::Null)
    ));
    assert!(matches!(loaddata_value(&tagged, &f), Ok(SqlValue::Json(ref v)) if *v == json!(1)));
    assert!(matches!(
        loaddata_value(&Value::Null, &f),
        Ok(SqlValue::Json(Value::Null))
    ));
    f.nullable = true;
    assert!(matches!(
        loaddata_value(&Value::Null, &f),
        Ok(SqlValue::Null)
    ));
}

#[test]
fn json_to_sql_value_null_passes_through() {
    let f = field("x", crate::core::FieldType::I64);
    let v = json_to_sql_value(&serde_json::Value::Null, &f).unwrap();
    assert!(matches!(v, crate::core::SqlValue::Null));
}

#[test]
fn json_to_sql_value_i32_range_check() {
    let f = field("x", crate::core::FieldType::I32);
    let v = json_to_sql_value(&serde_json::json!(42), &f).unwrap();
    assert!(matches!(v, crate::core::SqlValue::I32(42)));
    // Out of range → Err.
    let r = json_to_sql_value(&serde_json::json!(99999999999_i64), &f);
    assert!(r.is_err());
}

/// Both arms the I64 docs promise, and the fractional time dumpdata writes (#1911).
#[test]
fn json_to_sql_value_reads_what_dumpdata_writes() {
    let f = field("x", crate::core::FieldType::I64);
    let v = json_to_sql_value(&serde_json::json!("42"), &f).unwrap();
    assert!(matches!(v, crate::core::SqlValue::I64(42)), "{v:?}");
    assert!(json_to_sql_value(&serde_json::json!("4x"), &f).is_err());

    let t = field("t", crate::core::FieldType::Time);
    let v = json_to_sql_value(&serde_json::json!("12:34:56.789"), &t).unwrap();
    let want = chrono::NaiveTime::from_hms_milli_opt(12, 34, 56, 789).unwrap();
    assert!(
        matches!(v, crate::core::SqlValue::Time(x) if x == want),
        "{v:?}"
    );
}

#[test]
fn json_to_sql_value_string_passes_through() {
    let f = field("x", crate::core::FieldType::String);
    let v = json_to_sql_value(&serde_json::json!("hello"), &f).unwrap();
    match v {
        crate::core::SqlValue::String(s) => assert_eq!(s, "hello"),
        other => panic!("expected SqlValue::String, got {other:?}"),
    }
}

#[test]
fn json_to_sql_value_date_parses_iso() {
    let f = field("x", crate::core::FieldType::Date);
    let v = json_to_sql_value(&serde_json::json!("2025-01-02"), &f).unwrap();
    assert!(matches!(v, crate::core::SqlValue::Date(_)));
    // Malformed → Err.
    let r = json_to_sql_value(&serde_json::json!("not a date"), &f);
    assert!(r.is_err());
}

#[test]
fn json_to_sql_value_datetime_parses_rfc3339() {
    let f = field("x", crate::core::FieldType::DateTime);
    let v = json_to_sql_value(&serde_json::json!("2025-01-02T12:00:00+00:00"), &f).unwrap();
    assert!(matches!(v, crate::core::SqlValue::DateTime(_)));
}

#[test]
fn json_to_sql_value_decimal_parses_string_and_number() {
    let f = field("x", crate::core::FieldType::Decimal);
    // From string.
    let v = json_to_sql_value(&serde_json::json!("123.45"), &f).unwrap();
    assert!(matches!(v, crate::core::SqlValue::Decimal(_)));
    // From number (dumpdata emits a string, but be forgiving).
    let v = json_to_sql_value(&serde_json::json!(42), &f).unwrap();
    assert!(matches!(v, crate::core::SqlValue::Decimal(_)));
}

#[test]
fn json_to_sql_value_binary_hex_round_trip() {
    let f = field("x", crate::core::FieldType::Binary);
    let v = json_to_sql_value(&serde_json::json!("deadbeef"), &f).unwrap();
    match v {
        crate::core::SqlValue::Binary(b) => assert_eq!(b, vec![0xde, 0xad, 0xbe, 0xef]),
        other => panic!("expected Binary, got {other:?}"),
    }
    // Malformed (odd length).
    let r = json_to_sql_value(&serde_json::json!("abc"), &f);
    assert!(r.is_err());
}

#[test]
fn json_to_sql_value_json_field_stays_value() {
    let f = field("x", crate::core::FieldType::Json);
    let v = json_to_sql_value(&serde_json::json!({"k": "v"}), &f).unwrap();
    match v {
        crate::core::SqlValue::Json(j) => assert_eq!(j, serde_json::json!({"k": "v"})),
        other => panic!("expected Json, got {other:?}"),
    }
}

// -------- showurls --------

#[test]
fn showurls_help_emits_usage_line() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showurls_cmd(&["--help".into()], &mut buf);
    assert!(r.is_ok());
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("showurls"), "help text mentions verb: {s}");
    assert!(s.contains("plain"));
    assert!(s.contains("json"));
}

#[test]
fn showurls_rejects_unknown_format() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showurls_cmd(&["--format".into(), "xml".into()], &mut buf);
    assert!(r.is_err());
    let e = format!("{}", r.unwrap_err());
    assert!(e.contains("unknown value"), "error: {e}");
}

#[test]
fn showurls_rejects_unknown_flag() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showurls_cmd(&["--badflag".into()], &mut buf);
    assert!(r.is_err());
}

#[test]
fn showurls_rejects_positional() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showurls_cmd(&["extra".into()], &mut buf);
    assert!(r.is_err());
    let e = format!("{}", r.unwrap_err());
    assert!(e.contains("positional"));
}

#[test]
fn showurls_plain_format_runs_without_error() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showurls_cmd(&[], &mut buf);
    // Whether named URLs exist in this test binary depends on
    // other crates / test code that may have registered them.
    // The verb itself must always succeed.
    assert!(r.is_ok(), "showurls plain shouldn't error: {r:?}");
}

#[test]
fn showurls_json_format_emits_json_array() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showurls_cmd(&["--format".into(), "json".into()], &mut buf);
    assert!(r.is_ok());
    let s = String::from_utf8(buf).unwrap();
    // Must be valid JSON.
    let parsed: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
    assert!(parsed.is_array(), "expected JSON array, got: {s}");
}

// -------- showmodels --------

#[test]
fn showmodels_help_short_circuits() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(&["--help".into()], &mut buf);
    assert!(r.is_ok());
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("showmodels"));
    assert!(s.contains("--app"));
}

#[test]
fn showmodels_rejects_unknown_format() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(&["--format".into(), "xml".into()], &mut buf);
    assert!(r.is_err());
    let e = format!("{}", r.unwrap_err());
    assert!(e.contains("unknown value"));
}

#[test]
fn showmodels_rejects_unknown_flag() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(&["--badflag".into()], &mut buf);
    assert!(r.is_err());
}

#[test]
fn showmodels_rejects_positional() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(&["unexpected".into()], &mut buf);
    assert!(r.is_err());
}

#[test]
fn showmodels_app_filter_requires_value() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(&["--app".into()], &mut buf);
    assert!(r.is_err());
}

#[test]
fn showmodels_plain_runs_clean() {
    // Inventory contents depend on what's linked in the test
    // binary; we just verify the verb completes without error.
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(&[], &mut buf);
    assert!(r.is_ok(), "showmodels should not error: {r:?}");
}

#[test]
fn showmodels_json_emits_parseable_array() {
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(&["--format".into(), "json".into()], &mut buf);
    assert!(r.is_ok());
    let s = String::from_utf8(buf).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
    assert!(parsed.is_array());
}

// -------- flush --------

#[test]
fn parse_flush_args_defaults() {
    let p = parse_flush_args(&[]).unwrap();
    assert!(!p.yes);
    assert!(p.apps.is_empty());
    assert!(p.models.is_empty());
    assert!(!p.help);
}

#[test]
fn parse_flush_args_yes_flag() {
    let p = parse_flush_args(&["--yes".into()]).unwrap();
    assert!(p.yes);
}

#[test]
fn parse_flush_args_collects_multiple_apps_and_models() {
    let args: Vec<String> = vec![
        "--app".into(),
        "blog".into(),
        "--model".into(),
        "Article".into(),
        "--app".into(),
        "shop".into(),
        "--model".into(),
        "shop.Order".into(),
    ];
    let p = parse_flush_args(&args).unwrap();
    assert_eq!(p.apps, vec!["blog", "shop"]);
    assert_eq!(p.models, vec!["Article", "shop.Order"]);
}

#[test]
fn parse_flush_args_help_short_circuits() {
    let p = parse_flush_args(&["--help".into()]).unwrap();
    assert!(p.help);
}

/// A cycle is broken at a nullable link; with none, its tables are named.
#[test]
fn flush_plan_breaks_cycles_at_nullable_links() {
    use crate::core::{FieldSchema, FieldType, ModelSchema, Relation};
    fn table(name: &'static str, to: &'static str, nullable: bool) -> &'static ModelSchema {
        let mut f = FieldSchema::new("link", "link", FieldType::I64);
        f.relation = Some(Relation::fk(to, "id"));
        f.nullable = nullable;
        let mut s = ModelSchema::new(name, name);
        s.fields = Box::leak(Box::new([f]));
        Box::leak(Box::new(s))
    }
    let stuck = flush_plan(&[table("a", "b", false), table("b", "a", false)]);
    assert_eq!(stuck.stuck, ["a", "b"]);
    let plan = flush_plan(&[table("a", "b", true), table("b", "a", false)]);
    assert!(plan.stuck.is_empty());
    let unlinked: Vec<_> = plan
        .unlink
        .iter()
        .map(|(s, c)| (s.table, c.clone()))
        .collect();
    assert_eq!(unlinked, [("a", vec!["link"])]);
    // b's NOT NULL link to a still binds: b goes first.
    let order: Vec<_> = plan.order.iter().map(|s| s.table).collect();
    assert_eq!(order, ["b", "a"]);
}

#[test]
fn parse_flush_args_rejects_unknown_flag() {
    let r = parse_flush_args(&["--badflag".into()]);
    assert!(r.is_err());
}

#[test]
fn parse_flush_args_rejects_positional() {
    let r = parse_flush_args(&["unexpected".into()]);
    assert!(r.is_err());
}

#[test]
fn parse_flush_args_app_requires_value() {
    let r = parse_flush_args(&["--app".into()]);
    assert!(r.is_err());
}

#[test]
fn parse_flush_args_model_requires_value() {
    let r = parse_flush_args(&["--model".into()]);
    assert!(r.is_err());
}

// -------- sendtestemail --------
//
// These tests + the args struct live under `feature = "config"`
// because `sendtestemail_cmd` reads `Settings.email.*` to pick a
// backend; the `not(feature = "config")` stub short-circuits with
// a friendly error and never reaches the parser.

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_defaults_empty() {
    let p = parse_sendtestemail_args(&[]).unwrap();
    assert!(p.to.is_none());
    assert!(p.from.is_none());
    assert!(p.subject.is_none());
    assert!(!p.help);
}

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_collects_to_from_subject() {
    let args: Vec<String> = vec![
        "--to".into(),
        "ops@example.com".into(),
        "--from".into(),
        "bot@example.com".into(),
        "--subject".into(),
        "ping".into(),
    ];
    let p = parse_sendtestemail_args(&args).unwrap();
    assert_eq!(p.to.as_deref(), Some("ops@example.com"));
    assert_eq!(p.from.as_deref(), Some("bot@example.com"));
    assert_eq!(p.subject.as_deref(), Some("ping"));
}

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_help_short_circuits() {
    let p = parse_sendtestemail_args(&["--help".into()]).unwrap();
    assert!(p.help);
}

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_rejects_unknown_flag() {
    let r = parse_sendtestemail_args(&["--bogus".into()]);
    assert!(r.is_err());
}

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_rejects_positional() {
    let r = parse_sendtestemail_args(&["unexpected".into()]);
    assert!(r.is_err());
}

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_to_requires_value() {
    let r = parse_sendtestemail_args(&["--to".into()]);
    assert!(r.is_err());
}

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_from_requires_value() {
    let r = parse_sendtestemail_args(&["--from".into()]);
    assert!(r.is_err());
}

#[cfg(all(feature = "config", feature = "email"))]
#[test]
fn parse_sendtestemail_args_subject_requires_value() {
    let r = parse_sendtestemail_args(&["--subject".into()]);
    assert!(r.is_err());
}

#[cfg(all(feature = "config", feature = "email"))]
#[tokio::test]
async fn sendtestemail_help_short_circuits_without_settings_lookup() {
    let mut buf: Vec<u8> = Vec::new();
    let r = sendtestemail_cmd(&["--help".into()], &mut buf).await;
    assert!(r.is_ok());
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("sendtestemail"));
    assert!(s.contains("--to"));
}

#[cfg(feature = "config")]
#[tokio::test]
async fn sendtestemail_errors_when_to_missing() {
    let mut buf: Vec<u8> = Vec::new();
    let r = sendtestemail_cmd(&[], &mut buf).await;
    assert!(r.is_err(), "expected Validation error for missing --to");
}

#[test]
fn showmodels_app_filter_excludes_other_apps() {
    // Filter to an app name that definitely doesn't exist —
    // result should be the "no models registered" message.
    let mut buf: Vec<u8> = Vec::new();
    let r = showmodels_cmd(
        &["--app".into(), "xyz_definitely_not_an_app".into()],
        &mut buf,
    );
    assert!(r.is_ok());
    let s = String::from_utf8(buf).unwrap();
    assert!(
        s.contains("no models registered"),
        "expected empty-output marker for unknown app, got: {s}"
    );
}

/// Default template (no `--tenant`) keeps the v0.28
/// `#[derive(ViewSet)]` shape — single-tenant projects rely on
/// the mount-time `pool` argument.
#[test]
fn viewset_template_pool_emits_derive_macro() {
    let body = viewset_template_pool(
        "PostViewSet",
        &ModelRef::bare("Post"),
        "post_view_set",
        "/posts",
        "rustango",
        "app",
        true,
    );
    assert!(
        body.contains("#[derive(ViewSet)]"),
        "expected derive macro, got: {body}"
    );
    assert!(
        body.contains("PostViewSet::router"),
        "expected `Name::router(...)` mount hint, got: {body}"
    );
    assert!(
        !body.contains("tenant_router"),
        "pool template must NOT reference tenant_router, got: {body}"
    );
    // Phase 2b of #145 — default crate root preserves the
    // pre-#145 `use rustango::ViewSet;` line bit-identically.
    assert!(
        body.contains("use rustango::ViewSet;"),
        "default crate root must emit `use rustango::ViewSet;`, got: {body}"
    );
}

/// A scaffolded ViewSet is not an anonymous CRUD API (#1857).
#[test]
fn viewset_templates_guard_writes() {
    let live = |body: &str, needle: &str| body.lines().any(|l| l.trim_start().starts_with(needle));
    let tenant = viewset_template_tenant(
        "PostViewSet",
        &ModelRef::bare("Post"),
        "post_view_set",
        "/posts",
        "rustango",
    );
    assert!(
        live(&tenant, ".permissions_for_model::<Post>()"),
        "{tenant}"
    );
    let pool = viewset_template_pool(
        "PostViewSet",
        &ModelRef::bare("Post"),
        "post_view_set",
        "/posts",
        "rustango",
        "app",
        true,
    );
    assert!(live(&pool, "read_only,"), "{pool}");
    // Both name the write allow-list.
    assert!(
        live(&tenant, "ViewSet::for_model(Post::SCHEMA, &[\"id\"])"),
        "{tenant}"
    );
    assert!(live(&pool, "fields       = \"id"), "{pool}");
}

#[test]
fn viewset_template_pool_threads_renamed_crate_root() {
    // Phase 2b of #145 — passing `"rustango_orm"` produces source
    // a bare-ORM consumer can compile (post-#144 carve-out).
    // The `#[viewset(...)]` attribute name stays literal —
    // proc-macro contract is independent of the consumer's
    // Cargo.toml dep rename (same pattern #142 / renamed-smoke
    // proves for `#[rustango(...)]`).
    let body = viewset_template_pool(
        "PostViewSet",
        &ModelRef::bare("Post"),
        "post_view_set",
        "/posts",
        "rustango_orm",
        "app",
        true,
    );
    assert!(
        body.contains("use rustango_orm::ViewSet;"),
        "renamed crate root must propagate to `use` line, got: {body}"
    );
    assert!(
        !body.contains("use rustango::"),
        "no bare `use rustango::` should remain after rename, got: {body}"
    );
    assert!(
        body.contains("#[viewset("),
        "derive attribute name stays literal across renames, got: {body}"
    );
}

#[test]
fn viewset_template_tenant_threads_renamed_crate_root() {
    let body = viewset_template_tenant(
        "PostViewSet",
        &ModelRef::bare("Post"),
        "post_view_set",
        "/posts",
        "rustango_orm",
    );
    assert!(body.contains("use rustango_orm::core::Model as _;"));
    assert!(body.contains("use rustango_orm::viewset::ViewSet;"));
    assert!(!body.contains("use rustango::"));
}

#[test]
fn api_routes_template_pool_threads_renamed_crate_root() {
    let body = api_routes_template_pool("blog", "rustango_orm", &[]);
    assert!(body.contains("use rustango_orm::sql::Pool;"));
    assert!(!body.contains("use rustango::"));
}

/// `--tenant` template uses `ViewSet::for_model(...).tenant_router(...)`
/// and pulls `crate::extractors::Tenant`-shape connections instead
/// of baking a pool at mount time. Required for tenancy projects
/// (#80).
#[test]
fn viewset_template_tenant_uses_tenant_router() {
    let body = viewset_template_tenant(
        "PostViewSet",
        &ModelRef::bare("Post"),
        "post_view_set",
        "/posts",
        "rustango",
    );
    assert!(
        body.contains("ViewSet::for_model"),
        "expected runtime ViewSet::for_model builder, got: {body}"
    );
    assert!(
        body.contains(".tenant_router("),
        "expected `.tenant_router(...)` call, got: {body}"
    );
    assert!(
        !body.contains("#[derive(ViewSet)]"),
        "tenant template must NOT use the derive macro (pool-coupled), got: {body}"
    );
    assert!(
        body.contains(r#".tenant_router("/api/posts")"#),
        "expected the table route, got: {body}"
    );
    // The hint comment in the template should match the actual
    // function call site Devs will copy from it.
    assert!(
        body.contains("pub fn router()"),
        "expected `pub fn router()` so api_routes.rs can `.merge(...)`, got: {body}"
    );
    // v0.30.5 — the scaffolded body must demonstrate the v0.30
    // unified builder chain (filter / search / ordering / page /
    // perms) so users discover the surface without reading the
    // docs. Stale "v1 scope" caveat must NOT appear.
    assert!(
        !body.contains("v1 scope"),
        "v0.30 unification removed the v1 scope caveat — \
             template must reflect full feature parity, got: {body}"
    );
    for knob in [
        ".filter_fields(",
        ".search_fields(",
        ".ordering(",
        ".page_size(",
        ".permissions_for_model::",
    ] {
        assert!(
            body.contains(knob),
            "expected `{knob}` in tenant template (commented hint), got: {body}"
        );
    }
}

/// CWD is process-global; tests that chdir must be serialized
/// or `cargo test`'s default thread-pool will race them and
/// trip "No such file or directory" when one test's tempdir
/// drop runs while another is restoring CWD. This static
/// mutex serializes every chdir-based test in this module.
fn cwd_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// `make:worker` puts this name into the generated binary, so a
/// wrong answer is a path that cannot compile.
#[test]
fn package_name_reads_the_package_table() {
    let manifest = r#"[package]
name = "my-shop"
version = "0.1.0"

[dependencies]
name = "not-this-one"
rustango = { version = "0.71", features = ["batteries"] }
"#;
    assert_eq!(
        package_name_from_cargo_toml(manifest).as_deref(),
        // Hyphens become underscores: cargo's crate name, not the
        // package name, is what a `use` path needs.
        Some("my_shop"),
        "must read [package].name and fold hyphens, ignoring `name` keys \
             in any other table"
    );
}

#[test]
fn package_name_tolerates_spacing_and_missing_tables() {
    assert_eq!(
        package_name_from_cargo_toml("[package]\n  name   =   \"probe\"\n").as_deref(),
        Some("probe")
    );
    assert_eq!(
        package_name_from_cargo_toml("[dependencies]\nname = \"x\"\n"),
        None,
        "no [package] table → no name to report"
    );
    assert_eq!(package_name_from_cargo_toml(""), None);
}

/// `project_uses_tenancy` reads Cargo.toml from CWD and looks for
/// the `tenancy` feature on the `rustango` dep. Tested by
/// pushing a fixture file into a tempdir, chdir-ing in, and
/// asserting the result.
#[test]
fn project_uses_tenancy_detects_inline_features_array() {
    let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let cargo = dir.path().join("Cargo.toml");
    std::fs::write(
        &cargo,
        r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
rustango = { version = "0.30", features = ["tenancy", "manage"] }
"#,
    )
    .unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let detected = project_uses_tenancy();
    let _ = std::env::set_current_dir(&prev);
    assert!(
        detected,
        "inline-table dep with `tenancy` in features should auto-detect"
    );
}

/// `project_uses_tenancy` returns false on the dedicated dep
/// table form when `tenancy` isn't listed.
#[test]
fn project_uses_tenancy_false_when_feature_absent() {
    let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let cargo = dir.path().join("Cargo.toml");
    std::fs::write(
        &cargo,
        r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
rustango = { version = "0.30", features = ["postgres", "manage"] }
"#,
    )
    .unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let detected = project_uses_tenancy();
    let _ = std::env::set_current_dir(&prev);
    assert!(
        !detected,
        "no tenancy feature → must default to single-tenant scaffold"
    );
}

/// A relative `migrations` still finds the app folders beside it.
#[test]
fn check_dirs_scans_apps_beside_a_bare_migrations_dir() {
    let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("migrations")).unwrap();
    std::fs::create_dir_all(dir.path().join("blog/migrations")).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let labels: Vec<String> = check_dirs(Path::new("migrations"), true)
        .into_iter()
        .map(|(app, _)| app)
        .collect();
    let _ = std::env::set_current_dir(&prev);
    assert_eq!(labels, ["", "blog/"]);
}

/// Missing Cargo.toml (running outside a project) → false.
/// Scaffolder must not crash; auto-detect just falls back to
/// the safer single-tenant default.
#[test]
fn project_uses_tenancy_false_when_cargo_toml_missing() {
    let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let detected = project_uses_tenancy();
    let _ = std::env::set_current_dir(&prev);
    assert!(!detected);
}

/// `resolve_viewset_tenant_mode` is the pure decision the
/// `make:viewset` command runs after parsing flags. Tested as
/// a pure function so we don't need chdir + tempdir + writing
/// to a project tree (which earlier leaked test artifacts when
/// parallel tests raced on CWD).
///
/// Echo fires only on the implicit-auto-detect path; explicit
/// `--tenant` / `--no-tenant` users already know what they
/// asked for.
#[test]
fn resolve_viewset_tenant_mode_decision_table() {
    // (explicit_tenant, explicit_no_tenant, project_tenancy)
    // → (tenant_aware, echo)
    let cases: &[(bool, bool, bool, bool, bool)] = &[
        // No flags, no tenancy detected → pool, no echo.
        (false, false, false, false, false),
        // No flags, tenancy detected → tenant + echo.
        (false, false, true, true, true),
        // Explicit --tenant, no tenancy detected → tenant, no echo.
        (true, false, false, true, false),
        // Explicit --tenant, tenancy detected → tenant, no echo
        // (the user asked for it, no need to inform).
        (true, false, true, true, false),
        // Explicit --no-tenant always wins → pool, no echo.
        (false, true, false, false, false),
        (false, true, true, false, false),
        // Both flags set: --no-tenant is the safer default and
        // takes precedence (the function checks it first).
        (true, true, true, false, false),
    ];
    for &(et, ent, pt, want_tenant, want_echo) in cases {
        let (tenant, echo) = resolve_viewset_tenant_mode(et, ent, pt);
        assert_eq!(
            (tenant, echo),
            (want_tenant, want_echo),
            "case (explicit_tenant={et}, explicit_no_tenant={ent}, project_tenancy={pt})"
        );
    }
}

// -------- make:api_routes (#82-partial) --------

/// Tenancy template emits the no-arg `pub fn api()` shape and
/// references `tenant_router` in the placeholder hint.
#[test]
fn api_routes_template_tenant_emits_no_arg_fn() {
    let body = api_routes_template_tenant("regions");
    assert!(
        body.contains("pub fn api() -> Router<()>"),
        "expected no-arg `pub fn api()`, got: {body}"
    );
    assert!(
        body.contains(".merge(crate::<snake>::router())"),
        "expected a make:viewset --tenant mount hint, got: {body}"
    );
    assert!(
        !body.contains("PgPool"),
        "tenant template must NOT thread PgPool through api(), got: {body}"
    );
}

/// Default template threads the dialect-agnostic `sql::Pool` so
/// per-model viewsets have something to capture at mount time.
///
/// It threaded `PgPool` until 0.57.5, which pinned every generated
/// project to Postgres at the one place three backends are meant to
/// be interchangeable — `ViewSet::router_pool` takes `sql::Pool`.
#[test]
fn api_routes_template_pool_threads_a_dialect_agnostic_pool() {
    let body = api_routes_template_pool("blog", "rustango", &[]);
    assert!(
        body.contains("pub fn api(pool: Pool) -> Router<()>"),
        "expected pool-arg api fn, got: {body}"
    );
    assert!(
        body.contains("use rustango::sql::Pool;"),
        "default crate root must emit `use rustango::sql::Pool;`, got: {body}"
    );
    // Comments stripped first: the template's own header explains
    // why a `PgPool` would be wrong here, and a raw substring search
    // matches that prose rather than the code it warns about.
    let code: String = body
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("PgPool"),
        "a generated project must not be pinned to Postgres here, got: {code}"
    );
}

/// App-name validator accepts snake_case and rejects hyphens /
/// uppercase / leading digits — same rule we apply to table
/// names in the macro. Keeps `src/<app>/` shape Rust-friendly.
#[test]
fn is_valid_app_name_snake_case_only() {
    assert!(is_valid_app_name("regions"));
    assert!(is_valid_app_name("blog_posts"));
    assert!(is_valid_app_name("_internal"));
    assert!(!is_valid_app_name(""));
    assert!(!is_valid_app_name("Regions"));
    assert!(!is_valid_app_name("region-app"));
    assert!(!is_valid_app_name("9_apps"));
}

// -------- run_deploy_audit (`manage check --deploy`) --------

#[cfg(feature = "cache")]
#[test]
fn deploy_audit_flags_a_process_local_lockout_only() {
    use crate::account_lockout::Lockout;
    use std::sync::Arc;
    let mut out = DeployAuditFindings::default();
    login_store_audit(
        &Lockout::new(Arc::new(crate::cache::InMemoryCache::new())),
        &mut out,
    );
    // Advice, not a warning: the app may install a shared lockout at startup.
    assert!(
        out.info.iter().any(|w| w.contains("account lockout")),
        "{out:?}"
    );
    assert!(
        !out.warnings.iter().any(|w| w.contains("account lockout")),
        "{out:?}"
    );
    let scoped =
        crate::cache::ScopedCache::new(Arc::new(crate::cache::InMemoryCache::new()), "tenant:acme");
    assert!(Lockout::new(Arc::new(scoped)).is_process_local());
    // A store shared across replicas: nothing to say.
    let mut out = DeployAuditFindings::default();
    login_store_audit(&Lockout::new(Arc::new(SharedStandIn)), &mut out);
    assert!(
        !out.info.iter().any(|w| w.contains("account lockout")),
        "{out:?}"
    );
    // NullCache never locks: a warning, not advice (#1809).
    let mut out = DeployAuditFindings::default();
    login_store_audit(&Lockout::new(Arc::new(crate::cache::NullCache)), &mut out);
    assert!(
        out.warnings.iter().any(|w| w.contains("stores nothing")),
        "{out:?}"
    );
    // Also behind a tenant scope.
    let scoped = crate::cache::ScopedCache::new(Arc::new(crate::cache::NullCache), "tenant:acme");
    let mut out = DeployAuditFindings::default();
    login_store_audit(&Lockout::new(Arc::new(scoped)), &mut out);
    assert!(
        out.warnings.iter().any(|w| w.contains("stores nothing")),
        "{out:?}"
    );
}

/// Stands in for Redis/DB: keeps nothing, but claims nothing either.
#[cfg(feature = "cache")]
struct SharedStandIn;

#[cfg(feature = "cache")]
#[async_trait::async_trait]
impl crate::cache::Cache for SharedStandIn {
    async fn touch(
        &self,
        _: &str,
        _: Option<std::time::Duration>,
    ) -> Result<bool, crate::cache::CacheError> {
        Ok(false)
    }
    async fn get(&self, _: &str) -> Result<Option<String>, crate::cache::CacheError> {
        Ok(None)
    }
    async fn set(
        &self,
        _: &str,
        _: &str,
        _: Option<std::time::Duration>,
    ) -> Result<(), crate::cache::CacheError> {
        Ok(())
    }
    async fn delete(&self, _: &str) -> Result<(), crate::cache::CacheError> {
        Ok(())
    }
    async fn exists(&self, _: &str) -> Result<bool, crate::cache::CacheError> {
        Ok(false)
    }
    async fn clear(&self) -> Result<(), crate::cache::CacheError> {
        Ok(())
    }
}

#[cfg(all(feature = "cache", feature = "sqlite"))]
#[tokio::test]
async fn check_deploy_notes_the_default_in_memory_lockout() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let _ = check_cmd(
        &pool,
        Path::new("/nonexistent"),
        &["--deploy".into()],
        &mut buf,
    )
    .await;
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("[info]    account lockout"), "{s}");
}

/// An unknown verb names the nearest one, through the real dispatch.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn an_unknown_verb_suggests_the_nearest() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let err = run_with_writer(
        &pool,
        Path::new("/nonexistent"),
        ["migrat".to_owned()],
        &mut buf,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("did you mean `migrate`?"), "{err}");
}

/// A misspelt flag fails instead of running the plain check.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn check_refuses_an_unknown_argument() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let err = check_cmd(
        &pool,
        Path::new("/nonexistent"),
        &["--deplyo".into()],
        &mut buf,
    )
    .await
    .expect_err("--deplyo must fail");
    assert!(err.to_string().contains("--deplyo"), "{err}");
}

/// A file that creates `table`.
#[cfg(feature = "sqlite")]
fn create_mig(name: &str, prev: Option<&str>, table: &str) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-10-14T00:00:00Z".into(),
        prev: prev.map(str::to_owned),
        atomic: true,
        scope: super::super::MigrationScope::Tenant,
        replaces: Vec::new(),
        merges: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{"name": table, "model": "T",
                "fields": [{"name": "id", "column": "id", "ty": "i64",
                            "nullable": false, "primary_key": true}]}]
        }))
        .unwrap(),
        forward: vec![Operation::Schema(super::super::SchemaChange::CreateTable(
            table.into(),
        ))],
    }
}

#[cfg(feature = "sqlite")]
async fn manage_out(pool: &Pool, dir: &Path, args: &[&str]) -> Result<String, MigrateError> {
    let mut buf: Vec<u8> = Vec::new();
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    run_with_writer(pool, dir, args, &mut buf).await?;
    Ok(String::from_utf8(buf).unwrap())
}

/// Dry-run, showmigrations and sqlmigrate see the system chain
/// `migrate` runs.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn previews_cover_the_system_chain() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"]).await;
    let out = out.unwrap();
    assert!(out.contains("not on disk yet"), "{out}");

    manage_out(&pool, &dir, &["migrate"])
        .await
        .expect("migrate");
    let step = file::list_dir(&root.path().join("system").join("migrations"))
        .unwrap()
        .into_iter()
        .find(|m| m.scope == super::super::MigrationScope::Tenant)
        .expect("a system step");
    crate::sql::raw_execute_pool(
        &pool,
        "DELETE FROM __rustango_system_migrations__ WHERE name = ?",
        vec![crate::core::SqlValue::String(step.name.clone())],
    )
    .await
    .unwrap();
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"]).await;
    let out = out.unwrap();
    assert!(out.contains(&format!("-- {} (system,", step.name)), "{out}");
    let out = manage_out(&pool, &dir, &["showmigrations"]).await;
    let out = out.unwrap();
    assert!(out.contains(&format!("[ ] {}", step.name)), "{out}");
    let out = manage_out(&pool, &dir, &["sqlmigrate", &step.name]).await;
    assert!(out.expect("sqlmigrate").contains(&step.name));
}

/// A system table the project chain creates is previewed
/// once, under the project, as `migrate` creates it.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn dry_run_leaves_project_owned_tables_to_the_project() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = Pool::connect("sqlite::memory:").await.unwrap();
    manage_out(&scratch, &dir, &["migrate"])
        .await
        .expect("migrate");
    let table = file::list_dir(&root.path().join("system").join("migrations"))
        .unwrap()
        .into_iter()
        .filter(|m| m.scope == super::super::MigrationScope::Tenant)
        .find_map(|m| {
            m.forward.iter().find_map(|op| match op {
                Operation::Schema(super::super::SchemaChange::CreateTable(t)) => Some(t.clone()),
                _ => None,
            })
        })
        .expect("a system table");
    file::write(
        &dir.join("0001_own.json"),
        &create_mig("0001_own", None, &table),
    )
    .unwrap();

    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"]).await;
    let out = out.unwrap();
    let creates = out
        .lines()
        .filter(|l| l.starts_with("CREATE TABLE") && l.contains(&format!("\"{table}\"")))
        .count();
    assert_eq!(creates, 1, "{out}");
    assert!(out.contains("(system,"), "{out}");
}

/// Dry-run shows the bootstrap DDL `migrate` runs for a missing table.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn dry_run_shows_bootstrap_ddl() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    // No chain creates translations, so only the bootstrap shows it.
    let creates_translations = |out: &str| {
        out.lines()
            .any(|l| l.starts_with("CREATE TABLE") && l.contains("rustango_translations"))
    };
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"])
        .await
        .unwrap();
    assert!(creates_translations(&out), "{out}");
    // A preview creates nothing: no table, no ledger.
    let tables: Vec<(i64,)> =
        crate::sql::raw_query_pool("SELECT COUNT(*) FROM sqlite_master", Vec::new(), &pool)
            .await
            .unwrap();
    assert_eq!(tables[0].0, 0, "the dry run wrote to the database");
    manage_out(&pool, &dir, &["migrate"])
        .await
        .expect("migrate");
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"])
        .await
        .unwrap();
    assert!(!creates_translations(&out), "{out}");
    assert!(out.contains("nothing to migrate"), "{out}");
}

/// Dry-run shows the system step `migrate` would generate first.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn dry_run_shows_the_system_step_migrate_generates() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"])
        .await
        .unwrap();
    assert!(out.contains("(system,"), "{out}");
    assert!(
        !root.path().join("system").exists(),
        "a preview writes no system files"
    );
}

/// Dry-run shows the columns `migrate` adds to a framework table
/// the project chain created.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn dry_run_shows_the_converge_of_project_owned_tables() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = Pool::connect("sqlite::memory:").await.unwrap();
    manage_out(&scratch, &dir, &["migrate"])
        .await
        .expect("migrate");
    let (table, column) = file::list_dir(&root.path().join("system").join("migrations"))
        .unwrap()
        .into_iter()
        .filter(|m| m.scope == super::super::MigrationScope::Tenant)
        .find_map(|m| {
            m.forward.iter().find_map(|op| match op {
                Operation::Schema(super::super::SchemaChange::CreateTable(t)) => {
                    let fields = &m.snapshot.table(t)?.fields;
                    let id = fields.iter().find(|f| f.primary_key)?;
                    let other = fields.iter().find(|f| !f.primary_key)?;
                    (id.column == "id").then(|| (t.clone(), other.column.clone()))
                }
                _ => None,
            })
        })
        .expect("a system table with an id pk");
    file::write(
        &dir.join("0001_own.json"),
        &create_mig("0001_own", None, &table),
    )
    .unwrap();

    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    runner::migrate_pool(&pool, &dir)
        .await
        .expect("project chain");
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"])
        .await
        .unwrap();
    let adds = format!("ALTER TABLE \"{table}\" ADD COLUMN \"{column}\"");
    assert!(out.contains(&adds), "{adds} missing from:\n{out}");
}

/// A dump reaches the caller's writer in large chunks, not per entry.
#[test]
fn fixture_writer_buffers() {
    struct Counting(usize, Vec<u8>);
    impl Write for Counting {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += 1;
            self.1.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Counting(0, Vec::new());
    let mut out = FixtureWriter::new(&mut sink, 2);
    for pk in 0..100 {
        out.entry(&serde_json::json!({"pk": pk})).unwrap();
    }
    out.finish().unwrap();
    let text = String::from_utf8(sink.1).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap()[99]["pk"],
        99
    );
    assert!(sink.0 < 10, "{} writes for 100 entries", sink.0);
}

/// A full disk on the last buffer must fail the dump, not be lost on drop.
#[test]
fn fixture_writer_reports_a_failed_flush() {
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Full;
    let mut out = FixtureWriter::new(&mut sink, 0);
    out.entry(&serde_json::json!({"pk": 1})).unwrap();
    let err = out.finish().expect_err("lost write error");
    assert!(err.to_string().contains("disk full"), "{err}");
}

#[allow(dead_code)]
#[derive(crate::Model, Debug)]
#[rustango(
    table = "dl1399_post",
    m2m(
        name = "tags",
        to = "dl1399_tag",
        through = "dl1399_post_tags",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct DlPost {
    #[rustango(primary_key)]
    pub id: i64,
}

#[allow(dead_code)]
#[derive(crate::Model, Debug)]
#[rustango(table = "dl1399_tag")]
pub struct DlTag {
    #[rustango(primary_key)]
    pub id: i64,
}

/// A page reads only its own rows' links, not the whole junction.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn m2m_links_reads_only_the_page() {
    use crate::core::Model as _;
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    for sql in [
        "CREATE TABLE dl1399_post_tags (post_id BIGINT NOT NULL, tag_id BIGINT NOT NULL)",
        "INSERT INTO dl1399_post_tags VALUES (1, 10), (2, 20)",
    ] {
        crate::sql::raw_execute_pool(&pool, sql, Vec::new())
            .await
            .unwrap();
    }
    let page = [crate::core::SqlValue::I64(1)];
    let links = m2m_links(&pool, DlPost::SCHEMA, &page).await.unwrap();
    let [(_, by_src)] = links.as_slice() else {
        panic!("one relation: {links:?}");
    };
    let srcs: Vec<&String> = by_src.keys().collect();
    assert_eq!(srcs, ["1"], "read links of rows outside the page");
}

/// Once a squash is applied, the file it replaced is not pending.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn a_squashed_file_is_not_pending() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let first = create_mig("0001_a", None, "sq1311_a");
    file::write(&dir.join("0001_a.json"), &first).unwrap();
    manage_out(&pool, &dir, &["migrate"])
        .await
        .expect("migrate");
    let mut squash = create_mig("0002_sq", Some("0001_a"), "sq1311_a");
    squash.replaces = vec!["0001_a".into()];
    file::write(&dir.join("0002_sq.json"), &squash).unwrap();
    manage_out(&pool, &dir, &["migrate"]).await.expect("squash");

    let out = manage_out(&pool, &dir, &["showmigrations"]).await;
    let out = out.unwrap();
    assert!(out.contains("[X] 0001_a"), "{out}");
    let out = manage_out(&pool, &dir, &["migrate", "--dry-run"]).await;
    let out = out.unwrap();
    assert!(!out.contains("0001_a"), "{out}");
}

/// `--squash` refuses pending files whose hand-written ops a re-diff
/// would lose, and deletes nothing.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn squash_refuses_hand_written_ops() {
    use super::super::{CallbackOp, SchemaChange};
    let hand = [
        Operation::Data(DataOp {
            sql: "UPDATE t SET x = 1".into(),
            reverse_sql: None,
            reversible: false,
        }),
        Operation::Callback(CallbackOp {
            name: "fill".into(),
            reverse_name: None,
        }),
        Operation::Schema(SchemaChange::RenameTable {
            old_name: "sq_a".into(),
            new_name: "sq_b".into(),
        }),
        Operation::Schema(SchemaChange::RenameColumn {
            table: "sq_a".into(),
            old_column: "x".into(),
            new_column: "y".into(),
        }),
    ];
    for op in hand {
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        file::write(
            &dir.join("0001_a.json"),
            &create_mig("0001_a", None, "sq_a"),
        )
        .unwrap();
        let mut second = create_mig("0002_b", Some("0001_a"), "sq_a");
        second.atomic = false;
        second.forward = vec![op.clone()];
        file::write(&dir.join("0002_b.json"), &second).unwrap();
        let err = manage_out(&pool, &dir, &["migrate", "--squash"])
            .await
            .expect_err("a re-diff would lose the op");
        assert!(err.to_string().contains("0002_b"), "{op:?}: {err}");
        assert!(!err.to_string().contains("UPDATE"), "the SQL leaked: {err}");
        assert!(
            dir.join("0002_b.json").exists(),
            "{op:?}: the file was deleted"
        );
    }
}

#[test]
fn missing_time_zone_tables_warn() {
    assert!(time_zone_warning(None).is_some());
    assert!(time_zone_warning(Some("2000-01-01 00:00:00")).is_none());
}

#[test]
fn ci_and_bin_collations_warn() {
    assert!(collation_warning("utf8mb4_0900_ai_ci").is_some());
    assert!(collation_warning("utf8mb4_general_ci").is_some());
    assert!(collation_warning("utf8mb4_0900_as_cs").is_none());
    assert!(collation_warning("utf8mb4_bin").is_some());
}

/// SQLite compares byte-wise, so there is nothing to read or warn about.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn check_deploy_has_no_collation_warning_on_sqlite() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let _ = check_cmd(
        &pool,
        Path::new("/nonexistent"),
        &["--deploy".into()],
        &mut buf,
    )
    .await;
    let s = String::from_utf8(buf).unwrap();
    assert!(!s.contains("collation"), "{s}");
}

fn good_prod_env() -> DeployAuditEnv {
    DeployAuditEnv {
        rustango_env: Some("prod".into()),
        session_secret: Some("a".repeat(48)),
        database_url: Ok("postgres://app:s3cr3t@db.example.com/app_prod".into()),
        database_style_conflict: None,
        apex_domain: ApexDomain::Tenancy(Some("app.example.com".into())),
        bind: Some("0.0.0.0:8080".into()),
        ungated_admin: false,
    }
}

/// #2425 — the tenant-scheme warning is the tenancy dispatcher's, so a
/// single-database deploy never sees it.
#[test]
fn deploy_audit_says_nothing_about_the_tenant_scheme() {
    let r = run(&good_prod_env());
    assert!(
        !r.warnings
            .iter()
            .any(|w| w.contains("RUSTANGO_TENANT_SCHEME")),
        "{:?}",
        r.warnings
    );
}

/// #1627 — an admin built without session auth is named.
#[test]
fn deploy_audit_warns_on_an_ungated_admin() {
    let env = DeployAuditEnv {
        ungated_admin: true,
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.warnings.iter().any(|w| w.contains("with_session_auth")),
        "{:?}",
        r.warnings
    );
}

/// The flag the audit reads is set by building an ungated admin, and
/// only by that: a gated or tenancy-gated one leaves it clear.
#[cfg(all(feature = "admin", feature = "sqlite"))]
#[tokio::test]
async fn only_an_ungated_admin_reaches_the_deploy_audit() {
    let _g = crate::admin::ungated_flag_lock().lock().await;
    crate::admin::reset_ungated_admin_built();
    let pool = crate::sql::Pool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    let secret = crate::session::SessionSecret::from_bytes(vec![7; 32]);
    let _gated = crate::admin::Builder::new(pool.clone())
        .with_session_auth(secret)
        .build();
    // The public `tenant_mode` hides models; it gates nothing.
    let gated_clear = !deploy_audit_env(false).ungated_admin;
    let _open = crate::admin::Builder::new(pool.clone())
        .tenant_mode()
        .build();
    let flagged = deploy_audit_env(false).ungated_admin;
    crate::admin::reset_ungated_admin_built();
    assert!(gated_clear, "a gated admin set the ungated flag");
    assert!(
        flagged,
        "a `tenant_mode` admin without auth was not flagged"
    );
}

fn run(env: &DeployAuditEnv) -> DeployAuditFindings {
    let mut out = DeployAuditFindings::default();
    run_deploy_audit(env, &mut out);
    out
}

#[test]
fn deploy_audit_clean_prod_env_has_no_warnings_or_errors() {
    let r = run(&good_prod_env());
    assert!(
        r.errors.is_empty(),
        "expected no errors in clean prod env, got: {:?}",
        r.errors
    );
    assert!(
        r.warnings.is_empty(),
        "expected no warnings in clean prod env, got: {:?}",
        r.warnings
    );
}

#[test]
fn deploy_audit_unset_session_secret_errors() {
    let env = DeployAuditEnv {
        session_secret: None,
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.errors
            .iter()
            .any(|e| e.contains("RUSTANGO_SESSION_SECRET")),
        "expected error for unset RUSTANGO_SESSION_SECRET, got: {:?}",
        r.errors
    );
}

/// The unset-secret error says what the runtime does: no start on prod,
/// the dev key in `./var/` off prod. Tenant cookies only in tenancy builds.
#[test]
fn deploy_audit_unset_session_secret_says_what_happens() {
    let env = DeployAuditEnv {
        session_secret: None,
        ..good_prod_env()
    };
    let r = run(&env);
    let e = r
        .errors
        .iter()
        .find(|e| e.contains("RUSTANGO_SESSION_SECRET is unset"))
        .expect("unset error");
    assert!(e.contains("will not start") && e.contains("./var/"), "{e}");
    assert!(!e.contains("ephemeral"), "{e}");
    assert_eq!(e.contains("tenant"), cfg!(feature = "tenancy"), "{e}");
}

/// Gated: without `admin`/`tenancy` the audit cannot decode, and
/// reports the secret unvalidated instead of measuring it.
#[cfg(any(feature = "admin", feature = "tenancy"))]
#[test]
fn deploy_audit_short_session_secret_errors() {
    // Valid base64, deliberately — decodes to 6 bytes. `"too-short"`
    // was the old fixture and its hyphen made it a *base64* failure,
    // so this case never actually reached the length check.
    let env = DeployAuditEnv {
        session_secret: Some("AAAAAAAA".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.errors.iter().any(|e| e.contains("decoded to 6 bytes")),
        "expected a decoded-length error, got: {:?}",
        r.errors
    );
}

/// Gated for the same reason as the two above.
#[cfg(any(feature = "admin", feature = "tenancy"))]
#[test]
fn deploy_audit_non_base64_session_secret_errors() {
    let env = DeployAuditEnv {
        session_secret: Some("correct-horse-battery-staple-and-then-som".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.errors.iter().any(|e| e.contains("not valid base64")),
        "a long non-base64 value is still not a key, got: {:?}",
        r.errors
    );
}

/// #1396 — the value that passed this check and broke the runtime.
///
/// 32 base64 characters is 24 bytes of key. The audit measured the
/// encoded string (`s.len() >= 32`), reported "length OK", and the
/// cookie layer then decoded it, saw 24 bytes, and fell back to a
/// random per-process key. Passing the check predicted nothing.
///
/// Both now call `SessionSecret::from_b64`, so this is the audit and
/// the runtime answering with one voice rather than two.
///
/// Gated like the code it covers: `session` is `admin`/`tenancy`-only,
/// and in a build with neither the audit reports the secret
/// unvalidated, so there is nothing here to assert.
#[cfg(any(feature = "admin", feature = "tenancy"))]
#[test]
fn deploy_audit_rejects_a_32_char_base64_secret_the_runtime_refuses() {
    let trap = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    assert_eq!(
        trap.len(),
        32,
        "the premise: 32 characters, so the old check passed"
    );
    assert!(
        crate::session::SessionSecret::from_b64(trap).is_err(),
        "the premise: the runtime refuses it"
    );

    let env = DeployAuditEnv {
        session_secret: Some(trap.into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.errors.iter().any(|e| e.contains("decoded to 24 bytes")),
        "check --deploy must refuse what the runtime refuses, got: {:?}",
        r.errors
    );
    assert!(
        !r.info.iter().any(|i| i.contains("SESSION_SECRET")),
        "and must not also report it OK, got: {:?}",
        r.info
    );
}

/// The scaffolder used to write
/// `RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more`
/// to `.env.example`; it now ships the line commented out, because the
/// value is not 32 bytes of base64 and was discarded silently (#1359).
///
/// This check stays regardless: every `.env` copied from an older
/// scaffold still carries it, and shipping a known-public "secret" to
/// production should be a loud error rather than a quiet fallback.
#[test]
fn deploy_audit_placeholder_session_secret_errors() {
    let env = DeployAuditEnv {
        session_secret: Some("change-me-base64-encoded-32-bytes-or-more".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.errors
            .iter()
            .any(|e| e.contains("placeholder") || e.contains("change-me")),
        "expected error for unchanged placeholder secret, got: {:?}",
        r.errors
    );
}

#[test]
fn deploy_audit_unset_rustango_env_warns() {
    let env = DeployAuditEnv {
        rustango_env: None,
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.warnings.iter().any(|w| w.contains("RUSTANGO_ENV")),
        "expected warning for unset RUSTANGO_ENV, got: {:?}",
        r.warnings
    );
}

#[test]
fn deploy_audit_dev_rustango_env_warns() {
    let env = DeployAuditEnv {
        rustango_env: Some("dev".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.warnings.iter().any(|w| w.contains("`dev`")),
        "expected warning for non-prod RUSTANGO_ENV, got: {:?}",
        r.warnings
    );
}

#[test]
fn deploy_audit_unset_database_url_errors() {
    let env = DeployAuditEnv {
        database_url: Err("environment variable `DATABASE_URL` is required".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.errors.iter().any(|e| e.contains("DATABASE_URL")),
        "expected error for unset DATABASE_URL, got: {:?}",
        r.errors
    );
}

/// On prod a URL only a settings file names is a deploy error.
#[test]
fn deploy_audit_refuses_a_settings_file_database_url_on_prod() {
    let get = |n: &str| (n == "RUSTANGO_ENV").then(|| "prod".to_owned());
    let env = DeployAuditEnv {
        database_url: crate::env::resolve_database_url_from(&get, Some("postgres://dev/x"))
            .map(|(url, _)| url)
            .map_err(|e| e.to_string()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.errors
            .iter()
            .any(|e| e.contains("RUSTANGO__DATABASE__URL") && !e.contains("dev/x")),
        "{:?}",
        r.errors
    );
}

#[test]
fn deploy_audit_reports_both_database_styles() {
    let env = DeployAuditEnv {
        database_style_conflict: Some("both DATABASE_URL and DB_USER are set".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.warnings.iter().any(|w| w.contains("DB_USER")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn deploy_audit_localhost_database_url_warns() {
    let env = DeployAuditEnv {
        database_url: Ok("postgres://app:p@localhost/db".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("DATABASE_URL") && w.contains("localhost")),
        "expected warning for localhost DATABASE_URL, got: {:?}",
        r.warnings
    );
}

/// A single-database deploy has no apex to set.
#[test]
fn deploy_audit_single_tenant_skips_the_apex() {
    let env = DeployAuditEnv {
        apex_domain: ApexDomain::SingleTenant,
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert!(!r.info.iter().any(|i| i.contains("RUSTANGO_APEX_DOMAIN")));
}

#[test]
fn deploy_audit_localhost_apex_warns_for_tenancy() {
    let env = DeployAuditEnv {
        apex_domain: ApexDomain::Tenancy(Some("localhost".into())),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("RUSTANGO_APEX_DOMAIN")),
        "expected warning for localhost apex, got: {:?}",
        r.warnings
    );
}

#[test]
fn deploy_audit_loopback_bind_warns() {
    let env = DeployAuditEnv {
        bind: Some("127.0.0.1:8080".into()),
        ..good_prod_env()
    };
    let r = run(&env);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("RUSTANGO_BIND") && w.contains("loopback")),
        "expected warning for loopback bind, got: {:?}",
        r.warnings
    );
}

/// `check --settings` names each key's source and never prints a secret.
#[cfg(feature = "config")]
#[test]
fn check_settings_names_sources_and_hides_secrets() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("default.toml"),
        "[database]\nurl = \"postgres://app:DBSECRET@db/app\"\npool_max_size = 7\n\
             [server]\nbind = \"127.0.0.1:9\"\n[mail]\nsmtp_host = \"smtp.x\"\n",
    )
    .unwrap();
    let env = vec![(
        "RUSTANGO__MAIL__SMTP_PASSWORD".to_owned(),
        "MAILSECRET".to_owned(),
    )];
    let report = |get: &dyn Fn(&str) -> Option<String>| {
        let loaded = crate::config::report_from(dir.path(), "dev", env.clone());
        let mut out = Vec::new();
        write_settings_report(&mut out, "dev", loaded, get).unwrap();
        String::from_utf8(out).unwrap()
    };
    let line = |text: &str, key: &str| {
        text.lines()
            .find(|l| l.trim_start().starts_with(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no {key} in {text}"))
            .to_owned()
    };
    let default_toml = dir.path().join("default.toml").display().to_string();
    let vars = |name: &str| {
        [
            ("RUSTANGO_BIND", "0.0.0.0:1"),
            ("RUSTANGO__MAIL__SMTP_PASSWORD", "MAILSECRET"),
        ]
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| (*v).to_owned())
    };
    let text = report(&vars);
    for secret in ["DBSECRET", "MAILSECRET"] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    let db = line(&text, "database.url");
    assert!(
        db.contains(&default_toml) && db.contains("<hidden>"),
        "{db}"
    );
    assert!(line(&text, "database.pool_max_size").ends_with(&format!("{default_toml} = 7")));
    assert!(line(&text, "server.bind").contains("env RUSTANGO_BIND = 0.0.0.0:1"));
    assert!(
        line(&text, "mail.smtp_password").contains("env RUSTANGO__MAIL__SMTP_PASSWORD = <hidden>")
    );
    assert!(line(&text, "database.backend").contains("default"));

    // A bare DATABASE_URL beats the file.
    let env_db = |name: &str| (name == "DATABASE_URL").then(|| "sqlite::memory:".to_owned());
    assert!(line(&report(&env_db), "database.url").contains("env DATABASE_URL = <hidden>"));

    // No URL anywhere: required and missing.
    let none = |_: &str| None;
    let mut out = Vec::new();
    write_settings_report(&mut out, "dev", Err(missing_config()), &none).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        line(&text, "database.url").contains("MISSING (required)"),
        "{text}"
    );

    // A bad DB_PORT is invalid, not missing.
    let bad_port = |name: &str| match name {
        "DB_USER" | "DB_NAME" => Some("x".to_owned()),
        "DB_PORT" => Some("nope".to_owned()),
        _ => None,
    };
    let db = line(&report(&bad_port), "database.url");
    assert!(db.contains("INVALID") && db.contains("DB_PORT"), "{db}");
}

#[cfg(feature = "config")]
fn missing_config() -> crate::config::ConfigError {
    crate::config::ConfigError::Io {
        path: "config/default.toml".into(),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    }
}

// -------- settings_audit_check (#87 slice 4) --------

#[cfg(feature = "config")]
fn settings_run(env_tier: &str, s: &crate::config::Settings) -> DeployAuditFindings {
    let mut out = DeployAuditFindings::default();
    settings_audit_check(env_tier, s, &mut out);
    out
}

/// In dev/staging tiers, the audit is a no-op — operators
/// expect dev defaults to be loud only when promoted to prod.
#[cfg(feature = "config")]
#[test]
fn settings_audit_dev_tier_is_quiet() {
    let mut s = crate::config::Settings::default();
    s.security.headers_preset = Some("dev".into());
    s.security.hsts_max_age_secs = Some(0);
    s.auth.argon2_memory_kib = Some(1024); // dev-fast
    let r = settings_run("dev", &s);
    assert!(
        r.warnings.is_empty(),
        "dev tier should be quiet, got: {:?}",
        r.warnings
    );
    assert!(
        r.info.is_empty(),
        "dev tier should be quiet, got: {:?}",
        r.info
    );
}

/// Prod `secure_cookies = false`, a header-stripping or unknown preset,
/// and a missing config are all reported.
#[cfg(feature = "config")]
#[test]
fn settings_audit_reports_insecure_cookies_presets_and_missing_config() {
    let mut s = crate::config::Settings::default();
    s.security.secure_cookies = Some(false);
    let r = settings_run("prod", &s);
    assert!(
        r.warnings.iter().any(|w| w.contains("secure_cookies")),
        "{:?}",
        r.warnings
    );
    for preset in ["empty", "strixt"] {
        let mut s = crate::config::Settings::default();
        s.security.headers_preset = Some(preset.into());
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("headers_preset") && w.contains(preset)),
            "{preset}: {:?}",
            r.warnings
        );
    }
    let mut s = crate::config::Settings::default();
    s.security.headers_preset = Some("relaxed".into());
    let r = settings_run("prod", &s);
    assert!(
        !r.warnings.iter().any(|w| w.contains("headers_preset")),
        "{:?}",
        r.warnings
    );

    let empty = tempfile::tempdir().unwrap();
    let missing = crate::config::Settings::load_from(empty.path(), "prod");
    let mut out = DeployAuditFindings::default();
    settings_audit_loaded("prod", missing, &mut out);
    assert!(
        out.warnings.iter().any(|w| w.contains("default.toml")),
        "{:?}",
        out.warnings
    );
    let missing = crate::config::Settings::load_from(empty.path(), "dev");
    let mut out = DeployAuditFindings::default();
    settings_audit_loaded("dev", missing, &mut out);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}

/// Any prod spelling runs the prod checks, in both halves of the audit.
#[cfg(feature = "config")]
#[test]
fn deploy_audit_treats_every_prod_spelling_as_prod() {
    let mut s = crate::config::Settings::default();
    s.security.hsts_max_age_secs = Some(0);
    for tier in ["PROD", "Production", " prod "] {
        let r = settings_run(tier, &s);
        assert!(
            r.warnings.iter().any(|w| w.contains("hsts_max_age_secs")),
            "{tier:?}: {:?}",
            r.warnings
        );
        let mut env = good_prod_env();
        env.rustango_env = Some(tier.into());
        let r = run(&env);
        assert!(r.warnings.is_empty(), "{tier:?}: {:?}", r.warnings);
    }
}

/// In prod, `headers_preset = "dev"` is a clear footgun —
/// emit a warning pointing at the fix.
#[cfg(feature = "config")]
#[test]
fn settings_audit_prod_with_dev_headers_preset_warns() {
    let mut s = crate::config::Settings::default();
    s.security.headers_preset = Some("dev".into());
    let r = settings_run("prod", &s);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("headers_preset") && w.contains("dev")),
        "expected warning for dev headers in prod, got: {:?}",
        r.warnings
    );
}

/// `hsts_max_age_secs = 0` in prod disables HSTS.
#[cfg(feature = "config")]
#[test]
fn settings_audit_prod_with_zero_hsts_warns() {
    let mut s = crate::config::Settings::default();
    s.security.hsts_max_age_secs = Some(0);
    let r = settings_run("prod", &s);
    assert!(
        r.warnings.iter().any(|w| w.contains("hsts_max_age_secs")),
        "expected HSTS warning, got: {:?}",
        r.warnings
    );
}

/// argon2 below the OWASP 2024 floor warns. Default values
/// (None) don't warn — they fall through to the framework's
/// hardcoded sensible default.
#[cfg(feature = "config")]
#[test]
fn settings_audit_prod_low_argon2_warns_but_unset_is_quiet() {
    let mut s = crate::config::Settings::default();
    s.auth.argon2_memory_kib = Some(4096); // way below 19456
    let r = settings_run("prod", &s);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("argon2_memory_kib") && w.contains("19456")),
        "expected argon2 floor warning, got: {:?}",
        r.warnings
    );

    // Unset = framework default. Quiet.
    let s_default = crate::config::Settings::default();
    let r = settings_run("prod", &s_default);
    assert!(
        !r.warnings.iter().any(|w| w.contains("argon2")),
        "default argon2 should be quiet, got: {:?}",
        r.warnings
    );
}

/// JWT access TTL > 1h in prod warns.
#[cfg(feature = "config")]
#[test]
fn settings_audit_prod_long_jwt_access_ttl_warns() {
    let mut s = crate::config::Settings::default();
    s.auth.jwt.access_ttl_secs = Some(86400); // 24h
    let r = settings_run("prod", &s);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("access_ttl_secs") && w.contains("86400")),
        "expected access TTL warning, got: {:?}",
        r.warnings
    );
}

/// Loopback bind in TOML triggers the warning even when
/// RUSTANGO_BIND env var isn't set.
#[cfg(feature = "config")]
#[test]
fn settings_audit_prod_loopback_bind_warns() {
    let mut s = crate::config::Settings::default();
    s.server.bind = Some("127.0.0.1:8080".into());
    let r = settings_run("prod", &s);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("[server] bind") && w.contains("loopback")),
        "expected loopback warning, got: {:?}",
        r.warnings
    );
}

/// `legacy_preset = true` is a deliberate choice — info, not
/// warning. `retention_days = None` is also info-level.
#[cfg(feature = "config")]
#[test]
fn settings_audit_legacy_preset_and_unset_retention_are_info() {
    let mut s = crate::config::Settings::default();
    s.routes.legacy_preset = Some(true);
    // PR #617 added an allowed_hosts warning on empty prod —
    // populate it here so this test stays focused on its
    // original concern (legacy_preset + retention_days info).
    s.security.allowed_hosts = vec!["example.com".into()];
    s.mail.backend = Some("smtp".into());
    s.server.request_timeout_secs = Some(30);
    let r = settings_run("prod", &s);
    assert!(
        r.info.iter().any(|i| i.contains("legacy_preset")),
        "expected legacy_preset info, got: {:?}",
        r.info
    );
    assert!(
        r.info.iter().any(|i| i.contains("retention_days")),
        "expected retention_days info, got: {:?}",
        r.info
    );
    // PR #617 added info-level (not warning) entries for
    // csrf_trusted_origins and secure_ssl_redirect — filter
    // them out of the warnings assertion. Real warnings would
    // still surface.
    assert!(
        r.warnings.is_empty(),
        "neither should be warnings, got: {:?}",
        r.warnings
    );
}

/// The Origin check is on by default (#1529); the audit must not say "disabled".
#[cfg(feature = "config")]
#[test]
fn settings_audit_empty_trusted_origins_does_not_claim_check_disabled() {
    let s = crate::config::Settings::default();
    let r = settings_run("prod", &s);
    let line = r
        .info
        .iter()
        .find(|i| i.contains("csrf_trusted_origins"))
        .expect("csrf_trusted_origins info line");
    assert!(!line.contains("disabled"), "{line}");
    assert!(line.contains("own Host"), "{line}");
}

// -------- v0.36 slice 7+10 — [admin] section audit --------

/// Bogus theme_mode trips the allowlist.
#[cfg(feature = "config")]
#[test]
fn settings_audit_admin_unknown_theme_mode_warns() {
    let mut s = crate::config::Settings::default();
    s.admin.theme_mode = Some("midnight".into());
    let r = settings_run("prod", &s);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("[admin]") && w.contains("theme_mode")),
        "expected theme_mode warning, got: {:?}",
        r.warnings
    );
}

/// url_prefix with a trailing slash trips the canonical-form
/// nudge (Builder strips it, but the config should be clean).
#[cfg(feature = "config")]
#[test]
fn settings_audit_admin_trailing_slash_url_prefix_warns() {
    let mut s = crate::config::Settings::default();
    s.admin.url_prefix = Some("/admin/".into());
    let r = settings_run("prod", &s);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("[admin]") && w.contains("url_prefix")),
        "expected url_prefix warning, got: {:?}",
        r.warnings
    );
}

/// Empty (root-mount) url_prefix stays quiet — legal, just unusual.
#[cfg(feature = "config")]
#[test]
fn settings_audit_admin_empty_url_prefix_is_quiet() {
    let mut s = crate::config::Settings::default();
    s.admin.url_prefix = Some("".into());
    let r = settings_run("prod", &s);
    assert!(
        !r.warnings
            .iter()
            .any(|w| w.contains("[admin]") && w.contains("url_prefix")),
        "empty url_prefix should be quiet, got: {:?}",
        r.warnings
    );
}

/// v0.36 slice 9 — pairing `tenancy` feature with a non-PG backend
/// is a misconfiguration: schema-mode tenancy needs Postgres.
/// This test only runs when the test profile has tenancy on AND
/// postgres is the active backend — every other feature combo
/// makes the assertion logically unreachable.
#[cfg(all(
    feature = "config",
    feature = "tenancy",
    feature = "postgres",
    any(feature = "sqlite", feature = "mysql"),
))]
#[test]
fn settings_audit_sqlite_backend_with_tenancy_warns_in_prod() {
    let mut s = crate::config::Settings::default();
    s.database.backend = Some("sqlite".into());
    let r = settings_run("prod", &s);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("[database]") && w.contains("tenancy")),
        "expected backend × tenancy warning, got: {:?}",
        r.warnings
    );
}
