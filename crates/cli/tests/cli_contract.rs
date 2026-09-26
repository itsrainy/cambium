//! The command line as a contract.
//!
//! A service is going to shell out to this binary rather than link against the
//! crates, so its name, the subcommands it answers to, and the exit codes it
//! returns are as much a published interface as the JSON it prints — and
//! nothing else in the tree would notice if one of them changed.
//!
//! `crates/engine/tests/json_contract.rs` pins the document shapes and the
//! error-to-exit-code mapping; this pins the surface a caller types.

use std::process::Command;

use cambium_testkit::published_fixture;

/// The binary is `cambi`. The crate is `cambium-cli` and the project is
/// cambium, which is exactly why this is worth pinning: renaming the crate or
/// re-describing the project must not rename the thing a caller execs.
///
/// `CARGO_BIN_EXE_cambi` does not exist unless the `[[bin]]` is named `cambi`,
/// so a rename fails this file at compile time rather than at a customer.
const CAMBI: &str = env!("CARGO_BIN_EXE_cambi");

fn run(args: &[&str]) -> (i32, String, String) {
    run_with_env(args, &[])
}

/// As [`run`], with environment variables set for the child alone.
fn run_with_env(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(CAMBI);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("the binary should be runnable");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_binary_is_named_cambi() {
    assert!(
        std::path::Path::new(CAMBI)
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == "cambi"),
        "cargo built {CAMBI}, which is not cambi"
    );

    // clap's own program name is separate from the file name, and it is what
    // appears in every usage string and error a caller reads.
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Usage: cambi "),
        "the usage line should name the binary a caller types:\n{stdout}"
    );
    assert!(
        !stdout.contains("Usage: cambium"),
        "the old name is still in the help output:\n{stdout}"
    );
}

#[test]
fn the_subcommands_are_the_ones_the_readme_documents() {
    let (_, stdout, _) = run(&["--help"]);
    // The verb tree docs/CLI.md settles on, and nothing else. `baseline` is
    // deliberately absent: it still works, and a deprecated spelling is not
    // advertised.
    for sub in [
        "init", "status", "doctor", "affected", "explain", "profile", "index", "inspect", "config",
    ] {
        assert!(
            stdout.contains(sub),
            "`{sub}` is missing from the top-level help:\n{stdout}"
        );
    }
    assert!(
        !stdout
            .lines()
            .any(|l| l.trim_start().starts_with("baseline")),
        "a deprecated spelling should not be advertised as a verb:\n{stdout}"
    );
    // The harness verbs left the binary for `cambium-bench` and were never in
    // the contract, so they are gone rather than deprecated: typing one is an
    // ordinary unknown-subcommand error from clap.
    for gone in ["bench", "synth"] {
        assert!(
            !stdout.lines().any(|l| l.trim_start().starts_with(gone)),
            "`{gone}` belongs to cambium-bench now:\n{stdout}"
        );
        let (code, out, err) = run(&[gone]);
        assert_ne!(code, 0, "`cambi {gone}` must not be an answer: {out}");
        assert!(
            err.contains("unrecognized subcommand") || err.contains("unexpected argument"),
            "`cambi {gone}` should be an ordinary clap error, not a deprecation:\n{err}"
        );
        assert!(
            !err.contains("CAMBI-W001"),
            "a verb that was never in the contract is not deprecated:\n{err}"
        );
    }
}

/// A caller that mistypes must not have an empty stdout read as "nothing is
/// affected", nor exit 2, which says "everything is affected" and makes a CI
/// script test `//...` on a typo. A command line the parser rejects is an
/// error, exit 1, with the parser's own message and its `--help` hint.
#[test]
fn a_bad_invocation_exits_nonzero_rather_than_printing_an_answer() {
    for argv in [
        &["affected", "--not-a-flag"][..],
        &["affected", "--max-depth"],
        &["--format", "nope", "status"],
        &["no-such-verb"],
        &["index"],
    ] {
        let (code, stdout, stderr) = run(argv);
        assert_eq!(
            code,
            1,
            "`cambi {}` is an error, exit 1:\n{stderr}",
            argv.join(" ")
        );
        assert!(stdout.is_empty(), "nothing should reach stdout: {stdout:?}");
        assert!(
            stderr.contains("error:") || stderr.contains("Usage: cambi"),
            "the parser says what went wrong:\n{stderr}"
        );
    }
    let (_, _, stderr) = run(&["affected", "--not-a-flag"]);
    assert!(stderr.contains("--not-a-flag"), "{stderr}");
    assert!(stderr.contains("--help"), "the hint stays:\n{stderr}");
    // Help and the version are answers, not errors.
    for argv in [&["--help"][..], &["affected", "--help"], &["--version"]] {
        let (code, stdout, _) = run(argv);
        assert_eq!(code, 0, "`cambi {}`", argv.join(" "));
        assert!(!stdout.is_empty());
    }
}

/// Builds a synthetic index in `dir` and returns its path.
///
/// The generator never goes near Bazel or git, so everything below runs in a
/// bare sandbox. It is built in process because the verb that builds one
/// moved out of `cambi` into `cambium-bench synth build-index`, and a `cambi`
/// test must not need another binary to have been built.
fn synth_index(dir: &std::path::Path) -> std::path::PathBuf {
    let out = dir.join("synth.cmbx");
    let spec = cambium_synth::GraphSpec::preset("tiny").expect("the tiny preset");
    let index = cambium_synth::generate(&spec).build_index("synthetic");
    cambium_index::format::write_index(&out, &index).expect("writing the index");
    out
}

#[test]
fn the_exit_codes_are_the_ones_the_readme_documents() {
    // 0 for an answer, 1 for a failure the caller caused: the two every
    // caller branches on, which must hold with nothing installed, so this
    // runs against a synthetic index and no workspace. The codes that need
    // one (3, 4 and 5) are pinned end to end by the fixture tests below.
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());

    let (code, _, _) = run(&["inspect", "stats", index.to_str().unwrap()]);
    assert_eq!(code, 0, "a question we can answer exits 0");

    let missing = dir.path().join("nope.cmbx");
    let (code, stdout, stderr) = run(&["inspect", "stats", missing.to_str().unwrap()]);
    assert_eq!(code, 1, "an unreadable index is a plain error");
    assert!(stdout.is_empty(), "nothing should reach stdout: {stdout:?}");
    assert!(!stderr.is_empty(), "the error should say what went wrong");

    let (code, _, _) = run(&[
        "inspect",
        "target",
        index.to_str().unwrap(),
        "//nowhere:nothing",
    ]);
    assert_eq!(code, 1, "an unknown label is a plain error");
}

/// The hub labels `inspect stats` reports, which is the only way to learn a
/// label out of an index without already knowing one.
fn hub_labels(index: &std::path::Path) -> Vec<String> {
    let (code, stdout, stderr) = run(&[
        "inspect",
        "stats",
        index.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(code, 0, "inspect stats failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("stats prints json");
    v["in_degree"]["top"]
        .as_array()
        .expect("stats carries the hub list")
        .iter()
        .filter_map(|t| t["label"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn json_documents_carry_the_current_schema_version() {
    // The version is shared by every document, so one reader that pins it is
    // enough to catch a bump made for one document and forgotten in another.
    const SCHEMA: u64 = 18;
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let hub = hub_labels(&index).first().expect("a hub").clone();
    let package = hub
        .trim_start_matches("//")
        .split_once(':')
        .expect("a hub label names a package")
        .0
        .to_string();

    for args in [
        vec![
            "inspect",
            "stats",
            index.to_str().unwrap(),
            "--format",
            "json",
        ],
        vec![
            "inspect",
            "targets",
            index.to_str().unwrap(),
            &package,
            "--format",
            "json",
        ],
        vec![
            "inspect",
            "rdeps",
            index.to_str().unwrap(),
            &hub,
            "--format",
            "json",
        ],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?} failed: {stderr}");
        let v: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("{args:?} did not print JSON: {e}\n{stdout}"));
        assert_eq!(
            v.get("schema").and_then(|s| s.as_u64()),
            Some(SCHEMA),
            "{args:?} does not carry schema {SCHEMA}:\n{stdout}"
        );
    }
}

#[test]
fn inspect_path_exits_nonzero_when_there_is_no_path() {
    // A shell branches on the exit code rather than parsing the output, so
    // "no path" must not look like "here is your path".
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let labels = hub_labels(&index);
    assert!(labels.len() >= 2, "the hub list should hold a few labels");

    // The graph is a DAG, so of any two distinct targets at most one direction
    // can have a dependency path; the other must report there is none.
    let (a, b) = (&labels[0], &labels[1]);
    let both = [
        run(&["inspect", "path", index.to_str().unwrap(), a, b]),
        run(&["inspect", "path", index.to_str().unwrap(), b, a]),
    ];
    let (code, stdout, stderr) = both
        .iter()
        .find(|(c, _, _)| *c != 0)
        .unwrap_or_else(|| panic!("a DAG cannot have a path both ways between {a} and {b}"));
    assert_eq!(*code, 1, "no path is a plain error");
    assert!(stdout.is_empty(), "nothing should reach stdout: {stdout:?}");
    assert!(
        stderr.contains("no dependency path"),
        "the caller should be told why:\n{stderr}"
    );
}

/// `--format` is the flag the surface documents; `--output` is what every
/// existing script types, and it has to keep working for a release. So does
/// `text` for the value `human`, which is what `text` always meant.
#[test]
fn the_old_output_spelling_still_works() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let file = index.to_str().unwrap();

    let (code, by_format, _) = run(&["inspect", "stats", file, "--format", "json"]);
    assert_eq!(code, 0, "--format json should be accepted");
    let (code, by_output, _) = run(&["inspect", "stats", file, "--output", "json"]);
    assert_eq!(code, 0, "--output stays as an alias of --format");
    assert_eq!(by_format, by_output, "the alias must not change the answer");

    let (code, human, _) = run(&["inspect", "stats", file, "--format", "human"]);
    assert_eq!(code, 0);
    let (code, text, _) = run(&["inspect", "stats", file, "--format", "text"]);
    assert_eq!(code, 0, "`text` stays as an alias of `human`");
    assert_eq!(human, text);
    // And the default is still what it was, which is `human`.
    let (_, default, _) = run(&["inspect", "stats", file]);
    assert_eq!(default, human, "human is the default, TTY or not");
}

/// There is no Bazel expression for a byte count, and a silent fallback to
/// text would leave a pipeline building the wrong thing.
#[test]
fn asking_a_data_query_for_the_query_format_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let (code, stdout, stderr) = run(&[
        "inspect",
        "stats",
        index.to_str().unwrap(),
        "--format",
        "query",
    ]);
    assert_eq!(code, 1, "a usage error is exit 1");
    assert!(stdout.is_empty(), "nothing should reach stdout: {stdout:?}");
    assert!(
        stderr.contains("error[CAMBI-E"),
        "the caller needs a code to branch on:\n{stderr}"
    );
}

#[test]
fn the_null_format_answers_with_the_exit_code_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let (code, stdout, _) = run(&[
        "inspect",
        "stats",
        index.to_str().unwrap(),
        "--format",
        "null",
    ]);
    assert_eq!(code, 0, "the query still runs and still answers");
    assert!(
        stdout.is_empty(),
        "--format null prints nothing: {stdout:?}"
    );

    // And the exit code is still the answer where it carries one.
    let missing = dir.path().join("nope.cmbx");
    let (code, stdout, _) = run(&[
        "inspect",
        "stats",
        missing.to_str().unwrap(),
        "--format",
        "null",
    ]);
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout:?}");
}

/// An error is a diagnostic like any other: a code a script can match, not a
/// prose line that changes with the wording.
#[test]
fn an_error_carries_a_code_on_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.cmbx");
    let (code, _, stderr) = run(&["inspect", "stats", missing.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("error[CAMBI-E"),
        "an unreadable index should be a coded diagnostic:\n{stderr}"
    );
}

#[test]
fn no_color_means_no_escape_codes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.cmbx");
    let (_, stdout, stderr) = run_with_env(
        &["inspect", "stats", missing.to_str().unwrap()],
        &[("NO_COLOR", "1")],
    );
    assert!(
        !stderr.contains('\x1b') && !stdout.contains('\x1b'),
        "NO_COLOR=1 must leave the output plain:\n{stderr}"
    );
    // --color always is an explicit request and outranks a bare NO_COLOR.
    let (_, _, coloured) = run_with_env(
        &[
            "inspect",
            "stats",
            missing.to_str().unwrap(),
            "--color",
            "always",
        ],
        &[("NO_COLOR", "1")],
    );
    assert!(
        coloured.contains('\x1b'),
        "--color always should colour anyway:\n{coloured}"
    );
}

/// Every spelling the rename moved keeps working for a release, and says so
/// once, on stderr, with the code `docs/DIAGNOSTICS.md` documents.
#[test]
fn the_old_spellings_still_work_and_name_the_new_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let file = index.to_str().unwrap();

    let (code, old, stderr) = run(&["index", "stats", file]);
    assert_eq!(code, 0, "the old query spelling still answers: {stderr}");
    assert!(
        stderr.contains("warning[CAMBI-W001]")
            && stderr.contains("cambi index stats is now cambi inspect stats"),
        "the deprecation should name the new spelling:\n{stderr}"
    );
    assert_eq!(
        stderr.lines().filter(|l| l.contains("CAMBI-W001")).count(),
        1,
        "one line per invocation:\n{stderr}"
    );
    let (_, new, _) = run(&["inspect", "stats", file]);
    assert_eq!(old, new, "the rename must not change the answer");

    // The selectors that became verbs, and the no-selector form that became
    // `index show`.
    let hub = hub_labels(&index).first().expect("a hub").clone();
    let (code, old, stderr) = run(&["index", "inspect", file, "--label", &hub]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("cambi index inspect --label is now cambi inspect target"));
    let (_, new, _) = run(&["inspect", "target", file, &hub]);
    assert_eq!(old, new);

    let (code, old, stderr) = run(&["index", "inspect", file]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("cambi index inspect is now cambi index show"));
    let (_, new, _) = run(&["index", "show", file]);
    assert_eq!(old, new);

    // `baseline` is `index`. `list` needs no workspace-side work beyond the
    // store, and a missing store is an error — the deprecation still fires,
    // which is the point.
    let (_, _, stderr) = run(&["baseline", "list"]);
    assert!(
        stderr.contains("cambi baseline list is now cambi index list"),
        "{stderr}"
    );

    // The old output flag and its old value.
    let (code, _, stderr) = run(&["inspect", "stats", file, "--output", "text"]);
    assert_eq!(code, 0);
    assert!(stderr.contains("--output is now --format"), "{stderr}");
    assert!(
        stderr.contains("--format text is now --format human"),
        "{stderr}"
    );
}

/// A deprecation is a warning like any other, so `--quiet` silences it and
/// stdout never carries it.
#[test]
fn quiet_suppresses_the_deprecation_warning() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let (code, stdout, stderr) = run(&["index", "stats", index.to_str().unwrap(), "--quiet"]);
    assert_eq!(code, 0);
    assert!(
        !stderr.contains("CAMBI-W001"),
        "--quiet means nothing on stderr on success:\n{stderr}"
    );
    assert!(!stdout.contains("CAMBI-W001"), "{stdout}");
}

/// The range is positional; `--base` / `--head` stay as aliases of its halves.
/// Both at once is a contradiction, and it is the command line's own error —
/// reported with a code, wherever it is typed, workspace or not.
#[test]
fn affected_rejects_a_range_and_the_old_flags_together() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = dir.path().to_str().unwrap();
    let (code, stdout, stderr) = run(&["-C", outside, "affected", "a..b", "--base", "c"]);
    assert_eq!(code, 1, "a usage error is exit 1");
    assert!(stdout.is_empty(), "nothing should reach stdout: {stdout:?}");
    assert!(
        stderr.contains("error[CAMBI-E013]") && stderr.contains("drop --base"),
        "the caller should be told which half to drop:\n{stderr}"
    );
}

/// Typing `cambi` and nothing else is the first thing a new user does, and the
/// question it asks is "where am I" — so it prints `status`, not a usage screen.
/// Outside a workspace too: "there is no workspace here" is the answer, and it
/// is not an error.
#[test]
fn a_bare_cambi_prints_status_even_outside_a_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = dir.path().to_str().unwrap();
    let (code, stdout, stderr) = run(&["-C", outside]);
    assert_eq!(code, 0, "a bare cambi must not fail: {stderr}");
    assert!(
        stdout.contains("no Bazel workspace found"),
        "the workspace line should say there is none:\n{stdout}"
    );
    assert!(
        !stdout.contains("Usage: cambi"),
        "a bare invocation must not answer with usage:\n{stdout}"
    );
    for section in ["Workspace", "Position", "Index", "Lineage", "Store"] {
        assert!(
            stdout.contains(section),
            "`{section}` is missing from status:\n{stdout}"
        );
    }
    // And the explicit spelling prints exactly the same thing.
    let (code, explicit, _) = run(&["-C", outside, "status"]);
    assert_eq!(code, 0);
    assert_eq!(explicit, stdout, "`cambi` is `cambi status`");
}

/// `status` is a new document, and a new document still carries the shared
/// schema version: a reader that pins it reads every document or none.
#[test]
fn status_json_carries_the_schema_and_a_field_per_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (code, stdout, stderr) = run(&[
        "-C",
        dir.path().to_str().unwrap(),
        "status",
        "--format",
        "json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("status prints one document");
    assert_eq!(v["schema"].as_u64(), Some(18));
    for field in ["workspace", "config", "store", "diagnostics"] {
        assert!(v.get(field).is_some(), "{field} is missing:\n{stdout}");
    }
    // The probes `status` deliberately does not run are absent, not null: it
    // runs no Bazel and writes nothing to the store.
    assert!(v.get("bazel").is_none(), "status must not run bazel");
    assert!(v.get("round_trip").is_none());
}

/// Nothing in `status` may shell out to Bazel: the verb has to answer on a
/// machine where Bazel is not installed, and instantly.
#[test]
fn status_does_not_need_a_bazel_binary() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();
    std::fs::write(dir.path().join(".bazelversion"), "7.4.1\n").unwrap();
    let (code, stdout, stderr) = run(&[
        "-C",
        dir.path().to_str().unwrap(),
        "status",
        "--bazel-binary",
        "/nonexistent/bazel",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("bzlmod"), "{stdout}");
    assert!(
        stdout.contains("7.4.1 (.bazelversion)"),
        "the pin is where the version comes from:\n{stdout}"
    );
}

/// `doctor` is what a bug report asks for, so its document is a list of graded
/// checks and its exit code says whether a run can work at all.
#[test]
fn doctor_json_is_a_checks_array_and_a_failure_exits_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = dir.path().to_str().unwrap();
    let (code, stdout, stderr) = run(&["-C", outside, "doctor", "--format", "json"]);
    assert_eq!(
        code, 1,
        "no workspace means a run cannot work here: {stderr}"
    );
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("doctor prints one document");
    assert_eq!(v["schema"].as_u64(), Some(18));
    let checks = v["checks"].as_array().expect("a checks array");
    assert!(!checks.is_empty());
    for c in checks {
        assert!(c["name"].is_string(), "every check is named: {c}");
        let level = c["level"].as_str().expect("every check is graded");
        assert!(
            ["ok", "warn", "fail"].contains(&level),
            "unexpected level {level}"
        );
        // Anything not ok has to carry a code and a way out of it.
        if level != "ok" {
            assert!(
                c["diagnostic"]["code"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("CAMBI-")),
                "a finding without a code is not actionable: {c}"
            );
        }
    }
    assert!(
        checks
            .iter()
            .any(|c| c["name"] == "workspace" && c["level"] == "fail"),
        "the missing workspace should be the failure:\n{stdout}"
    );
}

/// A store nobody configured is how a developer with no store works, and
/// `doctor` must not grade it as broken — it is the difference between "you
/// have not set this up" and "this is set up wrong".
#[test]
fn doctor_grades_an_unconfigured_store_ok() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();
    let (_, stdout, _) = run(&[
        "-C",
        dir.path().to_str().unwrap(),
        "doctor",
        "--format",
        "json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let store = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "store")
        .expect("a store check");
    assert_eq!(store["level"], "ok", "{store}");
    assert!(store.get("diagnostic").is_none());
}

/// A workspace with nothing in it, the way someone's first five minutes go.
fn bare_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();
    std::fs::write(dir.path().join(".bazelversion"), "7.4.1\n").unwrap();
    dir
}

/// What `init` writes has to be what the engine reads, and `config explain` has
/// to be able to say where every value came from — otherwise the file it wrote
/// is a file nothing can account for.
#[test]
fn init_writes_a_config_the_engine_loads_and_explain_attributes() {
    let dir = bare_workspace();
    let ws = dir.path().to_str().unwrap();
    let store = dir.path().join("store");
    let (code, stdout, stderr) = run(&[
        "-C",
        ws,
        "init",
        "--yes",
        "--store-dir",
        store.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "init failed: {stderr}");
    assert!(
        stdout.contains("bzlmod"),
        "it shows what it detected:\n{stdout}"
    );
    let last = stdout.trim_end().lines().last().unwrap_or_default();
    assert!(
        last.starts_with("next: cambi profile ") && last.contains("cambi index build --publish"),
        "the last line is the next command, the profile before the cold build:\n{stdout}"
    );
    let written = std::fs::read_to_string(dir.path().join("cambium.toml")).expect("a config file");
    assert!(written.contains("kind = \"dir\""), "{written}");

    // `config show` only prints what `Config::load` accepted, so it passing at
    // all is the engine accepting the file.
    let (code, shown, stderr) = run(&["-C", ws, "config", "show"]);
    assert_eq!(code, 0, "the engine should load what init wrote: {stderr}");
    assert!(shown.contains("config_hash = "), "{shown}");

    let (code, explained, stderr) = run(&["-C", ws, "config", "explain", "--format", "json"]);
    assert_eq!(code, 0, "{stderr}");
    let v: serde_json::Value = serde_json::from_str(&explained).expect("explain prints json");
    let values = v["values"].as_array().expect("a value per leaf key");
    let store_kind = values
        .iter()
        .find(|v| v["key"] == "baseline.store.kind")
        .expect("the store init wrote is explained");
    assert_eq!(store_kind["value"], "\"dir\"");
    assert!(
        store_kind["origin_label"]
            .as_str()
            .is_some_and(|s| s.ends_with("cambium.toml")),
        "the origin should be the file init wrote: {store_kind}"
    );
    // And a key nobody set is attributed to the defaults, not to the file.
    let untouched = values
        .iter()
        .find(|v| v["key"] == "baseline.retention_days")
        .expect("every leaf key is explained");
    assert_eq!(untouched["origin_label"], "default");
}

/// `cambium.toml` is checked in, and somebody wrote the rest of it on purpose.
#[test]
fn init_on_an_existing_file_preserves_what_it_was_not_asked_about() {
    let dir = bare_workspace();
    let ws = dir.path().to_str().unwrap();
    std::fs::write(
        dir.path().join("cambium.toml"),
        "# our notes\n[limits]\nmax_dirty_packages = 900\n",
    )
    .unwrap();
    let (code, _, stderr) = run(&["-C", ws, "init", "--yes", "--store-http", "https://x/y"]);
    assert_eq!(code, 0, "{stderr}");
    let written = std::fs::read_to_string(dir.path().join("cambium.toml")).unwrap();
    assert!(
        written.contains("# our notes"),
        "a comment was dropped:\n{written}"
    );
    assert!(
        written.contains("max_dirty_packages = 900"),
        "an unrelated key was dropped:\n{written}"
    );
    assert!(written.contains("kind = \"http\""), "{written}");
}

/// A script with no flags on a pipe cannot be prompted, and must be told which
/// flags exist rather than handed a store it did not choose.
#[test]
fn init_without_flags_on_a_pipe_is_a_usage_error_naming_the_flags() {
    let dir = bare_workspace();
    let (code, _, stderr) = run(&["-C", dir.path().to_str().unwrap(), "init"]);
    assert_eq!(code, 1, "a usage error is exit 1");
    assert!(stderr.contains("error[CAMBI-E013]"), "{stderr}");
    for flag in ["--store-dir", "--store-s3", "--store-none", "--yes"] {
        assert!(stderr.contains(flag), "{flag} is not offered:\n{stderr}");
    }
    assert!(
        !dir.path().join("cambium.toml").exists(),
        "a usage error must not write a file"
    );
}

/// `init` is the one verb a user runs before they trust cambium, so what it
/// does and does not touch is part of the contract: a config file, and nothing
/// else.
#[test]
fn init_writes_the_config_and_nothing_else() {
    let dir = bare_workspace();
    std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    let before: std::collections::BTreeSet<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let (code, stdout, stderr) = run(&[
        "-C",
        dir.path().to_str().unwrap(),
        "init",
        "--yes",
        "--store-none",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let after: std::collections::BTreeSet<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let new: Vec<_> = after.difference(&before).collect();
    assert_eq!(new.len(), 1, "init created more than cambium.toml: {new:?}");
    assert_eq!(new[0].to_str(), Some("cambium.toml"));
    assert!(
        stdout.trim_end().ends_with("next: cambi doctor"),
        "with no store the next step is doctor:\n{stdout}"
    );
    // A .gitignore that does not mention the local override is said, not fixed.
    assert!(stderr.contains("CAMBI-W020"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
        "target/\n",
        "init must not edit .gitignore"
    );
}

/// The Position line, end to end: a real git history, a real directory store,
/// and an index published for an ancestor.
///
/// This is the line `status` exists for, and it is worth pinning as text: a
/// caller reads it, and "3 commits back" is the difference between waiting and
/// not.
#[test]
fn status_names_the_nearest_index_and_how_far_back_it_is() {
    let at = 1;
    let f = published_fixture(at);
    let (ws, emitted_commits, lineage) = (&f.ws, &f.commits, &f.lineage);

    let (code, stdout, stderr) = run(&["-C", ws.to_str().unwrap(), "status"]);
    assert_eq!(code, 0, "{stderr}");
    let back = emitted_commits.len() - 1 - at;
    let head = &emitted_commits[emitted_commits.len() - 1][..7];
    let near = &emitted_commits[at][..7];
    let expected = format!(
        "HEAD {head} has no index; nearest is {near}, {back} commits back (lineage {}, splice)",
        &lineage[..6]
    );
    assert!(
        stdout.lines().any(|l| l.trim() == expected),
        "the Position line should read\n  {expected}\nand status printed\n{stdout}"
    );
    assert!(stdout.contains("reachable, 1 indexes"), "{stdout}");
    assert!(
        stdout.contains("packages") && stdout.contains("format     version"),
        "the Index section should describe the index a run would load:\n{stdout}"
    );
}

/// `explain` answers with a document, and there is no Bazel expression for
/// "why": asking for one is a usage error with a code, not a silent fallback.
#[test]
fn explain_has_no_query_form() {
    let (code, stdout, stderr) = run(&["explain", "//a:b", "--format", "query"]);
    assert_eq!(code, 1, "a usage error is exit 1: {stderr}");
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(
        stderr.contains("CAMBI-E013"),
        "the usage code should name itself:\n{stderr}"
    );
}

/// `--explain` on `affected` keeps working for one release and says what to
/// type instead. The warning is the contract here: it is how a script learns
/// to move before the flag goes.
#[test]
fn the_explain_flag_on_affected_still_works_and_says_what_replaced_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_, _, stderr) = run(&[
        "-C",
        dir.path().to_str().unwrap(),
        "affected",
        "--explain",
        "a..b",
    ]);
    assert!(
        stderr.contains("CAMBI-W001")
            && stderr.contains("--explain on affected is now cambi explain"),
        "the deprecated flag should name the verb:\n{stderr}"
    );
    // And it is not advertised: a flag kept for a release is not a flag to
    // start using.
    let (_, help, _) = run(&["affected", "--help"]);
    assert!(
        !help.contains("--explain"),
        "a deprecated flag should be hidden from help:\n{help}"
    );
}

/// `--dry-run` runs no Bazel, but it is still a run's plan: outside a workspace
/// the answer is the workspace error with its own code, not a usage error. A
/// caller branching on exit 1 versus 3 must not see the two swap places.
#[test]
fn a_dry_run_outside_a_workspace_is_the_workspace_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (code, stdout, stderr) = run(&[
        "-C",
        dir.path().to_str().unwrap(),
        "affected",
        "--dry-run",
        "a..b",
    ]);
    assert!(
        stderr.contains("CAMBI-E004"),
        "the workspace probe owns this failure:\n{stderr}"
    );
    assert!(
        !stderr.contains("CAMBI-E013"),
        "a missing workspace is not a usage error:\n{stderr}"
    );
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout:?}");
}

/// The plan, end to end, with no Bazel anywhere: a real git history, a real
/// directory store, an index published for an ancestor, and a diff classified
/// against it.
#[test]
fn a_dry_run_names_the_index_it_would_load_and_what_it_would_do() {
    let f = published_fixture(1);
    let range = format!("{}..{}", f.commits[1], f.commits[f.commits.len() - 1]);
    let (code, stdout, stderr) = run(&[
        "-C",
        f.ws.to_str().unwrap(),
        "--cache-dir",
        f.cache.to_str().unwrap(),
        "affected",
        "--dry-run",
        &range,
    ]);
    assert_eq!(code, 0, "a plan is not a verdict: {stderr}{stdout}");
    let line = |prefix: &str| -> String {
        stdout
            .lines()
            .find(|l| l.trim_start().starts_with(prefix))
            .unwrap_or_else(|| panic!("no `{prefix}` line in\n{stdout}"))
            .trim()
            .to_string()
    };
    assert!(
        line("index").contains(&f.commits[1][..7]) && line("index").contains(&f.lineage[..6]),
        "the index line names the revision and the lineage a run would splice onto:\n{stdout}"
    );
    assert!(
        line("diff").contains("changed file"),
        "the diff line counts what moved:\n{stdout}"
    );
    assert_eq!(line("mode"), "mode   splice", "{stdout}");

    // The same plan as one document, for a CI job that branches on it.
    let (code, stdout, stderr) = run(&[
        "-C",
        f.ws.to_str().unwrap(),
        "--cache-dir",
        f.cache.to_str().unwrap(),
        "--format",
        "json",
        "affected",
        "--dry-run",
        &range,
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: serde_json::Value = serde_json::from_str(&stdout).expect("one document");
    assert_eq!(doc["schema"], 18);
    assert_eq!(doc["plan"]["mode"], "splice");
    assert_eq!(doc["plan"]["index"]["revision"], f.commits[1]);
    assert!(
        doc["plan"]["dirty_packages"].is_u64(),
        "the diff was classified without Bazel:\n{stdout}"
    );
}

/// `--format labels` is the CI format: one label per line and nothing else,
/// so `bazel test --target_pattern_file` reads stdout as it is. An empty
/// answer is an empty stdout, not a header, and a global one is `//...` with
/// exit 2.
#[test]
fn labels_is_the_answer_one_per_line_and_nothing_else() {
    let f = published_fixture(1);
    let ws = f.ws.to_str().unwrap();
    let cache = f.cache.to_str().unwrap();
    let base = &f.commits[1];

    // Nothing changed: no lines at all, exit 0.
    let (code, stdout, stderr) = run(&[
        "-C",
        ws,
        "--cache-dir",
        cache,
        "--format",
        "labels",
        "affected",
        &format!("{base}..{base}"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "", "an empty answer is an empty stdout");

    // A commit on the indexed base that adds a .bazelrc: the whole graph.
    cambium_testkit::git(&f.ws, &["checkout", "-q", "--detach", base]);
    std::fs::write(f.ws.join(".bazelrc"), "build --jobs=4\n").unwrap();
    cambium_testkit::git(&f.ws, &["add", ".bazelrc"]);
    cambium_testkit::git(&f.ws, &["commit", "-q", "-m", "add a bazelrc"]);
    let (code, stdout, stderr) = run(&[
        "-C",
        ws,
        "--cache-dir",
        cache,
        "affected",
        base,
        "--tests-only",
        "--format",
        "labels",
    ]);
    assert_eq!(code, 2, "global is exit 2: {stderr}");
    assert_eq!(stdout, "//...\n", "a global answer is the one pattern");
}

/// `labels` has no meaning off `affected`, like `query`: a usage error with a
/// code and a help line, not a silent fallback to another shape.
#[test]
fn asking_a_data_query_for_labels_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    let (code, stdout, stderr) = run(&[
        "inspect",
        "stats",
        index.to_str().unwrap(),
        "--format",
        "labels",
    ]);
    assert_eq!(code, 1, "a usage error is exit 1");
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(stderr.contains("CAMBI-E013"), "{stderr}");
    assert!(
        stderr.contains("= help:"),
        "every E013 has a help line:\n{stderr}"
    );
}

/// A clone the way `actions/checkout` leaves one: a detached HEAD, no local
/// `main`, and `origin/main` present or not.
fn ci_clone(f: &cambium_testkit::PublishedFixture) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let clone = dir.path().join("clone");
    let url = format!("file://{}", f.ws.display());
    cambium_testkit::git(dir.path(), &["clone", "-q", &url, clone.to_str().unwrap()]);
    cambium_testkit::git(&clone, &["checkout", "-q", "--detach"]);
    cambium_testkit::git(&clone, &["branch", "-q", "-D", "main"]);
    std::fs::copy(f.ws.join("cambium.toml"), clone.join("cambium.toml")).unwrap();
    dir
}

/// With no range, `[git].default_branch` is `main`, and a CI checkout has
/// only `origin/main`: that stands in, and the narration says so.
#[test]
fn the_default_branch_falls_back_to_its_remote_tracking_branch() {
    let f = published_fixture(1);
    let dir = ci_clone(&f);
    let clone = dir.path().join("clone");
    let (code, stdout, stderr) = run(&[
        "-C",
        clone.to_str().unwrap(),
        "--cache-dir",
        f.cache.to_str().unwrap(),
        "affected",
        "--dry-run",
    ]);
    assert_eq!(code, 0, "{stderr}{stdout}");
    assert!(
        stderr.contains("no local branch main; comparing from origin/main"),
        "the fallback is said, not silent:\n{stderr}"
    );

    // Neither `main` nor `origin/main`: a usage error whose help names the
    // fetch that makes the ref.
    cambium_testkit::git(&clone, &["update-ref", "-d", "refs/remotes/origin/main"]);
    let (code, _, stderr) = run(&["-C", clone.to_str().unwrap(), "affected", "--dry-run"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("CAMBI-E013"), "{stderr}");
    assert!(
        stderr.contains("git fetch origin main:refs/remotes/origin/main --depth=N"),
        "{stderr}"
    );
}

/// A revision the clone does not have is E003 naming the spelling typed and
/// the fetch that brings it in, never git's usage text or the `--base` and
/// `--head` flags nobody passed.
#[test]
fn a_missing_revision_names_the_spelling_and_the_fetch() {
    let f = published_fixture(1);
    let dir = ci_clone(&f);
    let clone = dir.path().join("clone");
    let c = clone.to_str().unwrap();

    let (code, stdout, stderr) = run(&["-C", c, "affected", "origin/nope"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(
        stderr.contains("error[CAMBI-E003]: `origin/nope` does not name a commit in this clone"),
        "{stderr}"
    );
    assert!(
        stderr.contains("git fetch origin nope:refs/remotes/origin/nope --depth=N"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("--base") && !stderr.contains("Use '--'"),
        "{stderr}"
    );

    // `main` typed where only `origin/main` exists: say which one is here.
    let (code, _, stderr) = run(&["-C", c, "affected", "main..HEAD"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("`origin/main`"), "{stderr}");
}

/// `cambi ... | head`: the reader leaves, and the run ends quietly with exit
/// 0 rather than a panic.
#[test]
fn a_reader_that_closes_stdout_early_is_not_a_failure() {
    use std::process::Stdio;
    let dir = tempfile::tempdir().expect("tempdir");
    let index = synth_index(dir.path());
    // The read end is closed before the child starts, so its first write
    // meets a pipe with no reader, every time.
    let (reader, writer) = std::io::pipe().expect("a pipe");
    drop(reader);
    let out = Command::new(CAMBI)
        .args([
            "inspect",
            "stats",
            index.to_str().unwrap(),
            "--format",
            "json",
        ])
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .expect("the binary should be runnable");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// `--log` lines go to the same stderr the narration does and colour by the
/// same rule: none in a pipe.
#[test]
fn log_lines_in_a_pipe_carry_no_escape_codes() {
    let f = published_fixture(1);
    let base = &f.commits[1];
    let (code, _, stderr) = run(&[
        "-C",
        f.ws.to_str().unwrap(),
        "--cache-dir",
        f.cache.to_str().unwrap(),
        "--log",
        "info",
        "affected",
        &format!("{base}..{base}"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("INFO"),
        "the test needs a log line to look at:\n{stderr}"
    );
    assert!(!stderr.contains('\x1b'), "{stderr:?}");
}

/// Rewrites the fixture's `cambium.toml` to point `[baseline].store` at a
/// directory given as `path`, written as-is.
fn point_store_at(ws: &std::path::Path, path: &str) {
    let mut cfg = cambium_engine::Config::load(ws).unwrap();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir { path: path.into() };
    std::fs::write(ws.join("cambium.toml"), cfg.to_toml()).unwrap();
}

/// A relative directory store in the checked-in `cambium.toml` is read from
/// the workspace root, so `-C` from anywhere else reaches the same store.
#[test]
fn a_relative_directory_store_is_read_from_the_workspace_root() {
    let f = published_fixture(1);
    // The fixture keeps the store beside the workspace, at `../store`.
    point_store_at(&f.ws, "../store");
    let elsewhere = tempfile::tempdir().unwrap();
    let out = Command::new(CAMBI)
        .current_dir(elsewhere.path())
        .args(["-C", f.ws.to_str().unwrap(), "status"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("reachable, 1 indexes"),
        "the store should be found from another working directory:\n{stdout}"
    );
}

/// The directory store's one rule, end to end: a root that does not exist
/// is an empty store (exit 4 under `--no-full-build`, and the warning says
/// why), and a root that cannot be read is the store failing (exit 5).
#[test]
fn a_missing_store_directory_is_a_miss_and_an_unreadable_one_is_exit_5() {
    let f = published_fixture(1);
    let range = format!("{}..{}", f.commits[2], f.commits[3]);
    let args = |ws: &str| -> Vec<String> {
        [
            "-C",
            ws,
            "--cache-dir",
            f.cache.to_str().unwrap(),
            "affected",
            "--no-full-build",
            &range,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    };
    let ws = f.ws.to_str().unwrap();

    point_store_at(&f.ws, "never-published");
    let a = args(ws);
    let (code, _, stderr) = run(&a.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 4, "{stderr}");
    assert_eq!(
        stderr.matches("CAMBI-W007").count(),
        1,
        "a missing directory is said once, before the no-index error:\n{stderr}"
    );
    assert!(stderr.contains("CAMBI-E006"), "{stderr}");
    assert!(
        !f.ws.join("never-published").exists(),
        "a read creates nothing"
    );

    // Under `jsonl` the warning is a `diagnostic` event on stdout, as it
    // would be on success; the error stays on stderr.
    let mut j = a.clone();
    j.splice(2..2, ["--format".to_string(), "jsonl".to_string()]);
    let (code, stdout, stderr) = run(&j.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 4, "{stderr}");
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"] == "diagnostic" && e["code"] == "CAMBI-W007")
            .count(),
        1,
        "{stdout}"
    );
    assert!(!stderr.contains("CAMBI-W007"), "{stderr}");
    assert!(stderr.contains("CAMBI-E006"), "{stderr}");

    // A file where the directory should be: `/dev/null/nope`'s shape.
    point_store_at(&f.ws, "cambium.toml/nope");
    let (code, _, stderr) = run(&a.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 5, "{stderr}");
    assert!(stderr.contains("CAMBI-E011"), "{stderr}");
    assert!(
        !stderr.contains("CAMBI-W007"),
        "an error is not also a miss:\n{stderr}"
    );
}

/// `index gc` and `index list` report a store problem under the code every
/// other verb uses for it: `CAMBI-E008` when no store is configured,
/// `CAMBI-E012` when the backend has no such operation, each with its
/// `help:` line. Never the generic `CAMBI-E001`.
#[test]
fn index_gc_and_list_report_store_problems_under_their_own_codes() {
    let ws = bare_workspace();
    let w = ws.path().to_str().unwrap();
    for verb in ["gc", "list"] {
        let (code, stdout, stderr) = run(&["-C", w, "index", verb]);
        assert_eq!(code, 1, "{stderr}");
        assert!(stdout.is_empty(), "{stdout:?}");
        assert!(
            stderr.contains("error[CAMBI-E008]: no index store is configured"),
            "`index {verb}` with no store:\n{stderr}"
        );
        assert!(
            stderr.contains("= help: set [baseline].store in cambium.toml"),
            "{stderr}"
        );
        assert!(!stderr.contains("CAMBI-E001"), "{stderr}");
        assert!(!stderr.contains("baseline store"), "{stderr}");
    }

    // A command store with no `list_cmd` can neither enumerate nor collect.
    std::fs::write(
        ws.path().join("cambium.toml"),
        "[baseline]\nstore = { kind = \"command\", fetch_cmd = [\"false\"], push_cmd = [\"false\"] }\n",
    )
    .unwrap();
    let (code, _, stderr) = run(&["-C", w, "index", "gc"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[CAMBI-E012]"), "{stderr}");
    assert!(stderr.contains("lifecycle rule"), "{stderr}");
    let (code, _, stderr) = run(&["-C", w, "index", "list"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[CAMBI-E012]"), "{stderr}");
    assert!(stderr.contains("`list_cmd`"), "{stderr}");

    // Plain HTTP cannot list either; nothing is contacted to find that out.
    std::fs::write(
        ws.path().join("cambium.toml"),
        "[baseline]\nstore = { kind = \"http\", url = \"http://127.0.0.1:9/cambium\" }\n",
    )
    .unwrap();
    for verb in ["gc", "list"] {
        let (code, _, stderr) = run(&["-C", w, "index", verb]);
        assert_eq!(code, 1, "{stderr}");
        assert!(stderr.contains("error[CAMBI-E012]"), "{stderr}");
    }
}

/// What `CAMBI-W019`'s help promises, end to end: after a `cambium.toml`
/// edit, `cambi index gc` removes the lineage the old configuration built,
/// at once rather than once it is `retention_days` old, and
/// `--keep-incompatible` keeps it.
#[test]
fn gc_removes_a_lineage_the_configuration_moved_away_from() {
    let f = published_fixture(1);
    let ws = f.ws.to_str().unwrap();
    let cache = f.cache.to_str().unwrap();
    let mut cfg = cambium_engine::Config::load(&f.ws).unwrap();
    cfg.externals.index_max_packages += 1;
    std::fs::write(f.ws.join("cambium.toml"), cfg.to_toml()).unwrap();

    let (code, stdout, stderr) = run(&["-C", ws, "--cache-dir", cache, "status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("CAMBI-W019"), "{stdout}");
    assert!(
        stdout.contains("`cambi index gc` removes it now"),
        "{stdout}"
    );

    let listed = || -> String {
        let (code, stdout, stderr) = run(&["-C", ws, "index", "list"]);
        assert_eq!(code, 0, "{stderr}");
        stdout
    };
    assert!(listed().contains(&f.lineage));

    let (code, stdout, stderr) = run(&[
        "-C",
        ws,
        "--cache-dir",
        cache,
        "index",
        "gc",
        "--keep-incompatible",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), "removed 0 index files");
    assert!(
        listed().contains(&f.lineage),
        "--keep-incompatible keeps it"
    );

    let (code, stdout, stderr) = run(&["-C", ws, "--cache-dir", cache, "index", "gc"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), "removed 2 index files", "{stderr}");
    assert!(
        stderr.contains(&format!("lineage {}", f.lineage)),
        "the narration names the lineage:\n{stderr}"
    );
    assert!(!listed().contains(&f.lineage));
}

/// Diagnostics colour by the narration's rule: `CLICOLOR_FORCE` colours a
/// pipe, and `TERM=dumb` and `CLICOLOR=0` keep even that plain.
#[test]
fn diagnostics_colour_by_the_same_rule_as_the_narration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.cmbx");
    let stderr_under = |env: &[(&str, &str)]| {
        let mut cmd = Command::new(CAMBI);
        cmd.args(["inspect", "stats", missing.to_str().unwrap()]);
        for k in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "TERM", "CI"] {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("the binary should be runnable");
        assert_eq!(out.status.code(), Some(1));
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let forced = stderr_under(&[("CLICOLOR_FORCE", "1")]);
    assert!(
        forced.contains("\x1b[1;31merror[CAMBI-E"),
        "CLICOLOR_FORCE colours the diagnostic down a pipe:\n{forced:?}"
    );
    for off in [
        &[("CLICOLOR_FORCE", "1"), ("TERM", "dumb")][..],
        &[("CLICOLOR_FORCE", "1"), ("CLICOLOR", "0")],
        &[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1")],
        &[],
    ] {
        let plain = stderr_under(off);
        assert!(!plain.contains('\x1b'), "{off:?}:\n{plain:?}");
    }
}

/// The narration names a ref the way it was typed. Cutting every revision
/// at ten characters, as a sha is cut, printed `origin/main` as
/// `origin/mai`.
#[test]
fn the_narration_header_prints_a_ref_whole_and_a_sha_short() {
    let f = published_fixture(1);
    let base = &f.commits[1];
    cambium_testkit::git(&f.ws, &["branch", "-q", "release/2026-09-25", base]);
    let (code, _, stderr) = run(&[
        "-C",
        f.ws.to_str().unwrap(),
        "--cache-dir",
        f.cache.to_str().unwrap(),
        "affected",
        &format!("release/2026-09-25..{base}"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    let header = stderr
        .lines()
        .find(|l| l.starts_with("cambium affected"))
        .expect("a header line");
    assert!(
        header.contains(&format!("release/2026-09-25..{}", &base[..10])),
        "{header}"
    );
}

/// The `index` verbs narrate in the grammar every other verb does: a header,
/// phase lines, the keys under the `publish` phase. No bare `cambium: ...`
/// lines, and nothing at all under `--quiet`.
#[test]
fn index_publish_narrates_through_the_renderer() {
    let f = published_fixture(1);
    let ws = f.ws.to_str().unwrap();
    let file = f.cache.join("published.cmbx");
    let (code, stdout, stderr) = run(&["-C", ws, "index", "publish", file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.is_empty(), "{stdout:?}");
    let first = stderr.lines().next().unwrap_or_default();
    assert!(first.starts_with("cambium index publish"), "{stderr}");
    assert!(first.contains(&f.lineage), "{stderr}");
    assert!(stderr.contains("  publish ...."), "{stderr}");
    assert!(
        stderr.contains(&format!("    indexes/{}/", f.lineage)),
        "{stderr}"
    );
    assert!(!stderr.contains("cambium: "), "{stderr}");

    let (code, _, stderr) = run(&["-C", ws, "-q", "index", "publish", file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.is_empty(), "{stderr:?}");

    // An index that would go nowhere is said before anything is built.
    let bare = bare_workspace();
    let (code, _, stderr) = run(&["-C", bare.path().to_str().unwrap(), "index", "build"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[CAMBI-E013]"), "{stderr}");
    assert!(stderr.contains("--out <file>"), "{stderr}");
}

/// Under `--no-full-build` with no index in reach, the narration says what
/// the search found and stops there: it does not announce a cold build the
/// run is about to refuse. The error says index, not baseline.
#[test]
fn no_index_under_no_full_build_narrates_no_cold_build() {
    let f = published_fixture(1);
    point_store_at(&f.ws, "never-published");
    let range = format!("{}..{}", f.commits[2], f.commits[3]);
    let (code, _, stderr) = run(&[
        "-C",
        f.ws.to_str().unwrap(),
        "--cache-dir",
        f.cache.to_str().unwrap(),
        "affected",
        "--no-full-build",
        &range,
    ]);
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("none within"), "{stderr}");
    assert!(!stderr.contains("building cold"), "{stderr}");
    assert!(!stderr.contains("cold index"), "{stderr}");
    assert!(
        stderr.contains("error[CAMBI-E006]: no index is available for"),
        "{stderr}"
    );
    assert!(!stderr.contains("baseline index"), "{stderr}");
}

/// One refused index is one `CAMBI-W003`, and one log line, though the
/// lookup reaches it by its tree and again through its `commits/` pointer.
#[test]
fn a_refused_index_is_one_w003() {
    let f = published_fixture(1);
    let mut cfg = cambium_engine::Config::load(&f.ws).unwrap();
    cfg.externals.index_max_packages += 1;
    std::fs::write(f.ws.join("cambium.toml"), cfg.to_toml()).unwrap();
    let base = &f.commits[1];
    let (code, _, stderr) = run(&[
        "-C",
        f.ws.to_str().unwrap(),
        "--cache-dir",
        f.cache.to_str().unwrap(),
        "affected",
        "--no-full-build",
        &format!("{base}..{base}"),
    ]);
    assert_eq!(code, 4, "{stderr}");
    assert_eq!(stderr.matches("warning[CAMBI-W003]").count(), 1, "{stderr}");
    assert_eq!(
        stderr
            .matches("skipping an index built with a different cambium.toml")
            .count(),
        1,
        "{stderr}"
    );
}

/// A depth-1 clone of one branch, the way a CI checkout of a pull request
/// leaves it: no `main`, no `origin/main`, and no history behind `HEAD`.
/// Each missing revision gets the help that fits it, and none is told to
/// check its spelling when the fix is a fetch.
#[test]
fn a_depth_one_clone_names_the_fetch_or_the_deepen() {
    let f = published_fixture(1);
    cambium_testkit::git(&f.ws, &["branch", "-q", "pr", &f.commits[3]]);
    let dir = tempfile::tempdir().expect("tempdir");
    let clone = dir.path().join("clone");
    let url = format!("file://{}", f.ws.display());
    cambium_testkit::git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "--branch",
            "pr",
            &url,
            clone.to_str().unwrap(),
        ],
    );
    std::fs::copy(f.ws.join("cambium.toml"), clone.join("cambium.toml")).unwrap();
    let c = clone.to_str().unwrap();

    let (code, stdout, stderr) = run(&["-C", c, "affected", "main", "--dry-run"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(stderr.contains("error[CAMBI-E003]: `main`"), "{stderr}");
    assert!(
        stderr.contains("git fetch origin main:refs/remotes/origin/main --depth=N"),
        "{stderr}"
    );
    assert!(stderr.contains("pass `origin/main`"), "{stderr}");

    for rev in ["HEAD~1", "origin/pr~1"] {
        let (code, _, stderr) = run(&["-C", c, "affected", rev, "--dry-run"]);
        assert_eq!(code, 1, "{stderr}");
        assert!(stderr.contains("git fetch --deepen=N"), "{rev}:\n{stderr}");
        assert!(!stderr.contains("check the spelling"), "{rev}:\n{stderr}");
        assert!(!stderr.contains("pr~1:refs"), "{rev}:\n{stderr}");
    }
}
