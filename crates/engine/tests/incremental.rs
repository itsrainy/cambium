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
        materialize: true,
        publish_base: false,
        max_depth: None,
        allow_partial: false,
    }
}

/// The overlay's explanations must match too — they are produced from the same
/// walk's parent pointers, over a graph one path never built.
#[test]
fn the_overlay_explains_the_same_way() {
    let e = engine();
    let before = generate(&GraphSpec::preset("small").unwrap());
    let base = build(&e, &before, "c0");
    let t = (0..before.targets.len() as u32)
        .find(|&i| !before.targets[i as usize].srcs.is_empty())
        .unwrap();
    let slot = before.targets[t as usize].srcs[0];
    let mut after = before.clone();
    let changes = cambium_synth::apply_one(&mut after, &Mutation::ModifySource { target: t, slot });

    let overlay = overlay_answer(&e, &after, base.clone(), &changes, true);
    let materialised = e
        .compute(
            base.view(),
            &changes,
            &SynthQuerier::new(&after),
            &AffectedRequest {
                explain: true,
                ..request()
            },
        )
        .unwrap()
        .report;

    let a = overlay.explanations.expect("overlay explanations");
    let b = materialised
        .explanations
        .expect("materialised explanations");
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.target, y.target);
        assert_eq!(x.path, y.path);
        assert_eq!(x.direct, y.direct);
        assert_eq!(x.reason, y.reason);
    }
}

/// The overlay must stay proportional to the change, not to the graph.
#[test]
fn the_overlay_stays_small() {
    use cambium_index::{GraphView, IndexBuilder};

    let e = engine();
    let mut g = generate(&GraphSpec::preset("medium").unwrap());
    let base = build(&e, &g, "c0");
    let base_nodes = base.view().num_nodes();
    let spec = cambium_synth::ChangeSpec::preset("typical-pr").unwrap();
    let c = cambium_synth::commit(&mut g, &spec, 0);

    let base_view = base.view();
    let mut builder = IndexBuilder::from_view(base_view);
    let dirty: Vec<String> = c
        .changes
        .iter()
        .filter_map(|ch| {
            cambium_classify::PackageSet::from_index(base_view)
                .owner_of(&ch.path)
                .map(String::from)
        })
        .collect();
    let updates: Vec<_> = dirty
        .iter()
        .filter_map(|p| g.pkg_by_path.get(p).map(|&i| g.package_update(i)))
        .collect();
    builder.apply_packages(&updates);
    let overlay = builder.finish_overlay();
    let size = overlay.delta_size();

    assert_eq!(
        overlay.num_nodes(),
        base_nodes + size.new_nodes as u32,
        "the overlay's node space is the base plus what it appended"
    );
    let touched = size.changed_nodes + size.rows_gaining_edges + size.rows_losing_edges;
    assert!(
        touched < base_nodes as usize / 10,
        "the overlay touched {touched} of {base_nodes} nodes; it should be \
         proportional to the change, not the graph: {size:?}"
    );
}

/// The same run with the delta overlay instead of a materialised index.
///
/// The overlay exists so a pull-request run does not build a whole new index it
/// will throw away. It has to answer exactly the question the materialised path
/// answers, and this is what enforces that: every scenario below computes both
/// and requires them to be identical, down to the explanations.
fn overlay_answer(
    e: &Engine,
    after: &SynthGraph,
    base_index: IndexData,
    changes: &[cambium_diff::Change],
    explain: bool,
) -> cambium_engine::AffectedReport {
    let req = AffectedRequest {
        materialize: false,
        explain,
        ..request()
    };
    let out = e
        .compute(base_index.view(), changes, &SynthQuerier::new(after), &req)
        .expect("overlay run");
    assert!(
        out.index.is_none(),
        "an overlay run must not materialise an index"
    );
    out.report
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
    let overlay = overlay_answer(e, after, base_index.clone(), changes, false);
    let out = e
        .compute(
            base_index.view(),
            changes,
            &SynthQuerier::new(after),
            &request(),
        )
        .expect("engine run");

    // Every scenario is a gate on the overlay: it must produce the same report
    // the materialised path does.
    assert_eq!(
        overlay.affected, out.report.affected,
        "the overlay and the materialised index disagree about what is affected"
    );
    assert_eq!(overlay.direct, out.report.direct, "direct sets disagree");
    assert_eq!(overlay.tests, out.report.tests, "test sets disagree");
    assert_eq!(
        overlay.dirty_packages, out.report.dirty_packages,
        "dirty package sets disagree"
    );

    let ours: BTreeSet<String> = out.report.affected_labels().into_iter().collect();
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
        .compute(base.view(), &changes, &SynthQuerier::new(&g), &request())
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
        .compute(base.view(), &[], &SynthQuerier::new(&g), &request())
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
        .compute(base.view(), &changes, &SynthQuerier::new(&after), &req)
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

/// Schema 3: every affected target carries the depth at which the walk first
/// reached it, and `--max-depth` truncates the closure at that depth.
#[test]
fn affected_targets_carry_their_depth() {
    let e = engine();
    let before = generate(&GraphSpec::preset("small").unwrap());
    let base = build(&e, &before, "c0");
    let t = (0..before.targets.len() as u32)
        .find(|&i| !before.targets[i as usize].srcs.is_empty())
        .unwrap();
    let slot = before.targets[t as usize].srcs[0];
    let mut after = before.clone();
    let changes = cambium_synth::apply_one(&mut after, &Mutation::ModifySource { target: t, slot });

    let full = e
        .compute(
            base.view(),
            &changes,
            &SynthQuerier::new(&after),
            &request(),
        )
        .unwrap()
        .report;
    let direct: BTreeSet<&str> = full.direct.iter().map(String::as_str).collect();
    for a in &full.affected {
        assert_eq!(
            a.depth == 0,
            direct.contains(a.label.as_str()),
            "{}: depth 0 iff direct",
            a.label
        );
        assert_eq!(a.test, full.tests.contains(&a.label));
    }
    assert!(
        full.affected.iter().any(|a| a.depth > 0),
        "a source edit should reach something transitively"
    );

    let capped = e
        .compute(
            base.view(),
            &changes,
            &SynthQuerier::new(&after),
            &AffectedRequest {
                max_depth: Some(1),
                ..request()
            },
        )
        .unwrap()
        .report;
    let want: Vec<_> = full.affected_within(1).cloned().collect();
    assert_eq!(
        capped.affected, want,
        "--max-depth 1 is the depth<=1 prefix"
    );
    assert!(capped.affected.iter().all(|a| a.depth <= 1));
}
