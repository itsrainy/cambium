//! What a "tool" is, and how the suite runs one.
//!
//! The comparison is not specific to bazel-diff. It asks: given a
//! `(base, head)` change set, which targets does *tool X* say are affected, and how
//! long did it take to say it? Anything that can answer that (bazel-diff's
//! Rust binary, its Kotlin jar, Uber's Tango, bazel-contrib's
//! target-determinator, a shell script) plugs in here
//! by implementing [`Tool`], and [`crate::walk`] scores whatever it is
//! handed against the `rdeps` oracle, as it scores cambium, without knowing
//! which tool it is talking to.
//!
//! Two things every such tool needs, and which is why they live in
//! [`RunContext`] rather than in each implementation:
//!
//! - **The same Bazel.** The run's startup flags and the environment
//!   (`USE_BAZEL_VERSION` where a repository pins one) travel with the request, so
//!   a tool runs the same binary and the same `.bazelrc` cambium does.
//!   `--output_base` is the one flag that must *not* be shared: see
//!   [`RunContext::tool_startup_flags`].
//! - **A checkout at base.** bazel-diff-shaped tools hash the workspace at
//!   each end of the change set, which means the base revision has to be on disk.
//!   Moving cambium's own checkout there and back would invalidate the Bazel
//!   analysis cache twice per change set and race with anything else reading the
//!   worktree, so the suite hands tools a *second* git worktree pinned at
//!   base ([`BaseWorktree`]) and leaves the head checkout alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use cambium_index::label::{Label, Repo};

pub mod bazel_query;
pub mod bazeldiff;
pub mod cambi;
pub mod tango;
pub mod target_determinator;

// ------------------------------------------------------------ finding them

/// Where the peer tools are, as the command line said.
///
/// Every field is "nothing was said", and the binary fills them from its
/// flags (each with the environment variable of the same meaning as a
/// fallback) so the library reads no environment on a workload's behalf.
/// [`ToolLocations::from_env`] is the one place it can, for a caller with no
/// command line (the real-tool tests).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolLocations {
    /// The bazel-diff Rust binary (`$CAMBIUM_BAZEL_DIFF`).
    pub bazel_diff: Option<PathBuf>,
    /// The bazel-diff deploy jar (`$CAMBIUM_BAZEL_DIFF_JAR`).
    pub bazel_diff_jar: Option<PathBuf>,
    /// Where `tools/bazel-diff/fetch.sh` installed (`$CAMBIUM_BAZEL_DIFF_DIR`).
    pub bazel_diff_dir: Option<PathBuf>,
    /// `tango-cli` (`$CAMBIUM_TANGO`).
    pub tango: Option<PathBuf>,
    /// Where `tools/tango/fetch.sh` installed (`$CAMBIUM_TANGO_DIR`).
    pub tango_dir: Option<PathBuf>,
    /// `target-determinator` (`$CAMBIUM_TARGET_DETERMINATOR`).
    pub target_determinator: Option<PathBuf>,
    /// Where `tools/target-determinator/fetch.sh` installed
    /// (`$CAMBIUM_TARGET_DETERMINATOR_DIR`).
    pub target_determinator_dir: Option<PathBuf>,
}

impl ToolLocations {
    /// The environment's answer for every field. The names are the ones
    /// `docs/OPERATIONS.md` tables.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Self {
            bazel_diff: var(bazeldiff::BIN_ENV),
            bazel_diff_jar: var(bazeldiff::JAR_ENV),
            bazel_diff_dir: var(bazeldiff::DIR_ENV),
            tango: var(tango::BIN_ENV),
            tango_dir: var(tango::DIR_ENV),
            target_determinator: var(target_determinator::BIN_ENV),
            target_determinator_dir: var(target_determinator::DIR_ENV),
        }
    }
}

/// How one tool's installation is laid out, so the two adapters share one
/// lookup instead of a copy each: an explicit binary wins, else the binary
/// at its place under the installation directory, which is explicit or the
/// cache directory `fetch.sh` writes into.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Install {
    /// `~/.cache/cambium/<cache_name>`.
    pub cache_name: &'static str,
    /// The binary's path under the installation directory.
    pub binary: &'static str,
    /// What the error names when nothing is found.
    pub bin_env: &'static str,
    pub dir_env: &'static str,
    pub fetch_script: &'static str,
}

impl Install {
    /// The installation directory: explicit, else the cache.
    pub fn dir(&self, explicit: Option<&Path>) -> PathBuf {
        explicit
            .map(Path::to_path_buf)
            .unwrap_or_else(|| crate::util::cache_dir(self.cache_name))
    }

    /// The binary under `dir`, which must exist.
    pub fn locate_in(&self, dir: &Path) -> Result<PathBuf> {
        let bin = dir.join(self.binary);
        anyhow::ensure!(
            bin.exists(),
            "no {} at {}; set ${}, or ${}, or run {}",
            Path::new(self.binary)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(self.binary),
            bin.display(),
            self.bin_env,
            self.dir_env,
            self.fetch_script
        );
        Ok(bin)
    }

    /// The `VERSION` file `fetch.sh` writes beside the binary, trimmed, or
    /// `None` when there is none.
    pub fn version_text(dir: &Path) -> Option<String> {
        let s = std::fs::read_to_string(dir.join("VERSION")).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    /// A `key=value` per line `VERSION` file, rendered as `tag` plus the
    /// short `commit`, or just the tag when there is no commit.
    ///
    /// Both Go peers are pinned by tag and commit and neither can be asked its
    /// own version (upstream Tango ships no CLI at all; a `go build` of
    /// target-determinator leaves the version unstamped), so the file the
    /// fetch script wrote is the only identity the comparison can record.
    pub fn tag_and_commit(dir: &Path) -> Option<String> {
        let s = Self::version_text(dir)?;
        let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
        for line in s.lines() {
            if let Some((k, v)) = line.trim().split_once('=') {
                fields.insert(k, v);
            }
        }
        let tag = fields.get("tag").copied()?;
        Some(match fields.get("commit") {
            Some(c) => format!("{tag} {}", crate::util::short(c, 7)),
            None => tag.to_string(),
        })
    }
}

// ------------------------------------------------------------ naming them

/// The tool names a `--tools` list asks for, before any is looked for on
/// disk.
///
/// The list is exact when given; otherwise the default is two peers (a
/// bazel-diff and Tango) because a comparison against one tool only tells
/// you which of the two disagrees with cambium, not which of the three is the
/// odd one out. target-determinator is the third peer and is named
/// explicitly rather than run by default: a published run is already eight
/// hours, and its shape (one `cquery` per revision, no warm half) is the one
/// a reader reaches for last. cambium is not a peer: it is what the peers are
/// compared against and runs on every change set whether or not it is named,
/// so naming it alone is how a run measures cambium with no tool to disagree
/// with it.
pub fn wanted(names: &[String]) -> Result<Vec<String>> {
    let names: Vec<&str> = match names.is_empty() {
        true => vec!["bazel-diff", "tango"],
        false => names.iter().map(String::as_str).collect(),
    };
    let mut out = Vec::new();
    for n in names {
        match n.trim() {
            cambi::NAME => {}
            "bazel-diff" | "bazel-diff-rust" | "rust" => {
                out.push(bazeldiff::Implementation::Rust.name().to_string())
            }
            "bazel-diff-kotlin" | "kotlin" => {
                out.push(bazeldiff::Implementation::Kotlin.name().to_string())
            }
            "tango" => out.push(tango::NAME.to_string()),
            "target-determinator" | "td" => out.push(target_determinator::NAME.to_string()),
            "none" | "" => {}
            other => anyhow::bail!(
                "unknown tool {other:?}: cambium, bazel-diff, bazel-diff-kotlin, tango, \
                 target-determinator"
            ),
        }
    }
    Ok(out)
}

/// Locates one tool by name, or explains on stderr why it is being skipped.
///
/// A missing binary is a warning, not an error: the machine that runs the
/// nightly job may have bazel-diff and no Go toolchain, and a run that
/// refuses to compare anything because one of two optional tools is absent is
/// worse than a run that says so and compares the other.
pub fn open(name: &str, at: &ToolLocations) -> Result<Option<Box<dyn Tool>>> {
    use bazeldiff::{BazelDiff, Implementation};
    let found: Result<Box<dyn Tool>> = match name {
        "tango" => tango::Tango::discover_with(at).map(|t| Box::new(t) as _),
        "target-determinator" | "td" => {
            target_determinator::TargetDeterminator::discover_with(at).map(|t| Box::new(t) as _)
        }
        "bazel-diff-rust" | "rust" => {
            BazelDiff::discover_with(Implementation::Rust, at).map(|b| Box::new(b) as _)
        }
        "bazel-diff-kotlin" | "kotlin" => {
            BazelDiff::discover_with(Implementation::Kotlin, at).map(|b| Box::new(b) as _)
        }
        other => {
            anyhow::bail!(
                "unknown tool {other:?}: bazel-diff-rust, bazel-diff-kotlin, tango, \
                 target-determinator"
            )
        }
    };
    match found {
        Ok(p) => {
            tracing::info!(tool = p.name(), version = %p.version(), "tool");
            Ok(Some(p))
        }
        Err(e) => {
            eprintln!("cambium-bench: changes: skipping {name}: {e:#}");
            Ok(None)
        }
    }
}

/// One tool's answer for one commit change set.
#[derive(Clone, Debug, Default)]
pub struct ToolRun {
    /// The affected labels, as the tool printed them.
    pub labels: BTreeSet<String>,
    /// Wall time for the answer with nothing cached: for a hash-diffing tool,
    /// hashing *both* revisions plus the diff.
    pub cold_wall: Duration,
}

impl ToolRun {
    /// Restricts the answer to the universe cambium reports on: rules in the
    /// head index.
    ///
    /// cambium's walk emits `NodeKind::Rule` nodes only (see
    /// `cambium_index::walk`), so a tool's source-file rows would show up
    /// as `cambium_missing` (a *failure*) for a difference that is purely
    /// one of vocabulary. A label the index does not know at all is kept: that
    /// is a real disagreement (an external label, or something cambium never
    /// indexed) and the classifier should see it.
    pub fn restrict_to_rules(&mut self, index: cambium_index::IndexRef<'_>) {
        self.labels.retain(|label| match index.id_of(label) {
            Some(id) => index.is_rule(id),
            None => true,
        });
    }
}

/// One label, in the spelling every tool and cambium can be compared in.
///
/// Three tools print the same target three ways. Bazel's query proto under
/// bzlmod uses *canonical* repository names (`@@rules_cc+//:x`, or
/// `@@rules_cc~//:x` on Bazel 7) while Tango prints whatever the proto said
/// and bazel-diff prints the apparent name `@rules_cc//:x`. Left alone, every
/// external label in the run would appear on both sides of the symmetric
/// difference at once, drowning the classes that matter. So each tool runs
/// its labels through this on the way in:
///
/// - `@@repo//pkg:t` and `@repo//pkg:t` collapse to `@repo//pkg:t`;
/// - a canonical suffix (`repo+`, `repo+1.2.3`, `repo~override`) is trimmed
///   back to the module name;
/// - the main repository's `@//pkg:t` / `@@//pkg:t` becomes plain `//pkg:t`,
///   which is what cambium's index stores.
///
/// Anything that is not a label (`//external:x`, a bare path) is returned
/// unchanged apart from trimming.
pub fn normalise_label(label: &str) -> String {
    let l = label.trim();
    // Deliberately not resolved through a repository mapping: the point is
    // one spelling per module across tools that each write their own, and a
    // canonical name is trimmed back to the module rather than looked up.
    let (repo, path) = match Label::parse(l).map(|p| (p.repo(), p.rest())) {
        Some((Repo::Apparent(repo), path)) | Some((Repo::Canonical(repo), path)) => (repo, path),
        // A relative `:name` has no repository to spell; `//pkg:t` only needs
        // the shorthand check below.
        Some((Repo::Main, path)) if l.starts_with("//") => ("", path),
        // `@repo` on its own is shorthand for `@repo//:repo` and is left
        // alone rather than inventing one; a relative label and something
        // that is not a label at all are returned as they came.
        _ => return l.to_string(),
    };
    // `//a/b` is Bazel's shorthand for `//a/b:b`, and target-determinator
    // prints it that way for every target named after its package. Every
    // other tool writes the colon, and a set comparison reads the two
    // spellings as two targets, so the shorthand is expanded here.
    let path = match path.split_once(':') {
        Some(_) => path.to_string(),
        None => match path.rsplit_once('/') {
            Some((_, last)) if !last.is_empty() => format!("{path}:{last}"),
            _ if !path.is_empty() => format!("{path}:{path}"),
            _ => path.to_string(),
        },
    };
    if repo.is_empty() {
        return format!("//{path}");
    }
    let module = repo.split_once(['+', '~']).map(|(m, _)| m).unwrap_or(repo);
    format!("@{module}//{path}")
}

/// Everything a tool needs from the run besides the change set itself.
#[derive(Clone, Debug)]
pub struct RunContext {
    /// The Bazel (or bazelisk) binary the whole comparison uses.
    pub bazel_binary: PathBuf,
    /// Startup options placed before the command word, so a tool joins the
    /// same server as cambium.
    pub startup_flags: Vec<String>,
    /// Extra environment for every child process (`USE_BAZEL_VERSION`, ...).
    pub env: Vec<(String, String)>,
    /// A directory the tool may litter: hash files, raw outputs, logs.
    pub scratch: PathBuf,
    /// A checkout of the *base* revision, distinct from the head worktree.
    pub base_worktree: PathBuf,
}

impl RunContext {
    /// The startup flags a tool may use: the run's *minus*
    /// `--output_base`.
    ///
    /// An output base is only safe to share between invocations that run in
    /// the same workspace directory. Repository rules may write absolute paths
    /// into the external tree (envoy's `envoy_api` repo symlinks every entry
    /// of `api/` into the workspace it was fetched from) and the fetch marker
    /// does not mention the workspace, so Bazel never refetches. Point a
    /// second workspace (a tool's base worktree, tango's own scratch
    /// checkouts) at the same output base and it inherits the *first*
    /// workspace's symlinks; delete that first workspace and every one of them
    /// dangles. That is how a benchmark run ended with
    /// `Error computing the main repository mapping: ... '@@envoy_api//bazel'
    /// does not have [a package]` on every subsequent query: a hard exit 1,
    /// raised before the query is evaluated, so `--keep_going` cannot soften
    /// it, and cambium's own index queries were the casualty.
    ///
    /// Dropping the flag lets Bazel derive the output base from the workspace
    /// path, which is exactly the one-per-workspace invariant we need. The
    /// cost is one cold fetch per distinct workspace directory, amortised over
    /// every change set in the run, in exchange for tools that cannot corrupt
    /// each other or us.
    pub fn tool_startup_flags(&self) -> Vec<String> {
        self.startup_flags
            .iter()
            .filter(|f| !f.starts_with("--output_base"))
            .cloned()
            .collect()
    }

    /// A child process carrying this context's environment.
    ///
    /// Stdin is always `/dev/null` and `CI=1` rides along unless the caller's
    /// own environment already sets `CI`: see
    /// [`noninteractive_command`] for why.
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        noninteractive_command(program, &self.env)
    }

    pub fn scratch_file(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }
}

/// A child process carrying `env`, unable to block on a prompt: stdin is
/// nulled, and `CI=1` is set unless `env` already names `CI`.
///
/// A tool never has a terminal to prompt against (it runs unattended, in a
/// batch of hundreds of change sets) but some of them assume one anyway.
/// Nulling stdin makes any such prompt fail at once with EOF instead of
/// hanging, and removes any ambiguity about where that EOF came from; `CI=1`
/// is the general signal most such CLIs (Aspect's plugins included; see
/// `.aspect/bazelrc/ci*.bazelrc` in the benchmark's subject repositories)
/// already check to skip interactive behaviour rather than wait on a prompt
/// no one will answer.
///
/// Neither is a fix for a prompt that *cannot* be skipped. The Aspect CLI's
/// `query` wrapper opens an interactive preset picker whenever it gets no
/// positional expression (bazel-diff's `--query_file` invocation exactly)
/// and `CI=1` does not stop it; that is why the run never reaches the Aspect
/// CLI at all: see [`crate::checkout::bazelisk_env`].
///
/// The one process builder for every tool and for `cambi`: the product under
/// measurement is run exactly as a peer is.
pub fn noninteractive_command(
    program: impl AsRef<std::ffi::OsStr>,
    env: &[(String, String)],
) -> Command {
    let mut cmd = Command::new(program);
    cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    cmd.stdin(std::process::Stdio::null());
    if !env.iter().any(|(k, _)| k == "CI") {
        cmd.env("CI", "1");
    }
    cmd
}

/// A tool that answers "what is affected between base and head".
///
/// Object-safe on purpose: [`crate::walk::BazelChecks`] holds `&[&dyn Tool]`
/// so a walk can run several peers at every sampled step.
pub trait Tool {
    /// Stable identifier used in JSON keys and report columns
    /// (`bazel-diff-rust`, `bazel-diff-kotlin`, `tango`).
    fn name(&self) -> &str;

    /// The tool's own version string, recorded with every result so a change
    /// in disagreement counts can be attributed.
    fn version(&self) -> String;

    /// Runs the tool. `ws` is the head worktree; the base checkout is
    /// `ctx.base_worktree`.
    fn affected(&self, ws: &Path, base: &str, head: &str, ctx: &RunContext) -> Result<ToolRun>;
}

/// Output of a child process that was expected to succeed.
#[derive(Clone, Debug)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
    pub wall: Duration,
    /// The exit status, for a caller that accepts more than success.
    pub status: std::process::ExitStatus,
}

/// Runs `cmd`, timing it, and turns a non-zero exit into an error that carries
/// the tail of stderr: a tool failing on one change set out of two hundred is
/// only debuggable if the message says why.
pub fn run_capture(cmd: &mut Command, what: &str) -> Result<Captured> {
    run_capture_accepting(cmd, what, |s| s.success())
}

/// [`run_capture`] with its own idea of which exit statuses are answers:
/// `cambi affected` exits 2 for "everything is affected", which is one.
pub fn run_capture_accepting(
    cmd: &mut Command,
    what: &str,
    accept: impl Fn(std::process::ExitStatus) -> bool,
) -> Result<Captured> {
    let t = Instant::now();
    let out = cmd
        .output()
        .with_context(|| format!("spawning {what}: {cmd:?}"))?;
    let wall = t.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    anyhow::ensure!(
        accept(out.status),
        "{what} exited with {}\n--- stderr tail ---\n{}",
        out.status,
        tail(&stderr, 40)
    );
    Ok(Captured {
        stdout,
        stderr,
        wall,
        status: out.status,
    })
}

/// The last `n` lines of `s`.
pub(crate) fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// A second worktree of the repository, checked out at one revision and
/// removed when it goes out of scope.
///
/// The git plumbing lives in [`cambium_diff::Worktree`], which the engine also
/// uses to run Bazel at a revision the working tree is not on; this adds the
/// one thing the comparison needs on top: a copy of the untracked
/// `cambium.toml`.
#[derive(Debug)]
pub struct BaseWorktree {
    inner: cambium_diff::Worktree,
}

/// Copies the workspace's generated `cambium.toml` into the base worktree.
///
/// The benchmark writes that file into the repository root and excludes it from
/// git, so a fresh `git worktree add` does not have it, and on `bazel/bazel`
/// the root package is `filegroup(name = "srcs", srcs = glob(["*"]))`, which
/// swallows it. The head tree's `//:srcs` then lists `//:cambium.toml` and the
/// base tree's does not, so the tools see the filegroup change on every
/// change set while cambium (whose base index was built in a tree that had the file)
/// sees nothing. That surfaced as `//:srcs` in `cambium_missing` on a change set
/// whose only changed files were `.github/workflows/*.yml`, which no rule
/// consumes at all: a disagreement manufactured entirely by the harness.
///
/// A tracked `cambium.toml` is left alone: the checkout already has the right
/// version of it.
pub(crate) fn mirror_untracked_config(repo_worktree: &Path, base: &Path) -> Result<()> {
    let src = repo_worktree.join(cambium_engine::config::CONFIG_FILE);
    if !src.exists() {
        return Ok(());
    }
    let tracked = git(
        repo_worktree,
        &[
            "ls-files",
            "--error-unmatch",
            cambium_engine::config::CONFIG_FILE,
        ],
    )
    .is_ok();
    if tracked {
        return Ok(());
    }
    std::fs::copy(&src, base.join(cambium_engine::config::CONFIG_FILE))
        .with_context(|| format!("copying {} into the base worktree", src.display()))?;
    Ok(())
}

impl BaseWorktree {
    /// Adds (or reuses) a worktree of `repo_worktree` at `base_sha`, under
    /// `scratch`. Reuse matters: consecutive walk steps share revisions, and
    /// re-adding a worktree per step would dominate.
    pub fn create(repo_worktree: &Path, base_sha: &str, scratch: &Path) -> Result<Self> {
        std::fs::create_dir_all(scratch)?;
        let path = scratch.join("base-worktree");
        let inner = cambium_diff::Worktree::create(repo_worktree, base_sha, &path)
            .with_context(|| format!("adding a base worktree at {base_sha}"))?;
        mirror_untracked_config(repo_worktree, inner.path())?;
        Ok(Self { inner })
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    /// Moves this worktree to another revision, for the next change set.
    pub fn checkout(&self, sha: &str) -> Result<()> {
        self.inner.checkout(sha)?;
        mirror_untracked_config(self.inner.owner(), self.inner.path())?;
        Ok(())
    }

    /// Unregisters and deletes the worktree. `Drop` does this too, and ignores
    /// failures; call it directly to see them.
    pub fn remove(&self) -> Result<()> {
        Ok(self.inner.remove()?)
    }
}

pub(crate) use crate::checkout::git;

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RunContext {
        RunContext {
            bazel_binary: "bazelisk".into(),
            startup_flags: vec!["--output_base=/tmp/ob".into(), "--bazelrc=/tmp/rc".into()],
            env: vec![("USE_BAZEL_VERSION".into(), "7.4.1".into())],
            scratch: PathBuf::from("/tmp/scratch"),
            base_worktree: PathBuf::from("/tmp/base"),
        }
    }

    #[test]
    fn external_labels_collapse_to_one_spelling() {
        // The three spellings of the same external target agree.
        for l in ["@@rules_cc+//:x", "@rules_cc//:x", "@@rules_cc~//:x"] {
            assert_eq!(normalise_label(l), "@rules_cc//:x", "{l}");
        }
        assert_eq!(
            normalise_label("@@rules_cc+1.2.3//pkg:t"),
            "@rules_cc//pkg:t"
        );
        assert_eq!(
            normalise_label("@@bazel_tools//tools/cpp:toolchain"),
            "@bazel_tools//tools/cpp:toolchain"
        );
    }

    #[test]
    fn the_main_repository_loses_its_at_sign() {
        assert_eq!(normalise_label("@//pkg:t"), "//pkg:t");
        assert_eq!(normalise_label("@@//pkg:t"), "//pkg:t");
        assert_eq!(normalise_label("//pkg:t"), "//pkg:t");
        assert_eq!(normalise_label("  //pkg:t \n"), "//pkg:t");
    }

    #[test]
    fn labels_that_are_not_repository_qualified_are_left_alone() {
        assert_eq!(normalise_label("//external:foo"), "//external:foo");
        // target-determinator's shorthand for a target named after its package.
        assert_eq!(
            normalise_label("//tools/protoprint"),
            "//tools/protoprint:protoprint"
        );
        assert_eq!(
            normalise_label("//test/config_test"),
            "//test/config_test:config_test"
        );
        assert_eq!(
            normalise_label("@envoy_api//envoy/api"),
            "@envoy_api//envoy/api:api"
        );
        assert_eq!(normalise_label("@@rules_cc+//cc"), "@rules_cc//cc:cc");
        assert_eq!(normalise_label("@repo"), "@repo");
        assert_eq!(normalise_label("not a label"), "not a label");
    }

    #[test]
    fn the_rule_filter_drops_file_targets_and_keeps_unknown_labels() {
        // A hand-built index with one rule and one source file: the file-target
        // row is what cambium never reports, so it must not read as missing.
        let mut b = cambium_index::IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[cambium_index::PackageUpdate {
            package: "pkg".into(),
            build_hash: [0; 32],
            bzl_hash: [0; 32],
            targets: vec![
                cambium_index::TargetRow::rule(
                    "//pkg:t",
                    "genrule",
                    vec!["//pkg:a.txt".to_string()],
                ),
                cambium_index::TargetRow::source_file("//pkg:a.txt"),
            ],
        }]);
        let index = b.finish("head");
        let mut run = ToolRun {
            labels: ["//pkg:t", "//pkg:a.txt", "@ext//:x"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..Default::default()
        };
        run.restrict_to_rules(index.view());
        assert_eq!(
            run.labels.iter().cloned().collect::<Vec<_>>(),
            vec!["//pkg:t".to_string(), "@ext//:x".to_string()],
            "unknown labels stay: they are a real disagreement"
        );
    }

    /// An output base is per workspace directory; tools run in other
    /// workspaces, so they must not inherit ours.
    #[test]
    fn tools_do_not_inherit_the_output_base() {
        assert_eq!(ctx().tool_startup_flags(), vec!["--bazelrc=/tmp/rc"]);
    }

    /// A tool's stdin must never be a terminal a prompt could wait on:
    /// see [`noninteractive_command`]. `cat` on a null stdin sees
    /// EOF immediately and produces no output.
    #[test]
    fn a_tool_command_gets_no_stdin() {
        let out = ctx().command("cat").output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn a_tool_command_gets_ci_1_by_default() {
        let out = ctx()
            .command("sh")
            .arg("-c")
            .arg("echo $CI")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
    }

    /// An explicit `CI` in the context's own environment is not
    /// second-guessed.
    #[test]
    fn an_explicit_ci_in_the_context_env_is_left_alone() {
        let mut c = ctx();
        c.env.push(("CI".into(), "false".into()));
        let out = c.command("sh").arg("-c").arg("echo $CI").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "false");
    }

    #[test]
    fn a_failing_command_reports_its_stderr() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo boom >&2; exit 3"]);
        let err = run_capture(&mut cmd, "the thing").unwrap_err().to_string();
        assert!(err.contains("boom"), "{err}");
        assert!(err.contains("the thing"), "{err}");
    }

    #[test]
    fn the_stderr_tail_keeps_the_last_lines() {
        let s: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let t = tail(&s, 3);
        assert_eq!(t, "line97\nline98\nline99");
    }

    #[test]
    fn a_base_worktree_is_created_moved_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        cambium_testkit::init_repo(&repo);
        cambium_testkit::write(&repo, "a.txt", "one");
        let first = cambium_testkit::commit_all(&repo, "one");
        cambium_testkit::write(&repo, "a.txt", "two");
        let second = cambium_testkit::commit_all(&repo, "two");

        let scratch = dir.path().join("scratch");
        let path = {
            let wt = BaseWorktree::create(&repo, &first, &scratch).unwrap();
            assert_eq!(
                std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
                "one"
            );
            wt.checkout(&second).unwrap();
            assert_eq!(
                std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
                "two"
            );
            // The head checkout is undisturbed by any of this.
            assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "two");
            wt.path().to_path_buf()
        };
        assert!(!path.exists(), "dropping the worktree removes it");
    }
}
