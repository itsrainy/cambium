//! The emitted workspace against the in-memory graph.
//!
//! Three things have to agree: the index built straight from the graph, the
//! files `emit-bazel` writes, and the files `SynthFiles` serves the engine from
//! memory. A disagreement between any two would let a property test pass on a
//! graph Bazel itself would reject.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cambium_index::FileText;
use cambium_synth::{apply_one, emit_workspace, generate, GraphSpec, Mutation, SynthFiles};

fn tiny() -> cambium_synth::SynthGraph {
    generate(&GraphSpec::preset("tiny").unwrap())
}

#[test]
fn build_index_is_byte_identical_across_runs() {
    let a = tiny().build_index("c0");
    let mut b = tiny().build_index("c0");
    // The lineage is minted fresh and the timestamp is the clock; everything
    // else must be a function of the graph.
    b.header.lineage = a.header.lineage;
    b.provenance.created_unix = a.provenance.created_unix;

    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (dir.path().join("a.bin"), dir.path().join("b.bin"));
    cambium_index::format::write_index(&pa, &a).unwrap();
    cambium_index::format::write_index(&pb, &b).unwrap();
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        std::fs::read(&pb).unwrap(),
        "two builds of the same graph wrote different bytes"
    );
}

/// Every file the emitter writes that the in-memory server also answers for
/// must come back with the same text — or, for a BUILD file, with the prologue
/// the emitted file starts with, which is the part the engine's `.bzl` scan
/// reads.
#[test]
fn emitted_files_match_what_synth_files_serves() {
    let g = tiny();
    let dir = tempfile::tempdir().unwrap();
    emit_workspace(&g, dir.path()).unwrap();
    let files = SynthFiles(&g);

    let mut checked = 0;
    for path in walk(dir.path()) {
        let rel = path.strip_prefix(dir.path()).unwrap().to_str().unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        if rel == "MODULE.bazel" || name.ends_with(".bzl") {
            // The external module's files are Bazel's to read, not the engine's.
            if rel.starts_with("external/") {
                continue;
            }
            assert_eq!(
                files.text(rel).as_deref(),
                Some(on_disk.as_str()),
                "{rel} differs between disk and memory"
            );
            checked += 1;
        } else if name == "BUILD.bazel" {
            if let Some(served) = files.text(rel) {
                assert!(
                    on_disk.starts_with(&served),
                    "{rel} on disk does not start with the served prologue:\n{served}\n---\n{on_disk}"
                );
                checked += 1;
            }
        }
    }
    // Every generated .bzl, the shared rules, MODULE.bazel and every package.
    assert!(
        checked >= g.bzl.len() + 2 + g.packages.iter().filter(|p| p.alive).count(),
        "only {checked} files were compared"
    );
}

/// Generation is a pure function of the spec, for every shape the presets
/// name. The larger sizes are scaled down to the small preset's target count:
/// what is under test is the shape's code path, not its size, and the xxl
/// preset alone is ten million targets.
#[test]
fn every_preset_generates_deterministically() {
    for name in GraphSpec::PRESETS {
        let mut spec = GraphSpec::preset(name).unwrap();
        spec.targets = spec.targets.min(2_000);
        let a = generate(&spec);
        let b = generate(&spec);
        assert_eq!(a.targets.len(), b.targets.len(), "{name}");
        assert_eq!(a.packages.len(), b.packages.len(), "{name}");
        for i in 0..a.targets.len() {
            assert_eq!(a.targets[i].deps, b.targets[i].deps, "{name} target {i}");
            assert_eq!(a.targets[i].srcs, b.targets[i].srcs, "{name} target {i}");
            assert_eq!(
                a.target_hash(i as u32),
                b.target_hash(i as u32),
                "{name} target {i}"
            );
        }
        for (pa, pb) in a.packages.iter().zip(&b.packages) {
            assert_eq!(pa.path, pb.path, "{name}");
            assert_eq!(pa.exported, pb.exported, "{name}");
            assert_eq!(pa.bzl, pb.bzl, "{name}");
        }
        assert_eq!(
            a.build_index("x").header.counts.edges,
            b.build_index("x").header.counts.edges,
            "{name}"
        );
    }
}

/// After a package split that moves a target whose file another package
/// consumes through `exports_files`, every label in every emitted BUILD file
/// still resolves: the rule is defined in the package it names, or the file
/// exists there and is exported.
#[test]
fn split_then_emit_leaves_every_label_resolvable() {
    let mut g = generate(&GraphSpec::preset("small").unwrap());
    let (pkg, et) = g
        .packages
        .iter()
        .enumerate()
        .find_map(|(i, p)| p.exported.first().map(|&(t, _)| (i as u32, t)))
        .expect("the small preset exports a file");
    apply_one(
        &mut g,
        &Mutation::SplitPackage {
            package: pkg,
            targets: vec![et],
        },
    );
    // And delete something with dependents, so a dead label is on offer.
    let victim = (0..g.targets.len() as u32)
        .find(|&i| i != et && g.targets.iter().any(|t| t.alive && t.deps.contains(&i)))
        .unwrap();
    apply_one(&mut g, &Mutation::DeleteTarget { target: victim });

    let dir = tempfile::tempdir().unwrap();
    emit_workspace(&g, dir.path()).unwrap();
    assert_labels_resolve(dir.path());
}

/// The base graph passes the same check, so the split test's pass is not
/// vacuous for want of references.
#[test]
fn a_fresh_emit_leaves_every_label_resolvable() {
    let dir = tempfile::tempdir().unwrap();
    emit_workspace(&tiny(), dir.path()).unwrap();
    assert_labels_resolve(dir.path());
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                if e.file_name() != ".git" {
                    stack.push(p);
                }
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// A BUILD file's rule names and its `exports_files` list.
struct Package {
    rules: BTreeSet<String>,
    exported: BTreeSet<String>,
}

fn parse_build(text: &str) -> Package {
    let mut rules = BTreeSet::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("name = \"") {
            if let Some(name) = rest.strip_suffix("\",") {
                rules.insert(name.to_string());
            }
        }
    }
    let mut exported = BTreeSet::new();
    if let Some(i) = text.find("exports_files([") {
        let rest = &text[i + "exports_files([".len()..];
        let list = &rest[..rest.find("])").unwrap()];
        for f in list.split(',') {
            exported.insert(f.trim().trim_matches('"').to_string());
        }
    }
    Package { rules, exported }
}

/// Every `"//pkg:name"` and `"@synth_ext//ext:name"` string in every BUILD
/// file names something that exists.
fn assert_labels_resolve(root: &Path) {
    let mut packages: BTreeMap<String, Package> = BTreeMap::new();
    let mut references: Vec<(String, String, String)> = Vec::new(); // (from, pkg, name)
    let ext_root = root.join("external/synth_ext");

    for path in walk(root) {
        if path.file_name().unwrap() != "BUILD.bazel" || path.starts_with(&ext_root) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        let pkg = rel.strip_suffix("/BUILD.bazel").unwrap_or("").to_string();
        let parsed = parse_build(&text);
        for f in &parsed.exported {
            assert!(
                root.join(&pkg).join(f).exists(),
                "{rel}: exports_files names {f}, which is not in the package"
            );
        }
        packages.insert(pkg, parsed);
        for piece in text.split('"').skip(1).step_by(2) {
            if let Some(rest) = piece.strip_prefix("//") {
                if let Some((p, n)) = rest.split_once(':') {
                    // `//visibility:public` and `//conditions:default` are Bazel's own.
                    if p != "visibility" && p != "conditions" {
                        references.push((rel.clone(), p.to_string(), n.to_string()));
                    }
                }
            } else if let Some(rest) = piece.strip_prefix("@synth_ext//ext:") {
                references.push((rel.clone(), "@synth_ext//ext".to_string(), rest.to_string()));
            }
        }
    }
    let ext_build = std::fs::read_to_string(ext_root.join("ext/BUILD.bazel")).unwrap();
    let ext = parse_build(&ext_build);
    assert!(!references.is_empty());

    for (from, pkg, name) in references {
        if pkg == "@synth_ext//ext" {
            assert!(
                ext.rules.contains(&name),
                "{from}: @synth_ext//ext:{name} does not exist"
            );
            continue;
        }
        let Some(p) = packages.get(&pkg) else {
            panic!("{from}: //{pkg}:{name} names a package with no BUILD file");
        };
        if p.rules.contains(&name) {
            continue;
        }
        let file = root.join(&pkg).join(&name);
        assert!(
            file.exists(),
            "{from}: //{pkg}:{name} is neither a rule nor a file"
        );
        let same_package = from.strip_suffix("/BUILD.bazel").unwrap_or("") == pkg;
        assert!(
            same_package || name.ends_with(".bzl") || p.exported.contains(&name),
            "{from}: //{pkg}:{name} is a file another package uses but //{pkg} does not export it"
        );
    }
}
