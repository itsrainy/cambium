//! A `.bazelrc` edit is only a global invalidation when the diff says it could
//! be one.
//!
//! Real git (the engine reads both sides of the file with `git show`), a
//! synthetic graph, no Bazel. `.bazelrc` is a default invalidator, so the first
//! test is also the check that the screen runs *before* the generic rule.

use std::path::Path;
use std::process::Command;

use cambium_engine::{AffectedRequest, Config, Engine, RunOutcome, SynthQuerier};
use cambium_synth::{generate, GraphSpec, SynthGraph};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository with `.bazelrc` at `base` and then at `head`, and the engine
/// over it. Returns the two shas.
fn repo_with(base_rc: &str, head_rc: &str) -> (tempfile::TempDir, Engine, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.email", "t@example.com"]);
    git(p, &["config", "user.name", "t"]);
    std::fs::write(p.join(".bazelrc"), base_rc).unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "base"]);
    let base = git(p, &["rev-parse", "HEAD"]);
    std::fs::write(p.join(".bazelrc"), head_rc).unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "head"]);
    let head = git(p, &["rev-parse", "HEAD"]);
    let engine = Engine::with_parts(
        Config::default(),
        cambium_diff::Repo::new(p.to_path_buf()),
        cambium_bazel::Bazel::at(p.to_path_buf(), Some("/nonexistent/bazel")),
    );
    (dir, engine, base, head)
}

fn run(engine: &Engine, base: &str, head: &str, graph: &SynthGraph) -> RunOutcome {
    let index = engine
        .full_build_with(base, &SynthQuerier::new(graph))
        .unwrap();
    let req = AffectedRequest {
        materialize: true,
        no_full_build: true,
        ..AffectedRequest::new(base, head)
    };
    let changes = vec![cambium_diff::Change::new(
        cambium_diff::Status::Modified,
        ".bazelrc",
    )];
    engine
        .compute(index.view(), &changes, &SynthQuerier::new(graph), &req)
        .unwrap()
}

fn graph() -> SynthGraph {
    generate(&GraphSpec::preset("small").unwrap())
}

#[test]
fn a_jobs_change_invalidates_nothing() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "build --jobs=8\nbuild --remote_cache=grpc://old\n",
        "build --jobs=16\nbuild --remote_cache=grpc://new\n",
    );
    let out = run(&e, &base, &head, &g);
    let r = &out.report;
    assert!(!r.global, "global for {:?}", r.global_reason);
    assert!(r.dirty_packages.is_empty(), "{:?}", r.dirty_packages);
    assert!(r.affected.is_empty(), "{:?}", r.affected);
    assert_eq!(r.stats.bazelrc_safe_diffs, 1);
}

#[test]
fn a_define_change_is_global_with_the_flag_named() {
    let g = graph();
    let (_d, e, base, head) = repo_with(
        "build --jobs=8\n",
        "build --jobs=8\nbuild --define=engine=fast\n",
    );
    let out = run(&e, &base, &head, &g);
    let r = &out.report;
    assert!(r.global);
    assert_eq!(
        r.global_reason.as_deref(),
        Some(".bazelrc: build --define changed, not on the loading-irrelevant allowlist")
    );
    assert_eq!(r.stats.bazelrc_safe_diffs, 0);
}

#[test]
fn an_added_import_is_global() {
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

/// Real evidence, opt-in: classify every `.bazelrc` edit in the envoy corpus
/// checkout and print the verdict. Ignored because it needs
/// `~/.cache/cambium/corpus/envoy`; run with
/// `cargo test -p cambium-engine --test bazelrc_diff -- --ignored --nocapture`.
#[test]
#[ignore = "needs the envoy corpus checkout"]
fn envoy_bazelrc_commits() {
    let entry = std::env::var("CAMBIUM_BAZELRC_ENTRY").unwrap_or_else(|_| "envoy".into());
    let root = dirs_home().join(".cache/cambium/corpus").join(entry);
    if !root.exists() {
        eprintln!("no envoy checkout at {}", root.display());
        return;
    }
    // envoy's corpus pin; another entry is surveyed from its own HEAD.
    let pin = &std::env::var("CAMBIUM_BAZELRC_PIN")
        .unwrap_or_else(|_| "7ba51266cc60cd560c37ea9cb3e2adf70676b6cf".into());
    let known = Command::new("git")
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
    // The corpus's own window is the last 50 first-parent commits; widen it
    // with CAMBIUM_BAZELRC_WINDOW to survey more history.
    let n = std::env::var("CAMBIUM_BAZELRC_WINDOW").unwrap_or_else(|_| "50".into());
    let n = format!("-{}", n.trim_start_matches('-'));
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
            ".bazelrc",
        ],
    );
    for sha in touching.lines().filter(|s| window.contains(s)) {
        let subject = git(&root, &["log", "-1", "--format=%s", sha]);
        let parent = git(&root, &["rev-parse", &format!("{sha}^")]);
        let base = git(&root, &["show", &format!("{parent}:.bazelrc")]);
        let head = git(&root, &["show", &format!("{sha}:.bazelrc")]);
        let verdict = cambium_engine::bazelrc::classify(&base, &head, &[]);
        println!("{} {subject}\n    {verdict:?}", &sha[..12]);
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME").map(std::path::PathBuf::from).unwrap()
}
