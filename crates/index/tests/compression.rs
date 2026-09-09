//! Indexes compressed at rest.
//!
//! The reader takes both forms, so what these pin is that neither form is
//! special: the same index written raw and written as a zstd frame reads back
//! the same, a raw file from before compression existed still opens, and a
//! damaged frame is an error rather than a panic.

use cambium_index::format::{self, Compression};
use cambium_index::*;

fn tiny() -> IndexData {
    let mut b = IndexBuilder::new([7; 16]);
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
    b.finish("feedfacefeedface")
}

fn check(v: IndexRef<'_>) {
    assert_eq!(v.format_version, format::FORMAT_VERSION);
    assert_eq!(v.commit, "feedfacefeedface");
    assert_eq!(v.lineage, [7; 16]);
    assert_eq!(v.bazel_version, "7.4.1");
    assert_eq!(v.num_packages(), 3);
    assert_eq!(v.num_nodes(), 6);
    let ids = v.labels.build_map();
    let b = ids["//b:b"];
    assert!(v.forward.row(b).contains(&ids["//a:a"]));
}

/// The compressed file starts with zstd's magic, not cambium's, and reads
/// back as the same index.
#[test]
fn a_compressed_index_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index-deadbeef.bin");
    format::write_index(&path, &tiny()).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(format::is_zstd(&bytes), "write_index compresses by default");
    assert_ne!(bytes[..8], format::MAGIC);

    let f = IndexFile::open(&path).unwrap();
    assert!(f.is_compressed());
    check(f.view());
    assert_eq!(
        f.deserialize().unwrap().provenance.commit,
        "feedfacefeedface"
    );
}

/// The raw form still works, and the name does not change either way.
#[test]
fn a_raw_index_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index-deadbeef.bin");
    format::write_index_with(&path, &tiny(), Compression::None).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes[..8], format::MAGIC);
    assert!(!format::is_zstd(&bytes));

    let f = IndexFile::open(&path).unwrap();
    assert!(
        !f.is_compressed(),
        "the raw form is mapped, not decompressed"
    );
    check(f.view());
}

/// `Compression::None` is byte-for-byte what the code before compression
/// wrote, so a file from an older cambium opens unchanged — including the
/// checked-in fixture, which stays raw for exactly this reason.
#[test]
fn a_raw_file_written_by_the_old_code_still_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.bin");
    // What `to_bytes` produced then and produces now: the framed layout,
    // written straight out with no wrapper.
    std::fs::write(&path, format::to_bytes(&tiny()).unwrap()).unwrap();
    check(IndexFile::open(&path).unwrap().view());

    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.bin");
    let raw = std::fs::read(&fixture).unwrap();
    assert!(!format::is_zstd(&raw), "the fixture is checked in raw");
    assert!(IndexFile::open(&fixture).is_ok());
}

/// The compressed form of the checked-in fixture is smaller than the raw one.
#[test]
fn the_fixture_compresses() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.bin");
    let raw = std::fs::read(&fixture).unwrap();
    let compressed = zstd::stream::encode_all(&raw[..], format::DEFAULT_ZSTD_LEVEL).unwrap();
    assert!(
        compressed.len() < raw.len(),
        "compressed {} is not smaller than raw {}",
        compressed.len(),
        raw.len()
    );
    // And the same for what the writer itself produces.
    let plain = format::to_bytes(&tiny()).unwrap();
    let framed = format::to_bytes_with(&tiny(), Compression::default()).unwrap();
    assert!(
        framed.len() < plain.len(),
        "{} vs {}",
        framed.len(),
        plain.len()
    );
}

/// A frame that stops halfway is an error naming the file, not a panic and
/// not a bad-magic report about bytes nobody could read.
#[test]
fn a_truncated_frame_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cut.bin");
    let full = format::to_bytes_with(&tiny(), Compression::default()).unwrap();
    std::fs::write(&path, &full[..full.len() / 2]).unwrap();

    let Err(err) = IndexFile::open(&path) else {
        panic!("a half-written frame must not open");
    };
    assert!(
        matches!(err, IndexError::Decompress { .. }),
        "expected a decompression error, got {err:?}"
    );
    assert!(format::OwnedIndexBytes::new(&full[..full.len() / 2]).is_err());
}

/// A frame whose body is corrupt fails its checksum rather than being parsed
/// as an index.
#[test]
fn a_corrupt_frame_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.bin");
    let mut bytes = format::to_bytes_with(&tiny(), Compression::default()).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let Err(err) = IndexFile::open(&path) else {
        panic!("a corrupt frame must not open");
    };
    // The frame carries a content checksum precisely so this is caught here,
    // before the archive — which is cast in place, not validated — is touched.
    assert!(
        matches!(err, IndexError::Decompress { .. }),
        "expected a decompression error, got {err:?}"
    );
}

/// Bytes handed to the reader directly — the shape a store fetch has — take
/// either form too.
#[test]
fn owned_bytes_take_either_form() {
    let raw = format::to_bytes(&tiny()).unwrap();
    let compressed = format::to_bytes_with(&tiny(), Compression::default()).unwrap();
    check(format::OwnedIndexBytes::new(&raw).unwrap().view());
    check(format::OwnedIndexBytes::new(&compressed).unwrap().view());
}
