//! CSR construction, splice, serialisation round trip.

use cambium_index::*;

fn rule(pkg: &str, name: &str, deps: &[&str], srcs: &[&str]) -> TargetRow {
    let inputs = deps
        .iter()
        .chain(srcs.iter())
        .map(|s| s.to_string())
        .collect();
    let mut r = TargetRow::rule(label::rule_label(pkg, name), "synth_lib", inputs);
    r.rule_hash = *blake3::hash(format!("{pkg}:{name}:{deps:?}:{srcs:?}").as_bytes()).as_bytes();
    r
}

fn pkg(name: &str, targets: Vec<TargetRow>) -> PackageUpdate {
    PackageUpdate {
        package: name.to_string(),
        build_hash: *blake3::hash(name.as_bytes()).as_bytes(),
        bzl_hash: [0; 32],
        targets,
    }
}

/// a/b/c chain: //c:c -> //b:b -> //a:a, each with one source file.
fn sample() -> IndexData {
    let mut b = IndexBuilder::new(new_lineage());
    b.set_bazel_version("test");
    b.apply_packages(&[
        pkg(
            "a",
            vec![
                rule("a", "a", &[], &["//a:a.src"]),
                TargetRow::source_file("//a:a.src"),
            ],
        ),
        pkg(
            "b",
            vec![
                rule("b", "b", &["//a:a"], &["//b:b.src"]),
                TargetRow::source_file("//b:b.src"),
            ],
        ),
        pkg(
            "c",
            vec![
                rule("c", "c", &["//b:b"], &["//c:c.src"]),
                TargetRow::source_file("//c:c.src"),
                {
                    let mut t = rule("c", "c_test", &["//c:c"], &[]);
                    t.is_test = true;
                    t.rule_class = Some("synth_test".into());
                    t
                },
            ],
        ),
    ]);
    b.finish("deadbeef")
}

#[test]
fn builds_expected_graph() {
    let d = sample();
    let v = d.view();
    assert_eq!(v.commit, "deadbeef");
    assert_eq!(v.num_packages(), 3);

    let a = v.labels.build_map()["//a:a"];
    let bb = v.labels.build_map()["//b:b"];
    assert!(v.forward.row(bb).contains(&a));
    assert!(v.reverse.row(a).contains(&bb));
    assert_eq!(v.kind_of(a), NodeKind::Rule);
    assert_eq!(v.package_of(a), Some("a"));
    assert_eq!(v.rule_class(a), Some("synth_lib"));
}

#[test]
fn rows_are_sorted_and_deduped() {
    let mut b = IndexBuilder::new(new_lineage());
    b.apply_packages(&[pkg(
        "x",
        vec![
            TargetRow::rule(
                "//x:x",
                "synth_lib",
                vec![
                    "//x:c".into(),
                    "//x:a".into(),
                    "//x:c".into(),
                    "//x:b".into(),
                ],
            ),
            TargetRow::source_file("//x:a"),
            TargetRow::source_file("//x:b"),
            TargetRow::source_file("//x:c"),
        ],
    )]);
    let d = b.finish("0");
    let v = d.view();
    let x = v.labels.build_map()["//x:x"];
    let row = v.forward.row(x);
    assert_eq!(row.len(), 3);
    assert!(row.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn reverse_equals_naive_reference() {
    let d = sample();
    let v = d.view();
    let n = v.num_nodes() as usize;
    let mut naive = vec![Vec::new(); n];
    for u in 0..n as u32 {
        for &t in v.forward.row(u) {
            naive[t as usize].push(u);
        }
    }
    for row in naive.iter_mut() {
        row.sort_unstable();
    }
    for u in 0..n as u32 {
        assert_eq!(v.reverse.row(u), naive[u as usize].as_slice(), "node {u}");
    }
}

#[test]
fn walk_finds_reverse_closure() {
    let d = sample();
    let v = d.view();
    let m = v.labels.build_map();
    let r = walk(v, &[m["//a:a"]], &WalkOptions::default());
    let labels = view::labels_sorted(&v, &r.affected);
    assert_eq!(labels, vec!["//a:a", "//b:b", "//c:c", "//c:c_test"]);
    let tests = view::labels_sorted(&v, &r.tests);
    assert_eq!(tests, vec!["//c:c_test"]);
}

#[test]
fn walk_from_source_file_reaches_consumers() {
    let d = sample();
    let v = d.view();
    let m = v.labels.build_map();
    let src = m["//b:b.src"];
    assert_eq!(v.file_consumers(src).to_vec(), vec![m["//b:b"]]);
    let r = walk(v, &[src], &WalkOptions::default());
    assert_eq!(
        view::labels_sorted(&v, &r.affected),
        vec!["//b:b", "//c:c", "//c:c_test"]
    );
}

#[test]
fn explain_yields_path_back_to_seed() {
    let d = sample();
    let v = d.view();
    let m = v.labels.build_map();
    let r = walk(
        v,
        &[m["//a:a"]],
        &WalkOptions {
            explain: true,
            ..Default::default()
        },
    );
    let path = r.path_to_seed(m["//c:c_test"]).unwrap();
    let labels: Vec<_> = path.iter().map(|&i| v.label(i)).collect();
    assert_eq!(labels, vec!["//c:c_test", "//c:c", "//b:b", "//a:a"]);
}

#[test]
fn splice_preserves_untouched_rows_byte_for_byte() {
    let base = sample();
    let bv = base.view();
    let untouched: Vec<(String, Vec<String>)> = (0..bv.num_nodes())
        .filter(|&i| bv.package_of(i) != Some("c"))
        .map(|i| {
            (
                bv.label(i).to_string(),
                bv.forward
                    .row(i)
                    .iter()
                    .map(|&t| bv.label(t).to_string())
                    .collect(),
            )
        })
        .collect();

    let mut b = IndexBuilder::from_view(bv);
    // Package c gains a target and drops its test.
    let report = b.apply_packages(&[pkg(
        "c",
        vec![
            rule("c", "c", &["//b:b"], &["//c:c.src"]),
            TargetRow::source_file("//c:c.src"),
            rule("c", "c2", &["//c:c"], &[]),
        ],
    )]);
    let head = b.finish("cafebabe");
    let hv = head.view();

    assert_eq!(hv.lineage, bv.lineage, "splice preserves lineage");
    let m = hv.labels.build_map();
    assert!(hv.is_tombstone(m["//c:c_test"]));
    assert!(hv.forward.row(m["//c:c_test"]).is_empty());
    assert_eq!(
        report.packages[0]
            .deleted
            .iter()
            .map(|&i| hv.label(i))
            .collect::<Vec<_>>(),
        vec!["//c:c_test"]
    );
    assert_eq!(
        report.packages[0]
            .changed
            .iter()
            .map(|&i| hv.label(i))
            .collect::<Vec<_>>(),
        vec!["//c:c2"]
    );

    for (lbl, row) in untouched {
        let id = m[lbl.as_str()];
        let got: Vec<String> = hv
            .forward
            .row(id)
            .iter()
            .map(|&t| hv.label(t).to_string())
            .collect();
        assert_eq!(got, row, "forward row of {lbl} changed");
    }
}

#[test]
fn ids_are_stable_across_splices() {
    let base = sample();
    let bv = base.view();
    let before: Vec<(u32, String)> = (0..bv.num_nodes())
        .map(|i| (i, bv.label(i).to_string()))
        .collect();
    let mut b = IndexBuilder::from_view(bv);
    b.apply_packages(&[pkg("d", vec![rule("d", "d", &["//c:c"], &[])])]);
    let head = b.finish("2");
    let hv = head.view();
    for (id, label) in before {
        assert_eq!(hv.label(id), label);
    }
}

fn generated(label: &str, by: &str) -> TargetRow {
    let mut t = TargetRow::source_file(label);
    t.kind = NodeKind::GeneratedFile;
    t.generating_rule = Some(by.into());
    t
}

/// The generator's package and a consumer of its output, with the generator
/// named by `by`.
fn generator_and_consumer(by: &str) -> Vec<PackageUpdate> {
    vec![
        pkg(
            "g",
            vec![
                TargetRow::rule("//g:gen", "genrule", vec![]),
                TargetRow::rule("//g:gen2", "genrule", vec![]),
                generated("//g:out.h", by),
            ],
        ),
        pkg(
            "u",
            vec![TargetRow::rule(
                "//u:u",
                "synth_lib",
                vec!["//g:out.h".into()],
            )],
        ),
    ]
}

fn forward_labels(v: IndexRef<'_>, id: u32) -> Vec<String> {
    view::labels_sorted(&v, v.forward.row(id))
}

#[test]
fn a_generated_file_is_one_hop_between_consumer_and_generator() {
    let mut b = IndexBuilder::new(new_lineage());
    b.apply_packages(&generator_and_consumer("//g:gen"));
    let d = b.finish("1");
    let v = d.view();
    let m = v.labels.build_map();
    // The consumer's edge is to the file, the file's to its generator...
    assert_eq!(v.forward.row(m["//u:u"]), &[m["//g:out.h"]]);
    assert_eq!(v.forward.row(m["//g:out.h"]), &[m["//g:gen"]]);
    // ...and a change to the generator reaches the consumer through it.
    let r = walk(v, &[m["//g:gen"]], &WalkOptions::default());
    assert_eq!(
        view::labels_sorted(&v, &r.affected),
        vec!["//g:gen", "//u:u"]
    );
    assert_eq!(r.depth_of(m["//u:u"]), Some(2));
}

/// Re-querying only the generator's package after a different rule starts
/// producing the file must leave the consumer's row identical to what a full
/// rebuild writes. That is exactly what redirecting consumers straight at the
/// generating rule broke: the carried row kept naming the old generator.
#[test]
fn splicing_a_changed_generator_alone_equals_a_full_rebuild() {
    let mut b = IndexBuilder::new([3; 16]);
    b.apply_packages(&generator_and_consumer("//g:gen"));
    let base = b.finish("base");

    let head = generator_and_consumer("//g:gen2");
    // Splice: only package g is re-queried; u is carried.
    let mut s = IndexBuilder::from_view(base.view());
    s.apply_packages(&head[..1]);
    let spliced = s.finish("head");
    // Full rebuild of the same head state.
    let mut f = IndexBuilder::new([3; 16]);
    f.apply_packages(&head);
    let rebuilt = f.finish("head");

    let (sv, rv) = (spliced.view(), rebuilt.view());
    let su = sv.id_of("//u:u").unwrap();
    let ru = rv.id_of("//u:u").unwrap();
    assert_eq!(forward_labels(sv, su), forward_labels(rv, ru));
    assert_eq!(forward_labels(sv, su), vec!["//g:out.h"]);
    let out = sv.id_of("//g:out.h").unwrap();
    assert_eq!(forward_labels(sv, out), vec!["//g:gen2"]);

    // And the walks agree: the new generator reaches the consumer, the old
    // one no longer does.
    for (label, expect) in [
        ("//g:gen2", vec!["//g:gen2", "//u:u"]),
        ("//g:gen", vec!["//g:gen"]),
    ] {
        for v in [sv, rv] {
            let r = walk(v, &[v.id_of(label).unwrap()], &WalkOptions::default());
            assert_eq!(view::labels_sorted(&v, &r.affected), expect, "from {label}");
        }
    }
}

#[test]
fn deleting_a_package_tombstones_its_targets() {
    let base = sample();
    let mut b = IndexBuilder::from_view(base.view());
    let gone = b.delete_package("c");
    assert_eq!(gone.len(), 3);
    let d = b.finish("3");
    let v = d.view();
    let m = v.labels.build_map();
    assert!(v.is_tombstone(m["//c:c"]));
    assert!(v.package_tombstoned(v.package_id_of(m["//c:c"]).unwrap()));
    // A walk from //a:a no longer reports the deleted targets.
    let r = walk(v, &[m["//a:a"]], &WalkOptions::default());
    assert_eq!(view::labels_sorted(&v, &r.affected), vec!["//a:a", "//b:b"]);
}

#[test]
fn manual_targets_are_excluded_by_default() {
    let mut b = IndexBuilder::new(new_lineage());
    let mut t = TargetRow::rule("//m:m", "synth_test", vec!["//m:dep".into()]);
    t.manual = true;
    t.is_test = true;
    b.apply_packages(&[pkg(
        "m",
        vec![t, TargetRow::rule("//m:dep", "synth_lib", vec![])],
    )]);
    let d = b.finish("1");
    let v = d.view();
    let m = v.labels.build_map();
    let r = walk(v, &[m["//m:dep"]], &WalkOptions::default());
    assert_eq!(view::labels_sorted(&v, &r.affected), vec!["//m:dep"]);
    let r = walk(
        v,
        &[m["//m:dep"]],
        &WalkOptions {
            include_manual: true,
            ..Default::default()
        },
    );
    assert_eq!(
        view::labels_sorted(&v, &r.affected),
        vec!["//m:dep", "//m:m"]
    );
}

#[test]
fn round_trips_through_the_on_disk_format() {
    let d = sample();
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join(index_file_name(&d.header.lineage, "deadbeef"));
    format::write_index(&path, &d).unwrap();

    let f = IndexFile::open(&path).unwrap();
    let v = f.view();
    let o = d.view();
    assert_eq!(v.commit, o.commit);
    assert_eq!(v.lineage, o.lineage);
    assert_eq!(v.num_nodes(), o.num_nodes());
    assert_eq!(v.forward.edges, o.forward.edges);
    assert_eq!(v.reverse.offsets, o.reverse.offsets);
    for i in 0..v.num_nodes() {
        assert_eq!(v.label(i), o.label(i));
        assert_eq!(v.kind[i as usize], o.kind[i as usize]);
        assert_eq!(v.hash[i as usize], o.hash[i as usize]);
    }
    // ...and a spliced copy of the mmapped index is still walkable.
    let mut b = IndexBuilder::from_view(v);
    b.apply_packages(&[pkg("e", vec![rule("e", "e", &["//c:c"], &[])])]);
    let head = b.finish("f00d");
    let hv = head.view();
    let m = hv.labels.build_map();
    let r = walk(hv, &[m["//a:a"]], &WalkOptions::default());
    assert!(view::labels_sorted(&hv, &r.affected).contains(&"//e:e".to_string()));
}

#[test]
fn rejects_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("junk.bin");
    std::fs::write(&p, vec![0u8; 128]).unwrap();
    assert!(matches!(
        IndexFile::open(&p),
        Err(IndexError::BadMagic { .. })
    ));
}

#[test]
fn serialised_output_is_deterministic() {
    let mut a = IndexBuilder::new([7; 16]);
    a.apply_packages(&[pkg("a", vec![rule("a", "a", &[], &[])])]);
    let mut b = IndexBuilder::new([7; 16]);
    b.apply_packages(&[pkg("a", vec![rule("a", "a", &[], &[])])]);
    let (mut x, mut y) = (a.finish("c"), b.finish("c"));
    // created_unix is the only wall-clock field; normalise it.
    x.provenance.created_unix = 0;
    y.provenance.created_unix = 0;
    assert_eq!(format::to_bytes(&x).unwrap(), format::to_bytes(&y).unwrap());
}

#[test]
fn walk_records_depth_and_honours_max_depth() {
    let d = sample();
    let v = d.view();
    let m = v.labels.build_map();
    let r = walk(v, &[m["//a:a"]], &WalkOptions::default());
    assert_eq!(r.depth_of(m["//a:a"]), Some(0));
    assert_eq!(r.depth_of(m["//b:b"]), Some(1));
    assert_eq!(r.depth_of(m["//c:c"]), Some(2));
    assert_eq!(r.depth_of(m["//c:c_test"]), Some(3));
    assert_eq!(r.depth_of(m["//a:a.src"]), None, "never visited");
    // Package boundaries: a -> b -> c each cross one; c -> c_test stays home.
    assert_eq!(r.package_depth_of(m["//a:a"]), Some(0));
    assert_eq!(r.package_depth_of(m["//b:b"]), Some(1));
    assert_eq!(r.package_depth_of(m["//c:c"]), Some(2));
    assert_eq!(
        r.package_depth_of(m["//c:c_test"]),
        Some(2),
        "same package as //c:c"
    );
    assert_eq!(r.package_depth_of(m["//a:a.src"]), None, "never visited");

    let r = walk(
        v,
        &[m["//a:a"]],
        &WalkOptions {
            max_depth: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(view::labels_sorted(&v, &r.affected), vec!["//a:a", "//b:b"]);
    assert_eq!(
        r.package_depth_of(m["//c:c"]),
        None,
        "max_depth bounds both distances"
    );
    let r = walk(
        v,
        &[m["//a:a"]],
        &WalkOptions {
            max_depth: Some(0),
            ..Default::default()
        },
    );
    assert_eq!(view::labels_sorted(&v, &r.affected), vec!["//a:a"]);
}

/// A consumer of a deleted target is reached *through* the tombstone, so the
/// tombstone is a hop: the consumer sits at depth 1 from the deletion, not 0.
#[test]
fn a_tombstoned_dependency_counts_as_a_hop() {
    let base = sample();
    let mut b = IndexBuilder::from_view(base.view());
    // Delete //b:b; //c:c keeps depending on the (now tombstoned) label.
    b.apply_packages(&[pkg("b", vec![TargetRow::source_file("//b:b.src")])]);
    let head = b.finish("head");
    let hv = head.view();
    let m = hv.labels.build_map();
    assert!(hv.is_tombstone(m["//b:b"]));
    let r = walk(hv, &[m["//b:b"]], &WalkOptions::default());
    assert_eq!(r.depth_of(m["//b:b"]), Some(0));
    assert_eq!(r.depth_of(m["//c:c"]), Some(1));
    assert_eq!(r.depth_of(m["//c:c_test"]), Some(2));
    // The tombstone itself is never reported.
    assert_eq!(
        view::labels_sorted(&hv, &r.affected),
        vec!["//c:c", "//c:c_test"]
    );
}

/// `id_of` is the whole point of a splice: node ids stay comparable across
/// generations, so a base id means the same node in the spliced index. Check
/// every label, not a sample — a silent renumbering of one of them is exactly
/// the bug this guards.
#[test]
fn splice_preserves_the_id_of_every_base_label() {
    let base = sample();
    let bv = base.view();

    let mut b = IndexBuilder::from_view(bv);
    // Touch one package; every other label must keep its id, and the touched
    // package's labels must too, since none of them went away.
    b.apply_packages(&[pkg(
        "b",
        vec![
            rule("b", "b", &["//a:a"], &["//b:b.src", "//b:extra.src"]),
            TargetRow::source_file("//b:b.src"),
            TargetRow::source_file("//b:extra.src"),
        ],
    )]);
    let head = b.finish("head");
    let hv = head.view();

    for label in bv.labels.iter() {
        assert_eq!(
            hv.id_of(label),
            bv.id_of(label),
            "{label} was renumbered by the splice"
        );
    }
    // And the new label got an id past the base's.
    assert!(hv.id_of("//b:extra.src").unwrap() >= bv.num_nodes());
}

/// The `affected` path: `finish_overlay` answers the same reverse-closure
/// question as a materialised splice, while carrying only the delta.
#[test]
fn overlay_answers_like_a_materialised_splice() {
    let base = sample();

    let touch = |b: &mut IndexBuilder<'_>| {
        // Give //a:a a second source; its consumers are unchanged, but the
        // rule's row moves, so the reverse corrections are real.
        b.apply_packages(&[pkg(
            "a",
            vec![
                rule("a", "a", &[], &["//a:a.src", "//a:new.src"]),
                TargetRow::source_file("//a:a.src"),
                TargetRow::source_file("//a:new.src"),
            ],
        )]);
    };

    let mut m = IndexBuilder::from_view(base.view());
    touch(&mut m);
    let materialised = m.finish("head");
    let mv = materialised.view();

    let mut o = IndexBuilder::from_view(base.view());
    touch(&mut o);
    let overlay = o.finish_overlay();

    let seed = "//a:new.src";
    let opts = WalkOptions::default();
    let from_index = walk(mv, &[mv.id_of(seed).unwrap()], &opts);
    let from_overlay = walk_view(&overlay, &[overlay.id_of(seed).unwrap()], &opts);
    assert_eq!(
        view::labels_sorted(&overlay, &from_overlay.affected),
        view::labels_sorted(&mv, &from_index.affected),
    );
    assert_eq!(
        view::labels_sorted(&overlay, &from_overlay.affected),
        vec!["//a:a", "//b:b", "//c:c", "//c:c_test"]
    );

    // It stayed a delta rather than a copy.
    let size = overlay.delta_size();
    assert!(size.new_nodes + size.changed_nodes < base.view().num_nodes() as usize);
}

/// `OwnedIndexBytes` is the "I already have the bytes" load path (the store
/// hands them over in memory); it must produce the same view as the builder.
#[test]
fn owned_bytes_round_trip() {
    let data = sample();
    let raw = format::to_bytes(&data).unwrap();
    let owned = format::OwnedIndexBytes::new(&raw).unwrap();
    let (a, b) = (data.view(), owned.view());

    assert_eq!(b.commit, a.commit);
    assert_eq!(b.lineage, a.lineage);
    assert_eq!(b.bazel_version, a.bazel_version);
    assert_eq!(b.num_nodes(), a.num_nodes());
    assert_eq!(b.num_packages(), a.num_packages());
    assert_eq!(
        b.labels.iter().collect::<Vec<_>>(),
        a.labels.iter().collect::<Vec<_>>()
    );
    for u in 0..a.num_nodes() {
        assert_eq!(b.forward.row(u), a.forward.row(u));
        assert_eq!(b.reverse.row(u), a.reverse.row(u));
        assert_eq!(b.kind_of(u), a.kind_of(u));
        assert_eq!(b.package_of(u), a.package_of(u));
        assert_eq!(b.rule_class(u), a.rule_class(u));
    }

    // A byte flipped inside the payload must not be read as valid rows.
    let mut broken = raw.clone();
    let off = u64::from_le_bytes(broken[16..24].try_into().unwrap()) as usize;
    broken.truncate(off + 8);
    assert!(format::OwnedIndexBytes::new(&broken).is_err());
}

fn bnd(dir: &str, bazelignored: bool) -> cambium_index::Boundary {
    cambium_index::Boundary {
        dir: dir.to_string(),
        bazelignored,
        traversed: false,
    }
}

fn walked(dir: &str) -> cambium_index::Boundary {
    cambium_index::Boundary {
        dir: dir.to_string(),
        bazelignored: false,
        traversed: true,
    }
}

#[test]
fn repository_boundaries_round_trip_and_survive_a_splice() {
    let mut b = IndexBuilder::new(new_lineage());
    b.set_repo_boundaries(vec![
        bnd("third_party/xla", false),
        bnd("api", true),
        bnd("third_party/xla", false),
    ]);
    b.apply_packages(&[pkg("a", vec![rule("a", "a", &[], &[])])]);
    let d = b.finish("deadbeef");
    assert_eq!(
        d.view().repo_boundaries().collect::<Vec<_>>(),
        vec!["api", "third_party/xla"]
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join(index_file_name(&d.header.lineage, "deadbeef"));
    format::write_index(&path, &d).unwrap();
    let f = IndexFile::open(&path).unwrap();
    assert_eq!(
        f.view().repo_boundaries().collect::<Vec<_>>(),
        vec!["api", "third_party/xla"]
    );
    // Which of them `.bazelignore` names travels with the list.
    assert_eq!(
        f.view().boundaries().collect::<Vec<_>>(),
        vec![bnd("api", true), bnd("third_party/xla", false)]
    );
    assert!(f.view().boundary_is_bazelignored("api"));
    assert!(!f.view().boundary_is_bazelignored("third_party/xla"));
    assert!(!f.view().boundary_is_bazelignored("nowhere"));
    assert!(!f.view().boundary_is_traversed("api"));

    // A splice that never looks at the tree keeps the base's answer.
    let mut b = IndexBuilder::from_view(f.view());
    b.apply_packages(&[pkg("e", vec![rule("e", "e", &[], &[])])]);
    assert_eq!(
        b.finish("f00d")
            .view()
            .repo_boundaries()
            .collect::<Vec<_>>(),
        vec!["api", "third_party/xla"]
    );
}

#[test]
fn an_index_with_no_boundaries_reads_as_empty() {
    assert_eq!(sample().view().repo_boundaries().count(), 0);
}

#[test]
fn walks_the_tree_for_nested_repositories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (d, marker) in [
        ("third_party/xla", "WORKSPACE"),
        ("third_party/xla/nested", "REPO.bazel"),
        ("api", "MODULE.bazel"),
        ("ignored_repo", "WORKSPACE"),
        ("ignored_plain/sub", "WORKSPACE"),
        ("src/lib", "BUILD"),
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
        std::fs::write(root.join(d).join(marker), "").unwrap();
    }
    std::fs::write(root.join("WORKSPACE"), "").unwrap();
    // An ignored directory that is a repository of its own is a boundary and
    // says so; one that is not stays skipped, and neither is descended into.
    std::fs::write(root.join(".bazelignore"), "ignored_repo\nignored_plain\n").unwrap();
    assert_eq!(
        repo_boundaries(root),
        vec![
            bnd("api", false),
            bnd("ignored_repo", true),
            bnd("third_party/xla", false),
            bnd("third_party/xla/nested", false),
        ]
    );
}

#[test]
fn whether_the_universe_walked_into_a_boundary_round_trips() {
    // Which markers stop `//...` differs by Bazel version, so the flag is
    // what the universe query returned rather than what the marker predicts:
    // xla's `third_party/tsl` carries a `REPO.bazel` and Bazel 8.7 loads
    // main-repo packages under it all the same.
    let mut b = IndexBuilder::new(new_lineage());
    b.set_repo_boundaries(vec![walked("third_party/tsl"), bnd("api", true)]);
    b.apply_packages(&[pkg(
        "third_party/tsl/tsl/platform",
        vec![rule("third_party/tsl/tsl/platform", "dso_loader", &[], &[])],
    )]);
    let d = b.finish("deadbeef");
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join(index_file_name(&d.header.lineage, "deadbeef"));
    format::write_index(&path, &d).unwrap();
    let f = IndexFile::open(&path).unwrap();
    assert!(f.view().boundary_is_traversed("third_party/tsl"));
    assert!(!f.view().boundary_is_traversed("api"));
    assert!(!f.view().boundary_is_traversed("nowhere"));
    assert_eq!(
        f.view().boundaries().collect::<Vec<_>>(),
        vec![bnd("api", true), walked("third_party/tsl")]
    );

    // A splice keeps the base's answer: the run that makes one asks no
    // universe query and has nothing newer to say.
    let mut b = IndexBuilder::from_view(f.view());
    b.apply_packages(&[pkg("e", vec![rule("e", "e", &[], &[])])]);
    assert!(b.finish("f00d").view().boundary_is_traversed("third_party/tsl"));
}

#[test]
fn a_boundary_is_traversed_when_the_universe_returned_a_package_under_it() {
    let mut bs = vec![
        cambium_index::Boundary {
            dir: "third_party/tsl".into(),
            bazelignored: false,
            traversed: false,
        },
        cambium_index::Boundary {
            dir: "api".into(),
            bazelignored: true,
            traversed: true,
        },
    ];
    cambium_index::mark_traversed(
        &mut bs,
        ["lib", "third_party/tsl/tsl/platform", "third_party/tslx"],
    );
    assert!(bs[0].traversed);
    // A stale `true` is cleared: the answer is this universe query's, and a
    // directory name that merely shares a prefix is not under the boundary.
    assert!(!bs[1].traversed);
}
