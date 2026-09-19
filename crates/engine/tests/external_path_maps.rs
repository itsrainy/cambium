//! An edit to a file a repository is *built from* is not an edit to a `.bzl`
//! that repository *exports*.
//!
//! `@mod` is `api/` under another name, and `//app`'s load closure reaches
//! `@mod//bazel:defs.bzl`. Before the path mapping, an edit to any file under
//! `api/` re-queried every package with that load — on envoy, all twelve
//! hundred of them, which is a cold rebuild. `api/envoy/x.proto` translated
//! into the repository is `envoy/x.proto`, which nothing loads, so only the
//! label consumers of `@mod` are affected.
//!
//! The three cases that must keep the wide answer are here too: an edit that
//! *is* the loaded `.bzl`, a `MODULE.bazel` version bump, and a repository
//! whose mapping could not be derived.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::{FixedText, WorkspaceText};
use cambium_engine::query::Rows;
use cambium_engine::{AffectedRequest, EngineError, PackageQuerier};
use cambium_index::{FileText, IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Files, Scanning};

/// `mod` is `api/`, by `local_path_override`; `opaque` is defined by an
/// extension whose `.bzl` names `//conf:settings.yml` as an input, and no
/// static shape says where that file lands inside it. The same `.bzl` also
/// writes a BUILD file whose target lists `//notes` — the `@envoy_repo`
/// shape, where a generated `genrule` names `@envoy//changelogs`.
const MODULE_BASE: &str = "module(name = \"demo\", version = \"1\")\n\
     bazel_dep(name = \"mod\", version = \"1.0\")\n\
     local_path_override(module_name = \"mod\", path = \"api\")\n\
     gen = use_extension(\"//bazel:ext.bzl\", \"gen\")\n\
     use_repo(gen, \"opaque\")\n\
     load(\"//bazel:vendored.bzl\", \"vendored\")\n\
     vendored()\n";

fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("MODULE.bazel", MODULE_BASE);
    put(
        "bazel/ext.bzl",
        "CONF = attr.label(default = \"@demo//conf:settings.yml\")\n\
         BUILD = '''filegroup(name = \"thing\", srcs = [\"@demo//notes\"])'''\n\
",
    );
    put(
        "bazel/vendored.bzl",
        "def vendored():\n\
         \x20   new_local_repository(\n\
         \x20       name = \"vend\",\n\
         \x20       path = \"vendored\",\n\
         \x20       build_file = \"@demo//bazel:vend.BUILD\",\n\
         \x20   )\n",
    );
    // `//app` loads a `.bzl` out of `@mod` and out of `@opaque`, transitively;
    // `//lib` and `//conf` only name a target in one. Those are rules (2)
    // and (1).
    put("app/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    put(
        "bazel/defs.bzl",
        "load(\"@mod//bazel:defs.bzl\", \"r\")\n\
         load(\"@mod//bazel:more.bzl\", \"m\")\n\
         load(\"@opaque//:defs.bzl\", \"o\")\n\
         load(\"@vend//:defs.bzl\", \"v\")\n",
    );
    put("lib/BUILD", "");
    put("conf/BUILD", "");
    put("names/BUILD", "");
    put("notes/BUILD", "");
    Files(m)
}

fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule("//app:app", "demo_lib", vec![])],
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@mod//:thing".into()],
        )],
        "conf" => vec![TargetRow::rule(
            "//conf:conf",
            "demo_lib",
            vec!["@opaque//:thing".into()],
        )],
        "names" => vec![TargetRow::rule(
            "//names:names",
            "demo_lib",
            vec!["@vend//:thing".into()],
        )],
        // `@demo//notes` is the rule target `//notes:notes`, a filegroup over
        // a glob — what it is built from is an edge, not a path prefix.
        "notes" => vec![
            TargetRow::source_file("//notes:README.md"),
            TargetRow::rule(
                "//notes:notes",
                "filegroup",
                vec!["//notes:README.md".into()],
            ),
        ],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "lib", "conf", "names", "notes"];

const REPO_MAPPING: &str =
    "{\"\":\"\",\"demo\":\"\",\"mod\":\"mod+\",\"opaque\":\"opaque+\",\"vend\":\"vend+\"}";

fn scanning() -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
}

fn texts(base_module: &str, head_module: &str) -> FixedText {
    let mut m = BTreeMap::new();
    m.insert(
        ("base".to_string(), "MODULE.bazel".to_string()),
        base_module.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel".to_string()),
        head_module.to_string(),
    );
    FixedText(m)
}

fn run(
    index: &IndexData,
    paths: &[&str],
    texts: &dyn WorkspaceText,
) -> cambium_engine::AffectedReport {
    let changes: Vec<Change> = paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect();
    engine()
        .compute_with_texts(index.view(), &changes, &scanning(), &req(), texts)
        .unwrap()
        .report
}

#[test]
fn the_index_records_where_a_local_path_override_lands() {
    let (_e, index) = base(&scanning());
    let v = index.view();
    let mod_id = (0..v.num_repos())
        .find(|&r| v.repo_name(r) == "mod")
        .expect("the overridden module is a repository");
    let maps = v.repo_path_maps(mod_id);
    assert_eq!(maps.len(), 1, "{maps:?}");
    assert_eq!(maps[0].from, "api/");
    assert_eq!(maps[0].to, "");
    assert_eq!(
        maps[0].translate("api/envoy/x.proto").as_deref(),
        Some("envoy/x.proto")
    );
    // The extension-defined repository has no derivable mapping, which is what
    // keeps the old behaviour for it.
    let opaque = (0..v.num_repos())
        .find(|&r| v.repo_name(r) == "opaque")
        .expect("the extension's repository");
    assert!(v.repo_path_maps(opaque).is_empty());
}

#[test]
fn an_input_edit_under_a_mapped_repository_reaches_only_its_label_consumers() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &["api/envoy/config/v3/x.proto"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 1);
    // Rule (1): `//lib:lib` names `@mod//:thing`. Rule (2) did not fire, so
    // `//app`, whose load closure reaches `@mod//bazel:defs.bzl`, is neither
    // dirty nor affected.
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(r.affected_labels(), vec!["//lib:lib".to_string()]);
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
}

#[test]
fn an_input_edit_that_is_the_loaded_bzl_still_dirties_its_packages() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &["api/bazel/defs.bzl"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

#[test]
fn a_module_version_bump_still_dirties_the_loading_packages() {
    let (_e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"mod\", version = \"1.0\")",
        "bazel_dep(name = \"mod\", version = \"2.0\")",
    );
    let r = run(&index, &["MODULE.bazel"], &texts(MODULE_BASE, &head));
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "any file in a bumped module may have moved: {:?}",
        r.dirty_packages
    );
}

/// A version bump *and* an input edit in one commit: the bump keeps rule (2).
#[test]
fn a_bump_beats_an_input_edit_in_the_same_commit() {
    let (_e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"mod\", version = \"1.0\")",
        "bazel_dep(name = \"mod\", version = \"2.0\")",
    );
    let r = run(
        &index,
        &["MODULE.bazel", "api/envoy/x.proto"],
        &texts(MODULE_BASE, &head),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

/// `@opaque` has no derivable mapping, so an edit to a file it is declared to
/// read keeps the whole-closure answer.
#[test]
fn a_repository_with_no_derivable_mapping_keeps_the_old_behaviour() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &["conf/settings.yml"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 0);
    assert_eq!(r.direct, vec!["//conf:conf".to_string()]);
    // The rule reads the file, so anything it writes — `defs.bzl` included —
    // may have moved: the package whose closure loads it is re-queried.
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

/// `@opaque`'s rule writes a BUILD file whose target lists `//notes`. That is
/// the rule target `//notes:notes`, a build-time input of the repository's
/// *targets* and not of the rule: the rule never reads it, so
/// `@opaque//:defs.bzl` cannot have moved.
///
/// Which files `//notes:notes` is built from is a question for the graph, not
/// for a path prefix over `notes/` (issue #8): the walk from
/// `notes/README.md` reaches the filegroup, the filegroup is a recorded label
/// input of `@opaque`, and so `//conf`, which names `@opaque//:thing`, is
/// direct. `//app`, whose closure loads `@opaque//:defs.bzl`, is not
/// re-queried.
#[test]
fn a_path_named_only_by_a_generated_target_reaches_only_label_consumers() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &["notes/README.md"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    // Nothing in the diff named a repository; the walk did.
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert_eq!(r.stats.repos_dirty_by_target, 1);
    assert!(
        r.direct.contains(&"//conf:conf".to_string()),
        "{:?}",
        r.direct
    );
    let a = r.affected_labels();
    assert!(a.contains(&"//conf:conf".to_string()), "{a:?}");
    assert!(a.contains(&"//notes:notes".to_string()), "{a:?}");
    // Nothing is re-queried at all: rule (2) never fires for a repository
    // found this way, and a changed source file the graph already has edges
    // for does not dirty its package either.
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
}

/// The rule's own input and a target-only path in one commit: the input wins
/// for that repository.
#[test]
fn an_input_the_rule_reads_beats_a_target_only_path_in_the_same_commit() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &["conf/settings.yml", "notes/README.md"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

/// A commit that dirties no package at all, materialised. Its index can only
/// have come from the base's own tables, so it is the shape that shows what
/// the read-back drops.
fn spliced_over_nothing(cold: &IndexData) -> IndexData {
    engine()
        .compute_with_texts(
            cold.view(),
            &[Change::new(Status::Modified, "docs/notes.md")],
            &scanning(),
            &AffectedRequest {
                materialize: true,
                ..req()
            },
            &texts(MODULE_BASE, MODULE_BASE),
        )
        .unwrap()
        .index
        .expect("a materialised run yields an index")
}

/// A spliced index must carry every externals side table a cold build wrote.
///
/// The `.bzl` closure is read back out of the base index and handed to the
/// splice, and a commit that dirties no package re-derives none of it: any
/// column the read-back drops is dropped for good, in an index that gets
/// published and spliced onto again. The rebuild oracle compares nodes,
/// edges and rule hashes, so it sees none of this. The symptom is silent and
/// expensive — [`cambium_engine::externals`] reads a repository with no
/// recorded mapping as "the question cannot be answered", so rule (2) fires
/// and every package that loads a `.bzl` from the repository is re-queried.
#[test]
fn a_splice_carries_the_externals_side_tables_a_cold_build_wrote() {
    let (_e, cold) = base(&scanning());
    let spliced = spliced_over_nothing(&cold);
    let (c, s) = (cold.view(), spliced.view());
    assert_eq!(s.num_repos(), c.num_repos(), "repositories");
    for r in 0..c.num_repos() {
        let name = c.repo_name(r);
        let sr = (0..s.num_repos())
            .find(|&i| s.repo_name(i) == name)
            .unwrap_or_else(|| panic!("{name} survived the splice"));
        assert_eq!(
            s.repo_path_maps(sr),
            c.repo_path_maps(r),
            "path mapping of {name}"
        );
        assert_eq!(s.repo_canonical.get(sr), c.repo_canonical.get(r), "{name}");
    }
    assert_eq!(
        s.repo_input_patterns.len(),
        c.repo_input_patterns.len(),
        "input patterns"
    );
    for p in 0..c.repo_input_patterns.len() {
        let i = p as u32;
        assert_eq!(
            (
                c.repo_input_patterns.get(i),
                c.repo_input_prefix[p],
                c.repo_input_target_only[p]
            ),
            (
                s.repo_input_patterns.get(i),
                s.repo_input_prefix[p],
                s.repo_input_target_only[p]
            ),
            "input pattern {p}"
        );
    }
}

/// The consequence, end to end: the sparing survives a splice.
#[test]
fn an_input_edit_is_still_spared_against_a_spliced_base() {
    let (_e, cold) = base(&scanning());
    let spliced = spliced_over_nothing(&cold);
    let r = run(
        &spliced,
        &["api/envoy/config/v3/x.proto"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_label_only, 1);
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
}

/// `@vend` is a `new_local_repository` over `vendored/`, so its mapping is
/// exhaustive: everything it exports comes from that directory. Its
/// `build_file` is an input of the repository, and editing it dirties the
/// repository — but the file lives outside `vendored/`, so it cannot be one
/// of the `.bzl` files `//app`'s load closure reaches into the repository for,
/// and rule (2) does not fire. Before the `exhaustive` marker every uncovered
/// path kept the wide answer.
#[test]
fn an_input_edit_outside_an_exhaustive_mapping_does_not_re_query() {
    let (_e, index) = base(&scanning());
    let v = index.view();
    let vend = (0..v.num_repos())
        .find(|&r| v.repo_name(r) == "vend")
        .expect("the local repository");
    let maps = v.repo_path_maps(vend);
    // Two mappings, and both are true of it: the repository *is* `vendored/`,
    // and the BUILD file it was declared with is a file of this tree that
    // lands at its package root. The second is not exhaustive, and does not
    // need to be — the first already accounts for the repository, which is
    // what lets an uncovered path skip rule (2).
    assert_eq!(maps.len(), 2, "{maps:?}");
    assert!(maps.iter().any(|m| m.exhaustive && m.from == "vendored/"));
    assert!(maps
        .iter()
        .any(|m| !m.exhaustive && m.from == "bazel/vend.BUILD" && m.to == "BUILD.bazel"));

    let r = run(
        &index,
        &["bazel/vend.BUILD"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 1);
    assert_eq!(r.direct, vec!["//names:names".to_string()]);
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
}

/// And a path the exhaustive mapping *does* cover, which is a `.bzl` the
/// closure loads out of the repository, still fires rule (2).
#[test]
fn an_input_edit_inside_an_exhaustive_mapping_still_re_queries() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &["vendored/defs.bzl"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

/// The overflow diagnostic names, per repository, the changed file that moved
/// it — whichever route did the naming — and counts the packages rule (2)
/// reached once each, however many of the repository's labels a package's
/// closure carries.
#[test]
fn the_overflow_diagnostic_names_every_route_and_counts_packages_once() {
    use cambium_engine::externals::{analyse, mode_at, reach};
    use std::collections::BTreeSet;
    let (_e, index) = base(&scanning());
    let v = index.view();

    let change = |p: &str| vec![Change::new(Status::Modified, p)];
    let both = texts(MODULE_BASE, MODULE_BASE);
    let e = analyse(
        v,
        &change("api/envoy/x.proto"),
        "base",
        "head",
        &both,
        || mode_at(v, &both, "base"),
    );
    assert_eq!(e.dirtied_by["mod"], vec!["api/envoy/x.proto".to_string()]);
    assert_eq!(e.mode, None, "an input-path edit never asks which mode");

    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"mod\", version = \"1.0\")",
        "bazel_dep(name = \"mod\", version = \"2.0\")",
    );
    let both = texts(MODULE_BASE, &head);
    let e = analyse(v, &change("MODULE.bazel"), "base", "head", &both, || {
        mode_at(v, &both, "base")
    });
    assert_eq!(e.dirtied_by["mod"], vec!["MODULE.bazel".to_string()]);
    assert_eq!(e.mode, Some(cambium_index::Mode::Bzlmod));

    // `//app`'s closure carries two `@mod` labels; it is one package.
    let repos: BTreeSet<String> = ["mod".to_string()].into_iter().collect();
    let r = reach(v, &repos, &repos);
    assert_eq!(r.packages, ["app".to_string()].into_iter().collect());
    assert_eq!(r.packages_by_repo["mod"], 1);
}

/// A dirty package that comes back empty — it failed to load under
/// `--keep_going` — keeps no load closure: a full build at the same commit
/// never scans its BUILD file, and the rebuild oracle compares the two.
#[test]
fn a_package_bazel_could_not_load_keeps_no_load_closure() {
    /// `Scanning`, except that `app` never loads.
    struct AppFails(Scanning);
    impl PackageQuerier for AppFails {
        fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
            let mut rows = self.0.query(packages)?;
            for u in &mut rows.updates {
                if u.package == "app" {
                    u.targets.clear();
                }
            }
            Ok(rows)
        }
        fn universe(&self) -> Result<Rows, EngineError> {
            let mut rows = self.0.universe()?;
            rows.updates.retain(|u| u.package != "app");
            Ok(rows)
        }
        fn load_source(&self) -> Option<&dyn FileText> {
            self.0.load_source()
        }
        fn repo_mapping(&self) -> cambium_index::RepoMapping {
            self.0.repo_mapping()
        }
    }
    let consumers_of_defs = |v: cambium_index::IndexRef<'_>| -> Vec<String> {
        // No node at all when nothing loads it: the same as no consumers.
        let Some(id) = v.bzl_id("bazel/defs.bzl") else {
            return Vec::new();
        };
        v.bzl_consumers(id)
            .iter()
            .map(|&p| v.package_name(p).to_string())
            .collect()
    };
    let (_e, base) = base(&scanning());
    assert_eq!(consumers_of_defs(base.view()), vec!["app".to_string()]);

    let e = engine();
    let q = AppFails(scanning());
    let req = AffectedRequest {
        materialize: true,
        ..req()
    };
    let changes = vec![Change::new(Status::Modified, "app/BUILD")];
    let head = e
        .compute_with_texts(
            base.view(),
            &changes,
            &q,
            &req,
            &texts(MODULE_BASE, MODULE_BASE),
        )
        .unwrap()
        .index
        .expect("a spliced head index");
    let rebuilt = e.full_build_with("head", &q).unwrap();
    assert!(consumers_of_defs(rebuilt.view()).is_empty());
    assert_eq!(
        consumers_of_defs(head.view()),
        consumers_of_defs(rebuilt.view()),
        "the spliced index must agree with a full build about a package Bazel could not load"
    );
}
