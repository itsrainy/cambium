//! `skyframe-analysis`: an analysis-phase tier over a Skyframe dump.
//!
//! The question this strategy exists to ask is narrow. cambium is exact at
//! the *unconfigured* target graph, and the counterexamples it cannot answer
//! incrementally are the ones that live below it: toolchain resolution,
//! platform selection, configuration. Those are the `Escalated` list in
//! README.md, and every one of them ends the same way, with
//! `cold_rebuild_reason` set and `//...` queried once. They escalate because
//! *no edge exists* for the reverse walk to follow, not because the walk is
//! wrong.
//!
//! Bazel does have those edges, in the analysis phase, and it will print
//! them. So: run analysis once at the parent (`bazel build --nobuild
//! --keep_going //...`), dump the Skyframe graph filtered to the
//! analysis-phase kinds and the loading-phase kinds that reach them, reverse
//! it, store it, and answer at head with a reverse walk instead of a rebuild.
//!
//! Two variants, because the interesting cost is not the walk:
//!
//! - `skyframe-analysis` re-uses the parent's dump at head. Analysis runs
//!   once, in `prepare`; `answer` reads a file and walks it.
//! - `skyframe-analysis-warm` re-runs `--nobuild` at head on the same warm
//!   server and diffs the two dumps. It measures what a warm-server analysis
//!   costs per step and whether the diff is tighter than the walk.
//!
//! What was learned doing it is in docs/SKYFRAME_PROTOTYPES.md; the two
//! findings worth carrying in the code are in [`dump`] (nothing depends on a
//! `FILE_STATE`, so `FILE` has to be in the filter) and in
//! [`graph::seeds`] (no analysis node reads the bytes of an ordinary source
//! file, so a content edit travels only through the source file target).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use super::{Answer, Prepared, StepContext, Strategy};

pub mod dump;
pub mod graph;

pub use graph::Graph;

/// The strategy's own name, and the warm variant's.
pub const NAME: &str = "skyframe-analysis";
pub const WARM_NAME: &str = "skyframe-analysis-warm";

/// The universe analysis is asked for, and the fallback when it yields
/// nothing at all. `--keep_going` means a few unanalysable targets (on the
/// `bazel` subject, a GraalVM distribution that will not fetch on macOS) cost
/// their own subtree and not the run.
pub const UNIVERSE: &str = "//...";
pub const FALLBACK_UNIVERSE: &str = "//src/...";

/// The kinds the dump keeps.
///
/// `FILE` is in the list for a reason worth repeating: nothing in the graph
/// depends on a `FILE_STATE` directly, so leaving `FILE` out strands every
/// file state and the reverse walk from a changed `BUILD` file reaches
/// nothing. The loading-phase kinds are here because the analysis-phase ones
/// are only reachable through them.
pub const SKYKEY_FILTER: &str = "^(CONFIGURED_TARGET|ASPECT|TOOLCHAIN_RESOLUTION|\
SINGLE_TOOLCHAIN_RESOLUTION|REGISTERED_TOOLCHAINS|REGISTERED_EXECUTION_PLATFORMS|PLATFORM|\
BUILD_CONFIGURATION|PACKAGE|BZL_LOAD|FILE|FILE_STATE|DIRECTORY_LISTING|DIRECTORY_LISTING_STATE|\
GLOBS|PACKAGE_LOOKUP)";

/// One analysis, one dump, one parse.
struct Captured {
    graph: Graph,
    dump_bytes: u64,
    analysis: Duration,
    dump_wall: Duration,
    parse: Duration,
    universe: String,
    analyzed: Option<u64>,
}

/// `skyframe-analysis` and, with `warm` set, `skyframe-analysis-warm`.
pub struct AnalysisStrategy {
    pub warm: bool,
    /// Filled by `prepare`, read by `answer`.
    state: Option<PreparedState>,
}

struct PreparedState {
    graph_path: PathBuf,
    parent_nodes: BTreeSet<String>,
    universe: String,
}

impl AnalysisStrategy {
    pub fn new(warm: bool) -> Self {
        Self { warm, state: None }
    }
}

impl Strategy for AnalysisStrategy {
    fn name(&self) -> &str {
        if self.warm {
            WARM_NAME
        } else {
            NAME
        }
    }

    fn prepare(&mut self, cx: &StepContext<'_>) -> Result<Prepared> {
        // A clean server per step. Two reasons, both learned on the `bazel`
        // subject: a server that has analysed `//...` at several revisions and
        // dumped 285 MB of keys each time dies partway through the next dump
        // ("Server terminated abruptly"), and a prepare that inherits the
        // previous step's warm analysis is not the cold cost the harness is
        // there to measure. The warm variant still shares one server between
        // its own prepare and answer, which is the thing it measures.
        cx.bazel.shutdown();
        let started = Instant::now();
        let cap = capture(cx, cx.scratch, "parent")?;
        let mut notes = BTreeMap::new();
        notes.insert("universe".into(), cap.universe.clone());
        notes.insert("dump_bytes".into(), cap.dump_bytes.to_string());
        notes.insert("analysis_ms".into(), cap.analysis.as_millis().to_string());
        notes.insert("dump_ms".into(), cap.dump_wall.as_millis().to_string());
        notes.insert("parse_ms".into(), cap.parse.as_millis().to_string());
        notes.insert("nodes".into(), cap.graph.len().to_string());
        if let Some(a) = cap.analyzed {
            notes.insert("analyzed_targets".into(), a.to_string());
        }

        // Question two of the branch: how many distinct configurations is one
        // target analysed in?
        let by_label = cap.graph.configs_by_label();
        let all: BTreeSet<&str> = by_label.values().flatten().copied().collect();
        let multi = by_label.values().filter(|s| s.len() > 1).count();
        notes.insert("config_checksums".into(), all.len().to_string());
        notes.insert("labels_multi_config".into(), multi.to_string());
        notes.insert("configured_labels".into(), by_label.len().to_string());
        // Coverage: how much of the main repository analysis actually
        // reached. A target whose analysis failed (on this subject, everything
        // downstream of a GraalVM distribution that will not fetch on macOS)
        // has no node at all, and no reverse walk can reach what is not
        // there.
        let main_labels = by_label.keys().filter(|l| l.starts_with("//")).count();
        notes.insert("main_repo_labels".into(), main_labels.to_string());

        let graph_path = cx.scratch.join("analysis-graph.zst");
        let state_bytes = cap.graph.write(&graph_path)?;
        notes.insert("graph_bytes".into(), state_bytes.to_string());

        self.state = Some(PreparedState {
            graph_path,
            parent_nodes: if self.warm {
                cap.graph.nodes.iter().map(|n| n.text()).collect()
            } else {
                BTreeSet::new()
            },
            universe: cap.universe,
        });
        Ok(Prepared {
            wall: started.elapsed(),
            state_bytes,
            notes,
        })
    }

    fn answer(&mut self, cx: &StepContext<'_>) -> Result<Answer> {
        let started = Instant::now();
        let state = self
            .state
            .as_ref()
            .context("answer called before prepare")?;
        let mut notes = BTreeMap::new();
        notes.insert("universe".into(), state.universe.clone());

        let (graph, mut extra_seeds) = if self.warm {
            // The warm variant pays for analysis again, at head, on the same
            // server, and seeds the head graph with every key that appeared
            // or disappeared as well as with the changed paths.
            let cap = capture(cx, cx.scratch, "head")?;
            notes.insert(
                "head_analysis_ms".into(),
                cap.analysis.as_millis().to_string(),
            );
            notes.insert("head_dump_ms".into(), cap.dump_wall.as_millis().to_string());
            notes.insert("head_parse_ms".into(), cap.parse.as_millis().to_string());
            notes.insert("head_dump_bytes".into(), cap.dump_bytes.to_string());
            notes.insert("head_nodes".into(), cap.graph.len().to_string());
            let mut seeds = Vec::new();
            let mut appeared = 0usize;
            for (i, n) in cap.graph.nodes.iter().enumerate() {
                if !state.parent_nodes.contains(&n.text()) {
                    appeared += 1;
                    seeds.push(i as u32);
                }
            }
            notes.insert("keys_appeared".into(), appeared.to_string());
            notes.insert(
                "keys_vanished".into(),
                state
                    .parent_nodes
                    .iter()
                    .filter(|t| !cap.graph.contains_text(t))
                    .count()
                    .to_string(),
            );
            (cap.graph, seeds)
        } else {
            let g = Graph::read(&state.graph_path)?;
            (g, Vec::new())
        };

        let packages = graph.main_repo_packages();
        let workspace = cx.worktree.to_string_lossy().to_string();
        let (mut seeds, routes) = graph::seeds(&graph, &workspace, cx.changed, &packages);
        seeds.append(&mut extra_seeds);
        seeds.sort_unstable();
        seeds.dedup();
        for (route, n) in &routes {
            notes.insert(format!("seeds.{route}"), n.to_string());
        }
        notes.insert("seeds".into(), seeds.len().to_string());
        // A changed path that named no node at all is the honest measure of
        // what the tier cannot see.
        let unseeded = cx
            .changed
            .iter()
            .filter(|c| {
                let l = graph::source_label(&c.path, &packages);
                let named = l
                    .as_deref()
                    .map(|l| {
                        graph
                            .get(&dump::NodeKey::configured_target(l, None))
                            .is_some()
                    })
                    .unwrap_or(false);
                !named
            })
            .count();
        notes.insert("changed_paths".into(), cx.changed.len().to_string());
        notes.insert("changed_without_a_node".into(), unseeded.to_string());

        let reached = graph.reach(&seeds);
        let targets = graph.answer_labels(&reached);
        notes.insert("reached_nodes".into(), reached.len().to_string());
        Ok(Answer {
            targets,
            wall: started.elapsed(),
            notes,
        })
    }
}

/// Analysis, dump, parse. `tag` names the dump file inside `scratch` so the
/// warm variant's two dumps do not overwrite one another.
fn capture(cx: &StepContext<'_>, scratch: &Path, tag: &str) -> Result<Captured> {
    let t0 = Instant::now();
    let (universe, analyzed) = match analyse(cx, UNIVERSE)? {
        Some(n) => (UNIVERSE.to_string(), Some(n)),
        None => {
            tracing::warn!(
                universe = UNIVERSE,
                fallback = FALLBACK_UNIVERSE,
                "nothing analysed; falling back to the largest analysable universe"
            );
            (
                FALLBACK_UNIVERSE.to_string(),
                analyse(cx, FALLBACK_UNIVERSE)?,
            )
        }
    };
    let analysis = t0.elapsed();

    let dump_path = scratch.join(format!("dump-{tag}.txt"));
    let t1 = Instant::now();
    let file = std::fs::File::create(&dump_path)?;
    let mut cmd = cx.bazel.command("dump");
    cmd.arg("--skyframe=deps")
        .arg(format!("--skykey_filter={SKYKEY_FILTER}"))
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let out = cmd.output().context("bazel dump --skyframe=deps")?;
    anyhow::ensure!(
        out.status.success(),
        "bazel dump --skyframe=deps failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dump_wall = t1.elapsed();
    let dump_bytes = std::fs::metadata(&dump_path)?.len();

    let t2 = Instant::now();
    let parsed = dump::parse(std::io::BufReader::with_capacity(
        1 << 20,
        std::fs::File::open(&dump_path)?,
    ))?;
    let parse = t2.elapsed();
    anyhow::ensure!(!parsed.nodes.is_empty(), "the dump held no keys");
    tracing::info!(
        %tag,
        nodes = parsed.nodes.len(),
        edges = parsed.edges,
        unparsed = parsed.unparsed,
        bytes = dump_bytes,
        "parsed a skyframe dump"
    );
    // The dump is the biggest thing on disk and nothing reads it twice.
    let _ = std::fs::remove_file(&dump_path);
    Ok(Captured {
        graph: Graph::from_dump(parsed),
        dump_bytes,
        analysis,
        dump_wall,
        parse,
        universe,
        analyzed,
    })
}

/// `bazel build --nobuild --keep_going <universe>`, returning how many
/// targets it says it analysed.
///
/// `--keep_going` makes a partial analysis the normal outcome, so a non-zero
/// exit is not a failure here: what matters is whether Bazel reported any
/// targets at all. `None` means it did not.
fn analyse(cx: &StepContext<'_>, universe: &str) -> Result<Option<u64>> {
    let mut cmd = cx.bazel.command("build");
    cmd.arg("--nobuild")
        .arg("--keep_going")
        // Without it, a `test_suite` is expanded into its members and never
        // gets a CONFIGURED_TARGET of its own, so every test_suite in the
        // repository is missing from the tier while the `rdeps` oracle
        // reaches them all.
        .arg("--noexpand_test_suites")
        .arg(universe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("bazel build --nobuild {universe}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let found = found_targets(&text);
    if found.is_none() {
        tracing::warn!(
            universe,
            status = ?out.status.code(),
            tail = %tail(&text, 20),
            "analysis reported no targets"
        );
    }
    Ok(found)
}

/// `INFO: Found 3908 targets...` / `INFO: Analyzed 323 targets (…)`.
pub fn found_targets(text: &str) -> Option<u64> {
    for line in text.lines() {
        for marker in ["Found ", "Analyzed "] {
            if let Some(rest) = line.split_once(marker).map(|(_, r)| r) {
                if let Some(n) = rest.split_whitespace().next() {
                    if let Ok(v) = n.replace(',', "").parse::<u64>() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_analysed_target_count_is_read_off_bazels_own_line() {
        assert_eq!(found_targets("INFO: Found 3908 targets...\n"), Some(3908));
        assert_eq!(
            found_targets("INFO: Analyzed 323 targets (537 packages loaded)\n"),
            Some(323)
        );
        // A keep_going run that still analysed something.
        let real = "INFO: Found 3908 targets...\nERROR: command succeeded, but there \
                    were loading phase errors\nERROR: Build did NOT complete successfully\n";
        assert_eq!(found_targets(real), Some(3908));
        assert_eq!(found_targets("ERROR: no targets found\n"), None);
    }

    #[test]
    fn the_filter_names_file_as_well_as_file_state() {
        // The one thing about the filter that is load-bearing: without FILE,
        // every FILE_STATE is a reverse-walk dead end.
        assert!(SKYKEY_FILTER.contains("|FILE|"));
        assert!(SKYKEY_FILTER.contains("FILE_STATE"));
        assert!(SKYKEY_FILTER.contains("CONFIGURED_TARGET"));
        assert!(SKYKEY_FILTER.contains("TOOLCHAIN_RESOLUTION"));
    }

    #[test]
    fn the_two_variants_are_two_names() {
        assert_eq!(AnalysisStrategy::new(false).name(), "skyframe-analysis");
        assert_eq!(AnalysisStrategy::new(true).name(), "skyframe-analysis-warm");
    }
}
