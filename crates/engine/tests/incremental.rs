//! Spec 12.4: the incremental engine against a full rebuild and against the
//! naive reference algorithm, over synthetic graphs and with no Bazel.

use std::collections::BTreeSet;

use cambium_engine::{AffectedRequest, Config, Engine, SynthQuerier};
use cambium_index::IndexData;
use cambium_synth::{
    commit, generate, reference::reference_affected, ChangeSpec, GraphSpec, Mutation, SynthGraph,
};

fn engine() -> Engine {
    let cfg = Config::default();
    let dir = std::env::temp_dir().join("cambium-tests");
    let _ = std::fs::create_dir_all(&dir);
    Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.clone()),
        cambium_bazel::Bazel::at(dir, Some("/nonexistent/bazel")),
    )
}

fn build(e: &Engine, g: &SynthGraph, commit_id: &str) -> IndexData {
    e.full_build_with(commit_id, &SynthQuerier::new(g)).unwrap()
}

fn request() -> AffectedRequest {
    AffectedRequest {
        base: "base".into(),
        head: "head".into(),
        // The reference algorithm has no notion of tags, so compare like for like.
        include_manual: true,
        explain: false,
        no_full_build: true,
        publish_base: false,
    }
}

/// Runs one synthetic revision through the engine and returns
/// (engine answer, reference answer, the head index).
fn step(
    e: &Engine,
    before: &SynthGraph,
    after: &SynthGraph,
    base_index: IndexData,
    changes: &[cambium_diff::Change],
) -> (BTreeSet<String>, BTreeSet<String>, Option<IndexData>) {
    let out = e
        .compute(base_index, changes, &SynthQuerier::new(after), &request())
        .expect("engine run");
    let ours: BTreeSet<String> = out.report.affected.iter().cloned().collect();
    let theirs = reference_affected(before, after);
    (ours, theirs, out.index)
}

fn assert_sound(ours: &BTreeSet<String>, theirs: &BTreeSet<String>, what: &str) {
    let missing: Vec<&String> = theirs.difference(ours).collect();
    assert!(
        missing.is_empty(),
        "{what}: {} targets the reference says are affected are missing from the engine's answer \
         (first few: {:?})",
        missing.len(),
        missing.iter().take(5).collect::<Vec<_>>()
    );
}

#[test]
fn full_build_matches_the_synthetic_graph() {
    let e = engine();
    let g = generate(&GraphSpec::preset("small").unwrap());
    let idx = build(&e, &g, "c0");
    let v = idx.view();
    let map = v.labels.build_map();
    for id in 0..g.targets.len() as u32 {
        let label = g.target_label(id);
        let node = map[label.as_str()];
        assert!(v.is_rule(node), "{label} should be a rule");
        for &d in &g.targets[id as usize].deps {
            let dep = map[g.target_label(d).as_str()];
            assert!(
                v.forward.row(node).contains(&dep),
                "{label} should depend on {}",
                g.target_label(d)
            );
        }
    }
}

#[test]
fn single_mutations_match_the_reference() {
    let e = engine();
    let before = generate(&GraphSpec::preset("small").unwrap());
    let base = build(&e, &before, "c0");

    // One deterministic scenario per mutation kind the spec calls out.
    let pick_with_srcs = (0..before.targets.len() as u32)
        .find(|&i| !before.targets[i as usize].srcs.is_empty())
        .unwrap();
    let pick_with_deps = (0..before.targets.len() as u32)
        .find(|&i| before.targets[i as usize].deps.len() > 1)
        .unwrap();
    let consumed = (0..before.targets.len() as u32)
        .find(|&i| {
            before
                .targets
                .iter()
                .any(|t| t.alive && t.deps.contains(&i))
        })
        .unwrap();

    let scenarios: Vec<(&str, Mutation)> = vec![
        (
            "modify source",
            Mutation::ModifySource {
                target: pick_with_srcs,
                slot: before.targets[pick_with_srcs as usize].srcs[0],
            },
        ),
        (
            "add source",
            Mutation::AddSource {
                target: pick_with_srcs,
            },
        ),
        (
            "delete source",
            Mutation::DeleteSource {
                target: pick_with_srcs,
                slot: before.targets[pick_with_srcs as usize].srcs[0],
            },
        ),
        (
            "edit rule attr",
            Mutation::EditRuleAttr {
                target: pick_with_deps,
            },
        ),
        (
            "remove dep",
            Mutation::RemoveDep {
                target: pick_with_deps,
                dep: before.targets[pick_with_deps as usize].deps[0],
            },
        ),
        (
            "add target",
            Mutation::AddTarget {
                package: before.targets[0].package,
                deps: vec![],
            },
        ),
        ("delete target", Mutation::DeleteTarget { target: consumed }),
        ("edit bzl", Mutation::EditBzl { bzl: 0 }),
    ];

    for (name, m) in scenarios {
        let mut after = before.clone();
        let changes = cambium_synth::apply_one(&mut after, &m);
        let (ours, theirs, _) = step(&e, &before, &after, base.clone(), &changes);
        assert_sound(&ours, &theirs, name);
        assert_eq!(ours, theirs, "{name}: engine and reference disagree");
    }
}

#[test]
fn package_splits_and_merges_match_the_reference() {
    let e = engine();
    let before = generate(&GraphSpec::preset("small").unwrap());
    let base = build(&e, &before, "c0");

    let pkg = before.targets[0].package;
    let live: Vec<u32> = before.packages[pkg as usize]
        .targets
        .iter()
        .copied()
        .filter(|&t| before.targets[t as usize].alive)
        .collect();

    let mut after = before.clone();
    let changes = cambium_synth::apply_one(
        &mut after,
        &Mutation::SplitPackage {
            package: pkg,
            targets: live[..live.len().min(2)].to_vec(),
        },
    );
    let (ours, theirs, head) = step(&e, &before, &after, base, &changes);
    assert_sound(&ours, &theirs, "split package");
    assert_eq!(ours, theirs, "split package");

    // ...and merging it back.
    let sub = *after
        .pkg_by_path
        .iter()
        .find(|(p, _)| p.contains("/sub"))
        .expect("a sub package was created")
        .1;
    let merged_before = after.clone();
    let mut merged = after.clone();
    let changes = cambium_synth::apply_one(&mut merged, &Mutation::MergePackage { package: sub });
    let (ours, theirs, _) = step(&e, &merged_before, &merged, head.unwrap(), &changes);
    assert_sound(&ours, &theirs, "merge package");
    assert_eq!(ours, theirs, "merge package");
}

#[test]
fn incremental_matches_a_full_rebuild_over_a_commit_sequence() {
    let e = engine();
    let mut g = generate(&GraphSpec::preset("small").unwrap());
    let mut index = build(&e, &g, "c0");
    let spec = ChangeSpec::default();

    for rev in 0..12u64 {
        let before = g.clone();
        let c = commit(&mut g, &spec, rev);
        if c.changes.is_empty() {
            continue;
        }
        let (ours, theirs, head) = step(&e, &before, &g, index, &c.changes);
        assert_sound(&ours, &theirs, &format!("revision {rev}"));
        assert_eq!(ours, theirs, "revision {rev}: {:?}", c.mutations);

        // Spec 12.4 (1): the spliced index and a full rebuild describe the same
        // graph, up to id assignment.
        let head = head.expect("a non-global run produces an index");
        let fresh = build(&e, &g, &format!("c{}", rev + 1));
        assert_same_graph(&head, &fresh, rev);
        index = head;
    }
}

/// Compares two indexes by label, ignoring ids and tombstones.
fn assert_same_graph(spliced: &IndexData, fresh: &IndexData, rev: u64) {
    let (a, b) = (spliced.view(), fresh.view());
    let edges = |v: cambium_index::IndexRef<'_>| -> BTreeSet<(String, String)> {
        let mut out = BTreeSet::new();
        for id in 0..v.num_nodes() {
            if v.is_tombstone(id) || v.kind_of(id) == cambium_index::NodeKind::Unknown {
                continue;
            }
            for &d in v.forward.row(id) {
                out.insert((v.label(id).to_string(), v.label(d).to_string()));
            }
        }
        out
    };
    let rules = |v: cambium_index::IndexRef<'_>| -> BTreeSet<(String, [u8; 32])> {
        let mut out = BTreeSet::new();
        for id in 0..v.num_nodes() {
            if v.is_rule(id) && !v.is_tombstone(id) {
                out.insert((v.label(id).to_string(), v.hash[id as usize]));
            }
        }
        out
    };
    let (ea, eb) = (edges(a), edges(b));
    assert_eq!(
        ea.difference(&eb).collect::<Vec<_>>(),
        Vec::<&(String, String)>::new(),
        "revision {rev}: spliced index has edges the rebuild does not"
    );
    assert_eq!(
        eb.difference(&ea).collect::<Vec<_>>(),
        Vec::<&(String, String)>::new(),
        "revision {rev}: rebuild has edges the spliced index does not"
    );
    assert_eq!(rules(a), rules(b), "revision {rev}: rule hashes differ");
}

#[test]
fn a_global_invalidator_short_circuits() {
    let e = engine();
    let g = generate(&GraphSpec::preset("small").unwrap());
    let base = build(&e, &g, "c0");
    let changes = vec![cambium_diff::Change::new(
        cambium_diff::Status::Modified,
        "MODULE.bazel",
    )];
    let out = e
        .compute(base, &changes, &SynthQuerier::new(&g), &request())
        .unwrap();
    assert!(out.report.global);
    assert!(out.index.is_none());
    assert_eq!(out.report.as_query_expression(false), "//...");
}

#[test]
fn an_empty_diff_affects_nothing() {
    let e = engine();
    let g = generate(&GraphSpec::preset("small").unwrap());
    let base = build(&e, &g, "c0");
    let out = e
        .compute(base, &[], &SynthQuerier::new(&g), &request())
        .unwrap();
    assert!(out.report.affected.is_empty());
    assert!(out.report.dirty_packages.is_empty());
    assert_eq!(out.report.as_query_expression(false), "set()");
}

#[test]
fn explain_reports_a_path_back_to_the_change() {
    let e = engine();
    let before = generate(&GraphSpec::preset("small").unwrap());
    let base = build(&e, &before, "c0");
    let t = (0..before.targets.len() as u32)
        .find(|&i| {
            !before.targets[i as usize].srcs.is_empty()
                && before
                    .targets
                    .iter()
                    .any(|x| x.alive && x.deps.contains(&i))
        })
        .unwrap();
    let slot = before.targets[t as usize].srcs[0];
    let mut after = before.clone();
    let changes = cambium_synth::apply_one(&mut after, &Mutation::ModifySource { target: t, slot });

    let req = AffectedRequest {
        explain: true,
        ..request()
    };
    let out = e
        .compute(base, &changes, &SynthQuerier::new(&after), &req)
        .unwrap();
    let explanations = out.report.explanations.expect("explanations requested");
    let root = after.target_label(t);
    let e0 = explanations
        .iter()
        .find(|e| e.target == root)
        .expect("the edited target is explained");
    assert_eq!(e0.direct, root);
    assert!(
        matches!(&e0.reason, cambium_engine::DirectReason::File { path } if path == &after.src_path(t, slot)),
        "unexpected reason: {:?}",
        e0.reason
    );
    // Every explanation's path starts at the target and ends at a direct member.
    for e in &explanations {
        assert_eq!(e.path.first().unwrap(), &e.target);
        assert_eq!(e.path.last().unwrap(), &e.direct);
    }
}
