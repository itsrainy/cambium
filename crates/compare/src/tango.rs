//! Uber's [Tango] as a [`Producer`], through the `tango-cli` driver in
//! `tools/tango`.
//!
//! Tango is the third shape in the comparison. bazel-diff hashes each
//! revision's targets into a `{label: hash}` file and diffs the files;
//! cambium keeps a persistent index and splices the packages a commit
//! touched; Tango computes a *target graph* per revision, encodes both into
//! its TGB blob, and diffs the graphs — which is why it, alone of the three,
//! reports a **distance**: 0 for a target that changed in its own right, and
//! otherwise the number of reverse-dependency hops from the nearest such seed.
//!
//! Two consequences for this file:
//!
//! - **No warm mode.** Tango's server caches graph blobs; the driver does not,
//!   and computes both graphs on every invocation. So [`ProducerRun::warm_wall`]
//!   is `None` rather than a copy of the cold number — quoting the cold number
//!   twice would flatter cambium by making Tango look like it has no cache to
//!   lose, and quoting it as warm would flatter Tango.
//! - **Distances are kept.** They land in [`ProducerRun::depths`] and are
//!   reported against cambium's own per-target depth. Do not read a
//!   disagreement there as a bug in either tool: Tango's seed set is wider
//!   than cambium's direct set (every string attribute counts, including
//!   `generator_location`), so its seeds are systematically *wider*; and its
//!   graph contains file nodes, so a hop through a generated file costs a
//!   distance that cambium's rule-to-rule edge does not. On the smoke
//!   workspace, editing `lib/lib.txt` puts `//app:app` at Tango distance 2
//!   (`lib -> lib.out -> app`) against cambium depth 1 — a full agreement of
//!   0.5 on that pair, with neither tool wrong. The number is there to make
//!   that drift visible and measurable, not to gate anything.
//!
//! # Flags
//!
//! Tango's driver runs `bazel query 'deps(//...:all-targets)'` once per
//! revision, and on a real repository at a real pin that query does not load
//! cleanly — `bazel/bazel` on macOS pulls in a GraalVM distribution with no
//! `macos-x64` build and an RBE config that needs Docker — so without
//! `--keep_going` Bazel exits 3 and tango-cli fails the pair. So
//! [`Tango::DEFAULT_EXTRA_ARGS`] passes `--keep_going` through tango-cli's
//! `-extra-args`, which places it after the `query` word. The result is the
//! same universe bazel-diff (run with `-k`) and the `rdeps` oracle see:
//! everything that loaded. [`Tango::with_extra_args`] replaces the list.
//!
//! `--keep_going` alone is not enough: upstream Tango's Bazel client treats
//! *any* non-zero exit as a failed query, and `--keep_going` turns a load
//! error into exit **3** rather than into no error at all. So
//! `tools/tango/0001-keep-going-partial-graph.patch` — applied by `fetch.sh`, which
//! is the supported way to carry a local change to upstream — accepts exit 3
//! when the proto stream parsed, logs a warning, and uses the partial graph;
//! it also registers a dep that names an unloadable repository as an opaque
//! zero-hash node instead of failing the revision, which is sound here because
//! that repository is equally missing at base and at head.
//!
//! [Tango]: https://github.com/uber/tango

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::producer::{normalise_label, run_capture, Producer, ProducerRun, RunContext};

/// Environment overrides, in the order they are consulted.
pub const BIN_ENV: &str = "CAMBIUM_TANGO";
pub const DIR_ENV: &str = "CAMBIUM_TANGO_DIR";

pub const NAME: &str = "tango";

/// `~/.cache/cambium/tango`, where `tools/tango/fetch.sh` installs.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(DIR_ENV) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache/cambium/tango")
}

/// The tag and commit `fetch.sh` recorded, as one line.
///
/// The file is `key=value` per line; the binary itself has no `--version`
/// (upstream Tango ships no CLI at all), so this is the only identity the
/// comparison can record.
pub fn read_version_file(dir: &Path) -> Option<String> {
    let s = std::fs::read_to_string(dir.join("VERSION")).ok()?;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in s.lines() {
        if let Some((k, v)) = line.trim().split_once('=') {
            fields.insert(k, v);
        }
    }
    let tag = fields.get("tag").copied()?;
    Some(match fields.get("commit") {
        Some(c) => format!("{tag} {}", &c[..c.len().min(7)]),
        None => tag.to_string(),
    })
}

/// A located `tango-cli`.
#[derive(Clone, Debug)]
pub struct Tango {
    bin: PathBuf,
    version: String,
    /// `-max-distance`; `None` leaves the BFS unlimited, which is what the
    /// comparison wants (cambium's walk is unlimited too).
    max_distance: Option<u32>,
    /// `-exclude-external`. Off by default: an external label cambium never
    /// indexed is a *classified* disagreement (`bazeldiff_extra_external`),
    /// and hiding it inside the producer would hide how much of the answer it
    /// is.
    exclude_external: bool,
    /// `-extra-args`: extra `bazel query` arguments. See the module docs.
    extra_args: Vec<String>,
}

impl Tango {
    /// `$CAMBIUM_TANGO`, else `$CAMBIUM_TANGO_DIR/bin/tango-cli`, else
    /// `~/.cache/cambium/tango/bin/tango-cli`.
    pub fn discover() -> Result<Self> {
        let dir = default_dir();
        let bin = std::env::var_os(BIN_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("bin/tango-cli"));
        anyhow::ensure!(
            bin.exists(),
            "no tango-cli at {}; set ${BIN_ENV}, or ${DIR_ENV}, \
             or run tools/tango/fetch.sh",
            bin.display()
        );
        let version = read_version_file(&dir).unwrap_or_else(|| "unknown".into());
        Ok(Self::at(bin, version))
    }

    /// The `bazel query` arguments every run passes unless told otherwise.
    pub const DEFAULT_EXTRA_ARGS: &'static [&'static str] = &["--keep_going"];

    pub fn at(bin: PathBuf, version: String) -> Self {
        Self {
            bin,
            version,
            max_distance: None,
            exclude_external: false,
            extra_args: Self::DEFAULT_EXTRA_ARGS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    /// Replaces the `-extra-args` list (default: `--keep_going`).
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    pub fn extra_args(&self) -> &[String] {
        &self.extra_args
    }

    pub fn with_max_distance(mut self, d: Option<u32>) -> Self {
        self.max_distance = d;
        self
    }

    pub fn with_exclude_external(mut self, yes: bool) -> Self {
        self.exclude_external = yes;
        self
    }

    pub fn binary(&self) -> &Path {
        &self.bin
    }
}

impl Producer for Tango {
    fn name(&self) -> &str {
        NAME
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    /// One invocation per pair.
    ///
    /// `ws` is the head worktree, and it is handed over as the *repository*:
    /// tango-cli resolves both revisions in it and adds its own two detached
    /// worktrees, so neither cambium's checkout nor `ctx.base_worktree` is
    /// touched. The scratch directory is pinned under the run's scratch so a
    /// killed run leaves its worktrees somewhere findable rather than in
    /// `$TMPDIR`.
    fn affected(&self, ws: &Path, base: &str, head: &str, ctx: &RunContext) -> Result<ProducerRun> {
        std::fs::create_dir_all(&ctx.scratch)?;
        let short = |s: &str| s.chars().take(12).collect::<String>();
        let out = ctx.scratch_file(&format!("tango-{}-{}.json", short(base), short(head)));
        let worktrees = ctx.scratch_file("tango-worktrees");
        // Left over from a killed run, the worktrees are stale checkouts of
        // the wrong revisions; tango-cli would fail to add over them.
        let _ = std::fs::remove_dir_all(&worktrees);

        let mut cmd = ctx.command(&self.bin);
        cmd.arg("-workspace")
            .arg(ws)
            .arg("-base")
            .arg(base)
            .arg("-head")
            .arg(head)
            .arg("-bazel")
            .arg(&ctx.bazel_binary)
            .arg("-scratch")
            .arg(&worktrees)
            .arg("-o")
            .arg(&out);
        if !ctx.startup_flags.is_empty() {
            cmd.arg("-startup-options").arg(ctx.startup_flags_arg());
        }
        if !self.extra_args.is_empty() {
            cmd.arg("-extra-args").arg(self.extra_args.join(" "));
        }
        if let Some(d) = self.max_distance {
            cmd.arg("-max-distance").arg(d.to_string());
        }
        if self.exclude_external {
            cmd.arg("-exclude-external");
        }
        let c = run_capture(
            &mut cmd,
            &format!("tango-cli {}..{}", short(base), short(head)),
        )?;
        let _ = std::fs::remove_dir_all(&worktrees);

        let text = std::fs::read_to_string(&out)
            .with_context(|| format!("reading tango output {}", out.display()))?;
        let (labels, depths) = parse_output(&text)?;
        Ok(ProducerRun {
            labels,
            // Tango computes both graphs every time: there is no warm side to
            // report.
            cold_wall: c.wall,
            warm_wall: None,
            raw_output_path: Some(out),
            depths: Some(depths),
            dropped_file_targets: 0,
        })
    }
}

/// One row of tango-cli's JSON.
#[derive(Clone, Debug, Deserialize)]
pub struct Row {
    pub label: String,
    /// Reverse-dependency hops from the nearest seed; `-1` for a changed
    /// target no seed reaches.
    pub distance: i64,
    /// `new`, `deleted`, or `changed`.
    #[serde(default)]
    pub change_type: String,
}

/// Parses tango-cli's output into normalised labels plus their distances.
///
/// Every row is kept, `deleted` ones included: a target that vanished between
/// base and head is exactly what cambium reports through `DeletedDep`, and
/// dropping them here would manufacture disagreements. A negative distance
/// (unreachable from any seed) is recorded as no depth rather than as zero,
/// because zero means something specific.
pub fn parse_output(text: &str) -> Result<(BTreeSet<String>, BTreeMap<String, u32>)> {
    let rows: Vec<Row> = serde_json::from_str(text.trim()).context("parsing tango-cli JSON")?;
    let mut labels = BTreeSet::new();
    let mut depths = BTreeMap::new();
    for r in rows {
        let label = normalise_label(&r.label);
        if r.distance >= 0 {
            // Keep the shortest, in case two spellings normalised together.
            let d = r.distance as u32;
            depths
                .entry(label.clone())
                .and_modify(|e: &mut u32| *e = (*e).min(d))
                .or_insert(d);
        }
        labels.insert(label);
    }
    Ok((labels, depths))
}

/// The fraction of labels both tools reported for which they agree on the
/// depth, or `None` when they share nothing to compare.
///
/// Deliberately over the *shared* labels only: a label one tool never reported
/// has no depth to disagree about, and folding those in would make the number
/// a restatement of precision.
pub fn depth_agreement(
    cambium: &BTreeMap<String, u32>,
    producer: &BTreeMap<String, u32>,
) -> Option<f64> {
    let mut shared = 0usize;
    let mut same = 0usize;
    for (label, d) in producer {
        if let Some(c) = cambium.get(label) {
            shared += 1;
            if c == d {
                same += 1;
            }
        }
    }
    (shared > 0).then(|| same as f64 / shared as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_parses_into_labels_and_distances() {
        let text = r#"[
          {"label": "//pkg:t", "distance": 0, "change_type": "changed"},
          {"label": "//pkg:BUILD.bazel", "distance": 0, "change_type": "changed"},
          {"label": "//other:u", "distance": 2, "change_type": "changed"},
          {"label": "//gone:g", "distance": -1, "change_type": "deleted"}
        ]"#;
        let (labels, depths) = parse_output(text).unwrap();
        assert_eq!(
            labels.iter().cloned().collect::<Vec<_>>(),
            vec!["//gone:g", "//other:u", "//pkg:BUILD.bazel", "//pkg:t"],
            "deleted targets are part of the answer"
        );
        assert_eq!(depths["//pkg:t"], 0);
        assert_eq!(depths["//other:u"], 2);
        assert!(
            !depths.contains_key("//gone:g"),
            "an unreachable target has no depth, which is not depth zero"
        );
    }

    #[test]
    fn external_labels_are_normalised_on_the_way_in() {
        let text = r#"[{"label": "@@rules_cc+//:x", "distance": 1, "change_type": "changed"}]"#;
        let (labels, depths) = parse_output(text).unwrap();
        assert!(labels.contains("@rules_cc//:x"), "{labels:?}");
        assert_eq!(depths["@rules_cc//:x"], 1);
    }

    #[test]
    fn a_non_json_output_is_an_error_not_an_empty_answer() {
        // An empty set reads as "the tools agree", so it must never be the
        // fallback for output we failed to understand.
        assert!(parse_output("tango-cli: boom").is_err());
        assert_eq!(parse_output("[]").unwrap().0.len(), 0);
    }

    #[test]
    fn the_version_comes_from_the_file_fetch_sh_wrote() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_version_file(dir.path()), None);
        std::fs::write(
            dir.path().join("VERSION"),
            "tag=v0.0.46\ncommit=005b0dec444d956cda42ebb62cce227362783948\nrepo=x\n",
        )
        .unwrap();
        assert_eq!(
            read_version_file(dir.path()).as_deref(),
            Some("v0.0.46 005b0de")
        );
    }

    #[test]
    fn a_missing_installation_says_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(DIR_ENV, dir.path());
        std::env::remove_var(BIN_ENV);
        let err = Tango::discover().unwrap_err().to_string();
        std::env::remove_var(DIR_ENV);
        assert!(err.contains("bin/tango-cli"), "{err}");
        assert!(err.contains("fetch.sh"), "{err}");
    }

    #[test]
    fn depth_agreement_is_taken_over_the_shared_labels() {
        let map = |v: &[(&str, u32)]| -> BTreeMap<String, u32> {
            v.iter().map(|(l, d)| (l.to_string(), *d)).collect()
        };
        let cambium = map(&[("//a:a", 0), ("//b:b", 2), ("//c:c", 1)]);
        let producer = map(&[("//a:a", 0), ("//b:b", 1), ("//z:z", 5)]);
        assert_eq!(depth_agreement(&cambium, &producer), Some(0.5));
        assert_eq!(
            depth_agreement(&cambium, &map(&[("//z:z", 5)])),
            None,
            "nothing shared is not full agreement"
        );
    }
}
