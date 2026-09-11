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
    // The verb tree docs/CLI.md settles on, plus the two that leave the
    // binary in a later step. `baseline` is deliberately absent: it still
    // works, and a deprecated spelling is not advertised.
    for sub in [
        "status", "affected", "index", "inspect", "config", "bench", "synth",
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
}

#[test]
fn hermetic_output_never_tells_a_caller_to_type_cambium() {
    // `synth emit-bazel` never touches Bazel, so it is safe to run in any CI
    // sandbox. It prints a "try: cambi ..." suggestion, which has regressed
    // before to name the crate (`cambium`) rather than the binary (`cambi`).
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("ws");
    let (code, stdout, stderr) = run(&[
        "synth",
        "emit-bazel",
        "--spec",
        "tiny",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "emit-bazel failed: {stderr}");
    assert!(
        !stdout.contains("cambium "),
        "hermetic output leaked the crate name as a binary a caller would type:\n{stdout}"
    );
}

#[test]
fn a_bad_invocation_exits_nonzero_rather_than_printing_an_answer() {
    // A caller that mistypes must not have an empty stdout read as "nothing is
    // affected". clap uses 2 for usage errors; what matters here is that it is
    // not 0, and that stdout stays empty.
    let (code, stdout, stderr) = run(&["affected", "--not-a-flag"]);
    assert_ne!(code, 0, "a usage error must not look like success");
    assert!(stdout.is_empty(), "nothing should reach stdout: {stdout:?}");
    assert!(!stderr.is_empty(), "the error should say what went wrong");
}

/// Builds a synthetic index in `dir` and returns its path. `synth build-index`
/// never shells out to Bazel or git, so everything below runs in a bare
/// sandbox.
fn synth_index(dir: &std::path::Path) -> std::path::PathBuf {
    let out = dir.join("synth.cmbx");
    let (code, _, stderr) = run(&[
        "synth",
        "build-index",
        "--spec",
        "tiny",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "synth build-index failed: {stderr}");
    out
}

#[test]
fn the_exit_codes_are_the_ones_the_readme_documents() {
    // 0 for an answer, 1 for a failure the caller caused. The Bazel- and
    // baseline-specific codes (3, 4) need a workspace, so they are pinned
    // against `EngineError` in the unit tests instead; these two are the ones
    // every caller branches on, and they must hold with nothing installed.
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
    const SCHEMA: u64 = 9;
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
    assert_eq!(v["schema"].as_u64(), Some(9));
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
    assert_eq!(v["schema"].as_u64(), Some(9));
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
