//! bazel-contrib's [target-determinator] as a [`Tool`], built by
//! `tools/target-determinator/fetch.sh`.
//!
//! The fourth shape in the comparison, and the only one that works from the
//! *configured* graph. bazel-diff hashes each revision's loaded targets into a
//! `{label: hash}` file and diffs the files; Tango computes a target graph per
//! revision and diffs the graphs; cambium keeps a persistent index and splices
//! the packages a commit touched; target-determinator runs `bazel cquery` at
//! each revision, hashes each *configured* target, and reports the ones whose
//! hash moved. Its answer is a flat set of labels printed one per line, from
//! one invocation that does both revisions; it runs in a checkout of its own
//! (see [`TargetDeterminator::affected`]).
//!
//! # Flags
//!
//! Two of the tool's defaults are deliberately overridden, and both are places
//! where what a CI pipeline wants and what a comparison wants differ:
//!
//! - **`-before-query-error-behavior fatal`** instead of
//!   `ignore-and-build-all`. A pipeline that cannot query the base revision
//!   should build everything; a comparison that did so would record
//!   "everything is affected" as target-determinator's *answer*, at the
//!   precision of a global invalidation, rather than as the failure it is.
//!   `fatal` turns it into a non-zero exit, which [`run_capture`] reports with
//!   the tail of stderr the way every other foil's failure is reported.
//! - **`-nocache_results`**. The cquery result cache persists across
//!   invocations, so with it on a change set's cost would depend on which
//!   change sets ran before it. Its key also excludes the environment and the
//!   machine, which upstream's own README warns about.
//!
//! `-enforce-clean` is left at upstream's `allow-ignored`: the harness writes
//! an untracked `cambium.toml` into every checkout and excludes it through
//! `.git/info/exclude` (see `crate::checkout`), which a worktree shares with
//! the checkout that owns it, so the tool sees a clean tree and does not go
//! looking for a worktree of its own.
//!
//! [target-determinator]: https://github.com/bazel-contrib/target-determinator

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::tools::{
    normalise_label, run_capture, short_sha, Install, RunContext, Tool, ToolLocations, ToolRun,
};

/// Environment overrides, in the order they are consulted.
pub const BIN_ENV: &str = "CAMBIUM_TARGET_DETERMINATOR";
pub const DIR_ENV: &str = "CAMBIUM_TARGET_DETERMINATOR_DIR";

pub const NAME: &str = "target-determinator";

/// The head checkout the tool is handed, under the run's scratch. Reused
/// between change sets: see [`TargetDeterminator::affected`].
pub const HEAD_WORKTREE: &str = "target-determinator-head";

/// What the binary prints to *stdout* before dying, so that a pipeline feeding
/// its output straight to `bazel test` fails instead of testing nothing. Here
/// it would parse as a label if it were not caught.
const ERROR_LINE: &str = "Target Determinator invocation Error";

/// How `tools/target-determinator/fetch.sh` lays the installation out.
const INSTALL: Install = Install {
    cache_name: "target-determinator",
    binary: "bin/target-determinator",
    bin_env: BIN_ENV,
    dir_env: DIR_ENV,
    fetch_script: "tools/target-determinator/fetch.sh",
};

/// The tag and commit `fetch.sh` recorded, as one line.
///
/// Preferred over asking the binary: `go build` leaves upstream's version
/// unstamped (`0.0.0-unstamped`, since it comes from Bazel's workspace
/// status), so the pin is the only identity there is to record.
pub fn read_version_file(dir: &Path) -> Option<String> {
    Install::tag_and_commit(dir)
}

/// A located `target-determinator`.
#[derive(Clone, Debug)]
pub struct TargetDeterminator {
    bin: PathBuf,
    version: String,
}

impl TargetDeterminator {
    /// The pattern every run considers unless told otherwise, which is also
    /// upstream's default and the universe cambium's index covers.
    pub const DEFAULT_TARGETS: &'static str = "//...";

    /// The binary the command line named, else the one under the installation
    /// directory.
    pub fn discover_with(at: &ToolLocations) -> Result<Self> {
        let dir = INSTALL.dir(at.target_determinator_dir.as_deref());
        match &at.target_determinator {
            Some(bin) => Ok(Self::at(
                bin.clone(),
                read_version_file(&dir).unwrap_or_else(|| "unknown".into()),
            )),
            None => Self::discover_in(&dir),
        }
    }

    /// The installation under `dir`, as the fetch script lays one out.
    pub fn discover_in(dir: &Path) -> Result<Self> {
        let bin = INSTALL.locate_in(dir)?;
        let version = read_version_file(dir).unwrap_or_else(|| "unknown".into());
        Ok(Self::at(bin, version))
    }

    pub fn at(bin: PathBuf, version: String) -> Self {
        Self { bin, version }
    }

    /// The command line for one change set, in order.
    ///
    /// `ws` is the checkout the tool queries, which must already be at head:
    /// the tool derives its "after" revision from whatever is checked out
    /// there and takes only the "before" one, as the single positional
    /// argument that comes last.
    pub fn args(&self, ws: &Path, base: &str, ctx: &RunContext) -> Vec<String> {
        let mut args = vec![
            "-working-directory".to_string(),
            ws.display().to_string(),
            "-bazel".to_string(),
            ctx.bazel_binary.display().to_string(),
            "-targets".to_string(),
            Self::DEFAULT_TARGETS.to_string(),
            "-enforce-clean".to_string(),
            "allow-ignored".to_string(),
            "-analysis-cache-clear-strategy".to_string(),
            "skip".to_string(),
            "-before-query-error-behavior".to_string(),
            "fatal".to_string(),
            "-cache-dir".to_string(),
            ctx.scratch_file("target-determinator-cache")
                .display()
                .to_string(),
        ];
        // One flag per option: upstream's `-bazel-startup-opts` appends, where
        // bazel-diff and tango take the whole list as one space-joined string.
        // Not `startup_flags`: an output base is per workspace directory and
        // this tool runs in its own. See `RunContext::tool_startup_flags`.
        for flag in ctx.tool_startup_flags() {
            args.push("-bazel-startup-opts".to_string());
            args.push(flag);
        }
        // The cquery result cache stays off; see the module docs.
        args.push("-nocache_results".to_string());
        args.push(base.to_string());
        args
    }
}

impl Tool for TargetDeterminator {
    fn name(&self) -> &str {
        NAME
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    /// One invocation per change set, against a head checkout of our own.
    ///
    /// The tool checks the "before" revision out *in the directory it was
    /// given* whenever that directory is clean, and restores it afterwards.
    /// Pointed at `ws` — cambium's own checkout, which the rest of the run is
    /// reading — that would move head out from under the run and throw the
    /// Bazel analysis cache away twice per change set. So it gets a worktree
    /// of its own under the run's scratch instead, pinned at head.
    ///
    /// That worktree is *kept* between change sets rather than removed on the
    /// way out. Re-adding it per change set would mean a fresh checkout of
    /// every file in the repository (tensorflow, envoy) and so a cold Bazel
    /// server every time, which would price the harness's bookkeeping as the
    /// tool's cost.
    fn affected(&self, ws: &Path, base: &str, head: &str, ctx: &RunContext) -> Result<ToolRun> {
        std::fs::create_dir_all(&ctx.scratch)?;
        let wt = cambium_diff::Worktree::create(ws, head, &ctx.scratch_file(HEAD_WORKTREE))
            .with_context(|| format!("adding a head worktree at {head} for {NAME}"))?;
        crate::tools::mirror_untracked_config(ws, wt.path())?;

        let mut cmd = ctx.command(&self.bin);
        cmd.args(self.args(wt.path(), base, ctx));
        let c = run_capture(
            &mut cmd,
            &format!("{NAME} {}..{}", short_sha(base), short_sha(head)),
        )?;
        wt.keep();

        // The tool prints to stdout and has no output-file flag, so the raw
        // answer is only inspectable after the fact if we write it down.
        let out = ctx.scratch_file(&format!(
            "{NAME}-{}-{}.txt",
            short_sha(base),
            short_sha(head)
        ));
        std::fs::write(&out, &c.stdout)
            .with_context(|| format!("writing {NAME} output {}", out.display()))?;

        Ok(ToolRun {
            labels: parse_output(&c.stdout)?,
            cold_wall: c.wall,
        })
    }
}

/// Parses the tool's stdout into normalised labels.
///
/// One label per line. Under `-verbose` the label is followed by a space and
/// an explanation of why it is affected, so only the first token of a line is
/// read; without it the line is the label alone.
///
/// Anything that is not a label is an error rather than a skipped line. An
/// empty set reads as "the tools agree", so it must never be what output we
/// failed to understand degrades to — and the one thing the binary is known to
/// print to stdout that is not a label is the sentinel it emits when it is
/// about to die.
pub fn parse_output(text: &str) -> Result<BTreeSet<String>> {
    let mut labels = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        anyhow::ensure!(
            line != ERROR_LINE,
            "{NAME} printed {ERROR_LINE:?}: the invocation failed"
        );
        let label = line.split_whitespace().next().unwrap_or(line);
        anyhow::ensure!(
            label.starts_with("//") || label.starts_with('@'),
            "{NAME} printed a line that is not a label: {line:?}"
        );
        labels.insert(normalise_label(label));
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::git;

    fn ctx(scratch: &Path) -> RunContext {
        RunContext {
            bazel_binary: "bazelisk".into(),
            startup_flags: vec!["--output_base=/tmp/ob".into(), "--bazelrc=/tmp/rc".into()],
            env: vec![("USE_BAZEL_VERSION".into(), "7.4.1".into())],
            scratch: scratch.to_path_buf(),
            base_worktree: PathBuf::from("/tmp/base"),
        }
    }

    fn td() -> TargetDeterminator {
        TargetDeterminator::at(PathBuf::from("/bin/td"), "test".into())
    }

    #[test]
    fn the_before_revision_is_the_one_positional_argument_and_comes_last() {
        let args = td().args(Path::new("/ws"), "abc123", &ctx(Path::new("/tmp/s")));
        assert_eq!(args.last().unwrap(), "abc123");
        assert_eq!(
            args.iter().filter(|a| *a == "abc123").count(),
            1,
            "the head revision is never named: the tool reads it from the checkout"
        );
    }

    #[test]
    fn the_defaults_are_the_ones_a_comparison_needs_not_the_ones_a_pipeline_needs() {
        let args = td().args(Path::new("/ws"), "abc123", &ctx(Path::new("/tmp/s")));
        let after = |flag: &str| -> Option<String> {
            let i = args.iter().position(|a| a == flag)?;
            args.get(i + 1).cloned()
        };
        // A failed base query is a failure, not an answer of "everything".
        assert_eq!(
            after("-before-query-error-behavior").as_deref(),
            Some("fatal")
        );
        // Every change set is measured cold, and nothing is kept in $HOME.
        assert!(args.iter().any(|a| a == "-nocache_results"));
        assert_eq!(
            after("-cache-dir").as_deref(),
            Some("/tmp/s/target-determinator-cache")
        );
        assert_eq!(after("-targets").as_deref(), Some("//..."));
        assert_eq!(after("-enforce-clean").as_deref(), Some("allow-ignored"));
        assert_eq!(
            after("-analysis-cache-clear-strategy").as_deref(),
            Some("skip")
        );
        assert_eq!(after("-working-directory").as_deref(), Some("/ws"));
        assert_eq!(after("-bazel").as_deref(), Some("bazelisk"));
        assert!(!args.iter().any(|a| a == "-verbose"));
    }

    /// One flag per option, unlike bazel-diff and tango which take the whole
    /// list as one string; and never the run's own output base.
    #[test]
    fn each_startup_option_gets_its_own_flag_and_the_output_base_is_dropped() {
        let args = td().args(Path::new("/ws"), "abc123", &ctx(Path::new("/tmp/s")));
        let opts: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && args[i - 1] == "-bazel-startup-opts")
            .map(|(_, a)| a)
            .collect();
        assert_eq!(opts, vec!["--bazelrc=/tmp/rc"]);
        assert!(!args.iter().any(|a| a.contains("output_base")), "{args:?}");
    }

    #[test]
    fn the_output_parses_into_one_label_per_line() {
        let text = "//pkg:t\n//other:u\n\n@ext//:x\n";
        let labels = parse_output(text).unwrap();
        assert_eq!(
            labels.iter().cloned().collect::<Vec<_>>(),
            vec!["//other:u", "//pkg:t", "@ext//:x"],
            "blank lines are not labels"
        );
    }

    /// `-verbose` puts the explanation after the label on the same line.
    #[test]
    fn a_verbose_line_keeps_only_the_label() {
        let text = "//pkg:t Changes: InputFileHash(//pkg:a.txt), RuleAttr(srcs)\n";
        let labels = parse_output(text).unwrap();
        assert_eq!(labels.iter().cloned().collect::<Vec<_>>(), vec!["//pkg:t"]);
    }

    #[test]
    fn external_labels_are_normalised_on_the_way_in() {
        let labels = parse_output("@@rules_cc+//:x\n@@//pkg:t\n").unwrap();
        assert_eq!(
            labels.iter().cloned().collect::<Vec<_>>(),
            vec!["//pkg:t", "@rules_cc//:x"]
        );
    }

    #[test]
    fn output_that_is_not_labels_is_an_error_not_an_empty_answer() {
        // An empty set reads as "the tools agree", so it must never be the
        // fallback for output we failed to understand.
        let err = parse_output("//pkg:t\nTarget Determinator invocation Error\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invocation failed"), "{err}");
        assert!(parse_output("bazel: command not found").is_err());
        assert_eq!(parse_output("").unwrap().len(), 0);
    }

    #[test]
    fn the_version_comes_from_the_file_fetch_sh_wrote() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_version_file(dir.path()), None);
        std::fs::write(
            dir.path().join("VERSION"),
            "tag=v0.34.0\ncommit=d4b6125546979713431e63b5c3e65810fa989446\nrepo=x\n",
        )
        .unwrap();
        assert_eq!(
            read_version_file(dir.path()).as_deref(),
            Some("v0.34.0 d4b6125")
        );
    }

    #[test]
    fn a_missing_installation_says_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        let err = TargetDeterminator::discover_in(dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("bin/target-determinator"), "{err}");
        assert!(err.contains("fetch.sh"), "{err}");
    }

    /// The whole invocation against a stub binary: the tool must be handed a
    /// checkout that is *not* cambium's own, pinned at head, and that checkout
    /// must still be there for the next change set to reuse.
    #[test]
    fn the_tool_queries_a_head_worktree_of_its_own_and_that_worktree_survives() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalised: on macOS `$TMPDIR` is a symlink, and git records a
        // worktree's registered path already resolved.
        let dir_path = dir.path().canonicalize().unwrap();
        let repo = dir_path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.email", "t@example.com"]).unwrap();
        git(&repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "--quiet", "-m", "one"]).unwrap();
        let base = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        std::fs::write(repo.join("a.txt"), "two").unwrap();
        git(&repo, &["commit", "--quiet", "-am", "two"]).unwrap();
        let head = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let scratch = dir_path.join("scratch");
        let stub = write_stub(&dir_path);
        let run = TargetDeterminator::at(stub, "test".into())
            .affected(&repo, &base, &head, &ctx(&scratch))
            .expect("the stub answers");
        assert_eq!(
            run.labels.iter().cloned().collect::<Vec<_>>(),
            vec!["//pkg:t".to_string()]
        );
        // The raw answer is written down, since the tool only prints it.
        let raw = std::fs::read_to_string(scratch.join(format!(
            "{NAME}-{}-{}.txt",
            short_sha(&base),
            short_sha(&head)
        )))
        .unwrap();
        assert!(raw.contains("//pkg:t"), "{raw}");

        let wt = scratch.join(HEAD_WORKTREE);
        assert!(wt.is_dir(), "the worktree is kept for the next change set");
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).unwrap(),
            "two",
            "and it is pinned at head"
        );
        // cambium's own checkout was never handed over, so it never moved.
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "two");
        assert_eq!(
            std::fs::read_to_string(scratch.join("working-directory")).unwrap(),
            wt.display().to_string(),
            "the tool was pointed at the worktree, not at the repository"
        );
    }

    /// A stand-in that records the `-working-directory` it was given and
    /// prints one label, the way the real binary does.
    fn write_stub(dir: &Path) -> PathBuf {
        let path = dir.join("target-determinator-stub.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
set -e
ws=""; cache=""
while [ $# -gt 0 ]; do
  case "$1" in
    -working-directory) ws="$2"; shift 2 ;;
    -cache-dir) cache="$2"; shift 2 ;;
    *) shift ;;
  esac
done
mkdir -p "$(dirname "$cache")"
printf '%s' "$ws" > "$(dirname "$cache")/working-directory"
echo "//pkg:t"
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }
}
