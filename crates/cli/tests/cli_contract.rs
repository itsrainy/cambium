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
    let out = Command::new(CAMBI)
        .args(args)
        .output()
        .expect("the binary should be runnable");
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
    for sub in ["affected", "baseline", "index", "bench", "synth", "config"] {
        assert!(
            stdout.contains(sub),
            "`{sub}` is missing from the top-level help:\n{stdout}"
        );
    }
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

    let (code, _, _) = run(&["index", "stats", index.to_str().unwrap()]);
    assert_eq!(code, 0, "a question we can answer exits 0");

    let missing = dir.path().join("nope.cmbx");
    let (code, stdout, stderr) = run(&["index", "stats", missing.to_str().unwrap()]);
    assert_eq!(code, 1, "an unreadable index is a plain error");
    assert!(stdout.is_empty(), "nothing should reach stdout: {stdout:?}");
    assert!(!stderr.is_empty(), "the error should say what went wrong");

    let (code, _, _) = run(&[
        "index",
        "inspect",
        index.to_str().unwrap(),
        "--label",
        "//nowhere:nothing",
    ]);
    assert_eq!(code, 1, "an unknown label is a plain error");
}

/// The hub labels `index stats` reports, which is the only way to learn a
/// label out of an index without already knowing one.
fn hub_labels(index: &std::path::Path) -> Vec<String> {
    let (code, stdout, stderr) = run(&["index", "stats", index.to_str().unwrap(), "--output", "json"]);
    assert_eq!(code, 0, "index stats failed: {stderr}");
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
    const SCHEMA: u64 = 6;
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
        vec!["index", "stats", index.to_str().unwrap(), "--output", "json"],
        vec![
            "index",
            "targets",
            index.to_str().unwrap(),
            &package,
            "--output",
            "json",
        ],
        vec![
            "index",
            "rdeps",
            index.to_str().unwrap(),
            &hub,
            "--output",
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
fn index_path_exits_nonzero_when_there_is_no_path() {
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
        run(&["index", "path", index.to_str().unwrap(), a, b]),
        run(&["index", "path", index.to_str().unwrap(), b, a]),
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
