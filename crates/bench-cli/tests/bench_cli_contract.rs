//! The harness binary's own surface, and the one thing it must get right about
//! the product binary: reading back what `cambi` prints.
//!
//! `crates/cli/tests/cli_contract.rs` pins `cambi`'s surface, which is a
//! published contract. Nothing here is: this file exists because the harness is
//! the first caller to break when the surface moves, and that break should
//! arrive in CI rather than eight hours into a run.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    std::fs::create_dir_all(subject.join("history")).unwrap();
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

/// The verbs that left `cambi`. `subjects`, `history` and `changes` are what
/// `tools/bench/run-published.sh` drives; `replay` is what
/// `tools/bench/precheck.sh` drives.
#[test]
fn the_verbs_are_the_ones_that_left_cambi() {
    let (_, stdout, _) = run(Path::new(BENCH), &["--help"]);
    for sub in [
        "subjects", "history", "changes", "stages", "sweep", "regress", "replay", "labels",
        "report", "synth",
    ] {
        assert!(
            stdout.contains(sub),
            "`{sub}` is missing from the top-level help:\n{stdout}"
        );
    }
}

/// `synth emit-bazel` never touches Bazel, so it is safe in any CI sandbox. It
/// prints a "try: cambi ..." suggestion, which has regressed before to name a
/// crate (`cambium`, now also `cambium-bench`) rather than the binary a caller
/// types.
#[test]
fn hermetic_output_never_tells_a_caller_to_type_a_crate_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("ws");
    let (code, stdout, stderr) = run(
        Path::new(BENCH),
        &[
            "synth",
            "emit-bazel",
            "--spec",
            "tiny",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "emit-bazel failed: {stderr}");
    assert!(
        stdout.contains("try: cambi -C"),
        "the suggestion should name the product binary:\n{stdout}"
    );
    assert!(
        !stdout.contains("cambium "),
        "hermetic output leaked a crate name as a binary a caller would type:\n{stdout}"
    );
}

/// `synth build-index` is the cheap index the `inspect` verbs are pointed at,
/// and it moved here with the rest of the generator.
#[test]
fn synth_build_index_writes_an_index_the_engine_can_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("i.cmbx");
    let (code, stdout, stderr) = run(
        Path::new(BENCH),
        &[
            "synth",
            "build-index",
            "--spec",
            "tiny",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("nodes:"), "{stdout}");
    let f = cambium_index::IndexFile::open(&out).expect("a readable index");
    assert!(!f.view().commit.is_empty());
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

/// A synthetic workspace with a real git history, a directory store, and an
/// index published for the commit `at`. The rows come from the synthetic
/// source, so Bazel never runs. The same fixture `crates/cli/tests` uses.
struct PublishedFixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    cache: PathBuf,
    commits: Vec<String>,
}

fn published_fixture(at: usize) -> PublishedFixture {
    use cambium_engine::{Config, Engine, SynthQuerier};

    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = dir.path().join("store");
    let cache = dir.path().join("cache");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let graph = cambium_synth::generate(&cambium_synth::GraphSpec::preset("tiny").unwrap());
    let spec = cambium_synth::ChangeSpec {
        seed: 3,
        ..cambium_synth::ChangeSpec::preset("typical-pr").unwrap()
    };
    let emitted = cambium_synth::emit_git_history(graph, &spec, 4, &ws, true).expect("a history");
    let mut cfg = Config::load(&ws).unwrap();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: store_dir.clone(),
    };
    std::fs::write(ws.join("cambium.toml"), cfg.to_toml()).unwrap();

    let engine = Engine::with_parts(
        cfg.clone(),
        cambium_diff::Repo::new(ws.clone()),
        cambium_bazel::Bazel::at(ws.clone(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(cache.clone());
    let index = engine
        .full_build_with(
            &emitted.commits[at],
            &SynthQuerier::new(&emitted.graphs[at]),
        )
        .unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let file = cache.join("published.cmbx");
    cambium_index::format::write_index(&file, &index).unwrap();
    let repo = cambium_diff::Repo::new(ws.clone());
    let tree = repo.tree_of(&emitted.commits[at]).unwrap();
    cambium_engine::BaselineCache::new(
        &cambium_engine::store::DirStore::new(store_dir),
        &cache,
        cfg.hash(),
    )
    .publish(&file, &index.header.lineage, &emitted.commits[at], &tree)
    .expect("publishing");

    PublishedFixture {
        _dir: dir,
        ws,
        cache,
        commits: emitted.commits,
    }
}
