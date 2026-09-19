//! The harness binary's own surface, and the one thing it must get right about
//! the product binary: reading back what `cambi` prints.
//!
//! `crates/cli/tests/cli_contract.rs` pins `cambi`'s surface, which is a
//! published contract. Nothing here is: this file exists because the harness is
//! the first caller to break when the surface moves, and that break should
//! arrive in CI rather than eight hours into a run.

use std::path::{Path, PathBuf};
use std::process::Command;

use cambium_testkit::published_fixture;

/// The binary is `cambium-bench`: `tools/bench/*.sh` and the CI jobs exec that
/// name, and `CARGO_BIN_EXE_cambium-bench` does not exist unless the `[[bin]]`
/// is named that, so a rename fails here at compile time.
const BENCH: &str = env!("CARGO_BIN_EXE_cambium-bench");

/// The product binary, beside this test's own executable.
///
/// `CARGO_BIN_EXE_*` only names binaries of this package, so `cambi` is found
/// where cargo puts it: `target/<profile>/cambi`, two directories up from
/// `target/<profile>/deps/<test>-<hash>`.
fn cambi() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.parent()?.join("cambi");
    p.exists().then_some(p)
}

fn run(bin: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin)
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
fn the_binary_is_named_cambium_bench() {
    assert!(
        Path::new(BENCH)
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == "cambium-bench"),
        "cargo built {BENCH}, which is not cambium-bench"
    );
    let (code, stdout, _) = run(Path::new(BENCH), &["--help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Usage: cambium-bench "),
        "the usage line should name the binary the scripts exec:\n{stdout}"
    );
}

/// No verb, in a pipe, is the usage error it has always been: the launcher is
/// a screen, and a caller with no terminal must get `--help` and a non-zero
/// exit rather than a process waiting on a keystroke nobody can send.
#[test]
fn no_verb_without_a_terminal_prints_the_help_and_fails() {
    let (code, stdout, _) = run(Path::new(BENCH), &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("Usage: cambium-bench "), "{stdout}");
    assert!(
        stdout.contains("watch"),
        "the new verb is listed:\n{stdout}"
    );
}

/// `watch` reads files and nothing else, so it answers on a results tree with
/// no run behind it — which is what makes it usable on a copy.
#[test]
fn watch_once_reads_a_results_tree_it_did_not_produce() {
    let dir = tempfile::tempdir().unwrap();
    let subject = dir.path().join("envoy");
    std::fs::create_dir_all(subject.join("walk")).unwrap();
    let (code, stdout, _) = run(
        Path::new(BENCH),
        &[
            "watch",
            "--once",
            "--results-dir",
            &dir.path().display().to_string(),
        ],
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("cambium-bench watch"), "{stdout}");
    assert!(stdout.contains("envoy"), "{stdout}");

    let (code, stdout, _) = run(
        Path::new(BENCH),
        &[
            "--format",
            "json",
            "watch",
            "--once",
            "--results-dir",
            &dir.path().display().to_string(),
        ],
    );
    assert_eq!(code, 0, "{stdout}");
    let doc: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
    assert_eq!(doc["schema"], 1);
    assert_eq!(doc["subjects"][0]["subject"], "envoy");
}

/// The verbs that left `cambi`. `subjects` and `walk` are what
/// `tools/bench/run-published.sh` drives; `replay` is what
/// `tools/bench/precheck.sh` drives.
#[test]
fn the_verbs_are_the_ones_that_left_cambi() {
    let (_, stdout, _) = run(Path::new(BENCH), &["--help"]);
    for sub in [
        "subjects", "walk", "skyframe", "replay", "labels", "report", "watch", "run",
    ] {
        assert!(
            stdout.contains(sub),
            "`{sub}` is missing from the top-level help:\n{stdout}"
        );
    }
    // The retired micro-benchmark and per-workload verbs are gone.
    for sub in ["stages", "sweep", "regress", "synth", "history", "changes"] {
        assert!(
            !stdout.contains(&format!("  {sub}")),
            "`{sub}` should have left the harness:\n{stdout}"
        );
    }
}

/// The harness locates `cambi` the way the scripts hand it over, and records
/// the version string it found.
#[test]
fn the_harness_locates_cambi_and_records_its_version() {
    let Some(cambi) = cambi() else {
        panic!("no cambi beside this test; `cargo test --workspace` builds it");
    };
    let c = cambium_bench::tools::cambi::Cambi::at(&cambi).expect("the product binary");
    assert!(
        c.version().starts_with("cambi "),
        "the recorded version should be the binary's own line: {:?}",
        c.version()
    );
    // It is the `--version` line verbatim, which is what a result file records
    // so that a number names the binary that produced it.
    let (_, stdout, _) = run(&cambi, &["--version"]);
    assert_eq!(c.version(), stdout.trim());
}

/// The shell-out path, end to end, against a real `cambi` over a real
/// workspace: spawn, exit code, stdout, and the JSON read back with the
/// engine's own types.
///
/// The *full* affected path needs Bazel to query the dirty packages, and Bazel
/// is not available in this sandbox, so what is driven here is `--dry-run`:
/// the same argv shape, the same process, the same document discipline, and a
/// plan that is computed without Bazel. The report half of the parse is pinned
/// by the unit tests in `cambium_bench::tools::cambi`, which feed the parser a
/// document the engine serialised.
#[test]
fn the_harness_reads_back_a_document_the_real_cambi_printed() {
    let Some(cambi) = cambi() else {
        panic!("no cambi beside this test; `cargo test --workspace` builds it");
    };
    let f = published_fixture(1);
    let range = format!("{}..{}", f.commits[1], f.commits[f.commits.len() - 1]);
    let c = cambium_bench::tools::cambi::Cambi::at(&cambi).expect("the product binary");
    let out = Command::new(c.path())
        .arg("-C")
        .arg(&f.ws)
        .arg("--cache-dir")
        .arg(&f.cache)
        .args(["--format", "json", "affected", "--dry-run"])
        .arg(&range)
        .output()
        .expect("spawning cambi");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("one JSON document on stdout");
    assert_eq!(doc["schema"], cambium_engine::report::SCHEMA_VERSION);
    assert_eq!(doc["plan"]["mode"], "splice");
    assert_eq!(doc["plan"]["index"]["revision"], f.commits[1]);
}
