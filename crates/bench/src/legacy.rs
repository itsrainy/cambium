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
use std::process::Command;

use anyhow::{bail, Context, Result};
use cambium_engine::{AffectedRequest, SynthQuerier};
use cambium_synth::ChangeSpec;

use crate::harness::{peak_rss_mb, StageResult};
use crate::stages::StageContext;

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
    /// Explicit bazel-diff binary; otherwise `CAMBIUM_BAZEL_DIFF`, otherwise
    /// the path `tools/bazel-diff/fetch.sh` writes.
    pub binary: Option<PathBuf>,
    /// Keep the scratch workspaces here instead of a temporary directory.
    pub workdir: Option<PathBuf>,
}

impl Default for LegacyOptions {
    fn default() -> Self {
        Self {
            change_preset: "typical-pr".to_string(),
            seed: 0,
            binary: None,
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

/// Locates the bazel-diff binary, honouring the explicit path, then the
/// `CAMBIUM_BAZEL_DIFF` override, then the default cache location.
pub fn bazel_diff_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    let candidates = [
        explicit.map(PathBuf::from),
        std::env::var_os("CAMBIUM_BAZEL_DIFF").map(PathBuf::from),
        default_binary(),
    ];
    candidates.into_iter().flatten().find(|p| p.is_file())
}

fn default_binary() -> Option<PathBuf> {
    let dir = match std::env::var_os("CAMBIUM_BAZEL_DIFF_DIR") {
        Some(d) => PathBuf::from(d),
        None => home()?.join(".cache/cambium/bazel-diff"),
    };
    Some(dir.join("bin/bazel-diff-rust"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `<commit> <tag>` from the VERSION file the fetch script writes next to
/// `bin/`, so a result says which bazel-diff it was measured against.
fn version_of(binary: &Path) -> String {
    binary
        .parent()
        .and_then(|b| b.parent())
        .map(|d| d.join("VERSION"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Runs the stage. Errors when bazel-diff is missing, when it fails, or when
/// it reports a label cambium does not.
pub fn run(ctx: &StageContext, opts: &LegacyOptions) -> Result<LegacyRun> {
    let binary = bazel_diff_binary(opts.binary.as_deref()).ok_or_else(|| {
        anyhow::anyhow!(
            "bazel-diff binary not found; build it with tools/bazel-diff/fetch.sh \
             or set CAMBIUM_BAZEL_DIFF"
        )
    })?;
    let version = version_of(&binary);

    // Base graph, and the head graph the same commit produces. The engine and
    // bazel-diff see byte-identical inputs from here on.
    let spec = ctx.spec.clone().with_seed(opts.seed);
    let mut graph = cambium_synth::generate(&spec);
    let base_index = graph.build_index("c0");
    let change = ChangeSpec {
        seed: opts.seed,
        ..ChangeSpec::preset(&opts.change_preset)
            .with_context(|| format!("unknown change preset {}", opts.change_preset))?
    };

    let scratch = Scratch::new(opts.workdir.as_deref())?;
    let root = scratch.path();
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

    let shim = write_shim(root, &fixtures)?;

    // bazel-diff, cold: hash both revisions, then diff. Warm: the base hashes
    // are already on disk from the previous run, so only head is rehashed.
    let base_hashes = root.join("base.json");
    let head_hashes = root.join("head.json");
    let impacted = root.join("impacted.txt");

    let t = std::time::Instant::now();
    generate_hashes(&binary, &shim, &base_ws, &base_hashes)?;
    generate_hashes(&binary, &shim, &head_ws, &head_hashes)?;
    get_impacted(
        &binary,
        &shim,
        &head_ws,
        &base_hashes,
        &head_hashes,
        &impacted,
    )?;
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = std::time::Instant::now();
    generate_hashes(&binary, &shim, &head_ws, &head_hashes)?;
    get_impacted(
        &binary,
        &shim,
        &head_ws,
        &base_hashes,
        &head_hashes,
        &impacted,
    )?;
    let warm_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Only rule labels are comparable: bazel-diff's hash files also cover
    // source files and external targets, which cambium's affected set never
    // names.
    let rules: BTreeSet<String> = (0..graph.targets.len() as u32)
        .filter(|&i| graph.targets[i as usize].alive)
        .map(|i| graph.target_label(i))
        .collect();
    let bd: BTreeSet<String> = std::fs::read_to_string(&impacted)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| rules.contains(*l))
        .map(str::to_string)
        .collect();

    // cambium, on the same commit against the base index.
    let engine = crate::stages::engine();
    let querier = SynthQuerier::new(&graph);
    let req = AffectedRequest {
        base: "c0".into(),
        head: "c1".into(),
        include_manual: true,
        explain: false,
        no_full_build: true,
        materialize: false,
        publish_base: false,
        max_depth: None,
    };
    let (cambium_median, cambium_p90) = crate::harness::time(ctx.runs, || {
        engine
            .compute(base_index.view(), &commit.changes, &querier, &req)
            .expect("cambium pipeline")
    });
    let out = engine
        .compute(base_index.view(), &commit.changes, &querier, &req)
        .context("cambium pipeline")?;
    let cam: BTreeSet<String> = out
        .report
        .affected_labels()
        .into_iter()
        .filter(|l| rules.contains(l))
        .collect();

    let agreed: BTreeSet<String> = bd.intersection(&cam).cloned().collect();
    let cambium_only: BTreeSet<String> = cam.difference(&bd).cloned().collect();
    let bazel_diff_only: BTreeSet<String> = bd.difference(&cam).cloned().collect();

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
    extra.insert("changed_files".to_string(), commit.changes.len() as f64);
    extra.insert("targets".to_string(), graph.num_alive_targets() as f64);

    let mut notes = BTreeMap::new();
    notes.insert("bazel_diff_version".to_string(), version.clone());
    notes.insert(
        "bazel_diff_binary".to_string(),
        binary.display().to_string(),
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

// ------------------------------------------------------------------- driving

fn generate_hashes(binary: &Path, shim: &Path, workspace: &Path, out: &Path) -> Result<()> {
    run_bazel_diff(
        binary,
        &[
            "generate-hashes".as_ref(),
            "-w".as_ref(),
            workspace.as_os_str(),
            "-b".as_ref(),
            shim.as_os_str(),
            "--excludeExternalTargets".as_ref(),
            out.as_os_str(),
        ],
        workspace,
    )
}

fn get_impacted(
    binary: &Path,
    shim: &Path,
    workspace: &Path,
    starting: &Path,
    final_: &Path,
    out: &Path,
) -> Result<()> {
    run_bazel_diff(
        binary,
        &[
            "get-impacted-targets".as_ref(),
            "--startingHashes".as_ref(),
            starting.as_os_str(),
            "--finalHashes".as_ref(),
            final_.as_os_str(),
            "-w".as_ref(),
            workspace.as_os_str(),
            "-b".as_ref(),
            shim.as_os_str(),
            "-o".as_ref(),
            out.as_os_str(),
        ],
        workspace,
    )
}

fn run_bazel_diff(binary: &Path, args: &[&std::ffi::OsStr], cwd: &Path) -> Result<()> {
    let out = Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running {}", binary.display()))?;
    if !out.status.success() {
        bail!(
            "bazel-diff {:?} failed ({}):\n{}",
            args.first().map(|a| a.to_string_lossy().to_string()),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

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
