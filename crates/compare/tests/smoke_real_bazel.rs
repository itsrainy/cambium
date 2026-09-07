//! End-to-end three-way comparison against a *real* Bazel, on a repository
//! this test writes.
//!
//! Ignored by default, and deliberately so: it wants bazelisk on disk, it
//! downloads a Bazel release the first time, and each pair costs two
//! whole-repository `bazel query` runs per producer — tens of seconds even for
//! a five-target workspace. Run it explicitly:
//!
//! ```text
//! USE_BAZEL_VERSION=9.2.0 cargo test -p cambium-compare -- --ignored --nocapture
//! ```
//!
//! Skipped (not failed) when `~/.cache/cambium/bin/bazelisk` is absent, so a
//! machine without it can still run `--ignored` for the rest of the suite.
//!
//! What it is actually for: the unit tests pin the arithmetic and the parsing,
//! but only a real Bazel can tell you whether the three tools are being asked
//! the *same question*. Both of the bugs this file was written to catch were
//! of that kind — bazel-diff and Tango report file targets, and `rdeps`
//! returns them too, while cambium's walk emits rules only.

use std::path::{Path, PathBuf};
use std::process::Command;

use cambium_compare::{compare_pairs, CompareOptions, Producer};
use cambium_corpus::manifest::RepoEntry;
use cambium_corpus::{pairs, CorpusDir};

fn bazelisk() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("HOME")?).join(".cache/cambium/bin/bazelisk");
    p.exists().then_some(p)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A three-package genrule workspace with a macro, a `select`, and a file
/// nothing consumes, and a history that touches each of them in turn.
fn write_repo(root: &Path) -> Vec<String> {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "--quiet", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);

    write(
        root,
        "MODULE.bazel",
        "module(name = \"smoke\", version = \"0.0.1\")\n",
    );
    write(root, "defs/BUILD.bazel", "exports_files([\"defs.bzl\"])\n");
    write(
        root,
        "defs/defs.bzl",
        r#"def cat_rule(name, srcs):
    native.genrule(
        name = name,
        srcs = srcs,
        outs = [name + ".out"],
        cmd = "cat $(SRCS) > $@",
    )
"#,
    );
    write(root, "lib/lib.txt", "one\n");
    write(
        root,
        "lib/BUILD.bazel",
        r#"genrule(
    name = "lib",
    srcs = ["lib.txt"],
    outs = ["lib.out"],
    cmd = "cat $(SRCS) > $@",
    visibility = ["//visibility:public"],
)
"#,
    );
    write(root, "util/util.txt", "u\n");
    write(
        root,
        "util/BUILD.bazel",
        r#"genrule(
    name = "util",
    srcs = ["util.txt"],
    outs = ["util.out"],
    cmd = "cat $(SRCS) > $@",
    visibility = ["//visibility:public"],
)
"#,
    );
    // The consumer reaches both leaves through a `select`, so exactly one
    // branch is taken in any configuration while cambium keeps both.
    write(
        root,
        "app/BUILD.bazel",
        r#"load("//defs:defs.bzl", "cat_rule")

# `values` rather than `constraint_values`: a constraint would pull in
# `@platforms`, and Tango's query is `deps(//...:all-targets)`, which resolves
# external repositories that cambium's `//...:all-targets` never touches.
config_setting(
    name = "opt",
    values = {"compilation_mode": "opt"},
)

cat_rule(
    name = "app",
    srcs = select({
        ":opt": ["//lib:lib.out"],
        "//conditions:default": ["//util:util.out"],
    }),
)
"#,
    );
    write(root, "lib/orphan.txt", "nobody reads me\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "-m", "initial"]);

    let mut shas = vec![head(root)];
    // A source edit: one rule direct, its consumer transitive.
    write(root, "lib/lib.txt", "two\n");
    git(root, &["commit", "--quiet", "-am", "edit a source file"]);
    shas.push(head(root));
    // A rule edit in a BUILD file.
    write(
        root,
        "util/BUILD.bazel",
        r#"genrule(
    name = "util",
    srcs = ["util.txt"],
    outs = ["util.out"],
    cmd = "cat $(SRCS) | tr a-z A-Z > $@",
    visibility = ["//visibility:public"],
)
"#,
    );
    git(root, &["commit", "--quiet", "-am", "edit a rule"]);
    shas.push(head(root));
    // A macro edit: every rule proto in the loading package is byte-identical,
    // so only the static `.bzl` load closure catches it.
    write(
        root,
        "defs/defs.bzl",
        r#"def cat_rule(name, srcs):
    native.genrule(
        name = name,
        srcs = srcs,
        outs = [name + ".out"],
        cmd = "cat $(SRCS) | sort > $@",
    )
"#,
    );
    git(root, &["commit", "--quiet", "-am", "edit the macro"]);
    shas.push(head(root));
    // A file no target consumes.
    write(root, "lib/orphan.txt", "nobody reads me\nstill nobody\n");
    git(
        root,
        &["commit", "--quiet", "-am", "edit an unconsumed file"],
    );
    shas.push(head(root));
    shas
}

fn head(root: &Path) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `USE_BAZEL_VERSION`, if set: bazelisk reads it itself once it is in the
/// process environment (which every `Command` here inherits), but we also
/// thread it explicitly into `Checkout::engine`, which both sets it on the
/// `Bazel` (so it rides along to the compare producers via `run_context`'s
/// `env`, not just bazelisk's own child) and keys `--output_base` off it, so
/// three versions run side by side without sharing a server or an install.
fn bazel_version() -> Option<String> {
    std::env::var("USE_BAZEL_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
}

/// A scratch root that is stable across a version's runs (handy for
/// inspecting bazelisk's downloads and the output base by hand) rather than a
/// fresh `TempDir` every time. Override with `CAMBIUM_SMOKE_SCRATCH`;
/// defaults under the system tmp dir, so this works unmodified in CI.
fn scratch_root(version: Option<&str>) -> PathBuf {
    let base = std::env::var_os("CAMBIUM_SMOKE_SCRATCH")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("cambium-smoke-bazel"));
    match version {
        Some(v) => base.join(v),
        None => base.join("default"),
    }
}

#[test]
#[ignore = "needs bazelisk and a network fetch of a Bazel release; run with --ignored"]
fn the_three_tools_agree_on_a_real_bazel_workspace() {
    let Some(bazel) = bazelisk() else {
        eprintln!("no ~/.cache/cambium/bin/bazelisk; skipping");
        return;
    };
    let version = bazel_version();
    let root = scratch_root(version.as_deref());
    std::fs::create_dir_all(&root).unwrap();
    let tmp = tempfile::tempdir_in(&root).unwrap();
    let src = tmp.path().join("src");
    let shas = write_repo(&src);
    let pin = shas.last().unwrap().clone();

    let entry = RepoEntry {
        id: "smoke".into(),
        // A local path is a perfectly good git remote, which is what keeps
        // this test off the network for everything but Bazel itself.
        url: format!("file://{}", src.display()),
        mirror: None,
        pin: pin.clone(),
        replay_range: shas.len(),
        bazel: bazel.display().to_string(),
        bazel_version: None,
        extra_query_flags: Vec::new(),
        extra_startup_flags: Vec::new(),
        invalidators: Vec::new(),
        bazel_versions: Vec::new(),
        skip: false,
        notes: "synthetic smoke workspace".into(),
    };
    let corpus = CorpusDir::new(tmp.path().join("corpus"));
    let checkout = corpus.fetch(&entry).unwrap();
    checkout.write_config(&entry, version.as_deref()).unwrap();
    let engine = checkout.engine(&entry, version.as_deref()).unwrap();
    eprintln!(
        "bazel: {} (USE_BAZEL_VERSION={:?}), output_base under {}",
        bazel.display(),
        version,
        corpus.output_base(&entry.id, version.as_deref()).display()
    );

    let producers: Vec<Box<dyn Producer>> = [
        cambium_compare::BazelDiff::discover(cambium_compare::Implementation::Rust)
            .ok()
            .map(|b| Box::new(b) as Box<dyn Producer>),
        cambium_compare::Tango::discover()
            .ok()
            .map(|t| Box::new(t) as Box<dyn Producer>),
    ]
    .into_iter()
    .flatten()
    .collect();
    if producers.is_empty() {
        eprintln!("neither bazel-diff nor tango is installed; skipping");
        return;
    }
    let refs: Vec<&dyn Producer> = producers.iter().map(|p| p.as_ref()).collect();

    let results = tmp.path().join("results/smoke");
    let selected = pairs::select(
        &checkout.repo,
        &[pairs::PairKind::Adjacent],
        &pairs::PairOptions {
            repo_id: entry.id.clone(),
            pin: pin.clone(),
            replay_range: shas.len(),
            seed: 0,
            cap: 20,
            hub_packages: Vec::new(),
            invalidator_paths: Vec::new(),
            invalidator_globs: Vec::new(),
        },
    )
    .unwrap();
    assert!(selected.pairs.len() >= 3, "{:?}", selected.pairs);

    let opts = CompareOptions {
        oracle_k: 10,
        resume: false,
        scratch: tmp.path().join("scratch"),
        ..CompareOptions::new("smoke", pin, results)
    };
    let result = compare_pairs(&engine, &checkout, &selected.pairs, &refs, &opts).unwrap();

    // The rendered section, so a run of this test by hand shows the same table
    // the corpus suite would write into docs/CORPUS.md.
    println!(
        "{}",
        cambium_compare::report::render_repo_section(
            &cambium_compare::RepoResults::new("bazel", "bazel")
                .with_compares(vec![result.clone()]),
            None,
        )
    );

    for pair in &result.pairs {
        for p in &pair.producers {
            assert!(p.error.is_none(), "{} failed: {:?}", p.name, p.error);
        }
    }
    assert!(
        result.summary.min_recall >= 1.0,
        "cambium missed something Bazel's rdeps reached: {:#?}",
        result
            .pairs
            .iter()
            .filter_map(|p| p.oracle.as_ref())
            .filter(|o| !o.ok())
            .collect::<Vec<_>>()
    );
    let failures: usize = cambium_compare::Class::ALL
        .iter()
        .filter(|c| c.is_failure())
        .filter_map(|c| result.summary.by_class.get(c.name()))
        .sum();
    assert_eq!(
        failures, 0,
        "unexplained disagreements: {:#?}",
        result.summary.by_class
    );
    assert!(result.summary.pass, "{:#?}", result.summary);
}
