//! A checked-in index file, opened by the current code.
//!
//! The on-disk layout is produced by rkyv's derive, so a rkyv upgrade can move
//! it without a single line of cambium changing — and a moved layout that
//! `FORMAT_VERSION` did not follow makes every published baseline unreadable,
//! or worse, readable as the wrong rows. `rkyv` is therefore pinned to an exact
//! version in the workspace manifest, and this test is what notices if the pin
//! is ever relaxed and the bytes shift underneath us.
//!
//! To regenerate after a deliberate `FORMAT_VERSION` bump:
//!
//! ```text
//! CAMBIUM_REGENERATE_FIXTURE=1 cargo test -p cambium-index --test format_fixture
//! ```

use cambium_index::*;

const FIXTURE: &str = "tests/fixtures/tiny.bin";

/// A three-package chain with one rule and one source file each, built from a
/// fixed lineage and commit so the bytes are reproducible.
fn tiny() -> IndexData {
    let mut b = IndexBuilder::new([9; 16]);
    b.set_bazel_version("7.4.1");
    let mut packages = Vec::new();
    for (name, dep) in [("a", None), ("b", Some("//a:a")), ("c", Some("//b:b"))] {
        let src = format!("//{name}:{name}.src");
        let mut inputs = vec![src.clone()];
        inputs.extend(dep.map(str::to_string));
        let mut r = TargetRow::rule(label::rule_label(name, name), "synth_lib", inputs);
        r.rule_hash = *blake3::hash(name.as_bytes()).as_bytes();
        packages.push(PackageUpdate {
            package: name.to_string(),
            build_hash: *blake3::hash(name.as_bytes()).as_bytes(),
            bzl_hash: [0; 32],
            targets: vec![r, TargetRow::source_file(&src)],
        });
    }
    b.apply_packages(&packages);
    b.finish("0123456789abcdef")
}

#[test]
fn checked_in_fixture_still_reads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    if std::env::var_os("CAMBIUM_REGENERATE_FIXTURE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format::to_bytes(&tiny()).unwrap()).unwrap();
    }
    let raw = std::fs::read(&path).expect("fixture is checked in; see this file's header");
    let owned = format::OwnedIndexBytes::new(&raw).expect("fixture opens");
    let v = owned.view();

    assert_eq!(v.format_version, format::FORMAT_VERSION);
    assert_eq!(v.commit, "0123456789abcdef");
    assert_eq!(v.lineage, [9; 16]);
    assert_eq!(v.bazel_version, "7.4.1");
    assert_eq!(v.num_packages(), 3);
    assert_eq!(v.num_nodes(), 6);

    let ids = v.labels.build_map();
    for name in ["a", "b", "c"] {
        let rule = ids[format!("//{name}:{name}").as_str()];
        let src = ids[format!("//{name}:{name}.src").as_str()];
        assert_eq!(v.kind_of(rule), NodeKind::Rule);
        assert_eq!(v.kind_of(src), NodeKind::SourceFile);
        assert_eq!(v.package_of(rule), Some(name));
        assert_eq!(v.rule_class(rule), Some("synth_lib"));
        assert!(v.forward.row(rule).contains(&src));
        assert!(v.reverse.row(src).contains(&rule));
    }
    assert!(v.forward.row(ids["//b:b"]).contains(&ids["//a:a"]));
    assert!(v.forward.row(ids["//c:c"]).contains(&ids["//b:b"]));

    // The payload this build writes for the same graph must still be the
    // payload on disk, byte for byte: a layout shift under a relaxed rkyv pin
    // shows up here even if every field above still happens to read back. The
    // trailer is excluded because it carries the build's wall-clock time.
    let off = u64::from_le_bytes(raw[16..24].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(raw[24..32].try_into().unwrap()) as usize;
    assert_eq!(
        format::payload_bytes(&tiny()).unwrap(),
        raw[off..off + len],
        "the archive layout moved; see this file's header"
    );
}
