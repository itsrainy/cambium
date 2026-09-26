//! Uber's [Tango] as a [`Tool`], through the `tango-cli` driver in
//! `tools/tango`.
//!
//! Tango is the third shape in the comparison. bazel-diff hashes each
//! revision's targets into a `{label: hash}` file and diffs the files;
//! cambium keeps a persistent index and splices the packages a commit
//! touched; Tango computes a *target graph* per revision, encodes both into
//! its TGB blob, and diffs the graphs. Its rows carry a distance (0 for a
//! target that changed in its own right, otherwise the reverse-dependency hops
//! from the nearest such seed), which the walk does not read: it scores sets.
//!
//! Tango's server caches graph blobs; the driver does not, and computes both
//! graphs on every invocation, so the wall recorded is the whole of that.
//!
//! # Flags
//!
//! Tango's driver runs `bazel query 'deps(//...:all-targets)'` once per
//! revision, and on a real repository at a real pin that query does not load
//! cleanly — `bazel/bazel` on macOS pulls in a GraalVM distribution with no
//! `macos-x64` build and an RBE config that needs Docker — so without
//! `--keep_going` Bazel exits 3 and tango-cli fails the change set. So
//! [`Tango::DEFAULT_EXTRA_ARGS`] passes `--keep_going` through tango-cli's
//! `-extra-args`, which places it after the `query` word. The result is the
//! same universe bazel-diff (run with `-k`) and the `rdeps` oracle see:
//! everything that loaded.
//!
//! Neither `-max-distance` nor `-exclude-external` is passed. The BFS is left
//! unlimited because cambium's walk is unlimited too, and an external label
//! cambium never indexed is part of the answer being compared, so hiding it
//! inside the tool would hide how much of the answer it is.
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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::tools::{
    normalise_label, run_capture, Install, RunContext, Tool, ToolLocations, ToolRun,
};
use crate::util::short;

/// Environment overrides, in the order they are consulted.
pub const BIN_ENV: &str = "CAMBIUM_TANGO";
pub const DIR_ENV: &str = "CAMBIUM_TANGO_DIR";

pub const NAME: &str = "tango";

/// How `tools/tango/fetch.sh` lays the installation out.
const INSTALL: Install = Install {
    cache_name: "tango",
    binary: "bin/tango-cli",
    bin_env: BIN_ENV,
    dir_env: DIR_ENV,
    fetch_script: "tools/tango/fetch.sh",
};

/// The tag and commit `fetch.sh` recorded, as one line.
///
/// The file is `key=value` per line; the binary itself has no `--version`
/// (upstream Tango ships no CLI at all), so this is the only identity the
/// comparison can record.
pub fn read_version_file(dir: &Path) -> Option<String> {
    Install::tag_and_commit(dir)
}

/// A located `tango-cli`.
#[derive(Clone, Debug)]
pub struct Tango {
    bin: PathBuf,
    version: String,
}

impl Tango {
    /// `$CAMBIUM_TANGO`, else `$CAMBIUM_TANGO_DIR/bin/tango-cli`, else
    /// `~/.cache/cambium/tango/bin/tango-cli`.
    pub fn discover() -> Result<Self> {
        Self::discover_with(&ToolLocations::from_env())
    }

    /// The `tango-cli` the command line named, else the one under the
    /// installation directory.
    pub fn discover_with(at: &ToolLocations) -> Result<Self> {
        let dir = INSTALL.dir(at.tango_dir.as_deref());
        match &at.tango {
            Some(bin) => Ok(Self::at(
                bin.clone(),
                read_version_file(&dir).unwrap_or_else(|| "unknown".into()),
            )),
            None => Self::discover_in(&dir),
        }
    }

    /// The installation under `dir`, as `tools/tango/fetch.sh` lays one out.
    pub fn discover_in(dir: &Path) -> Result<Self> {
        let bin = INSTALL.locate_in(dir)?;
        let version = read_version_file(dir).unwrap_or_else(|| "unknown".into());
        Ok(Self::at(bin, version))
    }

    /// The `bazel query` arguments every run passes through `-extra-args`.
    pub const DEFAULT_EXTRA_ARGS: &'static [&'static str] = &["--keep_going"];

    pub fn at(bin: PathBuf, version: String) -> Self {
        Self { bin, version }
    }
}

impl Tool for Tango {
    fn name(&self) -> &str {
        NAME
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    /// One invocation per change set.
    ///
    /// `ws` is the head worktree, and it is handed over as the *repository*:
    /// tango-cli resolves both revisions in it and adds its own two detached
    /// worktrees, so neither cambium's checkout nor `ctx.base_worktree` is
    /// touched. The scratch directory is pinned under the run's scratch so a
    /// killed run leaves its worktrees somewhere findable rather than in
    /// `$TMPDIR`.
    fn affected(&self, ws: &Path, base: &str, head: &str, ctx: &RunContext) -> Result<ToolRun> {
        std::fs::create_dir_all(&ctx.scratch)?;
        let out = ctx.scratch_file(&format!(
            "tango-{}-{}.json",
            short(base, 12),
            short(head, 12)
        ));
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
        // Not `startup_flags`: tango runs Bazel in its own two worktrees, and
        // sharing an output base across workspace directories corrupts
        // workspace-absolute repository symlinks. See
        // `RunContext::tool_startup_flags`.
        let startup = ctx.tool_startup_flags();
        if !startup.is_empty() {
            cmd.arg("-startup-options").arg(startup.join(" "));
        }
        cmd.arg("-extra-args")
            .arg(Self::DEFAULT_EXTRA_ARGS.join(" "));
        let c = run_capture(
            &mut cmd,
            &format!("tango-cli {}..{}", short(base, 12), short(head, 12)),
        )?;
        let _ = std::fs::remove_dir_all(&worktrees);

        let text = std::fs::read_to_string(&out)
            .with_context(|| format!("reading tango output {}", out.display()))?;
        Ok(ToolRun {
            labels: parse_output(&text)?,
            cold_wall: c.wall,
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

/// Parses tango-cli's output into normalised labels.
///
/// Every row is kept, `deleted` ones included: a target that vanished between
/// base and head is exactly what cambium reports through `DeletedDep`, and
/// dropping them here would manufacture disagreements.
pub fn parse_output(text: &str) -> Result<BTreeSet<String>> {
    let rows: Vec<Row> = serde_json::from_str(text.trim()).context("parsing tango-cli JSON")?;
    Ok(rows.iter().map(|r| normalise_label(&r.label)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::git;

    /// A change set must still run when its scratch directory holds a worktree
    /// registration that is both missing from disk *and* locked.
    ///
    /// That is exactly what a killed run leaves behind: `tango-cli`'s own
    /// `addWorktree` (`tools/tango/cmd/tango-cli/main.go`) removes the
    /// directory before re-adding it, but a run that dies before its
    /// deferred cleanup runs can leave the registration behind, and — as
    /// happened against sourcegraph/sourcegraph, where 9 of 10 change sets failed
    /// with `fatal: '.../tango-worktrees/head' is a missing but locked
    /// worktree` — git then refuses `worktree add` outright rather than
    /// treating the stale entry as prunable. `tango-cli`'s `addWorktree` now
    /// unlocks, prunes, and force-removes a stale registration before
    /// adding (in that order: `prune` skips a locked entry by design, so the
    /// lock has to go first); this test stands a stub in for the real binary
    /// that performs the same recipe, so a regression that drops the
    /// recovery step (from either side of the boundary) shows up here
    /// without a real tango-cli build.
    #[test]
    fn a_change_set_still_runs_over_a_locked_missing_worktree() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalised: on macOS `$TMPDIR` is itself a symlink
        // (`/var/folders/... -> /private/var/folders/...`), and `git
        // worktree unlock`/`remove` match a worktree by its *registered*
        // absolute path, which git records already resolved. Passing the
        // symlinked form here would make every recovery command below fail
        // with "is not a working tree" against the very entry `git worktree
        // list` just showed us, which is a self-inflicted test bug, not the
        // one this test exists to catch.
        let dir_path = dir.path().canonicalize().unwrap();
        let repo = dir_path.join("repo");
        cambium_testkit::init_repo(&repo);
        cambium_testkit::write(&repo, "a.txt", "one");
        let base = cambium_testkit::commit_all(&repo, "one");
        cambium_testkit::write(&repo, "a.txt", "two");
        let head = cambium_testkit::commit_all(&repo, "two");

        // Same layout `Tango::affected` uses: `<scratch>/tango-worktrees/{base,head}`.
        let scratch = dir_path.join("scratch");
        let worktrees = scratch.join("tango-worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        let head_wt = worktrees.join("head");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                head_wt.to_str().unwrap(),
                &base,
            ],
        )
        .unwrap();
        git(&repo, &["worktree", "lock", head_wt.to_str().unwrap()]).unwrap();
        // Gone from disk, but still registered and locked — exactly the
        // state a killed run leaves behind and `git worktree prune` alone
        // will not clear, since prune skips locked entries by design.
        std::fs::remove_dir_all(&worktrees).unwrap();
        let listing = git(&repo, &["worktree", "list"]).unwrap();
        assert!(
            listing.contains("locked"),
            "the stale registration must still be there for this test to mean anything: {listing}"
        );

        let stub = write_recovering_stub(&dir_path);
        let ctx = RunContext {
            bazel_binary: "bazel".into(),
            startup_flags: vec![],
            env: vec![],
            scratch: scratch.clone(),
            base_worktree: dir_path.join("unused"),
        };
        let tango = Tango::at(stub, "test".into());
        let run = tango
            .affected(&repo, &base, &head, &ctx)
            .expect("the change set runs despite the stale locked worktree");
        assert_eq!(
            run.labels.iter().cloned().collect::<Vec<_>>(),
            vec!["//pkg:t".to_string()]
        );
    }

    /// A stand-in `tango-cli` that performs the same recovery
    /// `addWorktree` does before `git worktree add` — unlock, then prune
    /// (which only drops a missing entry once it is unlocked), then a
    /// force-remove for a locked entry prune does not cover — then adds both
    /// worktrees and writes one row of JSON to `-o`.
    fn write_recovering_stub(dir: &Path) -> PathBuf {
        let path = dir.join("tango-cli-stub.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
set -e
ws=""; base=""; head=""; scratch=""; out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -workspace) ws="$2"; shift 2 ;;
    -base) base="$2"; shift 2 ;;
    -head) head="$2"; shift 2 ;;
    -scratch) scratch="$2"; shift 2 ;;
    -o) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
for dir in "$scratch/base" "$scratch/head"; do
  git -C "$ws" worktree unlock "$dir" >/dev/null 2>&1 || true
  git -C "$ws" worktree prune >/dev/null 2>&1 || true
  git -C "$ws" worktree remove --force --force "$dir" >/dev/null 2>&1 || true
done
git -C "$ws" worktree add --detach --quiet "$scratch/base" "$base"
git -C "$ws" worktree add --detach --quiet "$scratch/head" "$head"
cat > "$out" <<JSON
[{"label": "//pkg:t", "distance": 0, "change_type": "changed"}]
JSON
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn the_output_parses_into_labels() {
        let text = r#"[
          {"label": "//pkg:t", "distance": 0, "change_type": "changed"},
          {"label": "//pkg:BUILD.bazel", "distance": 0, "change_type": "changed"},
          {"label": "//other:u", "distance": 2, "change_type": "changed"},
          {"label": "//gone:g", "distance": -1, "change_type": "deleted"}
        ]"#;
        let labels = parse_output(text).unwrap();
        assert_eq!(
            labels.iter().cloned().collect::<Vec<_>>(),
            vec!["//gone:g", "//other:u", "//pkg:BUILD.bazel", "//pkg:t"],
            "deleted targets are part of the answer"
        );
    }

    #[test]
    fn external_labels_are_normalised_on_the_way_in() {
        let text = r#"[{"label": "@@rules_cc+//:x", "distance": 1, "change_type": "changed"}]"#;
        let labels = parse_output(text).unwrap();
        assert!(labels.contains("@rules_cc//:x"), "{labels:?}");
    }

    #[test]
    fn a_non_json_output_is_an_error_not_an_empty_answer() {
        // An empty set reads as "the tools agree", so it must never be the
        // fallback for output we failed to understand.
        assert!(parse_output("tango-cli: boom").is_err());
        assert_eq!(parse_output("[]").unwrap().len(), 0);
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
        let err = Tango::discover_in(dir.path()).unwrap_err().to_string();
        assert!(err.contains("bin/tango-cli"), "{err}");
        assert!(err.contains("fetch.sh"), "{err}");
    }
}
