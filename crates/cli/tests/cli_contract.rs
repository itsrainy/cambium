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
        "small",
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
