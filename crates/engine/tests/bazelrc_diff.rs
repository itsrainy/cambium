//! A `.bazelrc` edit is only a global invalidation when the diff says it could
//! be one.
//!
//! Real git (the engine reads both sides of the rc chain with `git show`), a
//! synthetic graph, no Bazel. `.bazelrc` is a default invalidator, so the first
//! test is also the check that the screen runs *before* the generic rule.
//!
//! The screen's four scopes are each exercised against the engine: which
//! commands' lines the index query reads, which `--config` sections are in
//! effect for it, how an `import` is resolved at each end, and which files the
//! startup flags make rc files at all. Then the `--registry` route: a
//! lock-file question the externals stage answers.

use cambium_engine::{AffectedRequest, Config, Engine, RunOutcome, SynthQuerier};
use cambium_synth::{generate, GraphSpec, SynthGraph};
use cambium_testkit::{commit_all, git, init_repo, synthetic_engine, write};

/// A repository with `.bazelrc` at `base` and then at `head`, and the engine
/// over it. Returns the two shas.
fn repo_with(base_rc: &str, head_rc: &str) -> (tempfile::TempDir, Engine, String, String) {
    repo_with_files(
        &[(".bazelrc", base_rc)],
        &[(".bazelrc", head_rc)],
        Config::default(),
    )
}

/// A repository holding `base` files at the base commit and `head` files
/// (written over them) at the head commit, and an engine with `cfg` over it.
fn repo_with_files(
    base: &[(&str, &str)],
    head: &[(&str, &str)],
    cfg: Config,
) -> (tempfile::TempDir, Engine, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    init_repo(&root);
    for (p, t) in base {
        write(&root, p, t);
    }
    let base = commit_all(&root, "base");
    for (p, t) in head {
        write(&root, p, t);
    }
    let head = commit_all(&root, "head");
    let engine = synthetic_engine(&root, cfg);
    (dir, engine, base, head)
}

fn run(engine: &Engine, base: &str, head: &str, graph: &SynthGraph) -> RunOutcome {
    run_paths(engine, base, head, graph, &[".bazelrc"])
}

/// As [`run`], with the modified paths named.
fn run_paths(
    engine: &Engine,
    base: &str,
    head: &str,
    graph: &SynthGraph,
    paths: &[&str],
) -> RunOutcome {
    let index = engine
        .full_build_with(base, &SynthQuerier::new(graph))
        .unwrap();
    let req = AffectedRequest {
        materialize: true,
        no_full_build: true,
        ..AffectedRequest::new(base, head)
    };
    let changes: Vec<cambium_diff::Change> = paths
        .iter()
        .map(|p| cambium_diff::Change::new(cambium_diff::Status::Modified, *p))
        .collect();
    engine
        .compute(index.view(), &changes, &SynthQuerier::new(graph), &req)
        .unwrap()
}

fn graph() -> SynthGraph {
    generate(&GraphSpec::preset("small").unwrap())
}

/// The diagnostics a direct `compute` left on the engine's sink; `affected`
/// would have drained them into the report.
fn drained(engine: &Engine) -> Vec<cambium_engine::Diagnostic> {
    let mut out = Vec::new();
    engine.diagnostics().drain_into(&mut out);
    out
}

#[test]
fn a_jobs_change_invalidates_nothing() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "common --jobs=8\ncommon --remote_cache=grpc://old\n",
        "common --jobs=16\ncommon --remote_cache=grpc://new\n",
    );
    let out = run(&e, &base, &head, &g);
    let r = &out.report;
    assert!(!r.global, "global for {:?}", r.global_reason);
    assert!(r.dirty_packages.is_empty(), "{:?}", r.dirty_packages);
    assert!(r.affected.is_empty(), "{:?}", r.affected);
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);
}

#[test]
fn a_repo_env_change_is_global_with_the_flag_named() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "common --jobs=8\n",
        "common --jobs=8\ncommon --repo_env=CC=clang\n",
    );
    let out = run(&e, &base, &head, &g);
    let r = &out.report;
    assert!(r.global);
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common --repo_env changed, not on the loading-irrelevant allowlist")
    );
    assert_eq!(r.stats.bazelrc_safe_diffs, 0);
}

/// `query` inherits from no other command, so a `build` line, whatever it
/// carries, cannot change what the index query sees.
#[test]
fn a_line_the_query_does_not_read_invalidates_nothing() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "build --jobs=8\n",
        "build --jobs=8\nbuild --repo_env=CC=clang\ntest --enable_bzlmod\nbuild:ci --define=x=1\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(r.affected.is_empty());
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);
}

/// A flag under `command:config` reaches the query only when that config is
/// in effect for it: named by a line the query reads, or passed by
/// `[bazelrc].configs`.
#[test]
fn a_config_scoped_flag_follows_whether_the_config_is_in_effect() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "common --jobs=8\n",
        "common --jobs=8\ncommon:ci --repo_env=CC=clang\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "ci is never in effect: {:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);

    let (_d, e, base, head) = repo_with(
        "common --config=ci\ncommon:ci --jobs=8\n",
        "common --config=ci\ncommon:ci --jobs=8\ncommon:ci --repo_env=CC=clang\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common:ci --repo_env changed, not on the loading-irrelevant allowlist")
    );

    let mut cfg = Config::default();
    cfg.bazelrc.configs = vec!["ci".into()];
    let (_d, e, base, head) = repo_with_files(
        &[(".bazelrc", "common:ci --jobs=8\n")],
        &[(
            ".bazelrc",
            "common:ci --jobs=8\ncommon:ci --repo_env=CC=clang\n",
        )],
        cfg,
    );
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common:ci --repo_env changed, not on the loading-irrelevant allowlist"),
        "[bazelrc].configs puts ci in effect"
    );
}

/// `--config=x` is not a flag but an expansion: adding the line is screened
/// as the flags of the section it pulls in.
#[test]
fn a_config_reference_is_screened_as_its_section() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "common:ci --jobs=8\ncommon:ci --remote_cache=x\n",
        "common --config=ci\ncommon:ci --jobs=8\ncommon:ci --remote_cache=x\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "{:?}", r.global_reason);

    let (_d, e, base, head) = repo_with(
        "common:ci --repo_env=CC=clang\n",
        "common --config=ci\ncommon:ci --repo_env=CC=clang\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common:ci --repo_env changed, not on the loading-irrelevant allowlist")
    );

    let (_d, e, base, head) =
        repo_with("common --jobs=8\n", "common --jobs=8\ncommon --config=ci\n");
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common --config=ci changed, and no rc file defines `ci` for any command")
    );
}

/// `--enable_platform_specific_config` puts every host's section in effect,
/// since which host runs the query is not modelled.
#[test]
fn platform_sections_are_in_effect_once_the_flag_is() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "common --enable_platform_specific_config\ncommon:linux --jobs=8\n",
        "common --enable_platform_specific_config\ncommon:linux --jobs=8\ncommon:macos --repo_env=CC=clang\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common:macos --repo_env changed, not on the loading-irrelevant allowlist")
    );
    let (_d, e, base, head) = repo_with(
        "common:linux --jobs=8\n",
        "common:linux --jobs=8\ncommon:macos --repo_env=CC=clang\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "{:?}", r.global_reason);
}

#[test]
fn an_added_import_of_a_missing_file_is_global() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "build --jobs=8\n",
        "build --jobs=8\nimport %workspace%/ci.bazelrc\n",
    );
    let out = run(&e, &base, &head, &g);
    let r = &out.report;
    assert!(r.global);
    let reason = r.global_reason.clone().unwrap();
    assert!(reason.contains("import"), "{reason}");
}

/// `try-import` of a file the tree holds at neither revision is what Bazel
/// skips silently, and contributes no flag at either end. The apollo shape:
/// a developer override the repository ignores.
#[test]
fn a_try_import_of_a_missing_file_invalidates_nothing() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "common --jobs=8\n",
        "common --jobs=8\ntry-import %workspace%/.custom.bazelrc\n",
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);
}

/// An import line added for a file that exists at both ends is screened as
/// that file's flags, and the reason names the file.
#[test]
fn an_added_import_of_an_existing_file_is_screened_as_its_flags() {
    let g = graph();
    let harmless = (
        "tools/shared.rc",
        "common --jobs=4\ncommon --remote_cache=grpc://x\n",
    );
    let (_d, e, base, head) = repo_with_files(
        &[(".bazelrc", "common --jobs=8\n"), harmless],
        &[
            (
                ".bazelrc",
                "common --jobs=8\nimport %workspace%/tools/shared.rc\n",
            ),
            harmless,
        ],
        Config::default(),
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);

    let loud = (
        "tools/shared.rc",
        "common --jobs=4\ncommon --repo_env=CC=clang\n",
    );
    let (_d, e, base, head) = repo_with_files(
        &[(".bazelrc", "common --jobs=8\n"), loud],
        &[
            (
                ".bazelrc",
                "common --jobs=8\ntry-import %workspace%/tools/shared.rc\n",
            ),
            loud,
        ],
        Config::default(),
    );
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some("tools/shared.rc: common --repo_env changed, not on the loading-irrelevant allowlist")
    );
}

/// A file named `.bazelrc` that the root rc never imports belongs to some
/// nested workspace (envoy keeps one under `bazel/tests/external/`) and
/// Bazel never reads it for this one. Its edit is a source change, however
/// unsafe its flags look.
#[test]
fn a_nested_workspaces_bazelrc_is_not_this_workspaces() {
    let (_d, engine, base, head) = repo_with_files(
        &[
            (".bazelrc", "build --jobs=8\n"),
            ("tests/fixture/.bazelrc", "common --repo_env=x=1\n"),
        ],
        &[("tests/fixture/.bazelrc", "common --repo_env=x=2\n")],
        Config::default(),
    );
    let r = run_paths(&engine, &base, &head, &graph(), &["tests/fixture/.bazelrc"]).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_safe_diffs, 0, "it was not screened at all");
}

/// Under the startup option `--noworkspace_rc` Bazel reads neither the root
/// `.bazelrc` nor what it imports, so the file is inert for this workspace
/// (apollo runs this way on the benchmark: its own rc carries a query flag
/// the pinned Bazel refuses). An edit to it is an ordinary source change:
/// not screened, and not the `.bazelrc` invalidator either.
#[test]
fn a_root_rc_the_startup_flags_ignore_is_an_ordinary_change() {
    let g = graph();
    let mut cfg = Config::default();
    cfg.bazel.startup_flags = vec!["--noworkspace_rc".into()];
    let (_d, e, base, head) = repo_with_files(
        &[(".bazelrc", "common --repo_env=x=1\n")],
        &[(".bazelrc", "common --repo_env=x=2\n")],
        cfg,
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_safe_diffs, 0, "it was not screened at all");
    let diags = drained(&e);
    assert!(
        !diags.iter().any(|d| matches!(
            d.code,
            cambium_engine::Code::W004BazelrcGlobal | cambium_engine::Code::W013InvalidatorMatched
        )),
        "{diags:?}"
    );

    // The last spelling wins, as in Bazel's startup parser.
    let mut cfg = Config::default();
    cfg.bazel.startup_flags = vec!["--noworkspace_rc".into(), "--workspace_rc".into()];
    let (_d, e, base, head) = repo_with_files(
        &[(".bazelrc", "common --repo_env=x=1\n")],
        &[(".bazelrc", "common --repo_env=x=2\n")],
        cfg,
    );
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common --repo_env changed, not on the loading-irrelevant allowlist")
    );
}

/// And the converse: a file of any name that the root rc imports is an rc
/// file, and an unsafe flag in it is global, named by its own path.
#[test]
fn an_imported_rc_of_any_name_is_screened() {
    let (_d, engine, base, head) = repo_with_files(
        &[
            (".bazelrc", "import %workspace%/tools/shared.rc\n"),
            ("tools/shared.rc", "common --repo_env=x=1\n"),
        ],
        &[("tools/shared.rc", "common --repo_env=x=2\n")],
        Config::default(),
    );
    let r = run_paths(&engine, &base, &head, &graph(), &["tools/shared.rc"]).report;
    assert!(r.global, "an imported rc file's --repo_env is global");
    assert_eq!(
        r.global_reason.as_deref(),
        Some("tools/shared.rc: common --repo_env changed, not on the loading-irrelevant allowlist")
    );
}

// ---------------------------------------------------------------- --registry

const MODULE: &str = "module(name = \"demo\")\nbazel_dep(name = \"fmt\", version = \"1.0\")\n";

/// A `MODULE.bazel.lock` whose registry files for `fmt` were read from
/// `registry`, with the hashes given.
fn lock(registry: &str, module_hash: &str, source_hash: &str) -> String {
    format!(
        "{{\"lockFileVersion\": 24, \"registryFileHashes\": {{\
         \"{registry}/modules/fmt/1.0/MODULE.bazel\": \"{module_hash}\", \
         \"{registry}/modules/fmt/1.0/source.json\": \"{source_hash}\"}}, \
         \"selectedYankedVersions\": {{}}, \"moduleExtensions\": {{}}}}"
    )
}

const BCR: &str = "https://bcr.bazel.build";
const MIRROR: &str = "https://raw.githubusercontent.com/example/registry/abc123";

fn registry_rc(registry: &str) -> String {
    format!("common --jobs=8\ncommon --registry={registry}/\n")
}

/// The envoy shape: the registry pin moves and the lockfile is regenerated
/// against it. Every `registryFileHashes` key is rewritten, but the module
/// metadata behind them did not move, so nothing is affected.
#[test]
fn a_registry_change_whose_lock_moved_with_it_is_the_locks_question() {
    let g = graph();
    let (_d, e, base, head) = repo_with_files(
        &[
            (".bazelrc", &registry_rc(BCR)),
            ("MODULE.bazel", MODULE),
            ("MODULE.bazel.lock", &lock(BCR, "h1", "h2")),
        ],
        &[
            (".bazelrc", &registry_rc(MIRROR)),
            ("MODULE.bazel.lock", &lock(MIRROR, "h1", "h2")),
        ],
        Config::default(),
    );
    let r = run_paths(&e, &base, &head, &g, &[".bazelrc", "MODULE.bazel.lock"]).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(r.affected.is_empty(), "{:?}", r.affected);
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);
    assert_eq!(r.stats.repos_dirty_by_module, 0);

    // The same, with one module's metadata moved under the new registry:
    // the lock diff names it, as it would without the rc edit.
    let (_d, e, base, head) = repo_with_files(
        &[
            (".bazelrc", &registry_rc(BCR)),
            ("MODULE.bazel", MODULE),
            ("MODULE.bazel.lock", &lock(BCR, "h1", "h2")),
        ],
        &[
            (".bazelrc", &registry_rc(MIRROR)),
            ("MODULE.bazel.lock", &lock(MIRROR, "h1", "h3")),
        ],
        Config::default(),
    );
    let r = run_paths(&e, &base, &head, &g, &[".bazelrc", "MODULE.bazel.lock"]).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1, "fmt moved");
}

/// The registry moved and the lock did not: what the new registry serves is
/// not in the tree, so the externals stage escalates.
#[test]
fn a_registry_change_without_its_lock_is_global_through_the_externals_stage() {
    let g = graph();
    let (_d, e, base, head) = repo_with_files(
        &[
            (".bazelrc", &registry_rc(BCR)),
            ("MODULE.bazel", MODULE),
            ("MODULE.bazel.lock", &lock(BCR, "h1", "h2")),
        ],
        &[(".bazelrc", &registry_rc(MIRROR))],
        Config::default(),
    );
    let out = run(&e, &base, &head, &g);
    let r = &out.report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(
            "`.bazelrc` changed --registry and `MODULE.bazel.lock` did not move with it, so \
             which modules the new registries re-point cannot be read"
        )
    );
    assert!(
        drained(&e)
            .iter()
            .any(|d| d.code == cambium_engine::Code::W014ExternalsGlobal),
        "{:?}",
        drained(&e)
    );
}

/// With bzlmod off at base, `--registry` reads nothing: inert.
#[test]
fn a_registry_change_under_workspace_mode_is_inert() {
    let g = graph();
    let off = |registry: &str| format!("common --noenable_bzlmod\n{}", registry_rc(registry));
    let (_d, e, base, head) = repo_with_files(
        &[
            (".bazelrc", &off(BCR)),
            ("WORKSPACE", "workspace(name = \"demo\")\n"),
            ("MODULE.bazel", MODULE),
        ],
        &[(".bazelrc", &off(MIRROR))],
        Config::default(),
    );
    let r = run(&e, &base, &head, &g).report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);
}

/// With the externals stage off there is nobody to read the lock, and the
/// edit is the blunt W004 it always was.
#[test]
fn a_registry_change_with_the_externals_stage_off_is_the_old_global() {
    let g = graph();
    let mut cfg = Config::default();
    cfg.externals.enabled = false;
    let (_d, e, base, head) = repo_with_files(
        &[(".bazelrc", &registry_rc(BCR)), ("MODULE.bazel", MODULE)],
        &[(".bazelrc", &registry_rc(MIRROR))],
        cfg,
    );
    let r = run(&e, &base, &head, &g).report;
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: common --registry changed, not on the loading-irrelevant allowlist")
    );
    assert!(
        drained(&e)
            .iter()
            .any(|d| d.code == cambium_engine::Code::W004BazelrcGlobal),
        "{:?}",
        drained(&e)
    );
}

/// Real evidence, opt-in: screen every edit to one rc file of a subject
/// checkout and print the verdict, with a tally at the end. Ignored because
/// it needs `~/.cache/cambium/subjects/<entry>`; run with
/// `cargo test -p cambium-engine --test bazelrc_diff -- --ignored --nocapture`.
///
/// `CAMBIUM_BAZELRC_ENTRY` picks the subject, `CAMBIUM_BAZELRC_FILE` the rc
/// file (xla's globals are in `tensorflow.bazelrc`, envoy's in `.bazelrc`),
/// `CAMBIUM_BAZELRC_PIN` and `CAMBIUM_BAZELRC_WINDOW` the range. One file is
/// read, so the configs in effect are the ones it names for itself; a run
/// reads the whole import closure and can only have more in effect, which
/// screens more rather than less.
#[test]
#[ignore = "needs a subject checkout under ~/.cache/cambium/subjects"]
fn a_subjects_bazelrc_commits() {
    let entry = std::env::var("CAMBIUM_BAZELRC_ENTRY").unwrap_or_else(|_| "envoy".into());
    let root = dirs_home().join(".cache/cambium/subjects").join(&entry);
    if !root.exists() {
        eprintln!("no {entry} checkout at {}", root.display());
        return;
    }
    // envoy's pin, the default entry's; another entry is surveyed from its
    // own HEAD unless CAMBIUM_BAZELRC_PIN names one it has.
    let pin = &std::env::var("CAMBIUM_BAZELRC_PIN")
        .unwrap_or_else(|_| "7ba51266cc60cd560c37ea9cb3e2adf70676b6cf".into());
    let known = std::process::Command::new("git")
        .current_dir(&root)
        .args([
            "rev-parse",
            "--quiet",
            "--verify",
            &format!("{pin}^{{commit}}"),
        ])
        .output()
        .is_ok_and(|o| o.status.success());
    let pin = if known { pin.as_str() } else { "HEAD" };
    // The subject's own window is the last 50 first-parent commits; widen it
    // with CAMBIUM_BAZELRC_WINDOW to survey more history.
    let n = std::env::var("CAMBIUM_BAZELRC_WINDOW").unwrap_or_else(|_| "50".into());
    let n = format!("-{}", n.trim_start_matches('-'));
    let file = std::env::var("CAMBIUM_BAZELRC_FILE").unwrap_or_else(|_| ".bazelrc".into());
    let window = git(&root, &["log", "--first-parent", &n, "--format=%H", pin]);
    let window: Vec<&str> = window.lines().collect();
    let touching = git(
        &root,
        &[
            "log",
            "--first-parent",
            &n,
            "--format=%H",
            pin,
            "--",
            file.as_str(),
        ],
    );
    let (mut safe, mut registry) = (0usize, 0usize);
    let mut unknown: Vec<String> = Vec::new();
    for sha in touching.lines().filter(|s| window.contains(s)) {
        let subject = git(&root, &["log", "-1", "--format=%s", sha]);
        let parent = git(&root, &["rev-parse", &format!("{sha}^")]);
        let base = git(&root, &["show", &format!("{parent}:{file}")]);
        let head = git(&root, &["show", &format!("{sha}:{file}")]);
        let s = cambium_engine::bazelrc::screen(&base, &head, &[]);
        let verdict = match (&s.unknown, &s.registry, s.label_flags.first()) {
            (Some(b), ..) => {
                unknown.push(format!("{} {}", &sha[..12], b.why));
                format!("global: {}", b.why)
            }
            (None, Some(r), _) => {
                registry += 1;
                format!("externals stage: {} --registry", r.key.command)
            }
            (None, None, Some(f)) => {
                unknown.push(format!("{} {} (a build setting)", &sha[..12], f.label));
                format!("flag resolver: {}", f.label)
            }
            (None, None, None) => {
                safe += 1;
                format!("safe, {} changed triples", s.changed_flags)
            }
        };
        println!("{} {subject}\n    {verdict}", &sha[..12]);
    }
    // The line the local gate reads: how many of this file's edits the screen
    // clears, and the first cause of each one it does not.
    println!(
        "\n{entry} {file}: {safe} safe, {registry} to the externals stage, {} global",
        unknown.len()
    );
    for u in &unknown {
        println!("    {u}");
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME").map(std::path::PathBuf::from).unwrap()
}
