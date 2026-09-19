//! End-to-end walk against a *real* Bazel, on a repository this test writes.
//!
//! Ignored by default, and deliberately so: it wants bazelisk on disk, it
//! downloads a Bazel release the first time, and each change set costs two
//! whole-repository `bazel query` runs per tool — tens of seconds even for
//! a five-target workspace. Run it explicitly:
//!
//! ```text
//! USE_BAZEL_VERSION=9.2.0 cargo test -p cambium-bench --test smoke_real_bazel -- --ignored --nocapture
//! ```
//!
//! Skipped (not failed) when `~/.cache/cambium/bin/bazelisk` or both tools are
//! absent, so a machine without them can still run `--ignored` for the rest of
//! the suite. `CAMBIUM_REQUIRE_TOOLS=1` turns each skip into a failure; CI's
//! matrix job sets it, having just built the tools.
//!
//! What it is actually for: the unit tests pin the arithmetic and the parsing,
//! but only a real Bazel can tell you whether the three tools are being asked
//! the *same question*. Both of the bugs this file was written to catch were
//! of that kind — bazel-diff and Tango report file targets, and `rdeps`
//! returns them too, while cambium's walk emits rules only.

use std::path::{Path, PathBuf};

use cambium_testkit::{git, write};

/// The `cambi` the comparison measures: the one cargo built beside this test,
/// else whatever `--cambi`/`$CAMBI`/`PATH` names. The harness runs the product
/// binary rather than the engine, so this smoke test needs one on disk.
fn cambi() -> Cambi {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.join("cambi")))
        .filter(|p| p.exists());
    match beside {
        Some(p) => Cambi::at(&p).expect("the cambi cargo built"),
        None => Cambi::locate(None).expect("a cambi binary: cargo build -p cambium-cli"),
    }
}

use cambium_bench::checkout::SubjectsDir;
use cambium_bench::env::Environment;
use cambium_bench::manifest::RepoEntry;
use cambium_bench::tools::cambi::Cambi;
use cambium_bench::tools::Tool;
use cambium_bench::walk;

fn bazelisk() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("HOME")?).join(".cache/cambium/bin/bazelisk");
    p.exists().then_some(p)
}

/// Says why the test is not running, or refuses to skip when the environment
/// says the tools must be there.
fn skip(why: &str) {
    if std::env::var_os("CAMBIUM_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("CAMBIUM_REQUIRE_TOOLS is set: {why}");
    }
    eprintln!("{why}; skipping");
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
    git(root, &["rev-parse", "HEAD"])
}

/// `USE_BAZEL_VERSION`, if set: bazelisk reads it itself once it is in the
/// process environment (which every `Command` here inherits), but we also
/// thread it explicitly into `Checkout::engine`, which both sets it on the
/// `Bazel` (so it rides along to the compare tools via `run_context`'s
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
        skip("no ~/.cache/cambium/bin/bazelisk");
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
        pin: pin.clone(),
        history_window: shas.len(),
        bazel: bazel.display().to_string(),
        bazel_version: None,
        extra_query_flags: Vec::new(),
        extra_startup_flags: Vec::new(),
        publish_partial: false,
        invalidators: Vec::new(),
        skip: false,
        notes: "synthetic smoke workspace".into(),
    };
    let subjects = SubjectsDir::new(tmp.path().join("subjects"));
    let checkout = subjects.fetch(&entry).unwrap();
    checkout.write_config(&entry, version.as_deref()).unwrap();
    let engine = checkout.engine(&entry, version.as_deref()).unwrap();
    eprintln!(
        "bazel: {} (USE_BAZEL_VERSION={:?}), output_base under {}",
        bazel.display(),
        version,
        subjects
            .output_base(&entry.id, version.as_deref())
            .display()
    );

    let tools: Vec<Box<dyn Tool>> = [
        cambium_bench::tools::bazeldiff::BazelDiff::discover(
            cambium_bench::tools::bazeldiff::Implementation::Rust,
        )
        .ok()
        .map(|b| Box::new(b) as Box<dyn Tool>),
        cambium_bench::tools::tango::Tango::discover()
            .ok()
            .map(|t| Box::new(t) as Box<dyn Tool>),
    ]
    .into_iter()
    .flatten()
    .collect();
    if tools.is_empty() {
        skip("neither bazel-diff nor tango is installed");
        return;
    }
    let refs: Vec<&dyn Tool> = tools.iter().map(|p| p.as_ref()).collect();

    let results = tmp.path().join("results/smoke/walk");
    let (commits, steps) =
        walk::steps(&checkout.repo, &pin, shas.len(), &walk::Probes::default()).unwrap();
    assert!(steps.len() >= 3, "{steps:?}");

    let cambi = cambi();
    let driver = walk::CambiDriver {
        checkout: &checkout,
        cambi: &cambi,
    };
    let scratch = tmp.path().join("scratch");
    let live = walk::BazelChecks {
        engine: &engine,
        checkout: &checkout,
        tools: &refs,
        results_dir: &results,
        scratch: &scratch,
        oracle_k: 0,
        seed: 0,
    };
    let opts = walk::WalkOptions {
        repo_id: "smoke".into(),
        pin: pin.clone(),
        results_dir: results.clone(),
        state_dir: tmp.path().join("results/smoke"),
        bazel_version: version.clone().unwrap_or_default(),
        schedule: walk::Tier::Full.defaults(),
        oracle_sample: walk::Sample::all(&steps),
        peer_sample: walk::Sample::all(&steps),
        environment: Environment {
            machine: cambium_bench::harness::Machine::detect(),
            bazel_version_used: version.clone().unwrap_or_default(),
            cambi_version: cambi.version().to_string(),
            tool_sha: String::new(),
            engine_id: String::new(),
            config_hash: String::new(),
            output_bases: Default::default(),
        },
        publish: true,
        resume: false,
        diff_cap: 50,
        progress: None,
        only: false,
    };
    let result = walk::walk(&engine, &driver, &live, &commits, &steps, &opts).unwrap();

    // The rendered section, so a run of this test by hand shows the same table
    // the benchmark would write into docs/RESULTS.md.
    println!(
        "{}",
        cambium_bench::report::render_subject_section(&cambium_bench::report::SubjectResults {
            id: "smoke".into(),
            walk: Some(result.clone()),
        },)
    );

    for st in &result.steps {
        for t in &st.tools {
            assert!(!t.failed, "{} failed on {}..{}", t.name, st.parent, st.head);
        }
    }
    assert!(
        result.summary.soundness.worst_recall >= 1.0,
        "cambium missed something Bazel's rdeps reached at {}",
        result.summary.soundness.worst_step
    );
    assert!(result.summary.pass, "{:#?}", result.summary);
}

/// A second workspace, written for the oracle alone: `defs/defs.bzl` is both a
/// macro other packages load *and* a source of a filegroup in its own package.
///
/// That is the shape of the miss 11ae8e0 fixed — `utils.bzl` in the `srcs` of
/// fifteen filegroups in `bazel/bazel` — and it is the shape the sampled
/// oracle could not see, because its roots came from cambium's own direct set.
fn write_bzl_filegroup_repo(root: &Path) -> Vec<String> {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "--quiet", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);

    write(
        root,
        "MODULE.bazel",
        "module(name = \"oracle\", version = \"0.0.1\")\n",
    );
    write(
        root,
        "defs/BUILD.bazel",
        r#"exports_files(["defs.bzl"])

filegroup(
    name = "bzl_srcs",
    srcs = ["defs.bzl"],
    visibility = ["//visibility:public"],
)
"#,
    );
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
    write(
        root,
        "app/BUILD.bazel",
        r#"load("//defs:defs.bzl", "cat_rule")

cat_rule(
    name = "app",
    srcs = ["//lib:lib.out"],
)
"#,
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "-m", "initial"]);
    let mut shas = vec![head(root)];

    // The `.bzl`: `rdeps` names the filegroup that lists it, and nothing else.
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

    // A plain source: `rdeps` names the genrule that reads it and the app that
    // consumes its output.
    write(root, "lib/lib.txt", "two\n");
    git(root, &["commit", "--quiet", "-am", "edit a source file"]);
    shas.push(head(root));
    shas
}

/// The full oracle, against `rdeps` computed by hand on a workspace small
/// enough to compute it by hand.
///
/// The unit tests pin how the answer is scored; this pins that the question is
/// the right one — that a changed path really does become a label Bazel
/// accepts, and that the set that comes back is the set a person reading the
/// BUILD files would write down. No other tool runs: the oracle is what is
/// under test, so bazel-diff and Tango being absent must not skip it.
#[test]
#[ignore = "needs bazelisk and a network fetch of a Bazel release; run with --ignored"]
fn the_full_oracle_matches_hand_computed_rdeps() {
    let Some(bazel) = bazelisk() else {
        skip("no ~/.cache/cambium/bin/bazelisk");
        return;
    };
    let version = bazel_version();
    let root = scratch_root(version.as_deref());
    std::fs::create_dir_all(&root).unwrap();
    let tmp = tempfile::tempdir_in(&root).unwrap();
    let src = tmp.path().join("src");
    let shas = write_bzl_filegroup_repo(&src);
    let pin = shas.last().unwrap().clone();

    let entry = RepoEntry {
        id: "oracle".into(),
        url: format!("file://{}", src.display()),
        pin: pin.clone(),
        history_window: shas.len(),
        bazel: bazel.display().to_string(),
        bazel_version: None,
        extra_query_flags: Vec::new(),
        extra_startup_flags: Vec::new(),
        publish_partial: false,
        invalidators: Vec::new(),
        skip: false,
        notes: "oracle smoke workspace".into(),
    };
    let subjects = SubjectsDir::new(tmp.path().join("subjects"));
    let checkout = subjects.fetch(&entry).unwrap();
    checkout.write_config(&entry, version.as_deref()).unwrap();
    let engine = checkout.engine(&entry, version.as_deref()).unwrap();

    let (commits, steps) =
        walk::steps(&checkout.repo, &pin, shas.len(), &walk::Probes::default()).unwrap();
    assert!(steps.len() >= 2, "{steps:?}");
    let cambi = cambi();
    let driver = walk::CambiDriver {
        checkout: &checkout,
        cambi: &cambi,
    };
    let scratch = tmp.path().join("scratch");
    let results = tmp.path().join("results/oracle/walk");
    // No peers; the oracle on every step is what this test reads.
    let live = walk::BazelChecks {
        engine: &engine,
        checkout: &checkout,
        tools: &[],
        results_dir: &results,
        scratch: &scratch,
        oracle_k: 0,
        seed: 0,
    };
    let opts = walk::WalkOptions {
        repo_id: "oracle".into(),
        pin: pin.clone(),
        results_dir: results.clone(),
        state_dir: tmp.path().join("results/oracle"),
        bazel_version: version.clone().unwrap_or_default(),
        schedule: walk::Tier::Correctness.schedule(&walk::Overrides {
            oracle_sample: Some(0), // every step
            ..Default::default()
        }),
        oracle_sample: walk::Sample::all(&steps),
        peer_sample: walk::Sample::default(),
        environment: Environment {
            machine: cambium_bench::harness::Machine::detect(),
            bazel_version_used: version.clone().unwrap_or_default(),
            cambi_version: cambi.version().to_string(),
            tool_sha: String::new(),
            engine_id: String::new(),
            config_hash: String::new(),
            output_bases: Default::default(),
        },
        publish: true,
        resume: false,
        diff_cap: 50,
        progress: None,
        only: false,
    };
    let result = walk::walk(&engine, &driver, &live, &commits, &steps, &opts).unwrap();
    assert!(result.steps.len() >= 2);

    // `defs/defs.bzl` -> the filegroup that lists it in `srcs`. `//app:app`
    // reaches the file through a `load()`, which is not a `deps` edge, so
    // `rdeps` does not name it and cambium over-approximates by including it.
    let bzl = result.steps[0].oracle.as_ref().expect("an oracle ran");
    assert_eq!(bzl.roots, vec!["//defs:defs.bzl"], "{bzl:#?}");
    assert!(!bzl.sampled);
    assert_eq!(
        bzl.oracle_set.iter().cloned().collect::<Vec<_>>(),
        vec!["//defs:bzl_srcs"],
        "hand-computed rdeps of the changed .bzl: {bzl:#?}"
    );
    assert!(bzl.missing.is_empty(), "{bzl:#?}");
    assert_eq!(bzl.recall(), 1.0);

    // `lib/lib.txt` -> the genrule that reads it, and the app that consumes
    // its output.
    let src_set = result.steps[1].oracle.as_ref().expect("an oracle ran");
    assert_eq!(src_set.roots, vec!["//lib:lib.txt"], "{src_set:#?}");
    assert_eq!(
        src_set.oracle_set.iter().cloned().collect::<Vec<_>>(),
        vec!["//app:app", "//lib:lib"],
        "hand-computed rdeps of the changed source: {src_set:#?}"
    );
    assert!(src_set.missing.is_empty(), "{src_set:#?}");
    assert_eq!(src_set.recall(), 1.0);
    assert!(result.summary.pass, "{:#?}", result.summary);
}
