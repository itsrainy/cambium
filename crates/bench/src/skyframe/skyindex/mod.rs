//! The `skyframe-index` prototype: a cold index shaped like Skyframe's own
//! loading-phase keys.
//!
//! `prepare`, at the step's parent, loads the repository and then asks the
//! server what it now holds:
//!
//! 1. `bazel query //...:*` as a proto stream. It loads every package (which
//!    is what fills the Skyframe graph) and is also where the rule layer
//!    comes from, since a loading-phase dump on Bazel 9.2 leaves no `TARGET`
//!    keys behind.
//! 2. `bazel dump --skyframe=deps --skykey_filter=...` over the loading-phase
//!    kinds, parsed into [`graph::SkyIndex`] and written to
//!    `scratch/skyindex.bin`.
//!
//! `answer`, at the step's head, reads that file back and walks it: the
//! changed paths become `FILE_STATE` and `DIRECTORY_LISTING_STATE` keys, the
//! walk follows reverse dependencies over every stored kind, and every rule
//! it reaches is the answer. Nothing in the answer path looks at what kind of
//! change it was.
//!
//! Two variants, from the same index:
//!
//! - `skyframe-index` answers from the stored graph alone.
//! - `skyframe-index-requery` re-queries the packages the walk dirtied, at
//!   head, before the rule-level closure, the way the engine does. It costs a
//!   Bazel call and can name targets that did not exist at the parent, which
//!   the stored graph by construction cannot.

pub mod dump;
pub mod graph;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use anyhow::{Context as _, Result};
use cambium_bazel::{convert, ConvertOptions, ParsedTarget};
use cambium_index::NodeKind;

use super::{Answer, Prepared, StepContext, Strategy};
use graph::{Builder, RuleNode, SkyIndex};

/// The kinds the dump is asked for.
///
/// `FILE` matches `FILE_STATE` too, and `PACKAGE` matches `PACKAGE_LOOKUP`;
/// the filter is deliberately a prefix set rather than an exact list, because
/// a `PACKAGE` node's dependency on its `BUILD` file goes through a `FILE`
/// node and dropping that kind cuts every chain in half.
pub const SKYKEY_FILTER: &str = "^(PACKAGE|GLOBS|FILE|DIRECTORY_LISTING|BZL_LOAD)";

/// Where `prepare` leaves the index.
const INDEX_FILE: &str = "skyindex.bin";

pub struct SkyIndexStrategy {
    name: &'static str,
    /// Whether `answer` re-queries the dirty packages at head.
    requery: bool,
}

impl SkyIndexStrategy {
    pub fn plain() -> Self {
        Self {
            name: "skyframe-index",
            requery: false,
        }
    }

    pub fn requery() -> Self {
        Self {
            name: "skyframe-index-requery",
            requery: true,
        }
    }
}

impl Strategy for SkyIndexStrategy {
    fn name(&self) -> &str {
        self.name
    }

    fn prepare(&mut self, cx: &StepContext<'_>) -> Result<Prepared> {
        let start = Instant::now();
        let mut notes = BTreeMap::new();

        // The universe query is both the load that fills Skyframe and the
        // source of the rule layer.
        let t = Instant::now();
        let opts = ConvertOptions::default();
        let mut targets: Vec<ParsedTarget> = Vec::new();
        let outcome = cx
            .bazel
            .query_universe(|slab, ranges| {
                for &range in ranges {
                    if let Some(p) = convert(&cambium_proto::decode_frame(slab, range)?, &opts) {
                        targets.push(p);
                    }
                }
                Ok(())
            })
            .context("bazel query //...:* at the parent")?;
        notes.insert("query_ms".into(), t.elapsed().as_millis().to_string());
        notes.insert("query_targets".into(), outcome.targets_read.to_string());
        notes.insert(
            "query_load_errors".into(),
            outcome.load_errors.count.to_string(),
        );

        // The dump, over a server that has just loaded everything.
        let t = Instant::now();
        let text = run_dump(cx).context("bazel dump --skyframe=deps at the parent")?;
        notes.insert("dump_ms".into(), t.elapsed().as_millis().to_string());
        notes.insert("dump_bytes".into(), text.len().to_string());

        let t = Instant::now();
        let mut builder = Builder::new(&cx.worktree.to_string_lossy());
        let blocks = builder.add_dump(&text);
        for (label, node) in rule_layer(&targets) {
            builder.add_rule(label, node);
        }
        let index = builder.finish();
        notes.insert("parse_ms".into(), t.elapsed().as_millis().to_string());
        notes.insert("dump_blocks".into(), blocks.to_string());
        notes.insert("nodes".into(), index.node_count().to_string());
        notes.insert("edges".into(), index.edge_count().to_string());
        notes.insert("rules".into(), index.rules.len().to_string());

        let state_bytes = index.write(&cx.scratch.join(INDEX_FILE))?;
        Ok(Prepared {
            wall: start.elapsed(),
            state_bytes,
            notes,
        })
    }

    fn answer(&mut self, cx: &StepContext<'_>) -> Result<Answer> {
        let start = Instant::now();
        let mut notes = BTreeMap::new();

        let t = Instant::now();
        let mut index = SkyIndex::read(&cx.scratch.join(INDEX_FILE))
            .context("reading the index prepare left behind")?;
        notes.insert("load_ms".into(), t.elapsed().as_millis().to_string());

        let walk = index.answer(cx.changed);
        notes.insert(
            "dirty_packages".into(),
            walk.dirty_packages.len().to_string(),
        );
        notes.insert("unknown_seeds".into(), walk.unknown_seeds.to_string());
        let mut targets = walk.targets;

        if self.requery && !walk.dirty_packages.is_empty() {
            let t = Instant::now();
            let packages: Vec<String> = walk.dirty_packages.iter().cloned().collect();
            let fresh = query_packages(cx, &packages)?;
            notes.insert("requery_ms".into(), t.elapsed().as_millis().to_string());
            notes.insert("requeried".into(), packages.len().to_string());
            notes.insert("requery_rules".into(), fresh.len().to_string());
            index.replace_packages(&walk.dirty_packages, fresh);
            let changed: BTreeSet<&str> = cx.changed.iter().map(|c| c.path.as_str()).collect();
            targets = index.close_over_rules(&walk.dirty_packages, &changed);
        }

        Ok(Answer {
            targets,
            wall: start.elapsed(),
            notes,
        })
    }
}

/// `bazel dump --skyframe=deps` over [`SKYKEY_FILTER`], as text.
fn run_dump(cx: &StepContext<'_>) -> Result<String> {
    let mut cmd = cx.bazel.command("dump");
    cmd.arg("--skyframe=deps")
        .arg(format!("--skykey_filter={SKYKEY_FILTER}"));
    let out = cmd.output().context("spawning bazel dump")?;
    anyhow::ensure!(
        out.status.success(),
        "bazel dump exited with {}: {}",
        out.status,
        tail(&String::from_utf8_lossy(&out.stderr))
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `bazel query 'set(//a:* ...)'` at head, as a rule layer for those packages.
fn query_packages(cx: &StepContext<'_>, packages: &[String]) -> Result<BTreeMap<String, RuleNode>> {
    let opts = ConvertOptions::default();
    let mut targets: Vec<ParsedTarget> = Vec::new();
    // `--keep_going` is on: a dirty "package" may be a directory that holds
    // none, or one the change deleted, and either is an answer.
    cx.bazel
        .query_packages(packages, |slab, ranges| {
            for &range in ranges {
                if let Some(p) = convert(&cambium_proto::decode_frame(slab, range)?, &opts) {
                    targets.push(p);
                }
            }
            Ok(())
        })
        .context("re-querying the dirty packages at head")?;
    Ok(rule_layer(&targets))
}

/// The rule layer: every rule, the package it is in, the rules it depends on,
/// and the source paths it depends on.
///
/// A dependency on a generated file is recorded as a dependency on the rule
/// that generates it, which is the edge the reverse walk needs; a label the
/// query never returned (an external repository, or a package that failed to
/// load) is dropped, since nothing in a main-repository diff can dirty it.
fn rule_layer(targets: &[ParsedTarget]) -> BTreeMap<String, RuleNode> {
    let mut rules: BTreeMap<&str, &ParsedTarget> = BTreeMap::new();
    let mut generated: BTreeMap<&str, &str> = BTreeMap::new();
    let mut sources: BTreeSet<&str> = BTreeSet::new();
    for t in targets {
        match t.row.kind {
            NodeKind::Rule => {
                rules.insert(t.row.label.as_str(), t);
            }
            NodeKind::GeneratedFile => {
                if let Some(g) = t.row.generating_rule.as_deref() {
                    generated.insert(t.row.label.as_str(), g);
                }
            }
            NodeKind::SourceFile => {
                sources.insert(t.row.label.as_str());
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for (label, t) in &rules {
        let mut deps = Vec::new();
        let mut file_deps = Vec::new();
        for input in &t.row.inputs {
            let input = input.as_str();
            if rules.contains_key(input) {
                deps.push(input.to_string());
            } else if let Some(g) = generated.get(input) {
                deps.push((*g).to_string());
            } else if let Some(path) = graph::source_path(input) {
                // Either a source file the query returned, or a label in the
                // main repository the query did not: both are paths a diff
                // can name, and a path that is never touched costs nothing.
                file_deps.push(path);
            }
        }
        deps.sort();
        deps.dedup();
        file_deps.sort();
        file_deps.dedup();
        out.insert(
            (*label).to_string(),
            RuleNode {
                package: t.package.clone(),
                deps,
                file_deps,
            },
        );
    }
    out
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_index::TargetRow;

    fn parsed(package: &str, row: TargetRow) -> ParsedTarget {
        ParsedTarget {
            package: package.to_string(),
            row,
        }
    }

    fn rule(package: &str, label: &str, inputs: &[&str]) -> ParsedTarget {
        parsed(
            package,
            TargetRow {
                label: label.to_string(),
                kind: NodeKind::Rule,
                rule_class: Some("cc_library".to_string()),
                rule_hash: [0u8; 32],
                is_test: false,
                manual: false,
                inputs: inputs.iter().map(|s| s.to_string()).collect(),
                generating_rule: None,
            },
        )
    }

    #[test]
    fn the_rule_layer_resolves_generated_files_to_their_generator() {
        let mut gen = parsed("a", TargetRow::source_file("//a:made.h".to_string()));
        gen.row.kind = NodeKind::GeneratedFile;
        gen.row.generating_rule = Some("//a:gen".to_string());
        let targets = vec![
            rule("a", "//a:gen", &[]),
            rule("a", "//a:lib", &["//a:made.h", "//a:src.cc", "@@ext+//x:y"]),
            gen,
            parsed("a", TargetRow::source_file("//a:src.cc".to_string())),
        ];
        let layer = rule_layer(&targets);
        let lib = &layer["//a:lib"];
        assert_eq!(lib.package, "a");
        // The generated header became an edge to the rule that makes it.
        assert_eq!(lib.deps, vec!["//a:gen".to_string()]);
        // The source became a path; the external label is gone.
        assert_eq!(lib.file_deps, vec!["a/src.cc".to_string()]);
    }

    #[test]
    fn the_two_variants_are_the_two_names_the_runner_knows() {
        assert_eq!(SkyIndexStrategy::plain().name(), "skyframe-index");
        assert_eq!(SkyIndexStrategy::requery().name(), "skyframe-index-requery");
        assert!(SkyIndexStrategy::requery().requery);
        assert!(!SkyIndexStrategy::plain().requery);
    }
}
