//! End-to-end tests against a real Bazel (spec 12.2, 12.3).
//!
//! These are the tests that check the parts the synthetic oracles cannot: that
//! the vendored proto really decodes what Bazel emits, that `//pkg:*` and
//! `rbuildfiles` mean what we think they mean, and that our reverse closure
//! agrees with `bazel query rdeps`.
//!
//! They need Bazel, so they are opt-in:
//!
//! ```sh
//! CAMBIUM_E2E=1 cargo test -p cambium-engine --test e2e_bazel
//! CAMBIUM_E2E=1 CAMBIUM_BAZEL=/path/to/bazelisk cargo test ...
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use cambium_engine::{AffectedRequest, Engine};
use cambium_index::{IndexData, IndexRef, NodeKind};

fn enabled() -> bool {
    std::env::var("CAMBIUM_E2E").is_ok()
}

fn bazel_binary() -> Option<String> {
    std::env::var("CAMBIUM_BAZEL").ok()
}

macro_rules! skip_unless_e2e {
    () => {
        if !enabled() {
            eprintln!("skipping: set CAMBIUM_E2E=1 (and CAMBIUM_BAZEL if bazel is not on PATH)");
            return;
        }
    };
}

// --------------------------------------------------------------- git helpers

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn commit_all(dir: &Path, msg: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn write(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, contents).unwrap();
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
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "t"]);
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
            explain: false,
            no_full_build: true,
        };
        let out = engine
            .compute(
                self.base_index.clone(),
                &engine.repo.diff(&self.base, &head).expect("git diff"),
                &engine.bazel_querier(&head),
                &req,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        // Every scenario is also checked against the legacy algorithm this
        // design replaces: rebuild the whole graph at head and diff deep
        // hashes. That is the ground truth, and it needs no hand-written
        // expectations to go stale.
        if !out.report.global {
            let head_index = engine.full_build(&head).expect("full build at head");
            let legacy = legacy_affected(&self.root, &self.base, &head, &self.base_index, &head_index);
            let ours: BTreeSet<String> = out.report.affected.iter().cloned().collect();
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
                        let digest = memo[d as usize].unwrap_or_else(|| {
                            *blake3::hash(v.label(d).as_bytes()).as_bytes()
                        });
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
    report.affected.iter().map(|s| s.as_str()).collect()
}

// ------------------------------------------------------------------ scenarios

#[test]
fn golden_scenarios() {
    skip_unless_e2e!();
    let g = Golden::setup();

    // The whole chain from //lib:core up: the alias, the app, its test, and the
    // suite that lists it.
    let downstream: BTreeSet<&str> = [
        "//lib:core",
        "//app:core_alias",
        "//app:main",
        "//app:main_test",
        "//app:all_tests",
    ]
    .into_iter()
    .collect();

    // 1. Editing a source file reaches its consumers, and nothing else.
    let r = g.scenario("modify source", |root| {
        write(root, "lib/a.cc", "// a v2\n");
    });
    assert_eq!(set(&r), downstream, "modify source");
    assert!(r.dirty_packages.is_empty(), "a content edit dirties no package");
    assert_eq!(r.tests, vec!["//app:all_tests", "//app:main_test"]);

    // 2. A file added under an existing glob changes the target's inputs.
    let r = g.scenario("add globbed source", |root| {
        write(root, "lib/b.cc", "// b\n");
    });
    assert_eq!(set(&r), downstream, "add globbed source");
    assert_eq!(r.dirty_packages, vec!["lib"]);

    // 3. ...and deleting one does too.
    let r = g.scenario("delete source", |root| {
        std::fs::remove_file(root.join("lib/nested/deep.cc")).unwrap();
    });
    assert_eq!(set(&r), downstream, "delete source");

    // 4. Renaming a file is a delete plus an add.
    let r = g.scenario("rename source", |root| {
        std::fs::rename(root.join("lib/a.cc"), root.join("lib/renamed.cc")).unwrap();
    });
    assert_eq!(set(&r), downstream, "rename source");

    // 5. An attribute-only BUILD edit moves that target's hash and nothing below it.
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

    // 6. A new target in a package affects only itself.
    let r = g.scenario("add target", |root| {
        let p = root.join("app/BUILD.bazel");
        let s = std::fs::read_to_string(&p).unwrap();
        std::fs::write(
            &p,
            format!("{s}\nshared_lib(\n    name = \"extra\",\n    deps = [\":main\"],\n)\n"),
        )
        .unwrap();
    });
    assert_eq!(set(&r), ["//app:extra"].into_iter().collect::<BTreeSet<_>>());

    // 7. Deleting a target makes its consumers direct.
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

    // 8. A .bzl edit reaches every package that loads it.
    let r = g.scenario("edit bzl", |root| {
        let p = root.join("lib/defs.bzl");
        let s = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, s.replace("SHARED = \"v1\"", "SHARED = \"v2\"")).unwrap();
    });
    let affected = set(&r);
    for t in &downstream {
        assert!(affected.contains(t), "edit bzl should reach {t}: {affected:?}");
    }
    assert!(
        r.dirty_packages.contains(&"lib".to_string()) && r.dirty_packages.contains(&"app".to_string()),
        "rbuildfiles should report both loading packages: {:?}",
        r.dirty_packages
    );

    // 9. A file exported by one package and consumed by another.
    let r = g.scenario("modify exported file", |root| {
        write(root, "tools/data.txt", "data v2\n");
    });
    assert_eq!(
        set(&r),
        ["//other:reader"].into_iter().collect::<BTreeSet<_>>(),
        "exports_files consumer"
    );

    // 10. A select branch changing is a change to the target's inputs, because
    //     --proto:flatten_selects reports the union of the branches.
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
    assert!(set(&r).contains("//app:main"), "select branch change: {:?}", r.affected);

    // 11. Splitting a package: the parent's glob shrinks, so it changes too.
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
    assert!(affected.contains("//lib:core"), "parent package loses globbed files");
    assert!(affected.contains("//lib/nested:deep"), "new package's target is new");
    assert!(
        r.dirty_packages.contains(&"lib".to_string())
            && r.dirty_packages.contains(&"lib/nested".to_string()),
        "both packages dirty: {:?}",
        r.dirty_packages
    );

    // 12. A global invalidator short-circuits everything.
    let r = g.scenario("edit MODULE.bazel", |root| {
        write(
            root,
            "MODULE.bazel",
            "module(name = \"golden\", version = \"0.0.2\")\n",
        );
    });
    assert!(r.global, "MODULE.bazel edit is global");
    assert_eq!(r.as_query_expression(false), "//...");

    // 13. An ignored path changes nothing.
    let r = g.scenario("edit ignored path", |root| {
        write(root, "lib/README.md", "docs\n");
    });
    assert!(r.affected.is_empty(), "ignored path: {:?}", r.affected);
    assert!(!r.global);
}

/// Spec 12.3: our reverse closure must contain everything `bazel query rdeps`
/// reports. Extra targets are the accepted over-approximation.
#[test]
fn differential_check_against_bazel_rdeps() {
    skip_unless_e2e!();
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
                "--noimplicit_deps",
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
            },
        );
        let ours: BTreeSet<String> = cambium_index::walk::labels_sorted(v, &r.affected)
            .into_iter()
            .collect();
        let missing: Vec<&String> = theirs.difference(&ours).collect();
        assert!(
            missing.is_empty(),
            "rdeps({label}): the index is missing {missing:?}"
        );
        checked += 1;
    }
    assert!(checked > 5, "expected several rule targets, checked {checked}");
}

/// A synthetic workspace, driven through real Bazel over a generated history.
#[test]
fn synthetic_workspace_through_real_bazel() {
    skip_unless_e2e!();
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
        };
        let out = engine
            .run_incremental(index.clone(), &req)
            .unwrap_or_else(|e| panic!("commit {i}: {e}"));
        if out.report.global {
            index = engine.full_build(head).unwrap();
            continue;
        }

        let fresh = engine.full_build(head).expect("full build at head");
        let legacy = legacy_affected(root, base, head, &index, &fresh);
        let ours: BTreeSet<String> = out.report.affected.iter().cloned().collect();
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
