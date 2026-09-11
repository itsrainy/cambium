//! `legacy_bazel_diff`: the same change, measured against bazel-diff (spec 7).
//!
//! bazel-diff is the tool cambium replaces, so a headline number is only
//! honest if both sides answer the *same* question on the *same* graph. That
//! is what this stage arranges:
//!
//! * the synthetic graph is emitted twice — a `base/` and a `head/` workspace —
//!   because bazel-diff hashes source-file *content* off disk, not labels;
//! * the `streamed_proto` dump `bazel query` would produce is written for each
//!   revision, and a fake `bazel` shim serves the right one, so no Bazel server
//!   is in the measurement (this is exactly how bazel-diff's own perf gate
//!   runs);
//! * cambium answers from the base index and the same [`cambium_synth::Commit`]
//!   in memory.
//!
//! The two label sets must agree, with one legal asymmetry: cambium's index
//! keeps `select()` branches that a configured Bazel query resolves away, so
//! cambium may report *more* labels. A label bazel-diff calls impacted and
//! cambium does not is unsoundness, and fails the stage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cambium_engine::{AffectedRequest, SynthQuerier};
use cambium_synth::ChangeSpec;

use crate::harness::{peak_rss_mb, StageResult};
use crate::stages::StageContext;
use crate::tools::bazeldiff::BazelDiff;
use crate::tools::RunContext;

/// The stage's name, as `bench run --stage` and `bench sweep --stage` spell it.
pub const STAGE: &str = "legacy_bazel_diff";

/// Bazel version the shim reports. 7.4.1 matches the `.bazelversion` the synth
/// emitter writes, and is below the 8.2.0 cut-off at which bazel-diff switches
/// to `--output_file`; the shim answers both spellings anyway.
const SHIM_BAZEL_VERSION: &str = "7.4.1";

#[derive(Clone, Debug)]
pub struct LegacyOptions {
    /// Change preset applied to get the head revision.
    pub change_preset: String,
    pub seed: u64,
    /// Keep the scratch workspaces here instead of a temporary directory.
    pub workdir: Option<PathBuf>,
}

impl Default for LegacyOptions {
    fn default() -> Self {
        Self {
            change_preset: "typical-pr".to_string(),
            seed: 0,
            workdir: None,
        }
    }
}

/// What the stage found, beyond the timings.
#[derive(Clone, Debug)]
pub struct LegacyRun {
    pub result: StageResult,
    pub bazel_diff_version: String,
    /// Labels both tools call impacted.
    pub agreed: BTreeSet<String>,
    /// Labels only cambium reports; expected to be `select`-reachable extras.
    pub cambium_only: BTreeSet<String>,
    /// Labels only bazel-diff reports. Always empty on success.
    pub bazel_diff_only: BTreeSet<String>,
}

/// Runs the stage. Errors when bazel-diff is missing, when it fails, or when
/// it reports a label cambium does not.
///
/// Three phases: emit both revisions and the query dumps the shim serves;
/// time bazel-diff cold and warm over them; time cambium on the same commit
/// against the base index. Then the two label sets are compared.
pub fn run(ctx: &StageContext, bazel_diff: &BazelDiff, opts: &LegacyOptions) -> Result<LegacyRun> {
    // The stage's own bazel-diff: no `-k`, since the shim never fails to
    // "load", and `--excludeExternalTargets`, since the generated graph has no
    // externals and hashing them would be hashing nothing.
    let tool = bazel_diff
        .clone()
        .with_keep_going(false)
        .with_exclude_external_targets(true);
    let version = crate::tools::Tool::version(&tool);

    let spec = ctx.spec.clone().with_seed(opts.seed);
    let scratch = Scratch::new(opts.workdir.as_deref())?;
    let root = scratch.path();
    let revs = emit_revisions(&spec, opts, root)?;
    let shim = write_shim(root, &revs.fixtures)?;
    let bd_timing = time_bazel_diff(&tool, root, &revs, &shim)?;
    let cam_timing = time_cambium(ctx, &revs)?;

    // Only rule labels are comparable: bazel-diff's hash files also cover
    // source files and external targets, which cambium's affected set never
    // names.
    let graph = &revs.graph;
    let rules: BTreeSet<String> = (0..graph.targets.len() as u32)
        .filter(|&i| graph.targets[i as usize].alive)
        .map(|i| graph.target_label(i))
        .collect();
    let bd: BTreeSet<String> = std::fs::read_to_string(&bd_timing.impacted)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| rules.contains(*l))
        .map(str::to_string)
        .collect();
    let cam: BTreeSet<String> = cam_timing
        .affected
        .into_iter()
        .filter(|l| rules.contains(l))
        .collect();

    let agreed: BTreeSet<String> = bd.intersection(&cam).cloned().collect();
    let cambium_only: BTreeSet<String> = cam.difference(&bd).cloned().collect();
    let bazel_diff_only: BTreeSet<String> = bd.difference(&cam).cloned().collect();

    let (cold_ms, warm_ms) = (bd_timing.cold_ms, bd_timing.warm_ms);
    let (cambium_median, cambium_p90) = (cam_timing.median_ms, cam_timing.p90_ms);
    let mut extra = BTreeMap::new();
    extra.insert("bazel_diff_cold_ms".to_string(), cold_ms);
    extra.insert("bazel_diff_warm_ms".to_string(), warm_ms);
    extra.insert("cambium_ms".to_string(), cambium_median);
    extra.insert("cambium_p90_ms".to_string(), cambium_p90);
    extra.insert(
        "speedup_vs_cold".to_string(),
        cold_ms / cambium_median.max(1e-9),
    );
    extra.insert(
        "speedup_vs_warm".to_string(),
        warm_ms / cambium_median.max(1e-9),
    );
    extra.insert("agreed".to_string(), agreed.len() as f64);
    extra.insert("cambium_only".to_string(), cambium_only.len() as f64);
    extra.insert("bazel_diff_only".to_string(), bazel_diff_only.len() as f64);
    extra.insert("bazel_diff_impacted".to_string(), bd.len() as f64);
    extra.insert("cambium_affected".to_string(), cam.len() as f64);
    extra.insert(
        "changed_files".to_string(),
        revs.commit.changes.len() as f64,
    );
    extra.insert("targets".to_string(), graph.num_alive_targets() as f64);

    let mut notes = BTreeMap::new();
    notes.insert("bazel_diff_version".to_string(), version.clone());
    notes.insert(
        "bazel_diff_impl".to_string(),
        tool.implementation().name().to_string(),
    );
    notes.insert("change_preset".to_string(), opts.change_preset.clone());

    let result = StageResult {
        stage: STAGE.to_string(),
        preset: ctx.preset.clone(),
        spec_hash: spec.hash(),
        param: None,
        runs: ctx.runs,
        // The headline number is cambium's, so a sweep of this stage plots the
        // thing being sold; bazel-diff's numbers ride along in `extra`.
        median_ms: cambium_median,
        p90_ms: cambium_p90,
        peak_rss_mb: peak_rss_mb(),
        extra,
        notes,
    };

    if !bazel_diff_only.is_empty() {
        let sample: Vec<&str> = bazel_diff_only
            .iter()
            .take(10)
            .map(String::as_str)
            .collect();
        bail!(
            "unsound: bazel-diff reports {} label(s) cambium does not \
             (agreed {}, cambium-only {}); first: {}",
            bazel_diff_only.len(),
            agreed.len(),
            cambium_only.len(),
            sample.join(", ")
        );
    }

    Ok(LegacyRun {
        result,
        bazel_diff_version: version,
        agreed,
        cambium_only,
        bazel_diff_only,
    })
}

/// The two revisions on disk, and the same commit in memory.
struct Revisions {
    /// The graph after the commit: the head revision.
    graph: cambium_synth::SynthGraph,
    /// The index at the base revision, which cambium answers from.
    base_index: cambium_index::IndexData,
    commit: cambium_synth::Commit,
    base_ws: PathBuf,
    head_ws: PathBuf,
    /// The `streamed_proto` dumps the shim serves, one per revision.
    fixtures: PathBuf,
}

/// Base graph, and the head graph the same commit produces. The engine and
/// bazel-diff see byte-identical inputs from here on.
fn emit_revisions(
    spec: &cambium_synth::GraphSpec,
    opts: &LegacyOptions,
    root: &Path,
) -> Result<Revisions> {
    let mut graph = cambium_synth::generate(spec);
    let base_index = graph.build_index("c0");
    let change = ChangeSpec {
        seed: opts.seed,
        ..ChangeSpec::preset(&opts.change_preset)
            .with_context(|| format!("unknown change preset {}", opts.change_preset))?
    };
    let base_ws = root.join("base");
    let head_ws = root.join("head");
    let fixtures = root.join("fixtures");
    std::fs::create_dir_all(&fixtures)?;

    cambium_synth::emit_workspace(&graph, &base_ws)?;
    std::fs::write(
        fixtures.join("base.pb"),
        cambium_synth::proto::to_streamed_proto(&graph),
    )?;

    let commit = cambium_synth::commit(&mut graph, &change, 0);
    if commit.changes.is_empty() {
        bail!("change preset {} produced no changes", opts.change_preset);
    }
    cambium_synth::emit_workspace(&graph, &head_ws)?;
    std::fs::write(
        fixtures.join("head.pb"),
        cambium_synth::proto::to_streamed_proto(&graph),
    )?;
    Ok(Revisions {
        graph,
        base_index,
        commit,
        base_ws,
        head_ws,
        fixtures,
    })
}

/// bazel-diff's two numbers, and where its answer landed.
struct BazelDiffTiming {
    cold_ms: f64,
    warm_ms: f64,
    impacted: PathBuf,
}

/// bazel-diff, cold: hash both revisions, then diff. Warm: the base hashes
/// are already on disk from the cold run, so only head is rehashed.
///
/// The shim *is* the Bazel: it serves the recorded query for whichever
/// workspace it was pointed at, so the measurement is bazel-diff's own
/// hashing and diffing with no server in it.
fn time_bazel_diff(
    tool: &BazelDiff,
    root: &Path,
    revs: &Revisions,
    shim: &Path,
) -> Result<BazelDiffTiming> {
    let base_hashes = root.join("base.json");
    let head_hashes = root.join("head.json");
    let impacted = root.join("impacted.txt");
    let ctx_for = |ws: &Path| RunContext {
        bazel_binary: shim.to_path_buf(),
        startup_flags: Vec::new(),
        env: Vec::new(),
        scratch: root.to_path_buf(),
        base_worktree: ws.to_path_buf(),
    };
    let base_ctx = ctx_for(&revs.base_ws);
    let head_ctx = ctx_for(&revs.head_ws);
    let impact = || -> Result<()> {
        tool.get_impacted_targets(
            &revs.head_ws,
            &base_hashes,
            &head_hashes,
            &impacted,
            &head_ctx,
        )?;
        Ok(())
    };

    let t = std::time::Instant::now();
    tool.generate_hashes(&revs.base_ws, &base_hashes, &base_ctx, &[])?;
    tool.generate_hashes(&revs.head_ws, &head_hashes, &head_ctx, &[])?;
    impact()?;
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = std::time::Instant::now();
    tool.generate_hashes(&revs.head_ws, &head_hashes, &head_ctx, &[])?;
    impact()?;
    let warm_ms = t.elapsed().as_secs_f64() * 1000.0;
    Ok(BazelDiffTiming {
        cold_ms,
        warm_ms,
        impacted,
    })
}

/// cambium's numbers and its answer.
struct CambiumTiming {
    median_ms: f64,
    p90_ms: f64,
    affected: Vec<String>,
}

/// cambium, on the same commit against the base index, timed over the
/// stage's runs and then run once more for the answer.
fn time_cambium(ctx: &StageContext, revs: &Revisions) -> Result<CambiumTiming> {
    let engine = crate::stages::engine();
    let querier = SynthQuerier::new(&revs.graph);
    let req = AffectedRequest {
        base: "c0".into(),
        head: "c1".into(),
        include_manual: true,
        explain: false,
        no_full_build: true,
        materialize: false,
        publish_base: false,
        allow_partial: false,
        max_depth: None,
    };
    let (median_ms, p90_ms) = crate::harness::time(ctx.runs, || {
        engine
            .compute(revs.base_index.view(), &revs.commit.changes, &querier, &req)
            .expect("cambium pipeline")
    });
    let out = engine
        .compute(revs.base_index.view(), &revs.commit.changes, &querier, &req)
        .context("cambium pipeline")?;
    Ok(CambiumTiming {
        median_ms,
        p90_ms,
        affected: out.report.affected_labels(),
    })
}

// ------------------------------------------------------------------- driving

/// Writes the fake `bazel`. It answers the four subcommands bazel-diff issues,
/// and picks the fixture from the workspace it was pointed at, so one script
/// serves both revisions.
fn write_shim(root: &Path, fixtures: &Path) -> Result<PathBuf> {
    let path = root.join("bazel-shim");
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
fixtures={fixtures}
args=("$@")
command=
command_index=-1
workspace="$PWD"
for index in "${{!args[@]}}"; do
  case "${{args[$index]}}" in
    version|mod|info|query)
      if [ -z "$command" ]; then command="${{args[$index]}}"; command_index=$index; fi
      ;;
    -w|--workspacePath)
      workspace="${{args[$((index + 1))]}}"
      ;;
    --workspacePath=*)
      workspace="${{args[$index]#--workspacePath=}}"
      ;;
  esac
done
fixture="$fixtures/$(basename "$workspace").pb"
case "$command" in
  version)
    echo 'Build label: {version}'
    ;;
  mod)
    exit 1
    ;;
  info)
    echo "output_base: $fixtures/output_base"
    echo "workspace: $workspace"
    ;;
  query)
    output_file=
    for ((index = command_index + 1; index < ${{#args[@]}}; index++)); do
      case "${{args[$index]}}" in
        --output_file) ((index += 1)); output_file="${{args[$index]}}" ;;
        --output_file=*) output_file="${{args[$index]#--output_file=}}" ;;
      esac
    done
    if [ ! -f "$fixture" ]; then
      echo "no fixture for workspace $workspace" >&2
      exit 2
    fi
    if [ -n "$output_file" ]; then cp "$fixture" "$output_file"; else cat "$fixture"; fi
    ;;
  *)
    echo "unsupported shim invocation: $*" >&2
    exit 2
    ;;
esac
"#,
        fixtures = shell_quote(fixtures),
        version = SHIM_BAZEL_VERSION,
    );
    std::fs::write(&path, script)?;
    make_executable(&path)?;
    Ok(path)
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Either a caller-chosen directory or a temporary one that cleans itself up.
enum Scratch {
    Temp(tempfile::TempDir),
    Kept(PathBuf),
}

impl Scratch {
    fn new(dir: Option<&Path>) -> Result<Self> {
        match dir {
            Some(d) => {
                std::fs::create_dir_all(d)?;
                Ok(Self::Kept(d.to_path_buf()))
            }
            None => Ok(Self::Temp(tempfile::tempdir()?)),
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Temp(t) => t.path(),
            Self::Kept(p) => p,
        }
    }
}
