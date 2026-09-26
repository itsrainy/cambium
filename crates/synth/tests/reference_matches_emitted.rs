//! The reference answer against the emitted workspace.
//!
//! `reference_affected` reads the in-memory graph. Bazel reads the files
//! `emit_workspace` writes. If the graph holds an edge or an input the BUILD
//! files do not name, the reference marks targets Bazel would leave alone, and
//! every synthetic soundness test is checked against an answer that is too big.
//!
//! So this walks a history one mutation at a time and recomputes the affected
//! set from the workspace's files alone: each rule's stanza text, the `.bzl`
//! files its package loads, the source files it globs or names, and,
//! transitively, the rules its `deps`, `actual` or `tests` name. The two
//! answers must be equal. The files come from `package_files`, the list the
//! emitter writes, because writing every one to disk after every mutation
//! takes seconds; the walk checks that the files on disk read the same at its
//! start and end.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cambium_index::FileText;
use cambium_synth::{
    apply_one, commit, emit_workspace, generate, package_files, reference_affected, ChangeSpec,
    GraphSpec, SynthFiles, SynthGraph,
};

/// A workspace's files, by workspace-relative path.
type Tree = BTreeMap<String, String>;

/// The files on disk under `root`, leaving out `.git` and the external module,
/// which never changes.
fn tree_on_disk(root: &Path) -> Tree {
    let mut out = Tree::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let name = e.file_name();
            if p.is_dir() {
                if name != ".git" && name != "external" {
                    stack.push(p);
                }
            } else {
                let rel = p.strip_prefix(root).unwrap().to_str().unwrap().to_string();
                out.insert(rel, std::fs::read_to_string(&p).unwrap());
            }
        }
    }
    out
}

/// The same files without touching the disk: every live package's
/// `package_files`, and the `.bzl` files packages load.
fn tree_in_memory(g: &SynthGraph) -> Tree {
    use rayon::prelude::*;
    let mut out: Tree = (0..g.packages.len() as u32)
        .into_par_iter()
        .filter(|&p| g.packages[p as usize].alive)
        .flat_map_iter(|p| package_files(g, p))
        .collect();
    let served = SynthFiles(g);
    for path in g
        .bzl
        .iter()
        .map(|b| b.path.as_str())
        .chain(["rules/synth.bzl"])
    {
        out.insert(path.to_string(), served.text(path).unwrap());
    }
    out
}

/// One rule as the BUILD file declares it.
struct Stanza {
    /// The stanza's text, which carries every attribute Bazel sees.
    text: String,
    /// Files the package loads, by workspace-relative path.
    loads: Vec<String>,
    /// Source files the rule reads, by workspace-relative path.
    files: Vec<String>,
    /// Rule labels it names in `deps`, `actual` or `tests`.
    deps: Vec<String>,
}

/// Every rule in every subject package, by label. The shared rule
/// definitions under `rules/` and `tools/` are not subject packages.
fn read_rules(tree: &Tree) -> BTreeMap<String, Stanza> {
    let mut listing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for path in tree.keys() {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        listing.entry(dir).or_default().push(name);
    }
    let mut out = BTreeMap::new();
    for (dir, names) in &listing {
        if !names.contains(&"BUILD.bazel") || dir.starts_with("rules") || dir.starts_with("tools") {
            continue;
        }
        let build = if dir.is_empty() {
            "BUILD.bazel".to_string()
        } else {
            format!("{dir}/BUILD.bazel")
        };
        read_package(dir, &tree[&build], names, &mut out);
    }
    out
}

fn read_package(pkg: &str, text: &str, dir_files: &[&str], out: &mut BTreeMap<String, Stanza>) {
    let file_path = |name: &str| {
        if pkg.is_empty() {
            name.to_string()
        } else {
            format!("{pkg}/{name}")
        }
    };

    let mut loads = Vec::new();
    for chunk in text.split("\n\n") {
        let chunk = chunk.trim();
        if chunk.is_empty() || chunk.starts_with("exports_files(") {
            continue;
        }
        if chunk.starts_with("load(") {
            for line in chunk.lines() {
                let label = line
                    .strip_prefix("load(\"//")
                    .and_then(|r| r.split('"').next())
                    .expect("a load line names a repository label");
                let (p, f) = label.split_once(':').unwrap();
                loads.push(format!("{p}/{f}"));
            }
            continue;
        }
        let name = chunk
            .lines()
            .find_map(|l| l.trim().strip_prefix("name = \""))
            .and_then(|r| r.strip_suffix("\","))
            .unwrap_or_else(|| panic!("//{pkg}: a stanza with no name:\n{chunk}"));

        let mut files = Vec::new();
        let mut deps = Vec::new();
        // The per-target glob, matched against the package's files.
        if let Some(i) = chunk.find("glob([\"") {
            let pat = chunk[i + "glob([\"".len()..].split('"').next().unwrap();
            let (pre, suf) = pat.split_once('*').expect("a glob with one star");
            for f in dir_files {
                if f.starts_with(pre) && f.ends_with(suf) && f.len() >= pre.len() + suf.len() {
                    files.push(file_path(f));
                }
            }
        }
        for piece in chunk.split('"').skip(1).step_by(2) {
            let Some(rest) = piece.strip_prefix("//") else {
                continue;
            };
            let (p, n) = rest.split_once(':').unwrap();
            // Bazel's own labels, and the constant config_setting.
            if matches!(p, "visibility" | "conditions" | "tools/synth") {
                continue;
            }
            if n.ends_with(".src") {
                files.push(format!("{p}/{n}"));
            } else {
                deps.push(format!("//{p}:{n}"));
            }
        }
        files.sort();
        files.dedup();
        out.insert(
            format!("//{pkg}:{name}"),
            Stanza {
                text: chunk.to_string(),
                loads: loads.clone(),
                files,
                deps,
            },
        );
    }
}

/// A deep hash per rule, from the files alone.
fn deep_hashes(tree: &Tree) -> BTreeMap<String, [u8; 32]> {
    let rules = read_rules(tree);
    let mut memo: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    for label in rules.keys() {
        deep(tree, &rules, label, &mut memo);
    }
    memo
}

fn deep(
    tree: &Tree,
    rules: &BTreeMap<String, Stanza>,
    label: &str,
    memo: &mut BTreeMap<String, [u8; 32]>,
) -> [u8; 32] {
    if let Some(h) = memo.get(label) {
        return *h;
    }
    let s = rules
        .get(label)
        .unwrap_or_else(|| panic!("{label} is named but no BUILD file defines it"));
    let mut h = blake3::Hasher::new();
    h.update(s.text.as_bytes());
    for path in s.loads.iter().chain(&s.files) {
        let text = tree
            .get(path)
            .unwrap_or_else(|| panic!("{label} reads {path}, which does not exist"));
        h.update(path.as_bytes());
        h.update(text.as_bytes());
    }
    let mut deps: Vec<[u8; 32]> = s.deps.iter().map(|d| deep(tree, rules, d, memo)).collect();
    deps.sort();
    for d in deps {
        h.update(&d);
    }
    let out = *h.finalize().as_bytes();
    memo.insert(label.to_string(), out);
    out
}

/// The files on disk and the files in memory read as the same rules.
fn assert_disk_matches_memory(g: &SynthGraph, dir: &Path) {
    emit_workspace(g, dir).unwrap();
    assert_eq!(
        deep_hashes(&tree_on_disk(dir)),
        deep_hashes(&tree_in_memory(g)),
        "the emitted workspace and package_files disagree"
    );
}

/// Replays `revisions` commits of `change` over `graph` one mutation at a
/// time, and returns how many mutations it checked and a line for each one
/// whose two answers differ. With `on_disk`, the files on disk are checked
/// against the in-memory ones before the first mutation and after the last.
///
/// The reference side is [`cambium_synth::deep_hashes`] diffed across the
/// mutation, which is what `reference_affected` computes, taken once per
/// graph rather than twice.
fn walk(
    graph: SynthGraph,
    change: &ChangeSpec,
    revisions: u64,
    on_disk: bool,
) -> (usize, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut g = graph;
    if on_disk {
        assert_disk_matches_memory(&g, dir.path());
    }
    let both = |g: &SynthGraph| {
        rayon::join(
            || deep_hashes(&tree_in_memory(g)),
            || cambium_synth::deep_hashes(g),
        )
    };
    let (mut files_before, mut ref_before) = both(&g);
    let changed = |before: &BTreeMap<String, [u8; 32]>, after: &BTreeMap<String, [u8; 32]>| {
        after
            .iter()
            .filter(|(l, h)| before.get(*l) != Some(*h))
            .map(|(l, _)| l.clone())
            .collect::<BTreeSet<String>>()
    };
    let mut checked = 0;
    let mut wrong = Vec::new();
    for rev in 0..revisions {
        // The commit's own mutations, drawn against the graph as it stands,
        // then applied one by one.
        let mutations = commit(&mut g.clone(), change, rev).mutations;
        for m in mutations {
            apply_one(&mut g, &m);
            let (files_after, ref_after) = both(&g);
            let from_files = changed(&files_before, &files_after);
            let reference = changed(&ref_before, &ref_after);
            if reference != from_files {
                let over: Vec<_> = reference.difference(&from_files).collect();
                let under: Vec<_> = from_files.difference(&reference).collect();
                wrong.push(format!(
                    "rev {rev} {m:?}: {} only in the reference {:?}, {} only in the files {:?}",
                    over.len(),
                    &over[..over.len().min(3)],
                    under.len(),
                    &under[..under.len().min(3)],
                ));
            }
            checked += 1;
            (files_before, ref_before) = (files_after, ref_after);
        }
    }
    if on_disk {
        assert_disk_matches_memory(&g, dir.path());
    }
    // And the shortcut is the function the soundness tests call.
    let base = commit(&mut g.clone(), change, revisions).mutations;
    let mut after = g.clone();
    for m in &base {
        apply_one(&mut after, m);
    }
    assert_eq!(
        reference_affected(&g, &after),
        changed(
            &cambium_synth::deep_hashes(&g),
            &cambium_synth::deep_hashes(&after)
        )
    );
    (checked, wrong)
}

fn assert_walk(name: &str, graph: SynthGraph, change: ChangeSpec, revisions: u64, on_disk: bool) {
    let (checked, wrong) = walk(graph, &change, revisions, on_disk);
    assert!(checked > 0, "{name}: the walk applied no mutation");
    assert!(
        wrong.is_empty(),
        "{name}: {} of {checked} mutations disagree with the emitted files:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// The history `cambium-bench walk synth:small` emits and walks.
#[test]
fn the_bench_history_matches_the_emitted_files() {
    assert_walk(
        "synth:small typical-pr",
        generate(&GraphSpec::preset("small").unwrap().with_seed(0)),
        ChangeSpec {
            seed: 0,
            ..ChangeSpec::preset("typical-pr").unwrap()
        },
        20,
        true,
    );
}

/// A shorter history of another change preset, checked in memory only.
fn assert_preset_walk(preset: &str, seed: u64, revisions: u64) {
    assert_walk(
        preset,
        generate(&GraphSpec::preset("small").unwrap().with_seed(seed)),
        ChangeSpec {
            seed,
            ..ChangeSpec::preset(preset).unwrap()
        },
        revisions,
        false,
    );
}

/// Every mutation kind, splits and merges included.
#[test]
fn a_mixed_history_matches_the_emitted_files() {
    assert_preset_walk("mixed", 3, 8);
}

/// Mutations aimed at the hubs an alias may have been drawn onto.
#[test]
fn a_hub_history_matches_the_emitted_files() {
    assert_preset_walk("hub-touch", 5, 20);
}

/// Dependency edits, which an alias or a `test_suite` may be picked for.
#[test]
fn a_build_edit_history_matches_the_emitted_files() {
    assert_preset_walk("build-edit", 7, 12);
}
