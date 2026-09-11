//! End-to-end tests against a real Bazel (spec 12.2, 12.3).
//!
//! These are the tests that check the parts the synthetic oracles cannot: that
//! the vendored proto really decodes what Bazel emits, that `//pkg:*` and
//! `rbuildfiles` mean what we think they mean, and that our reverse closure
//! agrees with `bazel query rdeps`.
//!
//! They need Bazel, so every test here is `#[ignore]`d and opted into by
//! hand — `cargo test` reports them as ignored rather than as passes that
//! ran nothing:
//!
//! ```sh
//! CAMBIUM_E2E=1 cargo test -p cambium-engine --test e2e_bazel -- --ignored
//! CAMBIUM_E2E=1 CAMBIUM_BAZEL=/path/to/bazelisk cargo test ... -- --ignored
//! ```
//!
//! Run with `--ignored` but without `CAMBIUM_E2E` or a Bazel to run against,
//! each test fails with a message saying so, the way
//! `crates/bench/tests/smoke_real_bazel.rs` does under `CAMBIUM_REQUIRE_TOOLS`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use cambium_engine::{AffectedRequest, Engine};
use cambium_index::{IndexData, IndexRef, NodeKind};
use cambium_testkit::{commit_all, git, init_repo, write};

fn enabled() -> bool {
    std::env::var("CAMBIUM_E2E").is_ok()
}

fn bazel_binary() -> Option<String> {
    std::env::var("CAMBIUM_BAZEL").ok()
}

/// Whether a Bazel is reachable: `CAMBIUM_BAZEL` names a file, or `bazel` or
/// `bazelisk` is on `PATH`, which is where `Engine::open` looks without it.
fn bazel_reachable() -> bool {
    if let Some(b) = bazel_binary() {
        return Path::new(&b).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join("bazel").is_file() || dir.join("bazelisk").is_file())
}

/// The second guard behind `#[ignore]`: a test that was asked to run, with
/// `--ignored`, on a machine that cannot run it fails and says why, rather
/// than passing quietly.
fn require_bazel() {
    assert!(
        enabled(),
        "this test drives a real Bazel: set CAMBIUM_E2E=1 (and CAMBIUM_BAZEL if bazel is not on PATH) and run with --ignored"
    );
    assert!(
        bazel_reachable(),
        "CAMBIUM_E2E is set but no Bazel is reachable: put bazel or bazelisk on PATH, or point CAMBIUM_BAZEL at one"
    );
}

fn engine_for(root: &Path, config: &Path) -> Engine {
    Engine::open(root, Some(config), bazel_binary().as_deref()).expect("opening the workspace")
}

// ------------------------------------------------------------ golden workspace

/// A small workspace exercising the scenarios spec 12.2 lists, with names a
/// human can reason about.
fn write_golden_workspace(root: &Path, store: &Path) {
    write(
        root,
        "MODULE.bazel",
        "module(name = \"golden\", version = \"0.0.1\")\n",
    );
    write(root, ".bazelversion", "7.4.1\n");
    // The cold build at base runs Bazel, which writes `MODULE.bazel.lock`
    // into the tree; the first scenario's `git add -A` would otherwise commit
    // it, and an added lockfile is a global change, so that scenario would
    // test the lockfile rule instead of its own edit.
    write(root, ".gitignore", "MODULE.bazel.lock\nbazel-*\n");
    write(root, "rules/BUILD.bazel", "");
    write(
        root,
        "rules/synth.bzl",
        r##""""Rules that do no real work."""

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "\n".join([f.path for f in ctx.files.srcs]) + "\n")
    return [DefaultInfo(files = depset([out]))]

_ATTRS = {
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "mode": attr.string(default = "debug"),
}

mylib = rule(implementation = _impl, attrs = _ATTRS)

def _test_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [DefaultInfo(executable = out, files = depset([out]))]

my_test = rule(implementation = _test_impl, attrs = _ATTRS, test = True)
"##,
    );
    // A .bzl loaded by two packages, so an edit to it must reach both.
    write(
        root,
        "lib/defs.bzl",
        "load(\"//rules:synth.bzl\", \"mylib\", \"my_test\")\n\nSHARED = \"v1\"\n\n\
         def shared_lib(**kwargs):\n    mylib(**kwargs)\n\n\
         def shared_test(**kwargs):\n    my_test(**kwargs)\n",
    );
    write(
        root,
        "lib/BUILD.bazel",
        "load(\"//lib:defs.bzl\", \"shared_lib\")\n\n\
         shared_lib(\n    name = \"core\",\n    srcs = glob([\"**/*.cc\"]),\n    \
         deps = [\"//tools:helper\"],\n    visibility = [\"//visibility:public\"],\n)\n",
    );
    write(root, "lib/a.cc", "// a\n");
    write(root, "lib/nested/deep.cc", "// deep\n");

    write(
        root,
        "app/BUILD.bazel",
        "load(\"//lib:defs.bzl\", \"shared_lib\", \"shared_test\")\n\n\
         alias(\n    name = \"core_alias\",\n    actual = \"//lib:core\",\n)\n\n\
         shared_lib(\n    name = \"main\",\n    srcs = glob([\"*.cc\"]),\n    \
         deps = [\":core_alias\"] + select({\n        \"//tools:opt\": [\"//tools:helper\"],\n        \
         \"//conditions:default\": [],\n    }),\n)\n\n\
         shared_test(\n    name = \"main_test\",\n    deps = [\":main\"],\n)\n\n\
         test_suite(\n    name = \"all_tests\",\n    tests = [\":main_test\"],\n)\n",
    );
    write(root, "app/main.cc", "// main\n");

    write(
        root,
        "tools/BUILD.bazel",
        "load(\"//rules:synth.bzl\", \"mylib\")\n\n\
         config_setting(\n    name = \"opt\",\n    values = {\"compilation_mode\": \"opt\"},\n    \
         visibility = [\"//visibility:public\"],\n)\n\n\
         exports_files([\"data.txt\"])\n\n\
         mylib(\n    name = \"helper\",\n    srcs = [\"helper.cc\"],\n    \
         visibility = [\"//visibility:public\"],\n)\n",
    );
    write(root, "tools/helper.cc", "// helper\n");
    write(root, "tools/data.txt", "data v1\n");

    // A consumer of another package's exported file.
    write(
        root,
        "other/BUILD.bazel",
        "load(\"//rules:synth.bzl\", \"mylib\")\n\n\
         mylib(\n    name = \"reader\",\n    srcs = [\"//tools:data.txt\"],\n)\n",
    );

    write(
        root,
        "cambium.toml",
        &format!(
            "[invalidators]\n\
             paths = [\"MODULE.bazel\", \".bazelversion\", \"BUILD\", \"BUILD.bazel\"]\n\
             globs = [\"rules/**\"]\n\n\
             [ignore]\n\
             globs = [\"**/*.md\"]\n\n\
             [baseline]\n\
             store = {{ kind = \"dir\", path = \"{}\" }}\n",
            store.display()
        ),
    );
}

struct Golden {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    base: String,
    base_index: IndexData,
}

impl Golden {
    fn setup() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = root.join("_store");
        std::fs::create_dir_all(&store).unwrap();
        write_golden_workspace(&root, &store);
        init_repo(&root);
        let base = commit_all(&root, "base");

        let config = root.join("cambium.toml");
        let engine = engine_for(&root, &config);
        let base_index = engine.full_build(&base).expect("cold build at base");
        Golden {
            _dir: dir,
            root,
            config,
            base,
            base_index,
        }
    }

    /// Applies an edit, commits it, and returns the affected report.
    fn scenario(&self, name: &str, edit: impl FnOnce(&Path)) -> cambium_engine::AffectedReport {
        edit(&self.root);
        let head = commit_all(&self.root, name);
        let engine = engine_for(&self.root, &self.config);
        let req = AffectedRequest {
            base: self.base.clone(),
            head: head.clone(),
            include_manual: true,
            // On always, so a scenario can assert *why* a target was reported
            // and not merely that it was.
            explain: true,
            no_full_build: true,
            materialize: true,
            publish_base: false,
            max_depth: None,
            allow_partial: false,
        };
        let out = engine
            .compute(
                self.base_index.view(),
                &engine.repo.diff(&self.base, &head).expect("git diff"),
                &engine.bazel_querier(&head).expect("querier for head"),
                &req,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        // Every scenario is also checked against the legacy algorithm this
        // design replaces: rebuild the whole graph at head and diff deep
        // hashes. That is the ground truth, and it needs no hand-written
        // expectations to go stale.
        if !out.report.global {
            let head_index = engine.full_build(&head).expect("full build at head");
            let legacy =
                legacy_affected(&self.root, &self.base, &head, &self.base_index, &head_index);
            let ours: BTreeSet<String> = out.report.affected_labels().into_iter().collect();
            let missing: Vec<&String> = legacy.difference(&ours).collect();
            assert!(
                missing.is_empty(),
                "{name}: the engine missed {} targets the full rebuild reports: {missing:?}",
                missing.len()
            );
        }

        self.reset();
        out.report
    }

    fn reset(&self) {
        git(&self.root, &["reset", "-q", "--hard", &self.base]);
        git(&self.root, &["clean", "-qfd"]);
    }
}

/// The approach cambium replaces: hash every target together with the closure
/// of its inputs at both revisions, and diff.
///
/// Deliberately a second implementation, not a call into
/// `crates/bench/src/legacy.rs`, which does the same thing for the comparison
/// suite. Its whole job here is to be an independent oracle: if the two shared
/// code, a bug in the shared half would agree with itself and these assertions
/// would pass through it. This one is the short, obvious version — no caching,
/// no incrementality — and it is only ever run over the tiny golden workspace,
/// where being slow costs nothing.
fn legacy_affected(
    root: &Path,
    base_rev: &str,
    head_rev: &str,
    base: &IndexData,
    head: &IndexData,
) -> BTreeSet<String> {
    let b = deep_hashes(root, base_rev, base.view());
    let h = deep_hashes(root, head_rev, head.view());
    h.into_iter()
        .filter(|(label, hash)| b.get(label) != Some(hash))
        .map(|(label, _)| label)
        .collect()
}

fn deep_hashes(root: &Path, rev: &str, v: IndexRef<'_>) -> BTreeMap<String, [u8; 32]> {
    let repo = cambium_diff::Repo::new(root.to_path_buf());
    let n = v.num_nodes() as usize;
    let mut memo: Vec<Option<[u8; 32]>> = vec![None; n];

    // Iterative post-order so a deep graph cannot blow the stack.
    let mut out = BTreeMap::new();
    for root_id in 0..n as u32 {
        if memo[root_id as usize].is_some() {
            continue;
        }
        let mut stack = vec![(root_id, false)];
        while let Some((id, expanded)) = stack.pop() {
            if memo[id as usize].is_some() {
                continue;
            }
            if !expanded {
                stack.push((id, true));
                for &d in v.forward.row(id) {
                    if memo[d as usize].is_none() {
                        stack.push((d, false));
                    }
                }
                continue;
            }
            let mut hasher = blake3::Hasher::new();
            hasher.update(v.label(id).as_bytes());
            hasher.update(&v.hash[id as usize]);
            match v.kind_of(id) {
                NodeKind::SourceFile => {
                    // File contents are an input; the rule hash does not cover them.
                    if let Some(path) = cambium_index::label::source_path(v.label(id)) {
                        if let Some(bytes) = repo.read_at(rev, &path) {
                            hasher.update(blake3::hash(&bytes).as_bytes());
                        }
                    }
                }
                _ => {
                    // Sorted by label, not by id: ids are assigned differently
                    // in a spliced index and a fresh build, and the hash has to
                    // describe the graph, not the numbering.
                    let mut deps: Vec<(&str, [u8; 32])> = Vec::new();
                    for &d in v.forward.row(id) {
                        // A cycle would leave a dep unresolved; Bazel's graph
                        // has none, and falling back to the label keeps this
                        // terminating rather than wrong.
                        let digest = memo[d as usize]
                            .unwrap_or_else(|| *blake3::hash(v.label(d).as_bytes()).as_bytes());
                        deps.push((v.label(d), digest));
                    }
                    deps.sort_unstable();
                    for (_, d) in deps {
                        hasher.update(&d);
                    }
                }
            }
            let digest = *hasher.finalize().as_bytes();
            memo[id as usize] = Some(digest);
            if v.kind_of(id) == NodeKind::Rule && !v.is_tombstone(id) {
                out.insert(v.label(id).to_string(), digest);
            }
        }
    }
    out
}

fn set(report: &cambium_engine::AffectedReport) -> BTreeSet<&str> {
    report.affected.iter().map(|t| t.label.as_str()).collect()
}

// ------------------------------------------------------------------ scenarios

/// The golden workspace, built once.
///
/// Setting it up is a real `bazel query` over the whole workspace, which is
/// most of what these tests cost; every scenario then edits the checkout,
/// commits, asserts, and resets to base. That shared mutable checkout is why
/// the fixture is behind a mutex rather than merely a `OnceLock`: the tests
/// are one per scenario, so a failure names the scenario that failed, but they
/// still have to take their turn at the repository.
fn golden() -> std::sync::MutexGuard<'static, Golden> {
    static GOLDEN: std::sync::OnceLock<std::sync::Mutex<Golden>> = std::sync::OnceLock::new();
    let m = GOLDEN.get_or_init(|| std::sync::Mutex::new(Golden::setup()));
    // A scenario that panics mid-edit poisons the lock and leaves the checkout
    // dirty; the next test resets it rather than inheriting the mess, so one
    // failure does not cascade into twelve.
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            let g = poisoned.into_inner();
            g.reset();
            g
        }
    }
}

/// The whole chain from `//lib:core` up: the alias, the app, its test, and the
/// suite that lists it.
fn downstream() -> BTreeSet<&'static str> {
    [
        "//lib:core",
        "//app:core_alias",
        "//app:main",
        "//app:main_test",
        "//app:all_tests",
    ]
    .into_iter()
    .collect()
}

/// Editing a source file reaches its consumers, and nothing else.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_modified_source_reaches_its_consumers() {
    require_bazel();
    let g = golden();
    let r = g.scenario("modify source", |root| {
        write(root, "lib/a.cc", "// a v2\n");
    });
    assert_eq!(set(&r), downstream(), "modify source");
    assert!(
        r.dirty_packages.is_empty(),
        "a content edit dirties no package"
    );
    assert_eq!(r.tests, vec!["//app:all_tests", "//app:main_test"]);
}

/// A file added under an existing glob changes the target's inputs.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_file_added_under_a_glob_changes_the_targets_inputs() {
    require_bazel();
    let g = golden();
    let r = g.scenario("add globbed source", |root| {
        write(root, "lib/b.cc", "// b\n");
    });
    assert_eq!(set(&r), downstream(), "add globbed source");
    assert_eq!(r.dirty_packages, vec!["lib"]);
}

/// ...and deleting one does too.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_deleted_globbed_source_does_too() {
    require_bazel();
    let g = golden();
    let r = g.scenario("delete source", |root| {
        std::fs::remove_file(root.join("lib/nested/deep.cc")).unwrap();
    });
    assert_eq!(set(&r), downstream(), "delete source");
}

/// Renaming a file is a delete plus an add.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_renamed_source_is_a_delete_plus_an_add() {
    require_bazel();
    let g = golden();
    let r = g.scenario("rename source", |root| {
        std::fs::rename(root.join("lib/a.cc"), root.join("lib/renamed.cc")).unwrap();
    });
    assert_eq!(set(&r), downstream(), "rename source");
}

/// An attribute-only BUILD edit moves that target's hash and nothing below it.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn an_attribute_only_build_edit_moves_one_targets_hash() {
    require_bazel();
    let g = golden();
    let r = g.scenario("edit build attribute", |root| {
        let p = root.join("app/BUILD.bazel");
        let s = std::fs::read_to_string(&p).unwrap();
        std::fs::write(
            &p,
            s.replace(
                "shared_lib(\n    name = \"main\",",
                "shared_lib(\n    name = \"main\",\n    mode = \"opt\",",
            ),
        )
        .unwrap();
    });
    assert_eq!(
        set(&r),
        ["//app:main", "//app:main_test", "//app:all_tests"]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "edit build attribute"
    );
}

/// A new target in a package affects only itself.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_new_target_affects_only_itself() {
    require_bazel();
    let g = golden();
    let r = g.scenario("add target", |root| {
        let p = root.join("app/BUILD.bazel");
        let s = std::fs::read_to_string(&p).unwrap();
        std::fs::write(
            &p,
            format!("{s}\nshared_lib(\n    name = \"extra\",\n    deps = [\":main\"],\n)\n"),
        )
        .unwrap();
    });
    assert_eq!(
        set(&r),
        ["//app:extra"].into_iter().collect::<BTreeSet<_>>()
    );
}

/// Deleting a target makes its consumers direct.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_deleted_target_makes_its_consumers_direct() {
    require_bazel();
    let g = golden();
    let r = g.scenario("delete target", |root| {
        let p = root.join("app/BUILD.bazel");
        let s = std::fs::read_to_string(&p).unwrap();
        // Drop the test_suite and the test it lists.
        let s = s.replace(
            "shared_test(\n    name = \"main_test\",\n    deps = [\":main\"],\n)\n\n",
            "",
        );
        let s = s.replace(
            "test_suite(\n    name = \"all_tests\",\n    tests = [\":main_test\"],\n)\n",
            "",
        );
        std::fs::write(&p, s).unwrap();
    });
    assert!(
        !set(&r).contains("//app:main_test"),
        "a deleted target is not affected: {:?}",
        r.affected
    );
}

/// A .bzl edit reaches every package that loads it.
///
/// The edited constant is used by nothing, so every rule proto in the
/// loading packages comes back byte-identical and no rule hash moves: Bazel
/// folds a rule's *definition* environment into the proto, not the loading
/// BUILD file's load closure. (Verified against Bazel 7.4.1.) So this
/// scenario isolates the only mechanism that can catch a changed rule
/// implementation — every rule in a package the static load closure names
/// is direct regardless of its hash — and the reason assertion below keeps it
/// isolated. Should it ever start passing through the rule-hash path
/// instead, that assertion fails and says so, rather than the coverage
/// quietly evaporating.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_bzl_edit_reaches_every_package_that_loads_it() {
    require_bazel();
    let g = golden();
    let r = g.scenario("edit bzl", |root| {
        let p = root.join("lib/defs.bzl");
        let s = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, s.replace("SHARED = \"v1\"", "SHARED = \"v2\"")).unwrap();
    });
    let affected = set(&r);
    for t in &downstream() {
        assert!(
            affected.contains(t),
            "edit bzl should reach {t}: {affected:?}"
        );
    }
    assert!(
        r.dirty_packages.contains(&"lib".to_string())
            && r.dirty_packages.contains(&"app".to_string()),
        "the load closure should report both loading packages: {:?}",
        r.dirty_packages
    );
    let explanations = r
        .explanations
        .as_ref()
        .expect("explanations were requested");
    assert!(!explanations.is_empty(), "edit bzl reported nothing");
    for e in explanations {
        assert!(
            matches!(
                &e.reason,
                cambium_engine::DirectReason::BzlClosure { package }
                    if package == "lib" || package == "app"
            ),
            "edit bzl: {} was reported because {:?}. No rule hash moves for this \
             edit, so the bzl-closure branch is the only thing that can catch \
             it; any other reason means this scenario has stopped covering it.",
            e.target,
            e.reason
        );
    }
}

/// A file exported by one package and consumed by another.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_file_exported_by_one_package_and_consumed_by_another() {
    require_bazel();
    let g = golden();
    let r = g.scenario("modify exported file", |root| {
        write(root, "tools/data.txt", "data v2\n");
    });
    assert_eq!(
        set(&r),
        ["//other:reader"].into_iter().collect::<BTreeSet<_>>(),
        "exports_files consumer"
    );
}

/// A select branch changing is a change to the target's inputs, because
/// --proto:flatten_selects reports the union of the branches.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_changed_select_branch_changes_the_targets_inputs() {
    require_bazel();
    let g = golden();
    let r = g.scenario("change select branch", |root| {
        let p = root.join("app/BUILD.bazel");
        let s = std::fs::read_to_string(&p).unwrap();
        std::fs::write(
            &p,
            s.replace(
                "\"//conditions:default\": [],",
                "\"//conditions:default\": [\"//tools:helper\"],",
            ),
        )
        .unwrap();
    });
    assert!(
        set(&r).contains("//app:main"),
        "select branch change: {:?}",
        r.affected
    );
}

/// Splitting a package: the parent's glob shrinks, so it changes too.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn splitting_a_package_dirties_the_parent_too() {
    require_bazel();
    let g = golden();
    let r = g.scenario("add sub package", |root| {
        write(
            root,
            "lib/nested/BUILD.bazel",
            "load(\"//rules:synth.bzl\", \"mylib\")\n\n\
             mylib(\n    name = \"deep\",\n    srcs = glob([\"*.cc\"]),\n    \
             visibility = [\"//visibility:public\"],\n)\n",
        );
    });
    let affected = set(&r);
    assert!(
        affected.contains("//lib:core"),
        "parent package loses globbed files"
    );
    assert!(
        affected.contains("//lib/nested:deep"),
        "new package's target is new"
    );
    assert!(
        r.dirty_packages.contains(&"lib".to_string())
            && r.dirty_packages.contains(&"lib/nested".to_string()),
        "both packages dirty: {:?}",
        r.dirty_packages
    );
}

/// A global invalidator short-circuits everything.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_global_invalidator_short_circuits_everything() {
    require_bazel();
    let g = golden();
    let r = g.scenario("edit MODULE.bazel", |root| {
        write(
            root,
            "MODULE.bazel",
            "module(name = \"golden\", version = \"0.0.2\")\n",
        );
    });
    assert!(r.global, "MODULE.bazel edit is global");
    assert_eq!(r.as_query_expression(false), "//...");
}

/// An ignored path changes nothing.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn an_ignored_path_changes_nothing() {
    require_bazel();
    let g = golden();
    let r = g.scenario("edit ignored path", |root| {
        write(root, "lib/README.md", "docs\n");
    });
    assert!(r.affected.is_empty(), "ignored path: {:?}", r.affected);
    assert!(!r.global);
}

/// Spec 12.3: our reverse closure must contain everything `bazel query rdeps`
/// reports. Extra targets are the accepted over-approximation.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn differential_check_against_bazel_rdeps() {
    require_bazel();
    let g = Golden::setup();
    let engine = engine_for(&g.root, &g.config);
    let v = g.base_index.view();

    let mut checked = 0;
    for id in 0..v.num_nodes() {
        if v.kind_of(id) != NodeKind::Rule || v.is_tombstone(id) {
            continue;
        }
        let label = v.label(id);
        let out = Command::new(engine.bazel.binary())
            .current_dir(engine.workspace())
            .args([
                "query",
                &format!("rdeps(//..., {label})"),
                "--output=label",
                "--order_output=no",
                "--noshow_progress",
                "--ui_event_filters=-info",
            ])
            .output()
            .expect("bazel query");
        assert!(
            out.status.success(),
            "bazel rdeps({label}) failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let theirs: BTreeSet<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let r = cambium_index::walk::walk(
            v,
            &[id],
            &cambium_index::WalkOptions {
                include_manual: true,
                explain: false,
                max_depth: None,
            },
        );
        let ours: BTreeSet<String> = cambium_index::view::labels_sorted(&v, &r.affected)
            .into_iter()
            .collect();
        let missing: Vec<&String> = theirs.difference(&ours).collect();
        assert!(
            missing.is_empty(),
            "rdeps({label}): the index is missing {missing:?}"
        );
        checked += 1;
    }
    assert!(
        checked > 5,
        "expected several rule targets, checked {checked}"
    );
}

/// A synthetic workspace, driven through real Bazel over a generated history.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn synthetic_workspace_through_real_bazel() {
    require_bazel();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = cambium_synth::generate(&cambium_synth::GraphSpec::preset("tiny").unwrap());
    let change = cambium_synth::ChangeSpec {
        seed: 11,
        ..cambium_synth::ChangeSpec::preset("mixed").unwrap()
    };
    let repo = cambium_synth::emit_git_history(graph, &change, 4, root, false)
        .expect("emitting the workspace");

    let config = root.join("cambium.toml");
    let engine = engine_for(root, &config);
    // The emitter leaves the tree at the last commit; a cold build reads the
    // working tree, so check the base out first.
    git(root, &["checkout", "-q", &repo.commits[0]]);
    let mut index = engine
        .full_build(&repo.commits[0])
        .expect("cold build of the emitted workspace");

    for i in 1..repo.commits.len() {
        let (base, head) = (&repo.commits[i - 1], &repo.commits[i]);
        git(root, &["checkout", "-q", head]);
        let req = AffectedRequest {
            base: base.clone(),
            head: head.clone(),
            include_manual: true,
            explain: false,
            no_full_build: true,
            materialize: true,
            publish_base: false,
            max_depth: None,
            allow_partial: false,
        };
        let out = engine
            .run_incremental(index.view(), &req)
            .unwrap_or_else(|e| panic!("commit {i}: {e}"));
        if out.report.global {
            index = engine.full_build(head).unwrap();
            continue;
        }

        let fresh = engine.full_build(head).expect("full build at head");
        let legacy = legacy_affected(root, base, head, &index, &fresh);
        let ours: BTreeSet<String> = out.report.affected_labels().into_iter().collect();
        let missing: Vec<&String> = legacy.difference(&ours).collect();
        assert!(
            missing.is_empty(),
            "commit {i}: engine missed {} targets the full rebuild reports: {:?}",
            missing.len(),
            missing.iter().take(5).collect::<Vec<_>>()
        );

        // The spliced index and a full rebuild must describe the same graph.
        let spliced = out.index.expect("a non-global run produces an index");
        assert_same_edges(&spliced, &fresh, i);
        index = spliced;
    }
}

fn assert_same_edges(spliced: &IndexData, fresh: &IndexData, step: usize) {
    let edges = |v: IndexRef<'_>| -> BTreeSet<(String, String)> {
        let mut out = BTreeSet::new();
        for id in 0..v.num_nodes() {
            if v.is_tombstone(id) || v.kind_of(id) == NodeKind::Unknown {
                continue;
            }
            for &d in v.forward.row(id) {
                out.insert((v.label(id).to_string(), v.label(d).to_string()));
            }
        }
        out
    };
    let (a, b) = (edges(spliced.view()), edges(fresh.view()));
    let only_spliced: Vec<_> = a.difference(&b).take(5).collect();
    let only_fresh: Vec<_> = b.difference(&a).take(5).collect();
    assert!(
        only_spliced.is_empty() && only_fresh.is_empty(),
        "step {step}: spliced-only {only_spliced:?}, rebuild-only {only_fresh:?}"
    );
}

// ------------------------------------------------- external repositories

/// A bzlmod workspace whose external repositories are built from main-repo
/// files, both ways envoy does it:
///
/// - `mod_a/` is a `local_path_override` module, `.bazelignore`d out of the
///   main repository exactly as envoy hides `api/` behind `@envoy_api`;
/// - `@gen` is a `use_repo_rule` repository whose rule reads `config.txt`
///   through a label attribute, exactly as `@envoy_repo` reads
///   `.github/config.yml`, and whose generated BUILD file lists `//data` in
///   a target's `srcs`, exactly as `@envoy_repo//:project` lists
///   `@envoy//changelogs` in its `tools`.
///
/// Neither file belongs to a main-repo package, so before the external stage
/// existed the only sound answer for either was "everything".
fn write_external_workspace(root: &Path, store: &Path, bazel_version: &str) {
    write(root, ".bazelversion", &format!("{bazel_version}\n"));
    // `mod_a` is a module of its own; the main repository must not also load
    // it as a package, or its targets would be `//mod_a:...`.
    write(root, ".bazelignore", "mod_a\n");
    write(
        root,
        "MODULE.bazel",
        "module(name = \"root\", version = \"0.0.1\")\n\
         bazel_dep(name = \"mod_a\", version = \"0.0.1\")\n\
         local_path_override(\n    module_name = \"mod_a\",\n    path = \"mod_a\",\n)\n\
         gen_repo = use_repo_rule(\"//tools:gen.bzl\", \"gen_repo\")\n\
         gen_repo(\n    name = \"gen\",\n    config = \"@root//:config.txt\",\n)\n",
    );
    // The cold build at base runs Bazel, which writes the lockfile into the
    // tree; committing it with the *next* edit would make every scenario a
    // lockfile change. Repositories that track their lockfile commit it once,
    // at the same time as `MODULE.bazel`.
    write(root, ".gitignore", "MODULE.bazel.lock\nbazel-*\n");
    write(root, "config.txt", "v1\n");
    write(root, "BUILD.bazel", "exports_files([\"config.txt\"])\n");
    // The repository rule's own `.bzl` is *also* listed as a source of a
    // filegroup, the shape `bazel/bazel` uses throughout `tools/`
    // (`//tools/build_defs/repo:bzl_srcs` and friends list
    // `tools/build_defs/repo/utils.bzl`, which `MODULE.bazel` also loads).
    write(
        root,
        "tools/BUILD.bazel",
        "filegroup(\n    name = \"bzl_srcs\",\n    srcs = [\"gen.bzl\"],\n\
        \x20   visibility = [\"//visibility:public\"],\n)\n",
    );
    write(
        root,
        "tools/gen.bzl",
        "def _impl(rctx):\n\
        \x20   value = rctx.read(rctx.path(rctx.attr.config))\n\
        \x20   rctx.file(\"value.txt\", value)\n\
        \x20   rctx.file(\n\
        \x20       \"BUILD\",\n\
        \x20       'filegroup(name = \"value\", srcs = [\"value.txt\", \"@root//data\"], visibility = [\"//visibility:public\"])\\n',\n\
        \x20   )\n\n\
         gen_repo = repository_rule(\n\
        \x20   implementation = _impl,\n\
        \x20   attrs = {\"config\": attr.label(mandatory = True)},\n\
         )\n",
    );
    write(
        root,
        "tools/macros.bzl",
        "load(\"@mod_a//:defs.bzl\", _via_macro = \"via_macro\")\n\n\
         via_macro = _via_macro\n",
    );
    // What `@gen//:value`'s generated `srcs` names: a file the repository
    // rule never reads.
    write(
        root,
        "data/BUILD.bazel",
        "filegroup(name = \"data\", srcs = [\"notes.txt\"], visibility = [\"//visibility:public\"])\n",
    );
    write(root, "data/notes.txt", "n1\n");
    write(
        root,
        "mod_a/MODULE.bazel",
        "module(name = \"mod_a\", version = \"0.0.1\")\n",
    );
    write(
        root,
        "mod_a/BUILD.bazel",
        "filegroup(name = \"lib\", srcs = [\"lib.txt\"], visibility = [\"//visibility:public\"])\n",
    );
    write(root, "mod_a/lib.txt", "a1\n");
    // The `@envoy_api//bazel:api_build_system.bzl` shape: a `.bzl` inside the
    // overridden module that a main-repo package loads. Editing `lib.txt` must
    // not re-query the packages that load this file; editing *this* file must.
    write(
        root,
        "mod_a/defs.bzl",
        "def via_macro(name):\n\
        \x20   native.genrule(name = name, outs = [name + \".txt\"], cmd = \"echo v1 > $@\")\n",
    );
    write(
        root,
        "app/BUILD.bazel",
        "load(\"//tools:macros.bzl\", \"via_macro\")\n\n\
         via_macro(name = \"via_macro\")\n\n\
         genrule(\n    name = \"uses_gen\",\n    srcs = [\"@gen//:value\"],\n\
        \x20   outs = [\"g.txt\"],\n    cmd = \"cat $(SRCS) > $@\",\n)\n\n\
         genrule(\n    name = \"uses_mod_a\",\n    srcs = [\"@mod_a//:lib\"],\n\
        \x20   outs = [\"m.txt\"],\n    cmd = \"cat $(SRCS) > $@\",\n)\n\n\
         genrule(\n    name = \"downstream\",\n    srcs = [\":g.txt\"],\n\
        \x20   outs = [\"d.txt\"],\n    cmd = \"cat $(SRCS) > $@\",\n)\n\n\
         genrule(\n    name = \"uses_bzl_srcs\",\n    srcs = [\"//tools:bzl_srcs\"],\n\
        \x20   outs = [\"b.txt\"],\n    cmd = \"cat $(SRCS) > $@\",\n)\n\n\
         genrule(\n    name = \"unrelated\",\n    outs = [\"u.txt\"],\n    cmd = \"echo hi > $@\",\n)\n",
    );
    write(
        root,
        "cambium.toml",
        &format!(
            "[baseline]\nstore = {{ kind = \"dir\", path = \"{}\" }}\n",
            store.display()
        ),
    );
}

/// Main-repo targets `bazel query rdeps(//..., <pattern>)` reports.
fn rdeps_oracle(engine: &Engine, pattern: &str) -> BTreeSet<String> {
    let out = Command::new(engine.bazel.binary())
        .current_dir(engine.workspace())
        .args([
            "query",
            // Rules only: Bazel counts a generated file among a target's
            // reverse dependencies, cambium reports the rule that generates it.
            &format!("kind(rule, rdeps(//..., {pattern}))"),
            "--output=label",
            "--order_output=no",
            "--noshow_progress",
            "--ui_event_filters=-info",
        ])
        .output()
        .expect("bazel query");
    assert!(
        out.status.success(),
        "bazel rdeps({pattern}) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| s.starts_with("//"))
        .map(str::to_string)
        .collect()
}

#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn an_external_repository_edit_reaches_only_its_consumers() {
    require_bazel();
    // The two majors the benchmark and CI care about. Selected through
    // `.bazelversion` rather than `USE_BAZEL_VERSION` so that parallel tests
    // do not fight over one process-wide environment variable.
    for version in ["7.4.1", "9.2.0"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = root.join("_store");
        std::fs::create_dir_all(&store).unwrap();
        write_external_workspace(&root, &store, version);
        init_repo(&root);
        let base = commit_all(&root, "base");

        let config = root.join("cambium.toml");
        let engine = engine_for(&root, &config);
        let base_index = engine
            .full_build(&base)
            .unwrap_or_else(|e| panic!("{version}: cold build: {e}"));

        // The index knows which main-repo paths feed which repository.
        let v = base_index.view();
        let named = |p: &str| -> BTreeSet<String> {
            v.repos_for_path(p)
                .into_iter()
                .map(|r| v.repo_name(r).to_string())
                .collect()
        };
        assert_eq!(
            named("config.txt"),
            ["gen".to_string()].into_iter().collect::<BTreeSet<_>>(),
            "{version}: the repository rule's label attribute is an input map entry"
        );
        assert_eq!(
            named("mod_a/lib.txt"),
            ["mod_a".to_string()].into_iter().collect::<BTreeSet<_>>(),
            "{version}: a local_path_override directory is an input map entry"
        );
        // `@root//data` in the generated BUILD file is a rule-shaped label:
        // the target `//data:data`, not the `data/` subtree. It is stored as
        // a label pattern the graph answers, so no path under `data/` is in
        // the path map at all — the edit below reaches `@gen` through
        // `//data:data`'s own edge instead.
        assert_eq!(
            v.repos_for_label_kinds("//data:data")
                .into_iter()
                .map(|(r, feeds)| (v.repo_name(r).to_string(), feeds))
                .collect::<Vec<_>>(),
            vec![("gen".to_string(), false)],
            "{version}: a target only the generated BUILD file names is target-only"
        );
        assert!(
            v.repos_for_path_kinds("data/notes.txt").is_empty(),
            "{version}: a file of a target the generated BUILD file names is not a path input"
        );

        let check = |name: &str, rel: &str, contents: &str, pattern: Option<&str>| {
            write(&root, rel, contents);
            let head = commit_all(&root, name);
            let engine = engine_for(&root, &config);
            let req = AffectedRequest {
                base: base.clone(),
                head: head.clone(),
                include_manual: true,
                explain: true,
                no_full_build: true,
                materialize: false,
                publish_base: false,
                max_depth: None,
                allow_partial: false,
            };
            let out = engine
                .compute(
                    base_index.view(),
                    &engine.repo.diff(&base, &head).expect("git diff"),
                    &engine.bazel_querier(&head).expect("querier"),
                    &req,
                )
                .unwrap_or_else(|e| panic!("{version}/{name}: {e}"));
            assert!(
                !out.report.global,
                "{version}/{name}: went global: {:?}",
                out.report.global_reason
            );
            if let Some(pattern) = pattern {
                let ours: BTreeSet<String> = out.report.affected_labels().into_iter().collect();
                let theirs = rdeps_oracle(&engine, pattern);
                assert_eq!(
                    ours, theirs,
                    "{version}/{name}: cambium and `rdeps(//..., {pattern})` disagree"
                );
            }
            git(&root, &["reset", "-q", "--hard", &base]);
            git(&root, &["clean", "-qfd"]);
            out.report
        };
        let labels = |r: &cambium_engine::AffectedReport| -> BTreeSet<String> {
            r.affected_labels().into_iter().collect()
        };

        // The `@envoy_repo` shape: a file a repository rule reads by label.
        let gen = labels(&check(
            "edit config",
            "config.txt",
            "v2\n",
            Some("@gen//:value"),
        ));
        assert!(
            gen.contains("//app:uses_gen") && gen.contains("//app:downstream"),
            "{version}: {gen:?}"
        );
        assert!(!gen.contains("//app:unrelated"), "{version}: {gen:?}");

        // The other half of the `@envoy_repo` shape: a file the rule never
        // reads, listed in the `srcs` of a target it *writes*. Bazel's own
        // edge runs `//data:data` -> `@gen//:value` -> `//app:uses_gen`, and
        // that is the whole answer: nothing the rule writes can have moved,
        // so the package whose closure loads from `@gen` is not re-queried.
        let n_report = check(
            "edit data",
            "data/notes.txt",
            "n2\n",
            Some("//data:notes.txt + @gen//:value"),
        );
        let n = labels(&n_report);
        assert!(
            n.contains("//data:data")
                && n.contains("//app:uses_gen")
                && n.contains("//app:downstream"),
            "{version}: {n:?}"
        );
        assert!(!n.contains("//app:unrelated"), "{version}: {n:?}");
        // Nothing in the diff named a repository; the walk did, when it
        // reached `//data:data`.
        assert_eq!(
            n_report.stats.repos_dirty_by_input, 0,
            "{version}: a file of a target the generated BUILD file names is no path input"
        );
        assert_eq!(
            n_report.stats.repos_dirty_by_target, 1,
            "{version}: the walk reaching `//data:data` is what moves `@gen`"
        );
        assert!(
            !n_report.dirty_packages.contains(&"app".to_string()),
            "{version}: `app` must not be re-queried: {:?}",
            n_report.dirty_packages
        );

        // The `@envoy_api` shape: a `local_path_override` module's sources.
        //
        // `mod_a/lib.txt` is `lib.txt` inside `@mod_a`, and nothing loads that,
        // so the answer is exactly the label consumers — even though `//app`'s
        // load closure does reach `@mod_a//:defs.bzl`. That is issue #3: the
        // package is not re-queried at all.
        let a_report = check("edit mod_a", "mod_a/lib.txt", "a2\n", Some("@mod_a//:lib"));
        let a = labels(&a_report);
        assert!(a.contains("//app:uses_mod_a"), "{version}: {a:?}");
        assert!(!a.contains("//app:uses_gen"), "{version}: {a:?}");
        assert_eq!(
            a_report.stats.repos_label_only, 1,
            "{version}: an input edit that cannot be a loaded .bzl spares rule (a)"
        );
        assert!(
            a_report.dirty_packages.is_empty(),
            "{version}: no package should have been re-queried: {:?}",
            a_report.dirty_packages
        );

        // And the same repository's `.bzl`, which *is* loaded: the package
        // whose closure reaches it is re-queried, and the macro's new `cmd`
        // moves the rule it generates.
        let d_report = check(
            "edit mod_a defs.bzl",
            "mod_a/defs.bzl",
            "def via_macro(name):\n\
            \x20   native.genrule(name = name, outs = [name + \".txt\"], cmd = \"echo v2 > $@\")\n",
            None,
        );
        assert_eq!(d_report.stats.repos_label_only, 0, "{version}");
        assert!(
            d_report.dirty_packages.contains(&"app".to_string()),
            "{version}: {:?}",
            d_report.dirty_packages
        );
        let d = labels(&d_report);
        assert!(d.contains("//app:via_macro"), "{version}: {d:?}");
        assert!(!d.contains("//app:unrelated"), "{version}: {d:?}");

        // A `.bzl` that is *both* in the `MODULE.bazel` workspace closure and
        // a source of a filegroup. Both consequences are owed: the
        // repositories the file defines move, and the targets that list it in
        // `srcs` change content without any rule proto moving. cambium missed
        // the second on `bazel/bazel`'s `9a7ba209..cc5c15fa`, where fifteen
        // filegroups around `tools/build_defs/repo/utils.bzl` went unreported.
        let gen_bzl = std::fs::read_to_string(root.join("tools/gen.bzl")).unwrap();
        let b = labels(&check(
            "edit gen.bzl",
            "tools/gen.bzl",
            &format!("{gen_bzl}\n# the file's text moved; no rule's did\n"),
            Some("@gen//:value + //tools:bzl_srcs"),
        ));
        assert!(
            b.contains("//tools:bzl_srcs") && b.contains("//app:uses_bzl_srcs"),
            "{version}: a .bzl listed in srcs must reach its consumers: {b:?}"
        );
        assert!(
            b.contains("//app:uses_gen") && b.contains("//app:downstream"),
            "{version}: without losing the repository consequence: {b:?}"
        );
        assert!(!b.contains("//app:unrelated"), "{version}: {b:?}");
    }
}

/// Issue #4: the same `@mod_a//:defs.bzl` edit, reached by a BUILD file's
/// *own* `load()` rather than through a main-repo `.bzl` intermediary.
///
/// `//app` above goes through `//tools:macros.bzl`, and the external half of
/// a `.bzl`'s loads has always been recorded. The external half of a BUILD
/// file's loads was not, so a package in this shape — envoy's `bazel/coverage`,
/// `bazel/external` and the root package — was invisible to rule (a).
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_build_files_direct_external_load_reaches_its_package() {
    require_bazel();
    for version in ["7.4.1", "9.2.0"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = root.join("_store");
        std::fs::create_dir_all(&store).unwrap();
        write_external_workspace(&root, &store, version);
        write(
            &root,
            "direct/BUILD.bazel",
            "load(\"@mod_a//:defs.bzl\", \"via_macro\")\n\n\
             via_macro(name = \"direct\")\n\n\
             genrule(\n    name = \"unrelated\",\n    outs = [\"u.txt\"],\n    cmd = \"echo hi > $@\",\n)\n",
        );
        init_repo(&root);
        let base = commit_all(&root, "base");

        let config = root.join("cambium.toml");
        let engine = engine_for(&root, &config);
        let base_index = engine
            .full_build(&base)
            .unwrap_or_else(|e| panic!("{version}: cold build: {e}"));

        write(
            &root,
            "mod_a/defs.bzl",
            "def via_macro(name):\n\
            \x20   native.genrule(name = name, outs = [name + \".txt\"], cmd = \"echo v2 > $@\")\n",
        );
        let head = commit_all(&root, "edit mod_a defs.bzl");
        let engine = engine_for(&root, &config);
        let req = AffectedRequest {
            base: base.clone(),
            head: head.clone(),
            include_manual: true,
            explain: true,
            no_full_build: true,
            materialize: false,
            publish_base: false,
            max_depth: None,
            allow_partial: false,
        };
        let out = engine
            .compute(
                base_index.view(),
                &engine.repo.diff(&base, &head).expect("git diff"),
                &engine.bazel_querier(&head).expect("querier"),
                &req,
            )
            .unwrap_or_else(|e| panic!("{version}: {e}"));
        let r = out.report;
        assert!(!r.global, "{version}: went global: {:?}", r.global_reason);
        for pkg in ["app", "direct"] {
            assert!(
                r.dirty_packages.contains(&pkg.to_string()),
                "{version}: `{pkg}` loads the edited .bzl and must be re-queried: {:?}",
                r.dirty_packages
            );
        }
        let d: BTreeSet<String> = r.affected_labels().into_iter().collect();
        assert!(d.contains("//direct:direct"), "{version}: {d:?}");
        assert!(d.contains("//app:via_macro"), "{version}: {d:?}");
        assert!(!d.contains("//direct:unrelated"), "{version}: {d:?}");
        assert!(!d.contains("//app:unrelated"), "{version}: {d:?}");
    }
}

// ---------------------------------------- shapes the published run found

/// The Bazel this workspace actually resolves to, as a major version.
///
/// `.bazelversion` is what these tests write, but `USE_BAZEL_VERSION` outranks
/// it in bazelisk and the version matrix sets exactly that, so the WORKSPACE
/// cases have to ask rather than assume: Bazel 9 removed `WORKSPACE` support
/// altogether and those cases are skipped there, not failed.
fn resolved_bazel_major(root: &Path) -> u32 {
    let out = Command::new(bazel_binary().unwrap_or_else(|| "bazel".to_string()))
        .current_dir(root)
        .args(["--version"])
        .output()
        .expect("bazel --version");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.split_whitespace()
        .find_map(|w| w.split('.').next().and_then(|n| n.parse::<u32>().ok()))
        .unwrap_or_else(|| panic!("no version in `bazel --version`: {text}"))
}

fn affected_request(base: &str, head: &str, allow_partial: bool) -> AffectedRequest {
    AffectedRequest {
        base: base.to_string(),
        head: head.to_string(),
        include_manual: true,
        explain: true,
        no_full_build: true,
        materialize: true,
        publish_base: false,
        max_depth: None,
        allow_partial,
    }
}

/// The externals side of "an applied index equals a fresh cold build".
///
/// [`assert_same_edges`] compares the graph; this compares the repository
/// tables beside it — the repositories and their path maps, the input
/// patterns and what they feed, each workspace `.bzl`'s answer, and every
/// package's external loads. `crates/bench/src/oracles/rebuild.rs` compares
/// the same tables for the benchmark's rebuild oracle; this is deliberately a
/// local re-expression, because the engine crate does not depend on bench.
fn assert_same_externals(spliced: &IndexData, fresh: &IndexData, what: &str) {
    let (a, b) = (spliced.view(), fresh.view());
    let mut bad: Vec<String> = Vec::new();
    assert_eq!(
        a.bzl_closure, b.bzl_closure,
        "{what}: bzl closure present: applied {}, rebuilt {}",
        a.bzl_closure, b.bzl_closure
    );
    if !a.bzl_closure {
        return;
    }

    let repos = |v: IndexRef<'_>| -> BTreeMap<String, (String, Vec<String>)> {
        (0..v.num_repos())
            .map(|r| {
                let maps = v
                    .repo_path_maps(r)
                    .into_iter()
                    .map(|m| format!("{} -> {}", m.from, m.to))
                    .collect();
                (
                    v.repo_name(r).to_string(),
                    (v.repo_canonical.get(r).to_string(), maps),
                )
            })
            .collect()
    };
    let patterns = |v: IndexRef<'_>| -> BTreeMap<String, (bool, Vec<String>)> {
        (0..v.repo_input_patterns.len())
            .map(|i| {
                let p = i as u32;
                let key = format!(
                    "{}{}",
                    v.repo_input_patterns.get(p),
                    if v.repo_input_prefix[i] { "**" } else { "" }
                );
                let feeds = v
                    .repo_inputs
                    .row(p)
                    .iter()
                    .map(|&r| v.repo_name(r).to_string())
                    .collect();
                (key, (v.repo_input_target_only[i], feeds))
            })
            .collect()
    };
    let bzl_rows = |v: IndexRef<'_>| -> BTreeMap<String, (Vec<String>, Vec<String>)> {
        (0..v.num_bzl())
            .map(|id| {
                let defines = v
                    .bzl_repos_of(id)
                    .iter()
                    .map(|&r| v.repo_name(r).to_string())
                    .collect();
                let loads = v.bzl_external(id).into_iter().map(str::to_string).collect();
                (v.bzl_path(id).to_string(), (defines, loads))
            })
            .collect()
    };
    let pkg_ext = |v: IndexRef<'_>| -> BTreeMap<String, Vec<String>> {
        (0..v.num_packages())
            .map(|p| {
                (
                    v.package_name(p).to_string(),
                    v.package_external(p)
                        .into_iter()
                        .map(str::to_string)
                        .collect::<Vec<String>>(),
                )
            })
            .filter(|(_, e)| !e.is_empty())
            .collect()
    };

    let mut compare = |table: &str, a: BTreeMap<String, String>, b: BTreeMap<String, String>| {
        for (k, va) in &a {
            match b.get(k) {
                None => bad.push(format!("{table} `{k}` only in the applied index")),
                Some(vb) if vb != va => {
                    bad.push(format!("{table} `{k}`: applied {va}, rebuilt {vb}"))
                }
                Some(_) => {}
            }
        }
        for k in b.keys() {
            if !a.contains_key(k) {
                bad.push(format!("{table} `{k}` only in the cold rebuild"));
            }
        }
    };
    fn dbg<V: std::fmt::Debug>(m: BTreeMap<String, V>) -> BTreeMap<String, String> {
        m.into_iter().map(|(k, v)| (k, format!("{v:?}"))).collect()
    }
    compare("repository", dbg(repos(a)), dbg(repos(b)));
    compare("input pattern", dbg(patterns(a)), dbg(patterns(b)));
    compare("workspace .bzl", dbg(bzl_rows(a)), dbg(bzl_rows(b)));
    compare("external loads of", dbg(pkg_ext(a)), dbg(pkg_ext(b)));
    assert!(bad.is_empty(), "{what}: {bad:#?}");
}

/// Shape A, from `apollo` and `xla`: a package Bazel can never load.
///
/// Both subjects carry packages that fail to load at every revision — a BUILD
/// file loading a `.bzl` from a repository that does not exist on the box
/// (apollo's `@local_config_cuda//cuda:build_defs.bzl`). `bazel query //...`
/// exits 3 under `--keep_going`, the index is flagged partial, and the
/// packages Bazel refused come back with no rows.
///
/// What broke: the incremental splice scanned every *dirty* package's BUILD
/// file for its load closure whether or not Bazel had returned rows for it,
/// while a cold build only ever sees the packages `//...` loads. The applied
/// index therefore carried external-load rows for a package neither index has
/// a single target for, and disagreed with a fresh cold build on the externals
/// tables — invisible to `assert_same_edges`, and caught only by apollo's
/// history walk eight hours into the published 2026-09-10 run.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_permanently_unloadable_package_keeps_no_load_closure() {
    require_bazel();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = root.join("_store");
    std::fs::create_dir_all(&store).unwrap();

    write(
        &root,
        "MODULE.bazel",
        "module(name = \"root\", version = \"0.0.1\")\n",
    );
    write(&root, ".gitignore", "MODULE.bazel.lock\nbazel-*\n");
    write(
        &root,
        "app/BUILD.bazel",
        "genrule(\n    name = \"app\",\n    srcs = [\"in.txt\"],\n\
        \x20   outs = [\"out.txt\"],\n    cmd = \"cat $(SRCS) > $@\",\n)\n",
    );
    write(&root, "app/in.txt", "v1\n");
    // The package Bazel can never load: it loads a `.bzl` from a repository
    // nothing declares. Its BUILD file is real enough to have a load closure,
    // which is exactly what must not survive into the applied index.
    write(
        &root,
        "broken/BUILD.bazel",
        "load(\"@no_such_repo//cuda:build_defs.bzl\", \"cuda_library\")\n\n\
         cuda_library(name = \"broken\")\n",
    );
    write(
        &root,
        "cambium.toml",
        &format!(
            "[baseline]\nstore = {{ kind = \"dir\", path = \"{}\" }}\n",
            store.display()
        ),
    );
    init_repo(&root);
    let base = commit_all(&root, "base");

    let config = root.join("cambium.toml");
    let engine = engine_for(&root, &config);
    let base_index = engine.full_build(&base).expect("cold build at base");
    assert!(
        base_index.view().partial,
        "the base index must record the package Bazel refused"
    );
    assert_eq!(base_index.view().error_packages, vec!["broken".to_string()]);

    // An edit elsewhere, and a touch of the unloadable package's own BUILD
    // file, in one commit: the second is what puts `broken` in the dirty set,
    // where the splice used to read its closure back in.
    write(&root, "app/in.txt", "v2\n");
    let broken = std::fs::read_to_string(root.join("broken/BUILD.bazel")).unwrap();
    write(
        &root,
        "broken/BUILD.bazel",
        &format!("{broken}# still unloadable\n"),
    );
    let head = commit_all(&root, "edit elsewhere, touch the broken package");

    let engine = engine_for(&root, &config);
    let out = engine
        .compute(
            base_index.view(),
            &engine.repo.diff(&base, &head).expect("git diff"),
            &engine.bazel_querier(&head).expect("querier for head"),
            &affected_request(&base, &head, true),
        )
        .expect("incremental run over a partial base");
    assert!(
        !out.report.global,
        "an edit beside an unloadable package is not global: {:?}",
        out.report.global_reason
    );
    let applied = out.index.expect("a non-global run produces an index");
    assert!(
        applied.view().partial,
        "a splice on a partial base is partial"
    );

    let fresh = engine.full_build(&head).expect("cold build at head");
    assert_same_edges(&applied, &fresh, 0);
    assert_same_externals(&applied, &fresh, "permanently unloadable package");
}

/// A WORKSPACE-mode tree that also carries an inert `MODULE.bazel`, the shape
/// `envoy` had at `8424aa0d`.
///
/// `.bazelrc` turns bzlmod off, `WORKSPACE` declares `@gen` from a repository
/// rule reading a main-repo file by label, and `MODULE.bazel` sits there
/// naming the same repository without Bazel ever reading a line of it.
fn write_workspace_mode_workspace(root: &Path, store: &Path, major: u32) {
    // `--enable_workspace` exists from Bazel 8; on 7 WORKSPACE is still the
    // fallback and the flag is not a flag.
    let rc = if major >= 8 {
        "common --noenable_bzlmod\ncommon --enable_workspace\n"
    } else {
        "common --noenable_bzlmod\n"
    };
    write(root, ".bazelrc", rc);
    write(root, ".gitignore", "MODULE.bazel.lock\nbazel-*\n");
    write(
        root,
        "WORKSPACE",
        "workspace(name = \"root\")\n\
         load(\"//tools:gen.bzl\", \"gen_repo\")\n\n\
         gen_repo(\n    name = \"gen\",\n    config = \"//:config.txt\",\n)\n",
    );
    // Never read by Bazel here, and it names `gen` the way envoy's module file
    // names `envoy_build_config`: a repository WORKSPACE builds for real.
    write(
        root,
        "MODULE.bazel",
        "module(name = \"root\", version = \"0.0.1\")\n\
         bazel_dep(name = \"gen\", version = \"0.0.1\")\n\
         local_path_override(\n    module_name = \"gen\",\n    path = \"gen_module\",\n)\n",
    );
    write(root, "config.txt", "v1\n");
    write(root, "BUILD.bazel", "exports_files([\"config.txt\"])\n");
    write(root, "tools/BUILD.bazel", "exports_files([\"gen.bzl\"])\n");
    write(
        root,
        "tools/gen.bzl",
        "def _impl(rctx):\n\
        \x20   value = rctx.read(rctx.path(rctx.attr.config))\n\
        \x20   rctx.file(\"value.txt\", value)\n\
        \x20   rctx.file(\n\
        \x20       \"BUILD\",\n\
        \x20       'filegroup(name = \"value\", srcs = [\"value.txt\"], visibility = [\"//visibility:public\"])\\n',\n\
        \x20   )\n\n\
         gen_repo = repository_rule(\n\
        \x20   implementation = _impl,\n\
        \x20   attrs = {\"config\": attr.label(mandatory = True)},\n\
         )\n",
    );
    write(
        root,
        "app/BUILD.bazel",
        "genrule(\n    name = \"uses_gen\",\n    srcs = [\"@gen//:value\"],\n\
        \x20   outs = [\"g.txt\"],\n    cmd = \"cat $(SRCS) > $@\",\n)\n\n\
         genrule(\n    name = \"downstream\",\n    srcs = [\":g.txt\"],\n\
        \x20   outs = [\"d.txt\"],\n    cmd = \"cat $(SRCS) > $@\",\n)\n\n\
         genrule(\n    name = \"unrelated\",\n    outs = [\"u.txt\"],\n    cmd = \"echo hi > $@\",\n)\n",
    );
    write(
        root,
        "cambium.toml",
        &format!(
            "[baseline]\nstore = {{ kind = \"dir\", path = \"{}\" }}\n",
            store.display()
        ),
    );
}

/// Shape B, from `envoy`: a `MODULE.bazel` Bazel never reads.
///
/// envoy at `8424aa0d` runs WORKSPACE mode by `.bazelrc` and still carries a
/// module file. The externals stage read that file regardless, so an edit to
/// it (or to its lock) dirtied every repository it mentions, overflowed
/// `max_dirty_packages` and forced a cold rebuild — 8 packages dirtied where
/// `rbuildfiles` names 1097, on a run that took hours to reach the commit.
/// Bazel does not read the file, so the honest answer is nothing at all.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn an_inert_module_file_in_workspace_mode_moves_nothing() {
    require_bazel();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = root.join("_store");
    std::fs::create_dir_all(&store).unwrap();
    let major = resolved_bazel_major(&root);
    if major >= 9 {
        eprintln!("skipping: Bazel {major} has no WORKSPACE support");
        return;
    }
    write_workspace_mode_workspace(&root, &store, major);
    init_repo(&root);
    let base = commit_all(&root, "base");

    let config = root.join("cambium.toml");
    let engine = engine_for(&root, &config);
    let base_index = engine.full_build(&base).expect("cold build at base");

    for (name, path, contents) in [
        (
            "edit MODULE.bazel",
            "MODULE.bazel",
            "module(name = \"root\", version = \"0.0.2\")\n\
             bazel_dep(name = \"gen\", version = \"0.0.2\")\n\
             local_path_override(\n    module_name = \"gen\",\n    path = \"gen_module\",\n)\n",
        ),
        (
            "edit MODULE.bazel.lock",
            "MODULE.bazel.lock",
            "{\"lockFileVersion\": 11, \"registryFileHashes\": {}}\n",
        ),
    ] {
        write(&root, path, contents);
        let head = commit_all(&root, name);
        let engine = engine_for(&root, &config);
        let out = engine
            .compute(
                base_index.view(),
                &engine.repo.diff(&base, &head).expect("git diff"),
                &engine.bazel_querier(&head).expect("querier for head"),
                &affected_request(&base, &head, false),
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let r = &out.report;
        assert!(
            !r.global,
            "{name}: a file Bazel does not read cannot force a cold rebuild: {:?}",
            r.global_reason
        );
        assert!(
            r.affected.is_empty(),
            "{name}: nothing is affected: {:?}",
            r.affected
        );
        assert!(
            r.dirty_packages.is_empty(),
            "{name}: no package is re-queried: {:?}",
            r.dirty_packages
        );
        git(&root, &["reset", "-q", "--hard", &base]);
        git(&root, &["clean", "-qfd"]);
    }
}

/// The other half of shape B: `common:workspace --noenable_bzlmod` is a line
/// about a `--config=workspace` that nobody passed, and says nothing about the
/// mode. Read as the mode it would make a live `MODULE.bazel` inert, which is
/// the opposite mistake and loses real answers.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_config_scoped_bazelrc_line_does_not_set_the_mode() {
    require_bazel();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = root.join("_store");
    std::fs::create_dir_all(&store).unwrap();
    write_external_workspace(&root, &store, "7.4.1");
    write(
        &root,
        ".bazelrc",
        "common:workspace --noenable_bzlmod --enable_workspace\n",
    );
    write(&root, "config2.txt", "w1\n");
    write(
        &root,
        "BUILD.bazel",
        "exports_files([\"config.txt\", \"config2.txt\"])\n",
    );
    init_repo(&root);
    let base = commit_all(&root, "base");

    let config = root.join("cambium.toml");
    let engine = engine_for(&root, &config);
    let base_index = engine.full_build(&base).expect("cold build at base");

    // The module file is live, so pointing `@gen` at another file moves the
    // repository and everything that consumes it.
    let module = std::fs::read_to_string(root.join("MODULE.bazel")).unwrap();
    write(
        &root,
        "MODULE.bazel",
        &module.replace("@root//:config.txt", "@root//:config2.txt"),
    );
    let head = commit_all(&root, "point @gen at another file");
    let engine = engine_for(&root, &config);
    let out = engine
        .compute(
            base_index.view(),
            &engine.repo.diff(&base, &head).expect("git diff"),
            &engine.bazel_querier(&head).expect("querier"),
            &affected_request(&base, &head, false),
        )
        .expect("incremental run");
    let r = &out.report;
    let affected: BTreeSet<String> = r.affected_labels().into_iter().collect();
    assert!(
        r.global || affected.contains("//app:uses_gen"),
        "a live MODULE.bazel edit must have a consequence: global {:?}, affected {affected:?}",
        r.global_reason
    );
}

/// Shape C, from `sourcegraph`: a repository rule instantiated in `WORKSPACE`
/// itself, with a label literal pointing at a main-repo file.
///
/// sourcegraph writes `npm_translate_lock(name = "npm", pnpm_lock =
/// "//:pnpm-lock.yaml")` straight into `WORKSPACE`. The label harvest only
/// ever read the workspace closure's `.bzl` files, so the lock fed no
/// repository: an edit to it reached nothing, and the published run labelled
/// six targets `cambium_missing` on two change sets. The harvest reads the
/// workspace files too now.
#[test]
#[ignore = "needs a real Bazel; run with --ignored and CAMBIUM_E2E=1"]
fn a_label_written_in_workspace_itself_reaches_its_repositorys_consumers() {
    require_bazel();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = root.join("_store");
    std::fs::create_dir_all(&store).unwrap();
    let major = resolved_bazel_major(&root);
    if major >= 9 {
        eprintln!("skipping: Bazel {major} has no WORKSPACE support");
        return;
    }
    write_workspace_mode_workspace(&root, &store, major);
    init_repo(&root);
    let base = commit_all(&root, "base");

    let config = root.join("cambium.toml");
    let engine = engine_for(&root, &config);
    let base_index = engine.full_build(&base).expect("cold build at base");

    // The derivation says so before any diff runs: the label written in
    // `WORKSPACE` is an input of the repository declared beside it.
    let v = base_index.view();
    assert_eq!(
        v.repos_for_path("config.txt")
            .into_iter()
            .map(|r| v.repo_name(r).to_string())
            .collect::<BTreeSet<_>>(),
        ["gen".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "a label literal in WORKSPACE is a repository input"
    );

    write(&root, "config.txt", "v2\n");
    let head = commit_all(&root, "edit the file WORKSPACE names");
    let engine = engine_for(&root, &config);
    let out = engine
        .compute(
            base_index.view(),
            &engine.repo.diff(&base, &head).expect("git diff"),
            &engine.bazel_querier(&head).expect("querier"),
            &affected_request(&base, &head, false),
        )
        .expect("incremental run");
    let r = &out.report;
    assert!(!r.global, "went global: {:?}", r.global_reason);
    let ours: BTreeSet<String> = r.affected_labels().into_iter().collect();
    assert!(
        ours.contains("//app:uses_gen") && ours.contains("//app:downstream"),
        "the repository's consumers are affected: {ours:?}"
    );
    assert!(!ours.contains("//app:unrelated"), "{ours:?}");
    assert_eq!(
        ours,
        rdeps_oracle(&engine, "@gen//:value"),
        "cambium and `rdeps(//..., @gen//:value)` disagree"
    );
}
