//! Spec 12.1: CSR construction, splice, serialisation round trip.

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
    let labels = walk::labels_sorted(v, &r.affected);
    assert_eq!(labels, vec!["//a:a", "//b:b", "//c:c", "//c:c_test"]);
    let tests = walk::labels_sorted(v, &r.tests);
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
        walk::labels_sorted(v, &r.affected),
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

#[test]
fn generated_files_redirect_to_their_rule() {
    let mut b = IndexBuilder::new(new_lineage());
    let mut gen = TargetRow::source_file("//g:out.h");
    gen.kind = NodeKind::GeneratedFile;
    gen.generating_rule = Some("//g:gen".into());
    b.apply_packages(&[
        pkg(
            "g",
            vec![TargetRow::rule("//g:gen", "genrule", vec![]), gen],
        ),
        pkg(
            "u",
            vec![TargetRow::rule(
                "//u:u",
                "synth_lib",
                vec!["//g:out.h".into()],
            )],
        ),
    ]);
    let d = b.finish("1");
    let v = d.view();
    let m = v.labels.build_map();
    // The consumer points straight at the generating rule.
    assert_eq!(v.forward.row(m["//u:u"]), &[m["//g:gen"]]);
    // ...and a change to the generator reaches the consumer in one hop.
    let r = walk(v, &[m["//g:gen"]], &WalkOptions::default());
    assert!(walk::labels_sorted(v, &r.affected).contains(&"//u:u".to_string()));
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
    assert_eq!(walk::labels_sorted(v, &r.affected), vec!["//a:a", "//b:b"]);
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
    assert_eq!(walk::labels_sorted(v, &r.affected), vec!["//m:dep"]);
    let r = walk(
        v,
        &[m["//m:dep"]],
        &WalkOptions {
            include_manual: true,
            ..Default::default()
        },
    );
    assert_eq!(
        walk::labels_sorted(v, &r.affected),
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
    assert!(walk::labels_sorted(hv, &r.affected).contains(&"//e:e".to_string()));
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
