//! The measured stages (spec 13.5).
//!
//! Each stage is a function that builds its own inputs and returns a
//! [`StageResult`]. Keeping them here rather than inside criterion means the
//! same code serves `cambi bench run`, the sweeps, and the criterion
//! harness.

use std::collections::BTreeMap;

use cambium_classify::{Matchers, PackageSet};
use cambium_diff::{Change, Status};
use cambium_engine::{AffectedRequest, Config, Engine, SynthQuerier};
use cambium_index::{walk, Csr, IndexBuilder, IndexData, IndexFile, WalkOptions};
use cambium_synth::{ChangeSpec, GraphSpec, SynthGraph};

use crate::harness::{peak_rss_mb, time, StageResult};

/// Every stage that runs without Bazel.
pub const IN_MEMORY_STAGES: &[&str] = &[
    "build_index",
    "load_index",
    "classify",
    "parse_proto",
    "rule_hash",
    "splice",
    "reverse_rebuild",
    "walk",
    "e2e_memory",
    "e2e_materialize",
];

pub struct StageContext {
    pub preset: String,
    pub spec: GraphSpec,
    pub runs: usize,
}

impl StageContext {
    pub fn new(preset: &str, runs: usize) -> anyhow::Result<Self> {
        let spec =
            GraphSpec::preset(preset).ok_or_else(|| anyhow::anyhow!("unknown preset {preset}"))?;
        Ok(Self {
            preset: preset.to_string(),
            spec,
            runs,
        })
    }

    fn result(
        &self,
        stage: &str,
        median: f64,
        p90: f64,
        extra: BTreeMap<String, f64>,
    ) -> StageResult {
        StageResult {
            stage: stage.to_string(),
            preset: self.preset.clone(),
            spec_hash: self.spec.hash(),
            param: None,
            runs: self.runs,
            median_ms: median,
            p90_ms: p90,
            peak_rss_mb: peak_rss_mb(),
            extra,
            notes: BTreeMap::new(),
        }
    }
}

pub(crate) fn engine() -> Engine {
    let dir = std::env::temp_dir().join("cambium-bench");
    let _ = std::fs::create_dir_all(&dir);
    Engine::with_parts(
        Config::default(),
        cambium_diff::Repo::new(dir.clone()),
        cambium_bazel::Bazel::at(dir, Some("/nonexistent/bazel")),
    )
}

/// Runs the named stage, or every in-memory stage if `stage` is `None`.
pub fn run_stage(ctx: &StageContext, stage: &str) -> anyhow::Result<StageResult> {
    match stage {
        "build_index" => Ok(build_index(ctx)),
        "load_index" => Ok(load_index(ctx)),
        "classify" => Ok(classify(ctx)),
        "parse_proto" => Ok(parse_proto(ctx)),
        "rule_hash" => Ok(rule_hash(ctx)),
        "splice" => Ok(splice(ctx)),
        "reverse_rebuild" => Ok(reverse_rebuild(ctx)),
        "walk" => Ok(walk_stage(ctx, false)),
        "walk_hub" => Ok(walk_stage(ctx, true)),
        "e2e_memory" => Ok(e2e_memory(ctx)),
        "e2e_materialize" => Ok(e2e_materialize(ctx)),
        "legacy_bazel_diff" => {
            Ok(crate::legacy::run(ctx, &crate::legacy::LegacyOptions::default())?.result)
        }
        other => anyhow::bail!("unknown stage {other}"),
    }
}

fn graph(ctx: &StageContext) -> SynthGraph {
    cambium_synth::generate(&ctx.spec)
}

/// SynthGraph -> index. Reports targets/s and the serialised size.
pub fn build_index(ctx: &StageContext) -> StageResult {
    let g = graph(ctx);
    let updates = g.package_updates();
    let targets: usize = updates.iter().map(|u| u.targets.len()).sum();
    let (median, p90) = time(ctx.runs, || {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&updates);
        b.finish("bench")
    });
    let index = {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&updates);
        b.finish("bench")
    };
    let bytes = cambium_index::format::to_bytes(&index)
        .map(|b| b.len())
        .unwrap_or(0);
    let mut extra = BTreeMap::new();
    extra.insert("targets_per_s".into(), targets as f64 / (median / 1000.0));
    extra.insert("nodes".into(), index.header.counts.nodes as f64);
    extra.insert("edges".into(), index.header.counts.edges as f64);
    extra.insert("index_bytes".into(), bytes as f64);
    ctx.result("build_index", median, p90, extra)
}

/// mmap plus the lazy label map. The mmap itself is O(1); the map is the part
/// that scales, and only the paths that need it pay for it.
pub fn load_index(ctx: &StageContext) -> StageResult {
    let g = graph(ctx);
    let index = g.build_index("bench");
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("index.bin");
    cambium_index::format::write_index(&path, &index).expect("write index");

    let (map_median, map_p90) = time(ctx.runs, || {
        let f = IndexFile::open(&path).expect("open");
        let v = f.view();
        v.labels.build_map().len()
    });
    let (open_median, _) = time(ctx.runs, || {
        let f = IndexFile::open(&path).expect("open");
        f.view().num_nodes()
    });
    let mut extra = BTreeMap::new();
    extra.insert("mmap_only_ms".into(), open_median);
    extra.insert("labels".into(), index.header.counts.nodes as f64);
    extra.insert(
        "file_bytes".into(),
        std::fs::metadata(&path)
            .map(|m| m.len() as f64)
            .unwrap_or(0.0),
    );
    ctx.result("load_index", map_median, map_p90, extra)
}

/// Path -> package resolution and the dirty set, over a large change list.
pub fn classify(ctx: &StageContext) -> StageResult {
    let g = graph(ctx);
    let packages = PackageSet::new(g.packages.iter().map(|p| p.path.clone()));
    let matchers = Matchers::compile(&Config::default().rules()).expect("globs compile");
    let changes: Vec<Change> = (0..g.targets.len().min(50_000) as u32)
        .filter(|&i| !g.targets[i as usize].srcs.is_empty())
        .map(|i| {
            Change::new(
                Status::Modified,
                g.src_path(i, g.targets[i as usize].srcs[0]),
            )
        })
        .collect();
    let n = changes.len();
    let (median, p90) = time(ctx.runs, || {
        let mut p = PackageSet::new(packages.iter().map(String::from));
        // The synthetic sources are `.cc`, so no ignore glob matches and the
        // consumption oracle is never consulted.
        cambium_classify::dirty_set(&changes, &mut p, &matchers, &cambium_classify::NoConsumers)
    });
    let mut extra = BTreeMap::new();
    extra.insert("paths".into(), n as f64);
    extra.insert("paths_per_s".into(), n as f64 / (median / 1000.0));
    ctx.result("classify", median, p90, extra)
}

/// streamed_proto bytes -> index rows, including the rule hash.
///
/// Materialises the whole proto stream, so peak memory is a multiple of the
/// index size: run it on `small` and `medium`, not on `large` and above, where
/// the interesting question is the graph work rather than the parse rate.
pub fn parse_proto(ctx: &StageContext) -> StageResult {
    let g = graph(ctx);
    let bytes = cambium_synth::proto::to_streamed_proto(&g);
    let opts = cambium_bazel::ConvertOptions::default();
    let files = cambium_engine::query::no_files();
    let (median, p90) = time(ctx.runs, || {
        cambium_engine::query::parse_stream(&bytes, &opts, &files).expect("parse")
    });
    let mut extra = BTreeMap::new();
    extra.insert("bytes".into(), bytes.len() as f64);
    extra.insert(
        "mb_per_s".into(),
        bytes.len() as f64 / 1e6 / (median / 1000.0),
    );
    extra.insert(
        "targets_per_s".into(),
        g.num_alive_targets() as f64 / (median / 1000.0),
    );
    ctx.result("parse_proto", median, p90, extra)
}

/// Canonicalisation plus blake3 over rule protos.
///
/// Holds every rule proto in memory; see the note on [`parse_proto`].
pub fn rule_hash(ctx: &StageContext) -> StageResult {
    let g = graph(ctx);
    let targets = cambium_synth::proto::to_targets(&g);
    let rules: Vec<_> = targets.iter().filter_map(|t| t.rule.clone()).collect();
    let n = rules.len();
    let (median, p90) = time(ctx.runs, || {
        rules
            .iter()
            .map(|r| cambium_proto::rule_hash(r)[0] as u64)
            .sum::<u64>()
    });
    let mut extra = BTreeMap::new();
    extra.insert("rules".into(), n as f64);
    extra.insert("hashes_per_s".into(), n as f64 / (median / 1000.0));
    ctx.result("rule_hash", median, p90, extra)
}

/// Replacing the rows of a handful of packages in a loaded index.
pub fn splice(ctx: &StageContext) -> StageResult {
    let g = graph(ctx);
    let base = g.build_index("bench");
    // A typical dirty set: a few packages, as a `typical-pr` change would give.
    let dirty: Vec<u32> = (0..g.packages.len() as u32).step_by(97).take(8).collect();
    let updates: Vec<_> = dirty.iter().map(|&p| g.package_update(p)).collect();
    let (median, p90) = time(ctx.runs, || {
        let mut b = IndexBuilder::from_view(base.view());
        b.apply_packages(&updates);
        b.finish("head")
    });
    let mut extra = BTreeMap::new();
    extra.insert("dirty_packages".into(), dirty.len() as f64);
    extra.insert("edges".into(), base.header.counts.edges as f64);
    extra.insert("nodes".into(), base.header.counts.nodes as f64);
    ctx.result("splice", median, p90, extra)
}

/// Forward CSR -> reverse CSR, serial and parallel.
pub fn reverse_rebuild(ctx: &StageContext) -> StageResult {
    let g = graph(ctx);
    let index = g.build_index("bench");
    let v = index.view();
    let n = v.num_nodes() as usize;
    let (median, p90) = time(ctx.runs, || Csr::reverse_of(v.forward, n));
    let (par_median, _) = time(ctx.runs, || Csr::reverse_of_parallel(v.forward, n));
    let mut extra = BTreeMap::new();
    extra.insert("edges".into(), v.num_edges() as f64);
    extra.insert("parallel_ms".into(), par_median);
    extra.insert(
        "edges_per_s".into(),
        v.num_edges() as f64 / (median / 1000.0),
    );
    ctx.result("reverse_rebuild", median, p90, extra)
}

/// The reverse walk itself, from a small direct set or from a hub.
pub fn walk_stage(ctx: &StageContext, hub: bool) -> StageResult {
    let g = graph(ctx);
    let index = g.build_index("bench");
    let v = index.view();
    let map = v.labels.build_map();
    let direct: Vec<u32> = if hub {
        // Layer-0 targets with the largest in-degree: the worst case.
        let mut by_indeg: Vec<(u32, usize)> = (0..v.num_nodes())
            .filter(|&i| v.is_rule(i))
            .map(|i| (i, v.reverse.row_len(i)))
            .collect();
        by_indeg.sort_by_key(|&(_, d)| std::cmp::Reverse(d));
        by_indeg.iter().take(1).map(|&(i, _)| i).collect()
    } else {
        (0..g.targets.len() as u32)
            .step_by((g.targets.len() / 10).max(1))
            .take(10)
            .filter_map(|i| map.get(g.target_label(i).as_str()).copied())
            .collect()
    };
    let (median, p90) = time(ctx.runs, || walk::walk(v, &direct, &WalkOptions::default()));
    let r = walk::walk(v, &direct, &WalkOptions::default());
    let mut extra = BTreeMap::new();
    extra.insert("direct".into(), direct.len() as f64);
    extra.insert("affected".into(), r.affected.len() as f64);
    extra.insert(
        "affected_fraction".into(),
        r.affected.len() as f64 / index.header.counts.rules.max(1) as f64,
    );
    ctx.result(if hub { "walk_hub" } else { "walk" }, median, p90, extra)
}

/// The whole pipeline minus Bazel: dirty set, splice, direct set, walk.
pub fn e2e_memory(ctx: &StageContext) -> StageResult {
    e2e_memory_with(ctx, "typical-pr")
}

/// The same pipeline, materialising a full head index instead of walking a
/// delta overlay. The difference between the two is what item 6 bought.
pub fn e2e_materialize(ctx: &StageContext) -> StageResult {
    let mut r = e2e_memory_inner(ctx, "typical-pr", true);
    r.stage = "e2e_materialize".into();
    r
}

pub fn e2e_memory_with(ctx: &StageContext, change_preset: &str) -> StageResult {
    e2e_memory_inner(ctx, change_preset, false)
}

fn e2e_memory_inner(ctx: &StageContext, change_preset: &str, materialize: bool) -> StageResult {
    let e = engine();
    let mut g = graph(ctx);
    let base = g.build_index("c0");
    let spec = ChangeSpec::preset(change_preset).unwrap_or_default();
    let c = cambium_synth::commit(&mut g, &spec, 0);
    let req = AffectedRequest {
        base: "c0".into(),
        head: "c1".into(),
        include_manual: true,
        explain: false,
        no_full_build: true,
        materialize,
        publish_base: false,
        max_depth: None,
    };
    let q = SynthQuerier::new(&g);
    let (median, p90) = time(ctx.runs, || {
        e.compute(base.view(), &c.changes, &q, &req).expect("run")
    });
    let out = e.compute(base.view(), &c.changes, &q, &req).expect("run");
    let mut extra = BTreeMap::new();
    extra.insert("changed_files".into(), c.changes.len() as f64);
    extra.insert(
        "dirty_packages".into(),
        out.report.stats.dirty_packages as f64,
    );
    extra.insert("affected".into(), out.report.stats.affected_targets as f64);
    for (k, v) in [
        ("stage_dirty_ms", out.report.timings_ms.dirty),
        ("stage_query_ms", out.report.timings_ms.query),
        ("stage_splice_ms", out.report.timings_ms.splice),
        ("stage_direct_ms", out.report.timings_ms.direct),
        ("stage_walk_ms", out.report.timings_ms.walk),
    ] {
        extra.insert(k.into(), v as f64);
    }
    ctx.result("e2e_memory", median, p90, extra)
}

/// Builds an index for a preset, for callers that want one directly.
pub fn index_for(preset: &str) -> anyhow::Result<IndexData> {
    let spec = GraphSpec::preset(preset).ok_or_else(|| anyhow::anyhow!("unknown preset"))?;
    Ok(cambium_synth::generate(&spec).build_index("bench"))
}
