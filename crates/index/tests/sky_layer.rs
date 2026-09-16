//! The Skyframe loading layer, through a written index file.
//!
//! `crates/index/src/sky/tests.rs` checks the layer itself against real dumps.
//! This checks the half that only a whole index can: that the layer survives
//! being archived, mapped back and spliced, and that a base without one says
//! so rather than reading as an empty graph.

use cambium_index::sky::{Kind, Paths, SkyLayer};
use cambium_index::*;

const DUMP: &str = "\
PACKAGE:a
  Group 1:
    PACKAGE_LOOKUP:a

  Group 2:
    FILE:[/ws]/[a/BUILD]

  Group 3:
    PACKAGE_LOOKUP:a/sub

PACKAGE_LOOKUP:a
  Group 1:
    FILE:[/ws]/[a/BUILD]

PACKAGE_LOOKUP:a/sub
  Group 1:
    FILE:[/ws]/[a/sub/BUILD]

FILE:[/ws]/[a/BUILD]
  Group 1:
    FILE_STATE:[/ws]/[a/BUILD]

FILE:[/ws]/[a/sub/BUILD]
  Group 1:
    FILE_STATE:[/ws]/[a/sub/BUILD]

FILE_STATE:[/ws]/[a/BUILD]

FILE_STATE:[/ws]/[a/sub/BUILD]
";

fn layer() -> SkyLayer {
    let mut l = SkyLayer::new();
    l.apply_dump(DUMP, &Paths::new("/ws", "/ob"));
    l
}

fn package(name: &str) -> PackageUpdate {
    PackageUpdate {
        package: name.to_string(),
        build_hash: *blake3::hash(name.as_bytes()).as_bytes(),
        bzl_hash: [0; 32],
        targets: vec![TargetRow::rule(
            label::rule_label(name, "lib"),
            "cc_library",
            vec![],
        )],
    }
}

fn built() -> IndexData {
    let mut b = IndexBuilder::new(new_lineage());
    b.apply_packages(&[package("a")]);
    b.set_sky_layer(layer());
    b.finish("head")
}

/// The layer goes through the archive unchanged, and a walk over the mapped
/// bytes answers what a walk over the owned form answered.
#[test]
fn a_layer_survives_being_written_and_mapped_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    format::write_index(&path, &built()).unwrap();
    let f = IndexFile::open(&path).unwrap();
    let v = f.view();

    assert_eq!(v.format_version, format::FORMAT_VERSION);
    assert!(v.sky.present, "the header records that there is a layer");
    assert_eq!(v.sky.len(), 7);
    assert!(v.sky.id_of(Kind::Package, "a").is_some());
    // The workspace root is stored as the empty string, so the payload names
    // no path of the machine that built it.
    assert_eq!(v.sky.roots.get(cambium_index::sky::WORKSPACE), "");
    assert!(!String::from_utf8_lossy(v.sky.keys.bytes).contains("/ws"));

    let w = v.sky.walk(&cambium_index::sky::seeds([("a/sub/BUILD", true)]));
    assert!(w.packages.contains("a"), "{w:?}");
    assert!(w.lookups.contains("a/sub"), "{w:?}");
}

/// An index built without a dump says so. "Bazel's loading phase touched
/// nothing" and "nobody asked Bazel" are the same empty arrays, and a run over
/// the second has to fail with a reason rather than read it as "nothing is
/// dirty".
#[test]
fn an_index_built_without_a_dump_has_no_layer_rather_than_an_empty_one() {
    let mut b = IndexBuilder::new(new_lineage());
    b.apply_packages(&[package("a")]);
    let data = b.finish("head");
    let v = data.view();
    assert!(!v.sky.present);
    assert!(v.sky.is_empty());
    assert_eq!(
        v.sky.walk(&cambium_index::sky::seeds([("a/BUILD", false)])),
        Default::default()
    );
}

/// A splice that does not touch the layer carries it forward whole: the
/// packages a run did not re-query keep the rows they had, which is what makes
/// the incremental re-dump a small one.
#[test]
fn a_splice_carries_the_layer_forward() {
    let base = built();
    let mut b = IndexBuilder::from_view(base.view());
    b.apply_packages(&[package("a")]);
    let head = b.finish("head2");
    let v = head.view();
    assert!(v.sky.present);
    assert_eq!(v.sky.len(), base.view().sky.len());
    assert!(v
        .sky
        .walk(&cambium_index::sky::seeds([("a/sub/BUILD", true)]))
        .packages
        .contains("a"));
}

/// A splice that refreshes the layer replaces the rows it names and leaves the
/// rest, and a deleted package's row is retired with it.
#[test]
fn a_splice_can_refresh_and_retire_rows() {
    let base = built();
    let mut b = IndexBuilder::from_view(base.view());
    let mut l = b.take_sky_layer().expect("the base has a layer");
    l.delete_package("a");
    b.set_sky_layer(l);
    b.delete_package("a");
    let head = b.finish("head2");
    let v = head.view();
    assert!(v.sky.present);
    assert!(v.sky.id_of(Kind::Package, "a").is_none());
    let w = v.sky.walk(&cambium_index::sky::seeds([("a/BUILD", false)]));
    assert!(w.packages.is_empty(), "{w:?}");
}
